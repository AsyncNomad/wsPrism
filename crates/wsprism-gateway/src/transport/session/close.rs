//! How a session ends: the `Exit` reasons and the matching socket teardown.

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration, Instant};

use crate::realtime::{close_code, CloseSpec};

/// How long to keep the socket open after sending close 1009 (see S2).
const CLOSE_LINGER: Duration = Duration::from_millis(500);

/// How long to wait for the peer's close frame after sending ours (C3).
const CLOSE_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(1);

/// Why a session's main loop ended, i.e. how its socket must be closed.
pub(super) enum Exit {
    /// The peer closed (or the stream ended): just flush our close reply.
    Peer,
    /// The socket is unusable (write stalled or read failed): drop it.
    Abort,
    /// Server-initiated close with a reason, optionally preceded by a notice.
    Close(CloseSpec),
    /// Oversized frame (S2): close 1009, then linger (input can't be drained).
    TooBig,
}

/// True if the read failed because the frame/message exceeded the configured
/// codec limit (see `max_message_size` in `ws_upgrade`).
pub(super) fn is_message_too_long(e: axum::Error) -> bool {
    matches!(
        e.into_inner().downcast_ref::<tungstenite::Error>(),
        Some(tungstenite::Error::Capacity(
            tungstenite::error::CapacityError::MessageTooLong { .. }
        ))
    )
}

/// Tear the socket down according to `exit`.
pub(super) async fn finish(
    exit: Exit,
    ws_tx: &mut SplitSink<WebSocket, Message>,
    ws_rx: &mut SplitStream<WebSocket>,
    out_rx: &mut mpsc::Receiver<Message>,
    writer_timeout: Duration,
) {
    match exit {
        Exit::Peer => {
            // Flush tungstenite's queued close reply.
            let _ = timeout(writer_timeout, ws_tx.close()).await;
        }
        Exit::Abort => {}
        Exit::Close(spec) => close_session(ws_tx, ws_rx, out_rx, spec, writer_timeout).await,
        Exit::TooBig => {
            let close = Message::Close(Some(CloseFrame {
                code: close_code::MESSAGE_TOO_BIG,
                reason: "message too big".into(),
            }));
            let _ = timeout(writer_timeout, ws_tx.send(close)).await;
            // The oversized payload is still unread in the kernel buffer
            // (tungstenite rejects on the header and cannot skip the body).
            // Closing now would make the kernel send RST, and the peer may
            // discard our close frame. Linger briefly so it can read it first.
            tokio::time::sleep(CLOSE_LINGER).await;
        }
    }
}

/// Close politely (C3): flush what is already queued (so the notice is not
/// reordered ahead of earlier messages), send the notice and the close frame,
/// then wait briefly for the peer's close so the TCP teardown is clean.
/// Writes share one `writer_timeout` budget; a stalled peer is abandoned.
pub(super) async fn close_session(
    ws_tx: &mut SplitSink<WebSocket, Message>,
    ws_rx: &mut SplitStream<WebSocket>,
    out_rx: &mut mpsc::Receiver<Message>,
    spec: CloseSpec,
    writer_timeout: Duration,
) {
    out_rx.close(); // no new messages from here on
    let mut outgoing = Vec::new();
    while let Ok(m) = out_rx.try_recv() {
        if !matches!(m, Message::Close(_)) {
            outgoing.push(m);
        }
    }
    outgoing.extend(spec.notice.map(Message::Text));
    outgoing.push(Message::Close(Some(CloseFrame {
        code: spec.code,
        reason: spec.reason.into(),
    })));

    let deadline = Instant::now() + writer_timeout;
    for m in outgoing {
        if !matches!(
            tokio::time::timeout_at(deadline, ws_tx.send(m)).await,
            Ok(Ok(()))
        ) {
            return; // stalled or broken peer: give up
        }
    }

    let _ = timeout(CLOSE_HANDSHAKE_TIMEOUT, async {
        while let Some(Ok(m)) = ws_rx.next().await {
            if matches!(m, Message::Close(_)) {
                break;
            }
        }
    })
    .await;
}
