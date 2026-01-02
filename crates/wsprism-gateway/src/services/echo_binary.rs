use async_trait::async_trait;

use wsprism_core::error::Result;
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
        let out = Outgoing {
            qos: QoS::Lossy,
            payload: Payload::Binary(frame.payload.clone()),
        };

        if let Some(room) = ctx.active_room() {
            // room-based publish
            ctx.publish_room_lossy(room, out)?;
            Ok(())
        } else {
            // roomless: only echo to current session
            ctx.send_to_session(out)?;
            Ok(())
        }
    }
}
