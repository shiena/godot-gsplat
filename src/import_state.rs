use godot::classes::gltf_accessor::{GltfAccessorType, GltfComponentType};
use godot::classes::{GltfAccessor, GltfBufferView, GltfState};
use godot::prelude::*;

pub const BASE_EXTENSION: &str = "KHR_gaussian_splatting";
pub const COMPRESSION_EXTENSION: &str = "khr_gaussian_splatting_compression_spz";
pub const GLTF_STATE_KEY: &str = "godot_gsplat.import_state";
pub const NODE_STATE_KEY: &str = "godot_gsplat.node_state";
pub const PAYLOAD_LAYOUT_V1: &str = "gsplat.import.placeholder.v1";
pub const PAYLOAD_LAYOUT_FLOAT32_V1: &str = "gsplat.interleaved.float32.v1";
pub const FALLBACK_NONE: &str = "none";
pub const FALLBACK_COLOR_POINTS: &str = "color_points";

const DEFAULT_PROJECTION: &str = "perspective";
const DEFAULT_SORTING_METHOD: &str = "cameraDistance";
const POSITION_ATTRIBUTE: &str = "POSITION";
const ROTATION_ATTRIBUTE: &str = "KHR_gaussian_splatting:ROTATION";
const SCALE_ATTRIBUTE: &str = "KHR_gaussian_splatting:SCALE";
const OPACITY_ATTRIBUTE: &str = "KHR_gaussian_splatting:OPACITY";
const SH0_ATTRIBUTE: &str = "KHR_gaussian_splatting:SH_DEGREE_0_COEF_0";
const COLOR_ATTRIBUTE: &str = "COLOR_0";
const REQUIRED_ATTRIBUTES: [&str; 5] = [
    POSITION_ATTRIBUTE,
    ROTATION_ATTRIBUTE,
    SCALE_ATTRIBUTE,
    OPACITY_ATTRIBUTE,
    SH0_ATTRIBUTE,
];
pub const POINT_STRIDE_FLOATS: usize = 18;

#[derive(Clone, Debug)]
pub struct DecodedSplatData {
    pub point_count: i32,
    pub payload_layout: GString,
    pub payload: PackedByteArray,
    pub local_aabb: Aabb,
}

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

        if metadata.kernel.is_none() {
            push_message(
                &mut metadata.validation_errors,
                "Gaussian splat extension is missing required property kernel.",
            );
        }
        if metadata.color_space.is_none() {
            push_message(
                &mut metadata.validation_errors,
                "Gaussian splat extension is missing required property colorSpace.",
            );
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

    let Some(position_accessor_index) = dict_i32(&attributes, POSITION_ATTRIBUTE) else {
        return;
    };
    metadata.point_count = accessor_count(accessors, position_accessor_index).unwrap_or_default();

    let sh0_accessor_index = dict_i32(&attributes, SH0_ATTRIBUTE);
    validate_accessor_match(
        accessors,
        position_accessor_index,
        sh0_accessor_index,
        &mut metadata.validation_errors,
        POSITION_ATTRIBUTE,
        SH0_ATTRIBUTE,
    );
    validate_accessor_match(
        accessors,
        position_accessor_index,
        dict_i32(&attributes, ROTATION_ATTRIBUTE),
        &mut metadata.validation_errors,
        POSITION_ATTRIBUTE,
        ROTATION_ATTRIBUTE,
    );
    validate_accessor_match(
        accessors,
        position_accessor_index,
        dict_i32(&attributes, SCALE_ATTRIBUTE),
        &mut metadata.validation_errors,
        POSITION_ATTRIBUTE,
        SCALE_ATTRIBUTE,
    );
    validate_accessor_match(
        accessors,
        position_accessor_index,
        dict_i32(&attributes, OPACITY_ATTRIBUTE),
        &mut metadata.validation_errors,
        POSITION_ATTRIBUTE,
        OPACITY_ATTRIBUTE,
    );
}

