//! Keep-alive ping and idle timeout, observed on the wire by a client that
//! completes the upgrade but never sends anything (so it never answers pings
//! and never counts as active). Uses the smallest allowed intervals, so this
//! test takes ~10 s.

mod common;

use std::time::Duration;

use common::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const YAML: &str = r#"
version: 1
gateway: { ping_interval_ms: 5000, idle_timeout_ms: 10000 }
tenants:
  - id: t
"#;

/// Server-to-client frames are unmasked: [FIN|opcode][len][payload].
/// Returns (opcode, payload) for each complete frame in `buf`, from `pos`.
fn frames(buf: &[u8], pos: &mut usize) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    loop {
        let rest = &buf[*pos..];
        if rest.len() < 2 {
            return out;
        }
        let (len, hdr) = match rest[1] & 0x7f {
            126 if rest.len() >= 4 => (u16::from_be_bytes([rest[2], rest[3]]) as usize, 4),
            126 | 127 => return out, // larger frames don't occur here
            n => (n as usize, 2),
        };
        if rest.len() < hdr + len {
            return out;
        }
        out.push((rest[0] & 0x0f, rest[hdr..hdr + len].to_vec()));
        *pos += hdr + len;
    }
}

#[tokio::test]
async fn silent_client_is_pinged_then_closed_for_idleness() {
    let gw = spawn(YAML).await;
    let mut tcp = tokio::net::TcpStream::connect(gw.addr).await.expect("tcp");
    let req = format!(
        "GET /v1/ws?tenant=t HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer dev\r\n\
         Upgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
        gw.addr
    );
    tcp.write_all(req.as_bytes()).await.expect("write");

    let started = tokio::time::Instant::now();
    let mut buf = Vec::new();
    let mut pos = None; // set once the HTTP response headers are consumed
    let (mut ping_at, mut timeout_notice, mut close) = (None, false, None);
    let res = tokio::time::timeout(Duration::from_secs(15), async {
        let mut chunk = [0u8; 4096];
        loop {
            let n = tcp.read(&mut chunk).await.expect("read");
            if n == 0 {
                return;
            }
            buf.extend_from_slice(&chunk[..n]);
            if pos.is_none() {
                match buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    Some(i) => pos = Some(i + 4),
                    None => continue,
                }
            }
            let p = pos.as_mut().expect("headers consumed");
            for (op, payload) in frames(&buf, p) {
                match op {
                    0x9 => ping_at = ping_at.or(Some(started.elapsed())),
                    0x1 if String::from_utf8_lossy(&payload).contains("TIMEOUT") => {
                        timeout_notice = true
                    }
                    0x8 => {
                        let code = u16::from_be_bytes([payload[0], payload[1]]);
                        close = Some((code, String::from_utf8_lossy(&payload[2..]).into_owned()));
                        return;
                    }
                    _ => {}
                }
            }
        }
    })
    .await;
    assert!(res.is_ok(), "no close within 15 s");
    let ping_at = ping_at.expect("a keep-alive ping arrived");
    assert!(
        ping_at < Duration::from_secs(8),
        "first ping after {ping_at:?}"
    );
    assert!(timeout_notice, "sys.error TIMEOUT notice before the close");
    assert_eq!(close, Some((1000, "idle_timeout".to_string())));
    let closed_at = started.elapsed();
    assert!(
        (Duration::from_secs(9)..Duration::from_secs(13)).contains(&closed_at),
        "closed after {closed_at:?} (idle timeout 10 s)"
    );
}
