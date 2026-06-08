@tool
extends EditorPlugin

const EXTENSION_CLASS_NAME := "GltfGsplatDocumentExtension"
const SCENE_POST_IMPORT_PLUGIN_CLASS_NAME := "GsplatScenePostImportPlugin"
const EXTENSION_LIBRARY_PATH := "res://godot_gsplat.gdextension"

var _gltf_extension: Object
var _scene_post_import_plugin: Object

func _enter_tree() -> void:
	_register_gltf_extension()
	_register_scene_post_import_plugin()

func _exit_tree() -> void:
	_unregister_scene_post_import_plugin()
	_unregister_gltf_extension()

func _register_gltf_extension() -> void:
	if _gltf_extension != null:
		return
	if not _ensure_extension_library_loaded():
		return
	if not ClassDB.class_exists(EXTENSION_CLASS_NAME):
		push_warning("GDExtension class '%s' is not available yet." % EXTENSION_CLASS_NAME)
		return

	_gltf_extension = ClassDB.instantiate(EXTENSION_CLASS_NAME)
	GLTFDocument.register_gltf_document_extension(_gltf_extension, true)
	print("[godot-gsplat] Registered GLTF document extension.")

func _register_scene_post_import_plugin() -> void:
	if _scene_post_import_plugin != null:
		return
	if not _ensure_extension_library_loaded():
		return
	if not ClassDB.class_exists(SCENE_POST_IMPORT_PLUGIN_CLASS_NAME):
		push_warning("GDExtension class '%s' is not available yet." % SCENE_POST_IMPORT_PLUGIN_CLASS_NAME)
		return

	_scene_post_import_plugin = ClassDB.instantiate(SCENE_POST_IMPORT_PLUGIN_CLASS_NAME)
	add_scene_post_import_plugin(_scene_post_import_plugin, true)
	print("[godot-gsplat] Registered scene post import plugin.")

func _unregister_scene_post_import_plugin() -> void:
	if _scene_post_import_plugin == null:
		return

	remove_scene_post_import_plugin(_scene_post_import_plugin)
	_scene_post_import_plugin = null
	print("[godot-gsplat] Unregistered scene post import plugin.")

func _unregister_gltf_extension() -> void:
	if _gltf_extension == null:
		return

	GLTFDocument.unregister_gltf_document_extension(_gltf_extension)
	_gltf_extension = null
	print("[godot-gsplat] Unregistered GLTF document extension.")

func _ensure_extension_library_loaded() -> bool:
	if GDExtensionManager.is_extension_loaded(EXTENSION_LIBRARY_PATH):
		return true

	var status: int = GDExtensionManager.load_extension(EXTENSION_LIBRARY_PATH)
	if status == GDExtensionManager.LOAD_STATUS_OK:
		return true
	if status == GDExtensionManager.LOAD_STATUS_ALREADY_LOADED:
		return true

	push_warning("Failed to load GDExtension library: %s" % [status])
	return false
