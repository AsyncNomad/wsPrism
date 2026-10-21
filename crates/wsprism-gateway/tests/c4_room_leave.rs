//! C4: `room:leave` must leave the named room, not just the active one.

mod common;

use std::time::Duration;

use common::*;
use serde_json::json;

async fn join(ws: &mut Ws, room: &str) {
    send_json(ws, json!({"v":1,"svc":"room","type":"join","room":room})).await;
    match next_event(ws).await {
        Event::Json(v) if v["type"] == "joined" => {}
        other => panic!("expected sys.joined, got {other:?}"),
    }
}

async fn chat(ws: &mut Ws, room: &str, msg: &str) {
    send_json(
        ws,
        json!({"v":1,"svc":"chat","type":"send","room":room,"data":{"msg":msg}}),
    )
    .await;
}

/// Next chat message seen by `ws`, if any arrives shortly.
async fn next_chat(ws: &mut Ws) -> Option<String> {
    match next_event_within(ws, Duration::from_millis(300)).await {
        Event::Json(v) if v["svc"] == "chat" => Some(v["data"]["msg"].as_str()?.to_string()),
        Event::Timeout => None,
        other => panic!("unexpected event: {other:?}"),
    }
}

#[tokio::test]
async fn leave_named_room_keeps_other_memberships() {
    let gw = spawn(BASE_YAML).await;
    let mut me = connect(gw.addr, "t", Some("me")).await;
    let mut peer = connect(gw.addr, "t", Some("peer")).await;
    for room in ["r1", "r2"] {
        join(&mut me, room).await;
        join(&mut peer, room).await;
    }

    // r2 is active; explicitly leave r1.
    send_json(
        &mut me,
        json!({"v":1,"svc":"room","type":"leave","room":"r1"}),
    )
    .await;
    match next_event(&mut me).await {
        Event::Json(v) => {
            assert_eq!(v["type"], "left", "{v}");
            assert_eq!(v["room"], "r1", "{v}");
        }
        other => panic!("expected sys.left, got {other:?}"),
    }

    chat(&mut peer, "r1", "to-r1").await;
    assert_eq!(next_chat(&mut peer).await.as_deref(), Some("to-r1"));
    assert_eq!(
        next_chat(&mut me).await,
        None,
        "still receiving r1 after leaving it"
    );

    chat(&mut peer, "r2", "to-r2").await;
    assert_eq!(next_chat(&mut peer).await.as_deref(), Some("to-r2"));
    assert_eq!(
        next_chat(&mut me).await.as_deref(),
        Some("to-r2"),
        "lost r2"
    );
}

#[tokio::test]
async fn leaving_frees_a_per_user_room_slot() {
    let gw = spawn(
        r#"
version: 1
tenants:
  - id: t
    limits: { max_rooms_per_user: 2 }
    policy: { ext_allowlist: ["room:*"] }
"#,
    )
    .await;
    let mut me = connect(gw.addr, "t", None).await;
    join(&mut me, "r1").await;
    join(&mut me, "r2").await;

    send_json(
        &mut me,
        json!({"v":1,"svc":"room","type":"leave","room":"r1"}),
    )
    .await;
    assert!(matches!(next_event(&mut me).await, Event::Json(v) if v["type"] == "left"));

    join(&mut me, "r3").await; // the freed slot is usable
                               // Still a member of r2, so re-joining it doesn't need a free slot. (If the
                               // leave had dropped the active room r2 instead of r1, this would fail with
                               // "user room limit reached".)
    join(&mut me, "r2").await;
}

#[tokio::test]
async fn leave_without_room_leaves_active_room() {
    let gw = spawn(BASE_YAML).await;
    let mut me = connect(gw.addr, "t", None).await;
    join(&mut me, "r1").await;

    send_json(&mut me, json!({"v":1,"svc":"room","type":"leave"})).await;
    match next_event(&mut me).await {
        Event::Json(v) => assert_eq!(v["room"], "r1", "{v}"),
        other => panic!("expected sys.left, got {other:?}"),
    }
}

#[tokio::test]
async fn leaving_a_room_not_joined_is_an_error() {
    let gw = spawn(BASE_YAML).await;
    let mut me = connect(gw.addr, "t", None).await;

    send_json(
        &mut me,
        json!({"v":1,"svc":"room","type":"leave","room":"nope"}),
    )
    .await;
    match next_event(&mut me).await {
        Event::Json(v) => assert_eq!(v["type"], "error", "{v}"),
        other => panic!("expected sys.error, got {other:?}"),
    }
}
