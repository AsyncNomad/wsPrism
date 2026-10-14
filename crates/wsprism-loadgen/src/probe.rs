//! Timestamped probe messages, one encoding per lane.
//!
//! Every probe carries the send time as nanoseconds since a process-wide
//! epoch, so a receiver computes end-to-end latency without clock sync
//! (publisher and receivers live in the same process).

use std::sync::OnceLock;
use std::time::Instant;

use tokio_tungstenite::tungstenite::Message;

use crate::scenario::Lane;

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

pub fn now_nanos() -> u64 {
    epoch().elapsed().as_nanos() as u64
}

/// Build a probe for `room` with `payload` total payload bytes (min 8).
pub fn encode(lane: Lane, room: &str, payload: usize) -> Message {
    let ts = now_nanos();
    match lane {
        Lane::Hot => {
            // [v=1][svc=1 echo][op=1][flags=0][ts u64 LE][padding]
            let mut b = Vec::with_capacity(4 + payload.max(8));
            b.extend_from_slice(&[1, 1, 1, 0]);
            b.extend_from_slice(&ts.to_le_bytes());
            b.resize(4 + payload.max(8), 0);
            Message::Binary(b)
        }
        Lane::Ext => {
            let pad = "x".repeat(payload.saturating_sub(20));
            let msg = format!("{ts:020}{pad}");
            let env = serde_json::json!({
                "v": 1, "svc": "chat", "type": "send", "room": room,
                "data": { "msg": msg }
            });
            Message::Text(env.to_string())
        }
    }
}

/// Send timestamp of a received probe, or None for any other message.
pub fn decode(lane: Lane, msg: &Message) -> Option<u64> {
    match (lane, msg) {
        // The echo service broadcasts the payload only (header stripped).
        (Lane::Hot, Message::Binary(b)) => {
            let ts: [u8; 8] = b.get(..8)?.try_into().ok()?;
            Some(u64::from_le_bytes(ts))
        }
        (Lane::Ext, Message::Text(t)) => {
            let v: serde_json::Value = serde_json::from_str(t).ok()?;
            if v["svc"] != "chat" {
                return None;
            }
            v["data"]["msg"].as_str()?.get(..20)?.parse().ok()
        }
        _ => None,
    }
}
