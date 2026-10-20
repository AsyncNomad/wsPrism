//! Two gateway nodes sharing rooms through Redis (ADR 0011 R3). Needs a
//! local Redis: `WSPRISM_TEST_REDIS=redis://<host>:6379` (see
//! `bench/redis-test.sh`, which runs one on an isolated docker network);
//! without it these tests pass without doing anything.

mod common;

use std::time::Duration;

use common::*;
use serde_json::{json, Value};

fn redis_url() -> Option<String> {
    let url = std::env::var("WSPRISM_TEST_REDIS").ok();
    if url.is_none() {
        eprintln!("WSPRISM_TEST_REDIS not set: skipping the cluster test");
    }
    url
}

/// A unique key prefix per test, so tests sharing a Redis never see each
/// other's rooms or node slots.
fn prefix(name: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("t{}-{name}", n % 1_000_000_000)
}

const KEY: &str = "key-c";

async fn node(url: &str, prefix: &str) -> ApiGateway {
    node_with(url, prefix, 30).await
}

async fn node_with(url: &str, prefix: &str, slot_ttl_s: u64) -> ApiGateway {
    let d = ring::digest::digest(&ring::digest::SHA256, KEY.as_bytes());
    let hash: String = d.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    let yaml = format!(
        r#"
version: 1
gateway:
  api: {{ listen: "127.0.0.1:0" }}
  broker: {{ kind: redis, url: "{url}", prefix: "{prefix}", pubsub_connections: 2, slot_ttl_s: {slot_ttl_s} }}
tenants:
  - id: t
    auth: [{{ kind: dev }}, {{ kind: ticket }}]
    api_keys: [{{ name: backend, sha256: "{hash}" }}]
    policy: {{ ext_allowlist: ["room:*", "chat:*"], hot_allowlist: ["1:*"], presence_events: true }}
    history: [{{ rooms: "chat:*", size: 100, ttl_s: 600 }}]
"#
    );
    let cfg = wsprism_gateway::config::load_from_str(&yaml).unwrap();
    // Not `common::builder()`: the broker comes from this config.
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    // The node accepts once its broker has started (slot claimed).
    loop {
        match try_connect_with(
            &ws_url(gw.addr, "t", None),
            &[
                ("authorization", &bearer),
                ("sec-websocket-protocol", "wsprism.v2.json"),
            ],
        )
        .await
        {
            Ok((mut ws, _)) => match next_event(&mut ws).await {
                Event::Json(v) if v["type"] == "authed" => return ws,
                other => panic!("{other:?}"),
            },
            Err(e) if tokio::time::Instant::now() < deadline => {
                let _ = e;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => panic!("connect: {e}"),
        }
    }
}

async fn next_of(ws: &mut Ws, ty: &str) -> Value {
    loop {
        match next_event(ws).await {
            Event::Json(v) if v["type"] == ty => return v,
            Event::Json(_) | Event::Binary(_) => continue,
            other => panic!("expected {ty}, got {other:?}"),
        }
    }
}

async fn join(ws: &mut Ws, room: &str) -> Value {
    send_json(ws, json!({"svc":"room","type":"join","room":room,"seq":1})).await;
    let j = next_of(ws, "joined").await;
    // Subscriptions are applied asynchronously (ADR 0011 B5).
    tokio::time::sleep(Duration::from_millis(150)).await;
    j
}

#[tokio::test]
async fn rooms_span_nodes() {
    let Some(url) = redis_url() else { return };
    let p = prefix("span");
    let (a_node, b_node) = (node(&url, &p).await, node(&url, &p).await);
    let mut alice = user(&a_node, "alice").await;
    let mut bob = user(&b_node, "bob").await;
    let ja = join(&mut alice, "r").await;
    let jb = join(&mut bob, "r").await;

    // Member ids carry different node slots: unique across the cluster.
    let (ma, mb) = (
        ja["member_id"].as_u64().unwrap(),
        jb["member_id"].as_u64().unwrap(),
    );
    assert_ne!(ma >> 22, mb >> 22, "{ja} {jb}");
    // Alice hears of bob, who joined on the other node.
    let ev = next_of(&mut alice, "member_joined").await;
    assert_eq!(ev["data"]["user"], "bob");

    send_json(
        &mut alice,
        json!({"svc":"chat","type":"send","room":"r","data":{"msg":"from a"}}),
    )
    .await;
    assert_eq!(next_of(&mut bob, "msg").await["data"]["msg"], "from a");
    assert_eq!(next_of(&mut alice, "msg").await["data"]["msg"], "from a");

    // The server API on node B reaches alice on node A.
    let (status, body) = http(
        b_node.api,
        "POST",
        "/api/v1/publish",
        Some(KEY),
        Some(json!({"room":"r","data":{"x":1}})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(next_of(&mut alice, "publish").await["data"]["x"], 1);

    // Bob leaves by disconnecting: alice on the other node is told.
    drop(bob);
    let ev = next_of(&mut alice, "member_left").await;
    assert_eq!(ev["data"]["member_id"].as_u64(), Some(mb));
}

#[tokio::test]
async fn hot_state_crosses_nodes_with_the_local_room_id() {
    let Some(url) = redis_url() else { return };
    let p = prefix("hot");
    let (a_node, b_node) = (node(&url, &p).await, node(&url, &p).await);
    let mut alice = user(&a_node, "alice").await;
    let mut bob = user(&b_node, "bob").await;
    // Make the room ids differ between the nodes.
    join(&mut bob, "pad").await;
    let ja = join(&mut alice, "r").await;
    let jb = join(&mut bob, "r").await;
    let (chan_a, chan_b) = (
        ja["chan"].as_u64().unwrap() as u32,
        jb["chan"].as_u64().unwrap() as u32,
    );
    assert_ne!(chan_a, chan_b);

    // Alice publishes state on node A (v2: kind, flags, chan, svc, op, payload).
    let mut f = vec![0x01, 0x00];
    f.extend_from_slice(&chan_a.to_le_bytes());
    f.extend_from_slice(&[1, 2, b's']);
    send_bin(&mut alice, f).await;
    loop {
        match next_event(&mut bob).await {
            Event::Binary(b) => {
                assert_eq!(&b[2..6], &chan_b.to_le_bytes(), "bob's room id");
                let sender = u32::from_le_bytes(b[8..12].try_into().unwrap());
                assert_eq!(u64::from(sender), ja["member_id"].as_u64().unwrap());
                assert_eq!(b.last(), Some(&b's'));
                break;
            }
            Event::Json(_) => continue,
            other => panic!("{other:?}"),
        }
    }
}

/// The node slot outlives its TTL while the node runs (heartbeat), and a
/// second node takes another slot.
#[tokio::test]
async fn node_slots_are_kept_alive_and_distinct() {
    let Some(url) = redis_url() else { return };
    let p = prefix("slot");
    let a = node_with(&url, &p, 3).await;
    let _ = user(&a, "alice").await; // started
    let b = node_with(&url, &p, 3).await;
    let _ = user(&b, "bob").await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let client = redis::Client::open(url.as_str()).unwrap();
    let mut c = client.get_multiplexed_async_connection().await.unwrap();
    for slot in 0..2 {
        let owner: Option<String> = redis::cmd("GET")
            .arg(format!("{p}:slot:{slot}"))
            .query_async(&mut c)
            .await
            .unwrap();
        assert!(owner.is_some(), "slot {slot} expired despite the heartbeat");
    }
}

/// Cluster history (ADR 0011 R3-2): offsets come from Redis, so they agree
/// across nodes, and a client that left node A resumes on node B.
#[tokio::test]
async fn history_is_shared_and_recovery_works_across_nodes() {
    let Some(url) = redis_url() else { return };
    let p = prefix("hist");
    let (a_node, b_node) = (node(&url, &p).await, node(&url, &p).await);
    let mut alice = user(&a_node, "alice").await;
    let mut bob = user(&b_node, "bob").await;
    let ja = join(&mut alice, "chat:1").await;
    let epoch = ja["epoch"].as_str().expect("epoch").to_string();
    assert_eq!(ja["offset"], 0);
    let jb = join(&mut bob, "chat:1").await;
    assert_eq!(
        jb["epoch"].as_str(),
        Some(epoch.as_str()),
        "one history for the cluster"
    );

    send_json(
        &mut bob,
        json!({"svc":"chat","type":"send","room":"chat:1","data":{"msg":"one"}}),
    )
    .await;
    let on_a = next_of(&mut alice, "msg").await;
    let on_b = next_of(&mut bob, "msg").await;
    assert_eq!(
        (on_a["offset"].as_u64(), on_b["offset"].as_u64()),
        (Some(1), Some(1))
    );

    // Alice drops off node A; bob keeps talking on node B.
    drop(alice);
    for m in ["two", "three", "four"] {
        send_json(
            &mut bob,
            json!({"svc":"chat","type":"send","room":"chat:1","data":{"msg":m}}),
        )
        .await;
    }
    for _ in 0..3 {
        next_of(&mut bob, "msg").await;
    }

    // She comes back on node B and resumes after offset 1.
    let mut alice = user(&b_node, "alice").await;
    send_json(
        &mut alice,
        json!({"svc":"room","type":"join","room":"chat:1","seq":2,
               "data":{"recover":{"epoch": epoch, "offset": 1}}}),
    )
    .await;
    let j = next_of(&mut alice, "joined").await;
    assert_eq!(j["recovered"], true, "{j}");
    assert_eq!(j["offset"], 4);
    let mut got = Vec::new();
    for _ in 0..3 {
        let m = next_of(&mut alice, "msg").await;
        got.push((
            m["offset"].as_u64().unwrap(),
            m["data"]["msg"].as_str().unwrap().to_string(),
        ));
    }
    assert_eq!(
        got,
        [(2, "two".into()), (3, "three".into()), (4, "four".into())]
    );

    // Then live, from node A this time.
    let mut carol = user(&a_node, "carol").await;
    join(&mut carol, "chat:1").await;
    send_json(
        &mut carol,
        json!({"svc":"chat","type":"send","room":"chat:1","data":{"msg":"five"}}),
    )
    .await;
    let m = next_of(&mut alice, "msg").await;
    assert_eq!(
        (m["offset"].as_u64(), m["data"]["msg"].as_str()),
        (Some(5), Some("five"))
    );
}

/// If Redis loses part of a room's history (restart, eviction), a new epoch
/// starts: a client resyncs instead of resuming from offsets that now mean
/// something else.
#[tokio::test]
async fn lost_history_starts_a_new_epoch() {
    let Some(url) = redis_url() else { return };
    let p = prefix("lost");
    let a_node = node(&url, &p).await;
    let mut alice = user(&a_node, "alice").await;
    join(&mut alice, "chat:1").await;
    send_json(
        &mut alice,
        json!({"svc":"chat","type":"send","room":"chat:1","data":{"msg":"one"}}),
    )
    .await;
    let m = next_of(&mut alice, "msg").await;
    let epoch = {
        let mut probe = user(&a_node, "probe").await;
        join(&mut probe, "chat:1").await["epoch"].clone()
    };
    assert_eq!(m["offset"], 1);

    // Only the counter is lost.
    let client = redis::Client::open(url.as_str()).unwrap();
    let mut c = client.get_multiplexed_async_connection().await.unwrap();
    let _: i64 = redis::cmd("DEL")
        .arg(format!("{p}:h:{{t::chat:1}}:seq"))
        .query_async(&mut c)
        .await
        .unwrap();

    let mut bob = user(&a_node, "bob").await;
    send_json(
        &mut bob,
        json!({"svc":"room","type":"join","room":"chat:1","seq":2,
               "data":{"recover":{"epoch": epoch, "offset": 1}}}),
    )
    .await;
    let j = next_of(&mut bob, "joined").await;
    assert_eq!(j["recovered"], false, "{j}");
    assert_ne!(j["epoch"], epoch, "a new epoch");
    assert_eq!(j["offset"], 0);
}

/// One-time tickets are shared (ADR 0011 §6): issued by node A, redeemed on
/// node B, and only once anywhere. Redis holds a hash, never the ticket.
#[tokio::test]
async fn tickets_work_on_any_node_once() {
    let Some(url) = redis_url() else { return };
    let p = prefix("tk");
    let (a_node, b_node) = (node(&url, &p).await, node(&url, &p).await);
    let _ = user(&a_node, "warm").await; // both nodes started
    let _ = user(&b_node, "warm").await;
    let (status, body) = http(
        a_node.api,
        "POST",
        "/api/v1/tickets",
        Some(KEY),
        Some(json!({"user": "tia", "permissions": {"subscribe": ["chat:*"]}})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let ticket = body["ticket"].as_str().unwrap().to_string();

    let client = redis::Client::open(url.as_str()).unwrap();
    let mut c = client.get_multiplexed_async_connection().await.unwrap();
    let keys: Vec<String> = redis::cmd("KEYS")
        .arg(format!("{p}:kv:*"))
        .query_async(&mut c)
        .await
        .unwrap();
    assert_eq!(keys.len(), 1);
    assert!(
        !keys[0].contains(&ticket[4..]),
        "the key is a hash: {keys:?}"
    );

    let connect = |gw: &ApiGateway| {
        let url = ws_url(gw.addr, "t", None);
        let bearer = format!("Bearer {ticket}");
        async move {
            try_connect_with(
                &url,
                &[
                    ("authorization", &bearer),
                    ("sec-websocket-protocol", "wsprism.v2.json"),
                ],
            )
            .await
        }
    };
    let (mut ws, _) = connect(&b_node).await.expect("redeemed on the other node");
    match next_event(&mut ws).await {
        Event::Json(v) if v["type"] == "authed" => assert_eq!(v["data"]["user"], "tia"),
        other => panic!("{other:?}"),
    }
    // Its permissions came along.
    send_json(
        &mut ws,
        json!({"svc":"room","type":"join","room":"lobby","seq":1}),
    )
    .await;
    assert_eq!(
        next_of(&mut ws, "error").await["data"]["code"],
        "NOT_ALLOWED"
    );

    let again = connect(&a_node).await;
    assert!(
        matches!(&again, Err(e) if e.contains("401")),
        "{:?}",
        again.err()
    );
}
