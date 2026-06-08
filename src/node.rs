use godot::classes::image::Format as ImageFormat;
use godot::classes::mesh::{ArrayType, PrimitiveType};
use godot::classes::rendering_device::{
    DataFormat, ShaderLanguage, ShaderStage, TextureUsageBits, UniformType,
};
use godot::classes::GltfState;
use godot::classes::{
    ArrayMesh, Engine, Image, ImageTexture, MeshInstance3D, RdShaderSource, RdTextureFormat,
    RdTextureView, RdUniform, RenderingDevice, RenderingServer, Shader, ShaderMaterial, Texture2D,
    Texture2Drd,
};
use godot::prelude::*;

use crate::asset::GaussianSplatAsset;
use crate::backend::{GaussianSplatBackendSettings, BACKEND_PROFILE_DESKTOP};
use crate::cloud_settings::GaussianSplatCloudSettings;
use crate::import_state::{ImportedSplatMetadata, NODE_STATE_KEY, POINT_STRIDE_FLOATS};
use crate::render_packet::GaussianSplatRenderPacket;

// Texture-driven anisotropic Gaussian splat shader (Step 1 render path). Per-splat
// data (center, packed 3D covariance upper triangle, color) lives in `data_tex`,
// four RGBA-float texels per splat. The mesh is a static "slot" mesh: four
// vertices per splat at the origin, UV = quad corner in [-2, 2], UV2.x = slot
// index. Each slot resolves to a splat id via `sort_tex` when `sort_enabled`,
// otherwise slot == id (unsorted, matching the Phase 1 look). The resolved splat's
// covariance is projected to screen space with the perspective Jacobian and the
// corner is stretched along the projected ellipse axes, so the on-screen footprint
// is an oriented ellipse. Alpha is an isotropic Gaussian in the stretched corner
// space, which equals the anisotropic Gaussian on screen.
// NOTE: with `sort_enabled == 0` splats are not depth-sorted, so blending order is
// only approximate; the GPU compute sort (Step 2) drives `sort_tex` for the correct
// back-to-front order.
const SPLAT_TEXTURE_SHADER: &str = r#"
shader_type spatial;
render_mode unshaded, cull_disabled, blend_mix, depth_draw_never;

uniform sampler2D data_tex : filter_nearest;
uniform sampler2D sort_tex : filter_nearest;
uniform int splat_count;
uniform int sort_enabled;

varying vec2 v_corner;
varying vec4 v_color;

void vertex() {
    v_corner = UV;
    int slot = int(UV2.x + 0.5);
    int splat_id = slot;
    if (sort_enabled > 0) {
        int sw = textureSize(sort_tex, 0).x;
        splat_id = int(texelFetch(sort_tex, ivec2(slot % sw, slot / sw), 0).r + 0.5);
    }
    splat_id = clamp(splat_id, 0, splat_count - 1);

    // Fetch the splat's four-texel data block from the data texture.
    int w = textureSize(data_tex, 0).x;
    int base = splat_id * 4;
    vec4 t0 = texelFetch(data_tex, ivec2(base % w, base / w), 0);
    vec4 t1 = texelFetch(data_tex, ivec2((base + 1) % w, (base + 1) / w), 0);
    vec4 t2 = texelFetch(data_tex, ivec2((base + 2) % w, (base + 2) / w), 0);
    vec4 t3 = texelFetch(data_tex, ivec2((base + 3) % w, (base + 3) / w), 0);

    vec3 center = t0.xyz;
    v_color = t3;
    // Reconstruct the symmetric local-space 3D covariance from its upper triangle.
    mat3 cov3d = mat3(
        vec3(t1.x, t1.y, t1.z),
        vec3(t1.y, t1.w, t2.x),
        vec3(t1.z, t2.x, t2.y));

    vec4 center_view = VIEW_MATRIX * MODEL_MATRIX * vec4(center, 1.0);
    vec4 center_clip = PROJECTION_MATRIX * center_view;

    float z = center_view.z;
    if (z > -0.01) {
        // Behind or too close to the camera: push offscreen so the quad clips.
        POSITION = vec4(0.0, 0.0, 100.0, 1.0);
    } else {
        // Local covariance -> view space (upper-left 3x3 of view * model).
        mat3 view_linear = mat3(VIEW_MATRIX[0].xyz, VIEW_MATRIX[1].xyz, VIEW_MATRIX[2].xyz);
        mat3 model_linear = mat3(MODEL_MATRIX[0].xyz, MODEL_MATRIX[1].xyz, MODEL_MATRIX[2].xyz);
        mat3 W = view_linear * model_linear;
        mat3 cov_view = W * cov3d * transpose(W);

        // Jacobian of the perspective projection (focal lengths in pixels). The
        // perspective terms go in the third column (column-major construction).
        vec2 vp = VIEWPORT_SIZE;
        float fx = PROJECTION_MATRIX[0][0] * vp.x * 0.5;
        float fy = PROJECTION_MATRIX[1][1] * vp.y * 0.5;
        mat3 jacobian = mat3(
            vec3(fx / z, 0.0, 0.0),
            vec3(0.0, fy / z, 0.0),
            vec3(-(fx * center_view.x) / (z * z), -(fy * center_view.y) / (z * z), 0.0));
        mat3 cov2d = jacobian * cov_view * transpose(jacobian);

        // Dilate slightly so sub-pixel splats stay visible (anti-aliasing).
        float a = cov2d[0][0] + 0.3;
        float b = cov2d[0][1];
        float d = cov2d[1][1] + 0.3;

        // Eigenvalues -> screen-space major/minor axes (pixels).
        float mid = 0.5 * (a + d);
        float disc = sqrt(max(mid * mid - (a * d - b * b), 0.0));
        float lambda1 = mid + disc;
        float lambda2 = max(mid - disc, 0.0);
        vec2 axis_dir = normalize(vec2(b, lambda1 - a) + vec2(1e-6, 0.0));
        vec2 major_axis = min(sqrt(2.0 * lambda1), 1024.0) * axis_dir;
        vec2 minor_axis = min(sqrt(2.0 * lambda2), 1024.0) * vec2(axis_dir.y, -axis_dir.x);

        // Expand the quad corner along the projected ellipse axes, in clip space.
        vec2 screen_offset = v_corner.x * major_axis + v_corner.y * minor_axis;
        vec2 clip_offset = (screen_offset / vp) * 2.0 * center_clip.w;
        POSITION = center_clip + vec4(clip_offset, 0.0, 0.0);
    }
}

