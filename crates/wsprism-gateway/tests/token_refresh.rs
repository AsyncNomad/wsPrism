//! In-band token refresh (ADR 0012 §2): `sys.refresh` replaces the
//! credential without reconnecting; the user cannot change, narrowed
//! permissions apply at once, and authentication never blocks the session.

mod common;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::routing::post;
use axum::{Json, Router};
use common::*;
use jsonwebtoken::{encode, EncodingKey, Header};
use serde_json::{json, Value};

const SECRET: &str = "refresh-test-secret-refresh-test-secret-0123";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn jwt(sub: &str, ttl: u64, subscribe: Option<Value>) -> String {
    let mut claims = json!({"sub": sub, "exp": now() + ttl});
    if let Some(s) = subscribe {
        claims["wsprism"] = json!({"subscribe": s});
    }
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(SECRET.as_bytes()),
    )
    .unwrap()
}

fn secret_file() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wsprism-refresh-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("secret");
    std::fs::write(&p, SECRET).unwrap();
    p
}

/// Connect-proxy backend: `u:<user>` admits at once, `slow:<user>` after 400 ms.
async fn proxy_backend() -> SocketAddr {
    async fn hook(Json(b): Json<Value>) -> (axum::http::StatusCode, Json<Value>) {
        let token = b["token"].as_str().unwrap_or_default().to_string();
        if let Some(user) = token.strip_prefix("slow:") {
            tokio::time::sleep(Duration::from_millis(400)).await;
            return (
                axum::http::StatusCode::OK,
                Json(json!({"user": user, "expires_at": now() + 60})),
            );
        }
        match token.strip_prefix("u:") {
            Some(user) => (
                axum::http::StatusCode::OK,
                Json(json!({"user": user, "expires_at": now() + 60})),
            ),
            None => (axum::http::StatusCode::UNAUTHORIZED, Json(json!({}))),
        }
    }
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(l, Router::new().route("/auth", post(hook))).await;
    });
    addr
}

async fn gateway() -> Gateway {
    let backend = proxy_backend().await;
    spawn(&format!(
        r#"
version: 1
tenants:
  - id: j
    auth: [{{ kind: jwt, algorithm: HS256, secret_file: "{}", leeway_s: 0 }}]
    policy: {{ ext_allowlist: ["room:*"] }}
  - id: p
    auth: [{{ kind: proxy, url: "http://{backend}/auth", timeout_ms: 2000 }}]
    policy: {{ ext_allowlist: ["room:*"] }}
"#,
        secret_file().display()
    ))
    .await
}

async fn connect(gw: &Gateway, tenant: &str, token: &str) -> (Ws, Value) {
    let bearer = format!("Bearer {token}");
    let (mut ws, _) = try_connect_with(
        &ws_url(gw.addr, tenant, None),
        &[
            ("authorization", &bearer),
            ("sec-websocket-protocol", "wsprism.v2.json"),
        ],
    )
    .await
    .expect("connect");
    match next_event(&mut ws).await {
        Event::Json(v) if v["type"] == "authed" => (ws, v),
        other => panic!("{other:?}"),
    }
}

async fn reply_for(ws: &mut Ws, seq: u64) -> Value {
    loop {
        match next_event(ws).await {
            Event::Json(v) if v["reply_to"] == seq && v["type"] != "left" => return v,
            Event::Json(_) => continue,
            other => panic!("expected reply to {seq}, got {other:?}"),
        }
    }
}

fn refresh(token: &str, seq: u64) -> Value {
    json!({"svc":"sys","type":"refresh","seq":seq,"data":{"token":token}})
}

