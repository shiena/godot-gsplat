//! Embedded shader sources for the splat render path: the texture-driven
//! spatial shader (Step 1) and the GPU counting-sort compute stages (Step 2).

// Texture-driven anisotropic Gaussian splat shader (Step 1 render path). Per-splat
// data (center, per-axis scale, rotation quaternion, color) lives in `data_tex`,
// four RGBA-float texels per splat. The geometry is a low-level empty-vertex
// triangle surface with 6 virtual vertices per splat; VERTEX_ID / 6 is the slot
// and VERTEX_ID % 6 selects the quad corner in [-2, 2]. Each slot resolves to a
// splat id via `sort_tex` when `sort_enabled`,
// otherwise slot == id (unsorted, matching the Phase 1 look). The resolved splat's
// ellipsoid is projected to a screen-space ellipse *without* the affine/Jacobian
// approximation: the center and the three rotated/scaled semi-axes are projected to
// clip space, a projected quadric/conic is formed, and the ellipse is recovered by
// eigen-decomposition (exact, stable for the near-camera / wide-FOV views that
// matter for VR). The corner is stretched along the ellipse axes, so the on-screen
// footprint is an oriented ellipse; splats whose footprint is entirely outside the
// viewport are frustum-culled (pushed offscreen) to skip their overdraw. Alpha is
// an isotropic Gaussian in the stretched corner space, which equals the anisotropic
// Gaussian on screen, with energy-preserving antialiasing on sub-pixel footprints.
// Per-corner depth is the Gaussian peak along the view ray (ray vs ellipsoid),
// written to POSITION.z so splats intersect opaque geometry correctly while keeping
// early-z. The exact-projection / AA / ray-depth math is adapted from
// VRChatGaussianSplatting (MIT, Mykhailo Moroz): Shaders/GSMath.cginc + GS.cginc.
// NOTE: with `sort_enabled == 0` splats are not depth-sorted, so blending order is
// only approximate; the GPU compute sort (Step 2) drives `sort_tex` for the correct
// back-to-front order.
pub(super) const SPLAT_TEXTURE_SHADER: &str = r#"
shader_type spatial;
render_mode unshaded, cull_disabled, blend_mix, depth_draw_never;

uniform sampler2D data_tex : filter_nearest;
uniform sampler2D sort_tex : filter_nearest;
uniform sampler2D sort_tex_b : filter_nearest;
uniform int splat_count;
uniform int sort_enabled;
// 1 only when a per-eye sort actually wrote sort_tex_b. Head-center sorting
// must NOT route the second eye through sort_tex_b at all: on device the
// second-sampler binding intermittently resolved invalid and texelFetch then
// returned 0 for every slot — the whole right eye drew splat #0 and flashed
// washed-out white (Quest standalone and PC-VR alike).
uniform int sort_per_eye;
uniform int sh_degree;
// 0 = legacy RGBAF texels, 1 = packed RGBA8 records from .gsplatpack v2.
uniform int data_encoding;
uniform int packed_record_bytes;
uniform vec3 position_min;
uniform vec3 position_max;
uniform vec3 scale_min;
uniform vec3 scale_max;
uniform float sh_min;
uniform float sh_max;
// 0 = ray-vs-ellipsoid per-corner depth, 1 = splat-center depth.
uniform int splat_depth_mode;

varying vec2 v_corner;
varying vec4 v_color;

