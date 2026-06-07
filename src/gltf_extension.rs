use godot::classes::{GltfDocumentExtension, GltfNode, GltfState, IGltfDocumentExtension};
use godot::global::Error;
use godot::prelude::*;

use crate::asset::GaussianSplatAsset;
use crate::import_state::{
    inspect_gsplat_nodes, BASE_EXTENSION, COMPRESSION_EXTENSION, GLTF_STATE_KEY, NODE_STATE_KEY,
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
        _state: Option<Gd<GltfState>>,
        _gltf_node: Option<Gd<GltfNode>>,
        _extensions: VarDictionary,
    ) -> Error {
        Error::OK
    }

    fn import_post_parse(&mut self, state: Option<Gd<GltfState>>) -> Error {
        let Some(mut state) = state else {
            return Error::ERR_INVALID_PARAMETER;
        };

        let json = state.get_json();
        let accessors = state.get_accessors();
        let nodes = state.get_nodes();

        let metadata_list = match inspect_gsplat_nodes(&json, &accessors) {
            Ok(metadata_list) => metadata_list,
            Err(message) => {
                godot_error!("{message}");
                return Error::ERR_INVALID_DATA;
            }
        };

        let mut summaries = PackedStringArray::new();
        let mut has_errors = false;

        for metadata in metadata_list {
            let Some(node_index) = metadata.node_index else {
                continue;
            };
            let Some(mut gltf_node) = nodes.get(node_index as usize) else {
                godot_error!("Gaussian splat metadata references missing GLTF node.");
                has_errors = true;
                continue;
            };

            let dictionary = metadata.to_dictionary();
            gltf_node.set_additional_data(NODE_STATE_KEY, &Variant::from(dictionary));
            summaries.push(metadata.summary().as_str());

            if !metadata.is_valid() {
                has_errors = true;
                for message in metadata.validation_errors.as_slice() {
                    godot_error!(
                        "Gaussian splat validation failed for node {}: {}",
                        node_index,
                        message
                    );
                }
            }
            for warning in metadata.validation_warnings.as_slice() {
                godot_warn!(
                    "Gaussian splat import warning for node {}: {}",
                    node_index,
                    warning
                );
            }
        }

        {
            let mut data = state
                .get_additional_data(GLTF_STATE_KEY)
                .try_to::<VarDictionary>()
                .unwrap_or_default();
            data.set("summaries", &Variant::from(summaries));
            state.set_additional_data(GLTF_STATE_KEY, &Variant::from(data));
        }

        if has_errors {
            Error::ERR_INVALID_DATA
        } else {
            Error::OK
        }
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
            asset.bind_mut().initialize_from_import(raw_metadata);
            node.bind_mut().bind_asset(Some(asset));
        }

        Some(node.upcast())
    }
}
