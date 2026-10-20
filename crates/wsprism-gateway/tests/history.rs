//! Room history and recovery on one node (ADR 0011 §3, R2): messages of a
//! history room carry an offset; a client that comes back resumes from its
//! last offset and gets what it missed, in order, before anything newer —
//! or `recovered: false` when that is no longer possible.

mod common;

use std::time::Duration;

use common::*;
use serde_json::{json, Value};

const KEY: &str = "key-h";

fn yaml(queue: usize, size: usize) -> String {
    let d = ring::digest::digest(&ring::digest::SHA256, KEY.as_bytes());
    let hash: String = d.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    format!(
        r#"
version: 1
gateway:
  session_queue_capacity: {queue}
  api: {{ listen: "127.0.0.1:0" }}
tenants:
  - id: t
    api_keys: [{{ name: backend, sha256: "{hash}" }}]
    policy: {{ ext_allowlist: ["room:*", "chat:*"], presence_events: true }}
    history:
      - {{ rooms: "chat:*", size: {size}, ttl_s: 600 }}
"#
    )
}

/// History needs the single-node broker for now (ADR 0011 R3), so these
/// tests ignore `WSPRISM_TEST_BROKER`.
async fn gateway(queue: usize, size: usize) -> ApiGateway {
    let cfg = wsprism_gateway::config::load_from_str(&yaml(queue, size)).unwrap();
    spawn_built_with_api(
        wsprism_gateway::Gateway::builder()
            .config(cfg)
            .build()
            .unwrap(),
    )
    .await
}

