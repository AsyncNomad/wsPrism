//! Built-in room membership service (Ext Lane `room:*`).

use async_trait::async_trait;

use wsprism_core::error::{Result, WsPrismError};
use wsprism_core::protocol::text::Envelope;

use crate::dispatch::TextService;
use crate::realtime::{Outgoing, Payload, QoS, RealtimeCtx};
use crate::sys;

/// `room:join` / `room:leave`. The joined room becomes the session's active
/// room (the Hot Lane target); see `docs/PROTOCOL.md` §Rooms.
#[derive(Default)]
pub struct RoomService;

impl RoomService {
    pub fn new() -> Self {
        Self
    }

    fn join(ctx: &mut RealtimeCtx, room: Option<String>) -> Result<()> {
        let room = room.unwrap_or_else(|| "default".to_string());
        ctx.join_room(&room)?;
        ctx.set_active_room(Some(&room));
        reply(ctx, sys::joined(&room, &ctx.trace_id))
    }

    /// C4: leave the named room; without one, the active room.
    fn leave(ctx: &mut RealtimeCtx, room: Option<String>) -> Result<()> {
        let room = room
            .or_else(|| ctx.active_room().map(str::to_string))
            .filter(|r| ctx.is_in_room(r))
            .ok_or_else(|| WsPrismError::BadRequest("not a member of that room".into()))?;
        ctx.leave_room(&room);
        if ctx.active_room() == Some(room.as_str()) {
            ctx.set_active_room(None);
        }
        reply(ctx, sys::left(&room, &ctx.trace_id))
    }
}

/// Answer this session. Reliable: if its queue is full it is closed as a
/// slow consumer rather than silently missing the reply.
fn reply(ctx: &RealtimeCtx, text: String) -> Result<()> {
    ctx.send_to_session(Outgoing {
        qos: QoS::Reliable,
        payload: Payload::Utf8Bytes(text.into()),
    })
}

#[async_trait]
impl TextService for RoomService {
    fn svc(&self) -> &'static str {
        "room"
    }

    async fn handle(&self, ctx: &mut RealtimeCtx, env: Envelope) -> Result<()> {
        match env.msg_type.as_str() {
            "join" => Self::join(ctx, env.room),
            "leave" => Self::leave(ctx, env.room),
            _ => Err(WsPrismError::BadRequest("unknown room type".into())),
        }
    }
}
