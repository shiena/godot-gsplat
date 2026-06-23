//! Disk-backed splat payload packs for large scenes.
//!
//! Packs store quantized RGBA8 splat records so the renderer can upload selected
//! pages without expanding them back to RGBAF.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::chunking::{ChunkEntry, ChunkTable};
use crate::import_state::POINT_STRIDE_FLOATS;

const MAGIC: &[u8; 8] = b"GSPACK1\0";
const VERSION: u32 = 3;
pub const DEFAULT_PAGE_SPLATS: u32 = 16_384;
const HEADER_SIZE: u64 = 160;
const CHUNK_RECORD_SIZE: u64 = 64;
const PAGE_RECORD_SIZE: u64 = 32;
const NODE_LOD_RECORD_SIZE: u64 = 32;
const ENCODING_PACKED_RGBA8: u32 = 1;
const DEFAULT_LOD_LEVELS: u32 = 4;

#[derive(Clone, Debug)]
pub struct PackChunk {
    pub entry: ChunkEntry,
    pub first_page: u32,
    pub page_count: u32,
}

#[derive(Clone, Debug)]
pub struct PackPage {
    pub chunk_index: u32,
    pub lod_index: u32,
    pub start_in_lod: u32,
    pub count: u32,
    pub byte_offset: u64,
    pub byte_len: u32,
}

#[derive(Clone, Debug)]
pub struct PackNodeLod {
    pub chunk_index: u32,
    pub lod_level: u32,
    pub count: u32,
    pub first_page: u32,
    pub page_count: u32,
}

#[derive(Clone, Debug)]
pub struct GsplatPackIndex {
    pub path: PathBuf,
    pub point_count: u32,
    pub stride: usize,
    pub record_bytes: usize,
    pub sh_degree_available: i32,
    pub chunk_size: f32,
    pub grid_origin: [f32; 3],
    pub position_min: [f32; 3],
    pub position_max: [f32; 3],
    pub scale_min: [f32; 3],
    pub scale_max: [f32; 3],
    pub sh_min: f32,
    pub sh_max: f32,
    pub lod_levels: u32,
    pub chunks: Vec<PackChunk>,
    pub lods: Vec<PackNodeLod>,
    pub pages: Vec<PackPage>,
}

struct PackHeader {
    point_count: u32,
    stride: u32,
    record_bytes: u32,
    sh_degree_available: u32,
    chunk_size: f32,
    grid_origin: [f32; 3],
    position_min: [f32; 3],
    position_max: [f32; 3],
    scale_min: [f32; 3],
    scale_max: [f32; 3],
    sh_min: f32,
    sh_max: f32,
    chunk_count: u32,
    node_lod_count: u32,
    lod_levels: u32,
    page_count: u32,
    chunk_table_offset: u64,
    node_lod_table_offset: u64,
    page_table_offset: u64,
    payload_offset: u64,
}

impl GsplatPackIndex {
    pub fn chunk_table(&self) -> ChunkTable {
        ChunkTable {
            chunk_size: self.chunk_size,
            grid_origin: self.grid_origin,
            entries: self
                .chunks
                .iter()
                .map(|chunk| chunk.entry.clone())
                .collect(),
            stride: self.stride,
        }
    }

    pub fn read_page_bytes(&self, page_index: u32) -> Result<Vec<u8>, String> {
        let page = self
            .pages
            .get(page_index as usize)
            .ok_or_else(|| format!("Pack page {page_index} is out of range."))?;
        let mut file = File::open(&self.path)
            .map_err(|err| format!("Failed to open pack '{}': {err}", self.path.display()))?;
        file.seek(SeekFrom::Start(page.byte_offset))
            .map_err(|err| format!("Failed to seek pack page {page_index}: {err}"))?;
        let mut bytes = vec![0_u8; page.byte_len as usize];
        file.read_exact(&mut bytes)
            .map_err(|err| format!("Failed to read pack page {page_index}: {err}"))?;
        Ok(bytes)
    }

    pub fn best_lod_for_request(
        &self,
        chunk_index: u32,
        requested_count: u32,
    ) -> Option<&PackNodeLod> {
        let mut smallest: Option<&PackNodeLod> = None;
        let mut best_under_budget: Option<&PackNodeLod> = None;
        for lod in self
            .lods
            .iter()
            .filter(|lod| lod.chunk_index == chunk_index)
        {
            if smallest.is_none_or(|current| lod.count < current.count) {
                smallest = Some(lod);
            }
            if lod.count <= requested_count
                && best_under_budget.is_none_or(|current| lod.count > current.count)
            {
                best_under_budget = Some(lod);
            }
        }
        best_under_budget.or(smallest)
    }

