use godot::prelude::*;

#[derive(GodotClass)]
#[class(tool, base=Resource)]
pub struct GaussianSplatCloudSettings {
    #[base]
    base: Base<Resource>,

    debug_point_size: f32,
    debug_visible: bool,
    debug_fallback_enabled: bool,
    gaussian_scale_multiplier: f32,
    max_debug_splats: i32,
}

#[godot_api]
impl IResource for GaussianSplatCloudSettings {
    fn init(base: Base<Resource>) -> Self {
        Self {
            base,
            debug_point_size: 24.0,
            debug_visible: true,
            debug_fallback_enabled: false,
            gaussian_scale_multiplier: 3.0,
            max_debug_splats: 10_000,
        }
    }
}

#[godot_api]
impl GaussianSplatCloudSettings {
    #[func]
    pub fn get_debug_point_size(&self) -> f32 {
        self.debug_point_size
    }

    #[func]
    pub fn set_debug_point_size(&mut self, debug_point_size: f32) {
        self.debug_point_size = debug_point_size.max(1.0);
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn is_debug_visible(&self) -> bool {
        self.debug_visible
    }

    #[func]
    pub fn set_debug_visible(&mut self, debug_visible: bool) {
        self.debug_visible = debug_visible;
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn is_debug_fallback_enabled(&self) -> bool {
        self.debug_fallback_enabled
    }

    #[func]
    pub fn set_debug_fallback_enabled(&mut self, debug_fallback_enabled: bool) {
        self.debug_fallback_enabled = debug_fallback_enabled;
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
    pub fn get_max_debug_splats(&self) -> i32 {
        self.max_debug_splats
    }

    #[func]
    pub fn set_max_debug_splats(&mut self, max_debug_splats: i32) {
        self.max_debug_splats = max_debug_splats.max(1);
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn apply_debug_defaults(&mut self) {
        self.debug_point_size = 24.0;
        self.debug_visible = true;
        self.debug_fallback_enabled = true;
        self.gaussian_scale_multiplier = 3.0;
        self.max_debug_splats = 10_000;
        self.base_mut().emit_changed();
    }
}