pub fn decode_splat_payload(
    state: &Gd<GltfState>,
    metadata: &ImportedSplatMetadata,
) -> Result<DecodedSplatData, String> {
    let node_index = metadata
        .node_index
        .ok_or_else(|| "Gaussian splat metadata is missing node index.".to_string())?;
    let mesh_index = metadata
        .mesh_index
        .ok_or_else(|| format!("Gaussian splat node {node_index} is missing mesh index."))?;
    let primitive_index = metadata
        .primitive_index
        .ok_or_else(|| format!("Gaussian splat node {node_index} is missing primitive index."))?;

    let json = state.get_json();
    let primitive = primitive_dict_for_indices(&json, node_index, mesh_index, primitive_index)?;
    let attributes = dict_dictionary(&primitive, "attributes")
        .ok_or_else(|| "Gaussian splat primitive is missing attributes dictionary.".to_string())?;
    let accessors = state.get_accessors();
    let buffer_views = state.get_buffer_views();

    let positions = decode_required_vec_accessor(
        state,
        &accessors,
        &buffer_views,
        &attributes,
        POSITION_ATTRIBUTE,
        GltfAccessorType::VEC3,
    )?;
    let rotations = decode_required_vec_accessor(
        state,
        &accessors,
        &buffer_views,
        &attributes,
        ROTATION_ATTRIBUTE,
        GltfAccessorType::VEC4,
    )?;
    let scales = decode_required_vec_accessor(
        state,
        &accessors,
        &buffer_views,
        &attributes,
        SCALE_ATTRIBUTE,
        GltfAccessorType::VEC3,
    )?;
    let opacities = decode_required_vec_accessor(
        state,
        &accessors,
        &buffer_views,
        &attributes,
        OPACITY_ATTRIBUTE,
        GltfAccessorType::SCALAR,
    )?;
    let sh0 = decode_required_vec_accessor(
        state,
        &accessors,
        &buffer_views,
        &attributes,
        SH0_ATTRIBUTE,
        GltfAccessorType::VEC3,
    )?;
    let colors = decode_optional_color_accessor(state, &accessors, &buffer_views, &attributes)?;

    let point_count = positions.len() / 3;
    if point_count == 0 {
        return Err("Gaussian splat primitive contains zero points.".to_string());
    }

    if rotations.len() / 4 != point_count
        || scales.len() / 3 != point_count
        || opacities.len() != point_count
        || sh0.len() / 3 != point_count
        || colors.len() / 4 != point_count
    {
        return Err("Gaussian splat accessor lengths do not match POSITION count.".to_string());
    }

    let mut payload_floats = Vec::with_capacity(point_count * POINT_STRIDE_FLOATS);
    let mut min = Vector3::new(f32::INFINITY, f32::INFINITY, f32::INFINITY);
    let mut max = Vector3::new(f32::NEG_INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY);

    for (point_index, opacity) in opacities.iter().enumerate().take(point_count) {
        let position_offset = point_index * 3;
        let rotation_offset = point_index * 4;
        let color_offset = point_index * 4;

        let position = Vector3::new(
            positions[position_offset],
            positions[position_offset + 1],
            positions[position_offset + 2],
        );
        min.x = min.x.min(position.x);
        min.y = min.y.min(position.y);
        min.z = min.z.min(position.z);
        max.x = max.x.max(position.x);
        max.y = max.y.max(position.y);
        max.z = max.z.max(position.z);

        payload_floats.extend_from_slice(&[
            position.x,
            position.y,
            position.z,
            rotations[rotation_offset],
            rotations[rotation_offset + 1],
            rotations[rotation_offset + 2],
            rotations[rotation_offset + 3],
            scales[position_offset],
            scales[position_offset + 1],
            scales[position_offset + 2],
            *opacity,
            sh0[position_offset],
            sh0[position_offset + 1],
            sh0[position_offset + 2],
            colors[color_offset],
            colors[color_offset + 1],
            colors[color_offset + 2],
            colors[color_offset + 3],
        ]);
    }

    let payload = PackedFloat32Array::from(payload_floats).to_byte_array();
    let size = max - min;
    let local_aabb = Aabb::new(min, size);

    Ok(DecodedSplatData {
        point_count: point_count as i32,
        payload_layout: GString::from(PAYLOAD_LAYOUT_FLOAT32_V1),
        payload,
        local_aabb,
    })
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

fn primitive_dict_for_indices(
    json: &VarDictionary,
    node_index: i32,
    mesh_index: i32,
    primitive_index: i32,
) -> Result<VarDictionary, String> {
    let nodes = dict_array(json, "nodes").unwrap_or_default();
    let meshes = dict_array(json, "meshes").unwrap_or_default();

    let node = nodes
        .get(node_index as usize)
        .and_then(|value| value.try_to::<VarDictionary>().ok())
        .ok_or_else(|| format!("Gaussian splat node {node_index} is missing from glTF JSON."))?;
    let node_mesh_index = dict_i32(&node, "mesh")
        .ok_or_else(|| format!("Gaussian splat node {node_index} does not reference a mesh."))?;
    if node_mesh_index != mesh_index {
        return Err(format!(
            "Gaussian splat node {node_index} expected mesh {mesh_index}, found {node_mesh_index}."
        ));
    }

    let mesh = meshes
        .get(mesh_index as usize)
        .and_then(|value| value.try_to::<VarDictionary>().ok())
        .ok_or_else(|| format!("Gaussian splat mesh {mesh_index} is missing from glTF JSON."))?;
    let primitives = dict_array(&mesh, "primitives")
        .ok_or_else(|| format!("Gaussian splat mesh {mesh_index} is missing primitives."))?;
    primitives
        .get(primitive_index as usize)
        .and_then(|value| value.try_to::<VarDictionary>().ok())
        .ok_or_else(|| {
            format!("Gaussian splat primitive {primitive_index} is missing from mesh {mesh_index}.")
        })
}

fn decode_required_vec_accessor(
    state: &Gd<GltfState>,
    accessors: &Array<Gd<GltfAccessor>>,
    buffer_views: &Array<Gd<GltfBufferView>>,
    attributes: &VarDictionary,
    attribute_name: &str,
    expected_type: GltfAccessorType,
) -> Result<Vec<f32>, String> {
    let accessor_index = dict_i32(attributes, attribute_name)
        .ok_or_else(|| format!("Missing required attribute {attribute_name}."))?;
    decode_float_accessor(
        state,
        accessors,
        buffer_views,
        accessor_index,
        attribute_name,
        &[expected_type],
    )
}

fn decode_optional_color_accessor(
    state: &Gd<GltfState>,
    accessors: &Array<Gd<GltfAccessor>>,
    buffer_views: &Array<Gd<GltfBufferView>>,
    attributes: &VarDictionary,
) -> Result<Vec<f32>, String> {
    let Some(accessor_index) = dict_i32(attributes, COLOR_ATTRIBUTE) else {
        let point_count = dict_i32(attributes, POSITION_ATTRIBUTE)
            .and_then(|index| accessor_count(accessors, index))
            .unwrap_or_default()
            .max(0) as usize;
        let mut colors = Vec::with_capacity(point_count * 4);
        for _ in 0..point_count {
            colors.extend_from_slice(&[1.0, 1.0, 1.0, 1.0]);
        }
        return Ok(colors);
    };

    let values = decode_float_accessor(
        state,
        accessors,
        buffer_views,
        accessor_index,
        COLOR_ATTRIBUTE,
        &[GltfAccessorType::VEC3, GltfAccessorType::VEC4],
    )?;
    let accessor = accessors
        .get(accessor_index as usize)
        .ok_or_else(|| format!("COLOR_0 accessor {accessor_index} is missing."))?;
    let component_count = accessor_component_count(accessor.get_accessor_type(), COLOR_ATTRIBUTE)?;

    if component_count == 4 {
        return Ok(values);
    }

    let point_count = accessor.get_count() as usize;
    let mut colors = Vec::with_capacity(point_count * 4);
    for point_index in 0..point_count {
        let offset = point_index * 3;
        colors.extend_from_slice(&[values[offset], values[offset + 1], values[offset + 2], 1.0]);
    }
    Ok(colors)
}

fn decode_float_accessor(
    state: &Gd<GltfState>,
    accessors: &Array<Gd<GltfAccessor>>,
    buffer_views: &Array<Gd<GltfBufferView>>,
    accessor_index: i32,
    attribute_name: &str,
    expected_types: &[GltfAccessorType],
) -> Result<Vec<f32>, String> {
    let accessor = accessors.get(accessor_index as usize).ok_or_else(|| {
        format!("Attribute {attribute_name} references missing accessor {accessor_index}.")
    })?;

    if accessor.get_sparse_count() != 0 {
        return Err(format!(
            "Attribute {attribute_name} uses sparse accessor data, which is not supported yet."
        ));
    }

    let accessor_type = accessor.get_accessor_type();
    if !expected_types.contains(&accessor_type) {
        return Err(format!(
            "Attribute {attribute_name} uses unexpected accessor type {:?}.",
            accessor_type
        ));
    }

    if accessor.get_component_type() != GltfComponentType::SINGLE_FLOAT {
        return Err(format!(
            "Attribute {attribute_name} uses unsupported component type {:?}. Only float accessors are supported right now.",
            accessor.get_component_type()
        ));
    }

    if accessor.get_normalized() {
        return Err(format!(
            "Attribute {attribute_name} is normalized. Normalized accessors are not supported yet."
        ));
    }

    let buffer_view_index = accessor.get_buffer_view();
    if buffer_view_index < 0 {
        return Err(format!(
            "Attribute {attribute_name} accessor {accessor_index} has no buffer view."
        ));
    }

    let buffer_view = buffer_views
        .get(buffer_view_index as usize)
        .ok_or_else(|| {
            format!(
                "Attribute {attribute_name} references missing buffer view {buffer_view_index}."
            )
        })?;
    let raw = buffer_view.load_buffer_view_data(state);
    let bytes = raw.as_slice();
    let component_count = accessor_component_count(accessor_type, attribute_name)?;
    let value_count = accessor.get_count().max(0) as usize;
    let element_size = component_count * std::mem::size_of::<f32>();
    let byte_stride = match buffer_view.get_byte_stride() {
        value if value <= 0 => element_size,
        value => usize::try_from(value)
            .map_err(|_| format!("Attribute {attribute_name} has invalid byte stride {value}."))?,
    };
    if byte_stride < element_size {
        return Err(format!(
            "Attribute {attribute_name} byte stride {byte_stride} is smaller than element size {element_size}."
        ));
    }

    let base_offset = usize::try_from(accessor.get_byte_offset()).map_err(|_| {
        format!(
            "Attribute {attribute_name} has invalid byte offset {}.",
            accessor.get_byte_offset()
        )
    })?;

    let required_bytes = base_offset
        .checked_add(value_count.saturating_sub(1) * byte_stride)
        .and_then(|value| value.checked_add(element_size))
        .ok_or_else(|| format!("Attribute {attribute_name} byte range overflowed."))?;
    if required_bytes > bytes.len() {
        return Err(format!(
            "Attribute {attribute_name} reads past the end of its buffer view."
        ));
    }

    let mut values = Vec::with_capacity(value_count * component_count);
    for value_index in 0..value_count {
        let element_offset = base_offset + value_index * byte_stride;
        for component_index in 0..component_count {
            let start = element_offset + component_index * std::mem::size_of::<f32>();
            let end = start + std::mem::size_of::<f32>();
            let component_bytes: [u8; 4] = bytes[start..end]
                .try_into()
                .map_err(|_| format!("Attribute {attribute_name} failed to decode float bytes."))?;
            values.push(f32::from_le_bytes(component_bytes));
        }
    }

    Ok(values)
}

fn accessor_component_count(
    accessor_type: GltfAccessorType,
    attribute_name: &str,
) -> Result<usize, String> {
    let component_count = match accessor_type {
        GltfAccessorType::SCALAR => 1,
        GltfAccessorType::VEC2 => 2,
        GltfAccessorType::VEC3 => 3,
        GltfAccessorType::VEC4 => 4,
        GltfAccessorType::MAT2 => 4,
        GltfAccessorType::MAT3 => 9,
        GltfAccessorType::MAT4 => 16,
        _ => {
            return Err(format!(
                "Attribute {attribute_name} uses unsupported accessor type {:?}.",
                accessor_type
            ));
        }
    };
    Ok(component_count)
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
    dict.get(key).and_then(variant_to_i32)
}

fn dict_bool(dict: &VarDictionary, key: &str) -> Option<bool> {
    dict.get(key).and_then(|value| value.try_to::<bool>().ok())
}

fn push_message(messages: &mut PackedStringArray, message: &str) {
    messages.push(message);
}

fn variant_to_i32(variant: Variant) -> Option<i32> {
    if let Ok(value) = variant.try_to::<i64>() {
        return i32::try_from(value).ok();
    }
    if let Ok(value) = variant.try_to::<f64>() {
        if value.fract() == 0.0 && value >= i32::MIN as f64 && value <= i32::MAX as f64 {
            return Some(value as i32);
        }
    }
    None
}
