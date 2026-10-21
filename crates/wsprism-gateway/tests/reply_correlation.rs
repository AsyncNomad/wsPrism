//! Responses carry `reply_to` = the request's `seq`, so clients can match
//! answers to pipelined requests.

mod common;

use common::*;
use serde_json::{json, Value};

const YAML: &str = r#"
version: 1
tenants:
  - id: t
    policy:
      ext_allowlist: ["room:*", "chat:*"]
      hot_allowlist: ["1:*"]
      hot_requires_active_room: true   # hot frame before joining -> sys.error
"#;

async fn next_json(ws: &mut Ws) -> Value {
    match next_event(ws).await {
        Event::Json(v) => v,
        other => panic!("expected a JSON reply, got {other:?}"),
    }
}

#[tokio::test]
async fn success_reply_echoes_seq() {
    let gw = spawn(YAML).await;
    let mut ws = connect(gw.addr, "t", None).await;
    send_json(
        &mut ws,
        json!({"v":1,"svc":"room","type":"join","room":"r","seq":7}),
    )
    .await;
    let v = next_json(&mut ws).await;
    assert_eq!(v["type"], "joined", "{v}");
    assert_eq!(v["reply_to"], 7, "{v}");
}

#[tokio::test]
async fn replies_without_request_seq_have_no_reply_to() {
    let gw = spawn(YAML).await;
    let mut ws = connect(gw.addr, "t", None).await;
    send_json(
        &mut ws,
        json!({"v":1,"svc":"room","type":"join","room":"r"}),
    )
    .await;
    let v = next_json(&mut ws).await;
    assert!(v.get("reply_to").is_none(), "{v}");
}

#[tokio::test]
async fn policy_and_service_errors_echo_seq() {
    let gw = spawn(YAML).await;
    let mut ws = connect(gw.addr, "t", None).await;

    // Not in the allowlist -> policy reject.
    send_json(&mut ws, json!({"v":1,"svc":"admin","type":"x","seq":11})).await;
    let v = next_json(&mut ws).await;
    assert_eq!(
        (v["type"].as_str(), v["reply_to"].as_u64()),
        (Some("error"), Some(11)),
        "{v}"
    );
    // Valid request the tenant doesn't permit: NOT_ALLOWED, not BAD_REQUEST.
    assert_eq!(v["data"]["code"], "NOT_ALLOWED", "{v}");

    // Allowed, but the service fails (not a member of the room).
    send_json(
        &mut ws,
        json!({"v":1,"svc":"chat","type":"send","room":"r","seq":12,"data":{"msg":"x"}}),
    )
    .await;
    let v = next_json(&mut ws).await;
    assert_eq!(v["data"]["code"], "NOT_ALLOWED", "{v}");
    assert_eq!(v["reply_to"], 12, "{v}");
}

#[tokio::test]
async fn hot_lane_seq_is_echoed_on_errors() {
    let gw = spawn(YAML).await;
    let mut ws = connect(gw.addr, "t", None).await;
    // [v=1][svc=1][op=1][flags=SEQ][seq=5 LE][payload]
    let mut frame = vec![1, 1, 1, 0x01];
    frame.extend_from_slice(&5u32.to_le_bytes());
    frame.push(0xAA);
    send_bin(&mut ws, frame).await;
    let v = next_json(&mut ws).await;
    assert_eq!(v["type"], "error", "{v}");
    assert_eq!(v["reply_to"], 5, "{v}");
}
