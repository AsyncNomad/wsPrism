//! Experimental raw transport (ADR 0007 stage 1, `gateway.transport: raw`):
//! the same client-visible behaviour on the gateway-written send path.

mod common;

use common::*;
use futures_util::SinkExt;
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;

const YAML: &str = r#"
version: 1
gateway:
  transport: raw
tenants:
  - id: t
    limits:
      max_frame_bytes: 1024
    policy:
      ext_allowlist: ["room:*", "chat:*"]
      hot_allowlist: ["1:*"]
"#;

async fn join(ws: &mut Ws, room: &str) {
    send_json(ws, json!({"v":1,"svc":"room","type":"join","room":room})).await;
    match next_event(ws).await {
        Event::Json(v) if v["type"] == "joined" => {}
        other => panic!("expected sys.joined, got {other:?}"),
    }
}

#[tokio::test]
async fn chat_and_hot_fan_out_work_on_the_raw_send_path() {
    let gw = spawn(YAML).await;
    let mut a = connect(gw.addr, "t", Some("a")).await;
    let mut b = connect(gw.addr, "t", Some("b")).await;
    join(&mut a, "r").await;
    join(&mut b, "r").await;

    send_json(
        &mut a,
        json!({"v":1,"svc":"chat","type":"send","room":"r","data":{"msg":"hi"}}),
    )
    .await;
    match next_event(&mut b).await {
        Event::Json(v) => assert_eq!(v["data"]["msg"], "hi", "{v}"),
        other => panic!("expected chat, got {other:?}"),
    }

    // Hot (v1): payload fan-out to the active room.
    let payload = vec![7u8; 300]; // > 125 bytes: 16-bit length header
    send_bin(&mut a, hot_frame(1, 1, &payload)).await;
    loop {
        match next_event(&mut b).await {
            Event::Binary(p) => {
                assert_eq!(p, payload);
                break;
            }
            Event::Json(_) => continue,
            other => panic!("expected hot payload, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn server_closes_are_explained_on_the_raw_send_path() {
    let gw = spawn(YAML).await;
    let mut old = connect(gw.addr, "t", Some("tab")).await;
    let _new = connect(gw.addr, "t", Some("tab")).await;
    let mut kicked = false;
    loop {
        match next_event(&mut old).await {
            Event::Json(v) if v["type"] == "kicked" => kicked = true,
            Event::Close(Some(f)) => {
                assert!(kicked, "sys.kicked precedes the close");
                assert_eq!((u16::from(f.code), f.reason.as_ref()), (1008, "replaced"));
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    // Oversized message: 1009 from the size check in the reader.
    let mut c = connect(gw.addr, "t", Some("c")).await;
    c.send(Message::Text("x".repeat(4096))).await.expect("send");
    match next_event(&mut c).await {
        Event::Close(Some(f)) => assert_eq!(u16::from(f.code), 1009),
        other => panic!("expected close 1009, got {other:?}"),
    }
}
