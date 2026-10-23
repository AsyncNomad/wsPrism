//! After Redis loses its data (restart without persistence), a node takes
//! its slot back at once instead of at the next heartbeat (`slot_ttl_s`/3,
//! 10 s by default): meanwhile a node starting up would lease the "free"
//! slot and fence the running one for nothing. A file of its own: it
//! flushes and disconnects the shared test Redis (`WSPRISM_TEST_REDIS`).
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

mod common;

use std::time::{Duration, Instant};

use common::*;

#[tokio::test]
async fn a_node_takes_its_slot_back_right_after_redis_lost_it() {
    let Ok(url) = std::env::var("WSPRISM_TEST_REDIS") else {
        eprintln!("WSPRISM_TEST_REDIS not set: skipping");
        return;
    };
    let p = format!("refresh{}", std::process::id());
    let yaml = format!(
        r#"
version: 1
gateway:
  api: {{ listen: "127.0.0.1:0" }}
  broker: {{ kind: redis, url: "{url}", prefix: "{p}", slot_ttl_s: 30 }}
tenants:
  - id: t
    policy: {{ ext_allowlist: ["room:*"] }}
"#
    );
    let cfg = wsprism_gateway::config::load_from_str(&yaml).unwrap();
    let _gw = spawn_built_with_api(
        wsprism_gateway::Gateway::builder()
            .config(cfg)
            .build()
            .unwrap(),
    )
    .await;
    let client = redis::Client::open(url.as_str()).unwrap();
    let mut c = client.get_multiplexed_async_connection().await.unwrap();
    let slot_keys = |c: &mut redis::aio::MultiplexedConnection| {
        let mut c = c.clone();
        let p = p.clone();
        async move {
            let keys: Vec<String> = redis::cmd("KEYS")
                .arg(format!("{p}:slot:*"))
                .query_async(&mut c)
                .await
                .unwrap();
            keys
        }
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while slot_keys(&mut c).await.is_empty() {
        assert!(Instant::now() < deadline, "the node never took a slot");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Redis "restarts" empty: data gone, every connection dropped.
    let _: () = redis::cmd("FLUSHALL").query_async(&mut c).await.unwrap();
    for kind in ["pubsub", "normal"] {
        let _: redis::Value = redis::cmd("CLIENT")
            .arg("KILL")
            .arg("TYPE")
            .arg(kind)
            .query_async(&mut c)
            .await
            .unwrap_or(redis::Value::Nil);
    }
    let mut c = client.get_multiplexed_async_connection().await.unwrap();
    let lost = Instant::now();
    while slot_keys(&mut c).await.is_empty() {
        assert!(
            lost.elapsed() < Duration::from_secs(3),
            "the slot was not taken back within 3 s (heartbeat every 10 s)"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    eprintln!("slot back {:?} after Redis lost it", lost.elapsed());
}
