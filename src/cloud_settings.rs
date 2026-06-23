use godot::prelude::*;

// Chunk-selection strategies for a budget-limited active set. `nearest` fills the
// budget with the chunks closest to the camera (dense local bubble; the selection
// boundary can cut through surfaces of a capture larger than the budget).
// `coverage` spreads the budget across every chunk proportionally, keeping each
// chunk's importance-ranked prefix — the whole extent stays visible at reduced
// density, which reads better inside room-scale captures.
// `view_priority` keeps a wide view cone and full-distance range represented,
// then lowers density on distant / peripheral chunks when the target budget would
// otherwise be exceeded.
pub const CHUNK_SELECTION_NEAREST: &str = "nearest";
pub const CHUNK_SELECTION_COVERAGE: &str = "coverage";
pub const CHUNK_SELECTION_VIEW_PRIORITY: &str = "view_priority";

pub const DEFAULT_VIEW_PRIORITY_FOV_DEGREES: f32 = 200.0;
pub const DEFAULT_VIEW_PRIORITY_FULL_DISTANCE: f32 = 5.0;
pub const DEFAULT_VIEW_PRIORITY_TARGET_BUDGET: i32 = 800_000;
pub const DEFAULT_VIEW_PRIORITY_MIN_LOD_PER_CHUNK: i32 = 256;
pub const DEFAULT_XR_FIXED_SPLAT_BUDGET: i32 = 600_000;

#[derive(GodotClass)]
#[class(tool, base=Resource)]
pub struct GaussianSplatCloudSettings {
    #[base]
    base: Base<Resource>,

    splat_visible: bool,
    render_enabled: bool,
    gaussian_scale_multiplier: f32,
    max_preview_splat_radius: f32,
    max_preview_splats: i32,
    // Spherical-harmonics degree (0-3) to evaluate for view-dependent color. Capped
    // at the degree the source glTF actually provides.
    sh_degree: i32,
    chunk_selection: GString,
    view_priority_fov_degrees: f32,
    view_priority_full_distance: f32,
    view_priority_target_budget: i32,
    view_priority_min_lod_per_chunk: i32,
    xr_fixed_budget_enabled: bool,
    xr_fixed_splat_budget: i32,
}

#[godot_api]
impl IResource for GaussianSplatCloudSettings {
    fn init(base: Base<Resource>) -> Self {
        Self {
            base,
            splat_visible: true,
            render_enabled: true,
            gaussian_scale_multiplier: 1.0,
            max_preview_splat_radius: 0.02,
            // i32::MAX = "show every splat"; the node clamps it to the asset's
            // point count once an asset is bound, so a raw glTF loaded at runtime
            // (no .import) previews all of its points.
            max_preview_splats: i32::MAX,
            sh_degree: 3,
            chunk_selection: CHUNK_SELECTION_NEAREST.into(),
            view_priority_fov_degrees: DEFAULT_VIEW_PRIORITY_FOV_DEGREES,
            view_priority_full_distance: DEFAULT_VIEW_PRIORITY_FULL_DISTANCE,
            view_priority_target_budget: DEFAULT_VIEW_PRIORITY_TARGET_BUDGET,
            view_priority_min_lod_per_chunk: DEFAULT_VIEW_PRIORITY_MIN_LOD_PER_CHUNK,
            xr_fixed_budget_enabled: false,
            xr_fixed_splat_budget: DEFAULT_XR_FIXED_SPLAT_BUDGET,
        }
    }
}

#[godot_api]
impl GaussianSplatCloudSettings {
    #[func]
    pub fn is_splat_visible(&self) -> bool {
        self.splat_visible
    }

