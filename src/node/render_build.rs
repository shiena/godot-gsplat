//! Render-set construction for the texture-driven splat path: pack splats into
//! the per-splat data texture, build the MultiMesh quad + material, and stash
//! the GPU-sort inputs.

use godot::classes::image::Format as ImageFormat;
use godot::classes::mesh::{ArrayType, PrimitiveType};
use godot::classes::multi_mesh::TransformFormat;
use godot::classes::{
    ArrayMesh, Image, ImageTexture, MultiMesh, MultiMeshInstance3D, Shader, ShaderMaterial,
};
use godot::prelude::*;

use crate::asset::GaussianSplatAsset;
use crate::cloud_settings::GaussianSplatCloudSettings;
use crate::import_state::POINT_STRIDE_FLOATS;

use super::shaders::SPLAT_TEXTURE_SHADER;
use super::GaussianSplatNode3D;

// Width (in RGBA-float texels) of the per-splat data texture. Each splat occupies
// four consecutive texels, so a row holds SPLAT_DATA_TEX_WIDTH / 4 splats.
const SPLAT_DATA_TEX_WIDTH: i32 = 4096;

// CPU-built render data for the texture-driven splat path: the per-splat data
// texture bytes plus the per-splat sort-seed positions.
pub(super) struct SplatRenderData {
    pub(super) data_bytes: PackedByteArray,
    pub(super) tex_width: i32,
    pub(super) tex_height: i32,
    // Local-space splat centers as vec4(x, y, z, 1) in slot order, used to seed
    // the GPU sort's positions storage buffer (Step 2).
    pub(super) positions_ssbo: PackedByteArray,
    pub(super) splat_count: i32,
    pub(super) aabb: Aabb,
    pub(super) sh_degree: i32,
}

// Plain (Send) render-set data produced off the main thread by `pack_raw`: the data
// texture floats + sort positions + grown bounds. The main thread turns it into a
// `SplatRenderData` (Godot types) via `raw_to_render`.
pub(super) struct RawRenderData {
    pub(super) data: Vec<f32>,
    pub(super) tex_width: i32,
    pub(super) tex_height: i32,
    pub(super) positions: Vec<f32>,
    pub(super) splat_count: i32,
    pub(super) aabb_pos: [f32; 3],
    pub(super) aabb_size: [f32; 3],
    // SH degree packed into the data texture (drives the shader's texels-per-splat).
    pub(super) sh_degree: i32,
}

impl GaussianSplatNode3D {
    pub(super) fn ensure_splat_multimesh(&mut self) {
        if self.splat_multimesh.is_some() {
            return;
        }

        let mut mesh_instance = MultiMeshInstance3D::new_alloc();
        mesh_instance.set_name("SplatMultiMesh");
        self.base_mut()
            .add_child(&mesh_instance.clone().upcast::<Node>());
        self.splat_multimesh = Some(mesh_instance);
    }

    pub(super) fn clear_splat_multimesh(&mut self) {
        if let Some(mesh_instance) = &mut self.splat_multimesh {
            mesh_instance.set_visible(false);
        }
    }

    pub(super) fn rebuild_splat_multimesh(&mut self) {
        let Some(asset) = self.asset.clone() else {
            self.clear_splat_multimesh();
            return;
        };
        let cloud_settings = self.cloud_settings.clone();

        // No bound cloud settings means class defaults: rendering enabled.
        if !cloud_settings
            .as_ref()
            .map(|settings| settings.bind().is_render_enabled())
            .unwrap_or(true)
        {
            self.clear_splat_multimesh();
            return;
        }

        let Some(render) = self.build_splat_render_data(&asset, cloud_settings.as_ref()) else {
            self.clear_splat_multimesh();
            return;
        };

        self.apply_render_data(render);
    }

