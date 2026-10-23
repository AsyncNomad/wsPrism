//! Per-user session rules under concurrent handshakes (single node): the
//! limit and the sid takeover must hold when connections of one user
//! arrive at the same time, not only one after another.

mod common;

use std::time::Duration;

use common::*;

async fn authed_count(gw: &Gateway, sids: Vec<String>) -> (usize, Vec<Ws>) {
    let tasks: Vec<_> = sids
        .into_iter()
        .map(|sid| {
            let url = ws_url(gw.addr, "t", Some(&sid));
            tokio::spawn(async move {
                let (mut ws, _) = try_connect_url(&url).await.ok()?;
                match next_event(&mut ws).await {
                    Event::Json(v) if v["type"] == "authed" => Some(ws),
                    _ => None,
                }
            })
        })
        .collect();
    let mut open = Vec::new();
    for t in tasks {
        if let Ok(Some(ws)) = t.await {
            open.push(ws);
        }
    }
    (open.len(), open)
}

#[tokio::test]
async fn the_user_limit_holds_under_concurrent_handshakes() {
    let gw = spawn(
        r#"
version: 1
tenants:
  - id: t
    policy:
      ext_allowlist: ["room:*"]
      sessions: { mode: multi, max_sessions_per_user: 1, on_exceed: deny }
"#,
    )
    .await;
    for round in 0..5 {
        let sids = (0..48).map(|i| format!("r{round}s{i}")).collect();
        let (admitted, open) = authed_count(&gw, sids).await;
        assert_eq!(
            admitted, 1,
            "round {round}: {admitted} sessions admitted with a limit of 1"
        );
        drop(open);
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

#[tokio::test]
async fn one_sid_is_one_session_under_concurrent_handshakes() {
    let gw = spawn(
        r#"
version: 1
tenants:
  - id: t
    policy:
      ext_allowlist: ["room:*"]
      sessions: { mode: multi, max_sessions_per_user: 1000, on_exceed: deny }
"#,
    )
    .await;
    let sessions = gw.state.realtime().sessions.clone();
    for round in 0..5 {
        let sid = format!("tab{round}");
        let (_, open) = authed_count(&gw, vec![sid; 32]).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let live = sessions
            .local_sessions()
            .into_iter()
            .filter(|(_, _, s)| *s == format!("tab{round}"))
            .count();
        assert_eq!(live, 1, "round {round}: {live} live sessions share one sid");
        drop(open);
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}
