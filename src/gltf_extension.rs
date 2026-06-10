use godot::classes::{GltfDocumentExtension, GltfNode, GltfState, IGltfDocumentExtension, Node};
use godot::global::Error;
use godot::prelude::*;

use crate::asset::GaussianSplatAsset;
use crate::import_options::PreviewImportOptions;
use crate::import_state::{
    decode_splat_payload, inspect_gsplat_nodes, BASE_EXTENSION, COMPRESSION_EXTENSION,
    GLTF_STATE_KEY, NODE_ASSET_KEY, NODE_STATE_KEY,
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
        let mut entries = VarArray::new();
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
            entries.push(&Variant::from(dictionary.clone()));
            gltf_node.set_additional_data(NODE_STATE_KEY, &Variant::from(dictionary.clone()));
            // Decode the splat payload once here and stash the prepared asset on
            // the GLTF node; generate_scene_node() only binds it. This keeps node
            // generation cheap and the extension free of work that depends on
            // anything but the GLTFState.
            let asset = build_node_asset(Some(&state), &metadata, &dictionary);
            gltf_node.set_additional_data(NODE_ASSET_KEY, &Variant::from(asset));
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
            data.set("entries", &Variant::from(entries));
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
        state: Option<Gd<GltfState>>,
        gltf_node: Option<Gd<GltfNode>>,
        _scene_parent: Option<Gd<Node>>,
    ) -> Option<Gd<Node3D>> {
        let gltf_node = gltf_node?;
        let imported_transform = gltf_node.get_xform();

        // Common path: bind the asset prepared in import_post_parse. Fallback:
        // when the node's additional data did not survive, rebuild it from the
        // metadata stashed on the GLTFState (matched by mesh index).
        let mut asset = gltf_node
            .get_additional_data(NODE_ASSET_KEY)
            .try_to::<Gd<GaussianSplatAsset>>()
            .ok();
        if asset.is_none() {
            let raw_metadata = gltf_node
                .get_additional_data(NODE_STATE_KEY)
                .try_to::<VarDictionary>()
                .ok()
                .or_else(|| {
                    state
                        .as_ref()
                        .and_then(|state| metadata_from_state(state, gltf_node.get_mesh()))
                });
            if let Some(raw_metadata) = raw_metadata {
                let metadata = crate::import_state::ImportedSplatMetadata::from_dictionary(
                    raw_metadata.clone(),
                );
                asset = Some(build_node_asset(state.as_ref(), &metadata, &raw_metadata));
            }
        }

        let mut node = GaussianSplatNode3D::new_alloc();
        node.bind_mut().set_imported_transform(imported_transform);

        if let Some(asset) = asset {
            node.bind_mut().bind_asset(Some(asset));
            let preview_options = state
                .as_ref()
                .and_then(PreviewImportOptions::from_saved_import_file);
            if let Some(preview_options) = preview_options {
                let mut as_node = node.clone().upcast::<Node>();
                preview_options.apply_to_node(&mut as_node);
            }
        }

        Some(node.upcast())
    }
}

// Build a GaussianSplatAsset for one splat node: apply the import metadata and
// decode the payload from the GLTF state (falling back to a payload-less
// placeholder when decoding is impossible).
fn build_node_asset(
    state: Option<&Gd<GltfState>>,
    metadata: &crate::import_state::ImportedSplatMetadata,
    raw_metadata: &VarDictionary,
) -> Gd<GaussianSplatAsset> {
    let mut asset = GaussianSplatAsset::new_gd();
    asset.bind_mut().apply_import_metadata(raw_metadata.clone());
    if let Some(state) = state {
        match decode_splat_payload(state, metadata) {
            Ok(decoded) => {
                asset.bind_mut().apply_decoded_data(decoded);
            }
            Err(message) => {
                godot_error!("{message}");
                asset
                    .bind_mut()
                    .initialize_from_import(raw_metadata.clone());
            }
        }
    } else {
        asset
            .bind_mut()
            .initialize_from_import(raw_metadata.clone());
    }
    asset
}

fn metadata_from_state(state: &Gd<GltfState>, mesh_index: i32) -> Option<VarDictionary> {
    let data = state
        .get_additional_data(GLTF_STATE_KEY)
        .try_to::<VarDictionary>()
        .ok()?;
    let entries = data.get("entries")?.try_to::<VarArray>().ok()?;

    for entry in entries.iter_shared() {
        let dictionary = entry.try_to::<VarDictionary>().ok()?;
        let entry_mesh_index = dictionary
            .get("mesh_index")
            .and_then(|value| value.try_to::<i64>().ok())
            .and_then(|value| i32::try_from(value).ok())?;
        if entry_mesh_index == mesh_index {
            return Some(dictionary);
        }
    }

    None
}
