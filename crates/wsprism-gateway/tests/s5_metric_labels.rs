//! S5: attacker-controlled `tenant` query values must never become metric
//! label values (unbounded cardinality => unbounded memory).

mod common;

use common::*;

/// Handshake limiter on with a single token, so every attempt after the
/// first is rejected with 429 before the tenant is looked up.
const YAML: &str = r#"
version: 1
gateway:
  handshake_limit:
    enabled: true
    global_rps: 1
    global_burst: 1
    per_ip_rps: 1
    per_ip_burst: 1
tenants:
  - id: t
"#;

async fn try_connect(gw: &Gateway, tenant: &str) {
    let _ = try_connect_url(&ws_url(gw.addr, tenant, "dev", None)).await;
}

#[tokio::test]
async fn rate_limited_handshakes_do_not_create_per_tenant_series() {
    let gw = spawn(YAML).await;

    for i in 0..20 {
        try_connect(&gw, &format!("evil-{i}")).await;
    }

    let body = gw.state.metrics().render(&[]);
    assert!(
        !body.contains("evil-"),
        "unvalidated tenant leaked into metrics:\n{body}"
    );
    assert!(
        body.contains(
            r#"wsprism_handshake_rejections_total{reason="rate_limit",tenant="_unknown"}"#
        ),
        "rejections should be counted under the fixed _unknown label:\n{body}"
    );
}

#[tokio::test]
async fn unknown_tenant_rejections_are_counted_under_fixed_label() {
    // Limiter disabled: unknown tenants reach the lookup and get 400.
    let gw = spawn(BASE_YAML).await;

    for i in 0..5 {
        try_connect(&gw, &format!("nope-{i}")).await;
    }

    let body = gw.state.metrics().render(&[]);
    assert!(!body.contains("nope-"), "{body}");
    assert!(
        body.contains(
            r#"wsprism_handshake_rejections_total{reason="unknown_tenant",tenant="_unknown"} 5"#
        ),
        "{body}"
    );
}

#[tokio::test]
async fn known_tenant_keeps_its_label_when_rate_limited() {
    let gw = spawn(YAML).await;

    // First attempt consumes the only token; the rest are 429s.
    for _ in 0..3 {
        try_connect(&gw, "t").await;
    }

    let body = gw.state.metrics().render(&[]);
    assert!(
        body.contains(r#"wsprism_handshake_rejections_total{reason="rate_limit",tenant="t"} 2"#),
        "{body}"
    );
}
