extends Node3D

# Minimal glTF load demo. The glTF parse + splat decode of a large cloud can
# take many seconds, so it runs on a background thread: the main loop stays
# responsive and a status label is shown until the scene lands. Building a
# detached node tree on a thread is safe; only add_child happens on the main
# thread (via call_deferred).

const GltfRegistration := preload("res://addons/godot_gsplat/runtime/gltf_registration.gd")
const NODE_CLASS_NAME := "GaussianSplatNode3D"

## The splat glTF or prebuilt *.gsplatpack to load from the inspector.
## The large room sample uses a prebuilt pack so Android/Quest can stream pages
## from user:// instead of holding the full decoded glTF payload in memory.
## Switch to demo_runtime.gltf (271k) for the lightweight bonsai.
@export_file("*.gltf", "*.glb", "*.gsplatpack") var sample_path: String = "res://samples/converted/scene.gsplatpack"

## Optional 3D loading indicator (e.g. a panel parented to the XR camera).
## Shown while the glTF loads and hidden once the splats actually render;
## a Label3D child named "Label3D" (if any) receives status messages.
@export_node_path("Node3D") var loading_panel_path: NodePath

## Chunk selection under a limited budget. "view_priority" keeps the initial view
## cone and full-distance range represented, then lowers density on distant /
## peripheral chunks if the target budget would otherwise be exceeded.
@export_enum("nearest", "coverage", "view_priority") var splat_chunk_selection: String = "view_priority"

@export_group("View Priority Streaming")

## Wide initial cone around the HMD/camera forward vector. 200 degrees gives room
## for immediate head rotation without loading the entire room at full density.
@export_range(1.0, 360.0, 1.0) var view_priority_fov_degrees: float = 200.0

## Chunks inside this local-space distance remain candidates; over-budget cases
## reduce density instead of shrinking this distance.
@export_range(0.1, 100.0, 0.1, "or_greater") var view_priority_full_distance: float = 5.0

## Minimum prefix kept for a candidate chunk when the full candidate set exceeds
## the target budget. Lower values preserve more chunks; higher values preserve
## more local detail per represented chunk.
@export_range(1, 16384, 1, "or_greater") var view_priority_min_lod_per_chunk: int = 256

## Requested XR display refresh rate (0 = leave the runtime default). The splat
## workload misses 90 Hz on Quest 3 (App ≈14 ms vs the 11.1 ms budget), so the
## compositor reprojects every other frame — which shimmers on translucent
## splats. 72 Hz gives a 13.9 ms budget the workload actually fits.
@export var xr_refresh_rate: float = 72.0

## Render profile applied to the splat node after loading. Budget/SH only take
## effect on the live-decode (raw glTF) path; on the imported-scene path the
## render is baked, so only the backend settings apply (the node warns).
# Reprojection experiment (white-shimmer hunt): Middle (500k) should fit the
# 72 Hz budget (~11.3 ms est. vs 13.9), driving compositor reprojection to ~0.
@export_enum("Custom:0", "Low:1", "Middle:2", "High:3", "XR:4") var splat_render_profile: int = 2

# Per-field overrides on top of the selected render profile, applied via the
# node's get_profile_settings / apply_profile_settings round-trip: fetch the
# profile's resolved values, overwrite only the fields set below, re-apply. Leave
# a field at its sentinel ("profile_default" / 0 / -1) to keep the profile's
# value. On the imported-scene fallback only the depth-mode override takes
# visible effect — a baked render's budget/SH/target are fixed at import time.
@export_group("Profile Overrides")

## Backend platform target. "vr_safe" = spatial VR pipeline, "mobile" = mobile
## direct, "desktop" = desktop (clustered above the cluster threshold).
## "profile_default" keeps the profile's target.
@export_enum("profile_default", "desktop", "mobile", "vr_safe") var splat_target_hint_override: String = "vr_safe"

## Override the profile's splat budget (max rendered splats). When off, the
## profile's own budget is used (XR derives its from the asset extent) —
## budget is a free integer, so this toggle is how you select "profile default".
@export var splat_budget_override_enabled: bool = true

## Max rendered splats, used only when Splat Budget Override Enabled is on;
## clamped to the asset's point count.
@export_range(1000, 8000000, 1000, "or_greater") var splat_budget: int = 800000

