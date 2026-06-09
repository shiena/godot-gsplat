//! Spatial grid partitioning of the splat payload for chunked streaming (Phase C).
//!
//! The decoded payload is a flat array of `POINT_STRIDE_FLOATS` floats per splat.
//! `partition_payload` reorders it so every grid cell's splats are contiguous and
//! importance-sorted (most important first, for distance-LOD top-K), and returns a
//! `ChunkTable` describing each cell's payload range and (radius-grown) bounds.
//! This is pure data logic with no Godot engine dependency, so it is unit-testable
//! with `cargo test`.

use std::collections::HashMap;

use crate::import_state::POINT_STRIDE_FLOATS;

/// Default grid cell edge in world units (matches the Unity 3dgs reference).
pub const DEFAULT_CHUNK_SIZE: f32 = 2.0;

// Per-splat float layout offsets within a POINT_STRIDE_FLOATS block.
const POS_X: usize = 0; // center.xyz at [0..3]
const SCALE_X: usize = 7; // linear per-axis scale at [7..10]
const COLOR_A: usize = 17; // rendered alpha (color.a)

/// One grid cell of contiguously-stored splats in the reordered payload.
#[derive(Clone, Debug)]
pub struct ChunkEntry {
    pub grid: [i32; 3],
    /// Bounds of the cell's splat centers, grown by the largest member's ~3-sigma
    /// radius so distance selection still covers a big splat whose footprint spills
    /// past the cell boundary.
    pub aabb_min: [f32; 3],
    pub aabb_max: [f32; 3],
    /// First splat index of this chunk within the reordered payload.
    pub offset: u32,
    pub count: u32,
}

/// Describes how a reordered payload is partitioned into grid chunks.
#[derive(Clone, Debug, Default)]
pub struct ChunkTable {
    pub chunk_size: f32,
    pub grid_origin: [f32; 3],
    pub entries: Vec<ChunkEntry>,
    // Floats per splat in the payload this table describes (POINT_STRIDE_FLOATS core
    // plus any higher-SH coefficients appended at import).
    pub stride: usize,
}

/// A reordered payload plus the table describing its chunk layout.
pub struct PartitionedPayload {
    pub payload: Vec<f32>,
    pub table: ChunkTable,
}

/// Partition `payload` (flat, `stride` floats per splat) into a uniform grid of
/// `chunk_size` cells. Splats are reordered so each cell is contiguous and sorted by
/// importance descending (deterministic; ties broken by original index). The core
/// fields (position/scale/color) used for importance/bounds live in the first
/// POINT_STRIDE_FLOATS floats, so `stride` may be larger (appended higher-SH).
pub fn partition_payload(payload: &[f32], chunk_size: f32, stride: usize) -> PartitionedPayload {
    let stride = stride.max(POINT_STRIDE_FLOATS);
    let chunk_size = chunk_size.max(1.0e-4);
    let count = payload.len() / stride;

    if count == 0 {
        return PartitionedPayload {
            payload: Vec::new(),
            table: ChunkTable {
                chunk_size,
                grid_origin: [0.0; 3],
                entries: Vec::new(),
                stride,
            },
        };
    }

    let splat = |i: usize| -> &[f32] { &payload[i * stride..i * stride + stride] };

    // Grid origin = cloud min corner, so cell (0,0,0) starts at the bounds.
    let mut origin = [f32::INFINITY; 3];
    for i in 0..count {
        let s = splat(i);
        for k in 0..3 {
            origin[k] = origin[k].min(s[POS_X + k]);
        }
    }

    // Group splat indices by grid cell.
    let mut cells: HashMap<[i32; 3], Vec<u32>> = HashMap::new();
    for i in 0..count {
        let s = splat(i);
        let cell = [
            ((s[POS_X] - origin[0]) / chunk_size).floor() as i32,
            ((s[POS_X + 1] - origin[1]) / chunk_size).floor() as i32,
            ((s[POS_X + 2] - origin[2]) / chunk_size).floor() as i32,
        ];
        cells.entry(cell).or_default().push(i as u32);
    }

    // Deterministic chunk order: sort cells by grid coordinates.
    let mut keys: Vec<[i32; 3]> = cells.keys().copied().collect();
    keys.sort_unstable();

    let mut out = Vec::with_capacity(payload.len());
    let mut entries = Vec::with_capacity(keys.len());
    let mut offset: u32 = 0;

    for key in keys {
        let mut members = cells.remove(&key).unwrap();
        // Importance descending; ties by original index ascending (total stable order).
        members.sort_by(|&a, &b| {
            let ia = splat_importance(splat(a as usize));
            let ib = splat_importance(splat(b as usize));
            ib.partial_cmp(&ia)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.cmp(&b))
        });

        let mut amin = [f32::INFINITY; 3];
        let mut amax = [f32::NEG_INFINITY; 3];
        let mut max_radius = 0.0_f32;
        for &idx in &members {
            let s = splat(idx as usize);
            for k in 0..3 {
                amin[k] = amin[k].min(s[POS_X + k]);
                amax[k] = amax[k].max(s[POS_X + k]);
            }
            let r = s[SCALE_X]
                .abs()
                .max(s[SCALE_X + 1].abs())
                .max(s[SCALE_X + 2].abs());
            max_radius = max_radius.max(r);
        }
        let grow = max_radius * 3.0;
        for k in 0..3 {
            amin[k] -= grow;
            amax[k] += grow;
        }

        let count_u = members.len() as u32;
        for idx in members {
            out.extend_from_slice(splat(idx as usize));
        }
        entries.push(ChunkEntry {
            grid: key,
            aabb_min: amin,
            aabb_max: amax,
            offset,
            count: count_u,
        });
        offset += count_u;
    }

    PartitionedPayload {
        payload: out,
        table: ChunkTable {
            chunk_size,
            grid_origin: origin,
            entries,
            stride,
        },
    }
}

