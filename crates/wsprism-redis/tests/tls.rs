//! The broker over TLS (`rediss://`): a private CA and mutual TLS, against
//! the TLS-only Redis of `bench/redis-tls-test.sh` (`WSPRISM_TEST_REDIS_TLS`,
//! `WSPRISM_TEST_REDIS_TLS_DIR`); without it these tests do nothing.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use wsprism_engine::broker::Broker;
use wsprism_redis::{RedisBroker, RedisConfig, RedisTls};

fn tls_env() -> Option<(String, std::path::PathBuf)> {
    match (
        std::env::var("WSPRISM_TEST_REDIS_TLS"),
        std::env::var("WSPRISM_TEST_REDIS_TLS_DIR"),
    ) {
        (Ok(url), Ok(dir)) => Some((url, dir.into())),
        _ => {
            eprintln!("WSPRISM_TEST_REDIS_TLS(_DIR) not set: skipping");
            None
        }
    }
}

/// Start a broker with `tls`; Ok if it connected and leased a slot.
async fn starts(url: &str, tls: Option<RedisTls>) -> Result<u16, String> {
    let mut cfg = RedisConfig::new(url).prefix(format!("tls{}", std::process::id()));
    if let Some(tls) = tls {
        cfg = cfg.tls(tls);
    }
    let b = RedisBroker::new(cfg).map_err(|e| e.to_string())?;
    match tokio::time::timeout(Duration::from_secs(10), b.start()).await {
        Ok(r) => r.map_err(|e| e.to_string()),
        Err(_) => Err("timed out".into()),
    }
}

#[tokio::test]
async fn private_ca_and_client_certificate() {
    let Some((url, dir)) = tls_env() else { return };
    let read = |f: &str| std::fs::read(dir.join(f)).unwrap();
    let full = RedisTls::default()
        .ca(read("ca.crt"))
        .client(read("client.crt"), read("client.key"));
    assert!(
        starts(&url, Some(full)).await.is_ok(),
        "CA + client certificate"
    );

    // The server requires a client certificate.
    let no_client = RedisTls::default().ca(read("ca.crt"));
    assert!(
        starts(&url, Some(no_client)).await.is_err(),
        "no client certificate"
    );

    // The system trust store does not know the private CA.
    let no_ca = RedisTls::default().client(read("client.crt"), read("client.key"));
    assert!(
        starts(&url, Some(no_ca)).await.is_err(),
        "untrusted server certificate"
    );
    assert!(starts(&url, None).await.is_err(), "no TLS settings at all");
}

#[test]
fn tls_needs_rediss_and_never_prints_the_key() {
    let tls = RedisTls::default().client(b"cert".to_vec(), b"SECRET-KEY".to_vec());
    assert!(!format!("{tls:?}").contains("SECRET"));
    let cfg = RedisConfig::new("redis://127.0.0.1:6379").tls(tls);
    assert!(RedisBroker::new(cfg).is_err(), "tls with redis://");
}