    #[func]
    pub fn set_splat_visible(&mut self, splat_visible: bool) {
        self.splat_visible = splat_visible;
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn is_render_enabled(&self) -> bool {
        self.render_enabled
    }

    #[func]
    pub fn set_render_enabled(&mut self, render_enabled: bool) {
        self.render_enabled = render_enabled;
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn get_gaussian_scale_multiplier(&self) -> f32 {
        self.gaussian_scale_multiplier
    }

    #[func]
    pub fn set_gaussian_scale_multiplier(&mut self, gaussian_scale_multiplier: f32) {
        self.gaussian_scale_multiplier = gaussian_scale_multiplier.max(0.01);
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn get_max_preview_splat_radius(&self) -> f32 {
        self.max_preview_splat_radius
    }

    #[func]
    pub fn set_max_preview_splat_radius(&mut self, max_preview_splat_radius: f32) {
        self.max_preview_splat_radius = max_preview_splat_radius.max(0.001);
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn get_max_preview_splats(&self) -> i32 {
        self.max_preview_splats
    }

    #[func]
    pub fn set_max_preview_splats(&mut self, max_preview_splats: i32) {
        self.max_preview_splats = max_preview_splats.max(0);
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn get_sh_degree(&self) -> i32 {
        self.sh_degree
    }

    #[func]
    pub fn set_sh_degree(&mut self, sh_degree: i32) {
        self.sh_degree = sh_degree.clamp(0, 3);
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn get_chunk_selection(&self) -> GString {
        self.chunk_selection.clone()
    }

    #[func]
    pub fn set_chunk_selection(&mut self, chunk_selection: GString) {
        self.chunk_selection =
            normalize_chunk_selection(chunk_selection.to_string().as_str()).into();
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn get_view_priority_fov_degrees(&self) -> f32 {
        self.view_priority_fov_degrees
    }

    #[func]
    pub fn set_view_priority_fov_degrees(&mut self, fov_degrees: f32) {
        self.view_priority_fov_degrees = fov_degrees.clamp(1.0, 360.0);
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn get_view_priority_full_distance(&self) -> f32 {
        self.view_priority_full_distance
    }

    #[func]
    pub fn set_view_priority_full_distance(&mut self, full_distance: f32) {
        self.view_priority_full_distance = full_distance.max(0.0);
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn get_view_priority_target_budget(&self) -> i32 {
        self.view_priority_target_budget
    }

    #[func]
    pub fn set_view_priority_target_budget(&mut self, target_budget: i32) {
        self.view_priority_target_budget = target_budget.max(0);
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn get_view_priority_min_lod_per_chunk(&self) -> i32 {
        self.view_priority_min_lod_per_chunk
    }

    #[func]
    pub fn set_view_priority_min_lod_per_chunk(&mut self, min_lod_per_chunk: i32) {
        self.view_priority_min_lod_per_chunk = min_lod_per_chunk.max(1);
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn is_xr_fixed_budget_enabled(&self) -> bool {
        self.xr_fixed_budget_enabled
    }

    #[func]
    pub fn set_xr_fixed_budget_enabled(&mut self, enabled: bool) {
        self.xr_fixed_budget_enabled = enabled;
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn get_xr_fixed_splat_budget(&self) -> i32 {
        self.xr_fixed_splat_budget
    }

    #[func]
    pub fn set_xr_fixed_splat_budget(&mut self, budget: i32) {
        self.xr_fixed_splat_budget = budget.max(0);
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn apply_defaults(&mut self) {
        self.splat_visible = true;
        self.render_enabled = true;
        self.gaussian_scale_multiplier = 1.0;
        self.max_preview_splat_radius = 0.02;
        self.max_preview_splats = i32::MAX;
        self.sh_degree = 3;
        self.chunk_selection = CHUNK_SELECTION_NEAREST.into();
        self.view_priority_fov_degrees = DEFAULT_VIEW_PRIORITY_FOV_DEGREES;
        self.view_priority_full_distance = DEFAULT_VIEW_PRIORITY_FULL_DISTANCE;
        self.view_priority_target_budget = DEFAULT_VIEW_PRIORITY_TARGET_BUDGET;
        self.view_priority_min_lod_per_chunk = DEFAULT_VIEW_PRIORITY_MIN_LOD_PER_CHUNK;
        self.xr_fixed_budget_enabled = false;
        self.xr_fixed_splat_budget = DEFAULT_XR_FIXED_SPLAT_BUDGET;
        self.base_mut().emit_changed();
    }
}

fn normalize_chunk_selection(chunk_selection: &str) -> &'static str {
    match chunk_selection {
        CHUNK_SELECTION_COVERAGE => CHUNK_SELECTION_COVERAGE,
        CHUNK_SELECTION_VIEW_PRIORITY => CHUNK_SELECTION_VIEW_PRIORITY,
        _ => CHUNK_SELECTION_NEAREST,
    }
}
