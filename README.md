# godot-gsplat

`godot-gsplat` is a Godot 4 add-on for displaying 3D Gaussian Splatting on PC, mobile, and VR.

## Goals

- Support `KHR_gaussian_splatting` through a `GLTFDocumentExtension` implemented in godot-rust.
- Keep the runtime path `Node3D`-first so it can work on Quest native and other non-compositor targets.
- Leave room for optional support of `khr_gaussian_splatting_compression_spz`.

## Design Principles

- Import-first architecture.
- Stateless `GLTFDocumentExtension` implementation.
- Clear separation between import data, runtime node state, and rendering backend state.
- Shared data model across PC, mobile, and VR.

## Status

This repository is currently in the design and scaffolding phase.
Implementation will follow the documented architecture.
