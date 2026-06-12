use godot::prelude::*;

// Chunk-selection strategies for a budget-limited active set. `nearest` fills the
// budget with the chunks closest to the camera (dense local bubble; the selection
// boundary can cut through surfaces of a capture larger than the budget).
// `coverage` spreads the budget across every chunk proportionally, keeping each
// chunk's importance-ranked prefix — the whole extent stays visible at reduced
// density, which reads better inside room-scale captures.
pub const CHUNK_SELECTION_NEAREST: &str = "nearest";
pub const CHUNK_SELECTION_COVERAGE: &str = "coverage";

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
    pub fn apply_defaults(&mut self) {
        self.splat_visible = true;
        self.render_enabled = true;
        self.gaussian_scale_multiplier = 1.0;
        self.max_preview_splat_radius = 0.02;
        self.max_preview_splats = i32::MAX;
        self.sh_degree = 3;
        self.chunk_selection = CHUNK_SELECTION_NEAREST.into();
        self.base_mut().emit_changed();
    }
}

fn normalize_chunk_selection(chunk_selection: &str) -> &'static str {
    match chunk_selection {
        CHUNK_SELECTION_COVERAGE => CHUNK_SELECTION_COVERAGE,
        _ => CHUNK_SELECTION_NEAREST,
    }
}
