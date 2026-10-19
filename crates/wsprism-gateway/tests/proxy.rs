//! Backend proxies (ADR 0008 §6): the connect proxy (a backend decides who
//! connects) and the RPC proxy (a backend answers Ext requests). The
//! backend here is a local axum server; nothing leaves the machine.

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use common::*;
use serde_json::{json, Value};

#[derive(Clone, Default)]
struct Seen {
    /// Authorization header of every backend request.
    auth: Arc<Mutex<Vec<Option<String>>>>,
    /// Bodies of RPC requests.
    rpc: Arc<Mutex<Vec<Value>>>,
}

async fn connect_hook(
    State(seen): State<Seen>,
    headers: HeaderMap,
    Json(b): Json<Value>,
) -> (StatusCode, Json<Value>) {
    seen.auth.lock().unwrap().push(
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
    );
    assert_eq!(b["tenant"], "a");
    assert!(b["client_ip"].is_string());
    let token = b["token"].as_str().unwrap_or_default().to_string();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    match token.as_str() {
        "perm" => (
            StatusCode::OK,
            Json(json!({"user": "pat", "permissions": {"subscribe": ["lobby"], "publish": []}})),
        ),
        "expiring" => (
            StatusCode::OK,
            Json(json!({"user": "eve", "expires_at": now + 2})),
        ),
        "bad-user" => (StatusCode::OK, Json(json!({"user": "a::b"}))),
        "slow" => {
            tokio::time::sleep(Duration::from_secs(3)).await;
            (StatusCode::OK, Json(json!({"user": "late"})))
        }
        "down" => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({}))),
        t if t.starts_with("good:") => (StatusCode::OK, Json(json!({"user": &t[5..]}))),
        _ => (StatusCode::UNAUTHORIZED, Json(json!({}))),
    }
}

async fn rpc_hook(
    State(seen): State<Seen>,
    headers: HeaderMap,
    Json(b): Json<Value>,
) -> (StatusCode, Json<Value>) {
    seen.auth.lock().unwrap().push(
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
    );
    seen.rpc.lock().unwrap().push(b.clone());
    match b["type"].as_str().unwrap_or_default() {
        "echo" => (
            StatusCode::OK,
            Json(json!({"data": {"got": b["data"], "user": b["user"]}})),
        ),
        "fail" => (
            StatusCode::OK,
            Json(json!({"error": {"code": "NOT_FOUND", "msg": "no such item"}})),
        ),
        "hang" => {
            tokio::time::sleep(Duration::from_millis(800)).await;
            (StatusCode::OK, Json(json!({"data": "late"})))
        }
        "slow" => {
            tokio::time::sleep(Duration::from_secs(3)).await;
            (StatusCode::OK, Json(json!({"data": "too late"})))
        }
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"secret": "stack trace"})),
        ),
    }
}

async fn backend() -> (SocketAddr, Seen) {
    let seen = Seen::default();
    let app = Router::new()
        .route("/connect", post(connect_hook))
        .route("/rpc", post(rpc_hook))
        .with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    (addr, seen)
}

fn yaml(backend: SocketAddr) -> String {
    format!(
        r#"
version: 1
tenants:
  - id: a
    auth:
      - {{ kind: proxy, url: "http://{backend}/connect", timeout_ms: 500, secret_env: WSPRISM_TEST_BACKEND_SECRET }}
    rpc:
      url: "http://{backend}/rpc"
      allow: ["game:*"]
      timeout_ms: 400
      max_in_flight: 2
      secret_env: WSPRISM_TEST_BACKEND_SECRET
    policy: {{ ext_allowlist: ["room:*", "chat:*", "game:*"] }}
"#
    )
}

async fn gateway() -> (Gateway, Seen) {
    // Set once for the whole test binary (same value everywhere).
    std::env::set_var("WSPRISM_TEST_BACKEND_SECRET", "s3cret");
    let (addr, seen) = backend().await;
    (spawn(&yaml(addr)).await, seen)
}

