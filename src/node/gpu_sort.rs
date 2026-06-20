//! GPU counting-sort backend (Step 2): per-splat back-to-front ordering on the
//! main RenderingDevice, sampled by the splat shader via an R32F sort texture.

use godot::classes::image::Format as ImageFormat;
use godot::classes::rendering_device::{
    DataFormat, ShaderLanguage, ShaderStage, TextureUsageBits, UniformType,
};
use godot::classes::{
    RdShaderSource, RdTextureFormat, RdTextureView, RdUniform, RenderingDevice, RenderingServer,
    Texture2D, Texture2Drd, Time, Viewport, XrServer,
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

// Re-sort gating threshold: a fraction of the splat cloud diagonal the camera
// must move (in node-local space) before a re-sort. The camera-distance sort
// key is rotation-invariant, so only camera POSITION changes can change the
// order — rotation needs no gate at all.
const SORT_RESORT_POS_FRACTION: f32 = 0.002;
// Minimum interval between re-sorts. An unthrottled HMD re-sorts ~30x/s while
// looking around (measured on Quest 3: App 14 -> 20-24 ms at 800k splats).
const SORT_RESORT_MIN_INTERVAL_MS: u64 = 200;

// Runtime GPU counting-sort state. RIDs live on the main RenderingDevice; the
// per-frame dispatch runs via RenderingServer::call_on_render_thread.
pub(super) struct SortGpu {
    pub(super) ready: bool,
    pub(super) attempted: bool,
    pub(super) enabled_in_shader: bool,
    pub(super) dispatched_once: bool,
    // Whether the last dispatch wrote a separate second-eye order (per-eye VR);
    // decides what the material's sort_tex_b sampler must point at.
    pub(super) per_eye_dispatched: bool,
    // Head-center double buffering: which texture the material currently
    // samples. A re-sort scatters into the OTHER texture and flips afterwards,
    // so the sampled order is never written mid-frame (on tiler GPUs the
    // fragment work of a frame runs long after submission and overlapped the
    // rewrite — the mixed old/new order flashed washed-out white per re-sort).
    pub(super) front_is_b: bool,
    // When the last re-sort was dispatched (Time ticks, msec); throttles the
    // re-sort rate to SORT_RESORT_MIN_INTERVAL_MS.
    pub(super) last_sort_msec: u64,
    // Node-local camera position at the last sort; gates re-sorting (the
    // distance sort key only depends on the camera position in node space).
    pub(super) last_sort_cam_local: Option<Vector3>,
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
            per_eye_dispatched: false,
            // The first dispatch writes the non-front (A) texture and flips.
            front_is_b: true,
            last_sort_msec: 0,
            last_sort_cam_local: None,
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
        self.backend.sort.attempted = true;
        if self.backend.sort.positions.is_empty() {
            // Case B: no live asset (instanced from a pre-imported .scn) — recover
            // the splat centers from the serialized data texture.
            self.reconstruct_sort_inputs_from_material();
        }
        if self.backend.sort.splat_count > 0 && !self.backend.sort.positions.is_empty() {
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
        self.backend.sort.positions = PackedFloat32Array::from(positions).to_byte_array();
        self.backend.sort.splat_count = splat_count;
        self.backend.sort.local_aabb = Aabb::new(min, size).grow(size.length() * 0.05 + 0.01);
    }

    // Build the main-RenderingDevice resources for the GPU counting sort. Runs on
    // the main thread (== render thread under single-threaded rendering).
    pub(super) fn setup_sort(&mut self) {
        let count = self.backend.sort.splat_count.max(0);
        if count == 0 || self.backend.sort.positions.is_empty() {
            return;
        }

        let server = RenderingServer::singleton();
        let Some(mut device) = server.get_rendering_device() else {
            return;
        };

        // Positions storage buffer (vec4 per splat) + histogram/offset buffers.
        let pos_buf = device
            .storage_buffer_create_ex(self.backend.sort.positions.len() as u32)
            .data(&self.backend.sort.positions)
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

        self.backend.sort.pos_buf = pos_buf;
        self.backend.sort.hist_buf = hist_buf;
        self.backend.sort.off_buf = off_buf;
        self.backend.sort.blocks_buf = blocks_buf;
        self.backend.sort.sort_tex_rid = sort_tex_rid;
        self.backend.sort.shaders = shaders;
        self.backend.sort.pipelines = pipelines;
        self.backend.sort.sets = sets;
        self.backend.sort.tex_width = tex_width;
        self.backend.sort.tex_height = tex_height;
        self.backend.sort.texture = Some(texture);
        self.backend.sort.sort_tex_rid_b = sort_tex_rid_b;
        self.backend.sort.scatter_set_b = scatter_set_b;
        self.backend.sort.texture_b = Some(texture_b);
        self.backend.sort.ready = true;
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

        // Flat / head-center: a single view — the editor viewport camera in the
        // editor, the scene camera at runtime, the tracked HMD pose in XR (the
        // current-flagged Camera3D is not what an XR viewport renders from).
        let Some(view_world) = self.active_view_transform() else {
            return Vec::new();
        };
        vec![(view_world.affine_inverse() * model, 0)]
    }

    pub(super) fn vr_per_eye_enabled(&self) -> bool {
        self.backend_settings
            .as_ref()
            .map(|settings| settings.bind().get_vr_view_basis() == VR_VIEW_BASIS_PER_EYE)
            .unwrap_or(false)
    }

    // Acquire per-eye world views from the XR interface. The reference is the XR
    // play-space origin from the XRServer (synced by the current XROrigin3D) —
    // deriving it from the current camera's parent breaks when a non-XR camera is
    // left current. The exact per-eye math must be validated on real XR hardware.
    pub(super) fn xr_eye_views(
        &self,
        _viewport: &Gd<Viewport>,
        model: Transform3D,
    ) -> Option<Vec<(Transform3D, usize)>> {
        let xr = XrServer::singleton();
        let interface = xr.get_primary_interface()?;
        let view_count = interface.get_view_count().min(2);
        if view_count == 0 {
            return None;
        }
        let reference = xr.get_world_origin();
        let mut eyes = Vec::with_capacity(view_count as usize);
        for eye in 0..view_count {
            let eye_world = interface.get_transform_for_view(eye, reference);
            eyes.push((eye_world.affine_inverse() * model, eye as usize));
        }
        Some(eyes)
    }

    // Rate limit between re-sorts (true = enough time has passed). Keeps an HMD's
    // continuous micro-motion from dispatching a sort nearly every frame.
    pub(super) fn sort_interval_elapsed(&self) -> bool {
        let last = self.backend.sort.last_sort_msec;
        if last == 0 {
            return true;
        }
        Time::singleton().get_ticks_msec().saturating_sub(last) >= SORT_RESORT_MIN_INTERVAL_MS
    }

    pub(super) fn mark_sort_dispatched(&mut self) {
        self.backend.sort.last_sort_msec = Time::singleton().get_ticks_msec();
    }

    // Whether the camera moved (in node-local space) enough to warrant a re-sort.
    // Rotation never changes a camera-distance order, so it is not considered.
    pub(super) fn sort_cam_moved(&self, last: Vector3, current: Vector3) -> bool {
        let scale = self.backend.sort.local_aabb.size.length().max(1.0e-3);
        (current - last).length() > scale * SORT_RESORT_POS_FRACTION
    }

    // Queue a back-to-front counting sort per view (camera_view * node_model). One
    // pass on flat/head-center displays; one per eye for VR per-eye sorting, each
    // scattering into its own texture. The closure runs on the render thread and
    // must not touch the node.
    //
    // Head-center writes the BACK texture of the double buffer (never the one the
    // material currently samples); the caller flips the front afterwards. Per-eye
    // mode still writes both textures in place (experimental, unverified).
    pub(super) fn dispatch_sort(&self, eyes: &[(Transform3D, usize)]) {
        let head_center = eyes.len() == 1;
        // Resolve each eye to (push constant, scatter uniform set).
        let mut passes: Vec<(PackedByteArray, Rid)> = Vec::with_capacity(eyes.len());
        for &(combined, eye) in eyes {
            let (depth_min, depth_inv_range) = depth_range(combined, self.backend.sort.local_aabb);
            let push_constant = build_sort_push_constant(
                combined,
                depth_min,
                depth_inv_range,
                self.backend.sort.splat_count,
                self.backend.sort.tex_width,
            );
            let scatter_set = if head_center {
                // Back buffer: the texture NOT currently bound as sort_tex.
                if self.backend.sort.front_is_b {
                    self.backend.sort.sets[ST_SCATTER]
                } else {
                    self.backend.sort.scatter_set_b
                }
            } else if eye == 1 {
                self.backend.sort.scatter_set_b
            } else {
                self.backend.sort.sets[ST_SCATTER]
            };
            passes.push((push_constant, scatter_set));
        }

        let count = self.backend.sort.splat_count.max(0) as u32;
        let groups = count.div_ceil(SORT_LOCAL_SIZE);
        let bucket_bytes = (SORT_NUM_BUCKETS as u32) * 4;
        let scan_blocks = SORT_NUM_BLOCKS as u32;
        let hist_buf = self.backend.sort.hist_buf;
        let pipelines = self.backend.sort.pipelines;
        let sets = self.backend.sort.sets;

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
        free_sort_resources(&mut self.backend.sort);

        // Reset GPU state but keep the stashed inputs so a later tree re-entry can
        // rebuild the sort.
        let positions = std::mem::take(&mut self.backend.sort.positions);
        let splat_count = self.backend.sort.splat_count;
        let local_aabb = self.backend.sort.local_aabb;
        self.backend.sort = SortGpu::default();
        self.backend.sort.positions = positions;
        self.backend.sort.splat_count = splat_count;
        self.backend.sort.local_aabb = local_aabb;

        // Stop the shader from sampling a freed sort texture.
        self.set_material_sort(false);
    }

    pub(super) fn preserve_sort_for_draw_transition(&mut self) {
        self.clear_transition_sort();
        self.backend.transition_sort = Some(std::mem::take(&mut self.backend.sort));
    }

    pub(super) fn clear_transition_sort(&mut self) {
        if let Some(mut sort) = self.backend.transition_sort.take() {
            free_sort_resources(&mut sort);
        }
    }

    pub(super) fn set_material_sort(&self, enabled: bool) {
        let Some(mut material) = self.splat_material() else {
            return;
        };
        if enabled {
            if let (Some(texture), Some(texture_b)) =
                (&self.backend.sort.texture, &self.backend.sort.texture_b)
            {
                // Per-eye mode samples A for eye 0 and B for eye 1. Head-center
                // samples only sort_tex, pointed at the double buffer's FRONT
                // (the back is being rewritten by the next re-sort); sort_tex_b
                // is never sampled then (sort_per_eye == 0) but still gets a
                // valid texture bound.
                let (front, back) = if self.backend.sort.per_eye_dispatched {
                    (texture.clone(), texture_b.clone())
                } else if self.backend.sort.front_is_b {
                    (texture_b.clone(), texture_b.clone())
                } else {
                    (texture.clone(), texture.clone())
                };
                material.set_shader_parameter("sort_tex", &Variant::from(front));
                material.set_shader_parameter("sort_tex_b", &Variant::from(back));
            }
            material.set_shader_parameter(
                "sort_per_eye",
                &Variant::from(self.backend.sort.per_eye_dispatched as i32),
            );
            material.set_shader_parameter("sort_enabled", &Variant::from(1_i32));
        } else {
            material.set_shader_parameter("sort_enabled", &Variant::from(0_i32));
            material.set_shader_parameter("sort_per_eye", &Variant::from(0_i32));
            material.set_shader_parameter("sort_tex", &Variant::nil());
            material.set_shader_parameter("sort_tex_b", &Variant::nil());
        }
    }
}

fn free_sort_resources(sort: &mut SortGpu) {
    let has_resources = sort.pos_buf.is_valid()
        || sort.shaders[ST_COUNT].is_valid()
        || sort.sort_tex_rid.is_valid();
    if !has_resources {
        return;
    }
    let server = RenderingServer::singleton();
    if let Some(mut device) = server.get_rendering_device() {
        let buffers = [
            sort.sort_tex_rid,
            sort.sort_tex_rid_b,
            sort.pos_buf,
            sort.hist_buf,
            sort.off_buf,
            sort.blocks_buf,
        ];
        for rid in sort
            .sets
            .into_iter()
            .chain([sort.scatter_set_b])
            .chain(sort.pipelines)
            .chain(sort.shaders)
            .chain(buffers)
        {
            if rid.is_valid() {
                device.free_rid(rid);
            }
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

// Camera-distance bounds of the AABB under `view` (the combined camera_view *
// node_model transform; the camera sits at the view-space origin). The lower
// bound is fixed at 0 so it stays valid with the camera inside the cloud and
// the range is invariant under camera rotation (lengths are preserved) —
// rotation re-sorts then reproduce the identical bucketing.
fn depth_range(view: Transform3D, aabb: Aabb) -> (f32, f32) {
    let mut dist_max: f32 = 0.0;
    for corner in aabb_corners(aabb) {
        let dist = (view * corner).length();
        if dist.is_finite() {
            dist_max = dist_max.max(dist);
        }
    }
    (0.0, 1.0 / dist_max.max(1e-4))
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

#[cfg(test)]
mod tests {
    use super::*;

    // A camera rotation must not change the distance-sort normalization: the
    // re-sorted order then reproduces bit-identically and nothing flashes.
    #[test]
    fn depth_range_is_rotation_invariant() {
        let aabb = Aabb::new(Vector3::new(-3.0, -2.0, -5.0), Vector3::new(8.0, 4.0, 9.0));
        let view = Transform3D::IDENTITY.translated(Vector3::new(0.5, -1.0, 2.0));
        let rotated = Transform3D::default()
            .rotated(Vector3::UP, 0.7)
            .rotated(Vector3::RIGHT, -0.3)
            * view;
        let (min_a, inv_a) = depth_range(view, aabb);
        let (min_b, inv_b) = depth_range(rotated, aabb);
        assert_eq!(min_a, 0.0);
        assert_eq!(min_b, 0.0);
        let range_a = 1.0 / inv_a;
        let range_b = 1.0 / inv_b;
        assert!(
            (range_a - range_b).abs() < range_a * 1.0e-5,
            "rotation changed the distance range: {range_a} vs {range_b}"
        );
    }

    // With the camera inside the cloud the lower bound must stay 0 so nearby
    // splats keep distinct buckets instead of clamping into one.
    #[test]
    fn depth_range_lower_bound_is_zero_inside() {
        let aabb = Aabb::new(
            Vector3::new(-10.0, -5.0, -10.0),
            Vector3::new(20.0, 10.0, 20.0),
        );
        let inside = Transform3D::IDENTITY; // camera at the cloud center
        let (min_d, inv) = depth_range(inside, aabb);
        assert_eq!(min_d, 0.0);
        let range = 1.0 / inv;
        let half_diag = (Vector3::new(10.0, 5.0, 10.0)).length();
        assert!((range - half_diag).abs() < 1.0e-3);
    }
}
