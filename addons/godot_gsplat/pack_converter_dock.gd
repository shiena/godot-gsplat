@tool
extends VBoxContainer

const GltfRegistration := preload("res://addons/godot_gsplat/runtime/gltf_registration.gd")
const NODE_CLASS_NAME := "GaussianSplatNode3D"

var _source_edit: LineEdit
var _output_edit: LineEdit
var _convert_button: Button
var _status_label: Label
var _log: TextEdit
var _source_dialog: EditorFileDialog
var _output_dialog: EditorFileDialog
var _convert_thread: Thread

func _ready() -> void:
	name = "GSplat Pack Converter"
	custom_minimum_size = Vector2(360.0, 260.0)
	_build_ui()

func _notification(what: int) -> void:
	if what == NOTIFICATION_PREDELETE:
		_join_convert_thread()

func _build_ui() -> void:
	var title := Label.new()
	title.text = "Convert glTF to .gsplatpack"
	title.add_theme_font_size_override("font_size", 16)
	add_child(title)

	_source_edit = _add_path_row("Source glTF", "res://path/to/source.gltf", _browse_source)
	_output_edit = _add_path_row("Output pack", "res://path/to/source.gsplatpack", _browse_output)

	var button_row := HBoxContainer.new()
	add_child(button_row)

	var default_button := Button.new()
	default_button.text = "Use Default Output"
	default_button.pressed.connect(_use_default_output)
	button_row.add_child(default_button)

	_convert_button = Button.new()
	_convert_button.text = "Convert"
	_convert_button.pressed.connect(_start_convert)
	button_row.add_child(_convert_button)

	_status_label = Label.new()
	_status_label.text = "Ready."
	_status_label.autowrap_mode = TextServer.AUTOWRAP_WORD_SMART
	add_child(_status_label)

	_log = TextEdit.new()
	_log.editable = false
	_log.wrap_mode = TextEdit.LINE_WRAPPING_BOUNDARY
	_log.custom_minimum_size = Vector2(0.0, 120.0)
	_log.size_flags_vertical = Control.SIZE_EXPAND_FILL
	add_child(_log)

	_source_dialog = EditorFileDialog.new()
	_source_dialog.access = EditorFileDialog.ACCESS_FILESYSTEM
	_source_dialog.file_mode = EditorFileDialog.FILE_MODE_OPEN_FILE
	_source_dialog.title = "Select Gaussian splat glTF"
	_source_dialog.add_filter("*.gltf, *.glb ; glTF files")
	_source_dialog.file_selected.connect(_on_source_selected)
	add_child(_source_dialog)

	_output_dialog = EditorFileDialog.new()
	_output_dialog.access = EditorFileDialog.ACCESS_FILESYSTEM
	_output_dialog.file_mode = EditorFileDialog.FILE_MODE_SAVE_FILE
	_output_dialog.title = "Save .gsplatpack"
	_output_dialog.add_filter("*.gsplatpack ; Gaussian splat pack")
	_output_dialog.file_selected.connect(_on_output_selected)
	add_child(_output_dialog)

func _add_path_row(label_text: String, placeholder: String, browse_callback: Callable) -> LineEdit:
	var label := Label.new()
	label.text = label_text
	add_child(label)

	var row := HBoxContainer.new()
	add_child(row)

	var edit := LineEdit.new()
	edit.placeholder_text = placeholder
	edit.size_flags_horizontal = Control.SIZE_EXPAND_FILL
	row.add_child(edit)

	var browse := Button.new()
	browse.text = "Browse"
	browse.pressed.connect(browse_callback)
	row.add_child(browse)
	return edit

func _browse_source() -> void:
	_source_dialog.popup_file_dialog()

func _browse_output() -> void:
	if not _output_edit.text.strip_edges().is_empty():
		_output_dialog.current_path = _output_edit.text.strip_edges()
	elif not _source_edit.text.strip_edges().is_empty():
		_output_dialog.current_path = _default_output_path(_source_edit.text.strip_edges())
	_output_dialog.popup_file_dialog()

func _on_source_selected(path: String) -> void:
	path = _resource_path_if_inside_project(path)
	_source_edit.text = path
	if _output_edit.text.strip_edges().is_empty():
		_output_edit.text = _default_output_path(path)

func _on_output_selected(path: String) -> void:
	_output_edit.text = _ensure_pack_extension(_resource_path_if_inside_project(path))

