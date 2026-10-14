//! One benchmark connection: handshake, room join, then send/receive loops.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use hdrhistogram::Histogram;
use tokio::net::TcpStream;
use tokio::time::{interval_at, Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::probe;
use super::scenario::Lane;

pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const SETUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Connect, wait for `sys.authed`, join `room` and wait for `sys.joined`.
pub async fn setup(url: &str, ticket: &str, room: &str) -> Result<Ws, String> {
    let mut req = url.into_client_request().map_err(|e| e.to_string())?;
    let auth = format!("Bearer {ticket}")
        .parse()
        .map_err(|_| "ticket is not a valid header value".to_string())?;
    req.headers_mut().insert(AUTHORIZATION, auth);
    // TCP_NODELAY like real WebSocket clients (browsers), so client-side Nagle
    // delays don't pollute the measured latency.
    let connect = tokio_tungstenite::connect_async_with_config(req, None, true);
    let (mut ws, _) = tokio::time::timeout(SETUP_TIMEOUT, connect)
        .await
        .map_err(|_| "connect timeout".to_string())?
        .map_err(|e| format!("connect: {e}"))?;
    wait_sys(&mut ws, "authed").await?;
    let join = serde_json::json!({"v": 1, "svc": "room", "type": "join", "room": room});
    ws.send(Message::Text(join.to_string()))
        .await
        .map_err(|e| format!("join send: {e}"))?;
    wait_sys(&mut ws, "joined").await?;
    Ok(ws)
}

async fn wait_sys(ws: &mut Ws, ty: &str) -> Result<(), String> {
    let deadline = Instant::now() + SETUP_TIMEOUT;
    loop {
        let msg = tokio::time::timeout_at(deadline, ws.next())
            .await
            .map_err(|_| format!("timeout waiting for sys.{ty}"))?
            .ok_or_else(|| format!("closed before sys.{ty}"))?
            .map_err(|e| format!("read: {e}"))?;
        if let Message::Text(t) = &msg {
            let v: serde_json::Value = serde_json::from_str(t).map_err(|e| e.to_string())?;
            if v["svc"] == "sys" && v["type"] == ty {
                return Ok(());
            }
            if v["svc"] == "sys" && v["type"] == "error" {
                return Err(format!("sys.error while waiting for sys.{ty}: {v}"));
            }
        }
    }
}

/// What one connection observed.
pub struct ConnStats {
    pub sent: u64,
    pub received: u64,
    /// End-to-end latency in microseconds.
    pub latency_us: Histogram<u64>,
    pub closed_early: bool,
}

/// Run until `end`: publish probes at `rate`/s if `publish`, and record the
/// latency of every probe received. Keeps reading for `grace` after `end`.
pub async fn run(
    ws: Ws,
    lane: Lane,
    room: String,
    publish: Option<(u64, usize)>,
    start: Instant,
    end: Instant,
    grace: Duration,
) -> ConnStats {
    let (mut tx, mut rx) = ws.split();
    let sender = async move {
        let Some((rate, payload)) = publish else {
            return 0;
        };
        let mut tick = interval_at(start, Duration::from_nanos(1_000_000_000 / rate.max(1)));
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut sent = 0;
        while Instant::now() < end {
            tick.tick().await;
            if tx.send(probe::encode(lane, &room, payload)).await.is_err() {
                break;
            }
            sent += 1;
        }
        sent
    };
    let receiver = async move {
        let mut hist =
            Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).expect("histogram bounds");
        let mut received = 0;
        let mut closed_early = false;
        let stop = end + grace;
        loop {
            match tokio::time::timeout_at(stop, rx.next()).await {
                Err(_) => break,
                Ok(Some(Ok(msg))) => {
                    if let Some(ts) = probe::decode(lane, &msg) {
                        let us = probe::now_nanos().saturating_sub(ts) / 1_000;
                        let _ = hist.record(us.max(1));
                        received += 1;
                    } else if matches!(msg, Message::Close(_)) {
                        closed_early = Instant::now() < end;
                        break;
                    }
                }
                Ok(_) => {
                    closed_early = Instant::now() < end;
                    break;
                }
            }
        }
        (received, hist, closed_early)
    };
    let (sent, (received, latency_us, closed_early)) = tokio::join!(sender, receiver);
    ConnStats {
        sent,
        received,
        latency_us,
        closed_early,
    }
}