/// Visual-contribution heuristic for LOD top-K ordering: rendered alpha times
/// footprint (geometric mean of the per-axis linear scales). Larger = keep first.
fn splat_importance(s: &[f32]) -> f32 {
    let alpha = s[COLOR_A];
    let sx = s[SCALE_X].abs();
    let sy = s[SCALE_X + 1].abs();
    let sz = s[SCALE_X + 2].abs();
    let footprint = (sx * sy * sz).max(0.0).cbrt();
    alpha * footprint
}

/// Euclidean distance from a point to an axis-aligned box (0 if inside).
pub fn aabb_distance(p: [f32; 3], min: [f32; 3], max: [f32; 3]) -> f32 {
    let mut sum = 0.0_f32;
    for k in 0..3 {
        let d = (min[k] - p[k]).max(0.0).max(p[k] - max[k]);
        sum += d * d;
    }
    sum.sqrt()
}

/// Center of the union of all chunk bounds (a stable reference for selection when
/// no camera is available yet). `[0,0,0]` for an empty table.
pub fn table_center(table: &ChunkTable) -> [f32; 3] {
    let mut mn = [f32::INFINITY; 3];
    let mut mx = [f32::NEG_INFINITY; 3];
    for e in &table.entries {
        for k in 0..3 {
            mn[k] = mn[k].min(e.aabb_min[k]);
            mx[k] = mx[k].max(e.aabb_max[k]);
        }
    }
    if !mn[0].is_finite() {
        return [0.0; 3];
    }
    [
        (mn[0] + mx[0]) * 0.5,
        (mn[1] + mx[1]) * 0.5,
        (mn[2] + mx[2]) * 0.5,
    ]
}

/// Select the chunks nearest `cam` (by distance to their grown bounds), filling
/// `budget` with full chunks until it runs out, then a top-K (importance-ranked
/// prefix) of the boundary chunk so the budget is used fully. Each result is
/// `(chunk_index, lod_count)`. With an ample budget every chunk is included in full
/// (no detail loss). Returned sorted by chunk index (== payload-offset order) for a
/// cache-friendly gather.
pub fn select_chunks(table: &ChunkTable, cam: [f32; 3], budget: u32) -> Vec<(u32, u32)> {
    let n = table.entries.len();
    if n == 0 {
        return Vec::new();
    }
    let mut order: Vec<u32> = (0..n as u32).collect();
    order.sort_by(|&a, &b| {
        let ea = &table.entries[a as usize];
        let eb = &table.entries[b as usize];
        let da = aabb_distance(cam, ea.aabb_min, ea.aabb_max);
        let db = aabb_distance(cam, eb.aabb_min, eb.aabb_max);
        da.partial_cmp(&db)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(&b))
    });

    let mut selected: Vec<(u32, u32)> = Vec::new();
    let mut remaining = budget;
    for idx in order {
        if remaining == 0 {
            break;
        }
        let count = table.entries[idx as usize].count;
        if count <= remaining {
            selected.push((idx, count));
            remaining -= count;
        } else {
            // Boundary chunk: take its top-K most important splats (the prefix).
            selected.push((idx, remaining));
            break;
        }
    }
    selected.sort_unstable_by_key(|&(idx, _)| idx);
    selected
}