    // Build the data texture + MultiMesh + material from a finished render set and
    // re-arm the GPU sort. Shared by the synchronous rebuild above and the async
    // chunk rebuild (Phase C2b), which both produce a `SplatRenderData`.
    pub(super) fn apply_render_data(&mut self, render: SplatRenderData) {
        let cloud_settings = self.cloud_settings.clone();

        // Per-splat data texture (four RGBA-float texels per splat).
        let Some(image) = Image::create_from_data(
            render.tex_width,
            render.tex_height,
            false,
            ImageFormat::RGBAF,
            &render.data_bytes,
        ) else {
            self.clear_splat_multimesh();
            return;
        };
        let Some(data_texture) = ImageTexture::create_from_image(&image) else {
            self.clear_splat_multimesh();
            return;
        };

        self.ensure_splat_multimesh();
        let Some(mesh_instance) = &mut self.splat_multimesh else {
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
        material.set_shader_parameter("sh_degree", &Variant::from(render.sh_degree));
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
                .map(|settings| settings.bind().is_splat_visible())
                .unwrap_or(true),
        );

        // Stash GPU sort inputs (Step 2). A rebuild invalidates any prior sort
        // (the material above already starts with sort_enabled = 0).
        self.teardown_sort();
        self.backend.sort.positions = render.positions_ssbo;
        self.backend.sort.splat_count = render.splat_count;
        self.backend.sort.local_aabb = render.aabb;
        self.backend.sort.attempted = false;
    }

    pub(super) fn build_splat_render_data(
        &self,
        asset: &Gd<GaussianSplatAsset>,
        cloud_settings: Option<&Gd<GaussianSplatCloudSettings>>,
    ) -> Option<SplatRenderData> {
        let scale_multiplier = cloud_settings
            .map(|settings| settings.bind().get_gaussian_scale_multiplier())
            .unwrap_or(1.0)
            .max(0.01);

        // Chunked path: gather the currently selected chunks from the shared payload
        // (selection bounds the count). Legacy path (no chunk table): clone the asset
        // payload and uniformly decimate it to the budget.
        let (slice, stride, sh_degree): (Vec<f32>, usize, i32) =
            if let Some(rt) = &self.backend.chunks {
                // No bound cloud settings means class defaults: full SH degree.
                let cap = cloud_settings
                    .map(|settings| settings.bind().get_sh_degree())
                    .unwrap_or(3);
                (
                    crate::chunking::gather_active(
                        rt.payload.as_slice(),
                        rt.table.as_ref(),
                        &rt.active,
                    ),
                    rt.table.stride,
                    cap.clamp(0, 3).min(rt.sh_degree_available),
                )
            } else {
                let values = {
                    let asset_ref = asset.bind();
                    asset_ref.payload_float_values()?
                };
                let source_point_count = values.len() / POINT_STRIDE_FLOATS;
                let max_splats = cloud_settings
                    .map(|settings| settings.bind().get_max_preview_splats().max(0) as usize)
                    .unwrap_or(usize::MAX);
                let point_count = source_point_count.min(max_splats);
                if point_count == 0 {
                    return None;
                }
                let sample_stride = source_point_count.div_ceil(point_count);
                let mut slice = Vec::with_capacity(point_count * POINT_STRIDE_FLOATS);
                for slot in 0..point_count {
                    let pi = (slot * sample_stride).min(source_point_count - 1);
                    slice.extend_from_slice(
                        &values[pi * POINT_STRIDE_FLOATS..(pi + 1) * POINT_STRIDE_FLOATS],
                    );
                }
                (slice, POINT_STRIDE_FLOATS, 0)
            };

        pack_raw(&slice, scale_multiplier, stride, sh_degree).map(raw_to_render)
    }
}

// Higher-SH float / texel counts for degrees 0-3. Degree 0 lives in COLOR_0, so these
// count only the appended degree 1-3 coefficients packed after the four core texels.
fn sh_floats(degree: i32) -> usize {
    match degree.clamp(0, 3) {
        0 => 0,
        1 => 9,
        2 => 24,
        _ => 45,
    }
}
fn sh_texels(degree: i32) -> usize {
    sh_floats(degree).div_ceil(4)
}