void fragment() {
    float power = -dot(v_corner, v_corner);
    if (power < -4.0) {
        discard;
    }
    float alpha = v_color.a * exp(power);
    if (alpha < 0.0039) {
        discard;
    }
    ALBEDO = v_color.rgb;
    ALPHA = alpha;
}
"#;

// Width (in RGBA-float texels) of the per-splat data texture. Each splat occupies
// four consecutive texels, so a row holds SPLAT_DATA_TEX_WIDTH / 4 splats.
const SPLAT_DATA_TEX_WIDTH: i32 = 4096;

// CPU-built render data for the texture-driven splat path: the per-splat data
// texture bytes plus the static slot mesh arrays.
struct SplatRenderData {
    data_bytes: PackedByteArray,
    tex_width: i32,
    tex_height: i32,
    positions: PackedVector3Array,
    uvs: PackedVector2Array,
    uv2: PackedVector2Array,
    indices: PackedInt32Array,
    // Local-space splat centers as vec4(x, y, z, 1) in slot order, used to seed
    // the GPU sort's positions storage buffer (Step 2).
    positions_ssbo: PackedByteArray,
    splat_count: i32,
    aabb: Aabb,
}

// --- Step 2: GPU counting sort -------------------------------------------------
// Depth bucket count (sort precision) and compute workgroup size, mirroring the
// validated PoC. The sort-order texture is R32F; one texel per splat holds the
// splat id to draw at that slot, sampled by SPLAT_TEXTURE_SHADER.
const SORT_NUM_BUCKETS: i32 = 2048;
const SORT_LOCAL_SIZE: u32 = 256;
const SORT_TEX_WIDTH: i32 = 4096;

// Pass 1: count splats per depth bucket (bucket 0 = farthest).
const SORT_COUNT_GLSL: &str = r#"#version 450
layout(local_size_x = 256) in;
layout(set = 0, binding = 0, std430) restrict readonly buffer Pos { vec4 positions[]; };
layout(set = 0, binding = 1, std430) restrict buffer Hist { uint histogram[]; };
layout(push_constant, std430) uniform PC {
    mat4 view;
    float depth_min;
    float depth_inv_range;
    uint num_buckets;
    uint count;
    uint tex_width;
} pc;
void main() {
    uint i = gl_GlobalInvocationID.x;
    if (i >= pc.count) { return; }
    float vz = (pc.view * vec4(positions[i].xyz, 1.0)).z;
    float t = clamp((-vz - pc.depth_min) * pc.depth_inv_range, 0.0, 1.0);
    uint bucket = uint((1.0 - t) * float(pc.num_buckets - 1u) + 0.5);
    atomicAdd(histogram[bucket], 1u);
}
"#;

// Pass 2: serial exclusive prefix sum of the histogram into per-bucket offsets.
const SORT_SCAN_GLSL: &str = r#"#version 450
layout(local_size_x = 1) in;
layout(set = 0, binding = 1, std430) restrict readonly buffer Hist { uint histogram[]; };
layout(set = 0, binding = 2, std430) restrict buffer Off { uint offsets[]; };
layout(push_constant, std430) uniform PC {
    mat4 view;
    float depth_min;
    float depth_inv_range;
    uint num_buckets;
    uint count;
    uint tex_width;
} pc;
void main() {
    uint sum = 0u;
    for (uint b = 0u; b < pc.num_buckets; ++b) {
        offsets[b] = sum;
        sum += histogram[b];
    }
}
"#;

// Pass 3: scatter each splat id into its sorted slot in the R32F sort texture.
const SORT_SCATTER_GLSL: &str = r#"#version 450
layout(local_size_x = 256) in;
layout(set = 0, binding = 0, std430) restrict readonly buffer Pos { vec4 positions[]; };
layout(set = 0, binding = 2, std430) restrict buffer Off { uint offsets[]; };
layout(set = 0, binding = 3, r32f) uniform restrict writeonly image2D sort_img;
layout(push_constant, std430) uniform PC {
    mat4 view;
    float depth_min;
    float depth_inv_range;
    uint num_buckets;
    uint count;
    uint tex_width;
} pc;
void main() {
    uint i = gl_GlobalInvocationID.x;
    if (i >= pc.count) { return; }
    float vz = (pc.view * vec4(positions[i].xyz, 1.0)).z;
    float t = clamp((-vz - pc.depth_min) * pc.depth_inv_range, 0.0, 1.0);
    uint bucket = uint((1.0 - t) * float(pc.num_buckets - 1u) + 0.5);
    uint slot = atomicAdd(offsets[bucket], 1u);
    ivec2 c = ivec2(int(slot % pc.tex_width), int(slot / pc.tex_width));
    imageStore(sort_img, c, vec4(float(i), 0.0, 0.0, 0.0));
}
"#;

// Runtime GPU counting-sort state. RIDs live on the main RenderingDevice; the
// per-frame dispatch runs via RenderingServer::call_on_render_thread.
struct SortGpu {
    ready: bool,
    attempted: bool,
    enabled_in_shader: bool,
    dispatched_once: bool,
    // Inputs stashed when the splat render is (re)built.
    positions: PackedByteArray,
    splat_count: i32,
    local_aabb: Aabb,
    // GPU resources.
    tex_width: i32,
    tex_height: i32,
    pos_buf: Rid,
    hist_buf: Rid,
    off_buf: Rid,
    sort_tex_rid: Rid,
    count_shader: Rid,
    scan_shader: Rid,
    scatter_shader: Rid,
    count_pipe: Rid,
    scan_pipe: Rid,
    scatter_pipe: Rid,
    count_set: Rid,
    scan_set: Rid,
    scatter_set: Rid,
    texture: Option<Gd<Texture2Drd>>,
}

