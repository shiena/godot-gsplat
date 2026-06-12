extends RefCounted

# Shared setup helpers for the headless test scripts. Reuses the addon's
# registration helper so tests exercise the same code path as the plugin.

const GltfRegistration := preload("res://addons/godot_gsplat/runtime/gltf_registration.gd")
const NODE_CLASS_NAME := "GaussianSplatNode3D"


# Loads the GDExtension and registers the glTF document extension.
# Returns the live extension instance, or null on failure (callers should
# quit(1) then).
static func register_gltf_extension() -> Object:
	return GltfRegistration.register_gltf_extension()


static func unregister_gltf_extension(extension: Object) -> void:
	GltfRegistration.unregister_gltf_extension(extension)


# Parses `path` through GLTFDocument and returns the generated scene root,
# or null on failure (an error has been pushed).
static func generate_scene_from_gltf(path: String) -> Node:
	var document := GLTFDocument.new()
	var state := GLTFState.new()
	var append_status := document.append_from_file(path, state)
	if append_status != OK:
		push_error("Failed to append glTF '%s': %s" % [path, append_status])
		return null
	var root := document.generate_scene(state)
	if root == null:
		push_error("Failed to generate scene for '%s'." % [path])
	return root


static func find_first_splat_node(root: Node) -> Object:
	return find_first_by_class(root, NODE_CLASS_NAME)


static func find_first_by_class(root: Node, target_class_name: String) -> Object:
	if root.is_class(target_class_name):
		return root
	for child in root.get_children():
		var found := find_first_by_class(child, target_class_name)
		if found != null:
			return found
	return null
