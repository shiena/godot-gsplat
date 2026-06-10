//! GPU counting-sort backend (Step 2): per-splat back-to-front ordering on the
//! main RenderingDevice, sampled by the splat shader via an R32F sort texture.

use godot::classes::image::Format as ImageFormat;
use godot::classes::rendering_device::{
    DataFormat, ShaderLanguage, ShaderStage, TextureUsageBits, UniformType,
};
use godot::classes::{
    Node3D, RdShaderSource, RdTextureFormat, RdTextureView, RdUniform, RenderingDevice,
    RenderingServer, Texture2D, Texture2Drd, Viewport, XrServer,
};
use godot::prelude::*;

use crate::backend::VR_VIEW_BASIS_PER_EYE;

use super::shaders::{
    SORT_COUNT_GLSL, SORT_SCAN_ADD_GLSL, SORT_SCAN_BLOCKSUMS_GLSL, SORT_SCAN_LOCAL_GLSL,
    SORT_SCATTER_GLSL,
};
use super::GaussianSplatNode3D;

// --- Step 2: GPU counting sort -------------------------------------------------
// Depth bucket count (sort precision) and compute workgroup size, mirroring the
// validated PoC. The sort-order texture is R32F; one texel per splat holds the
// splat id to draw at that slot, sampled by SPLAT_TEXTURE_SHADER.
// 16-bit depth precision: 65536 buckets scanned with a parallel block prefix sum
// (SORT_SCAN_BLOCK threads per block, SORT_NUM_BLOCKS blocks). A single-pass
// counting sort is order-agnostic within a bucket, so a high bucket count yields
// near-exact back-to-front ordering. SORT_SCAN_BLOCK / SORT_NUM_BLOCKS are mirrored
// as `#define`s in the scan shaders (shaders.rs) and must stay in sync.
const SORT_NUM_BUCKETS: i32 = 65536;
const SORT_LOCAL_SIZE: u32 = 256;
const SORT_SCAN_BLOCK: i32 = 1024;
const SORT_NUM_BLOCKS: i32 = SORT_NUM_BUCKETS / SORT_SCAN_BLOCK;
const SORT_TEX_WIDTH: i32 = 4096;

// Compute stages: count -> scan_local -> scan_blocksums -> scan_add -> scatter.
const SORT_STAGES: usize = 5;
const ST_COUNT: usize = 0;
const ST_SCAN_LOCAL: usize = 1;
const ST_SCAN_BLOCKSUMS: usize = 2;
const ST_SCAN_ADD: usize = 3;
const ST_SCATTER: usize = 4;

// Re-sort gating thresholds: a static view keeps the previous back-to-front order.
// Position is a fraction of the splat cloud diagonal; orientation is a per-axis
// cosine (~0.3 degrees).
const SORT_RESORT_POS_FRACTION: f32 = 0.002;
const SORT_RESORT_AXIS_COS: f32 = 0.999_986;

// Runtime GPU counting-sort state. RIDs live on the main RenderingDevice; the
// per-frame dispatch runs via RenderingServer::call_on_render_thread.
pub(super) struct SortGpu {
    pub(super) ready: bool,
    pub(super) attempted: bool,
    pub(super) enabled_in_shader: bool,
    pub(super) dispatched_once: bool,
    // Last view (camera_view * node_model) used for a sort; gates re-sorting.
    pub(super) last_view: Option<Transform3D>,
    // Inputs stashed when the splat render is (re)built.
    pub(super) positions: PackedByteArray,
    pub(super) splat_count: i32,
    pub(super) local_aabb: Aabb,
    // GPU resources.
    pub(super) tex_width: i32,
    pub(super) tex_height: i32,
    pub(super) pos_buf: Rid,
    pub(super) hist_buf: Rid,
    pub(super) off_buf: Rid,
    pub(super) blocks_buf: Rid,
    pub(super) sort_tex_rid: Rid,
    pub(super) shaders: [Rid; SORT_STAGES],
    pub(super) pipelines: [Rid; SORT_STAGES],
    pub(super) sets: [Rid; SORT_STAGES],
    pub(super) texture: Option<Gd<Texture2Drd>>,
    // Second eye's sort order for VR per-eye sorting (unused on flat displays).
    pub(super) sort_tex_rid_b: Rid,
    pub(super) scatter_set_b: Rid,
    pub(super) texture_b: Option<Gd<Texture2Drd>>,
}

