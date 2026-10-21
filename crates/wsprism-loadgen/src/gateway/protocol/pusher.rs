//! Pusher Channels protocol 7 (Sockudo): `pusher:connection_established`
//! gives a socket id, the client signs `<socket_id>:<channel>` with the app
//! secret (HMAC-SHA256) to subscribe to `private-<room>`, then sends
//! `client-` events, which the server relays to the other subscribers (not
//! to the sender, per the Pusher spec). `pusher:ping` must be answered.
//!
//! The loadgen signs subscriptions itself, standing in for the app backend's
//! auth endpoint, so no extra HTTP hop is measured.
//! Server config: client events enabled, event rate limit raised
//! (see bench/competitors/sockudo.json).

use futures_util::SinkExt;
use ring::hmac;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use super::{await_text, json_body, open, probe_from_json, Incoming, Probe, Protocol, Ws};

pub struct Pusher {
    /// Base URL, e.g. `ws://10.77.0.1:6001`.
    pub url: String,
    pub key: String,
    pub secret: String,
}

const EVENT: &str = "client-probe";

impl Pusher {
    fn channel(room: &str) -> String {
        format!("private-{room}")
    }

    /// Private channel auth string: `<key>:<hex hmac_sha256(secret, socket_id:channel)>`.
    fn auth(&self, socket_id: &str, channel: &str) -> String {
        let key = hmac::Key::new(hmac::HMAC_SHA256, self.secret.as_bytes());
        let tag = hmac::sign(&key, format!("{socket_id}:{channel}").as_bytes());
        let hex: String = tag.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        format!("{}:{hex}", self.key)
    }
}

impl Protocol for Pusher {
    async fn setup(&self, _conn: usize, room: &str) -> Result<Ws, String> {
        let url = format!(
            "{}/app/{}?protocol=7&client=wsprism-loadgen&version=0.1",
            self.url.trim_end_matches('/'),
            self.key
        );
        let req = url.into_client_request().map_err(|e| e.to_string())?;
        let mut ws = open(req).await?;
        let socket_id = await_text(&mut ws, "pusher:connection_established", |t| {
            let v = event(t)?;
            match v["event"].as_str()? {
                "pusher:connection_established" => Some(
                    data(&v)
                        .and_then(|d| d["socket_id"].as_str().map(str::to_owned))
                        .ok_or_else(|| format!("no socket_id in {v}")),
                ),
                "pusher:error" => Some(Err(format!("pusher:error {v}"))),
                _ => None,
            }
        })
        .await?;
        let channel = Self::channel(room);
        let sub = serde_json::json!({
            "event": "pusher:subscribe",
            "data": { "channel": channel, "auth": self.auth(&socket_id, &channel) },
        });
        ws.send(Message::Text(sub.to_string()))
            .await
            .map_err(|e| format!("subscribe send: {e}"))?;
        await_text(&mut ws, "subscription_succeeded", |t| {
            let v = event(t)?;
            match v["event"].as_str()? {
                "pusher_internal:subscription_succeeded" if v["channel"] == channel.as_str() => {
                    Some(Ok(()))
                }
                "pusher:error" | "pusher:subscription_error" => Some(Err(format!("{v}"))),
                _ => None,
            }
        })
        .await?;
        Ok(ws)
    }

    fn encode(&self, room: &str, probe: Probe, _seq: u32, payload: usize) -> Message {
        let ev = serde_json::json!({
            "event": EVENT,
            "channel": Self::channel(room),
            "data": json_body(probe, payload),
        });
        Message::Text(ev.to_string())
    }

    fn decode(&self, msg: &Message, out: &mut Vec<Incoming>) {
        let Message::Text(t) = msg else { return };
        let Some(v) = event(t) else { return };
        match v["event"].as_str() {
            Some(EVENT) => {
                if let Some(p) = data(&v).as_ref().and_then(probe_from_json) {
                    out.push(Incoming::Probe(p));
                }
            }
            Some("pusher:ping") => {
                let pong = serde_json::json!({"event": "pusher:pong", "data": {}});
                out.push(Incoming::Reply(Message::Text(pong.to_string())));
            }
            _ => {}
        }
    }
}

fn event(text: &str) -> Option<serde_json::Value> {
    serde_json::from_str(text).ok()
}

/// Pusher puts `data` either as an object or as a JSON-encoded string.
fn data(v: &serde_json::Value) -> Option<serde_json::Value> {
    match &v["data"] {
        serde_json::Value::String(s) => serde_json::from_str(s).ok(),
        d @ serde_json::Value::Object(_) => Some(d.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proto() -> Pusher {
        Pusher {
            url: String::new(),
            key: "278d425bdf160c739803".into(),
            secret: "7ad3773142a6692b25b8".into(),
        }
    }

    #[test]
    fn private_channel_auth_matches_the_pusher_docs_example() {
        // https://pusher.com/docs/channels/library_auth_reference/auth-signatures/
        assert_eq!(
            proto().auth("1234.1234", "private-foobar"),
            "278d425bdf160c739803:58df8b0c36d6982b82c3ecf6b4662e34fe8c25bba48f5369f135bf843651c3a4"
        );
    }

    #[test]
    fn decodes_string_or_object_data_and_answers_pings() {
        let p = Probe {
            sender: 5,
            ts_ns: 77,
        };
        let body = json_body(p, 64);
        let as_obj = serde_json::json!({"event": EVENT, "channel": "private-r", "data": body});
        let as_str =
            serde_json::json!({"event": EVENT, "channel": "private-r", "data": body.to_string()});
        let ping = serde_json::json!({"event": "pusher:ping", "data": {}});
        let mut out = Vec::new();
        for m in [as_obj, as_str, ping] {
            proto().decode(&Message::Text(m.to_string()), &mut out);
        }
        assert_eq!(out[..2], [Incoming::Probe(p), Incoming::Probe(p)]);
        assert!(matches!(&out[2], Incoming::Reply(Message::Text(t)) if t.contains("pusher:pong")));
    }
}
