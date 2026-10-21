//! S1: the per-connection rate limit must cover the Hot Lane, not only Ext.

mod common;

use std::time::Duration;

use common::*;
use serde_json::json;

/// rps=1 / burst=5, connection scope: a burst of 50 must be clipped to ~5.
const YAML: &str = r#"
version: 1
tenants:
  - id: t
    limits:
      max_frame_bytes: 1024
    policy:
      rate_limit_rps: 1
      rate_limit_burst: 5
      rate_limit_scope: connection
      ext_allowlist: ["room:*"]
      hot_allowlist: ["1:*"]
      hot_requires_active_room: false
"#;

/// Count events until the connection has been quiet for `quiet`.
async fn drain_count(ws: &mut Ws, quiet: Duration) -> (usize, usize) {
    let (mut bins, mut jsons) = (0, 0);
    loop {
        match next_event_within(ws, quiet).await {
            Event::Binary(_) => bins += 1,
            Event::Json(_) => jsons += 1,
            Event::Timeout => return (bins, jsons),
            other => panic!("unexpected event: {other:?}"),
        }
    }
}

#[tokio::test]
async fn hot_lane_burst_is_clipped_by_connection_limiter() {
    let gw = spawn(YAML).await;
    let mut ws = connect(gw.addr, "t", None).await;

    for i in 0..50u8 {
        send_bin(&mut ws, hot_frame(1, 1, &[i])).await;
    }

    let (echoes, _) = drain_count(&mut ws, Duration::from_millis(500)).await;
    // burst (5) + at most one refill token while sending.
    assert!(
        (5..=6).contains(&echoes),
        "expected ~5 echoes after rate limiting, got {echoes}"
    );
}

#[tokio::test]
async fn hot_and_ext_share_one_connection_budget() {
    let gw = spawn(YAML).await;
    let mut ws = connect(gw.addr, "t", None).await;

    // Spend the whole burst on the Hot Lane...
    for i in 0..5u8 {
        send_bin(&mut ws, hot_frame(1, 1, &[i])).await;
    }
    let (echoes, _) = drain_count(&mut ws, Duration::from_millis(300)).await;
    assert_eq!(echoes, 5);

    // ...so an immediate Ext request is dropped (no sys.joined).
    send_json(
        &mut ws,
        json!({"v":1,"svc":"room","type":"join","room":"r"}),
    )
    .await;
    let (_, replies) = drain_count(&mut ws, Duration::from_millis(300)).await;
    assert_eq!(replies, 0, "ext request should have been rate limited");
}