impl Default for SortGpu {
    fn default() -> Self {
        Self {
            ready: false,
            attempted: false,
            enabled_in_shader: false,
            dispatched_once: false,
            last_view: None,
            positions: PackedByteArray::new(),
            splat_count: 0,
            local_aabb: Aabb::default(),
            tex_width: 0,
            tex_height: 0,
            pos_buf: Rid::Invalid,
            hist_buf: Rid::Invalid,
            off_buf: Rid::Invalid,
            blocks_buf: Rid::Invalid,
            sort_tex_rid: Rid::Invalid,
            shaders: [Rid::Invalid; SORT_STAGES],
            pipelines: [Rid::Invalid; SORT_STAGES],
            sets: [Rid::Invalid; SORT_STAGES],
            texture: None,
            sort_tex_rid_b: Rid::Invalid,
            scatter_set_b: Rid::Invalid,
            texture_b: None,
        }
    }
}

impl GaussianSplatNode3D {
    // One-shot attempt to bring up the GPU sort once renderable data exists.
    pub(super) fn try_enable_sort(&mut self) {
        self.sort.attempted = true;
        if self.sort.positions.is_empty() {
            // Case B: no live asset (instanced from a pre-imported .scn) — recover
            // the splat centers from the serialized data texture.
            self.reconstruct_sort_inputs_from_material();
        }
        if self.sort.splat_count > 0 && !self.sort.positions.is_empty() {
            self.setup_sort();
        }
    }

    // Recover per-splat centers from the data texture so a node instanced from a
    // pre-imported .scn (no live asset) can still be depth-sorted. Each splat is
    // four RGBA-float texels; texel 0 (.rgb) holds the center.
    pub(super) fn reconstruct_sort_inputs_from_material(&mut self) {
        let Some(material) = self.splat_material() else {
            return;
        };
        let splat_count = material
            .get_shader_parameter("splat_count")
            .try_to::<i32>()
            .unwrap_or(0);
        if splat_count <= 0 {
            return;
        }
        let Ok(data_texture) = material
            .get_shader_parameter("data_tex")
            .try_to::<Gd<Texture2D>>()
        else {
            return;
        };
        let Some(image) = data_texture.get_image() else {
            return;
        };

        let count = splat_count as usize;
        let mut positions: Vec<f32> = Vec::with_capacity(count * 4);
        let mut min = Vector3::new(f32::INFINITY, f32::INFINITY, f32::INFINITY);
        let mut max = Vector3::new(f32::NEG_INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY);

        // apply_render_data() always builds the data texture as RGBAF; any other
        // format means the material was replaced externally — skip sorting then.
        if image.get_format() != ImageFormat::RGBAF {
            return;
        }
        let floats = image.get_data().to_float32_array();
        let values = floats.as_slice();
        if values.len() < count * 16 {
            return;
        }
        for i in 0..count {
            let base = i * 16;
            let center = Vector3::new(values[base], values[base + 1], values[base + 2]);
            positions.extend_from_slice(&[center.x, center.y, center.z, 1.0]);
            min.x = min.x.min(center.x);
            min.y = min.y.min(center.y);
            min.z = min.z.min(center.z);
            max.x = max.x.max(center.x);
            max.y = max.y.max(center.y);
            max.z = max.z.max(center.z);
        }

        let size = max - min;
        self.sort.positions = PackedFloat32Array::from(positions).to_byte_array();
        self.sort.splat_count = splat_count;
        self.sort.local_aabb = Aabb::new(min, size).grow(size.length() * 0.05 + 0.01);
    }

