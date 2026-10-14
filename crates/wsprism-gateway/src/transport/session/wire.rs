//! The edge between engine frames and the WebSocket library.
//!
//! Only this module (and `codec` for inbound) knows the transport's message
//! type, so swapping the WebSocket implementation (see ROADMAP: transport
//! choice) touches the transport layer only.

use axum::extract::ws::{CloseFrame, Message};

use crate::realtime::{CloseSpec, OutFrame};

/// Convert a queued frame to the wire message.
///
/// axum 0.7 messages own `String`/`Vec<u8>`, so this is where each
/// recipient's copy of a shared payload is made (in that recipient's task,
/// not the publisher's). With a `Bytes`-based message type (axum 0.8) this
/// becomes a reference-count bump.
pub(super) fn to_message(frame: OutFrame) -> Message {
    match frame {
        // Valid UTF-8 by construction, so the lossy conversion never alters it.
        OutFrame::Text(t) => Message::Text(String::from_utf8_lossy(t.as_bytes()).into_owned()),
        OutFrame::Binary(b) => Message::Binary(b.to_vec()),
        OutFrame::Ping => Message::Ping(Vec::new()),
    }
}

/// The close frame for `spec`.
pub(super) fn close_message(spec_code: u16, reason: String) -> Message {
    Message::Close(Some(CloseFrame {
        code: spec_code,
        reason: reason.into(),
    }))
}

/// Notice + close frame for a server-initiated close, in sending order.
pub(super) fn close_messages(spec: CloseSpec) -> impl Iterator<Item = Message> {
    spec.notice
        .map(Message::Text)
        .into_iter()
        .chain(std::iter::once(close_message(spec.code, spec.reason)))
}
