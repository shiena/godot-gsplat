use godot::prelude::*;

use crate::render_packet::{
    GaussianSplatRenderPacket, RENDER_STATUS_EMPTY, RENDER_STATUS_PENDING_UPLOAD,
    RENDER_UPLOAD_UNSUPPORTED,
};

const MANAGER_BACKEND_STATUS_STAGING_ONLY: &str = "staging_only";

struct RegisteredPacket {
    handle: i64,
    packet: Gd<GaussianSplatRenderPacket>,
}

#[derive(GodotClass)]
#[class(tool, base=Resource)]
pub struct GaussianSplatRenderManager {
    #[base]
    base: Base<Resource>,

    next_handle: i64,
    max_upload_chunk_bytes: i64,
    packets: Vec<RegisteredPacket>,
}

#[godot_api]
impl IResource for GaussianSplatRenderManager {
    fn init(base: Base<Resource>) -> Self {
        Self {
            base,
            next_handle: 1,
            max_upload_chunk_bytes: 4 * 1024 * 1024,
            packets: Vec::new(),
        }
    }
}

#[godot_api]
impl GaussianSplatRenderManager {
    #[func]
    pub fn register_packet(&mut self, packet: Option<Gd<GaussianSplatRenderPacket>>) -> i64 {
        let Some(packet) = packet else {
            return 0;
        };
        if packet.bind().get_status() == RENDER_STATUS_EMPTY {
            return 0;
        }

        let handle = self.next_handle;
        self.next_handle += 1;
        self.packets.push(RegisteredPacket { handle, packet });
        self.base_mut().emit_changed();
        handle
    }

    #[func]
    pub fn unregister_packet(&mut self, handle: i64) -> bool {
        let initial_len = self.packets.len();
        self.packets.retain(|entry| entry.handle != handle);
        let removed = self.packets.len() != initial_len;
        if removed {
            self.base_mut().emit_changed();
        }
        removed
    }

    #[func]
    pub fn clear(&mut self) {
        self.packets.clear();
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn get_max_upload_chunk_bytes(&self) -> i64 {
        self.max_upload_chunk_bytes
    }

    #[func]
    pub fn set_max_upload_chunk_bytes(&mut self, max_upload_chunk_bytes: i64) {
        self.max_upload_chunk_bytes = max_upload_chunk_bytes.max(4096);
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn get_packet_count(&self) -> i32 {
        self.packets.len() as i32
    }

    #[func]
    pub fn export_upload_plan(&self) -> VarArray {
        let mut plan = VarArray::new();
        for entry in &self.packets {
            let packet = entry.packet.bind();
            if packet.get_status() != RENDER_STATUS_PENDING_UPLOAD {
                continue;
            }

            let payload_byte_len = packet.get_payload_byte_len().max(0);
            let mut byte_offset = 0_i64;
            while byte_offset < payload_byte_len {
                let byte_len = (payload_byte_len - byte_offset).min(self.max_upload_chunk_bytes);
                let mut item = VarDictionary::new();
                item.set("handle", entry.handle);
                item.set("revision", packet.get_revision());
                item.set("byte_offset", byte_offset);
                item.set("byte_len", byte_len);
                item.set("point_count", packet.get_point_count() as i64);
                item.set("upload_format", &Variant::from(packet.get_upload_format()));
                item.set("pipeline", &Variant::from(packet.get_pipeline()));
                plan.push(&Variant::from(item));
                byte_offset += byte_len;
            }
        }
        plan
    }

    #[func]
    pub fn export_stats(&self) -> VarDictionary {
        let mut pending_upload_count = 0_i64;
        let mut unsupported_count = 0_i64;
        let mut total_point_count = 0_i64;
        let mut total_payload_byte_len = 0_i64;

        for entry in &self.packets {
            let packet = entry.packet.bind();
            match packet.get_status().to_string().as_str() {
                RENDER_STATUS_PENDING_UPLOAD => pending_upload_count += 1,
                RENDER_UPLOAD_UNSUPPORTED => unsupported_count += 1,
                _ => {}
            }
            total_point_count += i64::from(packet.get_point_count());
            total_payload_byte_len += packet.get_payload_byte_len();
        }

        let mut dict = VarDictionary::new();
        dict.set("backend_status", MANAGER_BACKEND_STATUS_STAGING_ONLY);
        dict.set("packet_count", self.packets.len() as i64);
        dict.set("pending_upload_count", pending_upload_count);
        dict.set("unsupported_count", unsupported_count);
        dict.set("total_point_count", total_point_count);
        dict.set("total_payload_byte_len", total_payload_byte_len);
        dict.set("max_upload_chunk_bytes", self.max_upload_chunk_bytes);
        dict.set("cpu_mesh_expansion_allowed", false);
        dict
    }
}