## Spherical-harmonics degree: higher = more view-dependent color at more data
## texture / shader cost. -1 = use the profile's SH degree.
@export_enum("profile_default:-1", "SH0:0", "SH1:1", "SH2:2", "SH3:3") var splat_sh_degree_override: int = -1

## VR translucency sort basis. "head_center" sorts once for both eyes; "per_eye"
## sorts each eye independently (experimental, more expensive). "profile_default"
## keeps the profile's basis (all presets use head_center).
@export_enum("profile_default", "head_center", "per_eye") var splat_vr_view_basis_override: String = "profile_default"

## Splat depth mode. The per-corner ray-vs-ellipsoid depth path causes a steady
## VR white shimmer on high-alpha overlapping splats; "center" writes the
## splat-center depth instead and removes it (at some depth-fidelity cost).
## "profile_default" keeps the profile's choice (XR uses "center"; the rest
## use "ray"). Set "center" to e.g. try the High profile in VR without shimmer.
@export_enum("profile_default", "ray", "center") var splat_depth_mode_override: String = "profile_default"

var _gltf_extension: Object
var _loaded_scene: Node
var _load_thread: Thread
var _status_label: Label
var _loading_panel: Node3D

func _ready() -> void:
	_loading_panel = get_node_or_null(loading_panel_path) as Node3D
	_gltf_extension = GltfRegistration.register_gltf_extension()
	_ensure_environment()
	_apply_xr_refresh_rate()
	_start_loading()

# Request the configured XR refresh rate once the XR session is up. A request
# made before the session is running/focused is silently dropped by the
# runtime (observed on Quest: the app stayed at 90 Hz), so retry until the
# readback confirms the change.
func _apply_xr_refresh_rate() -> void:
	if xr_refresh_rate <= 0.0:
		return
	for i in range(600): # XR initializes a few frames in; give it ~10 s.
		if get_viewport().use_xr:
			break
		await get_tree().process_frame
	if not get_viewport().use_xr:
		return
	var xr_interface = XRServer.find_interface("OpenXR")
	if xr_interface == null or not xr_interface.is_initialized():
		return
	for attempt in range(30):
		xr_interface.display_refresh_rate = xr_refresh_rate
		await get_tree().create_timer(1.0).timeout
		if absf(xr_interface.display_refresh_rate - xr_refresh_rate) < 0.5:
			return

func _exit_tree() -> void:
	_join_load_thread()
	if is_instance_valid(_loaded_scene):
		_loaded_scene.queue_free()
		_loaded_scene = null
	if _gltf_extension != null:
		GltfRegistration.unregister_gltf_extension(_gltf_extension)
		_gltf_extension = null

func _ensure_environment() -> void:
	# The camera lives in the scene as an OrbitCamera (orbit_camera.gd).
	var light := DirectionalLight3D.new()
	light.rotation_degrees = Vector3(-45.0, 30.0, 0.0)
	add_child(light)

	var canvas := CanvasLayer.new()
	add_child(canvas)

	_status_label = Label.new()
	_status_label.position = Vector2(16.0, 16.0)
	_status_label.text = "godot-gsplat minimal glTF load demo"
	canvas.add_child(_status_label)

func _start_loading() -> void:
	if sample_path.is_empty():
		push_error("No sample glTF selected; set Sample Path in the inspector.")
		_status_label.text = "No sample glTF selected."
		_set_loading_panel_text("No sample glTF selected.")
		return
	_status_label.text = "Loading %s ..." % sample_path
	_set_loading_panel_visible(true)
	_set_loading_panel_text("Loading 3DGS ...")
	_load_thread = Thread.new()
	# Bind the path so the loader thread does not read node state.
	_load_thread.start(_load_scene_blocking.bind(_prepare_runtime_path(sample_path)))

