//! C9: the session loop must never await its own outbound queue (it is the
//! queue's only consumer). A full queue must end the session as a slow
//! consumer instead of freezing it forever.

mod common;

use std::time::Duration;

use common::*;
use serde_json::json;

/// Queue capacity 1 makes "queue full while handling a request" the common
/// case: pipelined requests produce replies faster than the loop writes them.
const YAML: &str = r#"
version: 1
gateway:
  session_queue_capacity: 1
tenants:
  - id: t
    policy:
      ext_allowlist: ["room:*"]
"#;

#[tokio::test]
async fn full_queue_never_freezes_the_session() {
    let gw = spawn(YAML).await;
    let mut ws = connect(gw.addr, "t", None).await;

    // Pipeline many requests without reading the replies.
    for i in 0..50 {
        send_json(
            &mut ws,
            json!({"v":1,"svc":"room","type":"join","room":format!("r{i}")}),
        )
        .await;
    }

    // The session must make progress: either answer everything or close
    // (slow consumer). A frozen session produces nothing until the deadline.
    let mut joined = 0;
    loop {
        match next_event_within(&mut ws, Duration::from_secs(3)).await {
            Event::Json(v) if v["type"] == "joined" => {
                joined += 1;
                if joined == 50 {
                    return;
                }
            }
            Event::Json(v) if v["type"] == "error" => {
                assert_eq!(v["data"]["code"], "SLOW_CONSUMER", "{v}");
            }
            Event::Close(_) | Event::Ended => return,
            Event::Timeout => panic!("session froze after {joined} replies"),
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
