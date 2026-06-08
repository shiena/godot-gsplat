use godot::classes::mesh::{ArrayCustomFormat, ArrayFormat, ArrayType, PrimitiveType};
use godot::classes::GltfState;
use godot::classes::{ArrayMesh, MeshInstance3D, Shader, ShaderMaterial};
use godot::obj::EngineBitfield;
use godot::prelude::*;

use crate::asset::GaussianSplatAsset;
use crate::backend::{GaussianSplatBackendSettings, BACKEND_PROFILE_DESKTOP};
use crate::cloud_settings::GaussianSplatCloudSettings;
use crate::import_state::{ImportedSplatMetadata, NODE_STATE_KEY, POINT_STRIDE_FLOATS};
use crate::render_packet::GaussianSplatRenderPacket;

// Anisotropic Gaussian splat shader. Each splat is a screen-aligned quad whose
// corners (UV in [-2, 2]) are stretched along the projected 2D covariance axes,
// so the on-screen footprint is an oriented ellipse. The 3D covariance (upper
// triangle) is passed per-vertex in CUSTOM0/CUSTOM1 and projected to screen
// space with the perspective Jacobian. Alpha is an isotropic Gaussian in the
// stretched corner space, which equals the anisotropic Gaussian on screen.
// NOTE: this path does not depth-sort splats yet, so blending order is only
// approximate; correct back-to-front sorting is the next step.
const GAUSSIAN_BILLBOARD_SHADER: &str = r#"
shader_type spatial;
render_mode unshaded, cull_disabled, blend_mix, depth_draw_never;

varying vec2 v_corner;
varying vec4 v_color;

void vertex() {
    v_corner = UV;
    v_color = COLOR;

    // Reconstruct the symmetric local-space 3D covariance from its upper triangle
    // (passed as three column vectors; the matrix is symmetric so order is moot).
    mat3 cov3d = mat3(
        vec3(CUSTOM0.x, CUSTOM0.y, CUSTOM0.z),
        vec3(CUSTOM0.y, CUSTOM0.w, CUSTOM1.x),
        vec3(CUSTOM0.z, CUSTOM1.x, CUSTOM1.y));

    vec4 center_view = VIEW_MATRIX * MODEL_MATRIX * vec4(VERTEX, 1.0);
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
}

#[godot_api]
impl INode3D for GaussianSplatNode3D {
    fn ready(&mut self) {
        self.sync_runtime_state();
        self.sync_node_name();
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

        self.ensure_debug_mesh_instance();

        let Some((positions, uvs, colors, custom0, custom1, indices)) =
            self.build_gaussian_billboard_arrays(&asset, cloud_settings.as_ref())
        else {
            self.clear_debug_mesh();
            return;
        };

        if positions.is_empty() {
            self.clear_debug_mesh();
            return;
        }

        let Some(mesh_instance) = &mut self.debug_mesh_instance else {
            return;
        };

        let mut arrays = VarArray::new();
        for _ in 0..ArrayType::MAX.ord() {
            arrays.push(&Variant::nil());
        }
        arrays.set(ArrayType::VERTEX.ord() as usize, &Variant::from(positions));
        arrays.set(ArrayType::TEX_UV.ord() as usize, &Variant::from(uvs));
        arrays.set(ArrayType::COLOR.ord() as usize, &Variant::from(colors));
        arrays.set(ArrayType::CUSTOM0.ord() as usize, &Variant::from(custom0));
        arrays.set(ArrayType::CUSTOM1.ord() as usize, &Variant::from(custom1));
        arrays.set(ArrayType::INDEX.ord() as usize, &Variant::from(indices));

        // CUSTOM0/CUSTOM1 carry the 3D covariance as four float channels each.
        let custom_format = ArrayCustomFormat::RGBA_FLOAT.ord() as u64;
        let surface_flags = ArrayFormat::from_ord(
            (custom_format << ArrayFormat::CUSTOM0_SHIFT.ord())
                | (custom_format << ArrayFormat::CUSTOM1_SHIFT.ord()),
        );

        let mut mesh = ArrayMesh::new_gd();
        mesh.add_surface_from_arrays_ex(PrimitiveType::TRIANGLES, &arrays)
            .flags(surface_flags)
            .done();

        let mut shader = Shader::new_gd();
        shader.set_code(GAUSSIAN_BILLBOARD_SHADER);

        let mut material = ShaderMaterial::new_gd();
        material.set_shader(&shader);

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
    }

    fn build_gaussian_billboard_arrays(
        &self,
        asset: &Gd<GaussianSplatAsset>,
        cloud_settings: Option<&Gd<GaussianSplatCloudSettings>>,
    ) -> Option<(
        PackedVector3Array,
        PackedVector2Array,
        PackedColorArray,
        PackedFloat32Array,
        PackedFloat32Array,
        PackedInt32Array,
    )> {
        let values = {
            let asset_ref = asset.bind();
            asset_ref.payload_float_values()?
        };

        let source_point_count = values.len() / POINT_STRIDE_FLOATS;
        let max_splats = cloud_settings
            .map(|settings| settings.bind().get_max_debug_splats().max(0) as usize)
            .unwrap_or(500_000);
        let point_count = source_point_count.min(max_splats);
        if point_count == 0 || point_count > (i32::MAX as usize / 4) {
            return None;
        }
        let sample_stride = source_point_count.div_ceil(point_count);

        let scale_multiplier = cloud_settings
            .map(|settings| settings.bind().get_gaussian_scale_multiplier())
            .unwrap_or(1.0)
            .max(0.01);
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
        let mut colors = Vec::with_capacity(point_count * 4);
        let mut custom0 = Vec::with_capacity(point_count * 4 * 4);
        let mut custom1 = Vec::with_capacity(point_count * 4 * 4);
        let mut indices = Vec::with_capacity(point_count * 6);

        for output_index in 0..point_count {
            let point_index = (output_index * sample_stride).min(source_point_count - 1);
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
            let color = Color::from_rgba(
                values[offset + 14],
                values[offset + 15],
                values[offset + 16],
                values[offset + 17],
            );

            for corner in corners {
                positions.push(center);
                uvs.push(corner);
                colors.push(color);
                custom0.extend_from_slice(&[cov[0], cov[1], cov[2], cov[3]]);
                custom1.extend_from_slice(&[cov[4], cov[5], 0.0, 0.0]);
            }

            let base = (output_index * 4) as i32;
            indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        }

        Some((
            PackedVector3Array::from(positions),
            PackedVector2Array::from(uvs),
            PackedColorArray::from(colors),
            PackedFloat32Array::from(custom0),
            PackedFloat32Array::from(custom1),
            PackedInt32Array::from(indices),
        ))
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
    [dot(0, 0), dot(0, 1), dot(0, 2), dot(1, 1), dot(1, 2), dot(2, 2)]
}
