use godot::classes::mesh::{ArrayType, PrimitiveType};
use godot::classes::GltfState;
use godot::classes::{ArrayMesh, MeshInstance3D, Shader, ShaderMaterial};
use godot::prelude::*;

use crate::asset::GaussianSplatAsset;
use crate::backend::{GaussianSplatBackendSettings, BACKEND_PROFILE_DESKTOP};
use crate::cloud_settings::GaussianSplatCloudSettings;
use crate::import_state::{ImportedSplatMetadata, NODE_STATE_KEY, POINT_STRIDE_FLOATS};
use crate::render_packet::GaussianSplatRenderPacket;

const GAUSSIAN_BILLBOARD_SHADER: &str = r#"
shader_type spatial;
render_mode unshaded, cull_disabled, blend_mix, depth_prepass_alpha;

varying vec2 splat_offset;

void vertex() {
    splat_offset = UV;

    vec3 center_world = (MODEL_MATRIX * vec4(VERTEX, 1.0)).xyz;
    vec3 camera_right = INV_VIEW_MATRIX[0].xyz;
    vec3 camera_up = INV_VIEW_MATRIX[1].xyz;
    vec3 world_offset = camera_right * UV.x * UV2.x + camera_up * UV.y * UV2.y;

    VERTEX = (inverse(MODEL_MATRIX) * vec4(center_world + world_offset, 1.0)).xyz;
}

void fragment() {
    float radius2 = dot(splat_offset, splat_offset);
    float alpha = COLOR.a * exp(-4.5 * radius2);
    if (alpha < 0.003) {
        discard;
    }

    ALBEDO = COLOR.rgb;
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
            .unwrap_or(10_000)
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

        let Some((positions, uvs, uv2s, colors, indices)) =
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
        arrays.set(ArrayType::TEX_UV2.ord() as usize, &Variant::from(uv2s));
        arrays.set(ArrayType::COLOR.ord() as usize, &Variant::from(colors));
        arrays.set(ArrayType::INDEX.ord() as usize, &Variant::from(indices));

        let mut mesh = ArrayMesh::new_gd();
        mesh.add_surface_from_arrays(PrimitiveType::TRIANGLES, &arrays);

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
        PackedVector2Array,
        PackedColorArray,
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
            .unwrap_or(3.0)
            .max(0.01);
        let max_splat_radius = cloud_settings
            .map(|settings| settings.bind().get_max_debug_splat_radius())
            .unwrap_or(0.02)
            .max(0.001);
        let corners = [
            Vector2::new(-1.0, -1.0),
            Vector2::new(1.0, -1.0),
            Vector2::new(1.0, 1.0),
            Vector2::new(-1.0, 1.0),
        ];

        let mut positions = Vec::with_capacity(point_count * 4);
        let mut uvs = Vec::with_capacity(point_count * 4);
        let mut uv2s = Vec::with_capacity(point_count * 4);
        let mut colors = Vec::with_capacity(point_count * 4);
        let mut indices = Vec::with_capacity(point_count * 6);

        for output_index in 0..point_count {
            let point_index = (output_index * sample_stride).min(source_point_count - 1);
            let offset = point_index * POINT_STRIDE_FLOATS;
            let center = Vector3::new(values[offset], values[offset + 1], values[offset + 2]);
            let radius = values[offset + 7]
                .max(values[offset + 8])
                .max(values[offset + 9])
                .max(0.0001)
                * scale_multiplier;
            let preview_radius = radius.clamp(0.001, max_splat_radius);
            let size = Vector2::new(preview_radius, preview_radius);
            let color = Color::from_rgba(
                values[offset + 14],
                values[offset + 15],
                values[offset + 16],
                values[offset + 17],
            );

            for corner in corners {
                positions.push(center);
                uvs.push(corner);
                uv2s.push(size);
                colors.push(color);
            }

            let base = (output_index * 4) as i32;
            indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        }

        Some((
            PackedVector3Array::from(positions),
            PackedVector2Array::from(uvs),
            PackedVector2Array::from(uv2s),
            PackedColorArray::from(colors),
            PackedInt32Array::from(indices),
        ))
    }
}
