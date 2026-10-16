//! Protocol v2 negotiation and v2 close codes (ADR 0006 §1, §6; step v2-1).
//! v1 behaviour is unchanged and covered by the existing tests.

mod common;

use common::*;
use futures_util::SinkExt;
use tokio_tungstenite::tungstenite::Message;

const YAML: &str = r#"
version: 1
tenants:
  - id: t
    limits:
      max_frame_bytes: 1024
    policy:
      ext_allowlist: ["room:*"]
"#;

const V2: (&str, &str) = ("sec-websocket-protocol", "wsprism.v2.json, wsprism.v1");

async fn connect_as(gw: &Gateway, sid: &str, proto: (&str, &str)) -> (Ws, Option<String>) {
    let (mut ws, resp) = try_connect_with(&ws_url(gw.addr, "t", Some(sid)), &[AUTH, proto])
        .await
        .expect("connect");
    let chosen = resp
        .headers()
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    match next_event(&mut ws).await {
        Event::Json(v) if v["type"] == "authed" => {}
        other => panic!("expected sys.authed, got {other:?}"),
    }
    (ws, chosen)
}

/// Skip notices; return the close frame's (code, reason).
async fn close_of(ws: &mut Ws) -> (u16, String) {
    loop {
        match next_event(ws).await {
            Event::Json(_) => continue,
            Event::Close(Some(f)) => return (u16::from(f.code), f.reason.to_string()),
            other => panic!("expected a close frame, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn server_prefers_v2_when_offered() {
    let gw = spawn(YAML).await;
    let (_, chosen) = connect_as(&gw, "a", V2).await;
    assert_eq!(chosen.as_deref(), Some("wsprism.v2.json"));
    let (_, chosen) = connect_as(&gw, "b", ("sec-websocket-protocol", "wsprism.v1")).await;
    assert_eq!(chosen.as_deref(), Some("wsprism.v1"));
}

#[tokio::test]
async fn replaced_v2_session_closes_with_4005() {
    let gw = spawn(YAML).await;
    let (mut old, _) = connect_as(&gw, "tab", V2).await;
    let _new = connect_as(&gw, "tab", V2).await;
    assert_eq!(close_of(&mut old).await, (4005, "replaced".to_string()));
}

#[tokio::test]
async fn v2_decode_error_closes_with_4010() {
    let gw = spawn(YAML).await;
    let (mut ws, _) = connect_as(&gw, "a", V2).await;
    ws.send(Message::Text("not json".into()))
        .await
        .expect("send");
    assert_eq!(close_of(&mut ws).await.0, 4010);
}

#[tokio::test]
async fn v2_oversized_message_closes_with_4009() {
    let gw = spawn(YAML).await;
    let (mut ws, _) = connect_as(&gw, "a", V2).await;
    ws.send(Message::Text("x".repeat(4096)))
        .await
        .expect("send");
    assert_eq!(close_of(&mut ws).await.0, 4009);
}

#[tokio::test]
async fn v1_keeps_standard_codes() {
    let gw = spawn(YAML).await;
    let (mut ws, _) = connect_as(&gw, "a", ("sec-websocket-protocol", "wsprism.v1")).await;
    ws.send(Message::Text("not json".into()))
        .await
        .expect("send");
    assert_eq!(close_of(&mut ws).await.0, 1002);
}

/// v2 control messages ignore unknown fields and need no `v` (ADR 0006 §5,
/// R6); v1 keeps rejecting them.
#[tokio::test]
async fn v2_envelope_ignores_unknown_fields_and_needs_no_version() {
    let gw = spawn(YAML).await;
    let (mut ws, _) = connect_as(&gw, "a", V2).await;
    let join = serde_json::json!({"svc":"room","type":"join","room":"r","future_field":{"x":1}});
    ws.send(Message::Text(join.to_string()))
        .await
        .expect("send");
    match next_event(&mut ws).await {
        Event::Json(v) => assert_eq!(v["type"], "joined", "{v}"),
        other => panic!("expected sys.joined, got {other:?}"),
    }

    let (mut v1, _) = connect_as(&gw, "b", ("sec-websocket-protocol", "wsprism.v1")).await;
    let join = serde_json::json!({"v":1,"svc":"room","type":"join","room":"r","future_field":1});
    v1.send(Message::Text(join.to_string()))
        .await
        .expect("send");
    assert_eq!(close_of(&mut v1).await.0, 1002, "v1 stays strict");
}
