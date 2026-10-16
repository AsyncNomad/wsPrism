//! wsPrism: bearer ticket, `room:join`, then Hot (binary echo service) or
//! Ext (`chat:send`) probes. Room fan-out includes the sender.

use std::collections::HashMap;
use std::sync::RwLock;

use futures_util::SinkExt;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::Message;

use super::{await_text, open, Incoming, Probe, Protocol, Ws};
use crate::gateway::scenario::Lane;

pub struct Wsprism {
    /// Base URL, e.g. `ws://10.77.0.1:8080`.
    pub url: String,
    pub tenant: String,
    pub ticket: String,
    /// Ticket `<ticket>:u<conn>`: one user per connection.
    pub distinct_users: bool,
    pub lane: Lane,
    /// v2: room name -> room id from `sys.joined` (the same for every member).
    pub chans: RwLock<HashMap<String, u32>>,
}

/// v2 Hot flags as sent by this client: KEY | SEQ.
const V2_FLAGS: u8 = 0x03;

/// Hot frame header: v=1, svc=1 (echo), op=1, flags=0.
const HOT_HEADER: [u8; 4] = [1, 1, 1, 0];
/// Ext probe text: sender (10 digits) + timestamp (20 digits) + padding.
const EXT_PREFIX: usize = 30;

impl Protocol for Wsprism {
    async fn setup(&self, conn: usize, room: &str) -> Result<Ws, String> {
        let url = format!(
            "{}/v1/ws?tenant={}&sid=c{conn}",
            self.url.trim_end_matches('/'),
            self.tenant
        );
        let mut req = url.into_client_request().map_err(|e| e.to_string())?;
        let ticket = if self.distinct_users {
            format!("{}:u{conn}", self.ticket)
        } else {
            self.ticket.clone()
        };
        let auth = format!("Bearer {ticket}")
            .parse()
            .map_err(|_| "ticket is not a valid header value".to_string())?;
        req.headers_mut().insert(AUTHORIZATION, auth);
        if self.lane == Lane::Hot2 {
            let proto = "wsprism.v2.json"
                .parse()
                .map_err(|_| "subprotocol header")?;
            req.headers_mut().insert("sec-websocket-protocol", proto);
        }
        let mut ws = open(req).await?;
        await_text(&mut ws, "sys.authed", |t| sys(t, "authed")).await?;
        let join = serde_json::json!({"v": 1, "svc": "room", "type": "join", "room": room});
        ws.send(Message::Text(join.to_string()))
            .await
            .map_err(|e| format!("join send: {e}"))?;
        if self.lane == Lane::Hot2 {
            let chan = await_text(&mut ws, "sys.joined", |t| {
                sys(t, "joined").map(|r| {
                    r.and_then(|()| {
                        let v: serde_json::Value =
                            serde_json::from_str(t).map_err(|e| e.to_string())?;
                        v["chan"]
                            .as_u64()
                            .map(|c| c as u32)
                            .ok_or_else(|| format!("no chan in {v}"))
                    })
                })
            })
            .await?;
            self.chans
                .write()
                .map_err(|_| "chans lock")?
                .insert(room.to_string(), chan);
        } else {
            await_text(&mut ws, "sys.joined", |t| sys(t, "joined")).await?;
        }
        Ok(ws)
    }

    fn encode(&self, room: &str, probe: Probe, seq: u32, payload: usize) -> Message {
        match self.lane {
            Lane::Hot2 => {
                // [kind=1][KEY|SEQ][chan u32][svc 1][op 1][key=0][seq][ts u64][sender u32][padding]
                let chan = self
                    .chans
                    .read()
                    .ok()
                    .and_then(|m| m.get(room).copied())
                    .unwrap_or(0);
                let body = payload.max(12);
                let mut b = Vec::with_capacity(16 + body);
                b.extend_from_slice(&[0x01, V2_FLAGS]);
                b.extend_from_slice(&chan.to_le_bytes());
                b.extend_from_slice(&[1, 1]);
                b.extend_from_slice(&0u32.to_le_bytes());
                b.extend_from_slice(&seq.to_le_bytes());
                b.extend_from_slice(&probe.ts_ns.to_le_bytes());
                b.extend_from_slice(&probe.sender.to_le_bytes());
                b.resize(16 + body, 0);
                Message::Binary(b)
            }
            Lane::Hot => {
                // [header][ts u64 LE][sender u32 LE][padding]
                let mut b = Vec::with_capacity(HOT_HEADER.len() + payload.max(12));
                b.extend_from_slice(&HOT_HEADER);
                b.extend_from_slice(&probe.ts_ns.to_le_bytes());
                b.extend_from_slice(&probe.sender.to_le_bytes());
                b.resize(HOT_HEADER.len() + payload.max(12), 0);
                Message::Binary(b)
            }
            Lane::Ext => {
                let pad = "x".repeat(payload.saturating_sub(EXT_PREFIX));
                let msg = format!("{:010}{:020}{pad}", probe.sender, probe.ts_ns);
                let env = serde_json::json!({
                    "v": 1, "svc": "chat", "type": "send", "room": room,
                    "data": { "msg": msg }
                });
                Message::Text(env.to_string())
            }
        }
    }

