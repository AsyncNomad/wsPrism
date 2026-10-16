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

const HOT_YAML: &str = r#"
version: 1
tenants:
  - id: t
    limits:
      max_frame_bytes: 1024
    policy:
      ext_allowlist: ["room:*"]
      hot_allowlist: ["1:*"]
"#;

/// Join `room`; returns the `sys.joined` message.
async fn join_v(ws: &mut Ws, room: &str, v1: bool) -> serde_json::Value {
    let mut msg = serde_json::json!({"svc":"room","type":"join","room":room});
    if v1 {
        msg["v"] = 1.into();
    }
    ws.send(Message::Text(msg.to_string())).await.expect("send");
    match next_event(ws).await {
        Event::Json(v) if v["type"] == "joined" => v,
        other => panic!("expected sys.joined, got {other:?}"),
    }
}

/// Client v2 Hot frame: [kind=1][flags][chan u32][svc][op][key?][seq?][payload].
fn hot_v2(flags: u8, chan: u32, key: Option<u32>, seq: Option<u32>, payload: &[u8]) -> Vec<u8> {
    let mut b = vec![0x01, flags];
    b.extend_from_slice(&chan.to_le_bytes());
    b.extend_from_slice(&[1, 1]);
    for v in [key, seq].into_iter().flatten() {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b.extend_from_slice(payload);
    b
}

/// Next binary message within `ms`, skipping JSON.
async fn next_binary(ws: &mut Ws, ms: u64) -> Option<Vec<u8>> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(ms);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match next_event_within(ws, left).await {
            Event::Binary(b) => return Some(b),
            Event::Json(_) => continue,
            _ => return None,
        }
    }
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}

#[tokio::test]
async fn v2_hot_frames_route_by_room_id_and_carry_the_sender() {
    let gw = spawn(HOT_YAML).await;
    let (mut a, _) = connect_as(&gw, "a", V2).await;
    let (mut b, _) = connect_as(&gw, "b", V2).await;
    let (mut c, _) = connect_as(&gw, "c", ("sec-websocket-protocol", "wsprism.v1")).await;
    let ja = join_v(&mut a, "r", false).await;
    let jb = join_v(&mut b, "r", false).await;
    join_v(&mut c, "r", true).await;

    // One room id for every member; a distinct member id per connection.
    let chan = ja["chan"].as_u64().expect("v2 joined carries chan") as u32;
    assert_eq!(jb["chan"].as_u64(), Some(chan as u64));
    let (ma, mb) = (
        ja["member_id"].as_u64().unwrap(),
        jb["member_id"].as_u64().unwrap(),
    );
    assert_ne!(ma, mb);

    a.send(Message::Binary(hot_v2(0x03, chan, Some(5), Some(1), b"hi")))
        .await
        .expect("send");

    // v2 recipient: [kind][flags=KEY|SEQ|SENDER][chan][svc][op][key][seq][sender][payload]
    let f = next_binary(&mut b, 2000)
        .await
        .expect("v2 member receives the frame");
    assert_eq!((f[0], f[1]), (0x01, 0x07), "{f:?}");
    assert_eq!(u32_at(&f, 2), chan);
    assert_eq!(&f[6..8], &[1, 1]);
    assert_eq!((u32_at(&f, 8), u32_at(&f, 12)), (5, 1), "key, seq");
    assert_eq!(u32_at(&f, 16) as u64, ma, "sender = publisher's member id");
    assert_eq!(&f[20..], b"hi");

    // v1 recipient in the same room: payload only, as v1 echo always was.
    assert_eq!(next_binary(&mut c, 2000).await.as_deref(), Some(&b"hi"[..]));
}

#[tokio::test]
async fn v2_hot_frame_to_a_room_not_joined_is_dropped() {
    let gw = spawn(HOT_YAML).await;
    let (mut a, _) = connect_as(&gw, "a", V2).await;
    let (mut b, _) = connect_as(&gw, "b", V2).await;
    let chan = join_v(&mut b, "r", false).await["chan"].as_u64().unwrap() as u32;
    // `a` never joined `r`: it must not be able to publish into it.
    a.send(Message::Binary(hot_v2(0, chan, None, None, b"x")))
        .await
        .expect("send");
    assert_eq!(next_binary(&mut b, 400).await, None);
}