async fn user(gw: &ApiGateway, name: &str) -> Ws {
    let bearer = format!("Bearer dev:{name}");
    let (mut ws, _) = try_connect_with(
        &ws_url(gw.addr, "t", None),
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

async fn join(ws: &mut Ws, room: &str, recover: Option<Value>) -> Value {
    let mut msg = json!({"svc":"room","type":"join","room":room,"seq":1});
    if let Some(r) = recover {
        msg["data"] = json!({ "recover": r });
    }
    send_json(ws, msg).await;
    next_of(ws, "joined").await
}

async fn next_of(ws: &mut Ws, ty: &str) -> Value {
    loop {
        match next_event(ws).await {
            Event::Json(v) if v["type"] == ty => return v,
            Event::Json(_) => continue,
            other => panic!("expected {ty}, got {other:?}"),
        }
    }
}

async fn say(ws: &mut Ws, room: &str, msg: &str) {
    send_json(
        ws,
        json!({"svc":"chat","type":"send","room":room,"data":{"msg":msg}}),
    )
    .await;
}

/// The next `n` chat messages: (offset, text).
async fn chats(ws: &mut Ws, n: usize) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    while out.len() < n {
        let v = next_of(ws, "msg").await;
        out.push((
            v["offset"].as_u64().expect("offset"),
            v["data"]["msg"].as_str().unwrap().to_string(),
        ));
    }
    out
}

/// Nothing more within a short wait (presence events aside).
async fn no_more_chats(ws: &mut Ws) {
    loop {
        match next_event_within(ws, Duration::from_millis(300)).await {
            Event::Timeout => return,
            Event::Json(v) if v["type"] != "msg" => continue,
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[tokio::test]
async fn messages_carry_offsets_and_joined_carries_the_position() {
    let gw = gateway(1024, 100).await;
    let mut a = user(&gw, "alice").await;
    let j = join(&mut a, "chat:1", None).await;
    let epoch = j["epoch"].as_str().expect("epoch").to_string();
    assert_eq!(j["offset"], 0);
    assert!(j.get("recovered").is_none(), "no recovery asked: {j}");
    say(&mut a, "chat:1", "one").await;
    say(&mut a, "chat:1", "two").await;
    assert_eq!(
        chats(&mut a, 2).await,
        [(1, "one".into()), (2, "two".into())]
    );

    let mut b = user(&gw, "bob").await;
    let j = join(&mut b, "chat:1", None).await;
    assert_eq!(
        (j["epoch"].as_str().unwrap(), j["offset"].as_u64()),
        (&*epoch, Some(2))
    );

    // A room without history: no position.
    let j = join(&mut b, "lobby", None).await;
    assert!(j.get("epoch").is_none(), "{j}");
}

#[tokio::test]
async fn a_returning_client_gets_what_it_missed_in_order() {
    let gw = gateway(1024, 100).await;
    let mut a = user(&gw, "alice").await;
    let j = join(&mut a, "chat:1", None).await;
    let epoch = j["epoch"].clone();
    let mut b = user(&gw, "bob").await;
    join(&mut b, "chat:1", None).await;
    say(&mut b, "chat:1", "before").await;
    let seen = chats(&mut a, 1).await;
    assert_eq!(seen, [(1, "before".into())]);

    drop(a); // connection lost
    for m in ["m2", "m3", "m4"] {
        say(&mut b, "chat:1", m).await;
    }
    // A server publish while away (no `room` of its own? it has one).
    let (status, _) = http(
        gw.api,
        "POST",
        "/api/v1/publish",
        Some(KEY),
        Some(json!({"room":"chat:1","data":{"msg":"m5"}})),
    )
    .await;
    assert_eq!(status, 200);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut a = user(&gw, "alice").await;
    let j = join(&mut a, "chat:1", Some(json!({"epoch": epoch, "offset": 1}))).await;
    assert_eq!(j["recovered"], true, "{j}");
    assert_eq!(j["offset"], 5);
    let mut got = Vec::new();
    for _ in 0..4 {
        let v = next_of(&mut a, if got.len() == 3 { "publish" } else { "msg" }).await;
        got.push(v["offset"].as_u64().unwrap());
    }
    assert_eq!(got, [2, 3, 4, 5]);
    // Then live.
    say(&mut b, "chat:1", "live").await;
    assert_eq!(chats(&mut a, 1).await, [(6, "live".into())]);
    no_more_chats(&mut a).await;
}

#[tokio::test]
async fn recovery_is_refused_when_it_cannot_be_exact() {
    let gw = gateway(1024, 3).await;
    let mut b = user(&gw, "bob").await;
    let j = join(&mut b, "chat:1", None).await;
    let epoch = j["epoch"].clone();
    for m in ["1", "2", "3", "4", "5"] {
        say(&mut b, "chat:1", m).await;
    }
    chats(&mut b, 5).await;

    let mut a = user(&gw, "alice").await;
    // Offset 1 was dropped (size 3 keeps 3..5): a gap, so no recovery.
    let j = join(&mut a, "chat:1", Some(json!({"epoch": epoch, "offset": 1}))).await;
    assert_eq!(j["recovered"], false, "{j}");
    no_more_chats(&mut a).await;
    // Another epoch.
    let j = join(
        &mut a,
        "chat:2",
        Some(json!({"epoch": "nope", "offset": 0})),
    )
    .await;
    assert_eq!(j["recovered"], false, "{j}");
    // Ahead of the room.
    let j = join(
        &mut a,
        "chat:1",
        Some(json!({"epoch": epoch, "offset": 99})),
    )
    .await;
    assert_eq!(j["recovered"], false, "{j}");
    // A room without history.
    let j = join(&mut a, "lobby", Some(json!({"epoch": "x", "offset": 0}))).await;
    assert_eq!(j["recovered"], false, "{j}");
    // At the top: nothing missed, recovered.
    let j = join(&mut a, "chat:1", Some(json!({"epoch": epoch, "offset": 5}))).await;
    assert_eq!(j["recovered"], true, "{j}");
    no_more_chats(&mut a).await;
    // Malformed recovery data is a bad request.
    send_json(
        &mut a,
        json!({"svc":"room","type":"join","room":"chat:1","seq":7,"data":{"recover":{"offset":"x"}}}),
    )
    .await;
    let e = next_of(&mut a, "error").await;
    assert_eq!(e["data"]["code"], "BAD_REQUEST", "{e}");
}

#[tokio::test]
async fn presence_events_are_not_kept_or_replayed() {
    let gw = gateway(1024, 100).await;
    let mut a = user(&gw, "alice").await;
    let j = join(&mut a, "chat:1", None).await;
    let epoch = j["epoch"].clone();
    let mut b = user(&gw, "bob").await;
    join(&mut b, "chat:1", None).await;
    let ev = next_of(&mut a, "member_joined").await;
    assert!(ev.get("offset").is_none(), "{ev}");
    drop(b);
    let ev = next_of(&mut a, "member_left").await;
    assert!(ev.get("offset").is_none(), "{ev}");

    let mut c = user(&gw, "carol").await;
    let j = join(&mut c, "chat:1", Some(json!({"epoch": epoch, "offset": 0}))).await;
    assert_eq!(
        (j["recovered"].clone(), j["offset"].clone()),
        (json!(true), json!(0))
    );
    no_more_chats(&mut c).await;
}

/// C1 + recovery: a reader too slow for Reliable traffic is closed, comes
/// back, and resumes without losing anything.
#[tokio::test]
async fn a_slow_consumer_resumes_where_it_was_closed() {
    let gw = gateway(32, 1000).await;
    let mut slow = user(&gw, "slow").await;
    let j = join(&mut slow, "chat:1", None).await;
    let epoch = j["epoch"].clone();
    let mut fast = user(&gw, "fast").await;
    join(&mut fast, "chat:1", None).await;

    // `slow` stops reading; 200 messages overflow its 32-frame queue.
    let mut last_fast = 0;
    for i in 0..200 {
        say(&mut fast, "chat:1", &format!("m{i}")).await;
    }
    for (off, _) in chats(&mut fast, 200).await {
        last_fast = off;
    }
    // What `slow` got before it was closed.
    let mut last = 0;
    loop {
        match next_event_within(&mut slow, Duration::from_secs(3)).await {
            Event::Json(v) if v["type"] == "msg" => last = v["offset"].as_u64().unwrap(),
            Event::Json(_) => continue,
            Event::Close(_) | Event::Ended => break,
            other => panic!("{other:?}"),
        }
    }
    assert!(last < last_fast, "slow was closed before the end ({last})");

    // Back, resuming after `last`: everything it missed arrives, in order,
    // although that is far more than its queue holds (the replay is one
    // queue entry).
    let mut s = user(&gw, "slow").await;
    let j = join(
        &mut s,
        "chat:1",
        Some(json!({"epoch": epoch, "offset": last})),
    )
    .await;
    assert_eq!(j["recovered"], true, "{j}");
    let n = (last_fast - last) as usize;
    assert!(n > 32, "more than the queue holds ({n})");
    let offsets: Vec<u64> = chats(&mut s, n).await.into_iter().map(|(o, _)| o).collect();
    assert_eq!(offsets, ((last + 1)..=last_fast).collect::<Vec<_>>());
    no_more_chats(&mut s).await;
}
