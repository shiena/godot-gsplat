extends SceneTree

const TestUtil := preload("res://tests/gsplat_test_util.gd")
const OPTION_PREVIEW_MAX_SPLATS := "gsplat/preview_max_splats"

var _gltf_extension: Object

func _initialize() -> void:
	_gltf_extension = TestUtil.register_gltf_extension()
	if _gltf_extension == null:
		push_error("Reimport test could not register the GLTF extension.")
		quit(1)
		return
	var plugin := preload("res://addons/godot_gsplat/plugin.gd").new()
	await process_frame
	await process_frame

	var gltf_path := _get_argument("--gltf-path=", "res://samples/converted/demo_1k.gltf")
	var preview_max_splats := _get_argument("--preview-max-splats=", "100000000000").to_int()
	var expected_preview_max_splats := _get_argument("--expected-preview-max-splats=", "1000").to_int()

	var import_path := gltf_path + ".import"
	var config := ConfigFile.new()
	var load_status := config.load(import_path)
	if load_status != OK:
		push_error("Failed to load import settings '%s': %s." % [import_path, load_status])
		quit(1)
		return

	config.set_value("params", OPTION_PREVIEW_MAX_SPLATS, preview_max_splats)
	var save_status := config.save(import_path)
	if save_status != OK:
		push_error("Failed to save import settings '%s': %s." % [import_path, save_status])
		quit(1)
		return
	print("[reimport_gltf_clamp] wrote %s=%s" % [OPTION_PREVIEW_MAX_SPLATS, preview_max_splats])

	var resource_filesystem := EditorInterface.get_resource_filesystem()
	resource_filesystem.scan()
	while resource_filesystem.is_scanning():
		await process_frame
	resource_filesystem.update_file(gltf_path)
	await process_frame
	resource_filesystem.reimport_files(PackedStringArray([gltf_path]))
	for frame in 60:
		await process_frame
	plugin.call("_clamp_import_preview_limit", gltf_path)

	var verify_config := ConfigFile.new()
	var verify_status := verify_config.load(import_path)
	if verify_status != OK:
		push_error("Failed to reload import settings '%s': %s." % [import_path, verify_status])
		quit(1)
		return

	var actual_preview_max_splats := int(verify_config.get_value("params", OPTION_PREVIEW_MAX_SPLATS, -1))
	print("[reimport_gltf_clamp] final %s=%s" % [OPTION_PREVIEW_MAX_SPLATS, actual_preview_max_splats])
	if actual_preview_max_splats != expected_preview_max_splats:
		push_error("Expected %s after reimport, got %s." % [expected_preview_max_splats, actual_preview_max_splats])
		TestUtil.unregister_gltf_extension(_gltf_extension)
		quit(1)
		return

	TestUtil.unregister_gltf_extension(_gltf_extension)
	_gltf_extension = null
	quit()

func _get_argument(prefix: String, fallback: String) -> String:
	for argument in OS.get_cmdline_user_args():
		if argument.begins_with(prefix):
			return argument.trim_prefix(prefix)
	return fallback
