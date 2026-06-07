use godot::prelude::*;

use crate::import_state::ImportedSplatMetadata;

pub const BACKEND_PROFILE_AUTOMATIC: &str = "automatic";
pub const BACKEND_PROFILE_DESKTOP: &str = "desktop";
pub const BACKEND_PROFILE_MOBILE: &str = "mobile";
pub const BACKEND_PROFILE_VR_SAFE: &str = "vr_safe";

pub const PIPELINE_DESKTOP_DIRECT: &str = "desktop_direct";
pub const PIPELINE_DESKTOP_CLUSTERED: &str = "desktop_clustered";
pub const PIPELINE_MOBILE_DIRECT: &str = "mobile_direct";
pub const PIPELINE_VR_SAFE_SPATIAL: &str = "vr_safe_spatial";
pub const PIPELINE_DESKTOP_COMPOSITOR: &str = "desktop_compositor";

#[derive(GodotClass)]
#[class(base=Resource)]
pub struct GaussianSplatBackendSettings {
    #[base]
    base: Base<Resource>,

    profile: GString,
    target_hint: GString,
    allow_compositor: bool,
    desktop_cluster_threshold: i32,
    mobile_point_budget: i32,
    vr_point_budget: i32,
}

#[godot_api]
impl IResource for GaussianSplatBackendSettings {
    fn init(base: Base<Resource>) -> Self {
        Self {
            base,
            profile: BACKEND_PROFILE_AUTOMATIC.into(),
            target_hint: BACKEND_PROFILE_DESKTOP.into(),
            allow_compositor: false,
            desktop_cluster_threshold: 200_000,
            mobile_point_budget: 131_072,
            vr_point_budget: 262_144,
        }
    }
}

#[godot_api]
impl GaussianSplatBackendSettings {
    #[func]
    pub fn get_profile(&self) -> GString {
        self.profile.clone()
    }

    #[func]
    pub fn set_profile(&mut self, profile: GString) {
        self.profile = profile;
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn get_target_hint(&self) -> GString {
        self.target_hint.clone()
    }

    #[func]
    pub fn set_target_hint(&mut self, target_hint: GString) {
        self.target_hint = target_hint;
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn is_compositor_allowed(&self) -> bool {
        self.allow_compositor
    }

    #[func]
    pub fn set_allow_compositor(&mut self, allow_compositor: bool) {
        self.allow_compositor = allow_compositor;
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn resolve_pipeline(&self, metadata: VarDictionary) -> GString {
        let metadata = ImportedSplatMetadata::from_dictionary(metadata);
        GString::from(self.resolve_pipeline_for_metadata(&metadata).as_str())
    }

    #[func]
    pub fn export_settings(&self) -> VarDictionary {
        let mut dict = VarDictionary::new();
        dict.set("profile", &Variant::from(self.profile.clone()));
        dict.set("target_hint", &Variant::from(self.target_hint.clone()));
        dict.set("allow_compositor", self.allow_compositor);
        dict.set(
            "desktop_cluster_threshold",
            self.desktop_cluster_threshold as i64,
        );
        dict.set("mobile_point_budget", self.mobile_point_budget as i64);
        dict.set("vr_point_budget", self.vr_point_budget as i64);
        dict
    }

    #[func]
    pub fn apply_desktop_defaults(&mut self) {
        self.profile = BACKEND_PROFILE_AUTOMATIC.into();
        self.target_hint = BACKEND_PROFILE_DESKTOP.into();
        self.allow_compositor = false;
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn apply_mobile_defaults(&mut self) {
        self.profile = BACKEND_PROFILE_MOBILE.into();
        self.target_hint = BACKEND_PROFILE_MOBILE.into();
        self.allow_compositor = false;
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn apply_vr_safe_defaults(&mut self) {
        self.profile = BACKEND_PROFILE_VR_SAFE.into();
        self.target_hint = BACKEND_PROFILE_VR_SAFE.into();
        self.allow_compositor = false;
        self.base_mut().emit_changed();
    }
}

impl GaussianSplatBackendSettings {
    pub fn resolve_pipeline_for_metadata(&self, metadata: &ImportedSplatMetadata) -> String {
        let profile = self.profile.to_string();
        let target_hint = self.target_hint.to_string();

        match profile.as_str() {
            BACKEND_PROFILE_DESKTOP => self.desktop_pipeline(metadata),
            BACKEND_PROFILE_MOBILE => PIPELINE_MOBILE_DIRECT.to_string(),
            BACKEND_PROFILE_VR_SAFE => PIPELINE_VR_SAFE_SPATIAL.to_string(),
            _ => self.resolve_automatic_pipeline(metadata, target_hint.as_str()),
        }
    }

    fn resolve_automatic_pipeline(
        &self,
        metadata: &ImportedSplatMetadata,
        target_hint: &str,
    ) -> String {
        match target_hint {
            BACKEND_PROFILE_VR_SAFE => PIPELINE_VR_SAFE_SPATIAL.to_string(),
            BACKEND_PROFILE_MOBILE => PIPELINE_MOBILE_DIRECT.to_string(),
            _ => self.desktop_pipeline(metadata),
        }
    }

    fn desktop_pipeline(&self, metadata: &ImportedSplatMetadata) -> String {
        if self.allow_compositor && metadata.point_count >= self.desktop_cluster_threshold {
            PIPELINE_DESKTOP_COMPOSITOR.to_string()
        } else if metadata.point_count >= self.desktop_cluster_threshold {
            PIPELINE_DESKTOP_CLUSTERED.to_string()
        } else {
            PIPELINE_DESKTOP_DIRECT.to_string()
        }
    }
}
