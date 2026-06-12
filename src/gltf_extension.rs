use godot::classes::{GltfDocumentExtension, GltfNode, GltfState, IGltfDocumentExtension};
use godot::global::Error;
use godot::prelude::*;

use crate::asset::GaussianSplatAsset;
use crate::import_state::{
    ImportedSplatMetadata, BASE_EXTENSION, COMPRESSION_EXTENSION, GLTF_STATE_KEY, NODE_STATE_KEY,
};
use crate::node::GaussianSplatNode3D;

#[derive(GodotClass)]
#[class(tool, init, base=GltfDocumentExtension)]
pub struct GltfGsplatDocumentExtension {
    #[base]
    base: Base<GltfDocumentExtension>,
}

#[godot_api]
impl IGltfDocumentExtension for GltfGsplatDocumentExtension {
    fn import_preflight(
        &mut self,
        state: Option<Gd<GltfState>>,
        extensions: PackedStringArray,
    ) -> Error {
        if let Some(mut state) = state {
            let mut data = VarDictionary::new();
            let supported = Variant::from(self.get_supported_extensions());
            let imported = Variant::from(extensions.clone());
            data.set("supported", &supported);
            data.set("imported", &imported);
            state.set_additional_data(GLTF_STATE_KEY, &Variant::from(data));
        }
        Error::OK
    }

    fn get_supported_extensions(&mut self) -> PackedStringArray {
        [BASE_EXTENSION, COMPRESSION_EXTENSION]
            .into_iter()
            .map(GString::from)
            .collect()
    }

    fn parse_node_extensions(
        &mut self,
        state: Option<Gd<GltfState>>,
        gltf_node: Option<Gd<GltfNode>>,
        extensions: VarDictionary,
    ) -> Error {
        let Some(mut gltf_node) = gltf_node else {
            godot_warn!("Gaussian splat import skipped because GLTF node was missing.");
            return Error::ERR_UNAVAILABLE;
        };

        if !extensions.contains_key(BASE_EXTENSION) {
            return Error::OK;
        }

        let metadata = ImportedSplatMetadata::from_extensions(BASE_EXTENSION, extensions.clone());
        let dictionary = metadata.to_dictionary();
        gltf_node.set_additional_data(NODE_STATE_KEY, &Variant::from(dictionary));

        if let Some(mut state) = state {
            let mut data = state
                .get_additional_data(GLTF_STATE_KEY)
                .try_to::<VarDictionary>()
                .unwrap_or_default();
            data.set(NODE_STATE_KEY, metadata.summary());
            state.set_additional_data(GLTF_STATE_KEY, &Variant::from(data));
        }

        Error::OK
    }

    fn generate_scene_node(
        &mut self,
        _state: Option<Gd<GltfState>>,
        gltf_node: Option<Gd<GltfNode>>,
        _scene_parent: Option<Gd<Node>>,
    ) -> Option<Gd<Node3D>> {
        let gltf_node = gltf_node?;
        let raw_metadata = gltf_node
            .get_additional_data(NODE_STATE_KEY)
            .try_to::<VarDictionary>()
            .ok();

        let mut node = GaussianSplatNode3D::new_alloc();

        if let Some(raw_metadata) = raw_metadata {
            node.bind_mut().set_import_metadata(raw_metadata.clone());

            let mut asset = GaussianSplatAsset::new_gd();
            asset.bind_mut().apply_import_metadata(raw_metadata);
            node.bind_mut().bind_asset(Some(asset));
        }

        Some(node.upcast())
    }
}