    // Build the main-RenderingDevice resources for the GPU counting sort. Runs on
    // the main thread (== render thread under single-threaded rendering).
    pub(super) fn setup_sort(&mut self) {
        let count = self.sort.splat_count.max(0);
        if count == 0 || self.sort.positions.is_empty() {
            return;
        }

        let server = RenderingServer::singleton();
        let Some(mut device) = server.get_rendering_device() else {
            return;
        };

        // Positions storage buffer (vec4 per splat) + histogram/offset buffers.
        let pos_buf = device
            .storage_buffer_create_ex(self.sort.positions.len() as u32)
            .data(&self.sort.positions)
            .done();
        let bucket_bytes = (SORT_NUM_BUCKETS as u32) * 4;
        let hist_buf = device.storage_buffer_create(bucket_bytes);
        let off_buf = device.storage_buffer_create(bucket_bytes);
        let blocks_buf = device.storage_buffer_create((SORT_NUM_BLOCKS as u32) * 4);

        // Sort-order texture (R32F): compute writes, the splat material samples.
        let tex_width = SORT_TEX_WIDTH;
        let tex_height = ((count + tex_width - 1) / tex_width).max(1);
        let mut format = RdTextureFormat::new_gd();
        format.set_width(tex_width as u32);
        format.set_height(tex_height as u32);
        format.set_format(DataFormat::R32_SFLOAT);
        format.set_usage_bits(
            TextureUsageBits::STORAGE_BIT
                | TextureUsageBits::SAMPLING_BIT
                | TextureUsageBits::CAN_UPDATE_BIT,
        );
        let sort_tex_rid = device.texture_create(&format, &RdTextureView::new_gd());
        let sort_tex_rid_b = device.texture_create(&format, &RdTextureView::new_gd());

        // Compile the counting-sort compute stages (one shader + pipeline each).
        let sources = [
            SORT_COUNT_GLSL,
            SORT_SCAN_LOCAL_GLSL,
            SORT_SCAN_BLOCKSUMS_GLSL,
            SORT_SCAN_ADD_GLSL,
            SORT_SCATTER_GLSL,
        ];
        let mut shaders = [Rid::Invalid; SORT_STAGES];
        let mut pipelines = [Rid::Invalid; SORT_STAGES];
        for (stage, source) in sources.iter().enumerate() {
            let Some(shader) = compile_compute_shader(&mut device, source) else {
                godot_error!("[gsplat] failed to compile sort compute stage {stage}");
                for rid in shaders.into_iter().chain(pipelines).chain([
                    pos_buf,
                    hist_buf,
                    off_buf,
                    blocks_buf,
                    sort_tex_rid,
                    sort_tex_rid_b,
                ]) {
                    if rid.is_valid() {
                        device.free_rid(rid);
                    }
                }
                return;
            };
            shaders[stage] = shader;
            pipelines[stage] = device.compute_pipeline_create(shader);
        }

        // Uniform sets per stage (bindings: pos=0, hist=1, off=2, img=3, blocks=4).
        let pos_u = ssbo_uniform(pos_buf, 0);
        let hist_u = ssbo_uniform(hist_buf, 1);
        let off_u = ssbo_uniform(off_buf, 2);
        let img_u = image_uniform(sort_tex_rid, 3);
        let blocks_u = ssbo_uniform(blocks_buf, 4);
        let mut sets = [Rid::Invalid; SORT_STAGES];
        sets[ST_COUNT] =
            device.uniform_set_create(&uniform_array(&[&pos_u, &hist_u]), shaders[ST_COUNT], 0);
        sets[ST_SCAN_LOCAL] = device.uniform_set_create(
            &uniform_array(&[&hist_u, &off_u, &blocks_u]),
            shaders[ST_SCAN_LOCAL],
            0,
        );
        sets[ST_SCAN_BLOCKSUMS] =
            device.uniform_set_create(&uniform_array(&[&blocks_u]), shaders[ST_SCAN_BLOCKSUMS], 0);
        sets[ST_SCAN_ADD] = device.uniform_set_create(
            &uniform_array(&[&off_u, &blocks_u]),
            shaders[ST_SCAN_ADD],
            0,
        );
        sets[ST_SCATTER] = device.uniform_set_create(
            &uniform_array(&[&pos_u, &off_u, &img_u]),
            shaders[ST_SCATTER],
            0,
        );
        // Second eye scatters into its own sort texture (VR per-eye).
        let img_u_b = image_uniform(sort_tex_rid_b, 3);
        let scatter_set_b = device.uniform_set_create(
            &uniform_array(&[&pos_u, &off_u, &img_u_b]),
            shaders[ST_SCATTER],
            0,
        );

        let mut texture = Texture2Drd::new_gd();
        texture.set_texture_rd_rid(sort_tex_rid);
        let mut texture_b = Texture2Drd::new_gd();
        texture_b.set_texture_rd_rid(sort_tex_rid_b);

        self.sort.pos_buf = pos_buf;
        self.sort.hist_buf = hist_buf;
        self.sort.off_buf = off_buf;
        self.sort.blocks_buf = blocks_buf;
        self.sort.sort_tex_rid = sort_tex_rid;
        self.sort.shaders = shaders;
        self.sort.pipelines = pipelines;
        self.sort.sets = sets;
        self.sort.tex_width = tex_width;
        self.sort.tex_height = tex_height;
        self.sort.texture = Some(texture);
        self.sort.sort_tex_rid_b = sort_tex_rid_b;
        self.sort.scatter_set_b = scatter_set_b;
        self.sort.texture_b = Some(texture_b);
        self.sort.ready = true;
        // The material is pointed at the sort texture only after the first dispatch
        // (see process); binding the Texture2Drd the frame it is created would make
        // the renderer sample an unregistered texture for one frame.
    }

