//! Where tickets may come from (`policy.ticket_sources`) and
//! `Sec-WebSocket-Protocol` version negotiation.

mod common;

use common::*;

const YAML: &str = r#"
version: 1
tenants:
  - id: t            # default sources: header, subprotocol
  - id: legacy
    policy:
      ticket_sources: [query]
"#;

fn is_status(r: &Result<impl Sized, String>, code: &str) -> bool {
    r.as_ref().is_err_and(|e| e.contains(code))
}

#[tokio::test]
async fn browser_ticket_via_subprotocol_echoes_only_the_version() {
    let gw = spawn(YAML).await;
    let offer = [("sec-websocket-protocol", "wsprism.v1, wsprism.auth.dev")];
    let (_, resp) = try_connect_with(&ws_url(gw.addr, "t", None), &offer)
        .await
        .expect("subprotocol ticket accepted");
    let echoed = resp
        .headers()
        .get("sec-websocket-protocol")
        .map(|v| v.to_str().unwrap_or(""));
    assert_eq!(
        echoed,
        Some("wsprism.v1"),
        "the auth entry must never be echoed"
    );
}

#[tokio::test]
async fn query_ticket_is_off_by_default() {
    let gw = spawn(YAML).await;
    let url = format!("{}&ticket=dev", ws_url(gw.addr, "t", None));
    let r = try_connect_with(&url, &[]).await;
    assert!(is_status(&r, "401"), "got {r:?}");

    let body = gw.state.metrics().render(&[]);
    assert!(
        body.contains(r#"wsprism_handshake_rejections_total{reason="auth_missing",tenant="t"} 1"#),
        "{body}"
    );
}

#[tokio::test]
async fn query_ticket_works_when_the_tenant_opts_in() {
    let gw = spawn(YAML).await;
    let url = format!("{}&ticket=dev", ws_url(gw.addr, "legacy", None));
    assert!(try_connect_with(&url, &[]).await.is_ok());
    // ...and the header is not accepted for that tenant.
    let r = try_connect_url(&ws_url(gw.addr, "legacy", None)).await;
    assert!(is_status(&r, "401"), "got {r:?}");
}

#[tokio::test]
async fn unsupported_protocol_version_is_rejected_with_400() {
    let gw = spawn(YAML).await;
    let offer = [AUTH, ("sec-websocket-protocol", "wsprism.v9")];
    let r = try_connect_with(&ws_url(gw.addr, "t", None), &offer).await;
    assert!(is_status(&r, "400"), "got {r:?}");
}
