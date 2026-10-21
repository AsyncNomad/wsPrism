//! A dropped Redis subscriber connection (ADR 0011 §4): the node
//! resubscribes and tells the room's members they may have missed messages
//! (`sys.gap`). Its own test binary: it kills every pub/sub connection on
//! the shared Redis, which would disturb tests running beside it.

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

/// When a node's subscriber connection drops, it resubscribes and tells
/// the room's members they may have missed messages (`sys.gap`), then
/// traffic flows again.
#[tokio::test]
async fn a_dropped_subscription_is_reported_as_a_gap() {
    let Some(url) = redis_url() else { return };
    let p = prefix("gap");
    let (a_node, b_node) = (node(&url, &p).await, node(&url, &p).await);
    let mut alice = user(&a_node, "alice").await;
    let mut bob = user(&b_node, "bob").await;
    join(&mut alice, "r").await;
    join(&mut bob, "r").await;

    // Kill every pub/sub connection (both nodes).
    let client = redis::Client::open(url.as_str()).unwrap();
    let mut c = client.get_multiplexed_async_connection().await.unwrap();
    let killed: i64 = redis::cmd("CLIENT")
        .arg("KILL")
        .arg("TYPE")
        .arg("pubsub")
        .query_async(&mut c)
        .await
        .unwrap();
    assert!(killed >= 2, "{killed}");

    assert_eq!(next_of(&mut alice, "gap").await["room"], "r");
    assert_eq!(next_of(&mut bob, "gap").await["room"], "r");
    send_json(
        &mut alice,
        json!({"svc":"chat","type":"send","room":"r","data":{"msg":"after"}}),
    )
    .await;
    assert_eq!(next_of(&mut bob, "msg").await["data"]["msg"], "after");
}
