@tool
extends EditorPlugin

const EXTENSION_CLASS_NAME := "GltfGsplatDocumentExtension"
const SCENE_POST_IMPORT_PLUGIN_CLASS_NAME := "GsplatScenePostImportPlugin"
const EXTENSION_LIBRARY_PATH := "res://godot_gsplat.gdextension"
const OPTION_PREVIEW_MAX_SPLATS := "gsplat/preview_max_splats"
const PENDING_PREVIEW_CLAMPS_PATH := "user://godot_gsplat_pending_preview_clamps.cfg"

var _gltf_extension: Object
var _scene_post_import_plugin: Object
var _resource_filesystem: EditorFileSystem
var _pending_clamp_poll_elapsed := 0.0

func _enter_tree() -> void:
	_register_gltf_extension()
	_register_scene_post_import_plugin()
	_register_filesystem_hooks()
	set_process(true)

func _exit_tree() -> void:
	set_process(false)
	_unregister_filesystem_hooks()
	_unregister_scene_post_import_plugin()
	_unregister_gltf_extension()

func _process(delta: float) -> void:
	_pending_clamp_poll_elapsed += delta
	if _pending_clamp_poll_elapsed < 0.25:
		return
	_pending_clamp_poll_elapsed = 0.0
	_apply_pending_preview_limit_clamps()

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
		print("[godot-gsplat] Editor filesystem hook unavailable.")
		return
	if not _resource_filesystem.resources_reimported.is_connected(_on_resources_reimported):
		_resource_filesystem.resources_reimported.connect(_on_resources_reimported)
		print("[godot-gsplat] Connected resources_reimported hook.")

func _unregister_filesystem_hooks() -> void:
	if _resource_filesystem == null:
		return
	if _resource_filesystem.resources_reimported.is_connected(_on_resources_reimported):
		_resource_filesystem.resources_reimported.disconnect(_on_resources_reimported)
	_resource_filesystem = null

func _on_resources_reimported(resources: PackedStringArray) -> void:
	print("[godot-gsplat] resources_reimported: %s" % [Array(resources)])
	_clamp_reimported_preview_limits(resources)

func _clamp_reimported_preview_limits(resources: PackedStringArray) -> void:
	for resource_path in resources:
		if _is_gltf_path(resource_path):
			_clamp_import_preview_limit(resource_path)

func _is_gltf_path(path: String) -> bool:
	var extension := path.get_extension().to_lower()
	return extension == "gltf" or extension == "glb"

func _clamp_import_preview_limit(source_path: String) -> void:
	var clamped_limit := _read_pending_preview_limit(source_path)
	if clamped_limit < 0:
		clamped_limit = _read_imported_point_count(source_path)
		print("[godot-gsplat] Clamp fallback point_count for '%s': %s." % [source_path, clamped_limit])
	if clamped_limit < 0:
		print("[godot-gsplat] Clamp skip: point count unavailable for '%s'." % [source_path])
		return

	if _apply_preview_limit_clamp(source_path, clamped_limit):
		_clear_pending_preview_limit(source_path)

func _apply_pending_preview_limit_clamps() -> void:
	var pending_config := ConfigFile.new()
	var load_status := pending_config.load(PENDING_PREVIEW_CLAMPS_PATH)
	if load_status != OK:
		return
	if not pending_config.has_section("files"):
		return

	for source_path in pending_config.get_section_keys("files"):
		var clamped_limit := int(pending_config.get_value("files", source_path, -1))
		print("[godot-gsplat] Pending clamp poll: source='%s', clamped=%s." % [source_path, clamped_limit])
		if clamped_limit < 0:
			_clear_pending_preview_limit(source_path)
			continue
		if _apply_preview_limit_clamp(source_path, clamped_limit):
			_clear_pending_preview_limit(source_path)

func _apply_preview_limit_clamp(source_path: String, max_valid_limit: int) -> bool:
	var import_path := source_path + ".import"
	var config := ConfigFile.new()
	var load_status := config.load(import_path)
	if load_status != OK:
		print("[godot-gsplat] Clamp skip: failed to load '%s': %s." % [import_path, load_status])
		return false
	if not config.has_section_key("params", OPTION_PREVIEW_MAX_SPLATS):
		print("[godot-gsplat] Clamp skip: '%s' has no %s." % [import_path, OPTION_PREVIEW_MAX_SPLATS])
		return true

	var saved_limit := int(config.get_value("params", OPTION_PREVIEW_MAX_SPLATS, 10000))
	var clamped_limit := clampi(saved_limit, 0, max_valid_limit)
	print("[godot-gsplat] Clamp check: source='%s', saved=%s, max_valid=%s, result=%s." % [source_path, saved_limit, max_valid_limit, clamped_limit])
	if clamped_limit == saved_limit:
		print("[godot-gsplat] Clamp skip: saved limit already valid for '%s': %s." % [source_path, saved_limit])
		return true

	config.set_value("params", OPTION_PREVIEW_MAX_SPLATS, clamped_limit)
	var save_status := config.save(import_path)
	if save_status != OK:
		push_warning("Failed to save clamped Gaussian splat preview limit for '%s': %s" % [source_path, save_status])
		return false
	print("[godot-gsplat] Clamped %s from %s to %s." % [OPTION_PREVIEW_MAX_SPLATS, saved_limit, clamped_limit])
	return true

func _read_pending_preview_limit(source_path: String) -> int:
	var config := ConfigFile.new()
	var load_status := config.load(PENDING_PREVIEW_CLAMPS_PATH)
	if load_status != OK:
		print("[godot-gsplat] Pending clamp unavailable for '%s': load_status=%s." % [source_path, load_status])
		return -1
	if not config.has_section_key("files", source_path):
		print("[godot-gsplat] Pending clamp missing for '%s'." % [source_path])
		return -1

	var clamped_limit := int(config.get_value("files", source_path, -1))
	print("[godot-gsplat] Pending clamp hit for '%s': %s." % [source_path, clamped_limit])
	return clamped_limit

func _clear_pending_preview_limit(source_path: String) -> void:
	var config := ConfigFile.new()
	var load_status := config.load(PENDING_PREVIEW_CLAMPS_PATH)
	if load_status != OK:
		return
	if not config.has_section_key("files", source_path):
		return

	config.set_value("files", source_path, null)
	var save_status := config.save(PENDING_PREVIEW_CLAMPS_PATH)
	if save_status != OK:
		push_warning("Failed to clear pending Gaussian splat preview limit for '%s': %s" % [source_path, save_status])
	else:
		print("[godot-gsplat] Cleared pending clamp for '%s'." % [source_path])

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
