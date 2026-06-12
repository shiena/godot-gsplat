extends Camera3D

## Orbits around the loaded Gaussian splat cloud. The target point and distance
## are auto-framed from the splat node's AABB once the scene has loaded.

## Orbit speed in degrees per second.
@export var orbit_speed: float = 20.0
## Camera height above the target, as a fraction of the cloud radius.
@export var height_ratio: float = 0.25
## Camera distance from the target, as a multiple of the cloud radius.
@export var distance_ratio: float = 2.4

var _angle: float = 0.0
var _target: Vector3 = Vector3.ZERO
var _radius: float = 2.0
var _framed: bool = false

func _ready() -> void:
	current = true

func _process(delta: float) -> void:
	# The splat scene is loaded after _ready, so frame lazily once it appears.
	if not _framed:
		_try_frame()

	_angle += deg_to_rad(orbit_speed) * delta
	var offset := Vector3(sin(_angle), height_ratio, cos(_angle)) * (_radius * distance_ratio)
	global_position = _target + offset
	look_at(_target, Vector3.UP)

func _try_frame() -> void:
	var scene := get_tree().current_scene
	if scene == null:
		return
	var node := _find_splat(scene)
	if node == null:
		return
	var visual := _find_visual_instance(node)
	if visual == null:
		return
	var local_aabb: AABB = visual.get_aabb()
	# The splat geometry is a unit quad expanded in the shader; bounds come from
	# the node's custom AABB, which is only populated once the cloud is built.
	if local_aabb.size.length() < 0.001:
		return
	var aabb: AABB = visual.get_global_transform() * local_aabb
	_target = aabb.position + aabb.size * 0.5
	_radius = maxf(aabb.size.length() * 0.5, 0.1)
	near = maxf(_radius * 0.01, 0.01)
	far = _radius * 40.0
	_framed = true

func _find_splat(n: Node) -> Node:
	if n.is_class("GaussianSplatNode3D"):
		return n
	for child in n.get_children():
		var found := _find_splat(child)
		if found != null:
			return found
	return null

func _find_visual_instance(n: Node) -> VisualInstance3D:
	for child in n.get_children():
		if child is VisualInstance3D:
			return child
	return null
