//! A one-time ticket stays one-time across a Sentinel failover with
//! replication behind (0.2, pre-release review #2): the replica is cut off
//! the network (`docker pause` would not do: the kernel still buffers the
//! replication stream, and the replica applies it when resumed),
//! the ticket is redeemed on the primary, the primary dies and the replica
//! (still holding the ticket) is promoted. Without waiting for a replica to
//! acknowledge the consumption, the same ticket would be accepted again.
//!
//! Against `bench/redis-sentinel-test.sh` (`WSPRISM_TEST_SENTINEL`,
//! `..._PRIMARY`, `..._REPLICA`; best with `WSPRISM_SENTINEL_DOWN_AFTER_MS=5000`,
//! see there); it stops the primary, so start that environment afresh
//! before each run. Without it this test does nothing.
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use common::*;
use serde_json::json;

const KEY: &str = "key-s";

async fn node(sentinel: &str, prefix: &str) -> ApiGateway {
    let d = ring::digest::digest(&ring::digest::SHA256, KEY.as_bytes());
    let hash: String = d.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    let yaml = format!(
        r#"
version: 1
gateway:
  api: {{ listen: "127.0.0.1:0" }}
  broker:
    kind: redis
    url: "redis://primary"
    prefix: "{prefix}"
    slot_ttl_s: 5
    sentinel: {{ addresses: ["{sentinel}"], master_name: mymaster }}
tenants:
  - id: t
    auth: [{{ kind: ticket }}]
    api_keys: [{{ name: backend, sha256: "{hash}" }}]
    policy: {{ ext_allowlist: ["room:*"] }}
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

fn docker(args: &[&str]) -> String {
    let out = Command::new("docker").args(args).output().unwrap();
    assert!(out.status.success(), "docker {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Connect with `ticket`: Ok if accepted, else the handshake error.
async fn redeem(gw: &ApiGateway, ticket: &str) -> Result<(), String> {
    let url = ws_url(gw.addr, "t", None);
    let bearer = format!("Bearer {ticket}");
    try_connect_with(&url, &[("authorization", &bearer)])
        .await
        .map(|_| ())
}

#[tokio::test]
async fn a_ticket_is_accepted_once_across_a_lagging_failover() {
    let (Ok(sentinel), Ok(primary), Ok(replica)) = (
        std::env::var("WSPRISM_TEST_SENTINEL"),
        std::env::var("WSPRISM_TEST_SENTINEL_PRIMARY"),
        std::env::var("WSPRISM_TEST_SENTINEL_REPLICA"),
    ) else {
        eprintln!("WSPRISM_TEST_SENTINEL(_PRIMARY, _REPLICA) not set: skipping");
        return;
    };
    let p = format!("sentk{}", std::process::id());
    let gw = node(&sentinel, &p).await;
    let (status, body) = http(
        gw.api,
        "POST",
        "/api/v1/tickets",
        Some(KEY),
        Some(json!({"user": "tia"})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let ticket = body["ticket"].as_str().unwrap().to_string();

    // The replica has the ticket; then it stops following the primary.
    let pattern = format!("{p}:kv:*");
    let deadline = Instant::now() + Duration::from_secs(10);
    while docker(&[
        "exec",
        &replica,
        "redis-cli",
        "--scan",
        "--pattern",
        &pattern,
    ])
    .trim()
    .is_empty()
    {
        assert!(
            Instant::now() < deadline,
            "the ticket never reached the replica"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let net = docker(&[
        "inspect",
        "-f",
        "{{range $k, $v := .NetworkSettings.Networks}}{{$k}} {{$v.IPAddress}}{{end}}",
        &replica,
    ]);
    let (net, ip) = net.trim().split_once(' ').unwrap();
    docker(&["network", "disconnect", net, &replica]);

    // Redeemed on the primary; the replica never hears of it.
    let first = redeem(&gw, &ticket).await;
    eprintln!("first redemption: {first:?}");

    // The primary dies; the replica (with the ticket) is promoted.
    docker(&["stop", "-t", "0", &primary]);
    docker(&["network", "connect", "--ip", ip, net, &replica]);
    let failed_over = Instant::now();
    let second = loop {
        assert!(
            // A Sentinel in TILT mode waits 30 s before failing over.
            failed_over.elapsed() < Duration::from_secs(90),
            "no answer 90 s after the failover"
        );
        match redeem(&gw, &ticket).await {
            Ok(()) => break true,
            Err(e) if e.contains("401") => break false,
            Err(_) => tokio::time::sleep(Duration::from_millis(300)).await, // 503: not yet
        }
    };
    // With `replica_ack` (default with Sentinel): the first redemption
    // fails (no replica acknowledged; 503), and after the failover the new
    // primary has no replica either (this setup has one), so the ticket is
    // consumed again unacknowledged and refused. Never accepted twice;
    // logins wait for a replica — the price of failing closed.
    eprintln!("second redemption accepted: {second}");
    assert!(
        !(first.is_ok() && second),
        "the ticket was accepted twice (before and after the failover)"
    );
}