// Fetch float `idx` from the flat SH region starting at texel `sh_base`.
float sh_tex_float(int sh_base, int idx, int w) {
    int t = sh_base + idx / 4;
    vec4 v = texelFetch(data_tex, ivec2(t % w, t / w), 0);
    int lane = idx - (idx / 4) * 4;
    return lane == 0 ? v.r : (lane == 1 ? v.g : (lane == 2 ? v.b : v.a));
}
vec3 sh_coeff(int sh_base, int c, int w) {
    int f = c * 3;
    return vec3(sh_tex_float(sh_base, f, w), sh_tex_float(sh_base, f + 1, w), sh_tex_float(sh_base, f + 2, w));
}
float packed_byte(int byte_index, int w) {
    int texel = byte_index / 4;
    vec4 v = texelFetch(data_tex, ivec2(texel % w, texel / w), 0);
    int lane = byte_index - texel * 4;
    float b = lane == 0 ? v.r : (lane == 1 ? v.g : (lane == 2 ? v.b : v.a));
    return floor(b * 255.0 + 0.5);
}
float packed_u16(int byte_index, int w) {
    return packed_byte(byte_index, w) + packed_byte(byte_index + 1, w) * 256.0;
}
float packed_i16_norm(int byte_index, int w) {
    float u = packed_u16(byte_index, w);
    float s = u >= 32768.0 ? u - 65536.0 : u;
    return clamp(s / 32767.0, -1.0, 1.0);
}
float packed_unorm(float q, float mn, float mx, float denom) {
    return mix(mn, mx, q / denom);
}
float packed_sh_float(int record_base, int idx, int w) {
    return packed_unorm(packed_byte(record_base + 24 + idx, w), sh_min, sh_max, 255.0);
}
vec3 packed_sh_coeff(int record_base, int c, int w) {
    int f = c * 3;
    return vec3(
        packed_sh_float(record_base, f, w),
        packed_sh_float(record_base, f + 1, w),
        packed_sh_float(record_base, f + 2, w)
    );
}
// View-dependent color: the base color (COLOR_0 == SH degree 0) plus the degree 1-3
// contributions for view direction `d`. Standard real-SH basis (INRIA 3DGS order).
vec3 eval_sh(vec3 base_color, int sh_base, int w, vec3 d, int degree) {
    float x = d.x, y = d.y, z = d.z;
    vec3 c = base_color;
    c += 0.48860251 * (-y * sh_coeff(sh_base, 0, w) + z * sh_coeff(sh_base, 1, w) - x * sh_coeff(sh_base, 2, w));
    if (degree >= 2) {
        float xx = x * x, yy = y * y, zz = z * z;
        float xy = x * y, yz = y * z, xz = x * z;
        c += 1.09254843 * xy * sh_coeff(sh_base, 3, w);
        c += -1.09254843 * yz * sh_coeff(sh_base, 4, w);
        c += 0.31539157 * (2.0 * zz - xx - yy) * sh_coeff(sh_base, 5, w);
        c += -1.09254843 * xz * sh_coeff(sh_base, 6, w);
        c += 0.54627422 * (xx - yy) * sh_coeff(sh_base, 7, w);
        if (degree >= 3) {
            c += -0.59004360 * y * (3.0 * xx - yy) * sh_coeff(sh_base, 8, w);
            c += 2.89061142 * xy * z * sh_coeff(sh_base, 9, w);
            c += -0.45704580 * y * (4.0 * zz - xx - yy) * sh_coeff(sh_base, 10, w);
            c += 0.37317633 * z * (2.0 * zz - 3.0 * xx - 3.0 * yy) * sh_coeff(sh_base, 11, w);
            c += -0.45704580 * x * (4.0 * zz - xx - yy) * sh_coeff(sh_base, 12, w);
            c += 1.44530572 * z * (xx - yy) * sh_coeff(sh_base, 13, w);
            c += -0.59004360 * x * (xx - 3.0 * yy) * sh_coeff(sh_base, 14, w);
        }
    }
    return max(c, vec3(0.0));
}
vec3 eval_sh_packed(vec3 base_color, int record_base, int w, vec3 d, int degree) {
    float x = d.x, y = d.y, z = d.z;
    vec3 c = base_color;
    c += 0.48860251 * (-y * packed_sh_coeff(record_base, 0, w) + z * packed_sh_coeff(record_base, 1, w) - x * packed_sh_coeff(record_base, 2, w));
    if (degree >= 2) {
        float xx = x * x, yy = y * y, zz = z * z;
        float xy = x * y, yz = y * z, xz = x * z;
        c += 1.09254843 * xy * packed_sh_coeff(record_base, 3, w);
        c += -1.09254843 * yz * packed_sh_coeff(record_base, 4, w);
        c += 0.31539157 * (2.0 * zz - xx - yy) * packed_sh_coeff(record_base, 5, w);
        c += -1.09254843 * xz * packed_sh_coeff(record_base, 6, w);
        c += 0.54627422 * (xx - yy) * packed_sh_coeff(record_base, 7, w);
        if (degree >= 3) {
            c += -0.59004360 * y * (3.0 * xx - yy) * packed_sh_coeff(record_base, 8, w);
            c += 2.89061142 * xy * z * packed_sh_coeff(record_base, 9, w);
            c += -0.45704580 * y * (4.0 * zz - xx - yy) * packed_sh_coeff(record_base, 10, w);
            c += 0.37317633 * z * (2.0 * zz - 3.0 * xx - 3.0 * yy) * packed_sh_coeff(record_base, 11, w);
            c += -0.45704580 * x * (4.0 * zz - xx - yy) * packed_sh_coeff(record_base, 12, w);
            c += 1.44530572 * z * (xx - yy) * packed_sh_coeff(record_base, 13, w);
            c += -0.59004360 * x * (xx - 3.0 * yy) * packed_sh_coeff(record_base, 14, w);
        }
    }
    return max(c, vec3(0.0));
}

