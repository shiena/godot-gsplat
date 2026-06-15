//! Chunk-streaming runtime (Phase C): spatial chunk selection around the camera
//! plus the async render-set rebuild worker.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use godot::prelude::*;

use super::render_build::{pack_raw, raw_to_render, RawRenderData};
use super::GaussianSplatNode3D;

pub(super) struct PageCacheState {
    pages: HashMap<u32, Arc<Vec<f32>>>,
    order: VecDeque<u32>,
}

impl PageCacheState {
    fn new() -> Self {
        Self {
            pages: HashMap::new(),
            order: VecDeque::new(),
        }
    }
}

type PageCache = Arc<Mutex<PageCacheState>>;
const PACK_PAGE_CACHE_LIMIT: usize = 64;

// Runtime chunk-streaming state (Phase C2): the spatial chunk table plus the
// currently selected (active) chunk indices and the selection-gating reference.
// Present only when the bound asset was decoded with a chunk table.
pub(super) struct ChunkRuntime {
    // Shared (immutable) so an async rebuild worker can borrow them off-thread.
    pub(super) table: Arc<crate::chunking::ChunkTable>,
    pub(super) payload: Option<Arc<Vec<f32>>>,
    pub(super) pack: Option<Arc<crate::gsplat_pack::GsplatPackIndex>>,
    pub(super) page_cache: Option<PageCache>,
    // Highest SH degree available in the payload (from the asset at refresh).
    pub(super) sh_degree_available: i32,
    // Active render set as (chunk_index, lod_count) — the importance top-K prefix of
    // each selected chunk taken this frame (Phase C3).
    pub(super) active: Vec<(u32, u32)>,
    pub(super) last_select_pos: Option<Vector3>,
    pub(super) last_select_forward: Option<Vector3>,
    pub(super) last_budget: i32,
    pub(super) last_view_priority_fov_degrees: f32,
    pub(super) last_view_priority_full_distance: f32,
    pub(super) last_view_priority_min_lod_per_chunk: i32,
    // In-flight async render-set rebuild (Phase C2b); None when idle.
    pub(super) pending: Option<std::sync::mpsc::Receiver<RawRenderData>>,
    // A rebuild was requested while one was in flight (e.g. a settings change);
    // re-kick it as soon as the running one lands so the change is not lost.
    pub(super) rebuild_queued: bool,
}

impl GaussianSplatNode3D {
    // Build the chunk-streaming runtime from the bound asset's chunk table, seeding
    // the active set with a budget-bounded selection around the cloud center (no
    // camera yet). None when the asset has no chunk table (placeholder/legacy).
    pub(super) fn refresh_chunk_runtime(&mut self) {
        let decoded = self.asset.as_ref().and_then(|asset| {
            let asset_ref = asset.bind();
            let table = asset_ref.chunk_table().cloned()?;
            let payload = asset_ref.payload_float_values();
            let pack = asset_ref.pack_index().cloned();
            if payload.is_none() && pack.is_none() {
                return None;
            }
            Some((table, payload, pack, asset_ref.sh_degree_available()))
        });
        let Some((table, payload, pack, sh_degree_available)) = decoded else {
            self.backend.chunks = None;
            return;
        };
        // Seed the active set with a budget-bounded selection around the cloud center
        // (no camera yet); process() re-selects from the real camera on the next tick.
        // View-priority intentionally waits for a real camera/HMD pose so it does
        // not waste the first pack read on an arbitrary cloud-center selection.
        let budget = self.active_budget();
        let center = crate::chunking::table_center(&table);
        let budget_u = if budget <= 0 { u32::MAX } else { budget as u32 };
        let active = if self.view_priority_selection_enabled() {
            Vec::new()
        } else if self.coverage_selection_enabled() {
            crate::chunking::select_chunks_coverage(&table, budget_u)
        } else {
            crate::chunking::select_chunks(&table, center, budget_u)
        };
        self.backend.chunks = Some(ChunkRuntime {
            table: Arc::new(table),
            payload: payload.map(Arc::new),
            pack: pack.map(Arc::new),
            page_cache: Some(Arc::new(Mutex::new(PageCacheState::new()))),
            sh_degree_available,
            active,
            last_select_pos: None,
            last_select_forward: None,
            last_budget: budget,
            last_view_priority_fov_degrees: self.view_priority_fov_degrees(),
            last_view_priority_full_distance: self.view_priority_full_distance(),
            last_view_priority_min_lod_per_chunk: self.view_priority_min_lod_per_chunk(),
            pending: None,
            rebuild_queued: false,
        });
    }

