use godot::classes::MeshInstance3D;
use godot::prelude::*;

#[derive(GodotClass)]
#[class(tool, init, base=MeshInstance3D)]
pub struct GaussianSplatPreviewMeshInstance3D {
    #[base]
    base: Base<MeshInstance3D>,

    #[var(get, set)]
    #[export]
    preview_max_splats: PhantomVar<i32>,
    #[var(get, set)]
    #[export]
    preview_max_splat_radius: PhantomVar<f32>,
    #[var(get, set)]
    #[export]
    preview_scale_multiplier: PhantomVar<f32>,
    #[var(
        no_set,
        get = get_show_all_preview_splats_button,
        usage_flags = [EDITOR],
        hint = TOOL_BUTTON,
        hint_string = "Show All Preview Splats"
    )]
    show_all_preview_splats_button: PhantomVar<Callable>,
}

#[godot_api]
impl GaussianSplatPreviewMeshInstance3D {
    #[func]
    pub fn get_preview_max_splats(&self) -> i32 {
        self.call_parent_i32("get_preview_max_splats", 0)
    }

    #[func]
    pub fn set_preview_max_splats(&mut self, max_splats: i32) {
        self.call_parent_void("set_preview_max_splats", &[Variant::from(max_splats)]);
    }

    #[func]
    pub fn get_preview_max_splat_radius(&self) -> f32 {
        self.call_parent_f32("get_preview_max_splat_radius", 0.02)
    }

    #[func]
    pub fn set_preview_max_splat_radius(&mut self, max_splat_radius: f32) {
        self.call_parent_void(
            "set_preview_max_splat_radius",
            &[Variant::from(max_splat_radius)],
        );
    }

    #[func]
    pub fn get_preview_scale_multiplier(&self) -> f32 {
        self.call_parent_f32("get_preview_scale_multiplier", 1.0)
    }

    #[func]
    pub fn set_preview_scale_multiplier(&mut self, scale_multiplier: f32) {
        self.call_parent_void(
            "set_preview_scale_multiplier",
            &[Variant::from(scale_multiplier)],
        );
    }

    #[func]
    pub fn show_all_preview_splats(&mut self) {
        self.call_parent_void("show_all_preview_splats", &[]);
    }

    #[func]
    pub fn get_show_all_preview_splats_button(&self) -> Callable {
        self.base().callable("show_all_preview_splats")
    }

    fn call_parent_i32(&self, method: &str, fallback: i32) -> i32 {
        self.call_parent(method, &[])
            .and_then(|value| value.try_to::<i32>().ok())
            .unwrap_or(fallback)
    }

    fn call_parent_f32(&self, method: &str, fallback: f32) -> f32 {
        self.call_parent(method, &[])
            .and_then(|value| value.try_to::<f32>().ok())
            .unwrap_or(fallback)
    }

    fn call_parent_void(&self, method: &str, args: &[Variant]) {
        let _ = self.call_parent(method, args);
    }

    fn call_parent(&self, method: &str, args: &[Variant]) -> Option<Variant> {
        let parent = self.base().get_parent()?;
        if !parent.has_method(method) {
            return None;
        }

        Some(parent.to_variant().call(method, args))
    }
}
