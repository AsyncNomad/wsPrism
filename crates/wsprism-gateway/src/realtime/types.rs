use axum::extract::ws::Message;
use bytes::Bytes;
use serde_json::Value;

use wsprism_core::error::{Result, WsPrismError};

/// Quality-of-Service strategy for outgoing delivery.
#[derive(Debug, Clone)]
pub enum QoS {
    /// Latency-critical: do not await; if the user's queue is full, drop.
    Lossy,
    /// Reliability-critical: attempt delivery and optionally time out.
    Reliable { timeout_ms: u64 },
}

impl Default for QoS {
    fn default() -> Self {
        QoS::Lossy
    }
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
    /// NOTE: axum::Message::Binary requires Vec<u8>, so Binary path clones into Vec.
    pub fn to_ws_message(&self) -> Message {
        match self {
            PreparedMsg::Text(s) => Message::Text(s.clone()),
            PreparedMsg::Binary(b) => Message::Binary(b.to_vec()),
        }
    }
}
