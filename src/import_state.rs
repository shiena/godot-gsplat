use godot::prelude::*;

pub const BASE_EXTENSION: &str = "KHR_gaussian_splatting";
pub const COMPRESSION_EXTENSION: &str = "khr_gaussian_splatting_compression_spz";
pub const GLTF_STATE_KEY: &str = "godot_gsplat.import_state";
pub const NODE_STATE_KEY: &str = "godot_gsplat.node_state";

#[derive(Clone, Debug, Default)]
pub struct ImportedSplatMetadata {
    pub source_extension: String,
    pub kernel: Option<String>,
    pub color_space: Option<String>,
    pub projection: String,
    pub sorting_method: String,
    pub compression: Option<String>,
    pub raw_extensions: VarDictionary,
}

impl ImportedSplatMetadata {
    pub fn from_extensions(source_extension: &str, extensions: VarDictionary) -> Self {
        let mut metadata = Self {
            source_extension: source_extension.to_string(),
            projection: "perspective".to_string(),
            sorting_method: "cameraDistance".to_string(),
            raw_extensions: extensions.clone(),
            ..Default::default()
        };

        if let Some(ext_variant) = extensions.get(BASE_EXTENSION) {
            if let Ok(ext_dict) = ext_variant.try_to::<VarDictionary>() {
                metadata.kernel = dict_string(&ext_dict, "kernel");
                metadata.color_space = dict_string(&ext_dict, "colorSpace");
                metadata.projection = dict_string(&ext_dict, "projection")
                    .unwrap_or_else(|| "perspective".to_string());
                metadata.sorting_method = dict_string(&ext_dict, "sortingMethod")
                    .unwrap_or_else(|| "cameraDistance".to_string());

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

    pub fn summary(&self) -> String {
        let kernel = self.kernel.as_deref().unwrap_or("unknown");
        let color_space = self.color_space.as_deref().unwrap_or("unknown");
        let compression = self.compression.as_deref().unwrap_or("none");

        format!(
            "extension={}; kernel={}; color_space={}; projection={}; sorting={}; compression={}",
            self.source_extension,
            kernel,
            color_space,
            self.projection,
            self.sorting_method,
            compression
        )
    }

    pub fn to_dictionary(&self) -> VarDictionary {
        let mut dict = VarDictionary::new();
        dict.set("source_extension", self.source_extension.as_str());
        dict.set("projection", self.projection.as_str());
        dict.set("sorting_method", self.sorting_method.as_str());

        if let Some(kernel) = &self.kernel {
            dict.set("kernel", kernel.as_str());
        }
        if let Some(color_space) = &self.color_space {
            dict.set("color_space", color_space.as_str());
        }
        if let Some(compression) = &self.compression {
            dict.set("compression", compression.as_str());
        }

        dict.set(
            "raw_extensions",
            &Variant::from(self.raw_extensions.clone()),
        );
        dict
    }

    pub fn from_dictionary(dict: VarDictionary) -> Self {
        let source_extension =
            dict_string(&dict, "source_extension").unwrap_or_else(|| BASE_EXTENSION.to_string());
        let projection =
            dict_string(&dict, "projection").unwrap_or_else(|| "perspective".to_string());
        let sorting_method =
            dict_string(&dict, "sorting_method").unwrap_or_else(|| "cameraDistance".to_string());
        let kernel = dict_string(&dict, "kernel");
        let color_space = dict_string(&dict, "color_space");
        let compression = dict_string(&dict, "compression");
        let raw_extensions = dict
            .get("raw_extensions")
            .and_then(|variant| variant.try_to::<VarDictionary>().ok())
            .unwrap_or_default();

        Self {
            source_extension,
            kernel,
            color_space,
            projection,
            sorting_method,
            compression,
            raw_extensions,
        }
    }
}

fn dict_string(dict: &VarDictionary, key: &str) -> Option<String> {
    dict.get(key)
        .and_then(|value| value.try_to::<GString>().ok())
        .map(|value| value.to_string())
}
