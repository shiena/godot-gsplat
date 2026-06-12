extends SceneTree

# Verifies the pre-imported .scn path: a GaussianSplatNode3D whose baked
# SplatMultiMesh child was serialized, but whose decoded asset was not, must
# reconnect to the baked render on ready() (adopt_serialized_render) and
# report renderable data. This is the standard runtime path for content
# imported in the editor, so it must keep working without a live asset.

const TestUtil := preload("res://tests/gsplat_test_util.gd")
const SAMPLE_PATH := "res://samples/minimal_gsplat/minimal_point.gltf"
const ROUNDTRIP_PATH := "user://gsplat_roundtrip_test.scn"

var _gltf_extension: Object

func _initialize() -> void:
	_gltf_extension = TestUtil.register_gltf_extension()
	if _gltf_extension == null:
		push_error("Roundtrip test could not register the GLTF extension.")
		quit(1)
		return

	var generated := TestUtil.generate_scene_from_gltf(SAMPLE_PATH)
	if generated == null:
		quit(1)
		return
	var splat_node := TestUtil.find_first_splat_node(generated)
	if splat_node == null:
		push_error("Generated scene does not contain a GaussianSplatNode3D.")
		quit(1)
		return

	# Serialize like the editor import does: the baked children belong to the
	# packed scene, the decoded asset (a non-exported field) does not.
	for child in splat_node.get_children():
		child.owner = splat_node
	var packed := PackedScene.new()
	if packed.pack(splat_node) != OK:
		push_error("Failed to pack GaussianSplatNode3D.")
		quit(1)
		return
	if ResourceSaver.save(packed, ROUNDTRIP_PATH) != OK:
		push_error("Failed to save packed scene.")
		quit(1)
		return

	var loaded: PackedScene = ResourceLoader.load(ROUNDTRIP_PATH, "", ResourceLoader.CACHE_MODE_IGNORE)
	if loaded == null:
		push_error("Failed to reload packed scene.")
		quit(1)
		return
	var restored: Node = loaded.instantiate()
	if restored.call("has_asset"):
		push_error("Decoded asset must not be serialized into the .scn.")
		quit(1)
		return

	# Entering the tree runs ready() -> adopt_serialized_render().
	root.add_child(restored)
	await process_frame

	var runtime_state: Dictionary = restored.call("export_runtime_state")
	if not runtime_state.get("asset_ready", false):
		push_error("Restored node did not adopt its serialized render (asset_ready=false).")
		quit(1)
		return
	var multimesh_instance: MultiMeshInstance3D = restored.get_node_or_null("SplatMultiMesh")
	if multimesh_instance == null or multimesh_instance.multimesh == null:
		push_error("Restored node is missing its baked SplatMultiMesh.")
		quit(1)
		return
	if multimesh_instance.multimesh.instance_count != 1:
		push_error("Expected 1 baked instance, got %s." % [multimesh_instance.multimesh.instance_count])
		quit(1)
		return

	print("[scene_roundtrip] restored asset_ready=%s instances=%s" % [
		runtime_state.get("asset_ready"), multimesh_instance.multimesh.instance_count])

	restored.queue_free()
	generated.queue_free()
	TestUtil.unregister_gltf_extension(_gltf_extension)
	_gltf_extension = null
	await process_frame
	quit()