func _prepare_runtime_path(path: String) -> String:
	if not path.ends_with(".gsplatpack") or not path.begins_with("res://"):
		return path
	var target := "user://%s" % path.get_file()
	var source := FileAccess.open(path, FileAccess.READ)
	if source == null:
		push_error("Failed to read gsplat pack from %s." % [path])
		return path
	var source_length := source.get_length()
	if FileAccess.file_exists(target):
		var existing := FileAccess.open(target, FileAccess.READ)
		var target_length := existing.get_length() if existing != null else -1
		var valid_magic := _has_gsplat_pack_magic(existing)
		existing = null
		if target_length == source_length and valid_magic:
			return ProjectSettings.globalize_path(target)
		push_warning("Replacing stale gsplat pack copy at %s (%d != %d bytes, magic=%s)." % [target, target_length, source_length, valid_magic])
	var file := FileAccess.open(target, FileAccess.WRITE)
	if file == null:
		push_error("Failed to create user copy of gsplat pack at %s." % [target])
		return path
	const COPY_BLOCK_BYTES := 1024 * 1024
	while source.get_position() < source.get_length():
		var remaining := source.get_length() - source.get_position()
		file.store_buffer(source.get_buffer(mini(COPY_BLOCK_BYTES, remaining)))
	file.flush()
	file = null
	source = null
	var copied := FileAccess.open(target, FileAccess.READ)
	if copied == null or copied.get_length() != source_length or not _has_gsplat_pack_magic(copied):
		push_error("Copied gsplat pack is invalid at %s." % [target])
		return path
	return ProjectSettings.globalize_path(target)

func _has_gsplat_pack_magic(file: FileAccess) -> bool:
	if file == null:
		return false
	file.seek(0)
	var magic := file.get_buffer(8)
	return magic.size() == 8 \
			and magic[0] == 71 \
			and magic[1] == 83 \
			and magic[2] == 80 \
			and magic[3] == 65 \
			and magic[4] == 67 \
			and magic[5] == 75 \
			and magic[6] == 49 \
			and magic[7] == 0

# Runs on the loader thread.
func _load_scene_blocking(path: String) -> void:
	# Parse the raw file when it exists (packed via a `keep`-imported copy);
	# otherwise instantiate the imported scene (decode happened at import time).
	if FileAccess.file_exists(path):
		# Build the splat node directly and select the render profile BEFORE the
		# asset binds: on bind the preset re-applies against the new asset, so the
		# very first chunk selection is already budget-bounded. Loading first and
		# applying the profile after would kick off an unbounded build of the
		# whole cloud (a ~400 MB data texture for a 6M-splat scene).
		var splat_node: Object = ClassDB.instantiate(NODE_CLASS_NAME)
		if splat_node == null:
			_on_load_failed.call_deferred("GaussianSplatNode3D is not registered.")
			return
		# Resolve the selected profile's settings, optionally override a single
		# field (the depth mode), and apply BEFORE the asset binds so the first
		# build is already budget-bounded. Backend fields persist across the bind;
		# budget/SH apply on bind.
		_apply_profile_with_override(splat_node)
		splat_node.call("set_chunk_selection", splat_chunk_selection)
		_apply_view_priority_settings(splat_node)
		splat_node.set("source_gltf", path)
		if not splat_node.call("has_asset"):
			var message := _decode_failure_message(splat_node, path)
			splat_node.free()
			_on_load_failed.call_deferred(message)
			return
		_on_load_finished.call_deferred(splat_node)
		return

	var packed := ResourceLoader.load(path) as PackedScene
	if packed == null:
		_on_load_failed.call_deferred("Sample not found as raw glTF or imported scene: %s" % [path])
		return
	var instantiated := packed.instantiate()
	if instantiated == null:
		_on_load_failed.call_deferred("Failed to instantiate imported sample scene.")
		return
	_on_load_finished.call_deferred(instantiated)

func _on_load_failed(message: String) -> void:
	_join_load_thread()
	push_error(message)
	_status_label.text = message
	_set_loading_panel_text(message)

func _decode_failure_message(splat_node: Object, path: String) -> String:
	if splat_node.has_method("get_last_load_error"):
		var detail := str(splat_node.call("get_last_load_error"))
		if not detail.is_empty():
			return detail
	return "Failed to decode splats from %s. Native extension did not report a detailed load error; rebuild and re-export the Android GDExtension library." % [path]

