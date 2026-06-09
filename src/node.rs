use godot::classes::image::Format as ImageFormat;
use godot::classes::mesh::{ArrayType, PrimitiveType};
use godot::classes::multi_mesh::TransformFormat;
use godot::classes::rendering_device::{
    DataFormat, ShaderLanguage, ShaderStage, TextureUsageBits, UniformType,
};
use godot::classes::GltfState;
use godot::classes::{
    ArrayMesh, Engine, Image, ImageTexture, MultiMesh, MultiMeshInstance3D, Node3D, RdShaderSource,
    RdTextureFormat, RdTextureView, RdUniform, RenderingDevice, RenderingServer, Shader,
    ShaderMaterial, Texture2D, Texture2Drd, Viewport, XrServer,
};
use godot::prelude::*;

use crate::asset::GaussianSplatAsset;
use crate::backend::{
    GaussianSplatBackendSettings, BACKEND_PROFILE_DESKTOP, BACKEND_PROFILE_MOBILE,
    BACKEND_PROFILE_VR_SAFE, VR_VIEW_BASIS_PER_EYE,
};
use crate::cloud_settings::GaussianSplatCloudSettings;
use crate::import_state::{ImportedSplatMetadata, NODE_STATE_KEY, POINT_STRIDE_FLOATS};

// Texture-driven anisotropic Gaussian splat shader (Step 1 render path). Per-splat
// data (center, packed 3D covariance upper triangle, color) lives in `data_tex`,
// four RGBA-float texels per splat. The geometry is a single quad rendered via
// MultiMesh (one instance per splat); the instance index (INSTANCE_ID) is the
// slot, UV = quad corner in [-2, 2]. Each slot resolves to a splat id via
// `sort_tex` when `sort_enabled`,
// otherwise slot == id (unsorted, matching the Phase 1 look). The resolved splat's
// covariance is projected to screen space with the perspective Jacobian and the
// corner is stretched along the projected ellipse axes, so the on-screen footprint
// is an oriented ellipse; splats whose footprint is entirely outside the viewport
// are frustum-culled (pushed offscreen) to skip their overdraw. Alpha is an
// isotropic Gaussian in the stretched corner space, which equals the anisotropic
// Gaussian on screen.
// NOTE: with `sort_enabled == 0` splats are not depth-sorted, so blending order is
// only approximate; the GPU compute sort (Step 2) drives `sort_tex` for the correct
// back-to-front order.
const SPLAT_TEXTURE_SHADER: &str = r#"
shader_type spatial;
render_mode unshaded, cull_disabled, blend_mix, depth_draw_never;

uniform sampler2D data_tex : filter_nearest;
uniform sampler2D sort_tex : filter_nearest;
uniform sampler2D sort_tex_b : filter_nearest;
uniform int splat_count;
uniform int sort_enabled;

varying vec2 v_corner;
varying vec4 v_color;

