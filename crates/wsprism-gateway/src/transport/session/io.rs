//! The transport seam of a session (T0, design §4.1).
//!
//! The session loop, policy, dispatch and teardown are written against these
//! two halves, never against a concrete socket type. A transport (today only
//! WebSocket, `ws_io`) decodes inbound messages into [`Inbound`] and turns
//! engine frames ([`OutFrame`]) into its own wire format.
//!
//! Split into an inbound and an outbound half because the loop waits on
//! both at once (`select!`), so they must be borrowable independently.
//!
//! Capabilities: every transport offers an ordered, reliable channel (what
//! `send` is). An unreliable datagram channel (WebTransport) is added with
//! the Hot slot (T0-3); a transport without one maps unreliable frames onto
//! the ordered channel, so services never depend on which one they got.

use std::future::Future;
use std::time::Duration;

use wsprism_protocol::error::WsPrismError;

use crate::realtime::OutFrame;
use crate::transport::codec::Inbound;

/// Why reading the next inbound message failed.
#[derive(Debug)]
pub(crate) enum RecvError {
    /// The message exceeded the tenant frame limit enforced by the codec (S2).
    TooBig,
    /// The connection is unusable (I/O or protocol error below the envelope).
    Broken,
    /// A complete message arrived but is not a valid envelope / hot frame.
    Decode(WsPrismError),
}

/// The outbound half (writes in order; slow-consumer timeouts are applied by
/// the caller).
pub(crate) trait SessionTx: Send {
    /// Write one frame on the ordered, reliable channel.
    fn send(&mut self, frame: OutFrame) -> impl Future<Output = Result<(), ()>> + Send;

    /// Queue one frame for writing without flushing. A burst of `feed`s
    /// followed by one `flush` lets the transport coalesce writes/syscalls
    /// with no session-side buffer (design: performance-architecture §3.4).
    fn feed(&mut self, frame: OutFrame) -> impl Future<Output = Result<(), ()>> + Send;

    /// Write out everything fed so far.
    fn flush(&mut self) -> impl Future<Output = Result<(), ()>> + Send;

    /// Send the transport's close signal with an RFC 6455-style code and
    /// reason (other transports carry the same codes, see PROTOCOL.md).
    fn send_close(
        &mut self,
        code: u16,
        reason: String,
    ) -> impl Future<Output = Result<(), ()>> + Send;

    /// The peer closed first: flush whatever close reply the transport owes.
    fn finish_peer_close(&mut self) -> impl Future<Output = ()> + Send;

    /// How long to keep the connection open after closing because a message
    /// was too big, so the peer can still read the close (see S2). Zero if
    /// the transport can discard unread input.
    fn too_big_linger(&self) -> Duration;
}

/// The inbound half.
pub(crate) trait SessionRx: Send {
    /// The next decoded message; `None` when the peer's stream has ended.
    fn recv(&mut self) -> impl Future<Output = Option<Result<Inbound, RecvError>>> + Send;

    /// After our close: read until the peer's close arrives (or the stream
    /// ends), so the teardown is clean. Bounded by the caller.
    fn await_peer_close(&mut self) -> impl Future<Output = ()> + Send;
}
