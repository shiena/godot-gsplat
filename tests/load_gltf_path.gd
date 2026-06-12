extends SceneTree

const TestUtil := preload("res://tests/gsplat_test_util.gd")
const DEFAULT_SAMPLE_PATH := "res://samples/minimal_gsplat/minimal_point.gltf"

var _gltf_extension: Object

func _initialize() -> void:
	_gltf_extension = TestUtil.register_gltf_extension()
	if _gltf_extension == null:
		push_error("Load test could not register the GLTF extension.")
		quit(1)
		return

	var sample_path := _get_sample_path()
	var root := TestUtil.generate_scene_from_gltf(sample_path)
	if root == null:
		quit(1)
		return

	var splat_node := TestUtil.find_first_splat_node(root)
	if splat_node == null:
		push_error("Generated scene does not contain a GaussianSplatNode3D.")
		quit(1)
		return

	var asset: Object = splat_node.get_asset()
	if asset == null:
		push_error("Generated GaussianSplatNode3D is missing its asset.")
		quit(1)
		return

	var point_count: int = asset.get_point_count()
	var payload: PackedByteArray = asset.get_payload()
	if point_count <= 0:
		push_error("Expected positive point count, got %s." % [point_count])
		quit(1)
		return
	if payload.is_empty():
		push_error("Expected decoded payload for '%s'." % [sample_path])
		quit(1)
		return
	var backend_model: Dictionary = splat_node.export_backend_model()
	if not backend_model.get("asset_ready", false):
		push_error("Expected backend model to report asset_ready.")
		quit(1)
		return
	if backend_model.get("asset_point_count", 0) != point_count:
		push_error("Backend model point count mismatch: %s != %s." % [backend_model.get("asset_point_count", 0), point_count])
		quit(1)
		return
	if String(backend_model.get("pipeline", "")).is_empty():
		push_error("Expected backend model to resolve a pipeline.")
		quit(1)
		return
	var splat_multimesh: MultiMeshInstance3D = splat_node.get_node_or_null("SplatMultiMesh")
	if splat_multimesh == null:
		push_error("Generated GaussianSplatNode3D is missing its SplatMultiMesh child.")
		quit(1)
		return
	if splat_multimesh.multimesh == null or splat_multimesh.multimesh.mesh == null:
		push_error("SplatMultiMesh child is missing its multimesh or quad mesh.")
		quit(1)
		return
	if not _has_editor_property(splat_node, "preview_max_splats"):
		push_error("GaussianSplatNode3D is missing inspector-editable property preview_max_splats.")
		quit(1)
		return
	if not _has_editor_property(splat_node, "show_all_preview_splats_action"):
		push_error("GaussianSplatNode3D is missing inspector action property show_all_preview_splats_action.")
		quit(1)
		return
	if _has_editor_property(splat_multimesh, "preview_max_splats"):
		push_error("SplatMultiMesh must not expose preview_max_splats; edit GaussianSplatNode3D or import options instead.")
		quit(1)
		return
	if _has_editor_property(splat_multimesh, "show_all_preview_splats_action"):
		push_error("SplatMultiMesh must not expose show_all_preview_splats_action; edit GaussianSplatNode3D or import options instead.")
		quit(1)
		return
	var expected_initial_preview_max_splats := _get_expected_initial_preview_max_splats()
	if expected_initial_preview_max_splats >= 0 and splat_node.get("preview_max_splats") != expected_initial_preview_max_splats:
		push_error("Expected initial preview_max_splats %s, got %s." % [expected_initial_preview_max_splats, splat_node.get("preview_max_splats")])
		quit(1)
		return
	splat_node.set("preview_max_splats", point_count + 1)
	if splat_node.get("preview_max_splats") != point_count:
		push_error("preview_max_splats did not clamp to point_count.")
		quit(1)
		return
	splat_node.set("preview_max_splats", -1)
	if splat_node.get("preview_max_splats") != 0:
		push_error("preview_max_splats did not clamp to lower bound 0.")
		quit(1)
		return
	splat_node.set("show_all_preview_splats_action", true)
	if splat_node.get("preview_max_splats") != point_count:
		push_error("Show All Preview Splats button did not set preview_max_splats to point_count.")
		quit(1)
		return
	var preview_target: int = mini(point_count, 100)
	splat_node.set("preview_max_splats", preview_target)
	if splat_node.get("preview_max_splats") != preview_target:
		push_error("preview_max_splats property did not round-trip through GaussianSplatNode3D.")
		quit(1)
		return
	# The rebuild swaps in a fresh MultiMesh, so re-read it from the node.
	# Chunked assets rebuild from the current chunk selection (the preview
	# budget only applies on the next camera-driven selection update), so the
	# strict instance count only holds for non-chunked assets; otherwise the
	# count must stay within [1, point_count].
	var preview_multimesh: MultiMesh = splat_multimesh.multimesh
	if preview_multimesh == null:
		push_error("SplatMultiMesh lost its MultiMesh after property update.")
		quit(1)
		return
	var is_chunked: bool = sample_path != DEFAULT_SAMPLE_PATH
	if is_chunked:
		if preview_multimesh.instance_count < 1 or preview_multimesh.instance_count > point_count:
			push_error("Expected 1..%s multimesh instances after property update, got %s." % [point_count, preview_multimesh.instance_count])
			quit(1)
			return
	elif preview_multimesh.instance_count != preview_target:
		push_error("Expected %s multimesh instances after property update, got %s." % [preview_target, preview_multimesh.instance_count])
		quit(1)
		return

	print("[load_gltf_path] path = %s" % [sample_path])
	print("[load_gltf_path] metadata summary = %s" % [asset.get_metadata_summary()])
	print("[load_gltf_path] payload layout = %s" % [asset.get_payload_layout()])
	print("[load_gltf_path] payload bytes = %s" % [payload.size()])
	print("[load_gltf_path] backend model pipeline = %s" % [backend_model.get("pipeline", "")])
	print("[load_gltf_path] SplatMultiMesh instances = %s" % [splat_multimesh.multimesh.instance_count])

	root.queue_free()
	root = null
	asset = null
	splat_node = null
	TestUtil.unregister_gltf_extension(_gltf_extension)
	_gltf_extension = null
	await process_frame
	quit()

