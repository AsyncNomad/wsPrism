//! Built-in room membership service (Ext Lane `room:*`).

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::value::RawValue;

use wsprism_protocol::error::{Result, WsPrismError};
use wsprism_protocol::protocol::text::Envelope;
use wsprism_protocol::protocol::ProtocolVersion;

use crate::dispatch::TextService;
use crate::realtime::core::{JoinOutcome, MemberSnapshot, Recover};
use crate::realtime::RealtimeCtx;
use crate::sys;

/// `room.join` data: `{"recover": {"epoch": "...", "offset": n}}`.
#[derive(Deserialize)]
struct JoinData {
    #[serde(default)]
    recover: Option<RecoverReq>,
}

#[derive(Deserialize)]
struct RecoverReq {
    epoch: String,
    offset: u64,
}

impl From<RecoverReq> for Recover {
    fn from(r: RecoverReq) -> Self {
        Recover {
            epoch: r.epoch,
            offset: r.offset,
        }
    }
}

/// `room:join` / `room:leave`. The joined room becomes the session's active
/// room (the Hot Lane target); see `docs/PROTOCOL.md` §Rooms.
#[derive(Default)]
pub struct RoomService;

impl RoomService {
    /// The built-in `room` service.
    pub fn new() -> Self {
        Self
    }

    async fn join(
        ctx: &mut RealtimeCtx,
        room: Option<String>,
        data: Option<&RawValue>,
    ) -> Result<()> {
        let room = room.unwrap_or_else(|| "default".to_string());
        let recover = match data {
            Some(raw) => serde_json::from_str::<JoinData>(raw.get())
                .map_err(|e| WsPrismError::BadRequest(format!("room.join invalid data: {e}")))?
                .recover
                .map(Recover::from),
            None => None,
        };
        match ctx.cluster_history(&room) {
            Some(keep) => {
                ctx.join_room_recovering(&room, keep, recover.as_ref(), |ctx, out| {
                    joined_msg(ctx, &room, out)
                })
                .await?
            }
            None => ctx.join_room_and_reply(&room, recover.as_ref(), |ctx, out| {
                joined_msg(ctx, &room, out)
            })?,
        };
        ctx.set_active_room(Some(&room));
        Ok(())
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
        ctx.reply(sys::left(&room, ctx.trace_id()))
    }
}

/// The `sys.joined` reply.
fn joined_msg(ctx: &RealtimeCtx, room: &str, out: &JoinOutcome) -> serde_json::Value {
    let mut msg = match ctx.protocol() {
        ProtocolVersion::V1 => sys::joined(room, ctx.trace_id()),
        ProtocolVersion::V2 => sys::joined_v2(
            room,
            out.joined.room_id,
            out.joined.member_id,
            ctx.trace_id(),
        ),
    };
    // Presence snapshot (tenant presence_events, ADR 0010).
    if let Some(snapshot) = ctx.room_members(room) {
        add_snapshot(&mut msg, &snapshot);
    }
    // History position and recovery result (ADR 0011 §3).
    if let Some(pos) = &out.history {
        msg["epoch"] = (*pos.epoch).into();
        msg["offset"] = pos.offset.into();
    }
    if let Some(recovered) = out.recovered {
        msg["recovered"] = recovered.into();
    }
    msg
}

/// `members` (and `members_truncated`) of a `sys.joined`.
fn add_snapshot(msg: &mut serde_json::Value, snapshot: &MemberSnapshot) {
    msg["members"] = snapshot
        .members
        .iter()
        .map(|m| serde_json::json!({"user": &*m.user, "member_id": m.member_id}))
        .collect();
    if snapshot.truncated {
        msg["members_truncated"] = true.into();
    }
}

#[async_trait]
impl TextService for RoomService {
    fn svc(&self) -> &'static str {
        "room"
    }

    async fn handle(&self, ctx: &mut RealtimeCtx, env: Envelope) -> Result<()> {
        match env.msg_type.as_str() {
            "join" => Self::join(ctx, env.room, env.data.as_deref()).await,
            "leave" => Self::leave(ctx, env.room),
            _ => Err(WsPrismError::BadRequest("unknown room type".into())),
        }
    }
}