    pub fn select_lods_view_priority(
        &self,
        cam: [f32; 3],
        forward: [f32; 3],
        fov_degrees: f32,
        full_distance: f32,
        budget: u32,
    ) -> Vec<(u32, u32)> {
        if self.chunks.is_empty() || self.lods.is_empty() || budget == 0 || full_distance <= 0.0 {
            return Vec::new();
        }

        let forward = normalize_or(forward, [0.0, 0.0, -1.0]);
        let fov_degrees = fov_degrees.clamp(1.0, 360.0);
        let cos_half_fov = (fov_degrees.to_radians() * 0.5).cos();
        let include_all_angles = fov_degrees >= 359.999;

        let mut candidates = Vec::new();
        for (idx, chunk) in self.chunks.iter().enumerate() {
            let distance = aabb_distance(cam, chunk.entry.aabb_min, chunk.entry.aabb_max);
            if distance > full_distance {
                continue;
            }

            let center = aabb_center(chunk.entry.aabb_min, chunk.entry.aabb_max);
            let to_center = [center[0] - cam[0], center[1] - cam[1], center[2] - cam[2]];
            let dir = normalize_or(to_center, forward);
            let center_dot = dot3(forward, dir).clamp(-1.0, 1.0);
            if !include_all_angles && center_dot < cos_half_fov {
                continue;
            }

            let lods = self.chunk_lods(idx as u32);
            let Some(min_lod) = lods.iter().min_by_key(|lod| lod.count).copied() else {
                continue;
            };
            let distance_score = 1.0 / (1.0 + distance.max(0.0));
            let angle_score = (center_dot + 1.0) * 0.5;
            let count_score = (chunk.entry.count.max(1) as f32).ln() * 0.01;
            let priority = distance_score * 2.0 + angle_score * 2.0 + count_score;
            candidates.push(LodSelectionCandidate {
                chunk_index: idx as u32,
                priority,
                selected_count: min_lod.count,
                lod_counts: lods.into_iter().map(|lod| lod.count).collect(),
            });
        }

        if candidates.is_empty() {
            return Vec::new();
        }

        candidates.sort_by(high_lod_priority_first);
        let mut total = 0_u64;
        let mut selected = Vec::new();
        for mut candidate in candidates {
            if total + candidate.selected_count as u64 > budget as u64 {
                continue;
            }
            total += candidate.selected_count as u64;
            candidate.lod_counts.sort_unstable();
            candidate.lod_counts.dedup();
            selected.push(candidate);
        }

        for candidate in &mut selected {
            for count in candidate.lod_counts.iter().copied().rev() {
                if count <= candidate.selected_count {
                    continue;
                }
                let extra = (count - candidate.selected_count) as u64;
                if total + extra <= budget as u64 {
                    total += extra;
                    candidate.selected_count = count;
                    break;
                }
            }
        }

        selected.sort_unstable_by_key(|candidate| candidate.chunk_index);
        selected
            .into_iter()
            .map(|candidate| (candidate.chunk_index, candidate.selected_count))
            .collect()
    }

    fn chunk_lods(&self, chunk_index: u32) -> Vec<&PackNodeLod> {
        let Some(chunk) = self.chunks.get(chunk_index as usize) else {
            return Vec::new();
        };
        self.lods
            .iter()
            .skip_while(|lod| lod.chunk_index < chunk_index)
            .take_while(|lod| lod.chunk_index == chunk_index)
            .filter(|lod| lod.count > 0 && lod.first_page >= chunk.first_page)
            .collect()
    }
}

struct LodSelectionCandidate {
    chunk_index: u32,
    priority: f32,
    selected_count: u32,
    lod_counts: Vec<u32>,
}

fn high_lod_priority_first(
    a: &LodSelectionCandidate,
    b: &LodSelectionCandidate,
) -> std::cmp::Ordering {
    b.priority
        .partial_cmp(&a.priority)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then(a.chunk_index.cmp(&b.chunk_index))
}

