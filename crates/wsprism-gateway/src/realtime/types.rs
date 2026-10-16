use std::sync::Arc;

use bytes::Bytes;
use serde_json::Value;

use wsprism_core::error::{Result, WsPrismError};

/// Quality-of-Service strategy for outgoing delivery. Neither variant ever
/// blocks the publisher; they differ in what happens when a recipient's
/// queue is full (see C1 in docs/HARDENING.md).
#[derive(Debug, Clone, Default)]
pub enum QoS {
    /// Latency-first: drop the message for that recipient (latest state wins).
    #[default]
    Lossy,
    /// Gap-free: the recipient gets every message in order, or is closed as a
    /// slow consumer (`slow_consumer`, 1008) so it knows to resync.
    Reliable,
}

/// Outgoing payload variants.
#[derive(Debug, Clone)]
pub enum Payload {
    /// JSON value serialized to text.
    TextJson(Value),
    /// Raw UTF-8 bytes. (Still sent as Text in WS transport.)
    Utf8Bytes(Bytes),
    /// Raw binary bytes.
    Binary(Bytes),
    /// A Hot frame, pre-encoded for both protocol versions.
    Hot(Arc<HotOut>),
}

/// A Hot frame on its way out, encoded once per protocol version so every
/// recipient of a version shares the same bytes (ADR 0006 R3, R7).
#[derive(Debug, PartialEq, Eq)]
pub struct HotOut {
    /// v1 recipients: the payload only (what the v1 echo always sent).
    pub v1: Bytes,
    /// v2 recipients: the full v2 frame with `chan` and `sender`.
    pub v2: Bytes,
    /// `(sender, svc, key)` for keyed frames: delivered through the
    /// recipient's Hot slot (latest value wins) instead of its queue.
    pub slot_key: Option<(u32, u8, u32)>,
}

/// Application-level outgoing message.
#[derive(Debug, Clone)]
pub struct Outgoing {
    pub qos: QoS,
    pub payload: Payload,
}

/// UTF-8 text in a shared, cheaply clonable buffer. Valid UTF-8 is
/// guaranteed by construction (checked once for untrusted bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Utf8Text(Bytes);

impl Utf8Text {
    /// Validate `bytes` as UTF-8 (once) and wrap them without copying.
    pub fn try_from_bytes(bytes: Bytes) -> Result<Self> {
        std::str::from_utf8(&bytes)
            .map_err(|e| WsPrismError::BadRequest(format!("utf8 invalid: {e}")))?;
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl From<String> for Utf8Text {
    fn from(s: String) -> Self {
        Self(Bytes::from(s))
    }
}

/// A message queued for one session's socket.
///
/// Transport-agnostic: the realtime core and services never see the
/// WebSocket library's message type (the transport converts at its edge,
/// `transport::session::ws_io`). Cloning is a reference-count bump, so a
/// fan-out to N sessions does not copy the payload N times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutFrame {
    Text(Utf8Text),
    Binary(Bytes),
    /// Keep-alive ping (empty payload).
    Ping,
    /// Hot frame; the transport picks the bytes for the session's version.
    Hot(Arc<HotOut>),
}

impl OutFrame {
    /// Encode a payload once, ready to be cloned to every recipient.
    pub fn from_payload(payload: &Payload) -> Result<Self> {
        match payload {
            Payload::TextJson(v) => {
                let s = serde_json::to_string(v)
                    .map_err(|e| WsPrismError::BadRequest(format!("json encode failed: {e}")))?;
                Ok(OutFrame::Text(s.into()))
            }
            Payload::Utf8Bytes(b) => Ok(OutFrame::Text(Utf8Text::try_from_bytes(b.clone())?)),
            Payload::Binary(b) => Ok(OutFrame::Binary(b.clone())),
            Payload::Hot(h) => Ok(OutFrame::Hot(Arc::clone(h))),
        }
    }

    /// A text frame from an owned string (no validation needed).
    pub fn text(s: String) -> Self {
        OutFrame::Text(s.into())
    }
}

/// RFC 6455 close codes used by the gateway.
pub mod close_code {
    /// Normal closure (e.g. idle timeout).
    pub const NORMAL: u16 = 1000;
    /// Server going away (draining / shutdown).
    pub const GOING_AWAY: u16 = 1001;
    /// Malformed input the gateway cannot decode.
    pub const PROTOCOL_ERROR: u16 = 1002;
    /// Policy violation (kicked, replaced, session limit, policy close).
    pub const POLICY: u16 = 1008;
    /// Message exceeded the tenant frame limit.
    pub const MESSAGE_TOO_BIG: u16 = 1009;
    /// Temporary capacity problem; the client may retry later.
    pub const TRY_AGAIN_LATER: u16 = 1013;
}

/// How a session should be closed: close frame code/reason, optionally
/// preceded by a `sys.*` notice explaining why.
#[derive(Debug, Clone)]
pub struct CloseSpec {
    pub code: u16,
    /// Short machine-readable reason (sent in the close frame, <= 123 bytes).
    pub reason: String,
    /// Optional text message (e.g. `sys.error` / `sys.kicked`) sent first.
    pub notice: Option<String>,
}

impl CloseSpec {
    pub fn new(code: u16, reason: impl Into<String>, notice: Option<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
            notice,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn invalid_utf8_is_rejected_once_at_construction() {
        assert!(Utf8Text::try_from_bytes(Bytes::from_static(&[0xff, 0xfe])).is_err());
        let t = Utf8Text::try_from_bytes(Bytes::from_static("héllo".as_bytes())).unwrap();
        assert_eq!(t.as_bytes(), "héllo".as_bytes());
    }

    #[test]
    fn fanout_clones_share_the_buffer() {
        let frame = OutFrame::from_payload(&Payload::Binary(Bytes::from(vec![1u8; 64]))).unwrap();
        let (OutFrame::Binary(a), OutFrame::Binary(b)) = (frame.clone(), frame) else {
            panic!("binary payload must stay binary");
        };
        assert_eq!(a.as_ptr(), b.as_ptr(), "clone must not copy the payload");
    }
}
