use godot::classes::GltfState;
use godot::prelude::*;

use crate::asset::GaussianSplatAsset;
use crate::import_state::{ImportedSplatMetadata, NODE_STATE_KEY};

#[derive(GodotClass)]
#[class(init, base=Node3D)]
pub struct GaussianSplatNode3D {
    #[base]
    base: Base<Node3D>,

    asset: Option<Gd<GaussianSplatAsset>>,
    metadata: ImportedSplatMetadata,
    is_bound: bool,
}

#[godot_api]
impl INode3D for GaussianSplatNode3D {
    fn ready(&mut self) {
        self.sync_node_name();
    }
}

#[godot_api]
impl GaussianSplatNode3D {
    #[func]
    pub fn bind_asset(&mut self, asset: Option<Gd<GaussianSplatAsset>>) {
        self.asset = asset;
        self.refresh_from_asset();
    }

    #[func]
    pub fn unbind_asset(&mut self) {
        self.asset = None;
        self.metadata = ImportedSplatMetadata::default();
        self.is_bound = false;
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
    pub fn stash_on_state(&self, state: Option<Gd<GltfState>>) {
        if let Some(mut state) = state {
            let dict = self.metadata.to_dictionary();
            state.set_additional_data(NODE_STATE_KEY, &Variant::from(dict));
        }
    }

    fn refresh_from_asset(&mut self) {
        if let Some(asset) = &self.asset {
            let asset = asset.clone();
            let asset_ref = asset.bind();
            self.metadata =
                ImportedSplatMetadata::from_dictionary(asset_ref.export_import_metadata());
            self.is_bound = true;
        } else {
            self.metadata = ImportedSplatMetadata::default();
            self.is_bound = false;
        }
        self.sync_node_name();
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
}
