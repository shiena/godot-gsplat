use godot::classes::GltfAccessor;
use godot::prelude::*;

pub const BASE_EXTENSION: &str = "KHR_gaussian_splatting";
pub const COMPRESSION_EXTENSION: &str = "khr_gaussian_splatting_compression_spz";
pub const GLTF_STATE_KEY: &str = "godot_gsplat.import_state";
pub const NODE_STATE_KEY: &str = "godot_gsplat.node_state";
pub const PAYLOAD_LAYOUT_V1: &str = "gsplat.import.placeholder.v1";
pub const FALLBACK_NONE: &str = "none";
pub const FALLBACK_COLOR_POINTS: &str = "color_points";

const DEFAULT_PROJECTION: &str = "perspective";
const DEFAULT_SORTING_METHOD: &str = "cameraDistance";
const REQUIRED_ATTRIBUTES: [&str; 5] = [
    "POSITION",
    "ROTATION",
    "SCALE",
    "OPACITY",
    "SH_DEGREE_0_COEF_0",
];

#[derive(Clone, Debug, Default)]
pub struct ImportedSplatMetadata {
    pub source_extension: String,
    pub node_index: Option<i32>,
    pub mesh_index: Option<i32>,
    pub primitive_index: Option<i32>,
    pub kernel: Option<String>,
    pub color_space: Option<String>,
    pub projection: String,
    pub sorting_method: String,
    pub compression: Option<String>,
    pub point_count: i32,
    pub has_color_fallback: bool,
    pub fallback_mode: String,
    pub validation_errors: PackedStringArray,
    pub validation_warnings: PackedStringArray,
    pub raw_extensions: VarDictionary,
}

impl ImportedSplatMetadata {
    pub fn from_extensions(source_extension: &str, extensions: VarDictionary) -> Self {
        let mut metadata = Self {
            source_extension: source_extension.to_string(),
            projection: DEFAULT_PROJECTION.to_string(),
            sorting_method: DEFAULT_SORTING_METHOD.to_string(),
            fallback_mode: FALLBACK_NONE.to_string(),
            raw_extensions: extensions.clone(),
            ..Default::default()
        };

        if let Some(ext_variant) = extensions.get(BASE_EXTENSION) {
            if let Ok(ext_dict) = ext_variant.try_to::<VarDictionary>() {
                metadata.kernel = dict_string(&ext_dict, "kernel");
                metadata.color_space = dict_string(&ext_dict, "colorSpace");
                metadata.projection = dict_string(&ext_dict, "projection")
                    .unwrap_or_else(|| DEFAULT_PROJECTION.to_string());
                metadata.sorting_method = dict_string(&ext_dict, "sortingMethod")
                    .unwrap_or_else(|| DEFAULT_SORTING_METHOD.to_string());

                if let Some(nested_variant) = ext_dict.get("extensions") {
                    if let Ok(nested_dict) = nested_variant.try_to::<VarDictionary>() {
                        if nested_dict.contains_key(COMPRESSION_EXTENSION) {
                            metadata.compression = Some(COMPRESSION_EXTENSION.to_string());
                        }
                    }
                }
            }
        }

        metadata
    }

    pub fn is_valid(&self) -> bool {
        self.validation_errors.is_empty()
    }

