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
const INTERNAL_OPTION_PREVIEW_MAX_SPLATS: &str = "gsplat_preview/preview_max_splats";
const INTERNAL_OPTION_PREVIEW_MAX_SPLAT_RADIUS: &str = "gsplat_preview/preview_max_splat_radius";
const INTERNAL_OPTION_PREVIEW_SCALE_MULTIPLIER: &str = "gsplat_preview/preview_scale_multiplier";

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
            apply_preview_options_to_tree(node, &options);
        } else if let Some(base_node) = base_node {
            apply_preview_options_to_tree(base_node, &options);
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
        apply_preview_options_to_tree(scene, &options);
    }
}

impl GsplatScenePostImportPlugin {
    fn read_options(&self) -> PreviewImportOptions {
        Self::read_options_from_plugin(&self.base())
    }

    fn read_options_with_subresources(&self) -> PreviewImportOptions {
        let mut options = self.read_options();
        let subresources = self.base().get_option_value("_subresources");
        apply_options_from_subresources(&mut options, &subresources);
        options
    }

    fn add_general_preview_options(&mut self) {
        self.add_i32_option(OPTION_PREVIEW_MAX_SPLATS, 10_000, "0,1,or_greater");
        self.add_f32_option(OPTION_PREVIEW_MAX_SPLAT_RADIUS, 0.02, "0.001,1.0,0.001");
        self.add_f32_option(OPTION_PREVIEW_SCALE_MULTIPLIER, 1.0, "0.01,64.0,0.01");
    }

    fn add_internal_preview_options(&mut self) {
        self.add_i32_option(INTERNAL_OPTION_PREVIEW_MAX_SPLATS, 10_000, "0,1,or_greater");
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

fn apply_options_from_subresources(options: &mut PreviewImportOptions, subresources: &Variant) {
    if let Some(max_splats) = find_i32_option(subresources, INTERNAL_OPTION_PREVIEW_MAX_SPLATS)
        .or_else(|| find_i32_option(subresources, OPTION_PREVIEW_MAX_SPLATS))
    {
        options.max_splats = max_splats;
    }
    if let Some(max_splat_radius) =
        find_f32_option(subresources, INTERNAL_OPTION_PREVIEW_MAX_SPLAT_RADIUS)
            .or_else(|| find_f32_option(subresources, OPTION_PREVIEW_MAX_SPLAT_RADIUS))
    {
        options.max_splat_radius = max_splat_radius;
    }
    if let Some(scale_multiplier) =
        find_f32_option(subresources, INTERNAL_OPTION_PREVIEW_SCALE_MULTIPLIER)
            .or_else(|| find_f32_option(subresources, OPTION_PREVIEW_SCALE_MULTIPLIER))
    {
        options.scale_multiplier = scale_multiplier;
    }
}

fn find_i32_option(value: &Variant, name: &str) -> Option<i32> {
    find_option(value, name).and_then(|value| value.try_to::<i32>().ok())
}

fn find_f32_option(value: &Variant, name: &str) -> Option<f32> {
    find_option(value, name).and_then(|value| value.try_to::<f32>().ok())
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