    // Re-select the active chunks when the camera moves far enough to change the
    // nearest-within-budget set, then rebuild the render set. Gated like the sort's
    // view-change check so a near-static camera does no work.
    pub(super) fn update_chunk_selection(&mut self) {
        // Skip while a rebuild is in flight; the next gate-crossing picks up the latest.
        if self
            .backend
            .chunks
            .as_ref()
            .map(|rt| rt.pending.is_some())
            .unwrap_or(false)
        {
            return;
        }
        // Coverage selection is camera-independent (it only changes with the
        // budget), so it neither needs a camera nor re-selects on movement.
        let coverage = self.coverage_selection_enabled();
        let view_priority = self.view_priority_selection_enabled();
        let cam = match self.camera_local_pos() {
            Some(cam) => cam,
            None if coverage => Vector3::ZERO,
            None => return,
        };
        let forward = if view_priority {
            match self.camera_local_forward() {
                Some(forward) => forward,
                None => return,
            }
        } else {
            Vector3::ZERO
        };
        let budget = self.active_budget();
        let fov_degrees = self.view_priority_fov_degrees();
        let full_distance = self.view_priority_full_distance();
        let min_lod = self.view_priority_min_lod_per_chunk();
        let changed = match &self.backend.chunks {
            Some(rt) => {
                let moved = if coverage {
                    false
                } else {
                    let threshold = (rt.table.chunk_size * 0.25).max(1.0e-3);
                    rt.last_select_pos
                        .map(|last| (cam - last).length() > threshold)
                        .unwrap_or(true)
                };
                let rotated = if view_priority {
                    rt.last_select_forward
                        .map(|last| last.dot(forward) < 0.996)
                        .unwrap_or(true)
                } else {
                    false
                };
                let view_priority_params_changed = view_priority
                    && ((fov_degrees - rt.last_view_priority_fov_degrees).abs() > f32::EPSILON
                        || (full_distance - rt.last_view_priority_full_distance).abs()
                            > f32::EPSILON
                        || min_lod != rt.last_view_priority_min_lod_per_chunk);
                budget != rt.last_budget
                    || moved
                    || rotated
                    || view_priority_params_changed
                    || rt.last_select_pos.is_none()
            }
            None => return,
        };
        if !changed {
            return;
        }
        let active = self.select_chunks(cam, budget);
        let differs = self
            .backend
            .chunks
            .as_ref()
            .map(|rt| rt.active != active)
            .unwrap_or(false);
        if let Some(rt) = self.backend.chunks.as_mut() {
            rt.last_select_pos = Some(cam);
            if view_priority {
                rt.last_select_forward = Some(forward);
            }
            rt.last_budget = budget;
            rt.last_view_priority_fov_degrees = fov_degrees;
            rt.last_view_priority_full_distance = full_distance;
            rt.last_view_priority_min_lod_per_chunk = min_lod;
            if differs {
                rt.active = active;
            }
        }
        if differs {
            self.begin_chunk_rebuild();
        }
    }

    pub(super) fn select_chunks(&self, cam_local: Vector3, budget: i32) -> Vec<(u32, u32)> {
        let Some(rt) = &self.backend.chunks else {
            return Vec::new();
        };
        let budget = if budget <= 0 { u32::MAX } else { budget as u32 };
        if self.view_priority_selection_enabled() {
            let forward = self
                .camera_local_forward()
                .unwrap_or_else(|| Vector3::new(0.0, 0.0, -1.0));
            crate::chunking::select_chunks_view_priority(
                rt.table.as_ref(),
                [cam_local.x, cam_local.y, cam_local.z],
                [forward.x, forward.y, forward.z],
                self.view_priority_fov_degrees(),
                self.view_priority_full_distance(),
                budget.min(self.view_priority_target_budget()),
                self.view_priority_min_lod_per_chunk() as u32,
            )
        } else if self.coverage_selection_enabled() {
            crate::chunking::select_chunks_coverage(rt.table.as_ref(), budget)
        } else {
            crate::chunking::select_chunks(
                rt.table.as_ref(),
                [cam_local.x, cam_local.y, cam_local.z],
                budget,
            )
        }
    }

