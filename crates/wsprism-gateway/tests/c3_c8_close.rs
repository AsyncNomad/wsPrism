//! C3: when the server ends a session it must tell the client why (sys.error
//! and/or a close code) instead of silently dropping the socket.
//! C8: server-initiated closes (kick/replace/drain) must end the session even
//! if the peer never completes the close handshake.

mod common;

use std::time::Duration;

use common::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

fn expect_error(ev: Event, code: &str) {
    match ev {
        Event::Json(v) => {
            assert_eq!(v["type"], "error", "{v}");
            assert_eq!(v["data"]["code"], code, "{v}");
        }
        other => panic!("expected sys.error {code}, got {other:?}"),
    }
}

fn expect_close(ev: Event, code: CloseCode) {
    match ev {
        Event::Close(Some(f)) => assert_eq!(f.code, code, "{f:?}"),
        other => panic!("expected close {code:?}, got {other:?}"),
    }
}

#[tokio::test]
async fn decode_error_is_reported_then_closed_1002() {
    let gw = spawn(BASE_YAML).await;
    let mut ws = connect(gw.addr, "t", None).await;

    use futures_util::SinkExt;
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        "not json".into(),
    ))
    .await
    .expect("send");

    expect_error(next_event(&mut ws).await, "BAD_REQUEST");
    expect_close(next_event(&mut ws).await, CloseCode::Protocol);
}

#[tokio::test]
async fn session_limit_deny_is_reported_then_closed_1008() {
    let gw = spawn(
        r#"
version: 1
tenants:
  - id: t
    policy:
      sessions: { mode: single, max_sessions_per_user: 1, on_exceed: deny }
"#,
    )
    .await;
    let _first = connect(gw.addr, "t", Some("a")).await;

    let (mut second, _) = try_connect_url(&ws_url(gw.addr, "t", Some("b")))
        .await
        .expect("upgrade succeeds; rejection is in-band");
    expect_error(next_event(&mut second).await, "TOO_MANY_SESSIONS");
    expect_close(next_event(&mut second).await, CloseCode::Policy);
}

#[tokio::test]
async fn bad_ticket_is_rejected_with_401_before_upgrade() {
    let gw = spawn(BASE_YAML).await;
    let bad = [("authorization", "Bearer not-a-ticket")];
    let Err(err) = try_connect_with(&ws_url(gw.addr, "t", None), &bad).await else {
        panic!("handshake must fail");
    };
    assert!(err.contains("401"), "expected HTTP 401, got: {err}");
}

#[tokio::test]
async fn replaced_session_gets_kicked_notice_and_close_1008() {
    let gw = spawn(BASE_YAML).await;
    let mut old = connect(gw.addr, "t", Some("tab")).await;
    let _new = connect(gw.addr, "t", Some("tab")).await;

    match next_event(&mut old).await {
        Event::Json(v) => assert_eq!(v["type"], "kicked", "{v}"),
        other => panic!("expected sys.kicked, got {other:?}"),
    }
    expect_close(next_event(&mut old).await, CloseCode::Policy);
}

#[tokio::test]
async fn draining_closes_sessions_with_1001() {
    let gw = spawn(BASE_YAML).await;
    let mut ws = connect(gw.addr, "t", None).await;

    gw.state.realtime().best_effort_shutdown_all("draining");

    expect_close(next_event(&mut ws).await, CloseCode::Away);
}

/// A raw TCP client that completes the WS upgrade but never speaks WebSocket
/// afterwards (so it never answers the server's close frame).
async fn mute_client(gw: &Gateway, sid: &str) -> tokio::net::TcpStream {
    let mut tcp = tokio::net::TcpStream::connect(gw.addr).await.expect("tcp");
    let req = format!(
        "GET /v1/ws?tenant=t&sid={sid} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer dev\r\n\
         Upgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
        gw.addr
    );
    tcp.write_all(req.as_bytes()).await.expect("write");
    tcp
}

#[tokio::test]
async fn evicted_session_ends_even_if_peer_never_answers_close() {
    // Long idle timeout: only an explicit server-side close can end it fast.
    let gw = spawn(
        r#"
version: 1
gateway: { ping_interval_ms: 5000, idle_timeout_ms: 30000 }
tenants:
  - id: t
"#,
    )
    .await;
    let mut mute = mute_client(&gw, "tab").await;
    tokio::time::sleep(Duration::from_millis(200)).await; // let it register
    let _new = connect(gw.addr, "t", Some("tab")).await; // replaces `mute`

    // Read (and ignore) whatever the server sends; it must then hang up.
    let eof = tokio::time::timeout(Duration::from_secs(4), async {
        let mut buf = [0u8; 4096];
        loop {
            match mute.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    })
    .await;
    assert!(eof.is_ok(), "server kept the evicted session open");
}
