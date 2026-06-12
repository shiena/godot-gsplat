use godot::classes::{
    ConfigFile, GltfDocumentExtension, GltfNode, GltfState, IGltfDocumentExtension,
};
use godot::global::Error;
use godot::prelude::*;

use crate::asset::GaussianSplatAsset;
use crate::import_state::{
    decode_splat_payload, inspect_gsplat_nodes, BASE_EXTENSION, COMPRESSION_EXTENSION,
    GLTF_STATE_KEY, NODE_STATE_KEY,
};
use crate::node::GaussianSplatNode3D;

const OPTION_PREVIEW_MAX_SPLATS: &str = "gsplat/preview_max_splats";
const OPTION_PREVIEW_MAX_SPLAT_RADIUS: &str = "gsplat/preview_max_splat_radius";
const OPTION_PREVIEW_SCALE_MULTIPLIER: &str = "gsplat/preview_scale_multiplier";
const INTERNAL_OPTION_PREVIEW_MAX_SPLATS: &str = "gsplat_preview/preview_max_splats";
const INTERNAL_OPTION_PREVIEW_MAX_SPLAT_RADIUS: &str = "gsplat_preview/preview_max_splat_radius";
const INTERNAL_OPTION_PREVIEW_SCALE_MULTIPLIER: &str = "gsplat_preview/preview_scale_multiplier";

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
        let mut raw_metadata = gltf_node
            .get_additional_data(NODE_STATE_KEY)
            .try_to::<VarDictionary>()
            .ok();
        if raw_metadata.is_none() {
            raw_metadata = state
                .as_ref()
                .and_then(|state| metadata_from_state(state, gltf_node.get_mesh()));
        }
        let preview_options = state
            .as_ref()
            .and_then(preview_options_from_saved_import_file);

        let mut node = GaussianSplatNode3D::new_alloc();
        node.bind_mut().set_imported_transform(imported_transform);

        if let Some(raw_metadata) = raw_metadata {
            node.bind_mut().set_import_metadata(raw_metadata.clone());

            let mut asset = GaussianSplatAsset::new_gd();
            asset.bind_mut().apply_import_metadata(raw_metadata.clone());

            let metadata = asset.bind().export_import_metadata();
            let metadata = crate::import_state::ImportedSplatMetadata::from_dictionary(metadata);

            if let Some(state) = state.as_ref() {
                match decode_splat_payload(state, &metadata) {
                    Ok(decoded) => {
                        asset.bind_mut().apply_decoded_data(decoded);
                    }
                    Err(message) => {
                        godot_error!("{message}");
                        asset.bind_mut().initialize_from_import(raw_metadata);
                    }
                }
            } else {
                asset.bind_mut().initialize_from_import(raw_metadata);
            }
            node.bind_mut().bind_asset(Some(asset));
            if let Some(preview_options) = preview_options {
                apply_preview_options_to_node(&mut node, &preview_options);
            }
        }

        Some(node.upcast())
    }
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

#[derive(Clone, Copy, Debug)]
struct SavedPreviewImportOptions {
    max_splats: Option<i32>,
    max_splat_radius: Option<f32>,
    scale_multiplier: Option<f32>,
}

fn preview_options_from_saved_import_file(
    state: &Gd<GltfState>,
) -> Option<SavedPreviewImportOptions> {
    let filename = state.get_filename().to_string();
    if filename.is_empty() {
        return None;
    }

    let mut config = ConfigFile::new_gd();
    let mut loaded = false;
    for import_path in import_path_candidates(state, filename.as_str()) {
        if config.load(import_path.as_str()) == Error::OK {
            loaded = true;
            break;
        }
    }
    if !loaded {
        return None;
    }

    let mut options = SavedPreviewImportOptions {
        max_splats: config_i32(&config, OPTION_PREVIEW_MAX_SPLATS),
        max_splat_radius: config_f32(&config, OPTION_PREVIEW_MAX_SPLAT_RADIUS),
        scale_multiplier: config_f32(&config, OPTION_PREVIEW_SCALE_MULTIPLIER),
    };

    if let Some(subresources) = config_value(&config, "_subresources") {
        apply_saved_options_from_subresources(&mut options, &subresources);
    }

    if options.max_splats.is_some()
        || options.max_splat_radius.is_some()
        || options.scale_multiplier.is_some()
    {
        Some(options)
    } else {
        None
    }
}