async fn connect_token(
    gw: &Gateway,
    token: &str,
    proto: Option<&str>,
) -> Result<(Ws, Value), String> {
    let bearer = format!("Bearer {token}");
    let mut headers = vec![("authorization", bearer.as_str())];
    if let Some(p) = proto {
        headers.push(("sec-websocket-protocol", p));
    }
    let (mut ws, _) = try_connect_with(&ws_url(gw.addr, "a", None), &headers).await?;
    match next_event(&mut ws).await {
        Event::Json(v) if v["type"] == "authed" => Ok((ws, v)),
        other => Err(format!("expected sys.authed, got {other:?}")),
    }
}

fn status_of(r: &Result<(Ws, Value), String>) -> String {
    match r {
        Ok(_) => "admitted".into(),
        Err(e) => e.clone(),
    }
}

#[tokio::test]
async fn connect_proxy_decides_who_connects() {
    let (gw, seen) = gateway().await;
    let (_ws, authed) = connect_token(&gw, "good:alice", None)
        .await
        .expect("admitted");
    assert_eq!(authed["data"]["user"], "alice");
    assert_eq!(
        seen.auth.lock().unwrap()[0].as_deref(),
        Some("Bearer s3cret"),
        "the gateway authenticates itself to the backend"
    );
    for (token, want) in [
        ("nope", "401"),
        ("bad-user", "401"),
        ("down", "503"),
        ("slow", "503"),
    ] {
        let r = connect_token(&gw, token, None).await;
        assert!(
            status_of(&r).contains(want),
            "{token}: want {want}, got {}",
            status_of(&r)
        );
    }
}

#[tokio::test]
async fn connect_proxy_permissions_and_expiry() {
    let (gw, _) = gateway().await;
    let (mut ws, _) = connect_token(&gw, "perm", None).await.expect("admitted");
    send_json(
        &mut ws,
        json!({"v":1,"svc":"room","type":"join","room":"secret"}),
    )
    .await;
    match next_event(&mut ws).await {
        Event::Json(v) => assert_eq!(v["data"]["code"], "NOT_ALLOWED", "{v}"),
        other => panic!("{other:?}"),
    }

    let (mut ws, _) = connect_token(&gw, "expiring", Some("wsprism.v2.json"))
        .await
        .expect("admitted");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    loop {
        match next_event_within(&mut ws, deadline - tokio::time::Instant::now()).await {
            Event::Json(_) => continue,
            Event::Close(Some(f)) => {
                assert_eq!(u16::from(f.code), 4001);
                break;
            }
            other => panic!("expected close 4001, got {other:?}"),
        }
    }
}