#[tokio::test]
async fn v2_client_setting_sender_is_a_decode_error() {
    let gw = spawn(HOT_YAML).await;
    let (mut a, _) = connect_as(&gw, "a", V2).await;
    let mut frame = hot_v2(0x04, 1, None, None, b"");
    frame.splice(8..8, 0u32.to_le_bytes());
    a.send(Message::Binary(frame)).await.expect("send");
    assert_eq!(close_of(&mut a).await.0, 4010);
}

/// T0-3: keyed Hot frames are conflated per recipient. A reader that falls
/// behind receives fewer frames than were sent, and the newest value last.
#[tokio::test]
async fn lagging_reader_gets_the_latest_keyed_state_not_a_backlog() {
    let gw = spawn(
        r#"
version: 1
gateway: { writer_send_timeout_ms: 10000 }
tenants:
  - id: t
    limits:
      max_frame_bytes: 65536
    policy:
      rate_limit_rps: 100000
      rate_limit_burst: 100000
      ext_allowlist: ["room:*"]
      hot_allowlist: ["1:*"]
"#,
    )
    .await;
    let (mut a, _) = connect_as(&gw, "a", V2).await;
    let (mut b, _) = connect_as(&gw, "b", V2).await;
    let chan = join_v(&mut a, "r", false).await["chan"].as_u64().unwrap() as u32;
    join_v(&mut b, "r", false).await;

    // `a` drains its own echoes so only `b` lags.
    let (mut a_tx, mut a_rx) = futures_util::StreamExt::split(a);
    let drain =
        tokio::spawn(
            async move { while futures_util::StreamExt::next(&mut a_rx).await.is_some() {} },
        );

    const SENT: u32 = 300;
    let payload = vec![0u8; 60 * 1024];
    for seq in 1..=SENT {
        a_tx.send(Message::Binary(hot_v2(
            0x03,
            chan,
            Some(0),
            Some(seq),
            &payload,
        )))
        .await
        .expect("send");
    }
    // Let the gateway route everything while `b` is not reading.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let (mut got, mut last_seq) = (0u32, 0u32);
    while let Some(f) = next_binary(&mut b, 1500).await {
        got += 1;
        last_seq = u32_at(&f, 12);
    }
    assert_eq!(last_seq, SENT, "the newest state is delivered last");
    assert!(got < SENT, "conflated: got {got} of {SENT}");
    drain.abort();
}

/// v2-4: with `hot_tick_hz`, a session's Hot slot is sent at most once per
/// tick, so a burst of keyed updates arrives as about one frame per tick,
/// newest last.
#[tokio::test]
async fn hot_tick_paces_keyed_updates_to_one_per_tick() {
    let gw = spawn(
        r#"
version: 1
tenants:
  - id: t
    limits:
      max_frame_bytes: 1024
    policy:
      rate_limit_rps: 100000
      rate_limit_burst: 100000
      ext_allowlist: ["room:*"]
      hot_allowlist: ["1:*"]
      hot_tick_hz: 10
"#,
    )
    .await;
    let (mut a, _) = connect_as(&gw, "a", V2).await;
    let (mut b, _) = connect_as(&gw, "b", V2).await;
    let chan = join_v(&mut a, "r", false).await["chan"].as_u64().unwrap() as u32;
    join_v(&mut b, "r", false).await;
    let (mut a_tx, mut a_rx) = futures_util::StreamExt::split(a);
    let drain =
        tokio::spawn(
            async move { while futures_util::StreamExt::next(&mut a_rx).await.is_some() {} },
        );

    // 50 updates of one key over ~500 ms: 5 ticks at 10 Hz.
    for seq in 1..=50u32 {
        a_tx.send(Message::Binary(hot_v2(
            0x03,
            chan,
            Some(0),
            Some(seq),
            b"s",
        )))
        .await
        .expect("send");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let (mut got, mut last_seq) = (0u32, 0u32);
    while let Some(f) = next_binary(&mut b, 400).await {
        got += 1;
        last_seq = u32_at(&f, 12);
    }
    assert_eq!(last_seq, 50, "the newest value arrives last");
    assert!(got <= 8, "paced to ~1 per 100 ms tick: got {got}");
    drain.abort();
}
