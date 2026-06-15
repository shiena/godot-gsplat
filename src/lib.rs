use godot::init::EditorRunBehavior;
use godot::prelude::*;

mod asset;
mod backend;
mod chunking;
mod cloud_settings;
mod gltf_extension;
mod gsplat_pack;
mod import_options;
mod import_state;
mod node;
mod scene_import_plugin;

struct GodotGsplatExtension;

#[gdextension]
unsafe impl ExtensionLibrary for GodotGsplatExtension {
    fn editor_run_behavior() -> EditorRunBehavior {
        EditorRunBehavior::ToolClassesOnly
    }
}

pub use asset::GaussianSplatAsset;
pub use backend::GaussianSplatBackendSettings;
pub use cloud_settings::GaussianSplatCloudSettings;
pub use gltf_extension::GltfGsplatDocumentExtension;
pub use node::GaussianSplatNode3D;
pub use scene_import_plugin::GsplatScenePostImportPlugin;
