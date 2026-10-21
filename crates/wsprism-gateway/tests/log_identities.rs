//! `gateway.log_identities` (pre-release review): session logs show user
//! ids, client sids and IPs only as configured. Its own test binary: it
//! installs a global log subscriber.
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};

use common::*;

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Connect as `alice` / sid `tab-7`, disconnect, return what was logged.
async fn session_logs(buf: &Buf, mode: &str) -> String {
    buf.0.lock().unwrap().clear();
    let yaml = format!(
        "version: 1\ngateway:\n  log_identities: {mode}\ntenants:\n  - id: t\n    policy: {{ ext_allowlist: [\"room:*\"] }}\n"
    );
    let gw = spawn(&yaml).await;
    let url = ws_url(gw.addr, "t", Some("tab-7"));
    let (ws, _) = try_connect_with(&url, &[("Authorization", "Bearer dev.alice")])
        .await
        .unwrap();
    drop(ws);
    // The session's last log line ("session raii cleanup done") is written
    // when the server notices the close.
    for _ in 0..100 {
        if String::from_utf8_lossy(&buf.0.lock().unwrap()).contains("cleanup done") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned()
}

#[tokio::test]
async fn identities_are_logged_as_configured() {
    let buf = Buf::default();
    let out = buf.clone();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(move || out.clone())
            .finish(),
    )
    .unwrap();

    let plain = session_logs(&buf, "plain").await;
    let line = plain
        .lines()
        .find(|l| l.contains("cleanup done"))
        .expect(&plain);
    assert!(
        line.contains("u=alice") && line.contains("s=tab-7") && line.contains("ip=127.0.0.1"),
        "{line}"
    );
    assert!(line.contains(" c="), "the connection id: {line}");

    let hashed = session_logs(&buf, "hashed").await;
    let line = hashed
        .lines()
        .find(|l| l.contains("cleanup done"))
        .expect(&hashed);
    assert!(
        line.contains("u=h:") && line.contains("s=h:") && line.contains("ip=h:"),
        "{line}"
    );
    assert!(
        !hashed.contains("alice") && !hashed.contains("tab-7") && !hashed.contains("127.0.0.1"),
        "{hashed}"
    );

    let off = session_logs(&buf, "off").await;
    let line = off
        .lines()
        .find(|l| l.contains("cleanup done"))
        .expect(&off);
    assert!(
        !line.contains("u=") && !line.contains(" s=") && !line.contains("ip="),
        "{line}"
    );
    assert!(
        !off.contains("alice") && !off.contains("tab-7") && !off.contains("127.0.0.1"),
        "{off}"
    );
}
