//! Integration-test harness: boots a real gateway on an ephemeral port and
//! drives it with a real WebSocket client.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Response as HandshakeResponse;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use wsprism_gateway::__internal::AppState;
use wsprism_gateway::{config, Gateway as Built};

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

/// The builder every test gateway starts from. With
/// `WSPRISM_TEST_BROKER=queued`, room traffic goes through a queued loopback
/// broker (ADR 0011 B3), so the whole suite checks that queued mode behaves
/// like a single node.
pub fn builder() -> wsprism_gateway::GatewayBuilder {
    let b = Built::builder();
    match std::env::var("WSPRISM_TEST_BROKER").as_deref() {
        Ok("queued") => b.broker(
            wsprism_engine::broker::testing::TestBroker::new(0),
            wsprism_engine::broker::OutboxLimits::default(),
        ),
        _ => b,
    }
}

/// Boot a gateway from YAML on 127.0.0.1:0.
///
/// The server runs on its own OS thread and multi-thread runtime, isolated
/// from the test's (client) runtime. If the server deadlocks or blocks a
/// worker, the client side still times out and the test *fails* instead of
/// hanging forever.
pub async fn spawn(yaml: &str) -> Gateway {
    let cfg = config::load_from_str(yaml).expect("test config must parse");
    spawn_built(builder().config(cfg).build().expect("gateway")).await
}

/// Serve an already built gateway.
pub async fn spawn_built(built: Built) -> Gateway {
    let state = built.__state().clone();
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
            built
                .serve_on(listener, None, std::future::pending())
                .await
                .expect("serve");
        });
    });
    Gateway { addr, state }
}

/// A gateway with the server API (ADR 0008 §3) on its own ephemeral port.
pub struct ApiGateway {
    pub addr: SocketAddr,
    pub api: SocketAddr,
    pub state: AppState,
}

/// Boot a gateway from YAML with the server API listener enabled.
pub async fn spawn_with_api(yaml: &str) -> ApiGateway {
    let cfg = config::load_from_str(yaml).expect("test config must parse");
    spawn_built_with_api(builder().config(cfg).build().expect("gateway")).await
}

/// Serve an already built gateway with the server API listener enabled.
pub async fn spawn_built_with_api(built: Built) -> ApiGateway {
    let state = built.__state().clone();
    let ws = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ws");
    let api = std::net::TcpListener::bind("127.0.0.1:0").expect("bind api");
    ws.set_nonblocking(true).expect("nonblocking");
    api.set_nonblocking(true).expect("nonblocking");
    let (addr, api_addr) = (
        ws.local_addr().expect("ws addr"),
        api.local_addr().expect("api addr"),
    );
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("server runtime");
        rt.block_on(async move {
            let ws = tokio::net::TcpListener::from_std(ws).expect("ws listener");
            let api = tokio::net::TcpListener::from_std(api).expect("api listener");
            built
                .serve_on(ws, Some(api), std::future::pending())
                .await
                .expect("serve");
        });
    });
    ApiGateway {
        addr,
        api: api_addr,
        state,
    }
}

/// Minimal HTTP/1.1 request (no client crate offline): returns the status
/// code and the body parsed as JSON (Null if empty or not JSON).
pub async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (u16, Value) {
    let (status, body) = http_raw(addr, method, path, bearer, body).await;
    // Bodies are small; chunked encoding is not used for them by axum here,
    // but tolerate it by finding the JSON object.
    let json = body
        .find('{')
        .and_then(|i| body[i..].rfind('}').map(|j| &body[i..i + j + 1]))
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or(Value::Null);
    (status, json)
}

/// Status and raw body of an HTTP/1.1 request.
pub async fn http_raw(
    addr: SocketAddr,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut tcp = TcpStream::connect(addr).await.expect("api connect");
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let auth = bearer
        .map(|k| format!("Authorization: Bearer {k}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: test\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    tcp.write_all(req.as_bytes()).await.expect("api write");
    let mut raw = Vec::new();
    tokio::time::timeout(WAIT, tcp.read_to_end(&mut raw))
        .await
        .expect("api response in time")
        .expect("api read");
    let text = String::from_utf8_lossy(&raw);
    let status = text
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("status code");
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, body.to_string())
}

pub fn ws_url(addr: SocketAddr, tenant: &str, sid: Option<&str>) -> String {
    let mut url = format!("ws://{addr}/v1/ws?tenant={tenant}");
    if let Some(sid) = sid {
        url.push_str(&format!("&sid={sid}"));
    }
    url
}

/// Default credentials: the dev ticket as a bearer token.
pub const AUTH: (&str, &str) = ("authorization", "Bearer dev");

/// Connect and consume the `sys.authed` greeting.
pub async fn connect(addr: SocketAddr, tenant: &str, sid: Option<&str>) -> Ws {
    let (mut ws, _) = try_connect_url(&ws_url(addr, tenant, sid))
        .await
        .expect("ws connect");
    match next_event(&mut ws).await {
        Event::Json(v) if v["svc"] == "sys" && v["type"] == "authed" => ws,
        other => panic!("expected sys.authed, got {other:?}"),
    }
}

/// Attempt a WS handshake with the default credentials, bounded by `WAIT`
/// (a hung server fails the test).
pub async fn try_connect_url(url: &str) -> Result<(Ws, HandshakeResponse), String> {
    try_connect_with(url, &[AUTH]).await
}

/// Attempt a WS handshake sending exactly `headers` (add `AUTH` to log in).
pub async fn try_connect_with(
    url: &str,
    headers: &[(&str, &str)],
) -> Result<(Ws, HandshakeResponse), String> {
    let mut req = url.into_client_request().map_err(|e| e.to_string())?;
    for (name, value) in headers {
        let value = value
            .parse()
            .map_err(|_| format!("bad header value {value}"))?;
        let name: axum::http::HeaderName = name
            .parse()
            .map_err(|_| format!("bad header name {name}"))?;
        req.headers_mut().append(name, value);
    }
    match tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(req)).await {
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
