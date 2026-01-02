use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use wsprism_core::error::{Result, WsPrismError};
use wsprism_core::protocol::text::Envelope;

use crate::dispatch::TextService;
use crate::realtime::{Outgoing, Payload, QoS, RealtimeCtx};

#[derive(Default)]
pub struct ChatService;

impl ChatService {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Debug, Deserialize)]
struct SendReq {
    msg: String,
}

#[async_trait]
impl TextService for ChatService {
    fn svc(&self) -> &'static str {
        "chat"
    }

    async fn handle(&self, ctx: RealtimeCtx, env: Envelope) -> Result<()> {
        match env.msg_type.as_str() {
            "send" => {
                let room = env
                    .room
                    .clone()
                    .ok_or_else(|| WsPrismError::BadRequest("chat.send requires room".into()))?;

                let raw = env
                    .data
                    .as_ref()
                    .ok_or_else(|| WsPrismError::BadRequest("chat.send requires data".into()))?;

                let req: SendReq = serde_json::from_str(raw.get())
                    .map_err(|e| WsPrismError::BadRequest(format!("chat.send invalid data: {e}")))?;

                let out = Outgoing {
                    qos: QoS::Reliable { timeout_ms: 1500 },
                    payload: Payload::TextJson(json!({
                        "v": 1,
                        "svc": "chat",
                        "type": "msg",
                        "room": room,
                        "data": { "from": ctx.user(), "msg": req.msg }
                    })),
                };

                // ✅ room은 이미 있으니 그대로 사용
                ctx.publish_room_reliable(&room, out).await
            }
            _ => Err(WsPrismError::BadRequest("unknown chat type".into())),
        }
    }
}