    pub(super) fn active_budget(&self) -> i32 {
        self.cloud_settings
            .as_ref()
            .map(|settings| settings.bind().get_max_preview_splats().max(0))
            .unwrap_or(i32::MAX)
    }

    pub(super) fn coverage_selection_enabled(&self) -> bool {
        self.cloud_settings
            .as_ref()
            .map(|settings| {
                settings.bind().get_chunk_selection()
                    == crate::cloud_settings::CHUNK_SELECTION_COVERAGE
            })
            .unwrap_or(false)
    }

    pub(super) fn view_priority_selection_enabled(&self) -> bool {
        self.cloud_settings
            .as_ref()
            .map(|settings| {
                settings.bind().get_chunk_selection()
                    == crate::cloud_settings::CHUNK_SELECTION_VIEW_PRIORITY
            })
            .unwrap_or(false)
    }

    fn view_priority_fov_degrees(&self) -> f32 {
        self.cloud_settings
            .as_ref()
            .map(|settings| settings.bind().get_view_priority_fov_degrees())
            .unwrap_or(crate::cloud_settings::DEFAULT_VIEW_PRIORITY_FOV_DEGREES)
    }

    fn view_priority_full_distance(&self) -> f32 {
        self.cloud_settings
            .as_ref()
            .map(|settings| settings.bind().get_view_priority_full_distance())
            .unwrap_or(crate::cloud_settings::DEFAULT_VIEW_PRIORITY_FULL_DISTANCE)
    }

    fn view_priority_target_budget(&self) -> u32 {
        self.cloud_settings
            .as_ref()
            .map(|settings| settings.bind().get_view_priority_target_budget().max(0) as u32)
            .unwrap_or(crate::cloud_settings::DEFAULT_VIEW_PRIORITY_TARGET_BUDGET as u32)
    }

    fn view_priority_min_lod_per_chunk(&self) -> i32 {
        self.cloud_settings
            .as_ref()
            .map(|settings| settings.bind().get_view_priority_min_lod_per_chunk())
            .unwrap_or(crate::cloud_settings::DEFAULT_VIEW_PRIORITY_MIN_LOD_PER_CHUNK)
            .max(1)
    }