    // Per-eye sort views (combined = camera_view * node_model). One entry on flat
    // displays; two (left, right) for VR per-eye sorting. Empty when there is no
    // active camera, in which case the previous order is kept.
    pub(super) fn current_sort_views(&self) -> Vec<(Transform3D, usize)> {
        let Some(viewport) = self.base().get_viewport() else {
            return Vec::new();
        };
        let model = self.base().get_global_transform();

        // VR per-eye path (structure only; unverified — needs real XR hardware).
        if viewport.is_using_xr() && self.vr_per_eye_enabled() {
            if let Some(eyes) = self.xr_eye_views(&viewport, model) {
                return eyes;
            }
        }

        // Flat / head-center: a single view from the active camera (the editor
        // viewport camera in the editor, the scene camera at runtime).
        let Some(camera) = self.active_camera() else {
            return Vec::new();
        };
        let view = camera.get_global_transform().affine_inverse();
        vec![(view * model, 0)]
    }

    pub(super) fn vr_per_eye_enabled(&self) -> bool {
        self.backend_settings
            .as_ref()
            .map(|settings| settings.bind().get_vr_view_basis() == VR_VIEW_BASIS_PER_EYE)
            .unwrap_or(false)
    }

    // Acquire per-eye world views from the XR interface. Assumes the standard
    // XROrigin3D > XRCamera3D hierarchy for the reference transform; the exact
    // per-eye math must be validated on real XR hardware.
    pub(super) fn xr_eye_views(
        &self,
        viewport: &Gd<Viewport>,
        model: Transform3D,
    ) -> Option<Vec<(Transform3D, usize)>> {
        let interface = XrServer::singleton().get_primary_interface()?;
        let view_count = interface.get_view_count().min(2);
        if view_count == 0 {
            return None;
        }
        let reference = viewport
            .get_camera_3d()
            .and_then(|camera| camera.get_parent())
            .and_then(|parent| parent.try_cast::<Node3D>().ok())
            .map(|origin| origin.get_global_transform())
            .unwrap_or(Transform3D::IDENTITY);
        let mut eyes = Vec::with_capacity(view_count as usize);
        for eye in 0..view_count {
            let eye_world = interface.get_transform_for_view(eye, reference);
            eyes.push((eye_world.affine_inverse() * model, eye as usize));
        }
        Some(eyes)
    }

