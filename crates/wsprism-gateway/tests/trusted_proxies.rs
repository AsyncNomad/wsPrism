//! Per-client handshake limits behind a trusted load balancer
//! (`gateway.trusted_proxies` + `X-Forwarded-For`).

mod common;

use common::*;

/// One handshake per client IP; everything arrives from 127.0.0.1 (the "LB").
fn yaml(trusted: &str) -> String {
    format!(
        r#"
version: 1
gateway:
  trusted_proxies: [{trusted}]
  handshake_limit:
    enabled: true
    global_rps: 100
    global_burst: 100
    per_ip_rps: 1
    per_ip_burst: 1
tenants:
  - id: t
"#
    )
}

async fn connect_as(gw: &Gateway, client: &str) -> Result<(), String> {
    try_connect_with(
        &ws_url(gw.addr, "t", None),
        &[AUTH, ("x-forwarded-for", client)],
    )
    .await
    .map(|_| ())
}

#[tokio::test]
async fn behind_trusted_proxy_each_client_has_its_own_bucket() {
    let gw = spawn(&yaml(r#""127.0.0.1""#)).await;

    assert!(connect_as(&gw, "198.51.100.1").await.is_ok());
    assert!(
        connect_as(&gw, "198.51.100.2").await.is_ok(),
        "a different client behind the same proxy must not share the bucket"
    );
    let again = connect_as(&gw, "198.51.100.1").await;
    assert!(
        again.as_ref().is_err_and(|e| e.contains("429")),
        "same client again must be limited, got {again:?}"
    );
}

#[tokio::test]
async fn untrusted_peer_cannot_spoof_its_address() {
    let gw = spawn(&yaml("")).await; // trust nobody

    assert!(connect_as(&gw, "198.51.100.1").await.is_ok());
    let spoofed = connect_as(&gw, "198.51.100.2").await;
    assert!(
        spoofed.as_ref().is_err_and(|e| e.contains("429")),
        "X-Forwarded-For from an untrusted peer must be ignored, got {spoofed:?}"
    );
}
