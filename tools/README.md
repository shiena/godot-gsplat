# Tools

## `ply_to_khr_gaussian_gltf.py`

Converts a common binary little-endian 3D Gaussian Splatting PLY file into a glTF asset using `KHR_gaussian_splatting`.

Example:

```powershell
python tools\ply_to_khr_gaussian_gltf.py `
  "C:\Users\shien\dev\3dgs\godot-gaussian-splatting\samples\assets\demo.ply" `
  samples\converted\demo.gltf
```

Small validation asset:

```powershell
python tools\ply_to_khr_gaussian_gltf.py `
  "C:\Users\shien\dev\3dgs\godot-gaussian-splatting\samples\assets\demo.ply" `
  samples\converted\demo_1k.gltf `
  --limit 1000
```

The converter:

- reads binary little-endian PLY vertex data;
- writes `.gltf` plus a sibling `.bin`;
- emits `POSITION`, `KHR_gaussian_splatting:ROTATION`, `KHR_gaussian_splatting:SCALE`, `KHR_gaussian_splatting:OPACITY`, `KHR_gaussian_splatting:SH_DEGREE_0_COEF_0`, and `COLOR_0`;
- emits SH degree 1-3 attributes from `f_rest_*` when present;
- converts common 3DGS activations by applying `exp(scale)` and `sigmoid(opacity)`;
- converts common 3DGS quaternion order from `w,x,y,z` to glTF `x,y,z,w`;
- applies `--coordinate-system supersplat` by default, baking the common SuperSplat PLY orientation into `POSITION` and `ROTATION`;
- derives `COLOR_0` from degree-0 SH for fallback point rendering.

Generated files under `samples/converted/` are local validation outputs and are ignored by Git.

### Coordinate Conversion

SuperSplat does not display common 3DGS PLY files as raw vertex data. Its loader returns a transform, and the editor applies that transform to the splat entity.

For local Godot validation, the converter bakes the same class of correction into the glTF data by default:

```text
position' = (x, -y, -z)
rotation' = quat(180 degrees around X) * rotation
```

This is a proper rotation with positive determinant, unlike a single-axis mirror. It keeps the coordinate system handedness valid for Gaussian covariance reconstruction.

Use `--coordinate-system raw` only when the source PLY is already authored in glTF/Godot-compatible orientation.