    // Whether the view moved/rotated enough to warrant a re-sort.
    pub(super) fn sort_view_changed(&self, last: Transform3D, current: Transform3D) -> bool {
        let scale = self.sort.local_aabb.size.length().max(1.0e-3);
        if (current.origin - last.origin).length() > scale * SORT_RESORT_POS_FRACTION {
            return true;
        }
        let axis_cos = normalized_dot(last.basis.col_a(), current.basis.col_a())
            .min(normalized_dot(last.basis.col_b(), current.basis.col_b()))
            .min(normalized_dot(last.basis.col_c(), current.basis.col_c()));
        axis_cos < SORT_RESORT_AXIS_COS
    }

    // Queue a back-to-front counting sort per view (camera_view * node_model). One
    // pass on flat displays; one per eye for VR, each scattering into its own sort
    // texture. The closure runs on the render thread and must not touch the node.
    pub(super) fn dispatch_sort(&self, eyes: &[(Transform3D, usize)]) {
        // Resolve each eye to (push constant, scatter uniform set). Eye 1 targets
        // the second sort texture; all others target the primary one.
        let mut passes: Vec<(PackedByteArray, Rid)> = Vec::with_capacity(eyes.len());
        for &(combined, eye) in eyes {
            let (depth_min, depth_inv_range) = depth_range(combined, self.sort.local_aabb);
            let push_constant = build_sort_push_constant(
                combined,
                depth_min,
                depth_inv_range,
                self.sort.splat_count,
                self.sort.tex_width,
            );
            let scatter_set = if eye == 1 {
                self.sort.scatter_set_b
            } else {
                self.sort.sets[ST_SCATTER]
            };
            passes.push((push_constant, scatter_set));
        }

        let count = self.sort.splat_count.max(0) as u32;
        let groups = count.div_ceil(SORT_LOCAL_SIZE);
        let bucket_bytes = (SORT_NUM_BUCKETS as u32) * 4;
        let scan_blocks = SORT_NUM_BLOCKS as u32;
        let hist_buf = self.sort.hist_buf;
        let pipelines = self.sort.pipelines;
        let sets = self.sort.sets;

        let callable = Callable::from_fn("gsplat_sort_dispatch", move |_args: &[&Variant]| {
            let server = RenderingServer::singleton();
            let Some(mut device) = server.get_rendering_device() else {
                return Variant::nil();
            };
            // Each eye recomputes the histogram for its own view (buffer_clear must
            // run outside a compute list), then runs the 5 stages.
            for (push_constant, scatter_set) in &passes {
                device.buffer_clear(hist_buf, 0, bucket_bytes);
                let list = device.compute_list_begin();
                let pc_len = push_constant.len() as u32;

                // Count splats per depth bucket.
                device.compute_list_bind_compute_pipeline(list, pipelines[ST_COUNT]);
                device.compute_list_bind_uniform_set(list, sets[ST_COUNT], 0);
                device.compute_list_set_push_constant(list, push_constant, pc_len);
                device.compute_list_dispatch(list, groups, 1, 1);
                device.compute_list_add_barrier(list);

                // Parallel block prefix sum of the histogram -> per-bucket offsets.
                // Every stage re-sets the push constant so its pipeline's expected
                // size matches (all stages declare the same 84-byte block).
                device.compute_list_bind_compute_pipeline(list, pipelines[ST_SCAN_LOCAL]);
                device.compute_list_bind_uniform_set(list, sets[ST_SCAN_LOCAL], 0);
                device.compute_list_set_push_constant(list, push_constant, pc_len);
                device.compute_list_dispatch(list, scan_blocks, 1, 1);
                device.compute_list_add_barrier(list);
                device.compute_list_bind_compute_pipeline(list, pipelines[ST_SCAN_BLOCKSUMS]);
                device.compute_list_bind_uniform_set(list, sets[ST_SCAN_BLOCKSUMS], 0);
                device.compute_list_set_push_constant(list, push_constant, pc_len);
                device.compute_list_dispatch(list, 1, 1, 1);
                device.compute_list_add_barrier(list);
                device.compute_list_bind_compute_pipeline(list, pipelines[ST_SCAN_ADD]);
                device.compute_list_bind_uniform_set(list, sets[ST_SCAN_ADD], 0);
                device.compute_list_set_push_constant(list, push_constant, pc_len);
                device.compute_list_dispatch(list, scan_blocks, 1, 1);
                device.compute_list_add_barrier(list);

                // Scatter each splat id into its sorted slot in this eye's texture.
                device.compute_list_bind_compute_pipeline(list, pipelines[ST_SCATTER]);
                device.compute_list_bind_uniform_set(list, *scatter_set, 0);
                device.compute_list_set_push_constant(list, push_constant, pc_len);
                device.compute_list_dispatch(list, groups, 1, 1);
                device.compute_list_end();
            }
            Variant::nil()
        });
        RenderingServer::singleton().call_on_render_thread(&callable);
    }

