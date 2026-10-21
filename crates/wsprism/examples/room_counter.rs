//! A gateway with one custom service, built only from the `wsprism` crate.
//!
//! `counter.incr` (in a joined room) bumps that room's counter and
//! broadcasts the new value to everyone in the room; `counter.get` answers
//! the caller only.
//!
//! Run:  cargo run -p wsprism --example room_counter -- path/to/wsprism.yaml
//! The config's tenant needs `ext_allowlist: ["room:*", "counter:*"]`.

use std::collections::HashMap;
use std::sync::Mutex;

use wsprism::prelude::*;
use wsprism::serde_json::json;

#[derive(Default)]
struct RoomCounter {
    counts: Mutex<HashMap<String, u64>>,
}

#[async_trait]
impl TextService for RoomCounter {
    fn svc(&self) -> &'static str {
        "counter"
    }

    async fn handle(&self, ctx: &mut RealtimeCtx, env: Envelope) -> Result<()> {
        let room = env
            .room
            .ok_or_else(|| WsPrismError::BadRequest("counter needs a room".into()))?;
        if !ctx.is_in_room(&room) {
            return Err(WsPrismError::NotAllowed("join the room first".into()));
        }
        // The key includes the tenant: one service instance serves every tenant.
        let key = format!("{}/{room}", ctx.tenant());
        let mut counts = self.counts.lock().unwrap_or_else(|p| p.into_inner());
        match env.msg_type.as_str() {
            "incr" => {
                let n = counts.entry(key).or_default();
                *n += 1;
                let msg = json!({"v": 1, "svc": "counter", "type": "value", "room": room, "data": {"n": *n, "by": ctx.user()}});
                drop(counts);
                ctx.publish_room_reliable(
                    &room,
                    Outgoing {
                        qos: QoS::Reliable,
                        payload: Payload::TextJson(msg),
                    },
                )
            }
            "get" => {
                let n = counts.get(&key).copied().unwrap_or(0);
                ctx.reply(json!({"v": 1, "svc": "counter", "type": "value", "room": room, "data": {"n": n}}))
            }
            other => Err(WsPrismError::BadRequest(format!(
                "unknown counter type: {other}"
            ))),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "wsprism.yaml".into());
    Gateway::builder()
        .config_file(config)?
        .text_service(RoomCounter::default())
        .build()?
        .serve()
        .await
}
