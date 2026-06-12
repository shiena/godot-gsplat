# godot-gsplat

English | [日本語](README_ja.md)

`godot-gsplat` is a Godot 4 add-on for displaying 3D Gaussian Splatting on PC, mobile, and VR.

Splats are imported through the `KHR_gaussian_splatting` glTF extension (implemented in
godot-rust) and rendered on a texture-driven low-level `RenderingServer` path: anisotropic splats
are projected in a shader (exact ellipsoid projection, no affine/Jacobian approximation),
depth-sorted on the GPU, and shaded with spherical harmonics evaluated per fragment. The runtime
path is `Node3D`-first so it can target Quest-native and other non-compositor setups.

## Goals

- Import `KHR_gaussian_splatting` through a `GLTFDocumentExtension` implemented in godot-rust.
- Keep the runtime path `Node3D`-first so it works on Quest native and other non-compositor targets.
- Share one data model across PC, mobile, and VR.
- Leave room for optional `KHR_gaussian_splatting_compression_spz_2` support.

## Design Principles

- Import-first architecture.
- Stateless `GLTFDocumentExtension` implementation.
- Clear separation between import data, runtime node state, and rendering backend state.
- Shared data model across PC, mobile, and VR.

## How it works

### Import

- `GltfGsplatDocumentExtension` (a `GLTFDocumentExtension`) parses the `KHR_gaussian_splatting`
  extension, decodes the per-splat attributes (position, rotation, scale, opacity, and SH
  coefficients) into an interleaved float32 payload, and produces a `GaussianSplatAsset` resource
  bound to a `GaussianSplatNode3D`.
- The payload is partitioned into a spatial grid at import so the runtime can stream a bounded
  subset of a large cloud.
- Large captures can also be converted to a disk-backed `.gsplatpack` file. The runtime then loads
  page-sized chunks from `user://` instead of keeping the whole decoded payload in memory at once,
  which is the Quest-friendly path for very large scenes.
- `GsplatScenePostImportPlugin` exposes preview options in the glTF import dialog
  (`gsplat/preview_max_splats`, `gsplat/preview_max_splat_radius`,
  `gsplat/preview_scale_multiplier`).

### Editor drag-and-drop

- Dropping a `.gltf`/`.glb` into the 3D viewport replaces the imported scene wrapper with a clean
  `GaussianSplatNode3D` whose `Source glTF` property points at the dropped file. The node then
  live-loads the splats, so the scene references the source asset instead of baking a copy.

### Rendering

- Each splat is one virtual quad emitted from a low-level `RenderingServer` mesh surface. The
  surface uses an empty vertex array and `splat_count * 6` virtual vertices; the shader derives the
  splat slot and quad corner from `VERTEX_ID`, so there is no per-splat transform stream.
  Per-splat data (center, per-axis scale, rotation quaternion, color, SH coefficients) is packed
  into a texture the shader samples.
- The vertex shader projects the splat ellipsoid to a screen-space ellipse **without the
  affine/Jacobian approximation** — it projects the center and the three rotated/scaled semi-axes
  to clip space, forms the projected quadric/conic, and recovers the ellipse by eigen-decomposition.
  This stays stable for the near-camera, wide-FOV views that matter for VR (where the classic
  Jacobian approximation blurs and drifts). It then expands the quad along the ellipse axes and
  frustum-culls off-screen splats.
- Sub-pixel footprints use energy-preserving antialiasing: the on-screen ellipse is clamped to a
  minimum size and alpha is scaled by the area ratio, so distant splats stay visible without
  over-brightening.
