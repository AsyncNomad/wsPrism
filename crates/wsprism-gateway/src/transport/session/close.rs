//! How a session ends: the `Exit` reasons and the matching teardown,
//! written against the transport seam (`io`).

use tokio::sync::mpsc;
use tokio::time::{timeout, Duration, Instant};

use super::io::{SessionRx, SessionTx};
use crate::realtime::{close_code, CloseSpec, OutFrame};
use crate::transport::subprotocol::ProtocolVersion;
use wsprism_protocol::protocol::close::wire_close_code;

/// Reason of the oversized-message close (S2; v2 maps it to 4009).
const TOO_BIG_REASON: &str = "message too big";

/// How long to wait for the peer's close after sending ours (C3).
const CLOSE_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(1);

/// Why a session's main loop ended, i.e. how its connection must be closed.
pub(super) enum Exit {
    /// The peer closed (or the stream ended): just flush our close reply.
    Peer,
    /// The connection is unusable (write stalled or read failed): drop it.
    Abort,
    /// Server-initiated close with a reason, optionally preceded by a notice.
    Close(CloseSpec),
    /// Oversized frame (S2): close 1009, then linger (input can't be drained).
    TooBig,
}

/// Tear the connection down according to `exit`.
pub(super) async fn finish<Tx: SessionTx, Rx: SessionRx>(
    exit: Exit,
    tx: &mut Tx,
    rx: &mut Rx,
    out_rx: &mut mpsc::Receiver<OutFrame>,
    protocol: ProtocolVersion,
    writer_timeout: Duration,
) {
    match exit {
        Exit::Peer => {
            let _ = timeout(writer_timeout, tx.finish_peer_close()).await;
        }
        Exit::Abort => {}
        Exit::Close(spec) => close_session(tx, rx, out_rx, spec, protocol, writer_timeout).await,
        Exit::TooBig => {
            let code = wire_close_code(protocol, close_code::MESSAGE_TOO_BIG, TOO_BIG_REASON);
            let _ = timeout(writer_timeout, tx.send_close(code, TOO_BIG_REASON.into())).await;
            tokio::time::sleep(tx.too_big_linger()).await;
        }
    }
}

/// Close politely (C3): flush what is already queued (so the notice is not
/// reordered ahead of earlier messages), send the notice and the close,
/// then wait briefly for the peer's close so the teardown is clean.
/// Writes share one `writer_timeout` budget; a stalled peer is abandoned.
pub(super) async fn close_session<Tx: SessionTx, Rx: SessionRx>(
    tx: &mut Tx,
    rx: &mut Rx,
    out_rx: &mut mpsc::Receiver<OutFrame>,
    spec: CloseSpec,
    protocol: ProtocolVersion,
    writer_timeout: Duration,
) {
    out_rx.close(); // no new messages from here on
    let mut outgoing = Vec::new();
    while let Ok(frame) = out_rx.try_recv() {
        outgoing.push(frame);
    }
    outgoing.extend(spec.notice.map(OutFrame::text));

    let deadline = Instant::now() + writer_timeout;
    for frame in outgoing {
        if !matches!(
            tokio::time::timeout_at(deadline, tx.send(frame)).await,
            Ok(Ok(()))
        ) {
            return; // stalled or broken peer: give up
        }
    }
    if !matches!(
        tokio::time::timeout_at(
            deadline,
            tx.send_close(
                wire_close_code(protocol, spec.code, &spec.reason),
                spec.reason
            ),
        )
        .await,
        Ok(Ok(()))
    ) {
        return;
    }

    let _ = timeout(CLOSE_HANDSHAKE_TIMEOUT, rx.await_peer_close()).await;
}
