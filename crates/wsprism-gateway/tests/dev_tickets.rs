//! Development ticket stub: `dev` -> `user:dev`, `dev:<id>` -> `user:<id>`
//! (`<id>` in `[A-Za-z0-9_-]{1,64}`). Lets benchmarks use one user per
//! connection; replaced by the `Authenticator` trait in M2.

mod common;

use common::*;

const YAML: &str = "version: 1\ntenants:\n  - id: t\n";

async fn authed_user(ticket: &str) -> Result<String, String> {
    let gw = spawn(YAML).await;
    let bearer = format!("Bearer {ticket}");
    let (mut ws, _) =
        try_connect_with(&ws_url(gw.addr, "t", None), &[("authorization", &bearer)]).await?;
    match next_event(&mut ws).await {
        Event::Json(v) if v["type"] == "authed" => {
            Ok(v["data"]["user"].as_str().unwrap_or("").to_string())
        }
        other => Err(format!("expected sys.authed, got {other:?}")),
    }
}

#[tokio::test]
async fn named_dev_ticket_maps_to_its_own_user() {
    assert_eq!(authed_user("dev").await.as_deref(), Ok("user:dev"));
    assert_eq!(authed_user("dev:alice").await.as_deref(), Ok("user:alice"));
}

#[tokio::test]
async fn malformed_dev_tickets_are_rejected() {
    for t in ["dev:", "dev:a::b", "dev:has.dot", "devx"] {
        let r = authed_user(t).await;
        assert!(r.as_ref().is_err_and(|e| e.contains("401")), "{t:?}: {r:?}");
    }
}