fn import_path_candidates(state: &Gd<GltfState>, filename: &str) -> Vec<String> {
    let base_path = state.get_base_path().to_string();
    let mut paths = Vec::new();

    if filename.ends_with(".gltf") || filename.ends_with(".glb") {
        paths.push(join_import_path(base_path.as_str(), filename, ""));
    } else {
        paths.push(join_import_path(base_path.as_str(), filename, ".gltf"));
        paths.push(join_import_path(base_path.as_str(), filename, ".glb"));
    }
    paths.push(format!("{filename}.import"));

    paths
}

fn join_import_path(base_path: &str, filename: &str, extension: &str) -> String {
    let import_file = format!("{filename}{extension}.import");
    if base_path.is_empty() {
        import_file
    } else if base_path.ends_with('/') {
        format!("{base_path}{import_file}")
    } else {
        format!("{base_path}/{import_file}")
    }
}

fn config_value(config: &ConfigFile, key: &str) -> Option<Variant> {
    if config.has_section_key("params", key) {
        Some(config.get_value("params", key))
    } else {
        None
    }
}

fn config_i32(config: &ConfigFile, key: &str) -> Option<i32> {
    config_value(config, key).and_then(|value| variant_to_i32(&value))
}

fn config_f32(config: &ConfigFile, key: &str) -> Option<f32> {
    config_value(config, key).and_then(|value| variant_to_f32(&value))
}

fn apply_saved_options_from_subresources(
    options: &mut SavedPreviewImportOptions,
    subresources: &Variant,
) {
    if let Some(max_splats) = find_i32_option(subresources, INTERNAL_OPTION_PREVIEW_MAX_SPLATS)
        .or_else(|| find_i32_option(subresources, OPTION_PREVIEW_MAX_SPLATS))
    {
        options.max_splats = Some(max_splats);
    }
    if let Some(max_splat_radius) =
        find_f32_option(subresources, INTERNAL_OPTION_PREVIEW_MAX_SPLAT_RADIUS)
            .or_else(|| find_f32_option(subresources, OPTION_PREVIEW_MAX_SPLAT_RADIUS))
    {
        options.max_splat_radius = Some(max_splat_radius);
    }
    if let Some(scale_multiplier) =
        find_f32_option(subresources, INTERNAL_OPTION_PREVIEW_SCALE_MULTIPLIER)
            .or_else(|| find_f32_option(subresources, OPTION_PREVIEW_SCALE_MULTIPLIER))
    {
        options.scale_multiplier = Some(scale_multiplier);
    }
}

fn find_i32_option(value: &Variant, name: &str) -> Option<i32> {
    find_option(value, name).and_then(|value| variant_to_i32(&value))
}

fn find_f32_option(value: &Variant, name: &str) -> Option<f32> {
    find_option(value, name).and_then(|value| variant_to_f32(&value))
}

fn find_option(value: &Variant, name: &str) -> Option<Variant> {
    if let Ok(dictionary) = value.try_to::<VarDictionary>() {
        if let Some(option_value) = dictionary.get(name) {
            return Some(option_value);
        }

        for nested_value in dictionary.values_array().iter_shared() {
            if let Some(option_value) = find_option(&nested_value, name) {
                return Some(option_value);
            }
        }
    }

    if let Ok(array) = value.try_to::<VarArray>() {
        for nested_value in array.iter_shared() {
            if let Some(option_value) = find_option(&nested_value, name) {
                return Some(option_value);
            }
        }
    }

    None
}

fn variant_to_i32(value: &Variant) -> Option<i32> {
    value.try_to::<i32>().ok().or_else(|| {
        value
            .try_to::<i64>()
            .ok()
            .and_then(|value| i32::try_from(value).ok())
    })
}

fn variant_to_f32(value: &Variant) -> Option<f32> {
    value
        .try_to::<f32>()
        .ok()
        .or_else(|| value.try_to::<f64>().ok().map(|value| value as f32))
}

fn apply_preview_options_to_node(
    node: &mut Gd<GaussianSplatNode3D>,
    options: &SavedPreviewImportOptions,
) {
    if let Some(max_splats) = options.max_splats {
        node.bind_mut().set_preview_max_splats(max_splats);
    }
    if let Some(max_splat_radius) = options.max_splat_radius {
        node.bind_mut()
            .set_preview_max_splat_radius(max_splat_radius);
    }
    if let Some(scale_multiplier) = options.scale_multiplier {
        node.bind_mut()
            .set_preview_scale_multiplier(scale_multiplier);
    }
}
