use axum::extract::ws::Message;
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
}

/// Application-level outgoing message.
#[derive(Debug, Clone)]
pub struct Outgoing {
    pub qos: QoS,
    pub payload: Payload,
}

/// Prepared message cached for broadcasting (serialize once, send N times).
#[derive(Debug, Clone)]
pub enum PreparedMsg {
    Text(String),
    Binary(Bytes),
}

impl PreparedMsg {
    pub fn prepare(out: &Outgoing) -> Result<Self> {
        match &out.payload {
            Payload::TextJson(v) => {
                let s = serde_json::to_string(v)
                    .map_err(|e| WsPrismError::BadRequest(format!("json encode failed: {e}")))?;
                Ok(PreparedMsg::Text(s))
            }
            Payload::Utf8Bytes(b) => {
                // validate UTF-8 once; send as Text
                let s = std::str::from_utf8(b)
                    .map_err(|e| WsPrismError::BadRequest(format!("utf8 invalid: {e}")))?
                    .to_owned();
                Ok(PreparedMsg::Text(s))
            }
            Payload::Binary(b) => Ok(PreparedMsg::Binary(b.clone())),
        }
    }

    /// Convert to axum::ws::Message for transport.
    /// NOTE: axum::Message::Binary requires `Vec<u8>`, so Binary path clones into Vec.
    pub fn to_ws_message(&self) -> Message {
        match self {
            PreparedMsg::Text(s) => Message::Text(s.clone()),
            PreparedMsg::Binary(b) => Message::Binary(b.to_vec()),
        }
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
