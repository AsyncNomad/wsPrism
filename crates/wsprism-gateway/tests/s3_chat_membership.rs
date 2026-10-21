//! S3: a session may only publish chat to rooms it has joined.

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

#[tokio::test]
async fn non_member_cannot_publish_to_room() {
    let gw = spawn(BASE_YAML).await;
    let mut member = connect(gw.addr, "t", Some("a")).await;
    let mut outsider = connect(gw.addr, "t", Some("b")).await;
    join(&mut member, "r").await;

    chat(&mut outsider, "r", "intrusion").await;

    match next_event(&mut outsider).await {
        Event::Json(v) => {
            assert_eq!(v["type"], "error", "{v}");
            assert_eq!(v["data"]["code"], "NOT_ALLOWED", "{v}");
        }
        other => panic!("expected sys.error for outsider, got {other:?}"),
    }
    match next_event_within(&mut member, Duration::from_millis(300)).await {
        Event::Timeout => {}
        other => panic!("member must not receive outsider's message, got {other:?}"),
    }
}

#[tokio::test]
async fn member_publishes_to_room() {
    let gw = spawn(BASE_YAML).await;
    let mut a = connect(gw.addr, "t", Some("a")).await;
    let mut b = connect(gw.addr, "t", Some("b")).await;
    join(&mut a, "r").await;
    join(&mut b, "r").await;

    chat(&mut a, "r", "hello").await;

    for ws in [&mut a, &mut b] {
        match next_event(ws).await {
            Event::Json(v) => {
                assert_eq!(v["svc"], "chat", "{v}");
                assert_eq!(v["data"]["msg"], "hello", "{v}");
            }
            other => panic!("expected chat.msg, got {other:?}"),
        }
    }
}