    // Kick off an async rebuild of the active render set on a worker thread (Phase
    // C2b). Only one runs at a time; the heavy gather + covariance packing happen off
    // the main thread, which later applies the result in `poll_chunk_rebuild`.
    pub(super) fn begin_chunk_rebuild(&mut self) {
        let scale_multiplier = self
            .cloud_settings
            .as_ref()
            .map(|settings| settings.bind().get_gaussian_scale_multiplier())
            .unwrap_or(1.0)
            .max(0.01);
        // While one is in flight, queue a re-kick instead of dropping the request
        // so settings changes made meanwhile still land.
        if let Some(rt) = self.backend.chunks.as_mut() {
            if rt.pending.is_some() {
                rt.rebuild_queued = true;
                return;
            }
        }
        let receiver = match &self.backend.chunks {
            Some(rt) if rt.pending.is_none() => {
                // No bound cloud settings means class defaults: full SH degree.
                let cap = self
                    .cloud_settings
                    .as_ref()
                    .map(|settings| settings.bind().get_sh_degree())
                    .unwrap_or(3);
                let sh_degree = cap.clamp(0, 3).min(rt.sh_degree_available);
                let payload = rt.payload.as_ref().map(Arc::clone);
                let pack = rt.pack.as_ref().map(Arc::clone);
                let page_cache = rt.page_cache.as_ref().map(Arc::clone);
                let table = Arc::clone(&rt.table);
                let active = rt.active.clone();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let slice = if let Some(payload) = payload {
                        crate::chunking::gather_active(payload.as_slice(), table.as_ref(), &active)
                    } else if let (Some(pack), Some(cache)) = (pack, page_cache) {
                        match gather_active_from_pack(pack.as_ref(), cache.as_ref(), &active) {
                            Ok(slice) => slice,
                            Err(err) => {
                                godot_error!("[gsplat] {err}");
                                Vec::new()
                            }
                        }
                    } else {
                        Vec::new()
                    };
                    if let Some(raw) = pack_raw(&slice, scale_multiplier, table.stride, sh_degree) {
                        let _ = tx.send(raw);
                    }
                });
                Some(rx)
            }
            _ => None,
        };
        if let Some(rx) = receiver {
            if let Some(rt) = self.backend.chunks.as_mut() {
                rt.pending = Some(rx);
            }
        }
    }

    // Apply a finished async rebuild, if any (Phase C2b). Non-blocking: keeps the
    // current render set until the worker delivers the new one, avoiding a hitch.
    pub(super) fn poll_chunk_rebuild(&mut self) {
        let result = match self
            .backend
            .chunks
            .as_ref()
            .and_then(|rt| rt.pending.as_ref())
        {
            Some(rx) => rx.try_recv(),
            None => return,
        };
        match result {
            Ok(raw) => {
                if let Some(rt) = self.backend.chunks.as_mut() {
                    rt.pending = None;
                }
                self.apply_render_data(raw_to_render(raw));
                self.kick_queued_rebuild();
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                if let Some(rt) = self.backend.chunks.as_mut() {
                    rt.pending = None;
                }
                self.kick_queued_rebuild();
            }
        }
    }

    fn kick_queued_rebuild(&mut self) {
        let queued = self
            .backend
            .chunks
            .as_mut()
            .map(|rt| std::mem::take(&mut rt.rebuild_queued))
            .unwrap_or(false);
        if queued {
            self.begin_chunk_rebuild();
        }
    }
}

fn gather_active_from_pack(
    pack: &crate::gsplat_pack::GsplatPackIndex,
    cache: &Mutex<PageCacheState>,
    active: &[(u32, u32)],
) -> Result<Vec<f32>, String> {
    let stride = pack.stride.max(crate::import_state::POINT_STRIDE_FLOATS);
    let mut total = 0usize;
    for &(ci, lod) in active {
        if let Some(chunk) = pack.chunks.get(ci as usize) {
            total += lod.min(chunk.entry.count) as usize * stride;
        }
    }
    let mut out = Vec::with_capacity(total);
    for &(ci, lod) in active {
        let Some(chunk) = pack.chunks.get(ci as usize) else {
            continue;
        };
        let mut remaining = lod.min(chunk.entry.count);
        for page_offset in 0..chunk.page_count {
            if remaining == 0 {
                break;
            }
            let page_index = chunk.first_page + page_offset;
            let Some(page) = pack.pages.get(page_index as usize) else {
                continue;
            };
            let page_values = read_page_cached(pack, cache, page_index)?;
            let take = remaining.min(page.count) as usize;
            let end = take * stride;
            if end <= page_values.len() {
                out.extend_from_slice(&page_values[..end]);
            }
            remaining -= take as u32;
        }
    }
    Ok(out)
}

fn read_page_cached(
    pack: &crate::gsplat_pack::GsplatPackIndex,
    cache: &Mutex<PageCacheState>,
    page_index: u32,
) -> Result<Arc<Vec<f32>>, String> {
    let mut guard = cache
        .lock()
        .map_err(|_| "Pack page cache is poisoned.".to_string())?;
    if let Some(values) = guard.pages.get(&page_index).cloned() {
        guard.order.retain(|&idx| idx != page_index);
        guard.order.push_back(page_index);
        return Ok(values);
    }
    drop(guard);

    let values = Arc::new(pack.read_page(page_index)?);
    let mut guard = cache
        .lock()
        .map_err(|_| "Pack page cache is poisoned.".to_string())?;
    guard.pages.insert(page_index, Arc::clone(&values));
    guard.order.retain(|&idx| idx != page_index);
    guard.order.push_back(page_index);
    while guard.pages.len() > PACK_PAGE_CACHE_LIMIT {
        if let Some(evict) = guard.order.pop_front() {
            guard.pages.remove(&evict);
        } else {
            break;
        }
    }
    Ok(values)
}
