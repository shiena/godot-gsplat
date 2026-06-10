//! Chunk-streaming runtime (Phase C): spatial chunk selection around the camera
//! plus the async render-set rebuild worker.

use std::sync::Arc;

use godot::prelude::*;

use super::render_build::{pack_raw, raw_to_render, RawRenderData};
use super::GaussianSplatNode3D;

// Runtime chunk-streaming state (Phase C2): the spatial chunk table plus the
// currently selected (active) chunk indices and the selection-gating reference.
// Present only when the bound asset was decoded with a chunk table.
pub(super) struct ChunkRuntime {
    // Shared (immutable) so an async rebuild worker can borrow them off-thread.
    pub(super) table: Arc<crate::chunking::ChunkTable>,
    pub(super) payload: Arc<Vec<f32>>,
    // Highest SH degree available in the payload (from the asset at refresh).
    pub(super) sh_degree_available: i32,
    // Active render set as (chunk_index, lod_count) — the importance top-K prefix of
    // each selected chunk taken this frame (Phase C3).
    pub(super) active: Vec<(u32, u32)>,
    pub(super) last_select_pos: Option<Vector3>,
    pub(super) last_budget: i32,
    // In-flight async render-set rebuild (Phase C2b); None when idle.
    pub(super) pending: Option<std::sync::mpsc::Receiver<RawRenderData>>,
}

impl GaussianSplatNode3D {
    // Build the chunk-streaming runtime from the bound asset's chunk table, seeding
    // the active set with a budget-bounded selection around the cloud center (no
    // camera yet). None when the asset has no chunk table (placeholder/legacy).
    pub(super) fn refresh_chunk_runtime(&mut self) {
        let decoded = self.asset.as_ref().and_then(|asset| {
            let asset_ref = asset.bind();
            let table = asset_ref.chunk_table().cloned()?;
            let payload = asset_ref.payload_float_values()?;
            Some((table, payload, asset_ref.sh_degree_available()))
        });
        let Some((table, payload, sh_degree_available)) = decoded else {
            self.chunk_runtime = None;
            return;
        };
        // Seed the active set with a budget-bounded selection around the cloud center
        // (no camera yet); process() re-selects from the real camera on the next tick.
        let budget = self.active_budget();
        let center = crate::chunking::table_center(&table);
        let budget_u = if budget <= 0 { u32::MAX } else { budget as u32 };
        let active = crate::chunking::select_chunks(&table, center, budget_u);
        self.chunk_runtime = Some(ChunkRuntime {
            table: Arc::new(table),
            payload: Arc::new(payload),
            sh_degree_available,
            active,
            last_select_pos: None,
            last_budget: budget,
            pending: None,
        });
    }

    // Re-select the active chunks when the camera moves far enough to change the
    // nearest-within-budget set, then rebuild the render set. Gated like the sort's
    // view-change check so a near-static camera does no work.
    pub(super) fn update_chunk_selection(&mut self) {
        // Skip while a rebuild is in flight; the next gate-crossing picks up the latest.
        if self
            .chunk_runtime
            .as_ref()
            .map(|rt| rt.pending.is_some())
            .unwrap_or(false)
        {
            return;
        }
        let Some(cam) = self.camera_local_pos() else {
            return;
        };
        let budget = self.active_budget();
        let changed = match &self.chunk_runtime {
            Some(rt) => {
                let threshold = (rt.table.chunk_size * 0.25).max(1.0e-3);
                match rt.last_select_pos {
                    Some(last) => budget != rt.last_budget || (cam - last).length() > threshold,
                    None => true,
                }
            }
            None => return,
        };
        if !changed {
            return;
        }
        let active = self.select_chunks(cam, budget);
        let differs = self
            .chunk_runtime
            .as_ref()
            .map(|rt| rt.active != active)
            .unwrap_or(false);
        if let Some(rt) = self.chunk_runtime.as_mut() {
            rt.last_select_pos = Some(cam);
            rt.last_budget = budget;
            if differs {
                rt.active = active;
            }
        }
        if differs {
            self.begin_chunk_rebuild();
        }
    }

    pub(super) fn select_chunks(&self, cam_local: Vector3, budget: i32) -> Vec<(u32, u32)> {
        let Some(rt) = &self.chunk_runtime else {
            return Vec::new();
        };
        let budget = if budget <= 0 { u32::MAX } else { budget as u32 };
        crate::chunking::select_chunks(
            rt.table.as_ref(),
            [cam_local.x, cam_local.y, cam_local.z],
            budget,
        )
    }

    pub(super) fn active_budget(&self) -> i32 {
        self.cloud_settings
            .as_ref()
            .map(|settings| settings.bind().get_max_preview_splats().max(0))
            .unwrap_or(i32::MAX)
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
        let receiver = match &self.chunk_runtime {
            Some(rt) if rt.pending.is_none() => {
                let cap = self
                    .cloud_settings
                    .as_ref()
                    .map(|settings| settings.bind().get_sh_degree())
                    .unwrap_or(0);
                let sh_degree = cap.clamp(0, 3).min(rt.sh_degree_available);
                let payload = Arc::clone(&rt.payload);
                let table = Arc::clone(&rt.table);
                let active = rt.active.clone();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let slice =
                        crate::chunking::gather_active(payload.as_slice(), table.as_ref(), &active);
                    if let Some(raw) = pack_raw(&slice, scale_multiplier, table.stride, sh_degree) {
                        let _ = tx.send(raw);
                    }
                });
                Some(rx)
            }
            _ => None,
        };
        if let Some(rx) = receiver {
            if let Some(rt) = self.chunk_runtime.as_mut() {
                rt.pending = Some(rx);
            }
        }
    }

    // Apply a finished async rebuild, if any (Phase C2b). Non-blocking: keeps the
    // current render set until the worker delivers the new one, avoiding a hitch.
    pub(super) fn poll_chunk_rebuild(&mut self) {
        let result = match self
            .chunk_runtime
            .as_ref()
            .and_then(|rt| rt.pending.as_ref())
        {
            Some(rx) => rx.try_recv(),
            None => return,
        };
        match result {
            Ok(raw) => {
                if let Some(rt) = self.chunk_runtime.as_mut() {
                    rt.pending = None;
                }
                self.apply_render_data(raw_to_render(raw));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                if let Some(rt) = self.chunk_runtime.as_mut() {
                    rt.pending = None;
                }
            }
        }
    }
}
