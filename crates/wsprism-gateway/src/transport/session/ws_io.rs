//! WebSocket implementation of the session transport seam (`io`).
//!
//! Only this module (and `codec` for decoding) knows axum's WebSocket types,
//! so swapping the WebSocket implementation (see ROADMAP: transport choice)
//! or adding another transport touches the transport layer only.

use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};

use super::io::{RecvError, SessionRx, SessionTx};
use crate::realtime::OutFrame;
use crate::transport::codec::{decode, Inbound};

/// How long to keep the socket open after sending close 1009 (see S2).
const CLOSE_LINGER: Duration = Duration::from_millis(500);

pub(crate) struct WsTx(SplitSink<WebSocket, Message>);
pub(crate) struct WsRx(SplitStream<WebSocket>);

/// Split an upgraded socket into the session's two halves.
pub(crate) fn split(socket: WebSocket) -> (WsTx, WsRx) {
    let (tx, rx) = socket.split();
    (WsTx(tx), WsRx(rx))
}

/// Convert a queued frame to the wire message.
///
/// axum 0.7 messages own `String`/`Vec<u8>`, so this is where each
/// recipient's copy of a shared payload is made (in that recipient's task,
/// not the publisher's). With a `Bytes`-based message type (axum 0.8) this
/// becomes a reference-count bump.
fn to_message(frame: OutFrame) -> Message {
    match frame {
        // Valid UTF-8 by construction, so the lossy conversion never alters it.
        OutFrame::Text(t) => Message::Text(String::from_utf8_lossy(t.as_bytes()).into_owned()),
        OutFrame::Binary(b) => Message::Binary(b.to_vec()),
        OutFrame::Ping => Message::Ping(Vec::new()),
    }
}

/// True if the read failed because the frame/message exceeded the configured
/// codec limit (see `max_message_size` in `ws_upgrade`).
fn is_message_too_long(e: axum::Error) -> bool {
    matches!(
        e.into_inner().downcast_ref::<tungstenite::Error>(),
        Some(tungstenite::Error::Capacity(
            tungstenite::error::CapacityError::MessageTooLong { .. }
        ))
    )
}

impl SessionTx for WsTx {
    async fn send(&mut self, frame: OutFrame) -> Result<(), ()> {
        self.0.send(to_message(frame)).await.map_err(|_| ())
    }

    async fn send_close(&mut self, code: u16, reason: String) -> Result<(), ()> {
        let close = Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        }));
        self.0.send(close).await.map_err(|_| ())
    }

    async fn finish_peer_close(&mut self) {
        // Flush tungstenite's queued close reply.
        let _ = self.0.close().await;
    }

    fn too_big_linger(&self) -> Duration {
        // The oversized payload is still unread in the kernel buffer
        // (tungstenite rejects on the header and cannot skip the body).
        // Closing now would make the kernel send RST, and the peer may
        // discard our close frame. Linger briefly so it can read it first.
        CLOSE_LINGER
    }
}

impl SessionRx for WsRx {
    async fn recv(&mut self) -> Option<Result<Inbound, RecvError>> {
        Some(match self.0.next().await? {
            Ok(msg) => decode(msg).map_err(RecvError::Decode),
            Err(e) => Err(if is_message_too_long(e) {
                RecvError::TooBig
            } else {
                RecvError::Broken
            }),
        })
    }

    async fn await_peer_close(&mut self) {
        while let Some(Ok(m)) = self.0.next().await {
            if matches!(m, Message::Close(_)) {
                break;
            }
        }
    }
}
