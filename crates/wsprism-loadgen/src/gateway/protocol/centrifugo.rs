//! Centrifugo bidirectional JSON protocol (v4+ command format):
//! `connect` → `subscribe` → client `publish`. Publications arrive as
//! `push.pub`, also to the publisher. Several replies/pushes can share one
//! frame, newline-separated. The server's application ping is `{}` and must
//! be answered with `{}`.
//!
//! Server config: anonymous connect, subscribe and publish allowed
//! (see bench/competitors/centrifugo.json).

use futures_util::SinkExt;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use super::{await_text, json_body, open, probe_from_json, Incoming, Probe, Protocol, Ws};

pub struct Centrifugo {
    /// Base URL, e.g. `ws://10.77.0.1:8000`.
    pub url: String,
}

/// Command ids 1 and 2 are connect/subscribe; publishes count up from here.
const FIRST_PUBLISH_ID: u32 = 100;

impl Protocol for Centrifugo {
    async fn setup(&self, _conn: usize, room: &str) -> Result<Ws, String> {
        let url = format!("{}/connection/websocket", self.url.trim_end_matches('/'));
        let req = url.into_client_request().map_err(|e| e.to_string())?;
        let mut ws = open(req).await?;
        command(&mut ws, 1, "connect", serde_json::json!({})).await?;
        command(
            &mut ws,
            2,
            "subscribe",
            serde_json::json!({ "channel": room }),
        )
        .await?;
        Ok(ws)
    }

    fn encode(&self, room: &str, probe: Probe, seq: u32, payload: usize) -> Message {
        let cmd = serde_json::json!({
            "id": FIRST_PUBLISH_ID.wrapping_add(seq),
            "publish": { "channel": room, "data": json_body(probe, payload) },
        });
        Message::Text(cmd.to_string())
    }

    fn decode(&self, msg: &Message, out: &mut Vec<Incoming>) {
        let Message::Text(t) = msg else { return };
        for line in t.split('\n').filter(|l| !l.is_empty()) {
            if line == "{}" {
                out.push(Incoming::Reply(Message::Text("{}".into())));
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if let Some(p) = probe_from_json(&v["push"]["pub"]["data"]) {
                out.push(Incoming::Probe(p));
            }
        }
    }
}

/// Send command `id` and wait for its reply (an `error` reply fails setup).
async fn command(ws: &mut Ws, id: u32, name: &str, body: serde_json::Value) -> Result<(), String> {
    let cmd = serde_json::json!({ "id": id, name: body });
    ws.send(Message::Text(cmd.to_string()))
        .await
        .map_err(|e| format!("{name} send: {e}"))?;
    await_text(ws, name, |t| {
        t.split('\n').find_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            if v["id"] != id {
                return None;
            }
            Some(match v.get("error") {
                Some(e) => Err(format!("{name} error: {e}")),
                None => Ok(()),
            })
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_batched_pushes_and_answers_pings() {
        let p1 = Probe {
            sender: 1,
            ts_ns: 10,
        };
        let p2 = Probe {
            sender: 2,
            ts_ns: 20,
        };
        let push =
            |p| serde_json::json!({"push": {"channel": "r", "pub": {"data": json_body(p, 64)}}});
        let frame = format!(
            "{}\n{{}}\n{}\n{}",
            push(p1),
            serde_json::json!({"id": 105, "publish": {}}),
            push(p2)
        );
        let mut out = Vec::new();
        Centrifugo { url: String::new() }.decode(&Message::Text(frame), &mut out);
        assert_eq!(
            out,
            vec![
                Incoming::Probe(p1),
                Incoming::Reply(Message::Text("{}".into())),
                Incoming::Probe(p2)
            ]
        );
    }

    #[test]
    fn publish_ids_do_not_collide_with_setup_commands() {
        let Message::Text(t) = Centrifugo { url: String::new() }.encode(
            "r",
            Probe {
                sender: 0,
                ts_ns: 0,
            },
            0,
            64,
        ) else {
            panic!()
        };
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        assert!(v["id"].as_u64().unwrap() > 2);
    }
}
