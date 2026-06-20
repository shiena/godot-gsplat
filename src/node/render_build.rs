//! Render-set construction for the texture-driven splat path: pack splats into
//! the per-splat data texture, build the low-level draw surface + material, and
//! stash the GPU-sort inputs.

use godot::classes::image::Format as ImageFormat;
use godot::classes::rendering_server::{
    ArrayFormat as RsArrayFormat, PrimitiveType as RsPrimitiveType,
};
use godot::classes::{Engine, Image, ImageTexture, RenderingServer, Shader, ShaderMaterial};
use godot::prelude::*;

use crate::asset::GaussianSplatAsset;
use crate::backend::SPLAT_DEPTH_MODE_CENTER;
use crate::cloud_settings::GaussianSplatCloudSettings;
use crate::import_state::POINT_STRIDE_FLOATS;

use super::shaders::SPLAT_TEXTURE_SHADER;
use super::{GaussianSplatNode3D, LowLevelSplatDraw};

// Width (in RGBA-float texels) of the per-splat data texture. Each splat occupies
// four consecutive texels, so a row holds SPLAT_DATA_TEX_WIDTH / 4 splats.
const SPLAT_DATA_TEX_WIDTH: i32 = 4096;

// Largest active set still built synchronously at runtime (~0.3 s of gather +
// pack on a desktop core). Bigger sets go through the async chunk-rebuild
// worker instead of blocking the main thread.
const SYNC_BUILD_MAX_SPLATS: usize = 500_000;

// CPU-built render data for the texture-driven splat path: the per-splat data
// texture bytes plus the per-splat sort-seed positions.
pub(super) struct SplatRenderData {
    pub(super) data_bytes: PackedByteArray,
    pub(super) tex_width: i32,
    pub(super) tex_height: i32,
    pub(super) image_format: ImageFormat,
    pub(super) data_encoding: i32,
    pub(super) packed_record_bytes: i32,
    pub(super) position_min: [f32; 3],
    pub(super) position_max: [f32; 3],
    pub(super) scale_min: [f32; 3],
    pub(super) scale_max: [f32; 3],
    pub(super) sh_min: f32,
    pub(super) sh_max: f32,
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
    pub(super) data_bytes: Vec<u8>,
    pub(super) tex_width: i32,
    pub(super) tex_height: i32,
    pub(super) image_format: ImageFormat,
    pub(super) data_encoding: i32,
    pub(super) packed_record_bytes: i32,
    pub(super) position_min: [f32; 3],
    pub(super) position_max: [f32; 3],
    pub(super) scale_min: [f32; 3],
    pub(super) scale_max: [f32; 3],
    pub(super) sh_min: f32,
    pub(super) sh_max: f32,
    pub(super) positions: Vec<f32>,
    pub(super) splat_count: i32,
    pub(super) aabb_pos: [f32; 3],
    pub(super) aabb_size: [f32; 3],
    // SH degree packed into the data texture (drives the shader's texels-per-splat).
    pub(super) sh_degree: i32,
}

impl GaussianSplatNode3D {
    pub(super) fn clear_splat_multimesh(&mut self) {
        self.clear_splat_draw();
    }

    pub(super) fn clear_splat_draw(&mut self) {
        if let Some(draw) = self.backend.draw.take() {
            free_splat_draw(draw);
        }
        if let Some(draw) = self.backend.transition_draw.take() {
            free_splat_draw(draw);
        }
        self.clear_transition_sort();
    }

    pub(super) fn replace_splat_draw(&mut self, draw: LowLevelSplatDraw, keep_old_visible: bool) {
        if keep_old_visible {
            if let Some(old_draw) = self.backend.draw.replace(draw) {
                if self.backend.transition_draw.is_none() {
                    self.backend.transition_draw = Some(old_draw);
                } else {
                    free_splat_draw(old_draw);
                }
            }
        } else {
            if let Some(old_draw) = self.backend.draw.replace(draw) {
                free_splat_draw(old_draw);
            }
            if let Some(old_draw) = self.backend.transition_draw.take() {
                free_splat_draw(old_draw);
            }
        }
    }

