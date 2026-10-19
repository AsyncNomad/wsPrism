//! ADR 0011 B3: every gateway path that reaches a room goes through the
//! broker — chat, Hot relay, server API publish (Ext and Hot), and presence
//! events from joins, leaves, closes, sid replacement and API disconnects.
//! A queued recording broker sees each one. (`WSPRISM_TEST_BROKER=queued`
//! additionally runs the whole suite through such a broker.)

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use serde_json::{json, Value};
use wsprism_engine::broker::testing::TestBroker;
use wsprism_engine::broker::OutboxLimits;
use wsprism_engine::spi::OutFrame;

const KEY: &str = "key-a";

fn yaml() -> String {
    let d = ring::digest::digest(&ring::digest::SHA256, KEY.as_bytes());
    let hash: String = d.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    format!(
        r#"
version: 1
gateway:
  api: {{ listen: "127.0.0.1:0" }}
tenants:
  - id: t
    api_keys: [{{ name: backend, sha256: "{hash}" }}]
    policy: {{ ext_allowlist: ["room:*", "chat:*"], hot_allowlist: ["1:*"], presence_events: true }}
"#
    )
}

async fn gateway() -> (ApiGateway, Arc<TestBroker>) {
    let broker = TestBroker::new(0);
    let cfg = wsprism_gateway::config::load_from_str(&yaml()).unwrap();
    let built = builder()
        .config(cfg)
        .broker(broker.clone(), OutboxLimits::default())
        .build()
        .unwrap();
    (spawn_built_with_api(built).await, broker)
}

async fn user(gw: &ApiGateway, name: &str, sid: Option<&str>) -> Ws {
    let bearer = format!("Bearer dev:{name}");
    let (mut ws, _) = try_connect_with(
        &ws_url(gw.addr, "t", sid),
        &[
            ("authorization", &bearer),
            ("sec-websocket-protocol", "wsprism.v2.json"),
        ],
    )
    .await
    .expect("connect");
    match next_event(&mut ws).await {
        Event::Json(v) if v["type"] == "authed" => ws,
        other => panic!("{other:?}"),
    }
}

async fn join(ws: &mut Ws, room: &str) -> Value {
    send_json(ws, json!({"svc":"room","type":"join","room":room})).await;
    loop {
        match next_event(ws).await {
            Event::Json(v) if v["type"] == "joined" => return v,
            Event::Json(_) | Event::Binary(_) => continue, // others' traffic
            other => panic!("{other:?}"),
        }
    }
}

fn kind(f: wsprism_engine::broker::BrokerFrame) -> String {
    assert_eq!(&*f.room_key, "t::r");
    match f.body {
        OutFrame::Text(t) => {
            let v: Value = serde_json::from_slice(t.as_bytes()).unwrap();
            v["type"].as_str().unwrap().to_string()
        }
        OutFrame::Hot(_) => "<hot>".into(),
        other => panic!("{other:?}"),
    }
}

/// The broker records exactly `want` (JSON `type`s, `<hot>` for Hot) from
/// here on: waits up to 5 s for them, then 200 ms for anything extra.
async fn expect(b: &TestBroker, want: &[&str]) {
    let mut got = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while got.len() < want.len() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
        got.extend(b.take_published().into_iter().map(kind));
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    got.extend(b.take_published().into_iter().map(kind));
    assert_eq!(got, want);
}

#[tokio::test]
async fn client_paths_go_through_the_broker() {
    let (gw, broker) = gateway().await;
    let mut a = user(&gw, "alice", None).await;
    let chan = join(&mut a, "r").await["chan"].as_u64().unwrap() as u32;
    let mut b = user(&gw, "bob", None).await;
    join(&mut b, "r").await;
    // alice's too: other nodes' members must hear of it.
    expect(&broker, &["member_joined", "member_joined"]).await;

    send_json(
        &mut b,
        json!({"svc":"chat","type":"send","room":"r","data":{"msg":"hi"}}),
    )
    .await;
    expect(&broker, &["msg"]).await;

    // Hot relay (v2 frame: kind, flags, chan, svc, op, payload).
    let mut frame = vec![0x01, 0x00];
    frame.extend_from_slice(&chan.to_le_bytes());
    frame.extend_from_slice(&[1, 1, b'x']);
    send_bin(&mut b, frame).await;
    expect(&broker, &["<hot>"]).await;

    send_json(&mut b, json!({"svc":"room","type":"leave","room":"r"})).await;
    expect(&broker, &["member_left"]).await;
    join(&mut b, "r").await;
    expect(&broker, &["member_joined"]).await;
    drop(b); // connection closes
    expect(&broker, &["member_left"]).await;

    // Replaced by a new connection with the same sid.
    let mut old = user(&gw, "carol", Some("tab")).await;
    join(&mut old, "r").await;
    expect(&broker, &["member_joined"]).await;
    let _new = user(&gw, "carol", Some("tab")).await;
    expect(&broker, &["member_left"]).await;
}

#[tokio::test]
async fn server_api_paths_go_through_the_broker() {
    let (gw, broker) = gateway().await;
    let mut a = user(&gw, "alice", None).await;
    join(&mut a, "r").await;
    let mut b = user(&gw, "bob", None).await;
    join(&mut b, "r").await;
    expect(&broker, &["member_joined", "member_joined"]).await;

    let (status, body) = http(
        gw.api,
        "POST",
        "/api/v1/publish",
        Some(KEY),
        Some(json!({"room":"r","data":{"x":1}})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    expect(&broker, &["publish"]).await;

    let (status, body) = http(
        gw.api,
        "POST",
        "/api/v1/publish",
        Some(KEY),
        Some(json!({"room":"r","hot":{"svc":1,"op":1,"data_b64":"eA=="}})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    expect(&broker, &["<hot>"]).await;

    let (status, body) = http(
        gw.api,
        "POST",
        "/api/v1/disconnect",
        Some(KEY),
        Some(json!({"user":"bob"})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    expect(&broker, &["member_left"]).await;
}

/// A full outbox answers the server API with 503 + Retry-After.
#[tokio::test]
async fn a_full_outbox_is_503_on_the_server_api() {
    let broker = TestBroker::new(0);
    let cfg = wsprism_gateway::config::load_from_str(&yaml()).unwrap();
    let built = builder()
        .config(cfg)
        .broker(broker.clone(), OutboxLimits::new(2, 2))
        .build()
        .unwrap();
    let gw = spawn_built_with_api(built).await;
    let mut a = user(&gw, "alice", None).await;
    join(&mut a, "r").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    broker.pause();
    let mut statuses = Vec::new();
    for _ in 0..6 {
        let (status, _) = http(
            gw.api,
            "POST",
            "/api/v1/publish",
            Some(KEY),
            Some(json!({"room":"r","data":{"x":1}})),
        )
        .await;
        statuses.push(status);
    }
    assert!(statuses.contains(&503), "{statuses:?}");
    broker.resume();
}