/// Concatenate the first `lod_count` (importance-ranked) splats of each selected
/// `(chunk_index, lod_count)` into one slice (the active render set). Out-of-range
/// chunks are skipped; `lod_count` is clamped to the chunk's size.
pub fn gather_active(payload: &[f32], table: &ChunkTable, active: &[(u32, u32)]) -> Vec<f32> {
    let stride = table.stride.max(POINT_STRIDE_FLOATS);
    let mut total = 0usize;
    for &(ci, lod) in active {
        if let Some(e) = table.entries.get(ci as usize) {
            total += lod.min(e.count) as usize * stride;
        }
    }
    let mut out = Vec::with_capacity(total);
    for &(ci, lod) in active {
        if let Some(e) = table.entries.get(ci as usize) {
            let take = lod.min(e.count);
            let start = e.offset as usize * stride;
            let end = (e.offset as usize + take as usize) * stride;
            if end <= payload.len() {
                out.extend_from_slice(&payload[start..end]);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_splat(pos: [f32; 3], scale: f32, alpha: f32) -> [f32; POINT_STRIDE_FLOATS] {
        let mut s = [0.0_f32; POINT_STRIDE_FLOATS];
        s[POS_X] = pos[0];
        s[POS_X + 1] = pos[1];
        s[POS_X + 2] = pos[2];
        s[6] = 1.0; // quat w
        s[SCALE_X] = scale;
        s[SCALE_X + 1] = scale;
        s[SCALE_X + 2] = scale;
        s[COLOR_A] = alpha;
        s
    }

    fn grid_payload() -> Vec<f32> {
        // 4x4 lattice spanning several 2.0-unit cells.
        let mut p = Vec::new();
        for x in 0..4 {
            for y in 0..4 {
                let s = make_splat([x as f32 * 2.5, y as f32 * 2.5, 0.0], 0.1, 0.5);
                p.extend_from_slice(&s);
            }
        }
        p
    }

    #[test]
    fn empty_payload_yields_empty_table() {
        let part = partition_payload(&[], 2.0, POINT_STRIDE_FLOATS);
        assert!(part.payload.is_empty());
        assert!(part.table.entries.is_empty());
    }

    #[test]
    fn ranges_are_contiguous_and_cover_all() {
        let payload = grid_payload();
        let n = payload.len() / POINT_STRIDE_FLOATS;
        let part = partition_payload(&payload, 2.0, POINT_STRIDE_FLOATS);
        assert!(part.table.entries.len() > 1, "expected multiple chunks");
        let mut expect = 0u32;
        let mut total = 0u32;
        for e in &part.table.entries {
            assert_eq!(e.offset, expect);
            expect += e.count;
            total += e.count;
        }
        assert_eq!(total as usize, n);
        assert_eq!(part.payload.len(), payload.len());
    }

    #[test]
    fn output_is_a_permutation_of_input() {
        let payload = grid_payload();
        let part = partition_payload(&payload, 2.0, POINT_STRIDE_FLOATS);
        let key = |chunk: &[f32]| {
            (
                chunk[POS_X].to_bits(),
                chunk[POS_X + 1].to_bits(),
                chunk[POS_X + 2].to_bits(),
            )
        };
        let mut a: Vec<_> = payload.chunks(POINT_STRIDE_FLOATS).map(key).collect();
        let mut b: Vec<_> = part.payload.chunks(POINT_STRIDE_FLOATS).map(key).collect();
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b);
    }

    #[test]
    fn importance_is_non_increasing_within_each_chunk() {
        let mut payload = Vec::new();
        for (i, (alpha, scale)) in [(0.2, 0.05), (0.9, 0.2), (0.5, 0.1), (0.5, 0.3)]
            .iter()
            .enumerate()
        {
            payload.extend_from_slice(&make_splat([0.1 * i as f32, 0.0, 0.0], *scale, *alpha));
        }
        let part = partition_payload(&payload, 100.0, POINT_STRIDE_FLOATS); // one big cell
        assert_eq!(part.table.entries.len(), 1);
        let count = part.table.entries[0].count as usize;
        let mut prev = f32::INFINITY;
        for i in 0..count {
            let s = &part.payload[i * POINT_STRIDE_FLOATS..(i + 1) * POINT_STRIDE_FLOATS];
            let imp = splat_importance(s);
            assert!(imp <= prev + 1.0e-6, "importance rose: {imp} > {prev}");
            prev = imp;
        }
    }

    #[test]
    fn centers_lie_within_grown_chunk_aabb() {
        let payload = grid_payload();
        let part = partition_payload(&payload, 2.0, POINT_STRIDE_FLOATS);
        for e in &part.table.entries {
            for i in e.offset..e.offset + e.count {
                let s = &part.payload
                    [i as usize * POINT_STRIDE_FLOATS..(i as usize + 1) * POINT_STRIDE_FLOATS];
                for k in 0..3 {
                    assert!(s[POS_X + k] >= e.aabb_min[k] - 1.0e-4);
                    assert!(s[POS_X + k] <= e.aabb_max[k] + 1.0e-4);
                }
            }
        }
    }

    #[test]
    fn select_fills_budget_and_is_offset_sorted() {
        let payload = grid_payload();
        let part = partition_payload(&payload, 2.0, POINT_STRIDE_FLOATS);
        let table = &part.table;
        let total: u32 = table.entries.iter().map(|e| e.count).sum();

        // Ample budget -> every chunk in full (no detail loss).
        let all = select_chunks(table, [0.0, 0.0, 0.0], u32::MAX);
        assert_eq!(all.len(), table.entries.len());
        let all_picked: u32 = all.iter().map(|&(_, lod)| lod).sum();
        assert_eq!(all_picked, total);

        // Reduced budget -> filled exactly, offset-sorted.
        let budget = (total / 3).max(1);
        let sel = select_chunks(table, [0.0, 0.0, 0.0], budget);
        assert!(!sel.is_empty());
        let picked: u32 = sel.iter().map(|&(_, lod)| lod).sum();
        assert_eq!(picked, budget.min(total));
        let mut sorted = sel.clone();
        sorted.sort_unstable_by_key(|&(idx, _)| idx);
        assert_eq!(sorted, sel, "selection must be offset-sorted");
    }

    #[test]
    fn gather_concatenates_selected_ranges() {
        let payload = grid_payload();
        let part = partition_payload(&payload, 2.0, POINT_STRIDE_FLOATS);
        let table = &part.table;
        let active: Vec<(u32, u32)> = vec![(0, 1), (2, 1)];
        let gathered = gather_active(&part.payload, table, &active);
        let expect: usize = active
            .iter()
            .map(|&(i, lod)| {
                lod.min(table.entries[i as usize].count) as usize * POINT_STRIDE_FLOATS
            })
            .sum();
        assert_eq!(gathered.len(), expect);
        let e0 = &table.entries[0];
        let start = e0.offset as usize * POINT_STRIDE_FLOATS;
        assert_eq!(
            &gathered[0..POINT_STRIDE_FLOATS],
            &part.payload[start..start + POINT_STRIDE_FLOATS]
        );
    }

    #[test]
    fn boundary_chunk_is_partially_taken() {
        // One chunk of 10 splats: a budget of 3 takes the top-3 importance prefix;
        // an ample budget takes all 10.
        let mut payload = Vec::new();
        for i in 0..10 {
            payload.extend_from_slice(&make_splat([0.01 * i as f32, 0.0, 0.0], 0.1, 0.5));
        }
        let part = partition_payload(&payload, 100.0, POINT_STRIDE_FLOATS);
        assert_eq!(part.table.entries.len(), 1);
        assert_eq!(
            select_chunks(&part.table, [0.0, 0.0, 0.0], 3),
            vec![(0u32, 3u32)]
        );
        assert_eq!(
            select_chunks(&part.table, [0.0, 0.0, 0.0], u32::MAX),
            vec![(0u32, 10u32)]
        );
    }
}
