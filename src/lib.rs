use godot::init::EditorRunBehavior;
use godot::prelude::*;

mod asset;
mod backend;
mod cloud_settings;
mod gltf_extension;
mod import_state;
mod node;
mod preview_mesh;
mod render_manager;
mod render_packet;

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
pub use preview_mesh::GaussianSplatPreviewMeshInstance3D;
pub use render_manager::GaussianSplatRenderManager;
pub use render_packet::GaussianSplatRenderPacket;