const float DIV_EPS = 1e-6;
const float SPLAT_MIN_STD_PX = 0.5;     // AA: minimum on-screen Gaussian std (px)
const float SPLAT_MAX_STD_PX = 1024.0;  // overdraw guard: clamp huge footprints (px)

// Rotate vector v by quaternion q (xyzw).
vec3 q_rotate(vec3 v, vec4 q) {
    vec3 t = 2.0 * cross(q.xyz, v);
    return v + q.w * t + cross(q.xyz, t);
}

void vertex() {
    int vertex_id = int(VERTEX_ID);
    int slot = vertex_id / 6;
    int quad_vertex = vertex_id - slot * 6;
    if (quad_vertex == 0 || quad_vertex == 3) {
        v_corner = vec2(-2.0, -2.0);
    } else if (quad_vertex == 1) {
        v_corner = vec2(2.0, -2.0);
    } else if (quad_vertex == 2 || quad_vertex == 4) {
        v_corner = vec2(2.0, 2.0);
    } else {
        v_corner = vec2(-2.0, 2.0);
    }
    int splat_id = slot;
    if (sort_enabled > 0) {
        // VIEW_INDEX selects the per-eye sort order under multiview (VR), but
        // only when a per-eye dispatch actually wrote sort_tex_b; head-center
        // shares the one order in sort_tex for both eyes.
        if (sort_per_eye > 0 && VIEW_INDEX == 1) {
            int sw = textureSize(sort_tex_b, 0).x;
            splat_id = int(texelFetch(sort_tex_b, ivec2(slot % sw, slot / sw), 0).r + 0.5);
        } else {
            int sw = textureSize(sort_tex, 0).x;
            splat_id = int(texelFetch(sort_tex, ivec2(slot % sw, slot / sw), 0).r + 0.5);
        }
    }
    splat_id = clamp(splat_id, 0, splat_count - 1);

    // Fetch the splat's data block from data_tex.
    int w = textureSize(data_tex, 0).x;
    int sh_tex = sh_degree <= 0 ? 0 : (sh_degree == 1 ? 3 : (sh_degree == 2 ? 6 : 12));
    int base = splat_id * (4 + sh_tex);
    vec3 center;
    vec3 scale;
    vec4 quat;
    if (data_encoding == 1) {
        int record_base = splat_id * packed_record_bytes;
        center = vec3(
            packed_unorm(packed_u16(record_base + 0, w), position_min.x, position_max.x, 65535.0),
            packed_unorm(packed_u16(record_base + 2, w), position_min.y, position_max.y, 65535.0),
            packed_unorm(packed_u16(record_base + 4, w), position_min.z, position_max.z, 65535.0)
        );
        scale = vec3(
            packed_unorm(packed_u16(record_base + 6, w), scale_min.x, scale_max.x, 65535.0),
            packed_unorm(packed_u16(record_base + 8, w), scale_min.y, scale_max.y, 65535.0),
            packed_unorm(packed_u16(record_base + 10, w), scale_min.z, scale_max.z, 65535.0)
        );
        quat = normalize(vec4(
            packed_i16_norm(record_base + 12, w),
            packed_i16_norm(record_base + 14, w),
            packed_i16_norm(record_base + 16, w),
            packed_i16_norm(record_base + 18, w)
        ));
        v_color = vec4(
            packed_byte(record_base + 20, w),
            packed_byte(record_base + 21, w),
            packed_byte(record_base + 22, w),
            packed_byte(record_base + 23, w)
        ) / 255.0;
        if (sh_degree >= 1) {
            vec3 center_world = (MODEL_MATRIX * vec4(center, 1.0)).xyz;
            vec3 cam = INV_VIEW_MATRIX[3].xyz;
            v_color.rgb = eval_sh_packed(v_color.rgb, record_base, w, normalize(center_world - cam), sh_degree);
        }
    } else {
        vec4 t0 = texelFetch(data_tex, ivec2(base % w, base / w), 0);
        vec4 t1 = texelFetch(data_tex, ivec2((base + 1) % w, (base + 1) / w), 0);
        vec4 t2 = texelFetch(data_tex, ivec2((base + 2) % w, (base + 2) / w), 0);
        vec4 t3 = texelFetch(data_tex, ivec2((base + 3) % w, (base + 3) / w), 0);
        center = t0.xyz;
        scale = vec3(t0.w, t1.x, t1.y);
        quat = vec4(t1.z, t1.w, t2.x, t2.y);
        v_color = t3;
        if (sh_degree >= 1) {
            vec3 center_world = (MODEL_MATRIX * vec4(center, 1.0)).xyz;
            vec3 cam = INV_VIEW_MATRIX[3].xyz;
            v_color.rgb = eval_sh(v_color.rgb, base + 4, w, normalize(center_world - cam), sh_degree);
        }
    }
    // View-dependent color from higher-degree SH (COLOR_0 is the degree-0 base).
    // --- Exact ellipsoid -> screen-ellipse projection (no affine/Jacobian
    // approximation). Project the center and the three rotated/scaled semi-axes to
    // clip space, build the projected quadric/conic, and recover the ellipse by
    // eigen-decomposition. Adapted from VRChatGaussianSplatting GSMath.cginc.
    mat4 mvp = PROJECTION_MATRIX * VIEW_MATRIX * MODEL_MATRIX;
    vec4 center_clip = mvp * vec4(center, 1.0);
        vec3 ax0 = q_rotate(vec3(scale.x, 0.0, 0.0), quat);
        vec3 ax1 = q_rotate(vec3(0.0, scale.y, 0.0), quat);
        vec3 ax2 = q_rotate(vec3(0.0, 0.0, scale.z), quat);

        bool valid = center_clip.w > DIV_EPS;
        vec2 ell_center = vec2(0.0);
        vec2 ell_axis = vec2(1.0, 0.0);
        vec2 ell_size = vec2(0.0);
        if (valid) {
            vec2 cndc = center_clip.xy / center_clip.w;
            vec4 ac0 = mvp * vec4(ax0, 0.0);
            vec4 ac1 = mvp * vec4(ax1, 0.0);
            vec4 ac2 = mvp * vec4(ax2, 0.0);
            vec3 HX = vec3(ac0.x - cndc.x * ac0.w, ac1.x - cndc.x * ac1.w, ac2.x - cndc.x * ac2.w);
            vec3 HY = vec3(ac0.y - cndc.y * ac0.w, ac1.y - cndc.y * ac1.w, ac2.y - cndc.y * ac2.w);
            vec3 HW = vec3(ac0.w, ac1.w, ac2.w);
            float c00 = dot(HX, HX);
            float c01 = dot(HX, HY);
            float c11 = dot(HY, HY);
            float c02 = dot(HX, HW);
            float c12 = dot(HY, HW);
            float c22 = dot(HW, HW) - center_clip.w * center_clip.w;
            if (c22 > 0.0) { c00 = -c00; c01 = -c01; c11 = -c11; c02 = -c02; c12 = -c12; c22 = -c22; }
            if (abs(c22) <= DIV_EPS) {
                valid = false;
            } else {
                float invC22 = 1.0 / c22;
                float invS = -invC22;
                vec2 localCenter = vec2(c02, c12) * invC22;
                float covXX = (c00 - c02 * c02 * invC22) * invS;
                float covXY = (c01 - c02 * c12 * invC22) * invS;
                float covYY = (c11 - c12 * c12 * invC22) * invS;
                float trace = covXX + covYY;
                float diff = covXX - covYY;
                float discr = sqrt(max(diff * diff + 4.0 * covXY * covXY, 0.0));
                float lMaj = 0.5 * (trace + discr);
                float lMin = 0.5 * (trace - discr);
                if (lMaj <= 0.0 || lMin <= 0.0) {
                    valid = false;
                } else {
                    ell_axis = (abs(covXY) + abs(lMaj - covXX) > DIV_EPS)
                        ? normalize(vec2(covXY, lMaj - covXX)) : vec2(1.0, 0.0);
                    ell_center = cndc + localCenter;
                    ell_size = sqrt(vec2(lMaj, lMin));
                }
            }
        }

        // Reject non-finite / degenerate results (NaN fails the > comparisons).
        if (!(ell_size.x > 0.0) || !(ell_size.y > 0.0)
            || !(abs(ell_center.x) < 1.0e4) || !(abs(ell_center.y) < 1.0e4)) {
            valid = false;
        }

        // --- Energy-preserving antialiasing: clamp the footprint to a minimum screen
        // size and dim alpha by the area ratio so sub-pixel splats stay visible without
        // over-brightening. Replaces the fixed covariance dilation. (GS.cginc.)
        vec2 half_px = VIEWPORT_SIZE * 0.5;            // 1 NDC unit == half_px pixels
        vec2 size_px = ell_size * half_px;
        vec2 size_aa = max(size_px, vec2(SPLAT_MIN_STD_PX));
        float area_scale = (size_px.x * size_px.y) / max(size_aa.x * size_aa.y, DIV_EPS);
        v_color.a *= area_scale;
        size_aa = min(size_aa, vec2(SPLAT_MAX_STD_PX));
        ell_size = size_aa / half_px;

        // Frustum cull: if the footprint is entirely off-viewport, collapse the quad.
        float reach = 2.0 * 1.41421356 * max(ell_size.x, ell_size.y);  // |UV|<=2, sqrt(2) sigma map
        if (!valid
            || ell_center.x - reach > 1.0 || ell_center.x + reach < -1.0
            || ell_center.y - reach > 1.0 || ell_center.y + reach < -1.0) {
            POSITION = vec4(2.0, 2.0, 2.0, 1.0);   // degenerate / clipped
        } else {
            // Stretch the quad corner along the ellipse axes. The sqrt(2) maps UV (in
            // [-2,2]) to standard-deviation units so the fragment's exp(-|UV|^2) equals
            // the true anisotropic Gaussian exp(-0.5 * mahalanobis^2).
            mat2 rot = mat2(vec2(ell_axis.x, ell_axis.y), vec2(-ell_axis.y, ell_axis.x));
            vec2 ndc = ell_center + rot * (v_corner * ell_size * 1.41421356);

            // Per-corner depth: depth of the Gaussian peak along the view ray through
            // this corner (ray vs ellipsoid, metric closest approach). Writes
            // POSITION.z only (keeps early-z; no fragment DEPTH). Falls back to the
            // splat-center depth. (GS.cginc GSTryGetRaySplatDepth.)
            float depth = center_clip.z / center_clip.w;
            vec3 cam_w = INV_VIEW_MATRIX[3].xyz;
            vec3 center_w = (MODEL_MATRIX * vec4(center, 1.0)).xyz;
            vec3 a0w = (MODEL_MATRIX * vec4(ax0, 0.0)).xyz;
            vec3 a1w = (MODEL_MATRIX * vec4(ax1, 0.0)).xyz;
            vec3 a2w = (MODEL_MATRIX * vec4(ax2, 0.0)).xyz;
            // Inverse of A = [a0w a1w a2w] via cofactor rows (maps world -> unit space).
            vec3 cof0 = cross(a1w, a2w);
            vec3 cof1 = cross(a2w, a0w);
            vec3 cof2 = cross(a0w, a1w);
            float det = dot(a0w, cof0);
            if (splat_depth_mode == 0 && abs(det) > DIV_EPS) {
                vec4 vdir = INV_PROJECTION_MATRIX * vec4(ndc, 1.0, 1.0);
                vec3 dir_w = normalize(mat3(INV_VIEW_MATRIX) * (vdir.xyz / vdir.w));
                float inv_det = 1.0 / det;
                vec3 oc = cam_w - center_w;
                vec3 m = vec3(dot(cof0, oc), dot(cof1, oc), dot(cof2, oc)) * inv_det;
                vec3 d = vec3(dot(cof0, dir_w), dot(cof1, dir_w), dot(cof2, dir_w)) * inv_det;
                float dd = dot(d, d);
                if (dd > DIV_EPS) {
                    float t = -dot(m, d) / dd;
                    if (t > DIV_EPS) {
                        vec4 hit_clip = PROJECTION_MATRIX * VIEW_MATRIX * vec4(cam_w + dir_w * t, 1.0);
                        if (hit_clip.w > DIV_EPS) {
                            depth = clamp(hit_clip.z / hit_clip.w, 0.0, 1.0);
                        }
                    }
                }
            }
            POSITION = vec4(ndc, depth, 1.0);
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

// Pass 1: count splats per distance bucket (bucket 0 = farthest).
// The key is the EUCLIDEAN camera distance (the glTF's sortingMethod =
// cameraDistance), not the view-axis depth: distances are invariant under
// camera rotation, so a head/orbit rotation re-sort reproduces the identical
// order instead of reshuffling every bucket (which flashed white across
// translucent room-scale captures on every re-sort).
pub(super) const SORT_COUNT_GLSL: &str = r#"#version 450
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
    float dist = length((pc.view * vec4(positions[i].xyz, 1.0)).xyz);
    float t = clamp((dist - pc.depth_min) * pc.depth_inv_range, 0.0, 1.0);
    uint bucket = uint((1.0 - t) * float(pc.num_buckets - 1u) + 0.5);
    atomicAdd(histogram[bucket], 1u);
}
"#;

// All stages declare the same 84-byte push constant (the scan stages only read
// num_buckets) so one push-constant value is valid for every dispatch in the list.
//
// Pass 2a: per-block exclusive prefix sum of the histogram into offsets, plus the
// per-block total into block_sums. BLOCK must match SORT_SCAN_BLOCK.
pub(super) const SORT_SCAN_LOCAL_GLSL: &str = r#"#version 450
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
pub(super) const SORT_SCAN_BLOCKSUMS_GLSL: &str = r#"#version 450
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
pub(super) const SORT_SCAN_ADD_GLSL: &str = r#"#version 450
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
pub(super) const SORT_SCATTER_GLSL: &str = r#"#version 450
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
    // Same camera-distance key as the count pass (rotation-invariant order).
    float dist = length((pc.view * vec4(positions[i].xyz, 1.0)).xyz);
    float t = clamp((dist - pc.depth_min) * pc.depth_inv_range, 0.0, 1.0);
    uint bucket = uint((1.0 - t) * float(pc.num_buckets - 1u) + 0.5);
    uint slot = atomicAdd(offsets[bucket], 1u);
    ivec2 c = ivec2(int(slot % pc.tex_width), int(slot / pc.tex_width));
    imageStore(sort_img, c, vec4(float(i), 0.0, 0.0, 0.0));
}
"#;
