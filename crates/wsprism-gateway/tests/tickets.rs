//! One-time tickets (ADR 0008 §4) and the per-user permissions they carry
//! (§2): issued by the backend through the server API, consumed on first
//! use, bound to their tenant.

mod common;

use common::*;
use serde_json::json;

fn digest(k: &str) -> String {
    let d = ring::digest::digest(&ring::digest::SHA256, k.as_bytes());
    d.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn yaml() -> String {
    format!(
        r#"
version: 1
gateway:
  api: {{ listen: "127.0.0.1:0" }}
tenants:
  - id: a
    auth: [{{ kind: ticket }}]
    api_keys: [{{ name: backend, sha256: "{}" }}]
    policy: {{ ext_allowlist: ["room:*", "chat:*"] }}
  - id: b
    auth: [{{ kind: ticket }}]
    api_keys: [{{ name: backend, sha256: "{}" }}]
    policy: {{ ext_allowlist: ["room:*"] }}
"#,
        digest("key-a"),
        digest("key-b")
    )
}

async fn issue(gw: &ApiGateway, key: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
    http(gw.api, "POST", "/api/v1/tickets", Some(key), Some(body)).await
}

async fn connect_ticket(gw: &ApiGateway, tenant: &str, ticket: &str) -> Result<Ws, String> {
    let bearer = format!("Bearer {ticket}");
    let (mut ws, _) = try_connect_with(
        &ws_url(gw.addr, tenant, None),
        &[("authorization", &bearer)],
    )
    .await?;
    match next_event(&mut ws).await {
        Event::Json(v) if v["type"] == "authed" => {
            assert!(v["data"]["user"].is_string());
            Ok(ws)
        }
        other => Err(format!("expected sys.authed, got {other:?}")),
    }
}

#[tokio::test]
async fn ticket_admits_its_user_once() {
    let gw = spawn_with_api(&yaml()).await;
    let (status, resp) = issue(&gw, "key-a", json!({"user":"alice"})).await;
    assert_eq!(status, 200, "{resp}");
    let ticket = resp["ticket"].as_str().expect("ticket").to_string();
    assert!(ticket.starts_with("wpt_"));

    let mut ws = connect_ticket(&gw, "a", &ticket)
        .await
        .expect("first use admits");
    send_json(
        &mut ws,
        json!({"v":1,"svc":"room","type":"join","room":"r"}),
    )
    .await;
    assert!(matches!(next_event(&mut ws).await, Event::Json(v) if v["type"] == "joined"));
    let again = connect_ticket(&gw, "a", &ticket).await;
    assert!(
        again.as_ref().is_err_and(|e| e.contains("401")),
        "second use: {again:?}"
    );
}

#[tokio::test]
async fn ticket_is_bound_to_its_tenant() {
    let gw = spawn_with_api(&yaml()).await;
    let (_, resp) = issue(&gw, "key-a", json!({"user":"alice"})).await;
    let ticket = resp["ticket"].as_str().unwrap();
    let r = connect_ticket(&gw, "b", ticket).await;
    assert!(r.as_ref().is_err_and(|e| e.contains("401")), "{r:?}");
}

#[tokio::test]
async fn expired_ticket_is_rejected() {
    let gw = spawn_with_api(&yaml()).await;
    let (_, resp) = issue(&gw, "key-a", json!({"user":"alice","ttl_s":1})).await;
    let ticket = resp["ticket"].as_str().unwrap().to_string();
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let r = connect_ticket(&gw, "a", &ticket).await;
    assert!(r.as_ref().is_err_and(|e| e.contains("401")), "{r:?}");
}

#[tokio::test]
async fn ticket_requests_are_validated() {
    let gw = spawn_with_api(&yaml()).await;
    assert_eq!(
        issue(&gw, "key-a", json!({"user":"alice","ttl_s":301}))
            .await
            .0,
        400
    );
    assert_eq!(
        issue(&gw, "key-a", json!({"user":"a::b"})).await.0,
        400,
        "user ids follow the id rule"
    );
}

/// ADR 0008 §2 end to end: a spectator ticket may join `lobby` but not
/// other rooms, and may not publish.
#[tokio::test]
async fn ticket_permissions_limit_subscribe_and_publish() {
    let gw = spawn_with_api(&yaml()).await;
    let (_, resp) = issue(
        &gw,
        "key-a",
        json!({"user":"spectator","permissions":{"subscribe":["lobby"],"publish":[]}}),
    )
    .await;
    let mut ws = connect_ticket(&gw, "a", resp["ticket"].as_str().unwrap())
        .await
        .unwrap();

    send_json(
        &mut ws,
        json!({"v":1,"svc":"room","type":"join","room":"lobby"}),
    )
    .await;
    assert!(matches!(next_event(&mut ws).await, Event::Json(v) if v["type"] == "joined"));

    send_json(
        &mut ws,
        json!({"v":1,"svc":"room","type":"join","room":"secret"}),
    )
    .await;
    match next_event(&mut ws).await {
        Event::Json(v) => assert_eq!(v["data"]["code"], "NOT_ALLOWED", "{v}"),
        other => panic!("expected NOT_ALLOWED, got {other:?}"),
    }

    send_json(
        &mut ws,
        json!({"v":1,"svc":"chat","type":"send","room":"lobby","data":{"msg":"hi"}}),
    )
    .await;
    match next_event(&mut ws).await {
        Event::Json(v) => assert_eq!(v["data"]["code"], "NOT_ALLOWED", "{v}"),
        other => panic!("expected NOT_ALLOWED for publish, got {other:?}"),
    }
}

/// A full tenant refuses before authenticating, so a one-time ticket is not
/// burned by a 503 (review M6).
#[tokio::test]
async fn capacity_refusal_does_not_consume_the_ticket() {
    let yaml = yaml().replacen(
        "    auth: [{ kind: ticket }]",
        "    limits: { max_sessions_total: 1 }\n    auth: [{ kind: ticket }]",
        1,
    );
    let gw = spawn_with_api(&yaml).await;
    let t1 = issue(&gw, "key-a", json!({"user":"u1"})).await.1["ticket"]
        .as_str()
        .unwrap()
        .to_string();
    let t2 = issue(&gw, "key-a", json!({"user":"u2"})).await.1["ticket"]
        .as_str()
        .unwrap()
        .to_string();
    let first = connect_ticket(&gw, "a", &t1)
        .await
        .expect("fills the tenant");
    let refused = connect_ticket(&gw, "a", &t2).await;
    assert!(
        matches!(&refused, Err(e) if e.contains("503")),
        "{:?}",
        refused.map(|_| ())
    );
    drop(first);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    connect_ticket(&gw, "a", &t2)
        .await
        .expect("the same ticket still works");
}