    pub(super) fn teardown_sort(&mut self) {
        let has_resources = self.sort.pos_buf.is_valid()
            || self.sort.shaders[ST_COUNT].is_valid()
            || self.sort.sort_tex_rid.is_valid();
        if has_resources {
            let server = RenderingServer::singleton();
            if let Some(mut device) = server.get_rendering_device() {
                let buffers = [
                    self.sort.sort_tex_rid,
                    self.sort.sort_tex_rid_b,
                    self.sort.pos_buf,
                    self.sort.hist_buf,
                    self.sort.off_buf,
                    self.sort.blocks_buf,
                ];
                for rid in self
                    .sort
                    .sets
                    .into_iter()
                    .chain([self.sort.scatter_set_b])
                    .chain(self.sort.pipelines)
                    .chain(self.sort.shaders)
                    .chain(buffers)
                {
                    if rid.is_valid() {
                        device.free_rid(rid);
                    }
                }
            }
        }

        // Reset GPU state but keep the stashed inputs so a later tree re-entry can
        // rebuild the sort.
        let positions = std::mem::take(&mut self.sort.positions);
        let splat_count = self.sort.splat_count;
        let local_aabb = self.sort.local_aabb;
        self.sort = SortGpu::default();
        self.sort.positions = positions;
        self.sort.splat_count = splat_count;
        self.sort.local_aabb = local_aabb;

        // Stop the shader from sampling a freed sort texture.
        self.set_material_sort(false);
    }

    pub(super) fn set_material_sort(&self, enabled: bool) {
        let Some(mut material) = self.splat_material() else {
            return;
        };
        if enabled {
            if let Some(texture) = &self.sort.texture {
                material.set_shader_parameter("sort_tex", &Variant::from(texture.clone()));
            }
            if let Some(texture_b) = &self.sort.texture_b {
                material.set_shader_parameter("sort_tex_b", &Variant::from(texture_b.clone()));
            }
            material.set_shader_parameter("sort_enabled", &Variant::from(1_i32));
        } else {
            material.set_shader_parameter("sort_enabled", &Variant::from(0_i32));
            material.set_shader_parameter("sort_tex", &Variant::nil());
            material.set_shader_parameter("sort_tex_b", &Variant::nil());
        }
    }
}

// Compile one GLSL compute shader on the given device, returning its shader RID.
fn compile_compute_shader(device: &mut Gd<RenderingDevice>, glsl: &str) -> Option<Rid> {
    let mut source = RdShaderSource::new_gd();
    source.set_language(ShaderLanguage::GLSL);
    source.set_stage_source(ShaderStage::COMPUTE, glsl);
    let spirv = device.shader_compile_spirv_from_source(&source)?;
    let error = spirv.get_stage_compile_error(ShaderStage::COMPUTE);
    if !error.to_string().is_empty() {
        godot_error!("[gsplat] sort compute compile error: {error}");
        return None;
    }
    let shader = device.shader_create_from_spirv(&spirv);
    shader.is_valid().then_some(shader)
}

