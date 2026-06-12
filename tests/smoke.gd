extends SceneTree

const TestUtil := preload("res://tests/gsplat_test_util.gd")
const ASSET_CLASS_NAME := "GaussianSplatAsset"
const BACKEND_CLASS_NAME := "GaussianSplatBackendSettings"
const CLOUD_SETTINGS_CLASS_NAME := "GaussianSplatCloudSettings"
const NODE_CLASS_NAME := "GaussianSplatNode3D"

var _gltf_extension: Object

func _initialize() -> void:
	_gltf_extension = TestUtil.register_gltf_extension()
	if _gltf_extension == null:
		push_error("Smoke test could not register the GLTF extension.")
		quit(1)
		return

	var asset: Object = ClassDB.instantiate(ASSET_CLASS_NAME)
	var backend: Object = ClassDB.instantiate(BACKEND_CLASS_NAME)
	var cloud_settings: Object = ClassDB.instantiate(CLOUD_SETTINGS_CLASS_NAME)
	var splat_node: Object = ClassDB.instantiate(NODE_CLASS_NAME)

	if asset == null or backend == null or cloud_settings == null or splat_node == null:
		push_error("Smoke test failed to instantiate one or more GDExtension classes.")
		quit(1)
		return

	backend.apply_vr_safe_defaults()
	asset.initialize_from_import(_build_demo_metadata())
	splat_node.bind_backend_settings(backend)
	splat_node.bind_asset(asset)

	var backend_model: Dictionary = splat_node.export_backend_model()
	var backend_settings: Dictionary = backend_model.get("backend_settings", {})
	if backend_model.get("pipeline", "") != "vr_safe_spatial":
		push_error("Expected vr_safe_spatial pipeline, got %s." % [backend_model.get("pipeline", "")])
		quit(1)
		return
	if backend_settings.get("allow_compositor", true):
		push_error("VR-safe backend settings must not allow compositor rendering.")
		quit(1)
		return
	if backend_settings.get("vr_view_basis", "") != "head_center":
		push_error("Expected head_center VR view basis, got %s." % [backend_settings.get("vr_view_basis", "")])
		quit(1)
		return
	if not cloud_settings.is_render_enabled():
		push_error("Splat render should be enabled by default.")
		quit(1)
		return
	# i32::MAX = "show every splat"; the node clamps it to the asset point count on load.
	if cloud_settings.get_max_preview_splats() != 2147483647:
		push_error("Splat budget should default to showing every splat, got %s." % [cloud_settings.get_max_preview_splats()])
		quit(1)
		return
	if cloud_settings.get_max_preview_splat_radius() > 0.02:
		push_error("Preview splat radius cap is too high: %s." % [cloud_settings.get_max_preview_splat_radius()])
		quit(1)
		return

	print("[smoke] backend model = %s" % [backend_model])
	print("[smoke] metadata summary = %s" % [splat_node.get_metadata_summary()])

	# A node with no injected settings must run on class defaults and must not
	# materialize settings resources until one is written to.
	var bare_node: Object = ClassDB.instantiate(NODE_CLASS_NAME)
	var bare_asset: Object = ClassDB.instantiate(ASSET_CLASS_NAME)
	bare_asset.initialize_from_import(_build_demo_metadata())
	bare_node.bind_asset(bare_asset)
	if bare_node.get_cloud_settings() != null or bare_node.get_backend_settings() != null:
		push_error("Binding an asset must not auto-create settings resources.")
		quit(1)
		return
	var bare_model: Dictionary = bare_node.export_backend_model()
	if bare_model.get("pipeline", "") != "desktop_direct":
		push_error("Expected default pipeline desktop_direct, got %s." % [bare_model.get("pipeline", "")])
		quit(1)
		return
	bare_node.set_preview_max_splats(4)
	if bare_node.get_cloud_settings() == null:
		push_error("Writing a preview setting should lazily create cloud settings.")
		quit(1)
		return
	if bare_node.get_preview_max_splats() != 4:
		push_error("Expected preview_max_splats 4 after write, got %s." % [bare_node.get_preview_max_splats()])
		quit(1)
		return
	bare_node.free()
	bare_asset = null

	# The XR preset (render_profile 4) does not pin an absolute splat count;
	# it scales the budget by the asset's spatial extent between 300k and 800k,
	# then clamps to the point count. A building-scale extent set before the asset
	# is bound must reach the ceiling once the asset arrives (exercises the
	# refresh_from_asset recompute path).
	var vr_big_node: Object = ClassDB.instantiate(NODE_CLASS_NAME)
	var vr_big_asset: Object = ClassDB.instantiate(ASSET_CLASS_NAME)
	var big_metadata: Dictionary = _build_demo_metadata()
	big_metadata["point_count"] = 2_000_000
	vr_big_asset.initialize_from_import(big_metadata)
	vr_big_asset.set_local_aabb(AABB(Vector3.ZERO, Vector3(40.0, 15.0, 40.0)))
	vr_big_node.set("render_profile", 4) # RenderProfile::XR, before the asset binds
	vr_big_node.bind_asset(vr_big_asset)
	if vr_big_node.get_preview_max_splats() != 800000:
		push_error("XR building-scale budget should reach the 800k ceiling, got %s." % [vr_big_node.get_preview_max_splats()])
		quit(1)
		return
	if vr_big_node.get_sh_degree() != 1:
		push_error("XR should use SH degree 1, got %s." % [vr_big_node.get_sh_degree()])
		quit(1)
		return
	var vr_big_model: Dictionary = vr_big_node.export_backend_model()
	if vr_big_model.get("pipeline", "") != "vr_safe_spatial":
		push_error("XR should resolve the vr_safe_spatial pipeline, got %s." % [vr_big_model.get("pipeline", "")])
		quit(1)
		return
	var vr_big_settings: Dictionary = vr_big_model.get("backend_settings", {})
	if vr_big_settings.get("vr_view_basis", "") != "head_center":
		push_error("XR should use the head_center VR view basis, got %s." % [vr_big_settings.get("vr_view_basis", "")])
		quit(1)
		return
	vr_big_node.free()
	vr_big_asset = null

	# A tabletop-scale extent set on an already-bound asset must stay at the 300k
	# floor (exercises the apply_render_profile adaptive path).
	var vr_small_node: Object = ClassDB.instantiate(NODE_CLASS_NAME)
	var vr_small_asset: Object = ClassDB.instantiate(ASSET_CLASS_NAME)
	var small_metadata: Dictionary = _build_demo_metadata()
	small_metadata["point_count"] = 2_000_000
	vr_small_asset.initialize_from_import(small_metadata)
	vr_small_asset.set_local_aabb(AABB(Vector3.ZERO, Vector3(0.5, 0.5, 0.5)))
	vr_small_node.bind_asset(vr_small_asset)
	vr_small_node.set("render_profile", 4) # RenderProfile::XR, after the asset binds
	if vr_small_node.get_preview_max_splats() != 300000:
		push_error("XR tabletop-scale budget should stay at the 300k floor, got %s." % [vr_small_node.get_preview_max_splats()])
		quit(1)
		return
	vr_small_node.free()
	vr_small_asset = null

	# resolve_pipeline matches an explicit backend profile before the target hint,
	# so a profile pinned by apply_mobile_defaults would silently defeat XR's
	# vr_safe pipeline unless the preset resets the profile to automatic.
	var vr_pinned_node: Object = ClassDB.instantiate(NODE_CLASS_NAME)
	var vr_pinned_asset: Object = ClassDB.instantiate(ASSET_CLASS_NAME)
	var vr_pinned_backend: Object = ClassDB.instantiate(BACKEND_CLASS_NAME)
	vr_pinned_backend.apply_mobile_defaults() # pins profile = "mobile"
	vr_pinned_asset.initialize_from_import(_build_demo_metadata())
	vr_pinned_node.bind_backend_settings(vr_pinned_backend)
	vr_pinned_node.bind_asset(vr_pinned_asset)
	vr_pinned_node.set("render_profile", 4) # RenderProfile::XR
	var vr_pinned_model: Dictionary = vr_pinned_node.export_backend_model()
	if vr_pinned_model.get("pipeline", "") != "vr_safe_spatial":
		push_error("XR after apply_mobile_defaults should still resolve vr_safe_spatial, got %s." % [vr_pinned_model.get("pipeline", "")])
		quit(1)
		return
	var vr_pinned_settings: Dictionary = vr_pinned_model.get("backend_settings", {})
	if vr_pinned_settings.get("profile", "") != "automatic":
		push_error("Render presets should reset the backend profile to automatic, got %s." % [vr_pinned_settings.get("profile", "")])
		quit(1)
		return
	vr_pinned_node.free()
	vr_pinned_asset = null
	vr_pinned_backend = null

	# A preset selected before any asset binds must not clamp its budget against
	# the missing asset (point_count 0); apply_render_profile skips the budget
	# write without an asset and refresh_from_asset re-applies the preset on bind.
	var pre_node: Object = ClassDB.instantiate(NODE_CLASS_NAME)
	var pre_asset: Object = ClassDB.instantiate(ASSET_CLASS_NAME)
	var pre_metadata: Dictionary = _build_demo_metadata()
	pre_metadata["point_count"] = 2_000_000
	pre_asset.initialize_from_import(pre_metadata)
	pre_node.set("render_profile", 1) # RenderProfile::Low, before the asset binds
	pre_node.bind_asset(pre_asset)
	if pre_node.get_preview_max_splats() != 150000:
		push_error("Low preset selected pre-bind should re-apply to 150k on bind, got %s." % [pre_node.get_preview_max_splats()])
		quit(1)
		return
	if pre_node.get("render_profile") != 1:
		push_error("Pre-bind preset should survive the asset bind, got profile %s." % [pre_node.get("render_profile")])
		quit(1)
		return
	pre_node.free()
	pre_asset = null

	splat_node.free()
	asset = null
	backend = null
	cloud_settings = null
	splat_node = null
	TestUtil.unregister_gltf_extension(_gltf_extension)
	_gltf_extension = null
	await process_frame
	quit()

func _build_demo_metadata() -> Dictionary:
	return {
		"source_extension": "KHR_gaussian_splatting",
		"node_index": 0,
		"mesh_index": 0,
		"primitive_index": 0,
		"projection": "perspective",
		"sorting_method": "cameraDistance",
		"kernel": "ellipse",
		"color_space": "srgb",
		"point_count": 8,
		"has_color_fallback": true,
		"fallback_mode": "color_points",
		"validation_errors": PackedStringArray(),
		"validation_warnings": PackedStringArray(),
		"raw_extensions": {},
	}
