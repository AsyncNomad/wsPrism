//! Integration-test harness: boots a real gateway on an ephemeral port and
//! drives it with a real WebSocket client.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use wsprism_gateway::{app_state::AppState, config, server};

pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How long a test waits for an expected event before failing.
pub const WAIT: Duration = Duration::from_secs(3);

/// Baseline config: one tenant `t` with a small frame limit, chat + room
/// allowed on the Ext lane and svc 1 (echo) allowed on the Hot lane.
pub const BASE_YAML: &str = r#"
version: 1
tenants:
  - id: t
    limits:
      max_frame_bytes: 1024
    policy:
      ext_allowlist: ["room:*", "chat:*"]
      hot_allowlist: ["1:*"]
      hot_requires_active_room: false
"#;

pub struct Gateway {
    pub addr: SocketAddr,
    pub state: AppState,
}

/// Boot a gateway from YAML on 127.0.0.1:0.
///
/// The server runs on its own OS thread and multi-thread runtime, isolated
/// from the test's (client) runtime. If the server deadlocks or blocks a
/// worker, the client side still times out and the test *fails* instead of
/// hanging forever.
pub async fn spawn(yaml: &str) -> Gateway {
    let cfg = config::load_from_str(yaml).expect("test config must parse");
    let state = AppState::new(cfg).expect("app state");
    let served = state.clone();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let addr = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("server runtime");
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).expect("listener");
            // Same server stack as the binary (socket options, drain).
            server::serve(listener, served, std::future::pending())
                .await
                .expect("serve");
        });
    });
    Gateway { addr, state }
}

pub fn ws_url(addr: SocketAddr, tenant: &str, ticket: &str, sid: Option<&str>) -> String {
    let mut url = format!("ws://{addr}/v1/ws?tenant={tenant}&ticket={ticket}");
    if let Some(sid) = sid {
        url.push_str(&format!("&sid={sid}"));
    }
    url
}

/// Connect and consume the `sys.authed` greeting.
pub async fn connect(addr: SocketAddr, tenant: &str, sid: Option<&str>) -> Ws {
    let (mut ws, _) = try_connect_url(&ws_url(addr, tenant, "dev", sid))
        .await
        .expect("ws connect");
    match next_event(&mut ws).await {
        Event::Json(v) if v["svc"] == "sys" && v["type"] == "authed" => ws,
        other => panic!("expected sys.authed, got {other:?}"),
    }
}

/// Attempt a WS handshake, bounded by `WAIT` (a hung server fails the test).
pub async fn try_connect_url(
    url: &str,
) -> Result<
    (
        Ws,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    ),
    String,
> {
    match tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(url)).await {
        Err(_) => Err("handshake timed out (server hung?)".into()),
        Ok(r) => r.map_err(|e| e.to_string()),
    }
}

#[derive(Debug)]
pub enum Event {
    Json(Value),
    Binary(Vec<u8>),
    Close(Option<CloseFrame<'static>>),
    /// Stream ended or errored without a Close frame.
    Ended,
    /// Nothing arrived within `WAIT`.
    Timeout,
}

/// Next application-level event (ping/pong skipped).
pub async fn next_event(ws: &mut Ws) -> Event {
    next_event_within(ws, WAIT).await
}

pub async fn next_event_within(ws: &mut Ws, wait: Duration) -> Event {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let msg = match tokio::time::timeout_at(deadline, ws.next()).await {
            Err(_) => return Event::Timeout,
            Ok(None) | Ok(Some(Err(_))) => return Event::Ended,
            Ok(Some(Ok(m))) => m,
        };
        match msg {
            Message::Text(s) => {
                return Event::Json(serde_json::from_str(&s).expect("server sent invalid json"))
            }
            Message::Binary(b) => return Event::Binary(b),
            Message::Close(f) => return Event::Close(f.map(|f| f.into_owned())),
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
        }
    }
}

pub async fn send_json(ws: &mut Ws, v: Value) {
    ws.send(Message::Text(v.to_string()))
        .await
        .expect("send text");
}

pub async fn send_bin(ws: &mut Ws, b: Vec<u8>) {
    ws.send(Message::Binary(b)).await.expect("send binary");
}

/// Hot Lane frame: [v=1][svc_id][opcode][flags=0][payload...]
pub fn hot_frame(svc_id: u8, opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut b = vec![1, svc_id, opcode, 0];
    b.extend_from_slice(payload);
    b
}
