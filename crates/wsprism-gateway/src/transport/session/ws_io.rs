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
use crate::transport::codec::{decode, Inbound};
use crate::transport::subprotocol::ProtocolVersion;
use wsprism_engine::spi::OutFrame;

/// How long to keep the socket open after sending close 1009 (see S2).
const CLOSE_LINGER: Duration = Duration::from_millis(500);

pub(crate) struct WsTx {
    sink: SplitSink<WebSocket, Message>,
    /// Picks the per-version bytes of Hot frames.
    protocol: ProtocolVersion,
}
pub(crate) struct WsRx {
    stream: SplitStream<WebSocket>,
    /// Decides which envelope rules apply to text messages.
    protocol: ProtocolVersion,
}

/// Split an upgraded socket into the session's two halves.
pub(crate) fn split(socket: WebSocket, protocol: ProtocolVersion) -> (WsTx, WsRx) {
    let (tx, stream) = socket.split();
    (WsTx { sink: tx, protocol }, WsRx { stream, protocol })
}

/// Convert a queued frame to the wire message.
///
/// axum 0.7 messages own `String`/`Vec<u8>`, so this is where each
/// recipient's copy of a shared payload is made (in that recipient's task,
/// not the publisher's). With a `Bytes`-based message type (axum 0.8) this
/// becomes a reference-count bump.
fn to_message(frame: OutFrame, protocol: ProtocolVersion) -> Message {
    match frame {
        // Valid UTF-8 by construction, so the lossy conversion never alters it.
        OutFrame::Text(t) => Message::Text(String::from_utf8_lossy(t.as_bytes()).into_owned()),
        OutFrame::Binary(b) => Message::Binary(b.to_vec()),
        OutFrame::Ping => Message::Ping(Vec::new()),
        OutFrame::Hot(h) => Message::Binary(match protocol {
            ProtocolVersion::V1 => h.v1.to_vec(),
            ProtocolVersion::V2 => h.v2.to_vec(),
        }),
        // Expanded by `WsTx::feed` before conversion; never nested.
        OutFrame::Batch(_) => Message::Ping(Vec::new()),
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
        self.feed(frame).await?;
        self.flush().await
    }

    async fn feed(&mut self, frame: OutFrame) -> Result<(), ()> {
        if let OutFrame::Batch(frames) = frame {
            for f in frames.iter() {
                self.sink
                    .feed(to_message(f.clone(), self.protocol))
                    .await
                    .map_err(|_| ())?;
            }
            return Ok(());
        }
        self.sink
            .feed(to_message(frame, self.protocol))
            .await
            .map_err(|_| ())
    }

    async fn flush(&mut self) -> Result<(), ()> {
        self.sink.flush().await.map_err(|_| ())
    }

    async fn send_close(&mut self, code: u16, reason: String) -> Result<(), ()> {
        let close = Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        }));
        self.sink.send(close).await.map_err(|_| ())
    }

    async fn finish_peer_close(&mut self) {
        // Flush tungstenite's queued close reply.
        let _ = self.sink.close().await;
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
        Some(match self.stream.next().await? {
            Ok(msg) => decode(msg, self.protocol).map_err(RecvError::Decode),
            Err(e) => Err(if is_message_too_long(e) {
                RecvError::TooBig
            } else {
                RecvError::Broken
            }),
        })
    }

    async fn await_peer_close(&mut self) {
        while let Some(Ok(m)) = self.stream.next().await {
            if matches!(m, Message::Close(_)) {
                break;
            }
        }
    }
}