pub fn write_pack(
    path: impl AsRef<Path>,
    payload: &[f32],
    table: &ChunkTable,
    sh_degree_available: i32,
    page_splats: u32,
) -> Result<GsplatPackIndex, String> {
    let path = path.as_ref();
    let stride = table.stride.max(POINT_STRIDE_FLOATS);
    let sh_degree_available = sh_degree_available
        .clamp(0, max_sh_degree_for_stride(stride))
        .max(0);
    if stride == 0 || !payload.len().is_multiple_of(stride) {
        return Err("Pack payload length is not aligned to the chunk stride.".to_string());
    }
    let page_splats = page_splats.max(1);
    let point_count = (payload.len() / stride) as u32;
    let mut chunks = Vec::with_capacity(table.entries.len());
    let mut lods = Vec::new();
    let mut pages = Vec::new();

    for (chunk_index, entry) in table.entries.iter().enumerate() {
        let chunk_first_page = pages.len() as u32;
        let lod_counts = lod_counts_for_chunk(entry.count, DEFAULT_LOD_LEVELS);
        for (lod_level, &lod_count) in lod_counts.iter().enumerate() {
            let lod_index = lods.len() as u32;
            let first_page = pages.len() as u32;
            let mut start = 0_u32;
            while start < lod_count {
                let count = (lod_count - start).min(page_splats);
                pages.push(PackPage {
                    chunk_index: chunk_index as u32,
                    lod_index,
                    start_in_lod: start,
                    count,
                    byte_offset: 0,
                    byte_len: count
                        .checked_mul(stride as u32)
                        .and_then(|v| v.checked_mul(4))
                        .ok_or_else(|| "Pack page byte length overflowed.".to_string())?,
                });
                start += count;
            }
            lods.push(PackNodeLod {
                chunk_index: chunk_index as u32,
                lod_level: lod_level as u32,
                count: lod_count,
                first_page,
                page_count: pages.len() as u32 - first_page,
            });
        }
        chunks.push(PackChunk {
            entry: entry.clone(),
            first_page: chunk_first_page,
            page_count: pages.len() as u32 - chunk_first_page,
        });
    }

    let mut lod_source_indices = Vec::with_capacity(lods.len());
    for lod in &lods {
        let entry = table
            .entries
            .get(lod.chunk_index as usize)
            .ok_or_else(|| "Pack LOD references an invalid chunk.".to_string())?;
        lod_source_indices.push(select_lod_source_indices(
            payload,
            stride,
            entry,
            lod.count as usize,
        ));
    }

    let ranges = QuantizationRanges::from_payload(payload, stride, sh_degree_available);
    let record_bytes = packed_record_bytes(sh_degree_available);

    let chunk_table_offset = HEADER_SIZE;
    let node_lod_table_offset = chunk_table_offset + CHUNK_RECORD_SIZE * chunks.len() as u64;
    let page_table_offset = node_lod_table_offset + NODE_LOD_RECORD_SIZE * lods.len() as u64;
    let payload_offset = page_table_offset + PAGE_RECORD_SIZE * pages.len() as u64;
    let mut cursor = payload_offset;
    for page in &mut pages {
        page.byte_offset = cursor;
        page.byte_len = page
            .count
            .checked_mul(record_bytes as u32)
            .ok_or_else(|| "Pack page byte length overflowed.".to_string())?;
        cursor += page.byte_len as u64;
    }

    let mut file = File::create(path)
        .map_err(|err| format!("Failed to create pack '{}': {err}", path.display()))?;
    let header = PackHeader {
        point_count,
        stride: stride as u32,
        record_bytes: record_bytes as u32,
        sh_degree_available: sh_degree_available.max(0) as u32,
        chunk_size: table.chunk_size,
        grid_origin: table.grid_origin,
        position_min: ranges.position_min,
        position_max: ranges.position_max,
        scale_min: ranges.scale_min,
        scale_max: ranges.scale_max,
        sh_min: ranges.sh_min,
        sh_max: ranges.sh_max,
        chunk_count: chunks.len() as u32,
        node_lod_count: lods.len() as u32,
        lod_levels: DEFAULT_LOD_LEVELS,
        page_count: pages.len() as u32,
        chunk_table_offset,
        node_lod_table_offset,
        page_table_offset,
        payload_offset,
    };
    write_header(&mut file, &header)?;
    for chunk in &chunks {
        write_chunk_record(&mut file, chunk)?;
    }
    for lod in &lods {
        write_node_lod_record(&mut file, lod)?;
    }
    for page in &pages {
        write_page_record(&mut file, page)?;
    }
    for page in &pages {
        let source_indices = lod_source_indices
            .get(page.lod_index as usize)
            .ok_or_else(|| "Pack page references an invalid LOD.".to_string())?;
        let start = page.start_in_lod as usize;
        let end = start + page.count as usize;
        let page_indices = source_indices
            .get(start..end)
            .ok_or_else(|| "Pack page range exceeds LOD length.".to_string())?;
        write_packed_records_by_index(
            &mut file,
            payload,
            stride,
            page_indices,
            sh_degree_available,
            &ranges,
        )?;
    }

    read_pack_index(path)
}

