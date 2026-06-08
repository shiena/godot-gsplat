use godot::builtin::VariantType;
use godot::classes::{
    editor_scene_post_import_plugin::InternalImportCategory, EditorScenePostImportPlugin,
    IEditorScenePostImportPlugin, Node, Resource,
};
use godot::prelude::*;
use godot::register::info::PropertyHint;

const OPTION_PREVIEW_MAX_SPLATS: &str = "gsplat/preview_max_splats";
const OPTION_PREVIEW_MAX_SPLAT_RADIUS: &str = "gsplat/preview_max_splat_radius";
const OPTION_PREVIEW_SCALE_MULTIPLIER: &str = "gsplat/preview_scale_multiplier";

#[derive(GodotClass)]
#[class(tool, init, base=EditorScenePostImportPlugin)]
pub struct GsplatScenePostImportPlugin {
    #[base]
    base: Base<EditorScenePostImportPlugin>,
}

#[godot_api]
impl IEditorScenePostImportPlugin for GsplatScenePostImportPlugin {
    fn get_import_options(&mut self, path: GString) {
        if !should_add_options_for_path(path) {
            return;
        }

        self.add_preview_options();
    }

    fn get_internal_import_options(&mut self, category: i32) {
        if category != InternalImportCategory::NODE.ord() {
            return;
        }

        self.add_preview_options();
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
        let is_gsplat_option = option == OPTION_PREVIEW_MAX_SPLATS
            || option == OPTION_PREVIEW_MAX_SPLAT_RADIUS
            || option == OPTION_PREVIEW_SCALE_MULTIPLIER;
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
        let is_gsplat_option = option == OPTION_PREVIEW_MAX_SPLATS
            || option == OPTION_PREVIEW_MAX_SPLAT_RADIUS
            || option == OPTION_PREVIEW_SCALE_MULTIPLIER;
        if is_gsplat_option {
            Variant::from(category == InternalImportCategory::NODE.ord())
        } else {
            Variant::nil()
        }
    }

    fn get_internal_option_update_view_required(&self, category: i32, option: GString) -> Variant {
        let option = option.to_string();
        let is_gsplat_option = option == OPTION_PREVIEW_MAX_SPLATS
            || option == OPTION_PREVIEW_MAX_SPLAT_RADIUS
            || option == OPTION_PREVIEW_SCALE_MULTIPLIER;
        Variant::from(category == InternalImportCategory::NODE.ord() && is_gsplat_option)
    }

    fn internal_process(
        &mut self,
        category: i32,
        base_node: Option<Gd<Node>>,
        node: Option<Gd<Node>>,
        _resource: Option<Gd<Resource>>,
    ) {
        if category != InternalImportCategory::NODE.ord() {
            return;
        }

        let options = self.read_options();
        if let Some(node) = node {
            apply_preview_options_to_tree(node, &options);
        } else if let Some(base_node) = base_node {
            apply_preview_options_to_tree(base_node, &options);
        }
    }

    fn post_process(&mut self, scene: Option<Gd<Node>>) {
        let Some(scene) = scene else {
            return;
        };

        let options = self.read_options();
        apply_preview_options_to_tree(scene, &options);
    }
}

impl GsplatScenePostImportPlugin {
    fn read_options(&self) -> PreviewImportOptions {
        Self::read_options_from_plugin(&self.base())
    }

    fn add_preview_options(&mut self) {
        self.add_i32_option(OPTION_PREVIEW_MAX_SPLATS, 10_000, "0,5000000,1");
        self.add_f32_option(OPTION_PREVIEW_MAX_SPLAT_RADIUS, 0.02, "0.001,1.0,0.001");
        self.add_f32_option(OPTION_PREVIEW_SCALE_MULTIPLIER, 1.0, "0.01,64.0,0.01");
    }

    fn read_options_from_plugin(plugin: &EditorScenePostImportPlugin) -> PreviewImportOptions {
        PreviewImportOptions {
            max_splats: option_i32(plugin, OPTION_PREVIEW_MAX_SPLATS, 10_000),
            max_splat_radius: option_f32(plugin, OPTION_PREVIEW_MAX_SPLAT_RADIUS, 0.02),
            scale_multiplier: option_f32(plugin, OPTION_PREVIEW_SCALE_MULTIPLIER, 1.0),
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

#[derive(Clone, Copy, Debug)]
struct PreviewImportOptions {
    max_splats: i32,
    max_splat_radius: f32,
    scale_multiplier: f32,
}

fn should_add_options_for_path(path: GString) -> bool {
    if path.is_empty() {
        return true;
    }

    let path = path.to_string().to_ascii_lowercase();
    path.ends_with(".gltf") || path.ends_with(".glb")
}

fn option_i32(plugin: &EditorScenePostImportPlugin, name: &str, fallback: i32) -> i32 {
    plugin
        .get_option_value(name)
        .try_to::<i32>()
        .unwrap_or(fallback)
}

fn option_f32(plugin: &EditorScenePostImportPlugin, name: &str, fallback: f32) -> f32 {
    plugin
        .get_option_value(name)
        .try_to::<f32>()
        .unwrap_or(fallback)
}

fn apply_preview_options_to_tree(mut node: Gd<Node>, options: &PreviewImportOptions) {
    apply_preview_options_to_node(&mut node, options);
    for child in node.get_children().iter_shared() {
        if let Ok(child) = child.try_cast::<Node>() {
            apply_preview_options_to_tree(child, options);
        }
    }
}

fn apply_preview_options_to_node(node: &mut Gd<Node>, options: &PreviewImportOptions) {
    if !node.is_class("GaussianSplatNode3D") {
        return;
    }

    node.call(
        "set_preview_max_splats",
        &[Variant::from(options.max_splats)],
    );
    node.call(
        "set_preview_max_splat_radius",
        &[Variant::from(options.max_splat_radius)],
    );
    node.call(
        "set_preview_scale_multiplier",
        &[Variant::from(options.scale_multiplier)],
    );
}
