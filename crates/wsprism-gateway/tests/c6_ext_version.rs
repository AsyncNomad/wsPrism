//! C6: Ext Lane envelopes with an unsupported protocol version are rejected
//! (the Hot Lane already enforces v=1).

mod common;

use common::*;
use serde_json::json;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

#[tokio::test]
async fn unsupported_ext_version_is_rejected() {
    let gw = spawn(BASE_YAML).await;
    let mut ws = connect(gw.addr, "t", None).await;

    send_json(
        &mut ws,
        json!({"v":2,"svc":"room","type":"join","room":"r"}),
    )
    .await;

    match next_event(&mut ws).await {
        Event::Json(v) => {
            assert_eq!(v["type"], "error", "{v}");
            assert_eq!(v["data"]["code"], "UNSUPPORTED_VERSION", "{v}");
        }
        other => panic!("expected sys.error UNSUPPORTED_VERSION, got {other:?}"),
    }
    match next_event(&mut ws).await {
        Event::Close(Some(f)) => assert_eq!(f.code, CloseCode::Protocol),
        other => panic!("expected close 1002, got {other:?}"),
    }
}