void vertex() {
    v_corner = UV;
    int slot = int(INSTANCE_ID);
    int splat_id = slot;
    if (sort_enabled > 0) {
        // VIEW_INDEX selects the per-eye sort order under multiview (VR). It is
        // always 0 on flat displays, so sort_tex_b is then unused.
        if (VIEW_INDEX == 1) {
            int sw = textureSize(sort_tex_b, 0).x;
            splat_id = int(texelFetch(sort_tex_b, ivec2(slot % sw, slot / sw), 0).r + 0.5);
        } else {
            int sw = textureSize(sort_tex, 0).x;
            splat_id = int(texelFetch(sort_tex, ivec2(slot % sw, slot / sw), 0).r + 0.5);
        }
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

        // Frustum cull: if the footprint lies entirely outside the viewport in X
        // or Y, push the splat offscreen so its quad clips (skips the overdraw).
        // The quad corner reaches +/-2, so the footprint radius is 2 * major axis.
        vec2 ndc = center_clip.xy / center_clip.w;
        vec2 margin = vec2(4.0 * length(major_axis)) / vp;
        if (ndc.x - margin.x > 1.0 || ndc.x + margin.x < -1.0
            || ndc.y - margin.y > 1.0 || ndc.y + margin.y < -1.0) {
            POSITION = vec4(0.0, 0.0, 100.0, 1.0);
        } else {
            // Expand the quad corner along the projected ellipse axes, in clip space.
            vec2 screen_offset = v_corner.x * major_axis + v_corner.y * minor_axis;
            vec2 clip_offset = (screen_offset / vp) * 2.0 * center_clip.w;
            POSITION = center_clip + vec4(clip_offset, 0.0, 0.0);
        }
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
// texture bytes plus the per-splat sort-seed positions.
struct SplatRenderData {
    data_bytes: PackedByteArray,
    tex_width: i32,
    tex_height: i32,
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
// 16-bit depth precision: 65536 buckets scanned with a parallel block prefix sum
// (SORT_SCAN_BLOCK threads per block, SORT_NUM_BLOCKS blocks). A single-pass
// counting sort is order-agnostic within a bucket, so a high bucket count yields
// near-exact back-to-front ordering. SORT_SCAN_BLOCK / SORT_NUM_BLOCKS are mirrored
// as `#define`s in the scan shaders below and must stay in sync.
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

// All stages declare the same 84-byte push constant (the scan stages only read
// num_buckets) so one push-constant value is valid for every dispatch in the list.
//
// Pass 2a: per-block exclusive prefix sum of the histogram into offsets, plus the
// per-block total into block_sums. BLOCK must match SORT_SCAN_BLOCK.
const SORT_SCAN_LOCAL_GLSL: &str = r#"#version 450
#define BLOCK 1024u
layout(local_size_x = 1024) in;
layout(set = 0, binding = 1, std430) restrict readonly buffer Hist { uint histogram[]; };
layout(set = 0, binding = 2, std430) restrict writeonly buffer Off { uint offsets[]; };
layout(set = 0, binding = 4, std430) restrict writeonly buffer Blocks { uint block_sums[]; };
layout(push_constant, std430) uniform PC {
    mat4 view;
    float depth_min;
    float depth_inv_range;
    uint num_buckets;
    uint count;
    uint tex_width;
} pc;
shared uint temp[BLOCK];
void main() {
    uint tid = gl_LocalInvocationID.x;
    uint gid = gl_WorkGroupID.x * BLOCK + tid;
    uint original = gid < pc.num_buckets ? histogram[gid] : 0u;
    temp[tid] = original;
    barrier();
    for (uint offset = 1u; offset < BLOCK; offset <<= 1u) {
        uint add = (tid >= offset) ? temp[tid - offset] : 0u;
        barrier();
        temp[tid] += add;
        barrier();
    }
    if (gid < pc.num_buckets) {
        offsets[gid] = temp[tid] - original; // exclusive prefix within the block
    }
    if (tid == BLOCK - 1u) {
        block_sums[gl_WorkGroupID.x] = temp[tid]; // inclusive total of the block
    }
}
"#;

// Pass 2b: exclusive prefix sum of the per-block totals. NUM_BLOCKS must match
// SORT_NUM_BLOCKS.
const SORT_SCAN_BLOCKSUMS_GLSL: &str = r#"#version 450
#define NUM_BLOCKS 64u
layout(local_size_x = 64) in;
layout(set = 0, binding = 4, std430) restrict buffer Blocks { uint block_sums[]; };
layout(push_constant, std430) uniform PC {
    mat4 view;
    float depth_min;
    float depth_inv_range;
    uint num_buckets;
    uint count;
    uint tex_width;
} pc;
shared uint temp[NUM_BLOCKS];
void main() {
    uint tid = gl_LocalInvocationID.x;
    uint original = block_sums[tid];
    temp[tid] = original;
    barrier();
    for (uint offset = 1u; offset < NUM_BLOCKS; offset <<= 1u) {
        uint add = (tid >= offset) ? temp[tid - offset] : 0u;
        barrier();
        temp[tid] += add;
        barrier();
    }
    if (pc.num_buckets > 0u) {
        block_sums[tid] = temp[tid] - original; // exclusive prefix of prior blocks
    }
}
"#;

// Pass 2c: add each block's prior-blocks offset to make a global exclusive scan.
const SORT_SCAN_ADD_GLSL: &str = r#"#version 450
#define BLOCK 1024u
layout(local_size_x = 1024) in;
layout(set = 0, binding = 2, std430) restrict buffer Off { uint offsets[]; };
layout(set = 0, binding = 4, std430) restrict readonly buffer Blocks { uint block_sums[]; };
layout(push_constant, std430) uniform PC {
    mat4 view;
    float depth_min;
    float depth_inv_range;
    uint num_buckets;
    uint count;
    uint tex_width;
} pc;
void main() {
    uint gid = gl_WorkGroupID.x * BLOCK + gl_LocalInvocationID.x;
    if (gid < pc.num_buckets) {
        offsets[gid] += block_sums[gl_WorkGroupID.x];
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
    // Last view (camera_view * node_model) used for a sort; gates re-sorting.
    last_view: Option<Transform3D>,
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
    blocks_buf: Rid,
    sort_tex_rid: Rid,
    shaders: [Rid; SORT_STAGES],
    pipelines: [Rid; SORT_STAGES],
    sets: [Rid; SORT_STAGES],
    texture: Option<Gd<Texture2Drd>>,
    // Second eye's sort order for VR per-eye sorting (unused on flat displays).
    sort_tex_rid_b: Rid,
    scatter_set_b: Rid,
    texture_b: Option<Gd<Texture2Drd>>,
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

// Inspector render-quality preset. Low/Middle/High are fixed presets that map to a
// backend platform target plus a splat budget; Custom leaves the individual fields
// (backend settings, preview limits) under manual control.
#[derive(GodotConvert, Var, Export, Clone, Copy, Eq, PartialEq, Debug, Default)]
#[godot(via = i64)]
#[repr(i64)]
enum RenderProfile {
    #[default]
    Custom = 0,
    Low = 1,
    Middle = 2,
    High = 3,
}

// Per-tier splat budgets (max rendered splats; clamped to the asset point count).
const RENDER_PROFILE_LOW_SPLATS: i32 = 150_000;
const RENDER_PROFILE_MIDDLE_SPLATS: i32 = 500_000;
const RENDER_PROFILE_HIGH_SPLATS: i32 = i32::MAX;

#[derive(GodotClass)]
#[class(tool, init, base=Node3D)]
pub struct GaussianSplatNode3D {
    #[base]
    base: Base<Node3D>,

    asset: Option<Gd<GaussianSplatAsset>>,
    cloud_settings: Option<Gd<GaussianSplatCloudSettings>>,
    backend_settings: Option<Gd<GaussianSplatBackendSettings>>,
    #[var(get, set)]
    #[export(file = "*.gltf,*.glb")]
    source_gltf: PhantomVar<GString>,
    #[var(get, set)]
    #[export]
    render_profile: PhantomVar<RenderProfile>,
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
    debug_mesh_instance: Option<Gd<MultiMeshInstance3D>>,
    sort: SortGpu,
    // Backing storage for the `render_profile` export (PhantomVar holds no state).
    render_profile_value: RenderProfile,
    // True while a preset is being applied, so preset-driven writes to the
    // individual fields don't flip the profile back to Custom.
    applying_profile: bool,
    // Backing storage for the `source_gltf` export (PhantomVar holds no state).
    source_gltf_path: GString,
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
        if !self.sort.ready {
            return;
        }
        let eyes = self.current_sort_views();
        let Some(&(primary, _)) = eyes.first() else {
            return;
        };
        // Re-sort only when the camera/node view changes meaningfully; a static
        // view keeps the last back-to-front order, saving per-frame GPU work.
        let should_sort = match self.sort.last_view {
            Some(last) => self.sort_view_changed(last, primary),
            None => true,
        };
        if should_sort {
            self.dispatch_sort(&eyes);
            self.sort.last_view = Some(primary);
        }
        // Enable sorted sampling one frame after the first dispatch, so the sort
        // texture is registered and written before the material binds it.
        if self.sort.last_view.is_some() && !self.sort.enabled_in_shader {
            if self.sort.dispatched_once {
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
    fn get_render_profile(&self) -> RenderProfile {
        self.render_profile_value
    }

    #[func]
    fn set_render_profile(&mut self, profile: RenderProfile) {
        self.render_profile_value = profile;
        self.apply_render_profile(profile);
    }

    // Apply a fixed Low/Middle/High preset: map to a backend platform target and a
    // splat budget. Custom makes no change (individual fields stay manual).
    fn apply_render_profile(&mut self, profile: RenderProfile) {
        let (target_hint, budget) = match profile {
            RenderProfile::Custom => return,
            RenderProfile::Low => (BACKEND_PROFILE_VR_SAFE, RENDER_PROFILE_LOW_SPLATS),
            RenderProfile::Middle => (BACKEND_PROFILE_MOBILE, RENDER_PROFILE_MIDDLE_SPLATS),
            RenderProfile::High => (BACKEND_PROFILE_DESKTOP, RENDER_PROFILE_HIGH_SPLATS),
        };
        self.ensure_backend_settings();
        if let Some(backend_settings) = &mut self.backend_settings {
            backend_settings
                .bind_mut()
                .set_target_hint(target_hint.into());
        }
        self.backend_state.profile_hint = self.resolve_backend_pipeline();
        self.mark_backend_dirty("render_profile");
        // The budget caps the rendered splat count and rebuilds the render. Guard
        // so this preset-driven write does not flip the profile back to Custom.
        self.applying_profile = true;
        self.set_preview_max_splats(budget);
        self.applying_profile = false;
    }

    #[func]
    fn get_source_gltf(&self) -> GString {
        self.source_gltf_path.clone()
    }

    #[func]
    fn set_source_gltf(&mut self, path: GString) {
        self.source_gltf_path = path.clone();
        // Only (re)load when a path is set. An empty path leaves the current asset
        // intact, so nodes created by the scene importer (which never set this) are
        // not cleared on load.
        if !path.to_string().is_empty() {
            self.load_from_gltf(path);
        }
    }

    // Decode the first splat primitive from the glTF and bind it as the asset.
    fn load_from_gltf(&mut self, path: GString) {
        let path_str = path.to_string();
        match crate::import_state::decode_first_splat_from_gltf(&path_str) {
            Ok((metadata, decoded)) => {
                let mut asset = GaussianSplatAsset::new_gd();
                {
                    let mut bound = asset.bind_mut();
                    bound.apply_import_metadata(metadata);
                    bound.apply_decoded_data(decoded);
                }
                self.bind_asset(Some(asset));
            }
            Err(error) => {
                godot_error!("[gsplat] failed to load splat from '{path_str}': {error}");
            }
        }
    }

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
        // A manual budget edit no longer matches a fixed preset, so drop to Custom.
        if !self.applying_profile {
            self.render_profile_value = RenderProfile::Custom;
        }
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
    }

    #[func]
    pub fn get_backend_settings(&self) -> Option<Gd<GaussianSplatBackendSettings>> {
        self.backend_settings.clone()
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

        let mut mesh_instance = MultiMeshInstance3D::new_alloc();
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

        // Single quad rendered via MultiMesh, one instance per splat. The shader
        // expands each instance's quad from `data_tex`, indexed by INSTANCE_ID.
        let corners = [
            Vector2::new(-2.0, -2.0),
            Vector2::new(2.0, -2.0),
            Vector2::new(2.0, 2.0),
            Vector2::new(-2.0, 2.0),
        ];
        let quad_positions: Vec<Vector3> = corners
            .iter()
            .map(|c| Vector3::new(c.x, c.y, 0.0))
            .collect();
        let quad_uvs: Vec<Vector2> = corners.to_vec();
        let quad_indices: Vec<i32> = vec![0, 1, 2, 0, 2, 3];

        let mut arrays = VarArray::new();
        for _ in 0..ArrayType::MAX.ord() {
            arrays.push(&Variant::nil());
        }
        arrays.set(
            ArrayType::VERTEX.ord() as usize,
            &Variant::from(PackedVector3Array::from(quad_positions)),
        );
        arrays.set(
            ArrayType::TEX_UV.ord() as usize,
            &Variant::from(PackedVector2Array::from(quad_uvs)),
        );
        arrays.set(
            ArrayType::INDEX.ord() as usize,
            &Variant::from(PackedInt32Array::from(quad_indices)),
        );

        let mut mesh = ArrayMesh::new_gd();
        mesh.add_surface_from_arrays_ex(PrimitiveType::TRIANGLES, &arrays)
            .done();

        // One identity transform per splat: the shader positions each splat from
        // the data texture, not from the instance transform, so transforms stay
        // identity and the instance index is the slot.
        let identity = [
            1.0_f32, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0,
        ];
        let mut buffer = Vec::with_capacity(render.splat_count.max(0) as usize * 12);
        for _ in 0..render.splat_count {
            buffer.extend_from_slice(&identity);
        }

        let mut multimesh = MultiMesh::new_gd();
        multimesh.set_transform_format(TransformFormat::TRANSFORM_3D);
        multimesh.set_mesh(&mesh.upcast::<godot::classes::Mesh>());
        multimesh.set_instance_count(render.splat_count);
        multimesh.set_buffer(&PackedFloat32Array::from(buffer));
        // The unit quad's own bounds do not cover the cloud and identity instance
        // transforms keep them at the origin, so give the MultiMesh an explicit
        // AABB. This drives `get_aabb()` for the editor import-preview auto-framing
        // (and the demo orbit camera); runtime culling uses the instance override.
        multimesh.set_custom_aabb(render.aabb);

        let mut shader = Shader::new_gd();
        shader.set_code(SPLAT_TEXTURE_SHADER);

        let mut material = ShaderMaterial::new_gd();
        material.set_shader(&shader);
        material.set_shader_parameter("data_tex", &Variant::from(data_texture));
        material.set_shader_parameter("splat_count", &Variant::from(render.splat_count));
        // Step 1 renders unsorted (slot == id); the compute sort (Step 2) flips this on.
        material.set_shader_parameter("sort_enabled", &Variant::from(0_i32));

        let material_resource = material.upcast::<godot::classes::Material>();
        mesh_instance.set_multimesh(&multimesh);
        mesh_instance.set_material_override(&material_resource);
        // The unit quad's own bounds do not cover the cloud, so set an explicit
        // AABB for frustum culling and the editor import-preview auto-framing.
        mesh_instance.set_custom_aabb(render.aabb);
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
        }

        let size = max - min;
        // Grow the bounds so splats extending past their centers stay visible.
        let aabb = Aabb::new(min, size).grow(size.length() * 0.05 + 0.01);

        Some(SplatRenderData {
            data_bytes: PackedFloat32Array::from(data).to_byte_array(),
            tex_width: tex_width as i32,
            tex_height: tex_height as i32,
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
            if let Ok(mesh_instance) = child.try_cast::<MultiMeshInstance3D>() {
                if mesh_instance.get_multimesh().is_some() {
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
    fn current_sort_views(&self) -> Vec<(Transform3D, usize)> {
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

        // Flat / head-center: a single view from the active camera.
        let Some(camera) = viewport.get_camera_3d() else {
            return Vec::new();
        };
        let view = camera.get_global_transform().affine_inverse();
        vec![(view * model, 0)]
    }

    fn vr_per_eye_enabled(&self) -> bool {
        self.backend_settings
            .as_ref()
            .map(|settings| settings.bind().get_vr_view_basis() == VR_VIEW_BASIS_PER_EYE)
            .unwrap_or(false)
    }

    // Acquire per-eye world views from the XR interface. Assumes the standard
    // XROrigin3D > XRCamera3D hierarchy for the reference transform; the exact
    // per-eye math must be validated on real XR hardware.
    fn xr_eye_views(
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
    fn sort_view_changed(&self, last: Transform3D, current: Transform3D) -> bool {
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
    fn dispatch_sort(&self, eyes: &[(Transform3D, usize)]) {
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

    fn teardown_sort(&mut self) {
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