pub fn read_pack_index(path: impl AsRef<Path>) -> Result<GsplatPackIndex, String> {
    let path = path.as_ref();
    let mut file = File::open(path)
        .map_err(|err| format!("Failed to open pack '{}': {err}", path.display()))?;
    let mut header = vec![0_u8; HEADER_SIZE as usize];
    file.read_exact(&mut header)
        .map_err(|err| format!("Failed to read pack header: {err}"))?;
    if &header[0..8] != MAGIC {
        return Err("Invalid gsplat pack magic.".to_string());
    }
    let version = read_u32_at(&header, 8)?;
    if version != VERSION {
        return Err(format!("Unsupported gsplat pack version {version}."));
    }
    let point_count = read_u32_at(&header, 12)?;
    let stride = read_u32_at(&header, 16)? as usize;
    let sh_degree_available = read_u32_at(&header, 20)? as i32;
    let chunk_size = read_f32_at(&header, 24)?;
    let grid_origin = [
        read_f32_at(&header, 28)?,
        read_f32_at(&header, 32)?,
        read_f32_at(&header, 36)?,
    ];
    let chunk_count = read_u32_at(&header, 40)? as usize;
    let page_count = read_u32_at(&header, 44)? as usize;
    let chunk_table_offset = read_u64_at(&header, 48)?;
    let page_table_offset = read_u64_at(&header, 56)?;
    let payload_offset = read_u64_at(&header, 64)?;
    let node_lod_table_offset = read_u64_at(&header, 72)?;
    let node_lod_count = read_u32_at(&header, 80)? as usize;
    let lod_levels = read_u32_at(&header, 84)?;
    let encoding = read_u32_at(&header, 96)?;
    if encoding != ENCODING_PACKED_RGBA8 {
        return Err(format!("Unsupported gsplat pack encoding {encoding}."));
    }
    let parsed = PackHeader {
        point_count,
        stride: stride as u32,
        record_bytes: read_u32_at(&header, 100)?,
        sh_degree_available: sh_degree_available.max(0) as u32,
        chunk_size,
        grid_origin,
        position_min: [
            read_f32_at(&header, 104)?,
            read_f32_at(&header, 108)?,
            read_f32_at(&header, 112)?,
        ],
        position_max: [
            read_f32_at(&header, 116)?,
            read_f32_at(&header, 120)?,
            read_f32_at(&header, 124)?,
        ],
        scale_min: [
            read_f32_at(&header, 128)?,
            read_f32_at(&header, 132)?,
            read_f32_at(&header, 136)?,
        ],
        scale_max: [
            read_f32_at(&header, 140)?,
            read_f32_at(&header, 144)?,
            read_f32_at(&header, 148)?,
        ],
        sh_min: read_f32_at(&header, 152)?,
        sh_max: read_f32_at(&header, 156)?,
        chunk_count: chunk_count as u32,
        node_lod_count: node_lod_count as u32,
        lod_levels,
        page_count: page_count as u32,
        chunk_table_offset,
        node_lod_table_offset,
        page_table_offset,
        payload_offset,
    };

    file.seek(SeekFrom::Start(parsed.chunk_table_offset))
        .map_err(|err| format!("Failed to seek pack chunk table: {err}"))?;
    let mut chunks = Vec::with_capacity(chunk_count);
    for _ in 0..chunk_count {
        chunks.push(read_chunk_record(&mut file)?);
    }

    file.seek(SeekFrom::Start(parsed.node_lod_table_offset))
        .map_err(|err| format!("Failed to seek pack node LOD table: {err}"))?;
    let mut lods = Vec::with_capacity(node_lod_count);
    for _ in 0..node_lod_count {
        lods.push(read_node_lod_record(&mut file)?);
    }

    file.seek(SeekFrom::Start(parsed.page_table_offset))
        .map_err(|err| format!("Failed to seek pack page table: {err}"))?;
    let mut pages = Vec::with_capacity(page_count);
    for _ in 0..page_count {
        pages.push(read_page_record(&mut file)?);
    }

    Ok(GsplatPackIndex {
        path: path.to_path_buf(),
        point_count: parsed.point_count,
        stride: parsed.stride as usize,
        record_bytes: parsed.record_bytes as usize,
        sh_degree_available: parsed.sh_degree_available as i32,
        chunk_size: parsed.chunk_size,
        grid_origin: parsed.grid_origin,
        position_min: parsed.position_min,
        position_max: parsed.position_max,
        scale_min: parsed.scale_min,
        scale_max: parsed.scale_max,
        sh_min: parsed.sh_min,
        sh_max: parsed.sh_max,
        lod_levels: parsed.lod_levels,
        chunks,
        lods,
        pages,
    })
}

fn write_header(file: &mut File, header: &PackHeader) -> Result<(), String> {
    file.write_all(MAGIC)
        .map_err(|err| format!("Failed to write pack header: {err}"))?;
    write_u32(file, VERSION)?;
    write_u32(file, header.point_count)?;
    write_u32(file, header.stride)?;
    write_u32(file, header.sh_degree_available)?;
    write_f32(file, header.chunk_size)?;
    for v in header.grid_origin {
        write_f32(file, v)?;
    }
    write_u32(file, header.chunk_count)?;
    write_u32(file, header.page_count)?;
    write_u64(file, header.chunk_table_offset)?;
    write_u64(file, header.page_table_offset)?;
    write_u64(file, header.payload_offset)?;
    write_u64(file, header.node_lod_table_offset)?;
    write_u32(file, header.node_lod_count)?;
    write_u32(file, header.lod_levels)?;
    write_padding(file, 8)?;
    write_u32(file, ENCODING_PACKED_RGBA8)?;
    write_u32(file, header.record_bytes)?;
    for v in header.position_min {
        write_f32(file, v)?;
    }
    for v in header.position_max {
        write_f32(file, v)?;
    }
    for v in header.scale_min {
        write_f32(file, v)?;
    }
    for v in header.scale_max {
        write_f32(file, v)?;
    }
    write_f32(file, header.sh_min)?;
    write_f32(file, header.sh_max)?;
    Ok(())
}

