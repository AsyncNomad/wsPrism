use serde_json::json;
use serde_json::value::RawValue;

use crate::{
    error::{AppError, Result},
    realtime::{
        core::{RealtimeCtx, RealtimeService},
        protocol::{
            envelope::{Envelope, Outgoing, OutgoingText, Payload},
            qos::QoS,
            types::RoomId,
        },
    },
};

#[derive(Default)]
pub struct ChatService;

impl ChatService {
    pub fn new() -> Self { Self::default() }
}

/// IMPORTANT (framework rule):
/// With lazy parsing, services must handle parsing errors gracefully.
fn parse_data<T: serde::de::DeserializeOwned>(raw: Option<Box<RawValue>>) -> Result<T> {
    let raw = raw.ok_or_else(|| AppError::BadRequest("missing data".into()))?;
    serde_json::from_str(raw.get()).map_err(|_| AppError::BadRequest("invalid data json".into()))
}

impl RealtimeService for ChatService {
    fn name(&self) -> &'static str { "chat" }

    fn handle<'a>(
        &'a self,
        ctx: RealtimeCtx,
        env: Envelope,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            match env.msg_type.as_str() {
                "join" => {
                    let room = env.room.ok_or_else(|| AppError::BadRequest("chat.join requires room".into()))?;
                    ctx.join_room(&RoomId(room));
                    Ok(())
                }
                "send" => {
                    #[derive(serde::Deserialize)]
                    struct ChatSend { text: String }

                    let room = env.room.ok_or_else(|| AppError::BadRequest("chat.send requires room".into()))?;
                    let req: ChatSend = parse_data(env.data)?;
                    let text = req.text.chars().take(500).collect::<String>();

                    let out = Outgoing {
                        qos: QoS::Reliable { timeout_ms: 50 },
                        payload: Payload::TextJson(OutgoingText {
                            svc: "chat".into(),
                            msg_type: "message".into(),
                            room: Some(room.clone()),
                            data: json!({ "from": ctx.user_id.0, "text": text }),
                        }),
                    };

                    ctx.publish_room_reliable(&RoomId(room), out).await?;
                    Ok(())
                }
                _ => Err(AppError::BadRequest(format!("unknown chat type: {}", env.msg_type))),
            }
        })
    }
}