impl Default for SortGpu {
    fn default() -> Self {
        Self {
            ready: false,
            attempted: false,
            enabled_in_shader: false,
            dispatched_once: false,
            positions: PackedByteArray::new(),
            splat_count: 0,
            local_aabb: Aabb::default(),
            tex_width: 0,
            tex_height: 0,
            pos_buf: Rid::Invalid,
            hist_buf: Rid::Invalid,
            off_buf: Rid::Invalid,
            sort_tex_rid: Rid::Invalid,
            count_shader: Rid::Invalid,
            scan_shader: Rid::Invalid,
            scatter_shader: Rid::Invalid,
            count_pipe: Rid::Invalid,
            scan_pipe: Rid::Invalid,
            scatter_pipe: Rid::Invalid,
            count_set: Rid::Invalid,
            scan_set: Rid::Invalid,
            scatter_set: Rid::Invalid,
            texture: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct NodeTransformState {
    imported_transform: Transform3D,
    effective_transform: Transform3D,
}

#[derive(Clone, Debug)]
struct NodeVisibilityState {
    runtime_visible: bool,
    asset_ready: bool,
}

impl Default for NodeVisibilityState {
    fn default() -> Self {
        Self {
            runtime_visible: true,
            asset_ready: false,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct NodeBackendState {
    revision: i64,
    asset_point_count: i32,
    profile_hint: String,
}

#[derive(GodotClass)]
#[class(tool, init, base=Node3D)]
pub struct GaussianSplatNode3D {
    #[base]
    base: Base<Node3D>,

    asset: Option<Gd<GaussianSplatAsset>>,
    cloud_settings: Option<Gd<GaussianSplatCloudSettings>>,
    backend_settings: Option<Gd<GaussianSplatBackendSettings>>,
    render_packet: Option<Gd<GaussianSplatRenderPacket>>,
    #[var(get, set)]
    #[export]
    preview_max_splats: PhantomVar<i32>,
    #[var(get, set)]
    #[export]
    preview_max_splat_radius: PhantomVar<f32>,
    #[var(get, set)]
    #[export]
    preview_scale_multiplier: PhantomVar<f32>,
    #[var(get, set, usage_flags = [EDITOR])]
    show_all_preview_splats_action: PhantomVar<bool>,
    // The decoded asset is not serialized into the .scn, so persist the point
    // count here to recover it after a scene reload.
    #[var(get, set, usage_flags = [STORAGE])]
    imported_point_count: PhantomVar<i32>,
    metadata: ImportedSplatMetadata,
    is_bound: bool,
    transform_state: NodeTransformState,
    visibility_state: NodeVisibilityState,
    backend_state: NodeBackendState,
    debug_mesh_instance: Option<Gd<MeshInstance3D>>,
    sort: SortGpu,
}

#[godot_api]
impl INode3D for GaussianSplatNode3D {
    fn ready(&mut self) {
        // Reconnect to the baked render child when instanced from a pre-imported
        // .scn (the field itself is not serialized); also marks renderable data.
        self.adopt_serialized_render();
        self.sync_runtime_state();
        self.sync_node_name();
        // The GPU sort runs at runtime only; the editor keeps the unsorted
        // Step 1 preview.
        if !Engine::singleton().is_editor_hint() {
            self.base_mut().set_process(true);
        }
    }

    fn process(&mut self, _delta: f64) {
        if Engine::singleton().is_editor_hint() {
            return;
        }
        if !self.sort.ready && !self.sort.attempted {
            self.try_enable_sort();
        }
        if self.sort.ready {
            self.dispatch_sort();
            // Enable sorted sampling one frame after setup, so the sort texture is
            // registered and holds valid contents before the material binds it.
            if self.sort.dispatched_once && !self.sort.enabled_in_shader {
                self.set_material_sort(true);
                self.sort.enabled_in_shader = true;
            }
            self.sort.dispatched_once = true;
        }
    }

    fn exit_tree(&mut self) {
        self.teardown_sort();
    }
}

#[godot_api]
impl GaussianSplatNode3D {
    #[func]
    pub fn bind_asset(&mut self, asset: Option<Gd<GaussianSplatAsset>>) {
        self.asset = asset;
        self.ensure_cloud_settings();
        self.ensure_backend_settings();
        self.refresh_from_asset();
    }

    #[func]
    pub fn unbind_asset(&mut self) {
        self.asset = None;
        self.metadata = ImportedSplatMetadata::default();
        self.is_bound = false;
        self.clear_render_packet();
        self.clear_debug_mesh();
        self.sync_node_name();
    }

    #[func]
    pub fn has_asset(&self) -> bool {
        self.asset.is_some()
    }

    #[func]
    pub fn is_bound(&self) -> bool {
        self.is_bound
    }

    #[func]
    pub fn get_metadata_summary(&self) -> GString {
        GString::from(self.metadata.summary().as_str())
    }

    #[func]
    pub fn set_import_metadata(&mut self, metadata: VarDictionary) {
        self.metadata = ImportedSplatMetadata::from_dictionary(metadata);
        self.is_bound = true;
        self.mark_backend_dirty("metadata");
        self.sync_node_name();
    }

    #[func]
    pub fn export_import_metadata(&self) -> VarDictionary {
        self.metadata.to_dictionary()
    }

    #[func]
    pub fn get_asset(&self) -> Option<Gd<GaussianSplatAsset>> {
        self.asset.clone()
    }

    #[func]
    pub fn get_imported_point_count(&self) -> i32 {
        self.backend_state.asset_point_count
    }

    #[func]
    pub fn set_imported_point_count(&mut self, point_count: i32) {
        self.backend_state.asset_point_count = point_count.max(0);
    }

    #[func]
    pub fn bind_cloud_settings(&mut self, cloud_settings: Option<Gd<GaussianSplatCloudSettings>>) {
        self.cloud_settings = cloud_settings;
        self.ensure_cloud_settings();
        self.mark_backend_dirty("cloud_settings");
        self.rebuild_debug_mesh();
    }

    #[func]
    pub fn get_cloud_settings(&self) -> Option<Gd<GaussianSplatCloudSettings>> {
        self.cloud_settings.clone()
    }

    #[func]
    pub fn get_preview_max_splats(&self) -> i32 {
        self.cloud_settings
            .as_ref()
            .map(|settings| settings.bind().get_max_debug_splats())
            .unwrap_or(i32::MAX)
    }

    #[func]
    pub fn set_preview_max_splats(&mut self, max_splats: i32) {
        self.ensure_cloud_settings();
        let max_splats = self.clamp_preview_max_splats(max_splats);
        if let Some(settings) = &mut self.cloud_settings {
            settings.bind_mut().set_max_debug_splats(max_splats);
        }
        self.mark_backend_dirty("preview_max_splats");
        self.rebuild_debug_mesh();
    }

    #[func]
    pub fn get_preview_max_splat_radius(&self) -> f32 {
        self.cloud_settings
            .as_ref()
            .map(|settings| settings.bind().get_max_debug_splat_radius())
            .unwrap_or(0.02)
    }

    #[func]
    pub fn set_preview_max_splat_radius(&mut self, max_splat_radius: f32) {
        self.ensure_cloud_settings();
        if let Some(settings) = &mut self.cloud_settings {
            settings
                .bind_mut()
                .set_max_debug_splat_radius(max_splat_radius);
        }
        self.mark_backend_dirty("preview_max_splat_radius");
        self.rebuild_debug_mesh();
    }

    #[func]
    pub fn get_preview_scale_multiplier(&self) -> f32 {
        self.cloud_settings
            .as_ref()
            .map(|settings| settings.bind().get_gaussian_scale_multiplier())
            .unwrap_or(1.0)
    }

    #[func]
    pub fn set_preview_scale_multiplier(&mut self, scale_multiplier: f32) {
        self.ensure_cloud_settings();
        if let Some(settings) = &mut self.cloud_settings {
            settings
                .bind_mut()
                .set_gaussian_scale_multiplier(scale_multiplier);
        }
        self.mark_backend_dirty("preview_scale_multiplier");
        self.rebuild_debug_mesh();
    }

    #[func]
    pub fn show_all_preview_splats(&mut self) {
        let asset_point_count = self
            .asset
            .as_ref()
            .map(|asset| asset.bind().get_point_count())
            .unwrap_or(0);
        self.set_preview_max_splats(asset_point_count);
    }

    #[func]
    pub fn get_show_all_preview_splats_action(&self) -> bool {
        false
    }

    #[func]
    pub fn set_show_all_preview_splats_action(&mut self, enabled: bool) {
        if enabled {
            self.show_all_preview_splats();
        }
    }

    #[func]
    pub fn bind_backend_settings(
        &mut self,
        backend_settings: Option<Gd<GaussianSplatBackendSettings>>,
    ) {
        self.backend_settings = backend_settings;
        self.ensure_backend_settings();
        self.backend_state.profile_hint = self.resolve_backend_pipeline();
        self.mark_backend_dirty("backend_settings");
        self.refresh_render_packet();
    }

    #[func]
    pub fn get_backend_settings(&self) -> Option<Gd<GaussianSplatBackendSettings>> {
        self.backend_settings.clone()
    }

    #[func]
    pub fn get_render_packet(&self) -> Option<Gd<GaussianSplatRenderPacket>> {
        self.render_packet.clone()
    }

    #[func]
    pub fn set_imported_transform(&mut self, transform: Transform3D) {
        self.transform_state.imported_transform = transform;
        self.transform_state.effective_transform = transform;
        self.base_mut().set_transform(transform);
        self.mark_backend_dirty("import_transform");
    }

    #[func]
    pub fn get_imported_transform(&self) -> Transform3D {
        self.transform_state.imported_transform
    }

    #[func]
    pub fn set_runtime_visible(&mut self, visible: bool) {
        self.visibility_state.runtime_visible = visible;
        self.sync_runtime_state();
    }

    #[func]
    pub fn is_runtime_visible(&self) -> bool {
        self.visibility_state.runtime_visible
    }

    #[func]
    pub fn get_backend_revision(&self) -> i64 {
        self.backend_state.revision
    }

    #[func]
    pub fn export_runtime_state(&self) -> VarDictionary {
        let mut dict = VarDictionary::new();
        dict.set("is_bound", self.is_bound);
        dict.set("runtime_visible", self.visibility_state.runtime_visible);
        dict.set("asset_ready", self.visibility_state.asset_ready);
        dict.set("backend_revision", self.backend_state.revision);
        dict.set(
            "asset_point_count",
            self.backend_state.asset_point_count as i64,
        );
        dict.set(
            "backend_profile_hint",
            self.backend_state.profile_hint.as_str(),
        );
        dict.set("metadata", &Variant::from(self.metadata.to_dictionary()));
        dict
    }

    #[func]
    pub fn export_backend_model(&self) -> VarDictionary {
        let mut dict = self.export_runtime_state();
        let pipeline = self.resolve_backend_pipeline();
        dict.set("pipeline", pipeline.as_str());
        if let Some(backend_settings) = &self.backend_settings {
            let settings_ref = backend_settings.bind();
            dict.set(
                "backend_settings",
                &Variant::from(settings_ref.export_settings()),
            );
        }
        if let Some(asset) = &self.asset {
            let asset_ref = asset.bind();
            dict.set(
                "asset_payload_layout",
                &Variant::from(asset_ref.get_payload_layout()),
            );
            dict.set(
                "asset_fallback_mode",
                &Variant::from(asset_ref.get_fallback_mode()),
            );
        }
        if let Some(render_packet) = &self.render_packet {
            let packet_ref = render_packet.bind();
            dict.set("render_packet", &Variant::from(packet_ref.export_packet()));
        }
        dict.set("preview_max_splats", self.get_preview_max_splats() as i64);
        dict.set(
            "preview_max_splat_radius",
            self.get_preview_max_splat_radius(),
        );
        dict.set(
            "preview_scale_multiplier",
            self.get_preview_scale_multiplier(),
        );
        dict
    }

    #[func]
    pub fn stash_on_state(&self, state: Option<Gd<GltfState>>) {
        if let Some(mut state) = state {
            let dict = self.metadata.to_dictionary();
            state.set_additional_data(NODE_STATE_KEY, &Variant::from(dict));
        }
    }

    fn refresh_from_asset(&mut self) {
        self.ensure_cloud_settings();
        self.ensure_backend_settings();
        self.ensure_render_packet();
        if let Some(asset) = &self.asset {
            let asset = asset.clone();
            let asset_ref = asset.bind();
            self.metadata =
                ImportedSplatMetadata::from_dictionary(asset_ref.export_import_metadata());
            self.is_bound = true;
            self.visibility_state.asset_ready = true;
            self.backend_state.asset_point_count = asset_ref.get_point_count();
            self.backend_state.profile_hint = self.resolve_backend_pipeline();
            drop(asset_ref);
            self.clamp_preview_settings_to_asset();
        } else {
            self.metadata = ImportedSplatMetadata::default();
            self.is_bound = false;
            self.visibility_state.asset_ready = false;
            self.backend_state.asset_point_count = 0;
            self.backend_state.profile_hint.clear();
        }
        self.mark_backend_dirty("asset");
        self.refresh_render_packet();
        self.rebuild_debug_mesh();
        self.sync_runtime_state();
        self.sync_node_name();
    }

    fn sync_runtime_state(&mut self) {
        let should_be_visible =
            self.visibility_state.runtime_visible && self.visibility_state.asset_ready;
        self.base_mut().set_visible(should_be_visible);
        self.transform_state.effective_transform = self.base().get_transform();
    }

    fn mark_backend_dirty(&mut self, reason: &str) {
        self.backend_state.revision += 1;
        if self.backend_state.profile_hint.is_empty() {
            self.backend_state.profile_hint = reason.to_string();
        }
    }

    fn ensure_backend_settings(&mut self) {
        if self.backend_settings.is_none() {
            let mut backend_settings = GaussianSplatBackendSettings::new_gd();
            backend_settings
                .bind_mut()
                .set_target_hint(BACKEND_PROFILE_DESKTOP.into());
            self.backend_settings = Some(backend_settings);
        }
    }

    fn ensure_cloud_settings(&mut self) {
        if self.cloud_settings.is_none() {
            self.cloud_settings = Some(GaussianSplatCloudSettings::new_gd());
        }
    }

    fn ensure_render_packet(&mut self) {
        if self.render_packet.is_none() {
            self.render_packet = Some(GaussianSplatRenderPacket::new_gd());
        }
    }

    fn clamp_preview_settings_to_asset(&mut self) {
        let max_splats = self.clamp_preview_max_splats(self.get_preview_max_splats());
        if let Some(settings) = &mut self.cloud_settings {
            settings.bind_mut().set_max_debug_splats(max_splats);
        }
    }

    fn clamp_preview_max_splats(&self, max_splats: i32) -> i32 {
        let asset_point_count = self
            .asset
            .as_ref()
            .map(|asset| asset.bind().get_point_count())
            .unwrap_or(0);
        max_splats.max(0).min(asset_point_count)
    }

    fn clear_render_packet(&mut self) {
        if let Some(render_packet) = &mut self.render_packet {
            render_packet.bind_mut().clear();
        }
    }

    fn refresh_render_packet(&mut self) {
        let Some(asset) = &self.asset else {
            self.clear_render_packet();
            return;
        };
        let Some(render_packet) = &mut self.render_packet else {
            return;
        };

        let pipeline = self.backend_state.profile_hint.clone();
        render_packet.bind_mut().prepare_from_asset(
            asset,
            pipeline.as_str(),
            self.backend_state.revision,
        );
    }

    fn resolve_backend_pipeline(&self) -> String {
        self.backend_settings
            .as_ref()
            .map(|backend_settings| {
                backend_settings
                    .bind()
                    .resolve_pipeline_for_metadata(&self.metadata)
            })
            .unwrap_or_else(|| "unconfigured".to_string())
    }

    fn sync_node_name(&mut self) {
        let name = if self.is_bound {
            let summary = self.metadata.summary();
            format!("GaussianSplatNode3D ({summary})")
        } else {
            "GaussianSplatNode3D".to_string()
        };
        self.base_mut().set_name(name.as_str());
    }

    fn ensure_debug_mesh_instance(&mut self) {
        if self.debug_mesh_instance.is_some() {
            return;
        }

        let mut mesh_instance = MeshInstance3D::new_alloc();
        mesh_instance.set_name("DebugPointCloud");
        self.base_mut()
            .add_child(&mesh_instance.clone().upcast::<Node>());
        self.debug_mesh_instance = Some(mesh_instance);
    }

    fn clear_debug_mesh(&mut self) {
        if let Some(mesh_instance) = &mut self.debug_mesh_instance {
            mesh_instance.set_visible(false);
        }
    }

    fn rebuild_debug_mesh(&mut self) {
        let Some(asset) = self.asset.clone() else {
            self.clear_debug_mesh();
            return;
        };
        let cloud_settings = self.cloud_settings.clone();

        if !cloud_settings
            .as_ref()
            .map(|settings| settings.bind().is_debug_fallback_enabled())
            .unwrap_or(false)
        {
            self.clear_debug_mesh();
            return;
        }

        let Some(render) = self.build_splat_render_data(&asset, cloud_settings.as_ref()) else {
            self.clear_debug_mesh();
            return;
        };

        // Per-splat data texture (four RGBA-float texels per splat).
        let Some(image) = Image::create_from_data(
            render.tex_width,
            render.tex_height,
            false,
            ImageFormat::RGBAF,
            &render.data_bytes,
        ) else {
            self.clear_debug_mesh();
            return;
        };
        let Some(data_texture) = ImageTexture::create_from_image(&image) else {
            self.clear_debug_mesh();
            return;
        };

        self.ensure_debug_mesh_instance();
        let Some(mesh_instance) = &mut self.debug_mesh_instance else {
            return;
        };

        // Static slot mesh: four origin vertices per splat, expanded in the shader.
        let mut arrays = VarArray::new();
        for _ in 0..ArrayType::MAX.ord() {
            arrays.push(&Variant::nil());
        }
        arrays.set(
            ArrayType::VERTEX.ord() as usize,
            &Variant::from(render.positions),
        );
        arrays.set(ArrayType::TEX_UV.ord() as usize, &Variant::from(render.uvs));
        arrays.set(
            ArrayType::TEX_UV2.ord() as usize,
            &Variant::from(render.uv2),
        );
        arrays.set(
            ArrayType::INDEX.ord() as usize,
            &Variant::from(render.indices),
        );

        let mut mesh = ArrayMesh::new_gd();
        mesh.add_surface_from_arrays_ex(PrimitiveType::TRIANGLES, &arrays)
            .done();
        // Vertices sit at the origin and are expanded in the shader, so the mesh
        // needs an explicit AABB covering the splat cloud to avoid frustum culling.
        mesh.set_custom_aabb(render.aabb);

        let mut shader = Shader::new_gd();
        shader.set_code(SPLAT_TEXTURE_SHADER);

        let mut material = ShaderMaterial::new_gd();
        material.set_shader(&shader);
        material.set_shader_parameter("data_tex", &Variant::from(data_texture));
        material.set_shader_parameter("splat_count", &Variant::from(render.splat_count));
        // Step 1 renders unsorted (slot == id); the compute sort (Step 2) flips this on.
        material.set_shader_parameter("sort_enabled", &Variant::from(0_i32));

        let mesh_resource = mesh.upcast::<godot::classes::Mesh>();
        let material_resource = material.upcast::<godot::classes::Material>();
        mesh_instance.set_mesh(&mesh_resource);
        mesh_instance.set_material_override(&material_resource);
        mesh_instance.set_visible(
            cloud_settings
                .as_ref()
                .map(|settings| settings.bind().is_debug_visible())
                .unwrap_or(true),
        );

        // Stash GPU sort inputs (Step 2). A rebuild invalidates any prior sort
        // (the material above already starts with sort_enabled = 0).
        self.teardown_sort();
        self.sort.positions = render.positions_ssbo;
        self.sort.splat_count = render.splat_count;
        self.sort.local_aabb = render.aabb;
        self.sort.attempted = false;
    }

    fn build_splat_render_data(
        &self,
        asset: &Gd<GaussianSplatAsset>,
        cloud_settings: Option<&Gd<GaussianSplatCloudSettings>>,
    ) -> Option<SplatRenderData> {
        let values = {
            let asset_ref = asset.bind();
            asset_ref.payload_float_values()?
        };

        let source_point_count = values.len() / POINT_STRIDE_FLOATS;
        let max_splats = cloud_settings
            .map(|settings| settings.bind().get_max_debug_splats().max(0) as usize)
            .unwrap_or(usize::MAX);
        let point_count = source_point_count.min(max_splats);
        if point_count == 0 || point_count > (i32::MAX as usize / 4) {
            return None;
        }
        let sample_stride = source_point_count.div_ceil(point_count);

        let scale_multiplier = cloud_settings
            .map(|settings| settings.bind().get_gaussian_scale_multiplier())
            .unwrap_or(1.0)
            .max(0.01);

        // Data texture: four RGBA-float texels per splat. Texel 0 = center.xyz,
        // texels 1-2 = packed covariance upper triangle, texel 3 = color rgba.
        let tex_width = SPLAT_DATA_TEX_WIDTH as usize;
        let tex_height = (point_count * 4).div_ceil(tex_width).max(1);
        let mut data = vec![0.0_f32; tex_width * tex_height * 4];

        // Quad corners in [-2, 2]; the vertex shader stretches them along the
        // projected ellipse axes, so the corner doubles as the Gaussian sample
        // coordinate (alpha = exp(-dot(corner, corner))).
        let corners = [
            Vector2::new(-2.0, -2.0),
            Vector2::new(2.0, -2.0),
            Vector2::new(2.0, 2.0),
            Vector2::new(-2.0, 2.0),
        ];

        let mut positions = Vec::with_capacity(point_count * 4);
        let mut uvs = Vec::with_capacity(point_count * 4);
        let mut uv2 = Vec::with_capacity(point_count * 4);
        let mut indices = Vec::with_capacity(point_count * 6);
        let mut positions_ssbo = Vec::with_capacity(point_count * 4);

        let mut min = Vector3::new(f32::INFINITY, f32::INFINITY, f32::INFINITY);
        let mut max = Vector3::new(f32::NEG_INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY);

        for slot in 0..point_count {
            let point_index = (slot * sample_stride).min(source_point_count - 1);
            let offset = point_index * POINT_STRIDE_FLOATS;
            let center = Vector3::new(values[offset], values[offset + 1], values[offset + 2]);
            let cov = covariance_upper_triangle(
                [
                    values[offset + 3],
                    values[offset + 4],
                    values[offset + 5],
                    values[offset + 6],
                ],
                [
                    values[offset + 7] * scale_multiplier,
                    values[offset + 8] * scale_multiplier,
                    values[offset + 9] * scale_multiplier,
                ],
            );

            // Pack into the splat's four-texel block (16 contiguous floats).
            let base = slot * 16;
            data[base] = center.x;
            data[base + 1] = center.y;
            data[base + 2] = center.z;
            data[base + 4] = cov[0];
            data[base + 5] = cov[1];
            data[base + 6] = cov[2];
            data[base + 7] = cov[3];
            data[base + 8] = cov[4];
            data[base + 9] = cov[5];
            data[base + 12] = values[offset + 14];
            data[base + 13] = values[offset + 15];
            data[base + 14] = values[offset + 16];
            data[base + 15] = values[offset + 17];

            min.x = min.x.min(center.x);
            min.y = min.y.min(center.y);
            min.z = min.z.min(center.z);
            max.x = max.x.max(center.x);
            max.y = max.y.max(center.y);
            max.z = max.z.max(center.z);

            positions_ssbo.extend_from_slice(&[center.x, center.y, center.z, 1.0]);

            let slot_coord = Vector2::new(slot as f32, 0.0);
            for corner in corners {
                positions.push(Vector3::ZERO);
                uvs.push(corner);
                uv2.push(slot_coord);
            }

            let quad = (slot * 4) as i32;
            indices.extend_from_slice(&[quad, quad + 1, quad + 2, quad, quad + 2, quad + 3]);
        }

        let size = max - min;
        // Grow the bounds so splats extending past their centers stay visible.
        let aabb = Aabb::new(min, size).grow(size.length() * 0.05 + 0.01);

        Some(SplatRenderData {
            data_bytes: PackedFloat32Array::from(data).to_byte_array(),
            tex_width: tex_width as i32,
            tex_height: tex_height as i32,
            positions: PackedVector3Array::from(positions),
            uvs: PackedVector2Array::from(uvs),
            uv2: PackedVector2Array::from(uv2),
            indices: PackedInt32Array::from(indices),
            positions_ssbo: PackedFloat32Array::from(positions_ssbo).to_byte_array(),
            splat_count: point_count as i32,
            aabb,
        })
    }

    // Reconnect the field to the baked render child when the node is deserialized
    // from a pre-imported .scn (the field itself is not serialized). A baked mesh
    // means there is renderable data even without a live asset.
    fn adopt_serialized_render(&mut self) {
        if self.debug_mesh_instance.is_some() {
            return;
        }
        for child in self.base().get_children().iter_shared() {
            if let Ok(mesh_instance) = child.try_cast::<MeshInstance3D>() {
                if mesh_instance.get_mesh().is_some() {
                    self.visibility_state.asset_ready = true;
                    self.debug_mesh_instance = Some(mesh_instance);
                    return;
                }
            }
        }
    }

    // One-shot attempt to bring up the GPU sort once renderable data exists.
    fn try_enable_sort(&mut self) {
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
    fn reconstruct_sort_inputs_from_material(&mut self) {
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

        if image.get_format() == ImageFormat::RGBAF {
            // Fast path: read the raw float bytes (16 floats per splat).
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
        } else {
            // Format-agnostic fallback.
            let width = image.get_width();
            if width <= 0 {
                return;
            }
            for i in 0..count {
                let texel = (i * 4) as i32;
                let pixel = image.get_pixel(texel % width, texel / width);
                let center = Vector3::new(pixel.r, pixel.g, pixel.b);
                positions.extend_from_slice(&[center.x, center.y, center.z, 1.0]);
                min.x = min.x.min(center.x);
                min.y = min.y.min(center.y);
                min.z = min.z.min(center.z);
                max.x = max.x.max(center.x);
                max.y = max.y.max(center.y);
                max.z = max.z.max(center.z);
            }
        }

        let size = max - min;
        self.sort.positions = PackedFloat32Array::from(positions).to_byte_array();
        self.sort.splat_count = splat_count;
        self.sort.local_aabb = Aabb::new(min, size).grow(size.length() * 0.05 + 0.01);
    }

    // Build the main-RenderingDevice resources for the GPU counting sort. Runs on
    // the main thread (== render thread under single-threaded rendering).
    fn setup_sort(&mut self) {
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

        let (Some(count_shader), Some(scan_shader), Some(scatter_shader)) = (
            compile_compute_shader(&mut device, SORT_COUNT_GLSL),
            compile_compute_shader(&mut device, SORT_SCAN_GLSL),
            compile_compute_shader(&mut device, SORT_SCATTER_GLSL),
        ) else {
            godot_error!("[gsplat] failed to compile sort compute shaders");
            for rid in [pos_buf, hist_buf, off_buf, sort_tex_rid] {
                if rid.is_valid() {
                    device.free_rid(rid);
                }
            }
            return;
        };

        let count_pipe = device.compute_pipeline_create(count_shader);
        let scan_pipe = device.compute_pipeline_create(scan_shader);
        let scatter_pipe = device.compute_pipeline_create(scatter_shader);

        let pos_u = ssbo_uniform(pos_buf, 0);
        let hist_u = ssbo_uniform(hist_buf, 1);
        let off_u = ssbo_uniform(off_buf, 2);
        let img_u = image_uniform(sort_tex_rid, 3);
        let count_set =
            device.uniform_set_create(&uniform_array(&[&pos_u, &hist_u]), count_shader, 0);
        let scan_set =
            device.uniform_set_create(&uniform_array(&[&hist_u, &off_u]), scan_shader, 0);
        let scatter_set =
            device.uniform_set_create(&uniform_array(&[&pos_u, &off_u, &img_u]), scatter_shader, 0);

        let mut texture = Texture2Drd::new_gd();
        texture.set_texture_rd_rid(sort_tex_rid);

        self.sort.pos_buf = pos_buf;
        self.sort.hist_buf = hist_buf;
        self.sort.off_buf = off_buf;
        self.sort.sort_tex_rid = sort_tex_rid;
        self.sort.count_shader = count_shader;
        self.sort.scan_shader = scan_shader;
        self.sort.scatter_shader = scatter_shader;
        self.sort.count_pipe = count_pipe;
        self.sort.scan_pipe = scan_pipe;
        self.sort.scatter_pipe = scatter_pipe;
        self.sort.count_set = count_set;
        self.sort.scan_set = scan_set;
        self.sort.scatter_set = scatter_set;
        self.sort.tex_width = tex_width;
        self.sort.tex_height = tex_height;
        self.sort.texture = Some(texture);
        self.sort.ready = true;
        // The material is pointed at the sort texture only after the first dispatch
        // (see process); binding the Texture2Drd the frame it is created would make
        // the renderer sample an unregistered texture for one frame.
    }

    // Queue one back-to-front counting sort for the current camera view. The
    // closure runs on the render thread and must not touch the node's storage.
    fn dispatch_sort(&self) {
        let Some(viewport) = self.base().get_viewport() else {
            return;
        };
        let Some(camera) = viewport.get_camera_3d() else {
            return;
        };

        // Sort by camera-space depth of (camera_view * node_model * center).
        let view = camera.get_global_transform().affine_inverse();
        let model = self.base().get_global_transform();
        let combined = view * model;
        let (depth_min, depth_inv_range) = depth_range(combined, self.sort.local_aabb);
        let push_constant = build_sort_push_constant(
            combined,
            depth_min,
            depth_inv_range,
            self.sort.splat_count,
            self.sort.tex_width,
        );

        let count = self.sort.splat_count.max(0) as u32;
        let groups = count.div_ceil(SORT_LOCAL_SIZE);
        let bucket_bytes = (SORT_NUM_BUCKETS as u32) * 4;

        let hist_buf = self.sort.hist_buf;
        let count_pipe = self.sort.count_pipe;
        let scan_pipe = self.sort.scan_pipe;
        let scatter_pipe = self.sort.scatter_pipe;
        let count_set = self.sort.count_set;
        let scan_set = self.sort.scan_set;
        let scatter_set = self.sort.scatter_set;

        let callable = Callable::from_fn("gsplat_sort_dispatch", move |_args: &[&Variant]| {
            let server = RenderingServer::singleton();
            let Some(mut device) = server.get_rendering_device() else {
                return Variant::nil();
            };
            device.buffer_clear(hist_buf, 0, bucket_bytes);
            let list = device.compute_list_begin();
            device.compute_list_bind_compute_pipeline(list, count_pipe);
            device.compute_list_bind_uniform_set(list, count_set, 0);
            device.compute_list_set_push_constant(list, &push_constant, push_constant.len() as u32);
            device.compute_list_dispatch(list, groups, 1, 1);
            device.compute_list_add_barrier(list);
            device.compute_list_bind_compute_pipeline(list, scan_pipe);
            device.compute_list_bind_uniform_set(list, scan_set, 0);
            device.compute_list_set_push_constant(list, &push_constant, push_constant.len() as u32);
            device.compute_list_dispatch(list, 1, 1, 1);
            device.compute_list_add_barrier(list);
            device.compute_list_bind_compute_pipeline(list, scatter_pipe);
            device.compute_list_bind_uniform_set(list, scatter_set, 0);
            device.compute_list_set_push_constant(list, &push_constant, push_constant.len() as u32);
            device.compute_list_dispatch(list, groups, 1, 1);
            device.compute_list_end();
            Variant::nil()
        });
        RenderingServer::singleton().call_on_render_thread(&callable);
    }

    fn teardown_sort(&mut self) {
        let has_resources = self.sort.pos_buf.is_valid()
            || self.sort.count_shader.is_valid()
            || self.sort.sort_tex_rid.is_valid();
        if has_resources {
            let server = RenderingServer::singleton();
            if let Some(mut device) = server.get_rendering_device() {
                for rid in [
                    self.sort.count_set,
                    self.sort.scan_set,
                    self.sort.scatter_set,
                    self.sort.count_pipe,
                    self.sort.scan_pipe,
                    self.sort.scatter_pipe,
                    self.sort.count_shader,
                    self.sort.scan_shader,
                    self.sort.scatter_shader,
                    self.sort.sort_tex_rid,
                    self.sort.pos_buf,
                    self.sort.hist_buf,
                    self.sort.off_buf,
                ] {
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

    fn splat_material(&self) -> Option<Gd<ShaderMaterial>> {
        self.debug_mesh_instance
            .as_ref()
            .and_then(|mesh_instance| mesh_instance.get_material_override())
            .and_then(|material| material.try_cast::<ShaderMaterial>().ok())
    }

    fn set_material_sort(&self, enabled: bool) {
        let Some(mut material) = self.splat_material() else {
            return;
        };
        if enabled {
            if let Some(texture) = &self.sort.texture {
                material.set_shader_parameter("sort_tex", &Variant::from(texture.clone()));
            }
            material.set_shader_parameter("sort_enabled", &Variant::from(1_i32));
        } else {
            material.set_shader_parameter("sort_enabled", &Variant::from(0_i32));
            material.set_shader_parameter("sort_tex", &Variant::nil());
        }
    }
}

// Build the upper triangle [xx, xy, xz, yy, yz, zz] of the 3D covariance
// Sigma = R * S^2 * R^T from a normalized rotation quaternion (xyzw) and the
// per-axis scale (linear standard deviation).
fn covariance_upper_triangle(quat: [f32; 4], scale: [f32; 3]) -> [f32; 6] {
    let [qx, qy, qz, qw] = quat;
    let [sx, sy, sz] = scale;

    // Rotation matrix columns from the quaternion.
    let r = [
        [
            1.0 - 2.0 * (qy * qy + qz * qz),
            2.0 * (qx * qy + qw * qz),
            2.0 * (qx * qz - qw * qy),
        ],
        [
            2.0 * (qx * qy - qw * qz),
            1.0 - 2.0 * (qx * qx + qz * qz),
            2.0 * (qy * qz + qw * qx),
        ],
        [
            2.0 * (qx * qz + qw * qy),
            2.0 * (qy * qz - qw * qx),
            1.0 - 2.0 * (qx * qx + qy * qy),
        ],
    ];

    // M = R * diag(scale); columns of R scaled by the matching axis scale.
    let m = [
        [r[0][0] * sx, r[1][0] * sy, r[2][0] * sz],
        [r[0][1] * sx, r[1][1] * sy, r[2][1] * sz],
        [r[0][2] * sx, r[1][2] * sy, r[2][2] * sz],
    ];

    // Sigma = M * M^T (symmetric).
    let dot = |a: usize, b: usize| m[a][0] * m[b][0] + m[a][1] * m[b][1] + m[a][2] * m[b][2];
    [
        dot(0, 0),
        dot(0, 1),
        dot(0, 2),
        dot(1, 1),
        dot(1, 2),
        dot(2, 2),
    ]
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
