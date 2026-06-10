use godot::classes::{
    Camera3D, EditorInterface, Engine, GltfState, MultiMeshInstance3D, Node3D, ShaderMaterial,
};
use godot::prelude::*;

use crate::asset::GaussianSplatAsset;
use crate::backend::{
    GaussianSplatBackendSettings, BACKEND_PROFILE_DESKTOP, BACKEND_PROFILE_MOBILE,
    BACKEND_PROFILE_VR_SAFE,
};
use crate::cloud_settings::GaussianSplatCloudSettings;
use crate::import_state::{ImportedSplatMetadata, NODE_STATE_KEY};

mod chunk_streaming;
mod gpu_sort;
mod render_build;
mod shaders;

use chunk_streaming::ChunkRuntime;
use gpu_sort::SortGpu;

// All rendering-backend state owned by the node, kept apart from the scene-graph
// state: the GPU depth sort and the chunk-streaming runtime. GPU resources are
// freed explicitly via teardown_sort() on exit_tree (not Drop: freeing RIDs needs
// the RenderingServer, which is not guaranteed to outlive the node at shutdown).
#[derive(Default)]
struct SplatRenderBackend {
    sort: SortGpu,
    chunks: Option<ChunkRuntime>,
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
// SH degree cap per render profile (Low cheapest for VR/mobile, High full fidelity).
const RENDER_PROFILE_LOW_SH_DEGREE: i32 = 0;
const RENDER_PROFILE_MIDDLE_SH_DEGREE: i32 = 1;
const RENDER_PROFILE_HIGH_SH_DEGREE: i32 = 3;

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
    #[var(get, set)]
    #[export]
    sh_degree: PhantomVar<i32>,
    #[var(get, set, usage_flags = [EDITOR])]
    show_all_preview_splats_action: PhantomVar<bool>,
    // The decoded asset is not serialized into the .scn, so persist the point
    // count here to recover it after a scene reload.
    #[var(get, set, usage_flags = [STORAGE])]
    imported_point_count: PhantomVar<i32>,
    is_bound: bool,
    transform_state: NodeTransformState,
    visibility_state: NodeVisibilityState,
    backend_state: NodeBackendState,
    splat_multimesh: Option<Gd<MultiMeshInstance3D>>,
    backend: SplatRenderBackend,
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
        // Run the per-frame loop (chunk streaming + GPU depth sort) in both the editor
        // and at runtime, so the editor preview is depth-sorted too (unsorted splats
        // over-blend and look washed out). The sort is view-change-gated, so a static
        // editor view costs nothing.
        self.base_mut().set_process(true);
    }

    fn process(&mut self, _delta: f64) {
        // Apply any finished async chunk rebuild, then re-select nearby chunks when
        // the camera crosses a boundary (Phase C2/C2b). Applying a rebuild tears down
        // the sort, which the block below brings back up at the new active count.
        self.poll_chunk_rebuild();
        self.update_chunk_selection();
        if !self.backend.sort.ready && !self.backend.sort.attempted {
            self.try_enable_sort();
        }
        if !self.backend.sort.ready {
            return;
        }
        let eyes = self.current_sort_views();
        let Some(&(primary, _)) = eyes.first() else {
            return;
        };
        // Re-sort only when the camera/node view changes meaningfully; a static
        // view keeps the last back-to-front order, saving per-frame GPU work.
        let should_sort = match self.backend.sort.last_view {
            Some(last) => self.sort_view_changed(last, primary),
            None => true,
        };
        if should_sort {
            self.dispatch_sort(&eyes);
            self.backend.sort.last_view = Some(primary);
        }
        // Enable sorted sampling one frame after the first dispatch, so the sort
        // texture is registered and written before the material binds it.
        if self.backend.sort.last_view.is_some() && !self.backend.sort.enabled_in_shader {
            if self.backend.sort.dispatched_once {
                self.set_material_sort(true);
                self.backend.sort.enabled_in_shader = true;
            }
            self.backend.sort.dispatched_once = true;
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
        let (target_hint, budget, sh_degree) = match profile {
            RenderProfile::Custom => return,
            RenderProfile::Low => (
                BACKEND_PROFILE_VR_SAFE,
                RENDER_PROFILE_LOW_SPLATS,
                RENDER_PROFILE_LOW_SH_DEGREE,
            ),
            RenderProfile::Middle => (
                BACKEND_PROFILE_MOBILE,
                RENDER_PROFILE_MIDDLE_SPLATS,
                RENDER_PROFILE_MIDDLE_SH_DEGREE,
            ),
            RenderProfile::High => (
                BACKEND_PROFILE_DESKTOP,
                RENDER_PROFILE_HIGH_SPLATS,
                RENDER_PROFILE_HIGH_SH_DEGREE,
            ),
        };
        self.ensure_backend_settings();
        if let Some(backend_settings) = &mut self.backend_settings {
            backend_settings
                .bind_mut()
                .set_target_hint(target_hint.into());
        }
        self.backend_state.profile_hint = self.resolve_backend_pipeline();
        self.mark_backend_dirty("render_profile");
        self.ensure_cloud_settings();
        if let Some(settings) = &mut self.cloud_settings {
            settings.bind_mut().set_sh_degree(sh_degree);
        }
        // The budget caps the rendered splat count and rebuilds the render. Guard
        // so these preset-driven writes do not flip the profile back to Custom.
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
        self.is_bound = false;
        self.clear_splat_multimesh();
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
        GString::from(self.imported_metadata().summary().as_str())
    }

    #[func]
    pub fn export_import_metadata(&self) -> VarDictionary {
        self.imported_metadata().to_dictionary()
    }

    // Import metadata is owned by the bound asset; the node derives it on demand
    // instead of holding a runtime copy.
    fn imported_metadata(&self) -> ImportedSplatMetadata {
        self.asset
            .as_ref()
            .map(|asset| {
                ImportedSplatMetadata::from_dictionary(asset.bind().export_import_metadata())
            })
            .unwrap_or_default()
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
        self.rebuild_splat_multimesh();
    }

    #[func]
    pub fn get_cloud_settings(&self) -> Option<Gd<GaussianSplatCloudSettings>> {
        self.cloud_settings.clone()
    }

    #[func]
    pub fn get_preview_max_splats(&self) -> i32 {
        self.cloud_settings
            .as_ref()
            .map(|settings| settings.bind().get_max_preview_splats())
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
            settings.bind_mut().set_max_preview_splats(max_splats);
        }
        self.mark_backend_dirty("preview_max_splats");
        self.rebuild_splat_multimesh();
    }

    #[func]
    pub fn get_sh_degree(&self) -> i32 {
        self.cloud_settings
            .as_ref()
            .map(|settings| settings.bind().get_sh_degree())
            .unwrap_or(3)
    }

    #[func]
    pub fn set_sh_degree(&mut self, sh_degree: i32) {
        // A manual SH-degree edit no longer matches a fixed preset, so drop to Custom.
        if !self.applying_profile {
            self.render_profile_value = RenderProfile::Custom;
        }
        self.ensure_cloud_settings();
        if let Some(settings) = &mut self.cloud_settings {
            settings.bind_mut().set_sh_degree(sh_degree);
        }
        self.mark_backend_dirty("sh_degree");
        self.rebuild_splat_multimesh();
    }

    #[func]
    pub fn get_preview_max_splat_radius(&self) -> f32 {
        self.cloud_settings
            .as_ref()
            .map(|settings| settings.bind().get_max_preview_splat_radius())
            .unwrap_or(0.02)
    }

    #[func]
    pub fn set_preview_max_splat_radius(&mut self, max_splat_radius: f32) {
        self.ensure_cloud_settings();
        if let Some(settings) = &mut self.cloud_settings {
            settings
                .bind_mut()
                .set_max_preview_splat_radius(max_splat_radius);
        }
        self.mark_backend_dirty("preview_max_splat_radius");
        self.rebuild_splat_multimesh();
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
        self.rebuild_splat_multimesh();
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
        dict.set(
            "metadata",
            &Variant::from(self.imported_metadata().to_dictionary()),
        );
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
            let dict = self.imported_metadata().to_dictionary();
            state.set_additional_data(NODE_STATE_KEY, &Variant::from(dict));
        }
    }

    fn refresh_from_asset(&mut self) {
        self.ensure_cloud_settings();
        self.ensure_backend_settings();
        if let Some(asset) = &self.asset {
            let asset = asset.clone();
            let asset_ref = asset.bind();
            self.is_bound = true;
            self.visibility_state.asset_ready = true;
            self.backend_state.asset_point_count = asset_ref.get_point_count();
            self.backend_state.profile_hint = self.resolve_backend_pipeline();
            drop(asset_ref);
            self.clamp_preview_settings_to_asset();
        } else {
            self.is_bound = false;
            self.visibility_state.asset_ready = false;
            self.backend_state.asset_point_count = 0;
            self.backend_state.profile_hint.clear();
        }
        self.mark_backend_dirty("asset");
        self.refresh_chunk_runtime();
        self.rebuild_splat_multimesh();
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
            settings.bind_mut().set_max_preview_splats(max_splats);
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
                    .resolve_pipeline_for_metadata(&self.imported_metadata())
            })
            .unwrap_or_else(|| "unconfigured".to_string())
    }

    fn sync_node_name(&mut self) {
        let name = if self.is_bound {
            let summary = self.imported_metadata().summary();
            format!("GaussianSplatNode3D ({summary})")
        } else {
            "GaussianSplatNode3D".to_string()
        };
        self.base_mut().set_name(name.as_str());
    }

    // The camera the sort/selection tracks: in the editor, the 3D viewport's
    // navigation camera (so the preview follows the editor view); at runtime, the
    // scene's active camera.
    fn active_camera(&self) -> Option<Gd<Camera3D>> {
        if Engine::singleton().is_editor_hint() {
            EditorInterface::singleton()
                .get_editor_viewport_3d()
                .and_then(|viewport| viewport.get_camera_3d())
        } else {
            self.base().get_viewport()?.get_camera_3d()
        }
    }

    fn camera_local_pos(&self) -> Option<Vector3> {
        let camera = self.active_camera()?;
        let cam_world = camera.get_global_transform().origin;
        Some(self.base().get_global_transform().affine_inverse() * cam_world)
    }

    // Reconnect the field to the baked render child when the node is deserialized
    // from a pre-imported .scn (the field itself is not serialized). A baked mesh
    // means there is renderable data even without a live asset.
    fn adopt_serialized_render(&mut self) {
        if self.splat_multimesh.is_some() {
            return;
        }
        for child in self.base().get_children().iter_shared() {
            if let Ok(mesh_instance) = child.try_cast::<MultiMeshInstance3D>() {
                if mesh_instance.get_multimesh().is_some() {
                    self.visibility_state.asset_ready = true;
                    self.splat_multimesh = Some(mesh_instance);
                    return;
                }
            }
        }
    }

    fn splat_material(&self) -> Option<Gd<ShaderMaterial>> {
        self.splat_multimesh
            .as_ref()
            .and_then(|mesh_instance| mesh_instance.get_material_override())
            .and_then(|material| material.try_cast::<ShaderMaterial>().ok())
    }
}
