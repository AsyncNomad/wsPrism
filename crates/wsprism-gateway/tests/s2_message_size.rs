//! S2: oversized messages must be rejected by the transport *before* any
//! decoding, and the client must be told why (close 1009).

mod common;

use common::*;
use serde_json::json;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

fn assert_closed_too_big(ev: Event) {
    match ev {
        Event::Close(Some(f)) => assert_eq!(f.code, CloseCode::Size, "close frame: {f:?}"),
        other => panic!("expected close 1009 (Message Too Big), got {other:?}"),
    }
}

#[tokio::test]
async fn oversized_text_is_closed_with_1009_without_decoding() {
    let gw = spawn(BASE_YAML).await;
    let mut ws = connect(gw.addr, "t", None).await;

    // A *valid* envelope, 64x over the 1024-byte tenant limit. If the gateway
    // decoded it first we would see a sys.error envelope instead of a close.
    let big = "x".repeat(64 * 1024);
    send_json(
        &mut ws,
        json!({"v":1,"svc":"chat","type":"send","room":"r","data":{"msg": big}}),
    )
    .await;

    assert_closed_too_big(next_event(&mut ws).await);
}

#[tokio::test]
async fn oversized_binary_is_closed_with_1009() {
    let gw = spawn(BASE_YAML).await;
    let mut ws = connect(gw.addr, "t", None).await;

    send_bin(&mut ws, hot_frame(1, 1, &vec![0u8; 64 * 1024])).await;

    assert_closed_too_big(next_event(&mut ws).await);
}

#[tokio::test]
async fn message_exactly_at_limit_is_accepted() {
    let gw = spawn(BASE_YAML).await;
    let mut ws = connect(gw.addr, "t", None).await;

    // 4-byte header + 1020-byte payload = 1024 bytes = max_frame_bytes.
    let payload = vec![7u8; 1020];
    send_bin(&mut ws, hot_frame(1, 1, &payload)).await;

    match next_event(&mut ws).await {
        Event::Binary(b) => assert_eq!(b, payload, "echo service returns payload"),
        other => panic!("expected echoed payload, got {other:?}"),
    }
}
