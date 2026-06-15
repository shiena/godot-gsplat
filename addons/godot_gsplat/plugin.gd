@tool
extends EditorPlugin

const GltfRegistration := preload("res://addons/godot_gsplat/runtime/gltf_registration.gd")
const PackConverterDock := preload("res://addons/godot_gsplat/pack_converter_dock.gd")
const SCENE_POST_IMPORT_PLUGIN_CLASS_NAME := "GsplatScenePostImportPlugin"
const OPTION_PREVIEW_MAX_SPLATS := "gsplat/preview_max_splats"
const GAUSSIAN_SPLAT_NODE_CLASS_NAME := "GaussianSplatNode3D"
const SOURCE_GLTF_PROPERTY := "source_gltf"
const PACK_CONVERTER_MENU_ITEM := "Godot Gsplat Pack Converter"

var _gltf_extension: Object
var _scene_post_import_plugin: Object
var _resource_filesystem: EditorFileSystem
var _pack_converter_dock: Control

func _enter_tree() -> void:
	_register_gltf_extension()
	_register_scene_post_import_plugin()
	_register_filesystem_hooks()
	_register_viewport_drop_hook()
	_register_pack_converter_tool()

func _exit_tree() -> void:
	_unregister_pack_converter_tool()
	_unregister_viewport_drop_hook()
	_unregister_filesystem_hooks()
	_unregister_scene_post_import_plugin()
	_unregister_gltf_extension()

func _register_gltf_extension() -> void:
	if _gltf_extension != null:
		return

	_gltf_extension = GltfRegistration.register_gltf_extension()
	if _gltf_extension != null:
		print("[godot-gsplat] Registered GLTF document extension.")

func _register_scene_post_import_plugin() -> void:
	if _scene_post_import_plugin != null:
		return
	if not GltfRegistration.ensure_extension_library_loaded():
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

	GltfRegistration.unregister_gltf_extension(_gltf_extension)
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

func _register_pack_converter_tool() -> void:
	if _pack_converter_dock != null:
		return
	_pack_converter_dock = PackConverterDock.new()
	add_control_to_dock(DOCK_SLOT_RIGHT_UL, _pack_converter_dock)
	_pack_converter_dock.hide()
	add_tool_menu_item(PACK_CONVERTER_MENU_ITEM, _show_pack_converter_tool)

func _unregister_pack_converter_tool() -> void:
	remove_tool_menu_item(PACK_CONVERTER_MENU_ITEM)
	if _pack_converter_dock == null:
		return
	remove_control_from_docks(_pack_converter_dock)
	_pack_converter_dock.queue_free()
	_pack_converter_dock = null

func _show_pack_converter_tool() -> void:
	if _pack_converter_dock == null:
		_register_pack_converter_tool()
	if _pack_converter_dock == null:
		return
	_pack_converter_dock.show()
	_pack_converter_dock.grab_focus()

# --- Drag-and-drop into the 3D viewport -------------------------------------
#
# Godot's 3D viewport has no EditorPlugin hook for drops, so dropping a glTF
# instances its imported scene: a plain Node3D root that wraps the generated
# GaussianSplatNode3D. We watch the editor tree for that freshly instanced
# wrapper and swap it for a clean GaussianSplatNode3D whose Source glTF points
# at the dropped file (it then live-loads the splats).

func _register_viewport_drop_hook() -> void:
	var tree := get_tree()
	if tree == null:
		return
	if not tree.node_added.is_connected(_on_scene_node_added):
		tree.node_added.connect(_on_scene_node_added)

func _unregister_viewport_drop_hook() -> void:
	var tree := get_tree()
	if tree == null:
		return
	if tree.node_added.is_connected(_on_scene_node_added):
		tree.node_added.disconnect(_on_scene_node_added)

func _on_scene_node_added(node: Node) -> void:
	# A just-dropped instance enters the tree before the editor assigns its owner;
	# nodes streamed in while a saved scene loads already carry their owner. Only
	# the former is a fresh drop we should rewrite.
	if node.owner != null:
		return
	var source_path := node.scene_file_path
	if source_path == "" or not _is_gltf_path(source_path):
		return
	if not _subtree_has_gsplat(node):
		return
	var edited_root := get_editor_interface().get_edited_scene_root()
	if edited_root == null:
		return
	var parent := node.get_parent()
	if parent == null:
		return
	if parent != edited_root and not edited_root.is_ancestor_of(parent):
		return
	# Defer: let the drop's own undo action finish committing first.
	_replace_with_gsplat_node.call_deferred(node, source_path, edited_root)

func _subtree_has_gsplat(node: Node) -> bool:
	if node.is_class(GAUSSIAN_SPLAT_NODE_CLASS_NAME):
		return true
	for child in node.get_children():
		if _subtree_has_gsplat(child):
			return true
	return false

