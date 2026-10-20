//! The Redis broker against a real Redis (`WSPRISM_TEST_REDIS`, see
//! `bench/redis-test.sh`); without it these tests do nothing.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use wsprism_engine::broker::Broker;
use wsprism_redis::{RedisBroker, RedisConfig};

fn redis_url() -> Option<String> {
    let url = std::env::var("WSPRISM_TEST_REDIS").ok();
    if url.is_none() {
        eprintln!("WSPRISM_TEST_REDIS not set: skipping");
    }
    url
}

fn prefix(name: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("rt{}-{name}", n % 1_000_000_000)
}

async fn broker(url: &str, prefix: &str) -> Arc<RedisBroker> {
    let b = Arc::new(RedisBroker::new(RedisConfig::new(url).prefix(prefix)).unwrap());
    b.start().await.unwrap();
    b
}

/// A10: however many reapers race, each dead member is reported once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_reapers_report_each_dead_member_once() {
    let Some(url) = redis_url() else { return };
    let p = prefix("reap");
    let (a, b) = (broker(&url, &p).await, broker(&url, &p).await);
    let client = redis::Client::open(url.as_str()).unwrap();
    let mut c = client.get_multiplexed_async_connection().await.unwrap();
    let mut pipe = redis::pipe();
    for i in 0..20 {
        pipe.cmd("HSET")
            .arg(format!("{p}:pr:{{t::r{}}}", i % 4))
            .arg(format!("t::g{i}::s#1"))
            .arg(format!("deadnode|999|g{i}|{i}|1"))
            .ignore();
    }
    for r in 0..4 {
        pipe.cmd("SADD")
            .arg(format!("{p}:nr:deadnode"))
            .arg(format!("t::r{r}"))
            .ignore();
    }
    pipe.cmd("HSET")
        .arg(format!("{p}:nodes"))
        .arg("deadnode")
        .arg(999)
        .ignore();
    let _: () = pipe.query_async(&mut c).await.unwrap();

    let calls: Vec<_> = (0..16)
        .map(|i| {
            let x = if i % 2 == 0 {
                Arc::clone(&a)
            } else {
                Arc::clone(&b)
            };
            tokio::spawn(async move { x.presence().unwrap().reap_dead().await.unwrap() })
        })
        .collect();
    let mut users = Vec::new();
    for c in calls {
        users.extend(c.await.unwrap().into_iter().map(|d| d.user.to_string()));
    }
    users.sort();
    let mut want: Vec<String> = (0..20).map(|i| format!("g{i}")).collect();
    want.sort();
    assert_eq!(users, want, "each dead member exactly once");
    assert!(a
        .presence()
        .unwrap()
        .members("t::r0")
        .await
        .unwrap()
        .is_empty());
}