    pub fn summary(&self) -> String {
        let kernel = self.kernel.as_deref().unwrap_or("unknown");
        let color_space = self.color_space.as_deref().unwrap_or("unknown");
        let compression = self.compression.as_deref().unwrap_or("none");
        let node_index = self
            .node_index
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string());
        let mesh_index = self
            .mesh_index
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string());
        let primitive_index = self
            .primitive_index
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string());
        let validity = if self.is_valid() { "valid" } else { "invalid" };

        format!(
            "extension={}; node={}; mesh={}; primitive={}; points={}; kernel={}; color_space={}; projection={}; sorting={}; compression={}; validity={}",
            self.source_extension,
            node_index,
            mesh_index,
            primitive_index,
            self.point_count,
            kernel,
            color_space,
            self.projection,
            self.sorting_method,
            compression,
            validity
        )
    }

    pub fn to_dictionary(&self) -> VarDictionary {
        let mut dict = VarDictionary::new();
        dict.set("source_extension", self.source_extension.as_str());
        dict.set("projection", self.projection.as_str());
        dict.set("sorting_method", self.sorting_method.as_str());
        dict.set("point_count", self.point_count as i64);
        dict.set("has_color_fallback", self.has_color_fallback);
        dict.set("fallback_mode", self.fallback_mode.as_str());

        if let Some(kernel) = &self.kernel {
            dict.set("kernel", kernel.as_str());
        }
        if let Some(color_space) = &self.color_space {
            dict.set("color_space", color_space.as_str());
        }
        if let Some(compression) = &self.compression {
            dict.set("compression", compression.as_str());
        }
        if let Some(node_index) = self.node_index {
            dict.set("node_index", node_index as i64);
        }
        if let Some(mesh_index) = self.mesh_index {
            dict.set("mesh_index", mesh_index as i64);
        }
        if let Some(primitive_index) = self.primitive_index {
            dict.set("primitive_index", primitive_index as i64);
        }

        dict.set(
            "raw_extensions",
            &Variant::from(self.raw_extensions.clone()),
        );
        dict.set(
            "validation_errors",
            &Variant::from(self.validation_errors.clone()),
        );
        dict.set(
            "validation_warnings",
            &Variant::from(self.validation_warnings.clone()),
        );
        dict
    }

    pub fn from_dictionary(dict: VarDictionary) -> Self {
        let source_extension =
            dict_string(&dict, "source_extension").unwrap_or_else(|| BASE_EXTENSION.to_string());
        let projection =
            dict_string(&dict, "projection").unwrap_or_else(|| DEFAULT_PROJECTION.to_string());
        let sorting_method = dict_string(&dict, "sorting_method")
            .unwrap_or_else(|| DEFAULT_SORTING_METHOD.to_string());
        let kernel = dict_string(&dict, "kernel");
        let color_space = dict_string(&dict, "color_space");
        let compression = dict_string(&dict, "compression");
        let node_index = dict_i32(&dict, "node_index");
        let mesh_index = dict_i32(&dict, "mesh_index");
        let primitive_index = dict_i32(&dict, "primitive_index");
        let point_count = dict_i32(&dict, "point_count").unwrap_or_default();
        let has_color_fallback = dict_bool(&dict, "has_color_fallback").unwrap_or(false);
        let fallback_mode =
            dict_string(&dict, "fallback_mode").unwrap_or_else(|| FALLBACK_NONE.to_string());
        let raw_extensions = dict
            .get("raw_extensions")
            .and_then(|variant| variant.try_to::<VarDictionary>().ok())
            .unwrap_or_default();
        let validation_errors = dict
            .get("validation_errors")
            .and_then(|variant| variant.try_to::<PackedStringArray>().ok())
            .unwrap_or_default();
        let validation_warnings = dict
            .get("validation_warnings")
            .and_then(|variant| variant.try_to::<PackedStringArray>().ok())
            .unwrap_or_default();

        Self {
            source_extension,
            node_index,
            mesh_index,
            primitive_index,
            kernel,
            color_space,
            projection,
            sorting_method,
            compression,
            point_count,
            has_color_fallback,
            fallback_mode,
            validation_errors,
            validation_warnings,
            raw_extensions,
        }
    }
}

pub fn inspect_gsplat_nodes(
    json: &VarDictionary,
    accessors: &Array<Gd<GltfAccessor>>,
) -> Result<Vec<ImportedSplatMetadata>, String> {
    let nodes = dict_array(json, "nodes").unwrap_or_default();
    let meshes = dict_array(json, "meshes").unwrap_or_default();
    let mut metadata_list = Vec::new();

    for node_index in 0..nodes.len() {
        let Some(node_dict) = nodes
            .get(node_index)
            .and_then(|value| value.try_to::<VarDictionary>().ok())
        else {
            continue;
        };

        let Some(mesh_index) = dict_i32(&node_dict, "mesh") else {
            continue;
        };

        let Some(mesh_dict) = meshes
            .get(mesh_index as usize)
            .and_then(|value| value.try_to::<VarDictionary>().ok())
        else {
            return Err(format!(
                "Node {} references missing mesh {}.",
                node_index, mesh_index
            ));
        };

        let Some(primitives) = dict_array(&mesh_dict, "primitives") else {
            continue;
        };

        let mut found_for_node = false;
        for primitive_index in 0..primitives.len() {
            let Some(primitive_dict) = primitives
                .get(primitive_index)
                .and_then(|value| value.try_to::<VarDictionary>().ok())
            else {
                continue;
            };

            let Some(extensions) = dict_dictionary(&primitive_dict, "extensions") else {
                continue;
            };
            if !extensions.contains_key(BASE_EXTENSION) {
                continue;
            }

            let mut metadata = ImportedSplatMetadata::from_extensions(BASE_EXTENSION, extensions);
            metadata.node_index = Some(node_index as i32);
            metadata.mesh_index = Some(mesh_index);
            metadata.primitive_index = Some(primitive_index as i32);
            validate_primitive(&primitive_dict, accessors, &mut metadata);
            metadata_list.push(metadata);
            found_for_node = true;
        }

        if found_for_node
            && metadata_list
                .iter()
                .filter(|item| item.node_index == Some(node_index as i32))
                .count()
                > 1
        {
            for metadata in metadata_list
                .iter_mut()
                .filter(|item| item.node_index == Some(node_index as i32))
            {
                push_message(
                    &mut metadata.validation_warnings,
                    "Multiple gaussian splat primitives were found for one node. The runtime currently generates one node per primitive match.",
                );
            }
        }
    }

    Ok(metadata_list)
}

