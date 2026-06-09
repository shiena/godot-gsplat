use godot::classes::GltfState;
use godot::prelude::*;

use crate::import_state::{
    DecodedSplatData, ImportedSplatMetadata, FALLBACK_NONE, GLTF_STATE_KEY,
    PAYLOAD_LAYOUT_FLOAT32_V1, PAYLOAD_LAYOUT_V1, POINT_STRIDE_FLOATS,
};

#[derive(GodotClass)]
#[class(tool, init, base=Resource)]
pub struct GaussianSplatAsset {
    #[base]
    base: Base<Resource>,

    metadata: ImportedSplatMetadata,
    point_count: i32,
    payload: PackedByteArray,
    payload_layout: GString,
    fallback_mode: GString,
    local_aabb: Aabb,
    // Spatial grid partition of `payload` (Phase C). In-memory only for now; not a
    // Godot property, so it is not serialized into a baked .scn (Case B keeps using
    // the baked render set until disk streaming lands).
    chunk_table: Option<crate::chunking::ChunkTable>,
}

#[godot_api]
impl GaussianSplatAsset {
    #[func]
    pub fn clear(&mut self) {
        self.metadata = ImportedSplatMetadata::default();
        self.point_count = 0;
        self.payload.clear();
        self.payload_layout = PAYLOAD_LAYOUT_V1.into();
        self.fallback_mode = FALLBACK_NONE.into();
        self.local_aabb = Aabb::default();
        self.chunk_table = None;
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn apply_import_metadata(&mut self, metadata: VarDictionary) {
        self.metadata = ImportedSplatMetadata::from_dictionary(metadata);
        self.point_count = self.metadata.point_count.max(0);
        self.payload_layout = PAYLOAD_LAYOUT_V1.into();
        self.fallback_mode = self.metadata.fallback_mode.as_str().into();
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn initialize_from_import(&mut self, metadata: VarDictionary) {
        self.apply_import_metadata(metadata);
        self.payload = build_placeholder_payload(&self.metadata);
        self.local_aabb = Aabb::default();
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn initialize_from_decoded(
        &mut self,
        metadata: VarDictionary,
        payload: PackedByteArray,
        payload_layout: GString,
        local_aabb: Aabb,
    ) {
        self.apply_import_metadata(metadata);
        self.payload = payload;
        self.payload_layout = payload_layout;
        self.local_aabb = local_aabb;
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn export_import_metadata(&self) -> VarDictionary {
        self.metadata.to_dictionary()
    }

    #[func]
    pub fn get_metadata_summary(&self) -> GString {
        GString::from(self.metadata.summary().as_str())
    }

    #[func]
    pub fn set_point_count(&mut self, point_count: i32) {
        self.point_count = point_count.max(0);
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn get_point_count(&self) -> i32 {
        self.point_count
    }

    #[func]
    pub fn set_payload(&mut self, payload: PackedByteArray) {
        self.payload = payload;
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn get_payload(&self) -> PackedByteArray {
        self.payload.clone()
    }

    #[func]
    pub fn get_payload_byte_len(&self) -> i64 {
        self.payload.len() as i64
    }

    #[func]
    pub fn get_payload_layout(&self) -> GString {
        self.payload_layout.clone()
    }

    #[func]
    pub fn get_fallback_mode(&self) -> GString {
        self.fallback_mode.clone()
    }

    #[func]
    pub fn has_point_fallback(&self) -> bool {
        self.fallback_mode != FALLBACK_NONE
    }

    #[func]
    pub fn set_local_aabb(&mut self, aabb: Aabb) {
        self.local_aabb = aabb;
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn get_local_aabb(&self) -> Aabb {
        self.local_aabb
    }

    #[func]
    pub fn extract_point_positions(&self) -> PackedVector3Array {
        if self.payload_layout != PAYLOAD_LAYOUT_FLOAT32_V1 {
            return PackedVector3Array::new();
        }

        let floats = self.payload.to_float32_array();
        let values = floats.as_slice();
        if !values.len().is_multiple_of(POINT_STRIDE_FLOATS) {
            return PackedVector3Array::new();
        }

        let point_count = values.len() / POINT_STRIDE_FLOATS;
        let mut positions = Vec::with_capacity(point_count);
        for point_index in 0..point_count {
            let offset = point_index * POINT_STRIDE_FLOATS;
            positions.push(Vector3::new(
                values[offset],
                values[offset + 1],
                values[offset + 2],
            ));
        }
        PackedVector3Array::from(positions)
    }

    #[func]
    pub fn extract_point_colors(&self) -> PackedColorArray {
        if self.payload_layout != PAYLOAD_LAYOUT_FLOAT32_V1 {
            return PackedColorArray::new();
        }

        let floats = self.payload.to_float32_array();
        let values = floats.as_slice();
        if !values.len().is_multiple_of(POINT_STRIDE_FLOATS) {
            return PackedColorArray::new();
        }

        let point_count = values.len() / POINT_STRIDE_FLOATS;
        let mut colors = Vec::with_capacity(point_count);
        for point_index in 0..point_count {
            let offset = point_index * POINT_STRIDE_FLOATS + 14;
            colors.push(Color::from_rgba(
                values[offset],
                values[offset + 1],
                values[offset + 2],
                values[offset + 3],
            ));
        }
        PackedColorArray::from(colors)
    }

    pub fn payload_float_values(&self) -> Option<Vec<f32>> {
        if self.payload_layout != PAYLOAD_LAYOUT_FLOAT32_V1 {
            return None;
        }

        let floats = self.payload.to_float32_array();
        let values = floats.as_slice();
        if !values.len().is_multiple_of(POINT_STRIDE_FLOATS) {
            return None;
        }

        Some(values.to_vec())
    }

    #[func]
    pub fn has_compression(&self) -> bool {
        self.metadata.compression.is_some()
    }

    #[func]
    pub fn get_source_extension(&self) -> GString {
        self.metadata.source_extension.as_str().into()
    }

    #[func]
    pub fn stash_on_state(&self, state: Option<Gd<GltfState>>) {
        if let Some(mut state) = state {
            let dict = self.metadata.to_dictionary();
            state.set_additional_data(GLTF_STATE_KEY, &Variant::from(dict));
        }
    }

    pub fn apply_decoded_data(&mut self, decoded: DecodedSplatData) {
        self.point_count = decoded.point_count.max(0);
        self.payload = decoded.payload;
        self.payload_layout = decoded.payload_layout;
        self.local_aabb = decoded.local_aabb;
        self.chunk_table = decoded.chunk_table;
        self.base_mut().emit_changed();
    }

    // Spatial chunk partition of the payload (Phase C), if the asset was decoded
    // with chunking. None for placeholder/legacy assets.
    pub fn chunk_table(&self) -> Option<&crate::chunking::ChunkTable> {
        self.chunk_table.as_ref()
    }
}

fn build_placeholder_payload(metadata: &ImportedSplatMetadata) -> PackedByteArray {
    let summary = format!(
        "layout={}; points={}; fallback={}",
        PAYLOAD_LAYOUT_V1, metadata.point_count, metadata.fallback_mode
    );
    PackedByteArray::from(summary.as_bytes())
}
