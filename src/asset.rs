use godot::classes::GltfState;
use godot::prelude::*;

use crate::import_state::{ImportedSplatMetadata, GLTF_STATE_KEY};

#[derive(GodotClass)]
#[class(init, base=Resource)]
pub struct GaussianSplatAsset {
    #[base]
    base: Base<Resource>,

    metadata: ImportedSplatMetadata,
    point_count: i32,
    payload: PackedByteArray,
    local_aabb: Aabb,
}

#[godot_api]
impl GaussianSplatAsset {
    #[func]
    pub fn clear(&mut self) {
        self.metadata = ImportedSplatMetadata::default();
        self.point_count = 0;
        self.payload.clear();
        self.local_aabb = Aabb::default();
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn apply_import_metadata(&mut self, metadata: VarDictionary) {
        self.metadata = ImportedSplatMetadata::from_dictionary(metadata);
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn export_import_metadata(&self) -> VarDictionary {
        self.metadata.to_dictionary()
    }

    #[func]
    pub fn get_metadata_summary(&self) -> GString {
        GString::from(self.metadata.summary().as_str())
    }

    #[func]
    pub fn set_point_count(&mut self, point_count: i32) {
        self.point_count = point_count.max(0);
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn get_point_count(&self) -> i32 {
        self.point_count
    }

    #[func]
    pub fn set_payload(&mut self, payload: PackedByteArray) {
        self.payload = payload;
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn get_payload(&self) -> PackedByteArray {
        self.payload.clone()
    }

    #[func]
    pub fn set_local_aabb(&mut self, aabb: Aabb) {
        self.local_aabb = aabb;
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn get_local_aabb(&self) -> Aabb {
        self.local_aabb
    }

    #[func]
    pub fn has_compression(&self) -> bool {
        self.metadata.compression.is_some()
    }

    #[func]
    pub fn get_source_extension(&self) -> GString {
        self.metadata.source_extension.as_str().into()
    }

    #[func]
    pub fn stash_on_state(&self, state: Option<Gd<GltfState>>) {
        if let Some(mut state) = state {
            let dict = self.metadata.to_dictionary();
            state.set_additional_data(GLTF_STATE_KEY, &Variant::from(dict));
        }
    }
}
