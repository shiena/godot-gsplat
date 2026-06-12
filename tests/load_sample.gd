extends SceneTree

const TestUtil := preload("res://tests/gsplat_test_util.gd")
const SAMPLE_PATH := "res://samples/minimal_gsplat/minimal_point.gltf"

var _gltf_extension: Object

func _initialize() -> void:
	_gltf_extension = TestUtil.register_gltf_extension()
	if _gltf_extension == null:
		push_error("Load test could not register the GLTF extension.")
		quit(1)
		return

	var root := TestUtil.generate_scene_from_gltf(SAMPLE_PATH)
	if root == null:
		quit(1)
		return

	var splat_node := TestUtil.find_first_splat_node(root)
	if splat_node == null:
		push_error("Generated scene does not contain a GaussianSplatNode3D.")
		quit(1)
		return

	print("[load] node class = %s" % [splat_node.get_class()])
	if splat_node.has_method("get_metadata_summary"):
		print("[load] node metadata summary = %s" % [splat_node.get_metadata_summary()])

	var asset: Object = splat_node.get_asset()
	if asset == null:
		push_error("Generated GaussianSplatNode3D is missing its asset.")
		quit(1)
		return

	var point_count: int = asset.get_point_count()
	var payload: PackedByteArray = asset.get_payload()
	if point_count != 1:
		push_error("Expected point_count == 1, got %s." % [point_count])
		quit(1)
		return
	if payload.size() != 72:
		push_error("Expected payload size == 72, got %s." % [payload.size()])
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
	if splat_multimesh.multimesh.mesh.get_surface_count() != 1:
		push_error("Expected SplatMultiMesh quad to have 1 surface, got %s." % [splat_multimesh.multimesh.mesh.get_surface_count()])
		quit(1)
		return
	if splat_multimesh.multimesh.instance_count != point_count:
		push_error("Expected %s multimesh instances, got %s." % [point_count, splat_multimesh.multimesh.instance_count])
		quit(1)
		return

	print("[load] metadata summary = %s" % [asset.get_metadata_summary()])
	print("[load] payload layout = %s" % [asset.get_payload_layout()])
	print("[load] payload bytes = %s" % [payload.size()])
	print("[load] local aabb = %s" % [asset.get_local_aabb()])
	print("[load] backend model pipeline = %s" % [backend_model.get("pipeline", "")])
	print("[load] SplatMultiMesh instances = %s" % [splat_multimesh.multimesh.instance_count])

	root.queue_free()
	root = null
	asset = null
	splat_node = null
	TestUtil.unregister_gltf_extension(_gltf_extension)
	_gltf_extension = null
	await process_frame
	quit()