func _get_sample_path() -> String:
	for argument in OS.get_cmdline_user_args():
		if argument.begins_with("--gltf-path="):
			var sample_path := argument.trim_prefix("--gltf-path=")
			var temporary_preview_max_splats := _get_temporary_import_preview_max_splats()
			if temporary_preview_max_splats >= 0:
				return _make_temporary_import_sample(sample_path, temporary_preview_max_splats)
			return sample_path
	return DEFAULT_SAMPLE_PATH

func _get_expected_initial_preview_max_splats() -> int:
	for argument in OS.get_cmdline_user_args():
		if argument.begins_with("--expected-initial-preview-max-splats="):
			return argument.trim_prefix("--expected-initial-preview-max-splats=").to_int()
	return -1

func _get_temporary_import_preview_max_splats() -> int:
	for argument in OS.get_cmdline_user_args():
		if argument.begins_with("--temporary-import-preview-max-splats="):
			return argument.trim_prefix("--temporary-import-preview-max-splats=").to_int()
	return -1

func _make_temporary_import_sample(source_path: String, preview_max_splats: int) -> String:
	var source_text := FileAccess.get_file_as_string(source_path)
	if source_text.is_empty():
		push_error("Failed to read temporary glTF source '%s'." % [source_path])
		quit(1)
		return source_path

	var temporary_path := "user://godot_gsplat_import_preview_test.gltf"
	var temporary_file := FileAccess.open(temporary_path, FileAccess.WRITE)
	if temporary_file == null:
		push_error("Failed to create temporary glTF '%s'." % [temporary_path])
		quit(1)
		return source_path
	temporary_file.store_string(source_text)
	temporary_file = null

	var source_bin_path := source_path.get_base_dir().path_join(source_path.get_file().get_basename() + ".bin")
	var source_bin_bytes := FileAccess.get_file_as_bytes(source_bin_path)
	if source_bin_bytes.is_empty():
		push_error("Failed to read temporary glTF buffer '%s'." % [source_bin_path])
		quit(1)
		return source_path
	var temporary_bin_path := "user://%s" % [source_bin_path.get_file()]
	var temporary_bin_file := FileAccess.open(temporary_bin_path, FileAccess.WRITE)
	if temporary_bin_file == null:
		push_error("Failed to create temporary glTF buffer '%s'." % [temporary_bin_path])
		quit(1)
		return source_path
	temporary_bin_file.store_buffer(source_bin_bytes)
	temporary_bin_file = null

	var temporary_import_file := FileAccess.open(temporary_path + ".import", FileAccess.WRITE)
	if temporary_import_file == null:
		push_error("Failed to create temporary import settings '%s'." % [temporary_path + ".import"])
		quit(1)
		return source_path
	temporary_import_file.store_string("[params]\n")
	temporary_import_file.store_string("gsplat/preview_max_splats=%s\n" % [preview_max_splats])
	temporary_import_file.store_string("gsplat/preview_max_splat_radius=0.02\n")
	temporary_import_file.store_string("gsplat/preview_scale_multiplier=1.0\n")
	temporary_import_file = null

	return temporary_path

func _has_editor_property(object: Object, property_name: String) -> bool:
	for property_info in object.get_property_list():
		if property_info.get("name", "") == property_name:
			return (property_info.get("usage", 0) & PROPERTY_USAGE_EDITOR) != 0
	return false