func _use_default_output() -> void:
	var source := _source_edit.text.strip_edges()
	if source.is_empty():
		_set_status("Select a source glTF first.", true)
		return
	_output_edit.text = _default_output_path(source)

func _start_convert() -> void:
	if _convert_thread != null:
		_set_status("Conversion is already running.", true)
		return

	var source := _source_edit.text.strip_edges()
	var output := _output_edit.text.strip_edges()
	if source.is_empty():
		_set_status("Source glTF is empty.", true)
		return
	if output.is_empty():
		output = _default_output_path(source)
		_output_edit.text = output
	output = _ensure_pack_extension(output)
	_output_edit.text = output

	if not _is_gltf_path(source):
		_set_status("Source must be a .gltf or .glb file.", true)
		return
	if not FileAccess.file_exists(source):
		_set_status("Source file does not exist: %s" % source, true)
		return
	if source == output:
		_set_status("Output path must differ from the source path.", true)
		return
	if not _ensure_output_parent(output):
		return
	if not GltfRegistration.ensure_extension_library_loaded():
		_set_status("Failed to load the godot-gsplat GDExtension library.", true)
		return
	if not ClassDB.class_exists(NODE_CLASS_NAME):
		_set_status("GDExtension class '%s' is not available." % NODE_CLASS_NAME, true)
		return

	_set_busy(true)
	_append_log("Converting %s -> %s" % [source, output])
	_convert_thread = Thread.new()
	var start_status := _convert_thread.start(_convert_blocking.bind(source, output))
	if start_status != OK:
		_join_convert_thread()
		_set_busy(false)
		_set_status("Failed to start converter thread: %s" % start_status, true)

# Runs on the worker thread.
func _convert_blocking(source: String, output: String) -> void:
	var ok := false
	var message := ""
	var node: Object = ClassDB.instantiate(NODE_CLASS_NAME)
	if node == null:
		message = "Failed to instantiate %s." % NODE_CLASS_NAME
	else:
		ok = bool(node.call("write_gsplat_pack_from_gltf", source, output))
		node.free()
		message = "Wrote %s" % output if ok else "Conversion failed."
	_on_convert_finished.call_deferred(ok, message, output)

func _on_convert_finished(ok: bool, message: String, output: String) -> void:
	_join_convert_thread()
	_set_busy(false)
	_set_status(message, not ok)
	if ok:
		_append_log(message)
		var fs := EditorInterface.get_resource_filesystem()
		if fs != null and output.begins_with("res://"):
			fs.scan()
	else:
		_append_log("Error: %s" % message)

func _join_convert_thread() -> void:
	if _convert_thread != null and _convert_thread.is_started():
		_convert_thread.wait_to_finish()
	_convert_thread = null

func _set_busy(busy: bool) -> void:
	_convert_button.disabled = busy
	_status_label.text = "Converting..." if busy else "Ready."

func _set_status(message: String, is_error: bool) -> void:
	_status_label.text = message
	if is_error:
		push_warning("[godot-gsplat] %s" % message)

func _append_log(message: String) -> void:
	if _log == null:
		return
	_log.text += "%s\n" % message
	_log.set_caret_line(_log.get_line_count())

func _default_output_path(source: String) -> String:
	return "%s.gsplatpack" % source.get_basename()

func _ensure_pack_extension(path: String) -> String:
	return path if path.get_extension().to_lower() == "gsplatpack" else "%s.gsplatpack" % path

func _is_gltf_path(path: String) -> bool:
	var extension := path.get_extension().to_lower()
	return extension == "gltf" or extension == "glb"

func _ensure_output_parent(path: String) -> bool:
	var global_path := ProjectSettings.globalize_path(path) if path.begins_with("res://") or path.begins_with("user://") else path
	var parent_dir := global_path.get_base_dir()
	if parent_dir.is_empty() or DirAccess.dir_exists_absolute(parent_dir):
		return true
	var status := DirAccess.make_dir_recursive_absolute(parent_dir)
	if status != OK:
		_set_status("Failed to create output directory '%s': %s" % [parent_dir, status], true)
		return false
	return true

func _resource_path_if_inside_project(path: String) -> String:
	if path.begins_with("res://") or path.begins_with("user://"):
		return path
	var normalized_path := path.replace("\\", "/")
	var project_root := ProjectSettings.globalize_path("res://").replace("\\", "/").trim_suffix("/")
	var project_prefix := "%s/" % project_root
	if normalized_path.to_lower().begins_with(project_prefix.to_lower()):
		return "res://%s" % normalized_path.substr(project_prefix.length())
	return path