#[tokio::test]
async fn refresh_keeps_the_session_past_the_old_expiry() {
    let gw = gateway().await;
    let (mut ws, authed) = connect(&gw, "j", &jwt("alice", 2, None)).await;
    let left = authed["data"]["expires_in_s"].as_u64().unwrap();
    assert!(left <= 2, "{authed}");

    send_json(&mut ws, refresh(&jwt("alice", 60, None), 1)).await;
    let v = reply_for(&mut ws, 1).await;
    assert_eq!(v["type"], "refreshed", "{v}");
    assert!(v["data"]["expires_in_s"].as_u64().unwrap() >= 58, "{v}");

    // Past the first token's expiry the session is still open.
    tokio::time::sleep(Duration::from_secs(3)).await;
    send_json(
        &mut ws,
        json!({"svc":"room","type":"join","room":"r","seq":2}),
    )
    .await;
    assert_eq!(reply_for(&mut ws, 2).await["type"], "joined");
}

#[tokio::test]
async fn refresh_cannot_change_the_user_and_bad_tokens_keep_the_session() {
    let gw = gateway().await;
    let (mut ws, _) = connect(&gw, "j", &jwt("alice", 60, None)).await;
    send_json(&mut ws, refresh(&jwt("mallory", 60, None), 1)).await;
    assert_eq!(reply_for(&mut ws, 1).await["data"]["code"], "AUTH_FAILED");
    send_json(&mut ws, refresh("not-a-token", 2)).await;
    assert_eq!(reply_for(&mut ws, 2).await["data"]["code"], "AUTH_FAILED");
    send_json(&mut ws, json!({"svc":"sys","type":"refresh","seq":3})).await;
    assert_eq!(reply_for(&mut ws, 3).await["data"]["code"], "BAD_REQUEST");
    // Still connected.
    send_json(
        &mut ws,
        json!({"svc":"room","type":"join","room":"r","seq":4}),
    )
    .await;
    assert_eq!(reply_for(&mut ws, 4).await["type"], "joined");
}

#[tokio::test]
async fn narrowed_permissions_apply_at_once() {
    let gw = gateway().await;
    let (mut ws, _) = connect(&gw, "j", &jwt("alice", 60, Some(json!(["*"])))).await;
    for (i, room) in ["keep", "drop"].iter().enumerate() {
        send_json(
            &mut ws,
            json!({"svc":"room","type":"join","room":room,"seq":i}),
        )
        .await;
        assert_eq!(reply_for(&mut ws, i as u64).await["type"], "joined");
    }
    send_json(
        &mut ws,
        refresh(&jwt("alice", 60, Some(json!(["keep"]))), 9),
    )
    .await;
    let mut left = Vec::new();
    loop {
        match next_event(&mut ws).await {
            Event::Json(v) if v["type"] == "left" => left.push(v["room"].clone()),
            Event::Json(v) if v["type"] == "refreshed" => break,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(left, vec![json!("drop")]);
    send_json(
        &mut ws,
        json!({"svc":"room","type":"join","room":"drop","seq":10}),
    )
    .await;
    assert_eq!(reply_for(&mut ws, 10).await["data"]["code"], "NOT_ALLOWED");
}

#[tokio::test]
async fn slow_authentication_does_not_block_the_session_and_one_refresh_at_a_time() {
    let gw = gateway().await;
    let (mut ws, _) = connect(&gw, "p", "u:bob").await;
    send_json(&mut ws, refresh("slow:bob", 1)).await;
    send_json(&mut ws, refresh("u:bob", 2)).await;
    send_json(
        &mut ws,
        json!({"svc":"room","type":"join","room":"r","seq":3}),
    )
    .await;
    let mut order = Vec::new();
    while order.len() < 3 {
        if let Event::Json(v) = next_event(&mut ws).await {
            if let Some(seq) = v["reply_to"].as_u64() {
                if seq == 2 {
                    assert_eq!(v["data"]["code"], "RATE_LIMITED", "{v}");
                }
                if seq == 1 {
                    assert_eq!(v["type"], "refreshed", "{v}");
                }
                order.push(seq);
            }
        }
    }
    assert_eq!(
        order.last(),
        Some(&1),
        "the slow refresh finishes last: {order:?}"
    );
}
