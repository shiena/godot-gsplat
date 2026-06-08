@tool
extends EditorPlugin

const EXTENSION_CLASS_NAME := "GltfGsplatDocumentExtension"
const SCENE_POST_IMPORT_PLUGIN_CLASS_NAME := "GsplatScenePostImportPlugin"
const EXTENSION_LIBRARY_PATH := "res://godot_gsplat.gdextension"
const OPTION_PREVIEW_MAX_SPLATS := "gsplat/preview_max_splats"

var _gltf_extension: Object
var _scene_post_import_plugin: Object
var _resource_filesystem: EditorFileSystem

func _enter_tree() -> void:
	_register_gltf_extension()
	_register_scene_post_import_plugin()
	_register_filesystem_hooks()

func _exit_tree() -> void:
	_unregister_filesystem_hooks()
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

func _register_filesystem_hooks() -> void:
	if _resource_filesystem != null:
		return
	_resource_filesystem = get_editor_interface().get_resource_filesystem()
	if _resource_filesystem == null:
		return
	if not _resource_filesystem.resources_reimported.is_connected(_on_resources_reimported):
		_resource_filesystem.resources_reimported.connect(_on_resources_reimported)

func _unregister_filesystem_hooks() -> void:
	if _resource_filesystem == null:
		return
	if _resource_filesystem.resources_reimported.is_connected(_on_resources_reimported):
		_resource_filesystem.resources_reimported.disconnect(_on_resources_reimported)
	_resource_filesystem = null

func _on_resources_reimported(resources: PackedStringArray) -> void:
	call_deferred("_clamp_reimported_preview_limits", resources)

func _clamp_reimported_preview_limits(resources: PackedStringArray) -> void:
	for resource_path in resources:
		if _is_gltf_path(resource_path):
			_clamp_import_preview_limit(resource_path)

func _is_gltf_path(path: String) -> bool:
	var extension := path.get_extension().to_lower()
	return extension == "gltf" or extension == "glb"

func _clamp_import_preview_limit(source_path: String) -> void:
	var import_path := source_path + ".import"
	var config := ConfigFile.new()
	if config.load(import_path) != OK:
		return
	if not config.has_section_key("params", OPTION_PREVIEW_MAX_SPLATS):
		return

	var saved_limit := int(config.get_value("params", OPTION_PREVIEW_MAX_SPLATS, 10000))
	var point_count := _read_imported_point_count(source_path)
	if point_count < 0:
		return

	var clamped_limit := clampi(saved_limit, 0, point_count)
	if clamped_limit == saved_limit:
		return

	config.set_value("params", OPTION_PREVIEW_MAX_SPLATS, clamped_limit)
	var save_status := config.save(import_path)
	if save_status != OK:
		push_warning("Failed to save clamped Gaussian splat preview limit for '%s': %s" % [source_path, save_status])
		return
	print("[godot-gsplat] Clamped %s from %s to %s." % [OPTION_PREVIEW_MAX_SPLATS, saved_limit, clamped_limit])

func _read_imported_point_count(source_path: String) -> int:
	var packed_scene := ResourceLoader.load(source_path)
	if packed_scene == null or not packed_scene is PackedScene:
		return -1

	var root: Node = packed_scene.instantiate()
	if root == null:
		return -1

	var point_count := _find_first_splat_point_count(root)
	root.queue_free()
	return point_count

func _find_first_splat_point_count(node: Node) -> int:
	if node.has_method("get_asset"):
		var asset: Object = node.call("get_asset")
		if asset != null and asset.has_method("get_point_count"):
			return int(asset.call("get_point_count"))

	for child in node.get_children():
		var point_count := _find_first_splat_point_count(child)
		if point_count >= 0:
			return point_count

	return -1

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
