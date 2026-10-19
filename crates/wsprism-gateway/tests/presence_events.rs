//! Presence events (ADR 0010): members learn who joins and leaves — by any
//! path — and a joiner gets a snapshot of who is there.

mod common;

use common::*;
use serde_json::{json, Value};

const YAML: &str = r#"
version: 1
tenants:
  - id: on
    policy: { ext_allowlist: ["room:*"], presence_events: true }
  - id: off
    policy: { ext_allowlist: ["room:*"] }
"#;

async fn connect_user(gw: &Gateway, tenant: &str, user: &str, sid: Option<&str>) -> Ws {
    let bearer = format!("Bearer dev:{user}");
    let (mut ws, _) = try_connect_with(
        &ws_url(gw.addr, tenant, sid),
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
    next_of(ws, "joined").await
}

/// Next JSON message of `type`, skipping others.
async fn next_of(ws: &mut Ws, ty: &str) -> Value {
    loop {
        match next_event(ws).await {
            Event::Json(v) if v["type"] == ty => return v,
            Event::Json(_) => continue,
            other => panic!("expected {ty}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn members_hear_joins_and_every_kind_of_leave() {
    let gw = spawn(YAML).await;
    let mut a = connect_user(&gw, "on", "alice", None).await;
    let ja = join(&mut a, "lobby").await;
    assert_eq!(ja["members"], json!([]), "alone");

    // Join: a hears it; b's snapshot lists a.
    let mut b = connect_user(&gw, "on", "bob", None).await;
    let jb = join(&mut b, "lobby").await;
    assert_eq!(
        jb["members"],
        json!([{"user": "alice", "member_id": ja["member_id"]}])
    );
    let ev = next_of(&mut a, "member_joined").await;
    assert_eq!(ev["room"], "lobby");
    assert_eq!(
        ev["data"],
        json!({"user": "bob", "member_id": jb["member_id"]})
    );

    // Explicit leave.
    send_json(&mut b, json!({"svc":"room","type":"leave","room":"lobby"})).await;
    let ev = next_of(&mut a, "member_left").await;
    assert_eq!(ev["data"]["member_id"], jb["member_id"]);

    // Disconnect.
    let jb2 = join(&mut b, "lobby").await;
    next_of(&mut a, "member_joined").await;
    drop(b);
    let ev = next_of(&mut a, "member_left").await;
    assert_eq!(ev["data"]["member_id"], jb2["member_id"]);

    // Eviction by a newer session with the same sid.
    let mut c = connect_user(&gw, "on", "carol", Some("tab")).await;
    let jc = join(&mut c, "lobby").await;
    next_of(&mut a, "member_joined").await;
    let _c2 = connect_user(&gw, "on", "carol", Some("tab")).await;
    let ev = next_of(&mut a, "member_left").await;
    assert_eq!(ev["data"]["member_id"], jc["member_id"]);
}

#[tokio::test]
async fn presence_events_are_opt_in() {
    let gw = spawn(YAML).await;
    let mut a = connect_user(&gw, "off", "alice", None).await;
    let ja = join(&mut a, "lobby").await;
    assert!(ja.get("members").is_none(), "{ja}");
    let mut b = connect_user(&gw, "off", "bob", None).await;
    join(&mut b, "lobby").await;
    match next_event_within(&mut a, std::time::Duration::from_millis(300)).await {
        Event::Timeout => {}
        other => panic!("no presence events when off, got {other:?}"),
    }
}