/// Next JSON message that answers request `seq`.
async fn reply_for(ws: &mut Ws, seq: u64) -> Value {
    loop {
        match next_event(ws).await {
            Event::Json(v) if v["reply_to"] == seq => return v,
            Event::Json(_) => continue,
            other => panic!("expected reply to {seq}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn rpc_forwards_requests_and_returns_replies() {
    let (gw, seen) = gateway().await;
    let (mut ws, _) = connect_token(&gw, "good:bob", None)
        .await
        .expect("admitted");

    // A room the session hasn't joined is not forwarded (review M1).
    send_json(
        &mut ws,
        json!({"v":1,"svc":"game","type":"echo","seq":6,"room":"r1","data":{"x":1}}),
    )
    .await;
    assert_eq!(reply_for(&mut ws, 6).await["data"]["code"], "NOT_ALLOWED");
    assert!(
        seen.rpc.lock().unwrap().is_empty(),
        "nothing reached the backend"
    );

    send_json(
        &mut ws,
        json!({"v":1,"svc":"room","type":"join","room":"r1","seq":5}),
    )
    .await;
    assert_eq!(reply_for(&mut ws, 5).await["type"], "joined");
    send_json(
        &mut ws,
        json!({"v":1,"svc":"game","type":"echo","seq":7,"room":"r1","data":{"x":1}}),
    )
    .await;
    let v = reply_for(&mut ws, 7).await;
    assert_eq!(v["svc"], "sys");
    assert_eq!(v["type"], "reply");
    assert_eq!(v["data"], json!({"got": {"x": 1}, "user": "bob"}));
    let req = seen.rpc.lock().unwrap()[0].clone();
    assert_eq!(req["tenant"], "a");
    assert_eq!(req["svc"], "game");
    assert_eq!(req["room"], "r1");
    assert_eq!(req["joined"], true, "membership verified by the gateway");
    assert!(req["sid"].is_string());

    send_json(&mut ws, json!({"v":1,"svc":"game","type":"fail","seq":8})).await;
    let v = reply_for(&mut ws, 8).await;
    assert_eq!(v["type"], "error");
    assert_eq!(v["data"]["code"], "NOT_FOUND");
    assert_eq!(v["data"]["msg"], "no such item");

    for (seq, ty) in [(9, "boom"), (10, "slow")] {
        send_json(&mut ws, json!({"v":1,"svc":"game","type":ty,"seq":seq})).await;
        let v = reply_for(&mut ws, seq).await;
        assert_eq!(v["data"]["code"], "UNAVAILABLE", "{ty}: {v}");
        assert!(
            !v.to_string().contains("stack trace"),
            "backend body never leaks"
        );
    }
}

#[tokio::test]
async fn rpc_never_blocks_the_session_and_is_bounded() {
    let (gw, _) = gateway().await;
    let (mut ws, _) = connect_token(&gw, "good:carl", None)
        .await
        .expect("admitted");

    // Two slow calls fill the per-session budget; a third is refused.
    for seq in [1, 2, 3] {
        send_json(&mut ws, json!({"v":1,"svc":"game","type":"hang","seq":seq})).await;
    }
    // A built-in request is answered while the calls are in flight.
    send_json(
        &mut ws,
        json!({"v":1,"svc":"room","type":"join","room":"lobby","seq":4}),
    )
    .await;

    let mut order = Vec::new();
    while order.len() < 4 {
        match next_event(&mut ws).await {
            Event::Json(v) if v["reply_to"].is_u64() => {
                let seq = v["reply_to"].as_u64().unwrap();
                if seq == 3 {
                    assert_eq!(v["data"]["code"], "RATE_LIMITED", "{v}");
                }
                order.push(seq);
            }
            Event::Json(_) => continue,
            other => panic!("{other:?}"),
        }
    }
    let pos = |s| order.iter().position(|&x| x == s).unwrap();
    assert!(
        pos(4) < pos(1) && pos(4) < pos(2),
        "join answered before the slow calls: {order:?}"
    );
    assert!(
        pos(3) < pos(1),
        "the refused call is answered at once: {order:?}"
    );

    // The budget is released when calls finish.
    send_json(&mut ws, json!({"v":1,"svc":"game","type":"echo","seq":5})).await;
    assert_eq!(reply_for(&mut ws, 5).await["type"], "reply");
}

#[tokio::test]
async fn rpc_config_is_validated() {
    let cases = [
        (
            "builtin svc",
            r#"rpc: { url: "http://127.0.0.1:1/rpc", allow: ["chat:*"] }"#,
        ),
        (
            "sys svc",
            r#"rpc: { url: "http://127.0.0.1:1/rpc", allow: ["sys:*"] }"#,
        ),
        (
            "bad rule",
            r#"rpc: { url: "http://127.0.0.1:1/rpc", allow: ["game"] }"#,
        ),
        (
            "bad url",
            r#"rpc: { url: "ftp://x/rpc", allow: ["game:*"] }"#,
        ),
        (
            "zero budget",
            r#"rpc: { url: "http://127.0.0.1:1/rpc", allow: ["game:*"], max_in_flight: 0 }"#,
        ),
    ];
    for (name, rpc) in cases {
        let yaml = format!("version: 1\ntenants:\n  - id: a\n    {rpc}\n");
        let built = wsprism_gateway::config::load_from_str(&yaml)
            .and_then(|c| wsprism_gateway::Gateway::builder().config(c).build());
        assert!(built.is_err(), "{name} must be rejected");
    }
    // An allowlisted rpc service passes the boot-time allowlist check.
    let ok = r#"
version: 1
tenants:
  - id: a
    rpc: { url: "http://127.0.0.1:1/rpc", allow: ["game:*"] }
    policy: { ext_allowlist: ["game:move"] }
"#;
    wsprism_gateway::config::load_from_str(ok)
        .and_then(|c| wsprism_gateway::Gateway::builder().config(c).build())
        .expect("rpc svc counts as registered");
}
