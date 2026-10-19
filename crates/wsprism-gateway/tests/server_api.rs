//! Server API (backend -> gateway, ADR 0008 §3): its own listener, per-tenant
//! API keys stored as SHA-256 hashes, publish / disconnect / presence.

mod common;

use common::*;
use serde_json::json;

/// API key "key-a" (tenant a) and "key-b" (tenant b), stored as hashes.
const KEY_A: &str = "key-a";
const KEY_B: &str = "key-b";

fn yaml() -> String {
    let h = |k: &str| {
        let d = ring::digest::digest(&ring::digest::SHA256, k.as_bytes());
        d.as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    format!(
        r#"
version: 1
gateway:
  api: {{ listen: "127.0.0.1:0" }}
tenants:
  - id: a
    api_keys: [{{ name: backend, sha256: "{}" }}]
    policy: {{ ext_allowlist: ["room:*"], hot_allowlist: ["1:*"] }}
  - id: b
    api_keys: [{{ name: backend, sha256: "{}" }}]
    policy: {{ ext_allowlist: ["room:*"] }}
"#,
        h(KEY_A),
        h(KEY_B)
    )
}

async fn join(ws: &mut Ws, room: &str) {
    send_json(ws, json!({"v":1,"svc":"room","type":"join","room":room})).await;
    match next_event(ws).await {
        Event::Json(v) if v["type"] == "joined" => {}
        other => panic!("expected sys.joined, got {other:?}"),
    }
}

#[tokio::test]
async fn api_requires_a_valid_key() {
    let gw = spawn_with_api(&yaml()).await;
    let body = json!({"room":"r","data":{"x":1}});
    assert_eq!(
        http(gw.api, "POST", "/api/v1/publish", None, Some(body.clone()))
            .await
            .0,
        401
    );
    assert_eq!(
        http(gw.api, "POST", "/api/v1/publish", Some("nope"), Some(body))
            .await
            .0,
        401
    );
}

#[tokio::test]
async fn publish_reaches_the_room_of_the_keys_tenant_only() {
    let gw = spawn_with_api(&yaml()).await;
    let mut ca = connect(gw.addr, "a", Some("ca")).await;
    let mut cb = connect(gw.addr, "b", Some("cb")).await;
    join(&mut ca, "r").await;
    join(&mut cb, "r").await; // same room name, other tenant

    let (status, resp) = http(
        gw.api,
        "POST",
        "/api/v1/publish",
        Some(KEY_A),
        Some(json!({"room":"r","data":{"score":3}})),
    )
    .await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(resp["recipients"], 1);
    match next_event(&mut ca).await {
        Event::Json(v) => {
            assert_eq!(
                (v["svc"].as_str(), v["type"].as_str()),
                (Some("server"), Some("publish")),
                "{v}"
            );
            assert_eq!(v["room"], "r");
            assert_eq!(v["data"]["score"], 3);
        }
        other => panic!("expected server.publish, got {other:?}"),
    }
    assert!(
        matches!(
            next_event_within(&mut cb, std::time::Duration::from_millis(300)).await,
            Event::Timeout
        ),
        "tenant b must not receive tenant a's publish"
    );
}

#[tokio::test]
async fn hot_publish_sends_binary_payload() {
    let gw = spawn_with_api(&yaml()).await;
    let mut ca = connect(gw.addr, "a", Some("ca")).await;
    join(&mut ca, "r").await;
    let (status, _) = http(
        gw.api,
        "POST",
        "/api/v1/publish",
        Some(KEY_A),
        Some(json!({"room":"r","hot":{"svc":1,"op":2,"data_b64":"AQID"}})),
    )
    .await;
    assert_eq!(status, 200);
    match next_event(&mut ca).await {
        Event::Binary(b) => assert_eq!(b, vec![1, 2, 3], "v1 clients get the payload"),
        other => panic!("expected binary, got {other:?}"),
    }
}

#[tokio::test]
async fn disconnect_closes_the_users_sessions_with_a_reason() {
    let gw = spawn_with_api(&yaml()).await;
    let mut ca = connect(gw.addr, "a", Some("ca")).await;
    let (status, resp) = http(
        gw.api,
        "POST",
        "/api/v1/disconnect",
        Some(KEY_A),
        Some(json!({"user":"user:dev","reason":"banned"})),
    )
    .await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(resp["closed"], 1);
    let mut kicked = false;
    loop {
        match next_event(&mut ca).await {
            Event::Json(v) if v["type"] == "kicked" => {
                assert_eq!(v["data"]["reason"], "banned");
                kicked = true;
            }
            Event::Close(Some(f)) => {
                assert!(kicked);
                assert_eq!(
                    (u16::from(f.code), f.reason.as_ref()),
                    (1008, "disconnected")
                );
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[tokio::test]
async fn presence_lists_the_rooms_users() {
    let gw = spawn_with_api(&yaml()).await;
    let mut ca = connect(gw.addr, "a", Some("ca")).await;
    join(&mut ca, "lobby").await;
    let (status, resp) = http(
        gw.api,
        "GET",
        "/api/v1/rooms/lobby/presence",
        Some(KEY_A),
        None,
    )
    .await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(resp["users"], json!(["user:dev"]));
    // Tenant b's key sees tenant b's (empty) room.
    let (_, resp) = http(
        gw.api,
        "GET",
        "/api/v1/rooms/lobby/presence",
        Some(KEY_B),
        None,
    )
    .await;
    assert_eq!(resp["users"], json!([]));
}

/// Metrics name tenants and reveal load: internal listener only, unless
/// `gateway.public_metrics` (review H2).
#[tokio::test]
async fn metrics_are_served_on_the_internal_listener_only() {
    let gw = spawn_with_api(&yaml()).await;
    let (status, body) = http_raw(gw.api, "GET", "/metrics", None, None).await;
    assert_eq!(status, 200);
    assert!(body.contains("wsprism_ws_sessions_active"), "{body}");
    let (status, _) = http_raw(gw.addr, "GET", "/metrics", None, None).await;
    assert_eq!(status, 404, "not on the public port");
    let (status, _) = http_raw(gw.addr, "GET", "/healthz", None, None).await;
    assert_eq!(status, 200, "health stays public for load balancers");

    let public =
        spawn("version: 1\ngateway: { public_metrics: true }\ntenants:\n  - id: t\n").await;
    let (status, _) = http_raw(public.addr, "GET", "/metrics", None, None).await;
    assert_eq!(status, 200);
}