// Pack an already-gathered splat slice (`stride` floats per splat) into plain (Send)
// render data off the main thread: project each splat's covariance and lay out the
// data-texture floats (4 core texels + SH texels for `sh_degree`) + sort positions +
// grown bounds. Shared by the synchronous build path and the async rebuild (C2b).
pub(super) fn pack_raw(
    slice: &[f32],
    scale_multiplier: f32,
    stride: usize,
    sh_degree: i32,
) -> Option<RawRenderData> {
    let stride = stride.max(POINT_STRIDE_FLOATS);
    let point_count = slice.len() / stride;
    let sh_degree = sh_degree.clamp(0, 3);
    let texels_per_splat = 4 + sh_texels(sh_degree);
    if point_count == 0 || point_count > (i32::MAX as usize / (4 * texels_per_splat)) {
        return None;
    }
    let tex_width = SPLAT_DATA_TEX_WIDTH as usize;
    let tex_height = (point_count * texels_per_splat).div_ceil(tex_width).max(1);
    let mut data = vec![0.0_f32; tex_width * tex_height * 4];
    let mut positions = Vec::with_capacity(point_count * 4);
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];

    for slot in 0..point_count {
        let off = slot * stride;
        let center = [slice[off], slice[off + 1], slice[off + 2]];
        // Normalize the rotation quaternion (xyzw); fall back to identity if degenerate.
        let (mut qx, mut qy, mut qz, mut qw) = (
            slice[off + 3],
            slice[off + 4],
            slice[off + 5],
            slice[off + 6],
        );
        let qlen = (qx * qx + qy * qy + qz * qz + qw * qw).sqrt();
        if qlen > 1.0e-12 {
            let inv = 1.0 / qlen;
            qx *= inv;
            qy *= inv;
            qz *= inv;
            qw *= inv;
        } else {
            qx = 0.0;
            qy = 0.0;
            qz = 0.0;
            qw = 1.0;
        }
        let sx = slice[off + 7] * scale_multiplier;
        let sy = slice[off + 8] * scale_multiplier;
        let sz = slice[off + 9] * scale_multiplier;
        let base = slot * texels_per_splat * 4;
        // Layout per splat (4 RGBA-float texels), read back by SPLAT_TEXTURE_SHADER:
        //   texel0 = center.xyz, scale.x
        //   texel1 = scale.y, scale.z, quat.x, quat.y
        //   texel2 = quat.z, quat.w, 0, 0
        //   texel3 = color.rgba
        data[base] = center[0];
        data[base + 1] = center[1];
        data[base + 2] = center[2];
        data[base + 3] = sx;
        data[base + 4] = sy;
        data[base + 5] = sz;
        data[base + 6] = qx;
        data[base + 7] = qy;
        data[base + 8] = qz;
        data[base + 9] = qw;
        data[base + 12] = slice[off + 14];
        data[base + 13] = slice[off + 15];
        data[base + 14] = slice[off + 16];
        data[base + 15] = slice[off + 17];
        // Higher-degree SH (degree 1..sh_degree) from the payload at off+18, packed
        // flat into the texels after the four core texels.
        let sh_count = sh_floats(sh_degree);
        for k in 0..sh_count {
            data[base + 16 + k] = slice[off + 18 + k];
        }
        for k in 0..3 {
            min[k] = min[k].min(center[k]);
            max[k] = max[k].max(center[k]);
        }
        positions.extend_from_slice(&[center[0], center[1], center[2], 1.0]);
    }

    let size = [max[0] - min[0], max[1] - min[1], max[2] - min[2]];
    let len = (size[0] * size[0] + size[1] * size[1] + size[2] * size[2]).sqrt();
    let g = len * 0.05 + 0.01;
    Some(RawRenderData {
        data,
        tex_width: tex_width as i32,
        tex_height: tex_height as i32,
        positions,
        splat_count: point_count as i32,
        aabb_pos: [min[0] - g, min[1] - g, min[2] - g],
        aabb_size: [size[0] + 2.0 * g, size[1] + 2.0 * g, size[2] + 2.0 * g],
        sh_degree,
    })
}

// Convert worker output into a `SplatRenderData` on the main thread (Godot types).
pub(super) fn raw_to_render(raw: RawRenderData) -> SplatRenderData {
    SplatRenderData {
        data_bytes: PackedFloat32Array::from(raw.data).to_byte_array(),
        tex_width: raw.tex_width,
        tex_height: raw.tex_height,
        positions_ssbo: PackedFloat32Array::from(raw.positions).to_byte_array(),
        splat_count: raw.splat_count,
        aabb: Aabb::new(
            Vector3::new(raw.aabb_pos[0], raw.aabb_pos[1], raw.aabb_pos[2]),
            Vector3::new(raw.aabb_size[0], raw.aabb_size[1], raw.aabb_size[2]),
        ),
        sh_degree: raw.sh_degree,
    }
}
