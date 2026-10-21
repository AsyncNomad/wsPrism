//! C1: a reliable publish must never block the publisher on a slow
//! recipient. A recipient that can't keep up is disconnected (slow consumer)
//! instead of stalling the room or silently missing messages.

mod common;

use std::time::{Duration, Instant};

use common::*;
use serde_json::json;

/// Big frames + small queues so a non-reading member saturates quickly.
const YAML: &str = r#"
version: 1
gateway:
  session_queue_capacity: 8
tenants:
  - id: t
    limits:
      max_frame_bytes: 65536
    policy:
      rate_limit_rps: 10000
      rate_limit_burst: 10000
      ext_allowlist: ["room:*", "chat:*"]
"#;

async fn join(ws: &mut Ws, room: &str) {
    send_json(ws, json!({"v":1,"svc":"room","type":"join","room":room})).await;
    match next_event(ws).await {
        Event::Json(v) if v["type"] == "joined" => {}
        other => panic!("expected sys.joined, got {other:?}"),
    }
}

/// Reads one event every 50ms: slow but steady, so the server's writer keeps
/// making progress and its write timeout never fires. Returns true once the
/// server disconnects it.
async fn slow_steady_reader(mut ws: Ws) -> bool {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
        match next_event_within(&mut ws, Duration::from_millis(500)).await {
            Event::Json(v) if v["svc"] == "chat" => {}
            Event::Json(v) => assert_eq!(v["data"]["code"], "SLOW_CONSUMER", "{v}"),
            Event::Close(_) | Event::Ended => return true,
            Event::Timeout => {}
            other => panic!("unexpected event: {other:?}"),
        }
    }
    false
}

#[tokio::test]
async fn slow_member_neither_blocks_publisher_nor_misses_silently() {
    let gw = spawn(YAML).await;
    let mut slow = connect(gw.addr, "t", Some("slow")).await;
    let mut sender = connect(gw.addr, "t", Some("sender")).await;
    join(&mut slow, "r").await;
    join(&mut sender, "r").await;
    let slow = tokio::spawn(slow_steady_reader(slow));

    // ~24 MB of chat: far more than the slow member's socket buffers + queue.
    let body = "x".repeat(60_000);
    let started = Instant::now();
    for i in 0..400 {
        send_json(
            &mut sender,
            json!({"v":1,"svc":"chat","type":"send","room":"r","data":{"msg":body}}),
        )
        .await;
        // The sender is a member too: its own echo must keep flowing.
        match next_event(&mut sender).await {
            Event::Json(v) if v["svc"] == "chat" => {}
            other => panic!("sender stalled at message {i}: {other:?}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "publisher throttled to the slow member's pace (at message {i})"
        );
    }

    // The slow member was cut off rather than left with a silent gap.
    assert!(
        slow.await.expect("reader task"),
        "slow member was never disconnected"
    );
}