fn write_chunk_record(file: &mut File, chunk: &PackChunk) -> Result<(), String> {
    for v in chunk.entry.grid {
        write_i32(file, v)?;
    }
    for v in chunk.entry.aabb_min {
        write_f32(file, v)?;
    }
    for v in chunk.entry.aabb_max {
        write_f32(file, v)?;
    }
    write_u32(file, chunk.entry.offset)?;
    write_u32(file, chunk.entry.count)?;
    write_u32(file, chunk.first_page)?;
    write_u32(file, chunk.page_count)?;
    write_padding(file, 12)
}

fn read_chunk_record(file: &mut File) -> Result<PackChunk, String> {
    let mut bytes = [0_u8; CHUNK_RECORD_SIZE as usize];
    file.read_exact(&mut bytes)
        .map_err(|err| format!("Failed to read pack chunk record: {err}"))?;
    Ok(PackChunk {
        entry: ChunkEntry {
            grid: [
                read_i32_at(&bytes, 0)?,
                read_i32_at(&bytes, 4)?,
                read_i32_at(&bytes, 8)?,
            ],
            aabb_min: [
                read_f32_at(&bytes, 12)?,
                read_f32_at(&bytes, 16)?,
                read_f32_at(&bytes, 20)?,
            ],
            aabb_max: [
                read_f32_at(&bytes, 24)?,
                read_f32_at(&bytes, 28)?,
                read_f32_at(&bytes, 32)?,
            ],
            offset: read_u32_at(&bytes, 36)?,
            count: read_u32_at(&bytes, 40)?,
        },
        first_page: read_u32_at(&bytes, 44)?,
        page_count: read_u32_at(&bytes, 48)?,
    })
}

fn write_node_lod_record(file: &mut File, lod: &PackNodeLod) -> Result<(), String> {
    write_u32(file, lod.chunk_index)?;
    write_u32(file, lod.lod_level)?;
    write_u32(file, lod.count)?;
    write_u32(file, lod.first_page)?;
    write_u32(file, lod.page_count)?;
    write_padding(file, 12)
}

fn read_node_lod_record(file: &mut File) -> Result<PackNodeLod, String> {
    let mut bytes = [0_u8; NODE_LOD_RECORD_SIZE as usize];
    file.read_exact(&mut bytes)
        .map_err(|err| format!("Failed to read pack node LOD record: {err}"))?;
    Ok(PackNodeLod {
        chunk_index: read_u32_at(&bytes, 0)?,
        lod_level: read_u32_at(&bytes, 4)?,
        count: read_u32_at(&bytes, 8)?,
        first_page: read_u32_at(&bytes, 12)?,
        page_count: read_u32_at(&bytes, 16)?,
    })
}

fn write_page_record(file: &mut File, page: &PackPage) -> Result<(), String> {
    write_u32(file, page.chunk_index)?;
    write_u32(file, page.start_in_lod)?;
    write_u32(file, page.count)?;
    write_u32(file, page.lod_index)?;
    write_u64(file, page.byte_offset)?;
    write_u32(file, page.byte_len)?;
    write_u32(file, 0)
}

fn read_page_record(file: &mut File) -> Result<PackPage, String> {
    let mut bytes = [0_u8; PAGE_RECORD_SIZE as usize];
    file.read_exact(&mut bytes)
        .map_err(|err| format!("Failed to read pack page record: {err}"))?;
    Ok(PackPage {
        chunk_index: read_u32_at(&bytes, 0)?,
        start_in_lod: read_u32_at(&bytes, 4)?,
        count: read_u32_at(&bytes, 8)?,
        lod_index: read_u32_at(&bytes, 12)?,
        byte_offset: read_u64_at(&bytes, 16)?,
        byte_len: read_u32_at(&bytes, 24)?,
    })
}

fn lod_counts_for_chunk(count: u32, max_levels: u32) -> Vec<u32> {
    if count == 0 {
        return Vec::new();
    }
    let mut counts = Vec::new();
    let mut current = count;
    for _ in 0..max_levels.max(1) {
        if counts.last().copied() != Some(current) {
            counts.push(current);
        }
        if current <= 1 {
            break;
        }
        current = current.div_ceil(2);
    }
    counts
}

