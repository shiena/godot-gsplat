use godot::init::EditorRunBehavior;
use godot::prelude::*;

mod asset;
mod gltf_extension;
mod import_state;
mod node;

struct GodotGsplatExtension;

#[gdextension]
unsafe impl ExtensionLibrary for GodotGsplatExtension {
    fn editor_run_behavior() -> EditorRunBehavior {
        EditorRunBehavior::ToolClassesOnly
    }
}

pub use asset::GaussianSplatAsset;
pub use gltf_extension::GltfGsplatDocumentExtension;
pub use node::GaussianSplatNode3D;
