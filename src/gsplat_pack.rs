//! Disk-backed splat payload packs for large scenes.
//!
//! Packs store quantized RGBA8 splat records so the renderer can upload selected
//! pages without expanding them back to RGBAF.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::chunking::{ChunkEntry, ChunkTable};
use crate::import_state::POINT_STRIDE_FLOATS;

const MAGIC: &[u8; 8] = b"GSPACK1\0";
const VERSION: u32 = 2;
pub const DEFAULT_PAGE_SPLATS: u32 = 16_384;
const HEADER_SIZE: u64 = 160;
const CHUNK_RECORD_SIZE: u64 = 64;
const PAGE_RECORD_SIZE: u64 = 32;
const ENCODING_PACKED_RGBA8: u32 = 1;

#[derive(Clone, Debug)]
pub struct PackChunk {
    pub entry: ChunkEntry,
    pub first_page: u32,
    pub page_count: u32,
}

#[derive(Clone, Debug)]
pub struct PackPage {
    pub chunk_index: u32,
    pub start_in_chunk: u32,
    pub count: u32,
    pub byte_offset: u64,
    pub byte_len: u32,
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
    pub chunks: Vec<PackChunk>,
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
    page_count: u32,
    chunk_table_offset: u64,
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
    let mut pages = Vec::new();

    for (chunk_index, entry) in table.entries.iter().enumerate() {
        let first_page = pages.len() as u32;
        let mut start = 0_u32;
        while start < entry.count {
            let count = (entry.count - start).min(page_splats);
            pages.push(PackPage {
                chunk_index: chunk_index as u32,
                start_in_chunk: start,
                count,
                byte_offset: 0,
                byte_len: count
                    .checked_mul(stride as u32)
                    .and_then(|v| v.checked_mul(4))
                    .ok_or_else(|| "Pack page byte length overflowed.".to_string())?,
            });
            start += count;
        }
        chunks.push(PackChunk {
            entry: entry.clone(),
            first_page,
            page_count: pages.len() as u32 - first_page,
        });
    }

    let ranges = QuantizationRanges::from_payload(payload, stride, sh_degree_available);
    let record_bytes = packed_record_bytes(sh_degree_available);

    let chunk_table_offset = HEADER_SIZE;
    let page_table_offset = chunk_table_offset + CHUNK_RECORD_SIZE * chunks.len() as u64;
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
        page_count: pages.len() as u32,
        chunk_table_offset,
        page_table_offset,
        payload_offset,
    };
    write_header(&mut file, &header)?;
    for chunk in &chunks {
        write_chunk_record(&mut file, chunk)?;
    }
    for page in &pages {
        write_page_record(&mut file, page)?;
    }
    for page in &pages {
        let chunk = table
            .entries
            .get(page.chunk_index as usize)
            .ok_or_else(|| "Pack page references an invalid chunk.".to_string())?;
        let start = (chunk.offset + page.start_in_chunk) as usize * stride;
        let end = start + page.count as usize * stride;
        if end > payload.len() {
            return Err("Pack page range exceeds payload length.".to_string());
        }
        write_packed_records(
            &mut file,
            &payload[start..end],
            stride,
            page.count as usize,
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
        page_count: page_count as u32,
        chunk_table_offset,
        page_table_offset,
        payload_offset: read_u64_at(&header, 64)?,
    };

    file.seek(SeekFrom::Start(parsed.chunk_table_offset))
        .map_err(|err| format!("Failed to seek pack chunk table: {err}"))?;
    let mut chunks = Vec::with_capacity(chunk_count);
    for _ in 0..chunk_count {
        chunks.push(read_chunk_record(&mut file)?);
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
        chunks,
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
    write_padding(file, 24)?;
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

fn write_page_record(file: &mut File, page: &PackPage) -> Result<(), String> {
    write_u32(file, page.chunk_index)?;
    write_u32(file, page.start_in_chunk)?;
    write_u32(file, page.count)?;
    write_u32(file, 0)?;
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
        start_in_chunk: read_u32_at(&bytes, 4)?,
        count: read_u32_at(&bytes, 8)?,
        byte_offset: read_u64_at(&bytes, 16)?,
        byte_len: read_u32_at(&bytes, 24)?,
    })
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

fn write_packed_records(
    file: &mut File,
    payload: &[f32],
    stride: usize,
    count: usize,
    sh_degree_available: i32,
    ranges: &QuantizationRanges,
) -> Result<(), String> {
    let record_bytes = packed_record_bytes(sh_degree_available);
    let sh_count = sh_floats(sh_degree_available);
    let mut record = vec![0_u8; record_bytes];
    for splat in payload.chunks_exact(stride).take(count) {
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
        assert_eq!(pack.pages.len(), 3);
        assert_eq!(pack.chunks[0].page_count, 2);
        assert_eq!(pack.chunks[1].page_count, 1);

        let first_page = pack.read_page_bytes(0).expect("read page");
        assert_eq!(first_page.len(), 2 * pack.record_bytes);

        let second_chunk_page = pack.read_page_bytes(2).expect("read page");
        assert_eq!(second_chunk_page.len(), 2 * pack.record_bytes);

        let _ = std::fs::remove_file(path);
    }
}
