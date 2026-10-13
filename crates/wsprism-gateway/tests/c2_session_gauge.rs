//! C2: `wsprism_ws_sessions_active` must stay exact when a session is kicked.

mod common;

use std::time::Duration;

use common::*;

const YAML: &str = r#"
version: 1
tenants:
  - id: t
    policy:
      sessions:
        mode: single
        max_sessions_per_user: 1
        on_exceed: kick_oldest
"#;

fn active_gauge(gw: &Gateway) -> Option<i64> {
    let body = gw.state.metrics().render(&[]);
    body.lines()
        .find_map(|l| l.strip_prefix(r#"wsprism_ws_sessions_active{tenant="t"} "#))
        .map(|v| v.parse().expect("gauge value"))
}

#[tokio::test]
async fn kicked_session_is_decremented_exactly_once() {
    let gw = spawn(YAML).await;
    let mut old = connect(gw.addr, "t", Some("a")).await;
    let _new = connect(gw.addr, "t", Some("b")).await; // kicks `old`

    // `old` is told it was kicked, then closed.
    match next_event(&mut old).await {
        Event::Json(v) => assert_eq!(v["type"], "kicked", "{v}"),
        other => panic!("expected sys.kicked, got {other:?}"),
    }
    drop(old); // let the old session task finish and run its cleanup

    // The gauge reads 1 right after the kick even with the bug; the double
    // decrement only happens once the old session's cleanup has run.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(gw.state.realtime().sessions.len_sessions(), 1);
    assert_eq!(active_gauge(&gw), Some(1));
}
