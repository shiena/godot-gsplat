//! Disk-backed splat payload packs for large scenes.
//!
//! A pack keeps the existing decoded float32 payload layout, but splits each
//! spatial chunk into fixed-size pages so runtime code can load only the pages
//! needed for the current active render set.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::chunking::{ChunkEntry, ChunkTable};
use crate::import_state::POINT_STRIDE_FLOATS;

const MAGIC: &[u8; 8] = b"GSPACK1\0";
const VERSION: u32 = 1;
pub const DEFAULT_PAGE_SPLATS: u32 = 16_384;
const HEADER_SIZE: u64 = 96;
const CHUNK_RECORD_SIZE: u64 = 64;
const PAGE_RECORD_SIZE: u64 = 32;

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
    pub sh_degree_available: i32,
    pub chunk_size: f32,
    pub grid_origin: [f32; 3],
    pub chunks: Vec<PackChunk>,
    pub pages: Vec<PackPage>,
}

struct PackHeader {
    point_count: u32,
    stride: u32,
    sh_degree_available: u32,
    chunk_size: f32,
    grid_origin: [f32; 3],
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

    pub fn read_page(&self, page_index: u32) -> Result<Vec<f32>, String> {
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
        bytes_to_f32_vec(&bytes)
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

    let chunk_table_offset = HEADER_SIZE;
    let page_table_offset = chunk_table_offset + CHUNK_RECORD_SIZE * chunks.len() as u64;
    let payload_offset = page_table_offset + PAGE_RECORD_SIZE * pages.len() as u64;
    let mut cursor = payload_offset;
    for page in &mut pages {
        page.byte_offset = cursor;
        cursor += page.byte_len as u64;
    }

    let mut file = File::create(path)
        .map_err(|err| format!("Failed to create pack '{}': {err}", path.display()))?;
    let header = PackHeader {
        point_count,
        stride: stride as u32,
        sh_degree_available: sh_degree_available.max(0) as u32,
        chunk_size: table.chunk_size,
        grid_origin: table.grid_origin,
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
        for value in &payload[start..end] {
            file.write_all(&value.to_le_bytes())
                .map_err(|err| format!("Failed to write pack payload: {err}"))?;
        }
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

    file.seek(SeekFrom::Start(chunk_table_offset))
        .map_err(|err| format!("Failed to seek pack chunk table: {err}"))?;
    let mut chunks = Vec::with_capacity(chunk_count);
    for _ in 0..chunk_count {
        chunks.push(read_chunk_record(&mut file)?);
    }

    file.seek(SeekFrom::Start(page_table_offset))
        .map_err(|err| format!("Failed to seek pack page table: {err}"))?;
    let mut pages = Vec::with_capacity(page_count);
    for _ in 0..page_count {
        pages.push(read_page_record(&mut file)?);
    }

    Ok(GsplatPackIndex {
        path: path.to_path_buf(),
        point_count,
        stride,
        sh_degree_available,
        chunk_size,
        grid_origin,
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
    write_padding(file, HEADER_SIZE as usize - 72)
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

fn bytes_to_f32_vec(bytes: &[u8]) -> Result<Vec<f32>, String> {
    if !bytes.len().is_multiple_of(4) {
        return Err("Pack page byte length is not float32 aligned.".to_string());
    }
    let mut values = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks_exact(4) {
        values.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(values)
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
        assert_eq!(pack.chunks.len(), 2);
        assert_eq!(pack.pages.len(), 3);
        assert_eq!(pack.chunks[0].page_count, 2);
        assert_eq!(pack.chunks[1].page_count, 1);

        let first_page = pack.read_page(0).expect("read page");
        assert_eq!(first_page.len(), 2 * stride);
        assert_eq!(first_page[0], 0.0);
        assert_eq!(first_page[stride], 100.0);

        let second_chunk_page = pack.read_page(2).expect("read page");
        assert_eq!(second_chunk_page[0], 300.0);
        assert_eq!(second_chunk_page[stride], 400.0);

        let _ = std::fs::remove_file(path);
    }
}
