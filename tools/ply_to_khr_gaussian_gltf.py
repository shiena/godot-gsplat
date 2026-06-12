#!/usr/bin/env python3
"""Convert a common 3DGS PLY file into KHR_gaussian_splatting glTF."""

from __future__ import annotations

import argparse
import json
import math
import struct
from dataclasses import dataclass
from pathlib import Path
from typing import BinaryIO


KHR_GAUSSIAN_SPLATTING = "KHR_gaussian_splatting"
SH_C0 = 0.2820947917738781

PLY_STRUCT_FORMATS = {
    "char": "b",
    "int8": "b",
    "uchar": "B",
    "uint8": "B",
    "short": "h",
    "int16": "h",
    "ushort": "H",
    "uint16": "H",
    "int": "i",
    "int32": "i",
    "uint": "I",
    "uint32": "I",
    "float": "f",
    "float32": "f",
    "double": "d",
    "float64": "d",
}


@dataclass(frozen=True)
class PlyProperty:
    name: str
    ply_type: str
    struct_format: str
    byte_size: int


@dataclass(frozen=True)
class PlyHeader:
    vertex_count: int
    properties: list[PlyProperty]
    data_offset: int


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Convert binary little-endian 3DGS PLY to KHR_gaussian_splatting glTF."
    )
    parser.add_argument(
        "input",
        type=Path,
        help="Input 3DGS .ply file.",
    )
    parser.add_argument(
        "output",
        type=Path,
        nargs="?",
        help="Output .gltf path. Defaults to samples/converted/<input-stem>.gltf.",
    )
    parser.add_argument(
        "--limit",
        type=int,
        default=None,
        help="Optional point limit for small validation assets.",
    )
    parser.add_argument(
        "--no-rest-sh",
        action="store_true",
        help="Do not emit SH degree 1-3 attributes from f_rest_* properties.",
    )
    parser.add_argument(
        "--color-space",
        choices=["lin_rec709_display", "srgb_rec709_display"],
        default="lin_rec709_display",
        help="KHR_gaussian_splatting colorSpace value.",
    )
    parser.add_argument(
        "--rotation-order",
        choices=["wxyz", "xyzw"],
        default="wxyz",
        help="PLY rotation property order. Common 3DGS PLY files use wxyz.",
    )
    parser.add_argument(
        "--raw-activations",
        action="store_true",
        help="Do not apply exp(scale) or sigmoid(opacity). This is usually not valid KHR data.",
    )
    parser.add_argument(
        "--coordinate-system",
        choices=["supersplat", "raw"],
        default="supersplat",
        help="Coordinate conversion to apply. 'supersplat' bakes the common SuperSplat PLY orientation into glTF.",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    input_path: Path = args.input
    output_path: Path = args.output or Path("samples/converted") / f"{input_path.stem}.gltf"
    output_path = output_path.resolve()
    bin_path = output_path.with_suffix(".bin")

    with input_path.open("rb") as file:
        header = read_ply_header(file)
        validate_required_properties(header)
        point_count = header.vertex_count if args.limit is None else min(header.vertex_count, args.limit)
        data = read_vertices(file, header, point_count)

    attributes = build_attributes(
        data,
        point_count,
        include_rest_sh=not args.no_rest_sh,
        raw_activations=args.raw_activations,
        rotation_order=args.rotation_order,
        color_space=args.color_space,
        coordinate_system=args.coordinate_system,
    )
    gltf, blob = build_gltf(input_path, bin_path.name, attributes, point_count, args.color_space)

    output_path.parent.mkdir(parents=True, exist_ok=True)
    bin_path.write_bytes(blob)
    output_path.write_text(json.dumps(gltf, indent=2), encoding="utf-8")

    print(f"Wrote {output_path}")
    print(f"Wrote {bin_path}")
    print(f"points={point_count} bytes={len(blob)}")


def read_ply_header(file: BinaryIO) -> PlyHeader:
    first_line = file.readline()
    if first_line != b"ply\n":
        raise ValueError("Input is not a PLY file.")

    vertex_count: int | None = None
    properties: list[PlyProperty] = []
    in_vertex = False

    while True:
        line_bytes = file.readline()
        if not line_bytes:
            raise ValueError("PLY header ended unexpectedly.")
        line = line_bytes.decode("ascii").strip()
        if line == "end_header":
            break
        if line == "format binary_little_endian 1.0":
            continue
        if line.startswith("format "):
            raise ValueError(f"Unsupported PLY format: {line}")
        if line.startswith("element "):
            parts = line.split()
            in_vertex = parts[1] == "vertex"
            if in_vertex:
                vertex_count = int(parts[2])
            continue
        if in_vertex and line.startswith("property "):
            parts = line.split()
            if parts[1] == "list":
                raise ValueError("List properties in vertex data are not supported.")
            ply_type = parts[1]
            name = parts[2]
            if ply_type not in PLY_STRUCT_FORMATS:
                raise ValueError(f"Unsupported PLY property type '{ply_type}' for '{name}'.")
            struct_format = PLY_STRUCT_FORMATS[ply_type]
            properties.append(
                PlyProperty(
                    name=name,
                    ply_type=ply_type,
                    struct_format=struct_format,
                    byte_size=struct.calcsize("<" + struct_format),
                )
            )

    if vertex_count is None:
        raise ValueError("PLY does not define a vertex element.")
    if not properties:
        raise ValueError("PLY vertex element has no supported properties.")

    return PlyHeader(vertex_count=vertex_count, properties=properties, data_offset=file.tell())


def validate_required_properties(header: PlyHeader) -> None:
    names = {prop.name for prop in header.properties}
    required = {
        "x",
        "y",
        "z",
        "f_dc_0",
        "f_dc_1",
        "f_dc_2",
        "opacity",
        "scale_0",
        "scale_1",
        "scale_2",
        "rot_0",
        "rot_1",
        "rot_2",
        "rot_3",
    }
    missing = sorted(required - names)
    if missing:
        raise ValueError(f"PLY is missing required 3DGS properties: {', '.join(missing)}")


def read_vertices(file: BinaryIO, header: PlyHeader, point_count: int) -> dict[str, list[float]]:
    file.seek(header.data_offset)
    row_size = sum(prop.byte_size for prop in header.properties)
    row_format = "<" + "".join(prop.struct_format for prop in header.properties)
    unpack_row = struct.Struct(row_format).unpack
    names = [prop.name for prop in header.properties]
    data = {name: [] for name in names}

    for _ in range(point_count):
        row = file.read(row_size)
        if len(row) != row_size:
            raise ValueError("PLY vertex data ended unexpectedly.")
        values = unpack_row(row)
        for name, value in zip(names, values):
            data[name].append(float(value))

    return data


def build_attributes(
    data: dict[str, list[float]],
    point_count: int,
    *,
    include_rest_sh: bool,
    raw_activations: bool,
    rotation_order: str,
    color_space: str,
    coordinate_system: str,
) -> dict[str, tuple[str, list[float], dict[str, list[float]]]]:
    positions: list[float] = []
    rotations: list[float] = []
    scales: list[float] = []
    opacities: list[float] = []
    sh0: list[float] = []
    colors: list[float] = []

    min_position = [math.inf, math.inf, math.inf]
    max_position = [-math.inf, -math.inf, -math.inf]

    for index in range(point_count):
        position = transform_position(
            [data["x"][index], data["y"][index], data["z"][index]],
            coordinate_system,
        )
        positions.extend(position)
        for axis in range(3):
            min_position[axis] = min(min_position[axis], position[axis])
            max_position[axis] = max(max_position[axis], position[axis])

        scale = [data[f"scale_{axis}"][index] for axis in range(3)]
        if not raw_activations:
            scale = [math.exp(value) for value in scale]
        scales.extend(max(0.0, value) for value in scale)

        opacity = data["opacity"][index]
        if not raw_activations:
            opacity = sigmoid(opacity)
        opacity = clamp(opacity, 0.0, 1.0)
        opacities.append(opacity)

        rotation = [data[f"rot_{axis}"][index] for axis in range(4)]
        if rotation_order == "wxyz":
            rotation = [rotation[1], rotation[2], rotation[3], rotation[0]]
        rotation = normalize_quaternion_xyzw(rotation)
        rotation = transform_rotation_xyzw(rotation, coordinate_system)
        rotations.extend(rotation)

        sh = [data[f"f_dc_{axis}"][index] for axis in range(3)]
        sh0.extend(sh)
        color = [clamp(channel * SH_C0 + 0.5, 0.0, 1.0) for channel in sh]
        if color_space == "srgb_rec709_display":
            color = [srgb_to_linear(channel) for channel in color]
        colors.extend([color[0], color[1], color[2], opacity])

    attributes: dict[str, tuple[str, list[float], dict[str, list[float]]]] = {
        "POSITION": ("VEC3", positions, {"min": min_position, "max": max_position}),
        "KHR_gaussian_splatting:ROTATION": ("VEC4", rotations, {}),
        "KHR_gaussian_splatting:SCALE": ("VEC3", scales, {}),
        "KHR_gaussian_splatting:OPACITY": ("SCALAR", opacities, {}),
        "KHR_gaussian_splatting:SH_DEGREE_0_COEF_0": ("VEC3", sh0, {}),
        "COLOR_0": ("VEC4", colors, {}),
    }

    if include_rest_sh and all(f"f_rest_{index}" in data for index in range(45)):
        add_rest_sh_attributes(attributes, data, point_count)

    return attributes


def add_rest_sh_attributes(
    attributes: dict[str, tuple[str, list[float], dict[str, list[float]]]],
    data: dict[str, list[float]],
    point_count: int,
) -> None:
    rest_offsets = [
        (1, 0, 3),
        (2, 3, 5),
        (3, 8, 7),
    ]
    for degree, start, count in rest_offsets:
        for coef in range(count):
            rest_index = start + coef
            values: list[float] = []
            for point_index in range(point_count):
                values.extend(
                    [
                        data[f"f_rest_{rest_index}"][point_index],
                        data[f"f_rest_{15 + rest_index}"][point_index],
                        data[f"f_rest_{30 + rest_index}"][point_index],
                    ]
                )
            attributes[f"KHR_gaussian_splatting:SH_DEGREE_{degree}_COEF_{coef}"] = (
                "VEC3",
                values,
                {},
            )


def build_gltf(
    input_path: Path,
    bin_name: str,
    attributes: dict[str, tuple[str, list[float], dict[str, list[float]]]],
    point_count: int,
    color_space: str,
) -> tuple[dict, bytes]:
    blob = bytearray()
    buffer_views: list[dict] = []
    accessors: list[dict] = []
    primitive_attributes: dict[str, int] = {}

    for semantic, (accessor_type, values, extras) in attributes.items():
        align_blob(blob, 4)
        byte_offset = len(blob)
        blob.extend(pack_floats(values))
        byte_length = len(blob) - byte_offset
        buffer_view_index = len(buffer_views)
        accessor_index = len(accessors)

        buffer_views.append(
            {
                "buffer": 0,
                "byteOffset": byte_offset,
                "byteLength": byte_length,
            }
        )
        accessor = {
            "bufferView": buffer_view_index,
            "byteOffset": 0,
            "componentType": 5126,
            "count": point_count,
            "type": accessor_type,
        }
        accessor.update(extras)
        accessors.append(accessor)
        primitive_attributes[semantic] = accessor_index

    gltf = {
        "asset": {
            "version": "2.0",
            "generator": "godot-gsplat tools/ply_to_khr_gaussian_gltf.py",
        },
        "extensionsUsed": [KHR_GAUSSIAN_SPLATTING],
        "scene": 0,
        "scenes": [{"nodes": [0]}],
        "nodes": [{"name": input_path.stem, "mesh": 0}],
        "meshes": [
            {
                "name": f"{input_path.stem}_mesh",
                "primitives": [
                    {
                        "mode": 0,
                        "attributes": primitive_attributes,
                        "extensions": {
                            KHR_GAUSSIAN_SPLATTING: {
                                "kernel": "ellipse",
                                "colorSpace": color_space,
                                "projection": "perspective",
                                "sortingMethod": "cameraDistance",
                            }
                        },
                    }
                ],
            }
        ],
        "accessors": accessors,
        "bufferViews": buffer_views,
        "buffers": [{"byteLength": len(blob), "uri": bin_name}],
    }
    return gltf, bytes(blob)


def pack_floats(values: list[float]) -> bytes:
    return struct.pack("<" + "f" * len(values), *values)


def align_blob(blob: bytearray, alignment: int) -> None:
    padding = (-len(blob)) % alignment
    if padding:
        blob.extend(b"\0" * padding)


def sigmoid(value: float) -> float:
    if value >= 0.0:
        exponent = math.exp(-value)
        return 1.0 / (1.0 + exponent)
    exponent = math.exp(value)
    return exponent / (1.0 + exponent)


def normalize_quaternion_xyzw(value: list[float]) -> list[float]:
    length = math.sqrt(sum(component * component for component in value))
    if length <= 0.0 or not math.isfinite(length):
        return [0.0, 0.0, 0.0, 1.0]
    return [component / length for component in value]


def transform_position(position: list[float], coordinate_system: str) -> list[float]:
    if coordinate_system == "raw":
        return position
    if coordinate_system == "supersplat":
        # Bake SuperSplat's common PLY orientation into the exported glTF data.
        return [position[0], -position[1], -position[2]]
    raise ValueError(f"Unsupported coordinate system: {coordinate_system}")


def transform_rotation_xyzw(rotation: list[float], coordinate_system: str) -> list[float]:
    if coordinate_system == "raw":
        return rotation
    if coordinate_system == "supersplat":
        correction = [1.0, 0.0, 0.0, 0.0]
        return normalize_quaternion_xyzw(mul_quaternion_xyzw(correction, rotation))
    raise ValueError(f"Unsupported coordinate system: {coordinate_system}")


def mul_quaternion_xyzw(a: list[float], b: list[float]) -> list[float]:
    ax, ay, az, aw = a
    bx, by, bz, bw = b
    return [
        aw * bx + ax * bw + ay * bz - az * by,
        aw * by - ax * bz + ay * bw + az * bx,
        aw * bz + ax * by - ay * bx + az * bw,
        aw * bw - ax * bx - ay * by - az * bz,
    ]


def clamp(value: float, lower: float, upper: float) -> float:
    return min(max(value, lower), upper)


def srgb_to_linear(value: float) -> float:
    if value <= 0.04045:
        return value / 12.92
    return ((value + 0.055) / 1.055) ** 2.4


if __name__ == "__main__":
    main()
