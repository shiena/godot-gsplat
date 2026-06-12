extends RefCounted

# Shared helpers for loading the GDExtension library and registering the
# Rust-side glTF document extension. Used by both the editor plugin
# (plugin.gd) and the runtime registrar (register_gsplat.gd).

const EXTENSION_CLASS_NAME := "GltfGsplatDocumentExtension"
const EXTENSION_LIBRARY_PATH := "res://godot_gsplat.gdextension"


static func ensure_extension_library_loaded() -> bool:
	if GDExtensionManager.is_extension_loaded(EXTENSION_LIBRARY_PATH):
		return true

	var status: int = GDExtensionManager.load_extension(EXTENSION_LIBRARY_PATH)
	if status == GDExtensionManager.LOAD_STATUS_OK:
		return true
	if status == GDExtensionManager.LOAD_STATUS_ALREADY_LOADED:
		return true

	push_warning("Failed to load GDExtension library: %s" % [status])
	return false


# Instantiates and registers the glTF document extension. Returns the live
# instance (keep it to unregister later), or null when unavailable.
static func register_gltf_extension() -> Object:
	if not ensure_extension_library_loaded():
		return null
	if not ClassDB.class_exists(EXTENSION_CLASS_NAME):
		push_warning("GDExtension class '%s' is not available yet." % EXTENSION_CLASS_NAME)
		return null

	var extension: Object = ClassDB.instantiate(EXTENSION_CLASS_NAME)
	GLTFDocument.register_gltf_document_extension(extension, true)
	return extension


static func unregister_gltf_extension(extension: Object) -> void:
	if extension == null:
		return
	GLTFDocument.unregister_gltf_document_extension(extension)
