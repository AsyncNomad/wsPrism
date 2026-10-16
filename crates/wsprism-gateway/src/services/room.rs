//! Built-in room membership service (Ext Lane `room:*`).

use async_trait::async_trait;

use wsprism_core::error::{Result, WsPrismError};
use wsprism_core::protocol::text::Envelope;
use wsprism_core::protocol::ProtocolVersion;

use crate::dispatch::TextService;
use crate::realtime::RealtimeCtx;
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
        let joined = ctx.join_room(&room)?;
        ctx.set_active_room(Some(&room));
        let msg = match ctx.protocol() {
            ProtocolVersion::V1 => sys::joined(&room, &ctx.trace_id),
            ProtocolVersion::V2 => {
                sys::joined_v2(&room, joined.room_id, joined.member_id, &ctx.trace_id)
            }
        };
        ctx.reply(msg)
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
        ctx.reply(sys::left(&room, &ctx.trace_id))
    }
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
