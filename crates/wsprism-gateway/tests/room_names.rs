//! Room names and room counts are bounded (review H1): a client must not be
//! able to grow presence memory without limit by joining rooms.

mod common;

use common::*;
use serde_json::json;

const YAML: &str = r#"
version: 1
tenants:
  - id: t
    policy:
      ext_allowlist: ["room:*"]
      rate_limit_rps: 10000
      rate_limit_burst: 10000
"#;

async fn join(ws: &mut Ws, room: &str) -> serde_json::Value {
    send_json(ws, json!({"v":1,"svc":"room","type":"join","room":room})).await;
    match next_event(ws).await {
        Event::Json(v) => v,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn oversized_or_control_char_room_names_are_rejected() {
    let gw = spawn(YAML).await;
    let mut ws = connect(gw.addr, "t", None).await;
    for bad in [
        "a".repeat(129),
        "line\nbreak".to_string(),
        "nul\u{0}".to_string(),
    ] {
        let v = join(&mut ws, &bad).await;
        assert_eq!(v["data"]["code"], "BAD_REQUEST", "{v}");
    }
    assert_eq!(join(&mut ws, &"a".repeat(128)).await["type"], "joined");
}

#[tokio::test]
async fn rooms_per_user_default_to_64() {
    let gw = spawn(YAML).await;
    let mut ws = connect(gw.addr, "t", None).await;
    for i in 0..64 {
        let v = join(&mut ws, &format!("r{i}")).await;
        assert_eq!(v["type"], "joined", "room {i}: {v}");
    }
    let v = join(&mut ws, "one-too-many").await;
    assert_eq!(v["data"]["code"], "RESOURCE_EXHAUSTED", "{v}");
}