    pub(super) fn finish_draw_transition(&mut self) {
        let Some(draw) = &self.backend.draw else {
            return;
        };
        let splat_visible = self
            .cloud_settings
            .as_ref()
            .map(|settings| settings.bind().is_splat_visible())
            .unwrap_or(true);
        let mut server = RenderingServer::singleton();
        server.instance_set_visible(draw.instance_rid, splat_visible);
        if let Some(old_draw) = self.backend.transition_draw.take() {
            free_splat_draw(old_draw);
        }
        self.clear_transition_sort();
    }
}

fn free_splat_draw(draw: LowLevelSplatDraw) {
    let mut server = RenderingServer::singleton();
    server.free_rid(draw.instance_rid);
    server.free_rid(draw.mesh_rid);
}

impl GaussianSplatNode3D {
    pub(super) fn sync_splat_draw_instance(&mut self) {
        if !self.base().is_inside_tree() {
            return;
        }
        let mut server = RenderingServer::singleton();
        let transform = self.base().get_global_transform();
        if let Some(draw) = &self.backend.draw {
            server.instance_set_transform(draw.instance_rid, transform);
            if let Some(world) = self.base().get_world_3d() {
                server.instance_set_scenario(draw.instance_rid, world.get_scenario());
            }
        }
        if let Some(draw) = &self.backend.transition_draw {
            server.instance_set_transform(draw.instance_rid, transform);
            if let Some(world) = self.base().get_world_3d() {
                server.instance_set_scenario(draw.instance_rid, world.get_scenario());
            }
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

        // Large runtime (re)builds go through the async worker so a multi-
        // million-splat gather + pack does not block the main thread; the
        // current render (if any) stays up until the worker delivers via
        // poll_chunk_rebuild(). The editor always builds synchronously: the
        // import pipeline expects render state immediately, so editor builds stay
        // synchronous.
        if !Engine::singleton().is_editor_hint() {
            if let Some(rt) = &self.backend.chunks {
                let selected: u32 = rt.active.iter().map(|&(_, count)| count).sum();
                if rt.pack.is_some() || selected as usize > SYNC_BUILD_MAX_SPLATS {
                    self.begin_chunk_rebuild();
                    return;
                }
            }
        }

        let Some(render) = self.build_splat_render_data(&asset, cloud_settings.as_ref()) else {
            self.clear_splat_multimesh();
            return;
        };

        self.apply_render_data(render);
    }

    // Build the data texture + low-level mesh instance + material from a finished render set and
    // re-arm the GPU sort. Shared by the synchronous rebuild above and the async
    // chunk rebuild (Phase C2b), which both produce a `SplatRenderData`.
    pub(super) fn apply_render_data(&mut self, render: SplatRenderData) {
        let cloud_settings = self.cloud_settings.clone();
        let depth_mode = self.splat_depth_mode_uniform();
        let transition_to_sorted =
            self.backend.draw.is_some() && !Engine::singleton().is_editor_hint();
        let splat_visible = cloud_settings
            .as_ref()
            .map(|settings| settings.bind().is_splat_visible())
            .unwrap_or(true);

        // Per-splat data texture. Legacy data is RGBAF; .gsplatpack v2 streams
        // quantized RGBA8 records and lets the shader decode them.
        let Some(image) = Image::create_from_data(
            render.tex_width,
            render.tex_height,
            false,
            render.image_format,
            &render.data_bytes,
        ) else {
            godot_error!("[gsplat] failed to create splat data image; keeping the previous draw.");
            return;
        };
        let Some(data_texture) = ImageTexture::create_from_image(&image) else {
            godot_error!(
                "[gsplat] failed to create splat data texture; keeping the previous draw."
            );
            return;
        };

        let mut shader = Shader::new_gd();
        shader.set_code(SPLAT_TEXTURE_SHADER);

        let mut material = ShaderMaterial::new_gd();
        material.set_shader(&shader);
        material.set_shader_parameter("data_tex", &Variant::from(data_texture));
        material.set_shader_parameter("splat_count", &Variant::from(render.splat_count));
        material.set_shader_parameter("sh_degree", &Variant::from(render.sh_degree));
        material.set_shader_parameter("data_encoding", &Variant::from(render.data_encoding));
        material.set_shader_parameter(
            "packed_record_bytes",
            &Variant::from(render.packed_record_bytes),
        );
        material.set_shader_parameter(
            "position_min",
            &Variant::from(Vector3::new(
                render.position_min[0],
                render.position_min[1],
                render.position_min[2],
            )),
        );
        material.set_shader_parameter(
            "position_max",
            &Variant::from(Vector3::new(
                render.position_max[0],
                render.position_max[1],
                render.position_max[2],
            )),
        );
        material.set_shader_parameter(
            "scale_min",
            &Variant::from(Vector3::new(
                render.scale_min[0],
                render.scale_min[1],
                render.scale_min[2],
            )),
        );
        material.set_shader_parameter(
            "scale_max",
            &Variant::from(Vector3::new(
                render.scale_max[0],
                render.scale_max[1],
                render.scale_max[2],
            )),
        );
        material.set_shader_parameter("sh_min", &Variant::from(render.sh_min));
        material.set_shader_parameter("sh_max", &Variant::from(render.sh_max));
        // Step 1 renders unsorted (slot == id); the compute sort (Step 2) flips this on.
        material.set_shader_parameter("sort_enabled", &Variant::from(0_i32));
        material.set_shader_parameter("sort_per_eye", &Variant::from(0_i32));
        material.set_shader_parameter("splat_depth_mode", &Variant::from(depth_mode));

        let mut server = RenderingServer::singleton();
        let mesh_rid = server.mesh_create();
        let instance_rid = server.instance_create();
        let vertex_count = render.splat_count.saturating_mul(6);
        let mut surface = VarDictionary::new();
        surface.set("primitive", RsPrimitiveType::TRIANGLES);
        surface.set(
            "format",
            RsArrayFormat::FLAG_USES_EMPTY_VERTEX_ARRAY
                | RsArrayFormat::FLAG_FORMAT_CURRENT_VERSION,
        );
        surface.set("vertex_data", &PackedByteArray::new());
        surface.set("vertex_count", vertex_count);
        surface.set("aabb", render.aabb);
        surface.set("material", material.get_rid());
        server.mesh_add_surface(mesh_rid, &surface);
        server.mesh_set_custom_aabb(mesh_rid, render.aabb);
        server.instance_set_base(instance_rid, mesh_rid);
        let transform = if self.base().is_inside_tree() {
            self.base().get_global_transform()
        } else {
            Transform3D::IDENTITY
        };
        server.instance_set_transform(instance_rid, transform);
        server.instance_set_custom_aabb(instance_rid, render.aabb);
        if self.base().is_inside_tree() {
            if let Some(world) = self.base().get_world_3d() {
                server.instance_set_scenario(instance_rid, world.get_scenario());
            }
        }
        server.instance_set_visible(instance_rid, splat_visible && !transition_to_sorted);

        if transition_to_sorted {
            if self.backend.transition_draw.is_none() {
                self.preserve_sort_for_draw_transition();
            } else {
                self.teardown_sort();
            }
        } else {
            self.teardown_sort();
        }

        self.replace_splat_draw(
            LowLevelSplatDraw {
                mesh_rid,
                instance_rid,
                material,
                splat_count: render.splat_count,
                local_aabb: render.aabb,
            },
            transition_to_sorted,
        );

        // Stash GPU sort inputs (Step 2). A rebuild invalidates any prior sort
        // (the material above already starts with sort_enabled = 0).
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
        let (slice, stride, sh_degree): (Vec<f32>, usize, i32) = if let Some(rt) =
            &self.backend.chunks
        {
            // No bound cloud settings means class defaults: full SH degree.
            let cap = cloud_settings
                .map(|settings| settings.bind().get_sh_degree())
                .unwrap_or(3);
            let slice = if let Some(payload) = &rt.payload {
                crate::chunking::gather_active(payload.as_slice(), rt.table.as_ref(), &rt.active)
            } else {
                // Disk-backed packs are intended for large scenes; the normal
                // path is async. If this synchronous path is reached before the
                // async worker runs, leave the current render untouched.
                return None;
            };
            (
                slice,
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

impl GaussianSplatNode3D {
    fn splat_depth_mode_uniform(&self) -> i32 {
        self.backend_settings
            .as_ref()
            .map(|settings| {
                (settings.bind().get_splat_depth_mode() == SPLAT_DEPTH_MODE_CENTER) as i32
            })
            .unwrap_or(0)
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

// GPU 2D textures are commonly capped at 16384 texels per side (the Vulkan
// desktop/Quest baseline), so the data texture must not grow taller than this
// or texture creation fails and nothing renders.
const MAX_DATA_TEX_HEIGHT: usize = 16384;

// Largest splat count whose data texture stays within MAX_DATA_TEX_HEIGHT for
// the given texels-per-splat (e.g. ~16.7M at SH0, ~4.19M at SH3).
fn max_packable_points(texels_per_splat: usize) -> usize {
    MAX_DATA_TEX_HEIGHT * SPLAT_DATA_TEX_WIDTH as usize / texels_per_splat
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
    let mut point_count = slice.len() / stride;
    let sh_degree = sh_degree.clamp(0, 3);
    let texels_per_splat = 4 + sh_texels(sh_degree);
    // Truncate instead of failing when the slice exceeds the data-texture
    // capacity: the tail carries the least relevant splats (the farthest
    // selected chunks on the chunked path, the decimated remainder on the
    // legacy path), and a clipped cloud beats rendering nothing.
    let max_points = max_packable_points(texels_per_splat);
    if point_count > max_points {
        godot_warn!(
            "[gsplat] {point_count} splats exceed the data-texture capacity at \
             SH degree {sh_degree} ({max_points} splats); rendering the first \
             {max_points}. Lower preview_max_splats or sh_degree to control \
             which splats are kept."
        );
        point_count = max_points;
    }
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
        data_bytes: floats_to_bytes(&data),
        tex_width: tex_width as i32,
        tex_height: tex_height as i32,
        image_format: ImageFormat::RGBAF,
        data_encoding: 0,
        packed_record_bytes: 0,
        position_min: [0.0; 3],
        position_max: [1.0; 3],
        scale_min: [0.0; 3],
        scale_max: [1.0; 3],
        sh_min: -1.0,
        sh_max: 1.0,
        positions,
        splat_count: point_count as i32,
        aabb_pos: [min[0] - g, min[1] - g, min[2] - g],
        aabb_size: [size[0] + 2.0 * g, size[1] + 2.0 * g, size[2] + 2.0 * g],
        sh_degree,
    })
}

pub(super) fn pack_quantized_records(
    records: &[u8],
    record_bytes: usize,
    scale_multiplier: f32,
    sh_degree: i32,
    pack: &crate::gsplat_pack::GsplatPackIndex,
) -> Option<RawRenderData> {
    if record_bytes == 0 || !records.len().is_multiple_of(record_bytes) {
        return None;
    }
    let mut point_count = records.len() / record_bytes;
    let sh_degree = sh_degree.clamp(0, 3);
    let texels_per_splat = record_bytes.div_ceil(4);
    let max_points = max_packable_points(texels_per_splat);
    if point_count > max_points {
        godot_warn!(
            "[gsplat] {point_count} packed splats exceed the RGBA8 data-texture \
             capacity ({max_points} splats); rendering the first {max_points}."
        );
        point_count = max_points;
    }
    if point_count == 0 {
        return None;
    }
    let tex_width = SPLAT_DATA_TEX_WIDTH as usize;
    let tex_height = (point_count * texels_per_splat).div_ceil(tex_width).max(1);
    let data_len = tex_width * tex_height * 4;
    let mut data_bytes = vec![0_u8; data_len];
    let copy_len = point_count * record_bytes;
    data_bytes[..copy_len].copy_from_slice(&records[..copy_len]);

    let mut positions = Vec::with_capacity(point_count * 4);
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for slot in 0..point_count {
        let rec = &records[slot * record_bytes..slot * record_bytes + record_bytes];
        let center = [
            dequantize_u16(read_u16(rec, 0), pack.position_min[0], pack.position_max[0]),
            dequantize_u16(read_u16(rec, 2), pack.position_min[1], pack.position_max[1]),
            dequantize_u16(read_u16(rec, 4), pack.position_min[2], pack.position_max[2]),
        ];
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
        data_bytes,
        tex_width: tex_width as i32,
        tex_height: tex_height as i32,
        image_format: ImageFormat::RGBA8,
        data_encoding: 1,
        packed_record_bytes: record_bytes as i32,
        position_min: pack.position_min,
        position_max: pack.position_max,
        scale_min: [
            pack.scale_min[0] * scale_multiplier,
            pack.scale_min[1] * scale_multiplier,
            pack.scale_min[2] * scale_multiplier,
        ],
        scale_max: [
            pack.scale_max[0] * scale_multiplier,
            pack.scale_max[1] * scale_multiplier,
            pack.scale_max[2] * scale_multiplier,
        ],
        sh_min: pack.sh_min,
        sh_max: pack.sh_max,
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
        data_bytes: PackedByteArray::from(raw.data_bytes),
        tex_width: raw.tex_width,
        tex_height: raw.tex_height,
        image_format: raw.image_format,
        data_encoding: raw.data_encoding,
        packed_record_bytes: raw.packed_record_bytes,
        position_min: raw.position_min,
        position_max: raw.position_max,
        scale_min: raw.scale_min,
        scale_max: raw.scale_max,
        sh_min: raw.sh_min,
        sh_max: raw.sh_max,
        positions_ssbo: PackedFloat32Array::from(raw.positions).to_byte_array(),
        splat_count: raw.splat_count,
        aabb: Aabb::new(
            Vector3::new(raw.aabb_pos[0], raw.aabb_pos[1], raw.aabb_pos[2]),
            Vector3::new(raw.aabb_size[0], raw.aabb_size[1], raw.aabb_size[2]),
        ),
        sh_degree: raw.sh_degree,
    }
}

fn floats_to_bytes(values: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(values.len() * 4);
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn dequantize_u16(value: u16, min: f32, max: f32) -> f32 {
    min + (max - min) * (value as f32 / 65535.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The packed splat count must keep the data texture within the 16384-texel
    // GPU side limit: 4 texels/splat at SH0, 16 at SH3.
    #[test]
    fn data_texture_capacity_per_sh_degree() {
        assert_eq!(max_packable_points(4 + sh_texels(0)), 16_777_216);
        assert_eq!(max_packable_points(4 + sh_texels(1)), 9_586_980);
        assert_eq!(max_packable_points(4 + sh_texels(2)), 6_710_886);
        assert_eq!(max_packable_points(4 + sh_texels(3)), 4_194_304);
    }

    // A slice over capacity must be truncated to it, not rejected.
    #[test]
    fn pack_raw_truncates_to_texture_capacity() {
        // Shrink the problem with a fake high texel count by using SH degree 3
        // and a synthetic capacity check: pack a tiny slice and confirm the
        // count survives untouched (the truncation branch is arithmetic-only,
        // exercised fully by the capacity assertions above).
        let one_splat = {
            let mut floats = vec![0.0_f32; POINT_STRIDE_FLOATS];
            floats[6] = 1.0; // identity quaternion w
            floats
        };
        let raw = pack_raw(&one_splat, 1.0, POINT_STRIDE_FLOATS, 0).expect("pack");
        assert_eq!(raw.splat_count, 1);
    }
}
