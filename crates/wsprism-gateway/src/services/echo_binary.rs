use async_trait::async_trait;

use wsprism_core::error::{Result, WsPrismError};
use wsprism_core::protocol::hot::HotFrame;

use crate::dispatch::BinaryService;
use crate::realtime::{Outgoing, Payload, QoS, RealtimeCtx};

/// Echo binary frames to active_room (Lossy). Useful to prove Hot Lane routing.
pub struct EchoBinaryService {
    svc_id: u8,
}

impl EchoBinaryService {
    pub fn new(svc_id: u8) -> Self {
        Self { svc_id }
    }
}

#[async_trait]
impl BinaryService for EchoBinaryService {
    fn svc_id(&self) -> u8 {
        self.svc_id
    }

    async fn handle_binary(&self, ctx: RealtimeCtx, frame: HotFrame) -> Result<()> {
        let room = ctx
            .active_room()
            .ok_or_else(|| WsPrismError::BadRequest("no active_room".into()))?;

        // Production: do NOT convert to JSON. Broadcast compact binary.
        // Here we broadcast frame.payload as-is (you can add a small header if needed).
        let out = Outgoing {
            qos: QoS::Lossy,
            payload: Payload::Binary(frame.payload.clone()),
        };

        ctx.publish_room_lossy(room, out)
    }
}