fn select_lod_source_indices(
    payload: &[f32],
    stride: usize,
    entry: &ChunkEntry,
    target_count: usize,
) -> Vec<u32> {
    let full_count = entry.count as usize;
    let target_count = target_count.min(full_count);
    if target_count == 0 {
        return Vec::new();
    }
    if target_count >= full_count {
        return (0..entry.count).map(|local| entry.offset + local).collect();
    }

    let dim = (target_count as f32).cbrt().ceil().max(1.0) as i32;
    let (center_min, center_max) = chunk_center_bounds(payload, stride, entry);
    let extent = [
        (center_max[0] - center_min[0]).max(f32::EPSILON),
        (center_max[1] - center_min[1]).max(f32::EPSILON),
        (center_max[2] - center_min[2]).max(f32::EPSILON),
    ];
    let mut occupied = HashSet::new();
    let mut selected_set = HashSet::new();
    let mut selected = Vec::with_capacity(target_count);
    for local in 0..entry.count {
        let index = entry.offset + local;
        let start = index as usize * stride;
        let splat = &payload[start..start + stride];
        let key = [
            (((splat[0] - center_min[0]) / extent[0]) * dim as f32)
                .floor()
                .clamp(0.0, (dim - 1) as f32) as i32,
            (((splat[1] - center_min[1]) / extent[1]) * dim as f32)
                .floor()
                .clamp(0.0, (dim - 1) as f32) as i32,
            (((splat[2] - center_min[2]) / extent[2]) * dim as f32)
                .floor()
                .clamp(0.0, (dim - 1) as f32) as i32,
        ];
        if occupied.insert(key) {
            selected_set.insert(index);
            selected.push(index);
            if selected.len() == target_count {
                return selected;
            }
        }
    }
    for local in 0..entry.count {
        let index = entry.offset + local;
        if selected_set.insert(index) {
            selected.push(index);
            if selected.len() == target_count {
                break;
            }
        }
    }
    selected
}

fn chunk_center_bounds(payload: &[f32], stride: usize, entry: &ChunkEntry) -> ([f32; 3], [f32; 3]) {
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for local in 0..entry.count {
        let index = entry.offset + local;
        let start = index as usize * stride;
        let Some(splat) = payload.get(start..start + 3) else {
            continue;
        };
        for k in 0..3 {
            min[k] = min[k].min(splat[k]);
            max[k] = max[k].max(splat[k]);
        }
    }
    if !min[0].is_finite() {
        return (entry.aabb_min, entry.aabb_max);
    }
    (min, max)
}

fn aabb_distance(p: [f32; 3], min: [f32; 3], max: [f32; 3]) -> f32 {
    let mut sum = 0.0_f32;
    for k in 0..3 {
        let d = (min[k] - p[k]).max(0.0).max(p[k] - max[k]);
        sum += d * d;
    }
    sum.sqrt()
}

fn aabb_center(min: [f32; 3], max: [f32; 3]) -> [f32; 3] {
    [
        (min[0] + max[0]) * 0.5,
        (min[1] + max[1]) * 0.5,
        (min[2] + max[2]) * 0.5,
    ]
}

fn normalize_or(v: [f32; 3], fallback: [f32; 3]) -> [f32; 3] {
    let len_sq = dot3(v, v);
    if len_sq <= 1.0e-12 {
        return fallback;
    }
    let inv_len = len_sq.sqrt().recip();
    [v[0] * inv_len, v[1] * inv_len, v[2] * inv_len]
}

