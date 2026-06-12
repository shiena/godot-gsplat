use godot::builtin::VariantType;
use godot::classes::{
    editor_scene_post_import_plugin::InternalImportCategory, EditorScenePostImportPlugin,
    IEditorScenePostImportPlugin, Node, Resource,
};
use godot::prelude::*;
use godot::register::info::PropertyHint;

use crate::import_options::{
    variant_to_f32, variant_to_i32, PreviewImportOptions, INTERNAL_OPTION_PREVIEW_MAX_SPLATS,
    INTERNAL_OPTION_PREVIEW_MAX_SPLAT_RADIUS, INTERNAL_OPTION_PREVIEW_SCALE_MULTIPLIER,
    OPTION_PREVIEW_MAX_SPLATS, OPTION_PREVIEW_MAX_SPLAT_RADIUS, OPTION_PREVIEW_SCALE_MULTIPLIER,
    PREVIEW_MAX_SPLATS_DEFAULT,
};

// Point counts are i32 throughout the pipeline, so i32::MAX is the real ceiling.
// A previous unbounded `or_greater` range let the inspector accept values that
// overflowed i64, raising a parse error and saving 0.
const PREVIEW_MAX_SPLATS_HINT: &str = "0,2147483647,1";

#[derive(GodotClass)]
#[class(tool, base=EditorScenePostImportPlugin)]
pub struct GsplatScenePostImportPlugin {
    #[base]
    base: Base<EditorScenePostImportPlugin>,
    cached_options: Option<PreviewImportOptions>,
}

#[godot_api]
impl IEditorScenePostImportPlugin for GsplatScenePostImportPlugin {
    fn init(base: Base<EditorScenePostImportPlugin>) -> Self {
        Self {
            base,
            cached_options: None,
        }
    }

    fn get_import_options(&mut self, path: GString) {
        if !should_add_options_for_path(path) {
            return;
        }

        self.add_general_preview_options();
    }

    fn get_internal_import_options(&mut self, category: i32) {
        if !is_node_like_category(category) {
            return;
        }

        self.add_internal_preview_options();
    }

    fn get_option_visibility(
        &self,
        path: GString,
        _for_animation: bool,
        option: GString,
    ) -> Variant {
        if !should_add_options_for_path(path) {
            return Variant::nil();
        }

        let option = option.to_string();
        let is_gsplat_option = is_general_preview_option(option.as_str());
        if is_gsplat_option {
            Variant::from(true)
        } else {
            Variant::nil()
        }
    }

    fn get_internal_option_visibility(
        &self,
        category: i32,
        _for_animation: bool,
        option: GString,
    ) -> Variant {
        let option = option.to_string();
        let is_gsplat_option = is_internal_preview_option(option.as_str());
        if is_gsplat_option {
            Variant::from(is_node_like_category(category))
        } else {
            Variant::nil()
        }
    }

    fn get_internal_option_update_view_required(&self, category: i32, option: GString) -> Variant {
        let option = option.to_string();
        let is_gsplat_option = is_internal_preview_option(option.as_str());
        Variant::from(is_node_like_category(category) && is_gsplat_option)
    }

    fn pre_process(&mut self, _scene: Option<Gd<Node>>) {
        self.cached_options = Some(self.read_options_with_subresources());
    }

    fn internal_process(
        &mut self,
        category: i32,
        base_node: Option<Gd<Node>>,
        node: Option<Gd<Node>>,
        _resource: Option<Gd<Resource>>,
    ) {
        if !is_node_like_category(category) {
            return;
        }

        let options = self.cached_options.unwrap_or_else(|| self.read_options());
        if let Some(node) = node {
            options.apply_to_tree(node);
        } else if let Some(base_node) = base_node {
            options.apply_to_tree(base_node);
        }
    }

    fn post_process(&mut self, scene: Option<Gd<Node>>) {
        let Some(scene) = scene else {
            return;
        };

        let options = self
            .cached_options
            .take()
            .unwrap_or_else(|| self.read_options());
        options.apply_to_tree(scene);
    }
}

impl GsplatScenePostImportPlugin {
    fn read_options(&self) -> PreviewImportOptions {
        Self::read_options_from_plugin(&self.base())
    }

