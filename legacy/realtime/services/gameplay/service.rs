use bytes::{BufMut, Bytes, BytesMut};
use serde_json::{json, value::RawValue};

use crate::{
    error::{AppError, Result},
    realtime::{
        core::{BinaryService, RealtimeCtx, RealtimeService},
        protocol::{
            envelope::{Envelope, Outgoing, OutgoingText, Payload},
            inbound::BinaryFrame,
            qos::QoS,
            types::RoomId,
        },
    },
};

#[derive(Default)]
pub struct GameplayService;

impl GameplayService {
    pub fn new() -> Self {
        Self::default()
    }
}

/// IMPORTANT (framework rule):
/// With lazy parsing, services must handle parsing errors gracefully.
/// Never unwrap JSON parsing in production path.
fn parse_data<T: serde::de::DeserializeOwned>(raw: Option<Box<RawValue>>) -> Result<T> {
    let raw = raw.ok_or_else(|| AppError::BadRequest("missing data".into()))?;
    serde_json::from_str(raw.get()).map_err(|_| AppError::BadRequest("invalid data json".into()))
}

/// Sample payload parser for opcode=1 (demo only).
///
/// Current payload format (kept for backward compatibility with your v3 demo client):
/// payload = [room_len u8][room bytes][seq u32 LE][keys u16 LE][mx i16 LE][my i16 LE]
///
/// NOTE (Pro Tip):
/// In real production, you usually *do not* include room_id string on every input packet.
/// Instead: join once (stateful), then input payload becomes: [seq][keys][mx][my] only.
/// That needs either:
/// - ctx/core exposing "current room(s) of user", or
/// - a per-session routing table in core.
/// We keep the current format because we are asked to modify *only this file*.
fn parse_opcode1(payload: &Bytes) -> Result<(RoomId, u32, u16, i16, i16)> {
    if payload.len() < 1 {
        return Err(AppError::BadRequest("payload too short".into()));
    }

    let room_len = payload[0] as usize;
    let need = 1 + room_len + 4 + 2 + 2 + 2;
    if payload.len() < need {
        return Err(AppError::BadRequest("payload truncated".into()));
    }

    let room_bytes = &payload[1..1 + room_len];
    let room = std::str::from_utf8(room_bytes).map_err(|_| AppError::BadRequest("room not utf8".into()))?;

    let mut idx = 1 + room_len;
    let seq = u32::from_le_bytes(payload[idx..idx + 4].try_into().unwrap());
    idx += 4;
    let keys = u16::from_le_bytes(payload[idx..idx + 2].try_into().unwrap());
    idx += 2;
    let mx = i16::from_le_bytes(payload[idx..idx + 2].try_into().unwrap());
    idx += 2;
    let my = i16::from_le_bytes(payload[idx..idx + 2].try_into().unwrap());

    Ok((RoomId(room.to_string()), seq, keys, mx, my))
}

/// Production binary response frame for gameplay broadcasts.
///
/// Why not JSON?
/// - JSON serialize per packet is CPU heavy.
/// - JSON expands payload (bandwidth waste).
///
/// We keep it minimal:
/// [opcode_out u8 = 2][seq u32 LE][keys u16 LE][mx i16 LE][my i16 LE]
///
/// Sender identity:
/// - For serious production, you want a compact numeric `player_id (u32/u64)`
///   and include it here.
/// - This skeleton doesn't define numeric IDs, so we omit it.
fn build_opcode2_moved(seq: u32, keys: u16, mx: i16, my: i16) -> Bytes {
    // 1 + 4 + 2 + 2 + 2 = 11 bytes
    let mut buf = BytesMut::with_capacity(11);
    buf.put_u8(2); // opcode 2: moved (server -> clients)
    buf.put_u32_le(seq);
    buf.put_u16_le(keys);
    buf.put_i16_le(mx);
    buf.put_i16_le(my);
    buf.freeze()
}

impl RealtimeService for GameplayService {
    fn name(&self) -> &'static str {
        "game"
    }

    fn handle<'a>(
        &'a self,
        ctx: RealtimeCtx,
        env: Envelope,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            match env.msg_type.as_str() {
                "join" => {
                    let room =
                        env.room.ok_or_else(|| AppError::BadRequest("game.join requires room".into()))?;
                    ctx.join_room(&RoomId(room));
                    Ok(())
                }

                // JSON fallback (debug/compat). Not latency-critical.
                "input" => {
                    // Optional: You can define a stricter schema here.
                    // For now, keep it flexible for testing.
                    let room =
                        env.room.ok_or_else(|| AppError::BadRequest("game.input requires room".into()))?;

                    // If client sends any JSON in data, decode it safely.
                    // This path isn't "extreme latency" anyway; it exists for debug and quick tests.
                    let data: serde_json::Value = match env.data {
                        Some(raw) => serde_json::from_str(raw.get()).unwrap_or(json!({})),
                        None => json!({}),
                    };

                    let out = Outgoing {
                        qos: QoS::Lossy,
                        payload: Payload::TextJson(OutgoingText {
                            svc: "game".into(),
                            msg_type: "input".into(),
                            room: Some(room.clone()),
                            data: json!({ "from": ctx.user_id.0, "seq": env.seq, "data": data }),
                        }),
                    };

                    ctx.publish_room_lossy(&RoomId(room), out)?;
                    Ok(())
                }

                _ => Err(AppError::BadRequest(format!("unknown game type: {}", env.msg_type))),
            }
        })
    }
}

impl BinaryService for GameplayService {
    fn svc_id(&self) -> u8 {
        1
    }

    fn handle_binary<'a>(
        &'a self,
        ctx: RealtimeCtx,
        frame: BinaryFrame,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            match frame.opcode {
                // opcode 1: client input (move)
                1 => {
                    let (room, seq, keys, mx, my) = parse_opcode1(&frame.payload)?;

                    // Production: broadcast binary (no serde_json allocation/serialization)
                    let bytes = build_opcode2_moved(seq, keys, mx, my);

                    let out = Outgoing {
                        qos: QoS::Lossy,
                        payload: Payload::Binary(bytes),
                    };

                    // Lossy is correct for realtime input: drop on overflow.
                    ctx.publish_room_lossy(&room, out)?;
                    Ok(())
                }

                _ => Err(AppError::BadRequest("unknown opcode".into())),
            }
        })
    }
}
