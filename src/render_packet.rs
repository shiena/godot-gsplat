use godot::prelude::*;

use crate::asset::GaussianSplatAsset;
use crate::import_state::PAYLOAD_LAYOUT_FLOAT32_V1;

pub const RENDER_STATUS_EMPTY: &str = "empty";
pub const RENDER_STATUS_PENDING_UPLOAD: &str = "pending_upload";
pub const RENDER_UPLOAD_INTERLEAVED_FLOAT32: &str = "interleaved_float32";
pub const RENDER_UPLOAD_UNSUPPORTED: &str = "unsupported";

#[derive(GodotClass)]
#[class(tool, base=Resource)]
pub struct GaussianSplatRenderPacket {
    #[base]
    base: Base<Resource>,

    asset: Option<Gd<GaussianSplatAsset>>,
    revision: i64,
    point_count: i32,
    payload_byte_len: i64,
    payload_layout: GString,
    upload_format: GString,
    pipeline: GString,
    status: GString,
    local_aabb: Aabb,
}

#[godot_api]
impl IResource for GaussianSplatRenderPacket {
    fn init(base: Base<Resource>) -> Self {
        Self {
            base,
            asset: None,
            revision: 0,
            point_count: 0,
            payload_byte_len: 0,
            payload_layout: "".into(),
            upload_format: RENDER_UPLOAD_UNSUPPORTED.into(),
            pipeline: "unconfigured".into(),
            status: RENDER_STATUS_EMPTY.into(),
            local_aabb: Aabb::default(),
        }
    }
}

#[godot_api]
impl GaussianSplatRenderPacket {
    #[func]
    pub fn clear(&mut self) {
        self.asset = None;
        self.revision = 0;
        self.point_count = 0;
        self.payload_byte_len = 0;
        self.payload_layout = "".into();
        self.upload_format = RENDER_UPLOAD_UNSUPPORTED.into();
        self.pipeline = "unconfigured".into();
        self.status = RENDER_STATUS_EMPTY.into();
        self.local_aabb = Aabb::default();
        self.base_mut().emit_changed();
    }

    #[func]
    pub fn get_asset(&self) -> Option<Gd<GaussianSplatAsset>> {
        self.asset.clone()
    }

    #[func]
    pub fn get_revision(&self) -> i64 {
        self.revision
    }

    #[func]
    pub fn get_point_count(&self) -> i32 {
        self.point_count
    }

    #[func]
    pub fn get_payload_byte_len(&self) -> i64 {
        self.payload_byte_len
    }

    #[func]
    pub fn get_payload_layout(&self) -> GString {
        self.payload_layout.clone()
    }

    #[func]
    pub fn get_upload_format(&self) -> GString {
        self.upload_format.clone()
    }

    #[func]
    pub fn get_pipeline(&self) -> GString {
        self.pipeline.clone()
    }

    #[func]
    pub fn get_status(&self) -> GString {
        self.status.clone()
    }

    #[func]
    pub fn get_local_aabb(&self) -> Aabb {
        self.local_aabb
    }

    #[func]
    pub fn is_upload_supported(&self) -> bool {
        self.upload_format == RENDER_UPLOAD_INTERLEAVED_FLOAT32
    }

    #[func]
    pub fn export_packet(&self) -> VarDictionary {
        let mut dict = VarDictionary::new();
        dict.set("revision", self.revision);
        dict.set("point_count", self.point_count as i64);
        dict.set("payload_byte_len", self.payload_byte_len);
        dict.set(
            "payload_layout",
            &Variant::from(self.payload_layout.clone()),
        );
        dict.set("upload_format", &Variant::from(self.upload_format.clone()));
        dict.set("pipeline", &Variant::from(self.pipeline.clone()));
        dict.set("status", &Variant::from(self.status.clone()));
        dict.set("local_aabb", self.local_aabb);
        dict.set("cpu_mesh_expansion_allowed", false);
        dict
    }
}

impl GaussianSplatRenderPacket {
    pub fn prepare_from_asset(
        &mut self,
        asset: &Gd<GaussianSplatAsset>,
        pipeline: &str,
        revision: i64,
    ) {
        let asset_ref = asset.bind();
        let payload_layout = asset_ref.get_payload_layout();
        self.asset = Some(asset.clone());
        self.revision = revision;
        self.point_count = asset_ref.get_point_count();
        self.payload_byte_len = asset_ref.get_payload_byte_len();
        self.payload_layout = payload_layout.clone();
        self.upload_format = resolve_upload_format(payload_layout.to_string().as_str()).into();
        self.pipeline = pipeline.into();
        self.status = if self.is_upload_supported() {
            RENDER_STATUS_PENDING_UPLOAD
        } else {
            RENDER_UPLOAD_UNSUPPORTED
        }
        .into();
        self.local_aabb = asset_ref.get_local_aabb();
        self.base_mut().emit_changed();
    }
}

fn resolve_upload_format(payload_layout: &str) -> &'static str {
    match payload_layout {
        PAYLOAD_LAYOUT_FLOAT32_V1 => RENDER_UPLOAD_INTERLEAVED_FLOAT32,
        _ => RENDER_UPLOAD_UNSUPPORTED,
    }
}