func _on_load_finished(generated: Node) -> void:
	_join_load_thread()
	_loaded_scene = generated
	add_child(_loaded_scene)
	_status_label.text = "godot-gsplat minimal glTF load demo"

	var splat_node: Object = _find_first_by_class(_loaded_scene, NODE_CLASS_NAME)
	if splat_node == null:
		push_error("Generated sample scene does not contain a GaussianSplatNode3D.")
		_set_loading_panel_text("No GaussianSplatNode3D in the sample.")
		return

	# The raw-glTF path already applied the profile before binding; on the
	# imported-scene fallback (baked render, no live asset) apply it here so the
	# backend settings match the selection plus any depth override.
	if not splat_node.call("has_asset"):
		_apply_profile_with_override(splat_node)
		splat_node.call("set_chunk_selection", splat_chunk_selection)
		_apply_view_priority_settings(splat_node)

	# Keep the panel up until the splats actually render: large clouds build
	# their first render set asynchronously after entering the tree.
	await _wait_for_first_render(splat_node)
	# Reassert a depth override straight on the live material. This is the
	# authoritative path for the baked-scene fallback (its material is built at
	# import and never rebuilt at runtime, so the backend-settings route is
	# ignored there); harmless for the raw path, which already built it correctly.
	if splat_depth_mode_override != "profile_default":
		_apply_depth_mode_to_material(splat_node)
	_set_loading_panel_visible(false)

func _wait_for_first_render(splat_node: Object) -> void:
	# Phase 1: wait for the first render set (async chunk build).
	var deadline := Time.get_ticks_msec() + 30000
	while Time.get_ticks_msec() < deadline:
		if int(splat_node.call("get_rendered_splat_count")) > 0:
			break
		await get_tree().process_frame
	# Phase 2: wait for the GPU depth sort to kick in — until then splats
	# blend unsorted and look like noise (~1 s on Quest for a baked scene).
	# Time-capped: without a RenderingDevice the sort never enables.
	deadline = Time.get_ticks_msec() + 5000
	while Time.get_ticks_msec() < deadline:
		if splat_node.call("is_depth_sorted"):
			return
		await get_tree().process_frame

func _set_loading_panel_visible(visible_now: bool) -> void:
	if _loading_panel != null:
		_loading_panel.visible = visible_now

func _set_loading_panel_text(text: String) -> void:
	if _loading_panel == null:
		return
	_set_loading_panel_visible(true)
	var label := _loading_panel.get_node_or_null("Label3D") as Label3D
	if label != null:
		label.text = text

# Resolve the selected profile's settings, overlay any per-field overrides set in
# the inspector, and apply via the node's get_profile_settings /
# apply_profile_settings round-trip. The profile keeps its own value for every
# field left at its sentinel. Applied before the asset binds on the raw path, so
# the first build and every async chunk rebuild read it.
func _apply_profile_with_override(splat_node: Object) -> void:
	var settings: Dictionary = splat_node.call("get_profile_settings", splat_render_profile)
	if splat_target_hint_override != "profile_default":
		settings["target_hint"] = splat_target_hint_override
	if splat_budget_override_enabled:
		settings["budget"] = splat_budget
	if splat_sh_degree_override >= 0:
		settings["sh_degree"] = splat_sh_degree_override
	if splat_vr_view_basis_override != "profile_default":
		settings["vr_view_basis"] = splat_vr_view_basis_override
	if splat_depth_mode_override != "profile_default":
		settings["splat_depth_mode"] = splat_depth_mode_override
	splat_node.call("apply_profile_settings", settings)

func _apply_view_priority_settings(splat_node: Object) -> void:
	if splat_chunk_selection != "view_priority":
		return
	var settings: Object = splat_node.call("get_cloud_settings")
	if settings == null:
		return
	settings.call("set_view_priority_fov_degrees", view_priority_fov_degrees)
	settings.call("set_view_priority_full_distance", view_priority_full_distance)
	settings.call("set_view_priority_target_budget", splat_budget)
	settings.call("set_view_priority_min_lod_per_chunk", view_priority_min_lod_per_chunk)

# Push the depth mode straight onto the live material (0 = ray, 1 = center).
# Used for the baked-scene path, whose material is never rebuilt at runtime.
func _apply_depth_mode_to_material(splat_node: Object) -> void:
	splat_node.call("set_live_splat_depth_mode", 1 if splat_depth_mode_override == "center" else 0)

func _join_load_thread() -> void:
	if _load_thread != null and _load_thread.is_started():
		_load_thread.wait_to_finish()
	_load_thread = null

func _find_first_by_class(root: Node, target_class_name: String) -> Object:
	if root.is_class(target_class_name):
		return root

	for child in root.get_children():
		var found := _find_first_by_class(child, target_class_name)
		if found != null:
			return found

	return null