fn ssbo_uniform(buffer: Rid, binding: i32) -> Gd<RdUniform> {
    let mut uniform = RdUniform::new_gd();
    uniform.set_uniform_type(UniformType::STORAGE_BUFFER);
    uniform.set_binding(binding);
    uniform.add_id(buffer);
    uniform
}

fn image_uniform(image: Rid, binding: i32) -> Gd<RdUniform> {
    let mut uniform = RdUniform::new_gd();
    uniform.set_uniform_type(UniformType::IMAGE);
    uniform.set_binding(binding);
    uniform.add_id(image);
    uniform
}

fn uniform_array(uniforms: &[&Gd<RdUniform>]) -> Array<Gd<RdUniform>> {
    let mut array = Array::new();
    for uniform in uniforms {
        array.push(*uniform);
    }
    array
}

// Pack the 84-byte std430 push constant: column-major mat4, depth range, and the
// bucket/count/tex-width scalars. Matches the layout in the sort compute shaders.
fn build_sort_push_constant(
    view: Transform3D,
    depth_min: f32,
    depth_inv_range: f32,
    count: i32,
    tex_width: i32,
) -> PackedByteArray {
    let columns = [
        (view.basis.col_a(), 0.0_f32),
        (view.basis.col_b(), 0.0_f32),
        (view.basis.col_c(), 0.0_f32),
        (view.origin, 1.0_f32),
    ];
    let mut bytes: Vec<u8> = Vec::with_capacity(84);
    for (column, w) in columns {
        bytes.extend_from_slice(&column.x.to_le_bytes());
        bytes.extend_from_slice(&column.y.to_le_bytes());
        bytes.extend_from_slice(&column.z.to_le_bytes());
        bytes.extend_from_slice(&w.to_le_bytes());
    }
    bytes.extend_from_slice(&depth_min.to_le_bytes());
    bytes.extend_from_slice(&depth_inv_range.to_le_bytes());
    bytes.extend_from_slice(&(SORT_NUM_BUCKETS as u32).to_le_bytes());
    bytes.extend_from_slice(&(count as u32).to_le_bytes());
    bytes.extend_from_slice(&(tex_width as u32).to_le_bytes());
    PackedByteArray::from(bytes)
}

// Camera-space depth bounds (as positive distances) of the AABB under `view`.
fn depth_range(view: Transform3D, aabb: Aabb) -> (f32, f32) {
    let mut depth_min = f32::INFINITY;
    let mut depth_max = f32::NEG_INFINITY;
    for corner in aabb_corners(aabb) {
        let view_z = (view * corner).z;
        depth_min = depth_min.min(-view_z);
        depth_max = depth_max.max(-view_z);
    }
    if !depth_min.is_finite() || !depth_max.is_finite() {
        return (0.0, 1.0);
    }
    (depth_min, 1.0 / (depth_max - depth_min).max(1e-4))
}

// Cosine of the angle between two vectors (1.0 = identical direction).
fn normalized_dot(a: Vector3, b: Vector3) -> f32 {
    a.normalized().dot(b.normalized())
}

fn aabb_corners(aabb: Aabb) -> [Vector3; 8] {
    let p = aabb.position;
    let s = aabb.size;
    [
        Vector3::new(p.x, p.y, p.z),
        Vector3::new(p.x + s.x, p.y, p.z),
        Vector3::new(p.x, p.y + s.y, p.z),
        Vector3::new(p.x, p.y, p.z + s.z),
        Vector3::new(p.x + s.x, p.y + s.y, p.z),
        Vector3::new(p.x + s.x, p.y, p.z + s.z),
        Vector3::new(p.x, p.y + s.y, p.z + s.z),
        Vector3::new(p.x + s.x, p.y + s.y, p.z + s.z),
    ]
}