    fn read_options_with_subresources(&self) -> PreviewImportOptions {
        let mut options = self.read_options();
        let subresources = self.base().get_option_value("_subresources");
        options.merge_subresources(&subresources);
        options
    }

    fn add_general_preview_options(&mut self) {
        self.add_i32_option(
            OPTION_PREVIEW_MAX_SPLATS,
            PREVIEW_MAX_SPLATS_DEFAULT,
            PREVIEW_MAX_SPLATS_HINT,
        );
        self.add_f32_option(OPTION_PREVIEW_MAX_SPLAT_RADIUS, 0.02, "0.001,1.0,0.001");
        self.add_f32_option(OPTION_PREVIEW_SCALE_MULTIPLIER, 1.0, "0.01,64.0,0.01");
    }

    fn add_internal_preview_options(&mut self) {
        self.add_i32_option(
            INTERNAL_OPTION_PREVIEW_MAX_SPLATS,
            PREVIEW_MAX_SPLATS_DEFAULT,
            PREVIEW_MAX_SPLATS_HINT,
        );
        self.add_f32_option(
            INTERNAL_OPTION_PREVIEW_MAX_SPLAT_RADIUS,
            0.02,
            "0.001,1.0,0.001",
        );
        self.add_f32_option(
            INTERNAL_OPTION_PREVIEW_SCALE_MULTIPLIER,
            1.0,
            "0.01,64.0,0.01",
        );
    }

    fn read_options_from_plugin(plugin: &EditorScenePostImportPlugin) -> PreviewImportOptions {
        PreviewImportOptions {
            max_splats: Some(option_i32(
                plugin,
                OPTION_PREVIEW_MAX_SPLATS,
                PREVIEW_MAX_SPLATS_DEFAULT,
            )),
            max_splat_radius: Some(option_f32(plugin, OPTION_PREVIEW_MAX_SPLAT_RADIUS, 0.02)),
            scale_multiplier: Some(option_f32(plugin, OPTION_PREVIEW_SCALE_MULTIPLIER, 1.0)),
        }
    }

    fn add_i32_option(&mut self, name: &str, default_value: i32, hint_string: &str) {
        self.base_mut()
            .add_import_option_advanced_ex(VariantType::INT, name, &Variant::from(default_value))
            .hint(PropertyHint::RANGE)
            .hint_string(hint_string)
            .done();
    }

    fn add_f32_option(&mut self, name: &str, default_value: f32, hint_string: &str) {
        self.base_mut()
            .add_import_option_advanced_ex(VariantType::FLOAT, name, &Variant::from(default_value))
            .hint(PropertyHint::RANGE)
            .hint_string(hint_string)
            .done();
    }
}

fn should_add_options_for_path(path: GString) -> bool {
    if path.is_empty() {
        return true;
    }

    let path = path.to_string().to_ascii_lowercase();
    path.ends_with(".gltf") || path.ends_with(".glb")
}

fn is_node_like_category(category: i32) -> bool {
    category == InternalImportCategory::NODE.ord()
        || category == InternalImportCategory::MESH_3D_NODE.ord()
        || category == InternalImportCategory::SKELETON_3D_NODE.ord()
}

fn is_general_preview_option(option: &str) -> bool {
    option == OPTION_PREVIEW_MAX_SPLATS
        || option == OPTION_PREVIEW_MAX_SPLAT_RADIUS
        || option == OPTION_PREVIEW_SCALE_MULTIPLIER
}

fn is_internal_preview_option(option: &str) -> bool {
    option == INTERNAL_OPTION_PREVIEW_MAX_SPLATS
        || option == INTERNAL_OPTION_PREVIEW_MAX_SPLAT_RADIUS
        || option == INTERNAL_OPTION_PREVIEW_SCALE_MULTIPLIER
}

fn option_i32(plugin: &EditorScenePostImportPlugin, name: &str, fallback: i32) -> i32 {
    variant_to_i32(&plugin.get_option_value(name)).unwrap_or(fallback)
}

fn option_f32(plugin: &EditorScenePostImportPlugin, name: &str, fallback: f32) -> f32 {
    variant_to_f32(&plugin.get_option_value(name)).unwrap_or(fallback)
}
