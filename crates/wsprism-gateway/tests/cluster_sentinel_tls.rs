//! Like `cluster_sentinel.rs`, with everything over TLS and client
//! certificates (sentinels, primary, replica: `rediss://` addresses and URL,
//! `gateway.broker.tls` for both): two nodes follow the primary across a
//! failover. Against `bench/redis-sentinel-tls-test.sh`
//! (`WSPRISM_TEST_SENTINEL_TLS`, `..._TLS_PRIMARY`, `..._TLS_DIR`; it stops
//! the primary, so start that environment afresh before each run); without
//! it this test does nothing.
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

mod common;

use std::time::{Duration, Instant};

use common::*;
use serde_json::{json, Value};

async fn node(sentinel: &str, dir: &str, prefix: &str) -> ApiGateway {
    let yaml = format!(
        r#"
version: 1
gateway:
  api: {{ listen: "127.0.0.1:0" }}
  broker:
    kind: redis
    url: "rediss://primary"       # TLS to the nodes; Sentinel names the host
    prefix: "{prefix}"
    slot_ttl_s: 5
    sentinel: {{ addresses: ["{sentinel}"], master_name: mymaster }}
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

/// The next message of type `ty` within `wait` (None on timeout).
async fn next_of_within(ws: &mut Ws, ty: &str, wait: Duration) -> Option<Value> {
    let deadline = Instant::now() + wait;
    loop {
        match next_event_within(ws, deadline.saturating_duration_since(Instant::now())).await {
            Event::Json(v) if v["type"] == ty => return Some(v),
            Event::Json(_) => {}
            Event::Timeout => return None,
            other => panic!("waiting for {ty}: {other:?}"),
        }
    }
}

fn send(msg: &str) -> Value {
    json!({"v":1,"svc":"chat","type":"send","room":"chat:1","data":{"msg":msg}})
}

#[tokio::test]
async fn nodes_follow_the_primary_across_a_failover_over_tls() {
    let (Ok(sentinel), Ok(primary), Ok(dir)) = (
        std::env::var("WSPRISM_TEST_SENTINEL_TLS"),
        std::env::var("WSPRISM_TEST_SENTINEL_TLS_PRIMARY"),
        std::env::var("WSPRISM_TEST_SENTINEL_TLS_DIR"),
    ) else {
        eprintln!("WSPRISM_TEST_SENTINEL_TLS(_PRIMARY, _DIR) not set: skipping");
        return;
    };
    let p = format!("stls{}", std::process::id());
    let (a, b) = (
        node(&sentinel, &dir, &p).await,
        node(&sentinel, &dir, &p).await,
    );
    let mut alice = connect(a.addr, "t", Some("a")).await;
    let mut bob = connect(b.addr, "t", Some("b")).await;
    for ws in [&mut alice, &mut bob] {
        send_json(
            ws,
            json!({"v":1,"svc":"room","type":"join","room":"chat:1","seq":1}),
        )
        .await;
        next_of_within(ws, "joined", Duration::from_secs(10))
            .await
            .expect("joined");
    }
    send_json(&mut alice, send("before")).await;
    let m = next_of_within(&mut bob, "msg", Duration::from_secs(10))
        .await
        .expect("before");
    assert_eq!(m["data"]["msg"], "before");

    // The primary crashes; Sentinel promotes the replica.
    let out = std::process::Command::new("docker")
        .args(["stop", "-t", "0", &primary])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let failed_over = Instant::now();

    // Messages from alice reach bob again through the new primary.
    let deadline = failed_over + Duration::from_secs(40);
    let mut n = 0;
    loop {
        assert!(
            Instant::now() < deadline,
            "no delivery 40 s after the failover"
        );
        n += 1;
        send_json(&mut alice, send(&format!("after {n}"))).await;
        if let Some(m) = next_of_within(&mut bob, "msg", Duration::from_secs(1)).await {
            if m["data"]["msg"]
                .as_str()
                .is_some_and(|s| s.starts_with("after"))
            {
                eprintln!(
                    "delivered again {:?} after the failover",
                    failed_over.elapsed()
                );
                break;
            }
        }
    }
}