fn dot3(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn sh_floats(degree: i32) -> usize {
    match degree.clamp(0, 3) {
        0 => 0,
        1 => 9,
        2 => 24,
        _ => 45,
    }
}

fn max_sh_degree_for_stride(stride: usize) -> i32 {
    match stride.saturating_sub(POINT_STRIDE_FLOATS) {
        extra if extra >= 45 => 3,
        extra if extra >= 24 => 2,
        extra if extra >= 9 => 1,
        _ => 0,
    }
}

pub fn packed_record_bytes(sh_degree_available: i32) -> usize {
    (24 + sh_floats(sh_degree_available)).next_multiple_of(4)
}

#[derive(Clone, Debug)]
struct QuantizationRanges {
    position_min: [f32; 3],
    position_max: [f32; 3],
    scale_min: [f32; 3],
    scale_max: [f32; 3],
    sh_min: f32,
    sh_max: f32,
}

impl QuantizationRanges {
    fn from_payload(payload: &[f32], stride: usize, sh_degree_available: i32) -> Self {
        let mut ranges = Self {
            position_min: [f32::INFINITY; 3],
            position_max: [f32::NEG_INFINITY; 3],
            scale_min: [f32::INFINITY; 3],
            scale_max: [f32::NEG_INFINITY; 3],
            sh_min: f32::INFINITY,
            sh_max: f32::NEG_INFINITY,
        };
        let sh_count = sh_floats(sh_degree_available);
        for splat in payload.chunks_exact(stride) {
            for k in 0..3 {
                ranges.position_min[k] = ranges.position_min[k].min(splat[k]);
                ranges.position_max[k] = ranges.position_max[k].max(splat[k]);
                ranges.scale_min[k] = ranges.scale_min[k].min(splat[7 + k]);
                ranges.scale_max[k] = ranges.scale_max[k].max(splat[7 + k]);
            }
            for &value in &splat[18..18 + sh_count] {
                ranges.sh_min = ranges.sh_min.min(value);
                ranges.sh_max = ranges.sh_max.max(value);
            }
        }
        for k in 0..3 {
            if !ranges.position_min[k].is_finite()
                || (ranges.position_max[k] - ranges.position_min[k]).abs() <= f32::EPSILON
            {
                ranges.position_min[k] = 0.0;
                ranges.position_max[k] = 1.0;
            }
            if !ranges.scale_min[k].is_finite()
                || (ranges.scale_max[k] - ranges.scale_min[k]).abs() <= f32::EPSILON
            {
                ranges.scale_min[k] = 0.0;
                ranges.scale_max[k] = 1.0;
            }
        }
        if sh_count == 0
            || !ranges.sh_min.is_finite()
            || (ranges.sh_max - ranges.sh_min).abs() <= f32::EPSILON
        {
            ranges.sh_min = -1.0;
            ranges.sh_max = 1.0;
        }
        ranges
    }
}

fn write_packed_records_by_index(
    file: &mut File,
    payload: &[f32],
    stride: usize,
    indices: &[u32],
    sh_degree_available: i32,
    ranges: &QuantizationRanges,
) -> Result<(), String> {
    let record_bytes = packed_record_bytes(sh_degree_available);
    let sh_count = sh_floats(sh_degree_available);
    let mut record = vec![0_u8; record_bytes];
    for &index in indices {
        let start = index as usize * stride;
        let splat = payload
            .get(start..start + stride)
            .ok_or_else(|| "Pack source index exceeds payload length.".to_string())?;
        record.fill(0);
        for k in 0..3 {
            write_u16_bytes(
                &mut record,
                k * 2,
                quantize_u16(splat[k], ranges.position_min[k], ranges.position_max[k]),
            );
            write_u16_bytes(
                &mut record,
                6 + k * 2,
                quantize_u16(splat[7 + k], ranges.scale_min[k], ranges.scale_max[k]),
            );
        }
        for k in 0..4 {
            write_i16_bytes(&mut record, 12 + k * 2, quantize_snorm_i16(splat[3 + k]));
        }
        for k in 0..4 {
            record[20 + k] = quantize_u8(splat[14 + k], 0.0, 1.0);
        }
        for k in 0..sh_count {
            record[24 + k] = quantize_u8(splat[18 + k], ranges.sh_min, ranges.sh_max);
        }
        file.write_all(&record)
            .map_err(|err| format!("Failed to write packed splat payload: {err}"))?;
    }
    Ok(())
}

fn quantize_u16(value: f32, min: f32, max: f32) -> u16 {
    let t = ((value - min) / (max - min).max(f32::EPSILON)).clamp(0.0, 1.0);
    (t * 65535.0 + 0.5) as u16
}

fn quantize_u8(value: f32, min: f32, max: f32) -> u8 {
    let t = ((value - min) / (max - min).max(f32::EPSILON)).clamp(0.0, 1.0);
    (t * 255.0 + 0.5) as u8
}

fn quantize_snorm_i16(value: f32) -> i16 {
    (value.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

fn write_u16_bytes(out: &mut [u8], offset: usize, value: u16) {
    let bytes = value.to_le_bytes();
    out[offset] = bytes[0];
    out[offset + 1] = bytes[1];
}

fn write_i16_bytes(out: &mut [u8], offset: usize, value: i16) {
    let bytes = value.to_le_bytes();
    out[offset] = bytes[0];
    out[offset + 1] = bytes[1];
}

fn write_padding(file: &mut File, len: usize) -> Result<(), String> {
    if len > 0 {
        file.write_all(&vec![0_u8; len])
            .map_err(|err| format!("Failed to write pack padding: {err}"))?;
    }
    Ok(())
}

fn write_u32(file: &mut File, value: u32) -> Result<(), String> {
    file.write_all(&value.to_le_bytes())
        .map_err(|err| format!("Failed to write pack u32: {err}"))
}

fn write_i32(file: &mut File, value: i32) -> Result<(), String> {
    file.write_all(&value.to_le_bytes())
        .map_err(|err| format!("Failed to write pack i32: {err}"))
}

fn write_u64(file: &mut File, value: u64) -> Result<(), String> {
    file.write_all(&value.to_le_bytes())
        .map_err(|err| format!("Failed to write pack u64: {err}"))
}

fn write_f32(file: &mut File, value: f32) -> Result<(), String> {
    file.write_all(&value.to_le_bytes())
        .map_err(|err| format!("Failed to write pack f32: {err}"))
}

fn read_u32_at(bytes: &[u8], offset: usize) -> Result<u32, String> {
    Ok(u32::from_le_bytes(read_array(bytes, offset)?))
}

fn read_i32_at(bytes: &[u8], offset: usize) -> Result<i32, String> {
    Ok(i32::from_le_bytes(read_array(bytes, offset)?))
}

fn read_u64_at(bytes: &[u8], offset: usize) -> Result<u64, String> {
    Ok(u64::from_le_bytes(read_array(bytes, offset)?))
}

fn read_f32_at(bytes: &[u8], offset: usize) -> Result<f32, String> {
    Ok(f32::from_le_bytes(read_array(bytes, offset)?))
}

fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], String> {
    let end = offset + N;
    let slice = bytes
        .get(offset..end)
        .ok_or_else(|| "Pack record is truncated.".to_string())?;
    slice
        .try_into()
        .map_err(|_| "Pack record has invalid byte length.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunking::ChunkEntry;

    #[test]
    fn pack_round_trip_preserves_index_and_page_payload() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "godot_gsplat_pack_test_{}.gsplatpack",
            std::process::id()
        ));
        let stride = POINT_STRIDE_FLOATS;
        let table = ChunkTable {
            chunk_size: 2.0,
            grid_origin: [0.0, 0.0, 0.0],
            entries: vec![
                ChunkEntry {
                    grid: [0, 0, 0],
                    aabb_min: [0.0, 0.0, 0.0],
                    aabb_max: [1.0, 1.0, 1.0],
                    offset: 0,
                    count: 3,
                },
                ChunkEntry {
                    grid: [1, 0, 0],
                    aabb_min: [2.0, 0.0, 0.0],
                    aabb_max: [3.0, 1.0, 1.0],
                    offset: 3,
                    count: 2,
                },
            ],
            stride,
        };
        let mut payload = Vec::new();
        for point in 0..5 {
            for field in 0..stride {
                payload.push((point * 100 + field) as f32);
            }
        }

        let pack = write_pack(&path, &payload, &table, 3, 2).expect("write pack");
        assert_eq!(pack.point_count, 5);
        assert_eq!(pack.record_bytes, packed_record_bytes(0));
        assert_eq!(pack.chunks.len(), 2);
        assert_eq!(pack.lods.len(), 5);
        assert_eq!(pack.pages.len(), 6);
        assert_eq!(pack.chunks[0].page_count, 4);
        assert_eq!(pack.chunks[1].page_count, 2);
        assert_eq!(pack.best_lod_for_request(0, 2).unwrap().count, 2);
        assert_eq!(pack.best_lod_for_request(1, 1).unwrap().count, 1);

        let first_page = pack.read_page_bytes(0).expect("read page");
        assert_eq!(first_page.len(), 2 * pack.record_bytes);

        let second_chunk_page = pack.read_page_bytes(4).expect("read page");
        assert_eq!(second_chunk_page.len(), 2 * pack.record_bytes);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn view_priority_lod_selection_upgrades_high_priority_nodes_within_budget() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "godot_gsplat_lod_select_test_{}.gsplatpack",
            std::process::id()
        ));
        let stride = POINT_STRIDE_FLOATS;
        let table = ChunkTable {
            chunk_size: 2.0,
            grid_origin: [0.0, 0.0, 0.0],
            entries: vec![
                ChunkEntry {
                    grid: [0, 0, 0],
                    aabb_min: [0.0, -0.5, -0.5],
                    aabb_max: [1.0, 0.5, 0.5],
                    offset: 0,
                    count: 8,
                },
                ChunkEntry {
                    grid: [1, 0, 0],
                    aabb_min: [5.0, -0.5, -0.5],
                    aabb_max: [6.0, 0.5, 0.5],
                    offset: 8,
                    count: 8,
                },
            ],
            stride,
        };
        let mut payload = Vec::new();
        for point in 0..16 {
            let mut splat = [0.0_f32; POINT_STRIDE_FLOATS];
            let local = (point % 8) as f32;
            splat[0] = if point < 8 {
                local * 0.1
            } else {
                5.0 + local * 0.1
            };
            splat[7] = 0.01;
            splat[8] = 0.01;
            splat[9] = 0.01;
            splat[17] = 1.0;
            payload.extend_from_slice(&splat);
        }

        let pack = write_pack(&path, &payload, &table, 0, 64).expect("write pack");
        let selected =
            pack.select_lods_view_priority([0.0, 0.0, 0.0], [1.0, 0.0, 0.0], 120.0, 10.0, 10);

        assert_eq!(selected, vec![(0, 8), (1, 2)]);
        assert!(selected.iter().map(|&(_, count)| count).sum::<u32>() <= 10);

        let _ = std::fs::remove_file(path);
    }
}
