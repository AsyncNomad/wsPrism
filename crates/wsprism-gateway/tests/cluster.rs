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
    api_keys: [{{ name: backend, sha256: "{hash}" }}]
    policy: {{ ext_allowlist: ["room:*", "chat:*"], hot_allowlist: ["1:*"], presence_events: true }}
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
