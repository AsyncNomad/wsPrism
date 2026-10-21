//! Two nodes on a TLS-only Redis with a private CA and mutual TLS
//! (`gateway.broker.tls`), against `bench/redis-tls-test.sh`
//! (`WSPRISM_TEST_REDIS_TLS`, `WSPRISM_TEST_REDIS_TLS_DIR`); without it this
//! test does nothing.
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

mod common;

use common::*;
use serde_json::{json, Value};

async fn node(url: &str, dir: &str, prefix: &str) -> ApiGateway {
    let yaml = format!(
        r#"
version: 1
gateway:
  api: {{ listen: "127.0.0.1:0" }}
  broker:
    kind: redis
    url: "{url}"
    prefix: "{prefix}"
    tls: {{ ca_file: "{dir}/ca.crt", client_cert_file: "{dir}/client.crt", client_key_file: "{dir}/client.key" }}
tenants:
  - id: t
    policy: {{ ext_allowlist: ["room:*", "chat:*"] }}
    history: [{{ rooms: "chat:*", size: 100, ttl_s: 600 }}]
"#
    );
    let cfg = wsprism_gateway::config::load_from_str(&yaml).unwrap();
    spawn_built_with_api(
        wsprism_gateway::Gateway::builder()
            .config(cfg)
            .build()
            .unwrap(),
    )
    .await
}

/// The next JSON message of type `ty` (panics after 10 s or on a close).
async fn next_of(ws: &mut Ws, ty: &str) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match next_event_within(ws, left).await {
            Event::Json(v) if v["type"] == ty => return v,
            Event::Json(_) => {}
            other => panic!("waiting for {ty}: {other:?}"),
        }
    }
}

#[tokio::test]
async fn nodes_share_a_history_room_over_mutual_tls() {
    let (Ok(url), Ok(dir)) = (
        std::env::var("WSPRISM_TEST_REDIS_TLS"),
        std::env::var("WSPRISM_TEST_REDIS_TLS_DIR"),
    ) else {
        eprintln!("WSPRISM_TEST_REDIS_TLS(_DIR) not set: skipping");
        return;
    };
    let p = format!("tlsgw{}", std::process::id());
    let (a, b) = (node(&url, &dir, &p).await, node(&url, &dir, &p).await);
    let mut alice = connect(a.addr, "t", Some("a")).await;
    let mut bob = connect(b.addr, "t", Some("b")).await;
    for ws in [&mut alice, &mut bob] {
        send_json(
            ws,
            json!({"v":1,"svc":"room","type":"join","room":"chat:1","seq":1}),
        )
        .await;
    }
    // Both joins answered, so both nodes subscribed.
    for ws in [&mut alice, &mut bob] {
        next_of(ws, "joined").await;
    }
    send_json(
        &mut alice,
        json!({"v":1,"svc":"chat","type":"send","room":"chat:1","data":{"msg":"over tls"}}),
    )
    .await;
    let m = next_of(&mut bob, "msg").await;
    assert_eq!(m["data"]["msg"], "over tls");
    assert_eq!(
        m["offset"], 1,
        "a kept message: the history script ran over TLS"
    );
}
