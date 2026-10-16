use async_trait::async_trait;

use std::sync::Arc;

use wsprism_core::error::{Result, WsPrismError};
use wsprism_core::protocol::hot::{encode_hot_frame_v2, HotFrame, HotOutV2};

use crate::dispatch::BinaryService;
use crate::realtime::{HotOut, Outgoing, Payload, QoS, RealtimeCtx};

/// Relays Hot frames to the room they target (v2: `chan`, v1: the active
/// room), stamping the publisher's member id for v2 recipients. v1 sessions
/// without an active room get their payload echoed back.
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

    async fn handle_binary(&self, ctx: &mut RealtimeCtx, frame: HotFrame) -> Result<()> {
        let Some((room, joined)) = ctx.hot_target(&frame) else {
            if frame.chan.is_some() {
                // v2 frames must name a joined room (the session loop already
                // drops unknown ones; this keeps the service safe on its own).
                return Err(WsPrismError::NotAllowed("not a member of that room".into()));
            }
            // v1 without an active room (allowed by policy): echo to self.
            return ctx.send_to_session(Outgoing {
                qos: QoS::Lossy,
                payload: Payload::Binary(frame.payload),
            });
        };
        // Encode once per protocol version; every recipient of a version
        // shares the bytes (ADR 0006 R3). v2 carries the room id and the
        // publisher's member id; v1 gets the payload as it always did.
        let v2 = encode_hot_frame_v2(&HotOutV2 {
            chan: joined.room_id,
            svc_id: frame.svc_id,
            opcode: frame.opcode,
            key: frame.key,
            seq: frame.seq,
            sender: joined.member_id,
            payload: frame.payload.clone(),
        });
        let out = Outgoing {
            qos: QoS::Lossy,
            payload: Payload::Hot(Arc::new(HotOut {
                v1: frame.payload,
                v2,
            })),
        };
        ctx.publish_room_lossy(room, out)
    }
}