func _replace_with_gsplat_node(wrapper: Node, source_path: String, edited_root: Node) -> void:
	if not is_instance_valid(wrapper) or not is_instance_valid(edited_root):
		return
	var parent := wrapper.get_parent()
	if parent == null:
		return
	if not ClassDB.class_exists(GAUSSIAN_SPLAT_NODE_CLASS_NAME):
		push_warning("GDExtension class '%s' is not available." % GAUSSIAN_SPLAT_NODE_CLASS_NAME)
		return

	var gsplat: Node = ClassDB.instantiate(GAUSSIAN_SPLAT_NODE_CLASS_NAME)
	gsplat.name = source_path.get_file().get_basename()
	var index := wrapper.get_index()
	var has_xform := wrapper is Node3D and gsplat is Node3D
	var xform: Transform3D = (wrapper as Node3D).transform if has_xform else Transform3D.IDENTITY

	var undo_redo := get_undo_redo()
	undo_redo.create_action("Drop Gaussian Splat glTF", UndoRedo.MERGE_DISABLE, edited_root)
	undo_redo.add_do_method(parent, "remove_child", wrapper)
	undo_redo.add_do_method(parent, "add_child", gsplat, true)
	undo_redo.add_do_method(parent, "move_child", gsplat, index)
	if has_xform:
		undo_redo.add_do_property(gsplat, "transform", xform)
	undo_redo.add_do_method(gsplat, "set_owner", edited_root)
	undo_redo.add_do_property(gsplat, SOURCE_GLTF_PROPERTY, source_path)
	undo_redo.add_do_method(self, "_select_only_node", gsplat)
	undo_redo.add_do_reference(gsplat)
	undo_redo.add_undo_method(parent, "remove_child", gsplat)
	undo_redo.add_undo_method(parent, "add_child", wrapper, true)
	undo_redo.add_undo_method(parent, "move_child", wrapper, index)
	undo_redo.add_undo_method(self, "_select_only_node", wrapper)
	undo_redo.add_undo_reference(wrapper)
	undo_redo.commit_action()

func _select_only_node(node: Node) -> void:
	if not is_instance_valid(node):
		return
	var selection := get_editor_interface().get_selection()
	if selection == null:
		return
	selection.clear()
	selection.add_node(node)

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

# Godot writes the .import params *after* the post-import plugin runs, so the raw
# value typed in the dialog is what gets stored — re-clamp it here.
func _clamp_import_preview_limit(source_path: String) -> void:
	var point_count := _read_imported_point_count(source_path)
	if point_count < 0:
		print("[godot-gsplat] Clamp skip: point count unavailable for '%s'." % [source_path])
		return

	# The editor re-reads .import when the dialog reopens, so the on-disk rewrite
	# is enough. Don't call EditorFileSystem.update_file() to notify it: a
	# ".import" path is registered as a regular file (no extension filter) and
	# then shows up in the FileSystem dock.
	_apply_preview_limit_clamp(source_path, point_count)

func _apply_preview_limit_clamp(source_path: String, max_valid_limit: int) -> bool:
	var import_path := source_path + ".import"
	var config := ConfigFile.new()
	var load_status := config.load(import_path)
	if load_status != OK:
		print("[godot-gsplat] Clamp skip: failed to load '%s': %s." % [import_path, load_status])
		return false
	if not config.has_section_key("params", OPTION_PREVIEW_MAX_SPLATS):
		return false

	var saved_limit := int(config.get_value("params", OPTION_PREVIEW_MAX_SPLATS, 10000))
	var clamped_limit := clampi(saved_limit, 0, max_valid_limit)
	print("[godot-gsplat] Clamp check: source='%s', saved=%s, max_valid=%s, result=%s." % [source_path, saved_limit, max_valid_limit, clamped_limit])
	if clamped_limit == saved_limit:
		return false

	config.set_value("params", OPTION_PREVIEW_MAX_SPLATS, clamped_limit)
	var save_status := config.save(import_path)
	if save_status != OK:
		push_warning("Failed to save clamped Gaussian splat preview limit for '%s': %s" % [source_path, save_status])
		return false
	print("[godot-gsplat] Clamped %s from %s to %s for '%s'." % [OPTION_PREVIEW_MAX_SPLATS, saved_limit, clamped_limit, source_path])
	return true

func _read_imported_point_count(source_path: String) -> int:
	# Bypass the cache so we read the scene just reimported, not a stale instance.
	var packed_scene := ResourceLoader.load(source_path, "", ResourceLoader.CACHE_MODE_IGNORE)
	if packed_scene == null or not packed_scene is PackedScene:
		return -1

	var root: Node = packed_scene.instantiate()
	if root == null:
		return -1

	var point_count := _find_first_splat_point_count(root)
	root.queue_free()
	return point_count

func _find_first_splat_point_count(node: Node) -> int:
	# The decoded asset is not serialized into the .scn, so prefer the persisted
	# imported_point_count; fall back to the live asset for freshly generated scenes.
	if node.has_method("get_imported_point_count"):
		var stored_count := int(node.call("get_imported_point_count"))
		if stored_count > 0:
			return stored_count
	if node.has_method("get_asset"):
		var asset: Object = node.call("get_asset")
		if asset != null and asset.has_method("get_point_count"):
			return int(asset.call("get_point_count"))

	for child in node.get_children():
		var point_count := _find_first_splat_point_count(child)
		if point_count >= 0:
			return point_count

	return -1