    fn decode(&self, msg: &Message, out: &mut Vec<Incoming>) {
        let probe = match (self.lane, msg) {
            // v2 server frame: [kind][flags][chan][svc][op][key?][seq?][sender][payload]
            (Lane::Hot2, Message::Binary(b)) => (|| {
                let flags = *b.get(1)?;
                if *b.first()? != 0x01 {
                    return None;
                }
                let opt = [0x01u8, 0x02, 0x04]
                    .iter()
                    .filter(|f| flags & **f != 0)
                    .count();
                let p = b.get(8 + 4 * opt..)?;
                Some(Probe {
                    ts_ns: u64::from_le_bytes(p.get(..8)?.try_into().ok()?),
                    sender: u32::from_le_bytes(p.get(8..12)?.try_into().ok()?),
                })
            })(),
            // The echo service broadcasts the payload only (header stripped).
            (Lane::Hot, Message::Binary(b)) => (|| {
                Some(Probe {
                    ts_ns: u64::from_le_bytes(b.get(..8)?.try_into().ok()?),
                    sender: u32::from_le_bytes(b.get(8..12)?.try_into().ok()?),
                })
            })(),
            (Lane::Ext, Message::Text(t)) => (|| {
                let v: serde_json::Value = serde_json::from_str(t).ok()?;
                if v["svc"] != "chat" {
                    return None;
                }
                let m = v["data"]["msg"].as_str()?;
                Some(Probe {
                    sender: m.get(..10)?.parse().ok()?,
                    ts_ns: m.get(10..EXT_PREFIX)?.parse().ok()?,
                })
            })(),
            _ => None,
        };
        out.extend(probe.map(Incoming::Probe));
    }
}

fn sys(text: &str, ty: &str) -> Option<Result<(), String>> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v["svc"] != "sys" {
        return None;
    }
    if v["type"] == ty {
        return Some(Ok(()));
    }
    (v["type"] == "error").then(|| Err(format!("sys.error while waiting for sys.{ty}: {v}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proto(lane: Lane) -> Wsprism {
        Wsprism {
            url: String::new(),
            tenant: "t".into(),
            ticket: "x".into(),
            distinct_users: false,
            lane,
            chans: Default::default(),
        }
    }

    #[test]
    fn hot_probe_round_trips_after_the_echo_strips_the_header() {
        let p = Probe {
            sender: 42,
            ts_ns: 9_876_543_210,
        };
        let Message::Binary(b) = proto(Lane::Hot).encode("r", p, 0, 64) else {
            panic!()
        };
        assert_eq!(b.len(), 4 + 64);
        let echoed = Message::Binary(b[4..].to_vec());
        let mut out = Vec::new();
        proto(Lane::Hot).decode(&echoed, &mut out);
        assert_eq!(out, vec![Incoming::Probe(p)]);
    }

    #[test]
    fn ext_probe_round_trips_through_the_chat_broadcast_shape() {
        let p = Probe {
            sender: 3,
            ts_ns: 55,
        };
        let Message::Text(t) = proto(Lane::Ext).encode("r", p, 0, 64) else {
            panic!()
        };
        let sent: serde_json::Value = serde_json::from_str(&t).unwrap();
        let broadcast = serde_json::json!({"svc": "chat", "type": "msg", "data": sent["data"]});
        let mut out = Vec::new();
        proto(Lane::Ext).decode(&Message::Text(broadcast.to_string()), &mut out);
        assert_eq!(out, vec![Incoming::Probe(p)]);
    }

    #[test]
    fn hot2_probe_round_trips_through_the_server_relay_format() {
        let w = proto(Lane::Hot2);
        w.chans.write().unwrap().insert("r".into(), 42);
        let p = Probe {
            sender: 7,
            ts_ns: 123_456_789,
        };
        let Message::Binary(c) = w.encode("r", p, 9, 64) else {
            panic!()
        };
        assert_eq!(&c[2..6], &42u32.to_le_bytes(), "chan from sys.joined");
        // The relay adds SENDER after key/seq: [kind][flags|4][chan][svc][op][key][seq][sender][payload]
        let mut relayed = c[..16].to_vec();
        relayed[1] |= 0x04;
        relayed.extend_from_slice(&99u32.to_le_bytes());
        relayed.extend_from_slice(&c[16..]);
        let mut out = Vec::new();
        w.decode(&Message::Binary(relayed), &mut out);
        assert_eq!(out, vec![Incoming::Probe(p)]);
    }
}
