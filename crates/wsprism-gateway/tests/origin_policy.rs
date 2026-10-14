//! Cross-site WebSocket hijacking protection: the browser `Origin` header is
//! checked against the tenant's `policy.allowed_origins`.

mod common;

use common::*;

const YAML: &str = r#"
version: 1
tenants:
  - id: open
    policy:
      allowed_origins: ["https://app.example.com", "https://*.example.org"]
  - id: locked   # no allowed_origins: browsers denied by default
"#;

async fn connect_from(gw: &Gateway, tenant: &str, origin: Option<&str>) -> Result<(), String> {
    let url = ws_url(gw.addr, tenant, "dev", None);
    let headers: Vec<(&'static str, &str)> = origin.map(|o| ("origin", o)).into_iter().collect();
    try_connect_with(&url, &headers).await.map(|_| ())
}

fn is_403(r: &Result<(), String>) -> bool {
    r.as_ref().is_err_and(|e| e.contains("403"))
}

#[tokio::test]
async fn allowed_origins_pass_and_others_get_403() {
    let gw = spawn(YAML).await;
    assert!(connect_from(&gw, "open", Some("https://app.example.com"))
        .await
        .is_ok());
    assert!(connect_from(&gw, "open", Some("https://eu.example.org"))
        .await
        .is_ok());
    let evil = connect_from(&gw, "open", Some("https://evil.example.net")).await;
    assert!(
        is_403(&evil),
        "foreign origin must be rejected, got {evil:?}"
    );
}

#[tokio::test]
async fn default_policy_rejects_browsers_but_not_native_clients() {
    let gw = spawn(YAML).await;
    let browser = connect_from(&gw, "locked", Some("https://app.example.com")).await;
    assert!(is_403(&browser), "got {browser:?}");
    assert!(connect_from(&gw, "locked", None).await.is_ok());
}

#[tokio::test]
async fn origin_rejections_are_counted() {
    let gw = spawn(YAML).await;
    let _ = connect_from(&gw, "locked", Some("https://x.io")).await;
    let body = gw.state.metrics().render(&[]);
    assert!(
        body.contains(
            r#"wsprism_handshake_rejections_total{reason="origin_not_allowed",tenant="locked"} 1"#
        ),
        "{body}"
    );
}
