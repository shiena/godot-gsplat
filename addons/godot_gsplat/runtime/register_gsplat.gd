extends Node

const GltfRegistration := preload("res://addons/godot_gsplat/runtime/gltf_registration.gd")

var _gltf_extension: Object

func _enter_tree() -> void:
	_register_gltf_extension()

func _exit_tree() -> void:
	_unregister_gltf_extension()

func _register_gltf_extension() -> void:
	if _gltf_extension != null:
		return

	_gltf_extension = GltfRegistration.register_gltf_extension()
	if _gltf_extension != null:
		print("[godot-gsplat] Runtime GLTF extension registered.")

func _unregister_gltf_extension() -> void:
	if _gltf_extension == null:
		return

	GltfRegistration.unregister_gltf_extension(_gltf_extension)
	_gltf_extension = null
	print("[godot-gsplat] Runtime GLTF extension unregistered.")