fn validate_primitive(
    primitive_dict: &VarDictionary,
    accessors: &Array<Gd<GltfAccessor>>,
    metadata: &mut ImportedSplatMetadata,
) {
    let mode = dict_i32(primitive_dict, "mode").unwrap_or(4);
    if mode != 0 {
        push_message(
            &mut metadata.validation_errors,
            "Gaussian splat primitives must use POINTS mode.",
        );
    }

    let Some(attributes) = dict_dictionary(primitive_dict, "attributes") else {
        push_message(
            &mut metadata.validation_errors,
            "Gaussian splat primitive is missing the attributes dictionary.",
        );
        return;
    };

    for attribute in REQUIRED_ATTRIBUTES {
        if !attributes.contains_key(attribute) {
            push_message(
                &mut metadata.validation_errors,
                &format!("Missing required attribute {attribute}."),
            );
        }
    }

    metadata.has_color_fallback = attributes.contains_key("COLOR_0");
    metadata.fallback_mode = if metadata.has_color_fallback {
        FALLBACK_COLOR_POINTS.to_string()
    } else {
        FALLBACK_NONE.to_string()
    };

    let Some(position_accessor_index) = dict_i32(&attributes, "POSITION") else {
        return;
    };
    metadata.point_count = accessor_count(accessors, position_accessor_index).unwrap_or_default();

    let sh0_accessor_index = dict_i32(&attributes, "SH_DEGREE_0_COEF_0");
    validate_accessor_match(
        accessors,
        position_accessor_index,
        sh0_accessor_index,
        &mut metadata.validation_errors,
        "POSITION",
        "SH_DEGREE_0_COEF_0",
    );
    validate_accessor_match(
        accessors,
        position_accessor_index,
        dict_i32(&attributes, "ROTATION"),
        &mut metadata.validation_errors,
        "POSITION",
        "ROTATION",
    );
    validate_accessor_match(
        accessors,
        position_accessor_index,
        dict_i32(&attributes, "SCALE"),
        &mut metadata.validation_errors,
        "POSITION",
        "SCALE",
    );
    validate_accessor_match(
        accessors,
        position_accessor_index,
        dict_i32(&attributes, "OPACITY"),
        &mut metadata.validation_errors,
        "POSITION",
        "OPACITY",
    );
}

fn validate_accessor_match(
    accessors: &Array<Gd<GltfAccessor>>,
    reference_index: i32,
    candidate_index: Option<i32>,
    messages: &mut PackedStringArray,
    reference_name: &str,
    candidate_name: &str,
) {
    let Some(candidate_index) = candidate_index else {
        return;
    };
    let reference_count = accessor_count(accessors, reference_index);
    let candidate_count = accessor_count(accessors, candidate_index);
    if reference_count.is_some() && candidate_count.is_some() && reference_count != candidate_count
    {
        push_message(
            messages,
            &format!("Accessor count mismatch between {reference_name} and {candidate_name}."),
        );
    }
}

fn accessor_count(accessors: &Array<Gd<GltfAccessor>>, accessor_index: i32) -> Option<i32> {
    accessors
        .get(accessor_index as usize)
        .map(|accessor| accessor.get_count() as i32)
}

fn dict_array(dict: &VarDictionary, key: &str) -> Option<VarArray> {
    dict.get(key)
        .and_then(|value| value.try_to::<VarArray>().ok())
}

fn dict_dictionary(dict: &VarDictionary, key: &str) -> Option<VarDictionary> {
    dict.get(key)
        .and_then(|value| value.try_to::<VarDictionary>().ok())
}

fn dict_string(dict: &VarDictionary, key: &str) -> Option<String> {
    dict.get(key)
        .and_then(|value| value.try_to::<GString>().ok())
        .map(|value| value.to_string())
}

fn dict_i32(dict: &VarDictionary, key: &str) -> Option<i32> {
    dict.get(key)
        .and_then(|value| value.try_to::<i64>().ok())
        .and_then(|value| i32::try_from(value).ok())
}

fn dict_bool(dict: &VarDictionary, key: &str) -> Option<bool> {
    dict.get(key).and_then(|value| value.try_to::<bool>().ok())
}

fn push_message(messages: &mut PackedStringArray, message: &str) {
    messages.push(message);
}