- Per-corner depth (the `splat_depth_mode = ray` default) is the Gaussian peak along the view ray
  (ray vs ellipsoid), written to `POSITION.z` so splats intersect opaque geometry correctly while
  keeping early-z. A `center` mode writes the splat-center depth to every corner instead, trading
  that per-corner accuracy to remove a VR white shimmer the ray path can produce on high-alpha
  overlapping splats (see [VR runtime settings](#vr-runtime-settings)).
- The fragment shader applies the Gaussian falloff and discards subpixel contributions.
- View-dependent color comes from spherical harmonics evaluated in the shader, degree 0–3.
- The exact projection / AA / ray-depth math is adapted from
  [VRChatGaussianSplatting](https://github.com/MichaelMoroz/VRChatGaussianSplatting) (MIT).

### Depth sorting

- A GPU compute counting sort (16-bit depth buckets with a multi-stage prefix sum) re-orders
  splats back-to-front whenever the view changes; static views skip the re-sort. A per-eye sort
  path exists for VR.
- **Limitation — multiple simultaneous cameras.** A node keeps one back-to-front order, computed
  for a single camera. If the same node is rendered by more than one active camera in the same
  frame (split-screen, picture-in-picture, mirrors, or several SubViewports sharing one `World3D`),
  only that one camera's view blends correctly; the others show wrong front/back ordering. Splat
  geometry is still correct in every viewport — only the alpha-compositing order is shared. (VR
  multiview, two eyes in one viewport, is handled separately and is unaffected.) To show the same
  cloud from different cameras at once, duplicate the node per viewport with its own `World3D`. The
  general multi-camera fix is not planned.

### Antialiasing (MSAA)

- Gaussian splats do **not** benefit from MSAA: they are alpha-blended billboards, not geometric
  edges, so MSAA only adds cost — especially in VR/Quest where fill rate is scarce. The shader's
  own energy-preserving antialiasing already handles sub-pixel footprints.
- Godot's `rendering/anti_aliasing/quality/msaa_3d` defaults to *Disabled*, which is the
  recommended setting for splat scenes. If you enable MSAA for other geometry, expect extra cost
  with little visual gain on the splats; for XR in particular, prefer leaving `msaa_3d` disabled.

### VR runtime settings

- For VR, use `render_profile = XR`. It selects the VR-safe pipeline, head-center sorting, the
  lower SH/budget profile intended for headset rendering, and `splat_depth_mode = center`.
- `splat_depth_mode = center` removes a steady white shimmer that the per-corner ray-depth path
  produces in stereo on high-alpha overlapping splats. The `XR` profile enables it by default;
  other profiles keep `ray` and can opt in via the backend settings (or a per-field override).
- Keep `rendering/anti_aliasing/quality/msaa_3d` set to *Disabled* for VR. The splat renderer
  does not rely on MSAA, and disabling it avoids unnecessary cost on Quest and PC-VR.

### Render profiles

`render_profile` is the main quality knob on `GaussianSplatNode3D`. Each preset resolves to a
backend pipeline target, a splat budget, an SH degree, a VR view basis, and a splat depth mode:

| Profile | Pipeline target | Splat budget | SH degree | Depth mode | Use it when |
|---|---|---|---|---|---|
| `Low` | VR-safe | ≈150k | 0 | `ray` | Lowest cost — weak GPUs, or large clouds you want to cap hard. |
| `Middle` | Mobile | ≈500k | 1 | `ray` | Balanced mobile/desktop quality. |
| `High` | Desktop | Unbounded (all splats) | 3 | `ray` | Maximum fidelity on a capable desktop GPU: full SH, no cap. |
| `XR` | VR-safe | 300k–800k, scaled to the asset's spatial extent | 1 | `center` | VR/headset rendering — `center` depth avoids the stereo white shimmer the `ray` path causes on high-alpha overlapping splats. |
| `Custom` | manual | manual | manual | manual | Full manual control; set automatically when you edit any individual field. |

- Every preset uses the **head-center** VR view basis; per-eye is reachable only through the backend
  settings (per-eye sorting is still unverified on device).
- The splat budget is always clamped to the asset's point count, so a small cloud renders in full
  even under `High`.
- `XR`'s budget interpolates between 300k (spatial extent ≈2 units) and 800k (≈30 units), so a
  tabletop capture spends fewer splats and a building-scale capture more (`xr_adaptive_budget`).
- Editing any individual field (budget, SH degree, depth mode, …) switches the profile to `Custom`
  and stops a preset from being re-applied when an asset (re)binds.
- A profile resolves to a settings dictionary scripts can read, override per field, and re-apply via
  `get_profile_settings` / `apply_profile_settings` — e.g. run `High` but override only the depth
  mode. See `demo/minimal_demo.gd`.

### Tuning workflow

| Step | What to set | How to choose it |
|---|---|---|
| 1 | `render_profile` | Pick the baseline first. Use `XR` for Quest/XR scenes you want to ship on headset hardware, `High` when you want the highest source quality and will tune the budget manually, and `Custom` only when you are already overriding multiple fields. |
| 2 | `get_profile_settings(profile)` | Read the preset dictionary and keep the fields you want as-is. Override only the values you actually need to move away from the preset. |
| 3 | `budget` / `sh_degree` / `splat_depth_mode` | Tune render cost before streaming shape. On Quest, lower `budget` first because it usually buys the biggest performance gain; then drop `sh_degree` if the scene still misses frame time; keep `splat_depth_mode = center` when you see stereo shimmer. |
| 4 | `splat_chunk_selection = view_priority` | Turn on streaming once the render budget is in range. This keeps the current view represented first, then degrades distant/peripheral chunks. |
| 5 | `view_priority_target_budget` / `view_priority_min_lod_per_chunk` / `view_priority_fov_degrees` / `view_priority_full_distance` | Adjust the streaming selector in this order: target budget first, minimum per-chunk LOD second, FOV third, full-distance last. This keeps tuning stable because the first two decide total work, while the latter two decide which parts of the cloud get to stay full-density. |

The dictionary `get_profile_settings` returns and `apply_profile_settings` accepts has five keys.
Pass a partial dictionary to override only the keys you care about — a missing key keeps the node's
current value:

| Key | Type | Values | Notes |
|---|---|---|---|
| `target_hint` | String | `desktop` / `mobile` / `vr_safe` | Backend pipeline target. Applied immediately and kept across an asset (re)bind. |
| `budget` | int | splat count, or `-1` | Max active splats, clamped to the asset's point count. `-1` derives the budget from the asset's spatial extent (the `XR` adaptive curve). |
| `sh_degree` | int | `0`–`3` | Spherical-harmonics degree evaluated in the shader. |
| `vr_view_basis` | String | `head_center` / `per_eye` | Sort/cull basis for VR. Applied immediately and kept across a rebind. |
| `splat_depth_mode` | String | `ray` / `center` | Per-corner ray-vs-ellipsoid depth, or flat splat-center depth. Applied immediately and kept across a rebind. |

- `target_hint`, `vr_view_basis`, and `splat_depth_mode` are backend-target fields: they apply at
  once and survive an asset rebind.
- `budget` and `sh_degree` depend on the bound asset (the adaptive budget needs its extent), so they
  are applied once an asset binds and re-applied on every rebind — which is what lets a per-field
  override survive a rebind.

### Quality and scaling controls
| Setting | Type | Meaning |
|---|---|---|
| `splat_chunk_selection` | String | `view_priority` keeps a wide cone around the camera/HMD represented at full density, then lowers density on distant or peripheral chunks when the target budget would otherwise be exceeded. It is the main streaming mode for Quest-scale room captures. |
| `view_priority_fov_degrees` | float | Controls that cone width. The default `200` degrees is intentionally wider than a normal FOV so quick head turns do not reveal empty areas while chunks are still loading. |
| `view_priority_full_distance` | float | Keeps every chunk within that local-space radius in the candidate set. Lowering it reduces the amount of world that is guaranteed full-density; raising it keeps more nearby detail at the cost of more active splats. |
| `view_priority_target_budget` | int | The splat budget used by the streaming selector. When the view cone would exceed it, the selector reduces distant/peripheral chunks instead of dropping the whole region. |
| `view_priority_min_lod_per_chunk` | int | The smallest prefix kept for each selected chunk when density has to be reduced. Lower values preserve more chunk coverage; higher values preserve more detail inside the chunks that stay active. |
| `GaussianSplatBackendSettings` | Resource | Resolves the render pipeline from a target hint (`Desktop` / `Mobile` / `VR-safe`) and carries the VR view basis (`head-center` / `per-eye`) and splat depth mode (`ray` / `center`). |

## Components

| Class | Base | Role |
|---|---|---|
| `GaussianSplatNode3D` | `Node3D` | Runtime node; binds an asset or live-loads `source_gltf`, builds render data, drives the GPU sort. |
| `GaussianSplatAsset` | `Resource` | Decoded splat payload, layout, AABB, and optional chunk table. |
| `GaussianSplatCloudSettings` | `Resource` | Per-cloud visibility, scale, splat budget, and SH degree. |
| `GaussianSplatBackendSettings` | `Resource` | Target / pipeline / VR view-basis / splat depth-mode selection. |
| `GltfGsplatDocumentExtension` | `GLTFDocumentExtension` | Imports `KHR_gaussian_splatting`. |
| `GsplatScenePostImportPlugin` | `EditorScenePostImportPlugin` | Import-dialog preview options. |

## Status

Implemented:

- godot-rust GDExtension class registration and the editor plugin.
- `KHR_gaussian_splatting` glTF import into a `GaussianSplatAsset`, including higher-order SH on a
  variable-stride payload.
- Texture-driven low-level `RenderingServer` splat renderer with exact ellipsoid projection (no
  affine/Jacobian approximation), energy-preserving antialiasing, selectable per-corner ray /
  splat-center depth, vertex-shader frustum culling, and no per-splat transform stream.
- GPU compute depth sort with adaptive re-sort gating.
- In-shader spherical harmonics evaluation (degree 0–3), gated by render profile.
- Low/Middle/High/XR/Custom render-quality profiles.
- Spatial-grid chunking at import with a bounded, importance-ranked, asynchronously rebuilt active
  chunk set.
- Editor drag-and-drop that links a dropped glTF as a source instead of baking it.

Implemented but unverified:

- VR per-eye sorting and rendering paths exist, but have not been verified on device.

Not implemented yet:

- `KHR_gaussian_splatting_compression_spz_2` decoding (the extension name is reserved only).

## Requirements & build

- Godot 4.5+ (the project is authored against 4.7).
- A Rust toolchain. The GDExtension uses the `godot` crate 0.5 (`api-4-5`).

Build the extension, then open the project and enable the **Godot Gsplat** add-on:

```powershell
cargo build            # debug   -> target/debug/godot_gsplat.dll
cargo build --release  # release -> target/release/godot_gsplat.dll
```

`godot_gsplat.gdextension` points at `res://target/{debug,release}/godot_gsplat.dll`. The add-on
(`addons/godot_gsplat`) registers the glTF document extension, the post-import plugin, and the
viewport drop hook.

## Demo

`demo/minimal_demo.tscn` (the project's main scene) loads a sample cloud with an orbiting camera.

### Loading large clouds at runtime

Parsing + decoding a multi-million-splat glTF takes seconds, so the demo runs
`GLTFDocument.append_from_file` / `generate_scene` on a background `Thread` and only adds the
generated (detached) tree to the scene on the main thread — see `demo/minimal_demo.gd` for the
pattern. On the node side, render-set (re)builds above ~500k active splats are handed to the async
chunk-rebuild worker instead of blocking the main thread (the editor always builds synchronously so
imports can bake the render into the `.scn`). For shipping, prefer editor-imported scenes: the
decode then happens once at import time and the baked render loads instantly.

## Converting glTF to .gsplatpack

`.gsplatpack` is the preferred runtime format for large splat clouds on memory-constrained targets such as Meta Quest. It keeps the source asset on disk as streamable pages instead of forcing the full decoded splat payload to stay resident in memory.

The pack converter takes an existing `KHR_gaussian_splatting` `.gltf` or `.glb` file as input. If your source is a `.ply`, convert it to glTF first with the script in the next section, then pack the generated glTF.

In the Godot editor:

1. Enable the Godot Gsplat add-on.
2. Open `Project > Tools > Godot Gsplat Pack Converter`.
3. Set `Source glTF` to the `.gltf` or `.glb` file.
4. Set `Output pack` to a `.gsplatpack` path, or enable `Use Default Output` to write the pack next to the source file with the same basename.
5. Click `Convert`.

Use the generated pack anywhere the add-on accepts a splat source, for example `source_gltf` on `GaussianSplatNode3D` or `sample_path` in the demos. For Android or Quest exports, add the `.gsplatpack` path, for example `samples/converted/scene.gsplatpack`, to the export include filters so the file is copied into the exported package.

Keep the original glTF as the authoritative source when you need to regenerate the pack with newer converter settings.

## Converting splats to glTF

`tools/ply_to_khr_gaussian_gltf.py` converts a binary little-endian 3DGS `.ply` into a
`KHR_gaussian_splatting` glTF (position, rotation, scale, opacity, SH degree 0–3, and a `COLOR_0`
fallback). See `tools/README.md` for usage and coordinate-system options.

## References

Projects consulted while building this add-on (for ideas and structure, not copied implementation):

- [KhronosGroup/glTF](https://github.com/KhronosGroup/glTF) — `KHR_gaussian_splatting` extension specification.
- [MichaelMoroz/VRChatGaussianSplatting](https://github.com/MichaelMoroz/VRChatGaussianSplatting) (MIT) — exact ellipsoid projection, energy-preserving antialiasing, and ray-based per-corner depth.
- [ReconWorldLab/godot-gaussian-splatting](https://github.com/ReconWorldLab/godot-gaussian-splatting) — Godot-side node/resource structure and VR view/projection handling.
- [playcanvas/supersplat](https://github.com/playcanvas/supersplat) — data-processing / render-orchestration separation and scaling ideas.
- [BladeTransformerLLC/gauzilla](https://github.com/BladeTransformerLLC/gauzilla) — loader/decoder/renderer split and async/streaming ideas.
- [godotengine/godot](https://github.com/godotengine/godot) — editor import behavior and glTF import-option conventions.

## License

Released under the [MIT License](LICENSE).
