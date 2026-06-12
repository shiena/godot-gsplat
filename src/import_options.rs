//! Shared handling of the `gsplat/preview_*` import options.
//!
//! Two import paths consume the same options and must agree on how they are
//! read and applied:
//! - `GsplatScenePostImportPlugin` reads them from the live import options
//!   during a regular (re)import.
//! - `GltfGsplatDocumentExtension` re-reads them from the saved `.import`
//!   file, because Godot's Advanced Import Settings preview does not invoke
//!   scene post-import plugins (observed in 4.7 beta). Drop
//!   [`PreviewImportOptions::from_saved_import_file`] once the editor calls
//!   post-import plugins for previews.

use godot::classes::{ConfigFile, GltfState, Node};
use godot::global::Error;
use godot::prelude::*;

pub const OPTION_PREVIEW_MAX_SPLATS: &str = "gsplat/preview_max_splats";
pub const OPTION_PREVIEW_MAX_SPLAT_RADIUS: &str = "gsplat/preview_max_splat_radius";
pub const OPTION_PREVIEW_SCALE_MULTIPLIER: &str = "gsplat/preview_scale_multiplier";
pub const INTERNAL_OPTION_PREVIEW_MAX_SPLATS: &str = "gsplat_preview/preview_max_splats";
pub const INTERNAL_OPTION_PREVIEW_MAX_SPLAT_RADIUS: &str =
    "gsplat_preview/preview_max_splat_radius";
pub const INTERNAL_OPTION_PREVIEW_SCALE_MULTIPLIER: &str =
    "gsplat_preview/preview_scale_multiplier";

// Default preview limit. i32::MAX means "show every splat": it clamps to the
// asset's actual point count on load, so a freshly imported glTF previews all
// of its points. Users can still lower it in the import dialog.
pub const PREVIEW_MAX_SPLATS_DEFAULT: i32 = i32::MAX;

/// Preview options read from an import dialog or a saved `.import` file.
/// `None` means "not specified" and leaves the node's current value alone.
#[derive(Clone, Copy, Debug, Default)]
pub struct PreviewImportOptions {
    pub max_splats: Option<i32>,
    pub max_splat_radius: Option<f32>,
    pub scale_multiplier: Option<f32>,
}

impl PreviewImportOptions {
    pub fn is_empty(&self) -> bool {
        self.max_splats.is_none()
            && self.max_splat_radius.is_none()
            && self.scale_multiplier.is_none()
    }

    /// Read the options back from the `.import` file next to the glTF being
    /// parsed. Returns `None` when no file or no gsplat option was found.
    pub fn from_saved_import_file(state: &Gd<GltfState>) -> Option<Self> {
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

        let mut options = Self {
            max_splats: config_i32(&config, OPTION_PREVIEW_MAX_SPLATS),
            max_splat_radius: config_f32(&config, OPTION_PREVIEW_MAX_SPLAT_RADIUS),
            scale_multiplier: config_f32(&config, OPTION_PREVIEW_SCALE_MULTIPLIER),
        };

        if let Some(subresources) = config_value(&config, "_subresources") {
            options.merge_subresources(&subresources);
        }

        if options.is_empty() {
            None
        } else {
            Some(options)
        }
    }

    /// Override fields with per-node values nested in a `_subresources`
    /// dictionary (the internal `gsplat_preview/*` keys win over the general
    /// `gsplat/*` ones).
    pub fn merge_subresources(&mut self, subresources: &Variant) {
        if let Some(max_splats) = find_i32_option(subresources, INTERNAL_OPTION_PREVIEW_MAX_SPLATS)
            .or_else(|| find_i32_option(subresources, OPTION_PREVIEW_MAX_SPLATS))
        {
            self.max_splats = Some(max_splats);
        }
        if let Some(max_splat_radius) =
            find_f32_option(subresources, INTERNAL_OPTION_PREVIEW_MAX_SPLAT_RADIUS)
                .or_else(|| find_f32_option(subresources, OPTION_PREVIEW_MAX_SPLAT_RADIUS))
        {
            self.max_splat_radius = Some(max_splat_radius);
        }
        if let Some(scale_multiplier) =
            find_f32_option(subresources, INTERNAL_OPTION_PREVIEW_SCALE_MULTIPLIER)
                .or_else(|| find_f32_option(subresources, OPTION_PREVIEW_SCALE_MULTIPLIER))
        {
            self.scale_multiplier = Some(scale_multiplier);
        }
    }

    /// Apply the specified fields to `node` when it is a GaussianSplatNode3D.
    pub fn apply_to_node(&self, node: &mut Gd<Node>) {
        if !node.is_class("GaussianSplatNode3D") {
            return;
        }

        if let Some(max_splats) = self.max_splats {
            node.call("set_preview_max_splats", &[Variant::from(max_splats)]);
        }
        if let Some(max_splat_radius) = self.max_splat_radius {
            node.call(
                "set_preview_max_splat_radius",
                &[Variant::from(max_splat_radius)],
            );
        }
        if let Some(scale_multiplier) = self.scale_multiplier {
            node.call(
                "set_preview_scale_multiplier",
                &[Variant::from(scale_multiplier)],
            );
        }
    }

    /// Apply recursively to every GaussianSplatNode3D in the subtree.
    pub fn apply_to_tree(&self, mut node: Gd<Node>) {
        self.apply_to_node(&mut node);
        for child in node.get_children().iter_shared() {
            if let Ok(child) = child.try_cast::<Node>() {
                self.apply_to_tree(child);
            }
        }
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

fn find_i32_option(value: &Variant, name: &str) -> Option<i32> {
    find_option(value, name).and_then(|value| variant_to_i32(&value))
}

fn find_f32_option(value: &Variant, name: &str) -> Option<f32> {
    find_option(value, name).and_then(|value| variant_to_f32(&value))
}

// Depth-first search for an option key nested anywhere inside the
// dictionaries/arrays Godot stores in `_subresources`.
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

pub fn variant_to_i32(value: &Variant) -> Option<i32> {
    value.try_to::<i32>().ok().or_else(|| {
        value
            .try_to::<i64>()
            .ok()
            .map(|value| value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32)
    })
}

pub fn variant_to_f32(value: &Variant) -> Option<f32> {
    value
        .try_to::<f32>()
        .ok()
        .or_else(|| value.try_to::<f64>().ok().map(|value| value as f32))
}
