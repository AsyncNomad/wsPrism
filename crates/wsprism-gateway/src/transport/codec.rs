//! Decode-once codec for the transport layer.
//!
//! - Text frames => Envelope (lazy `RawValue` for data)
//! - Binary frames => HotFrame (panic-free bytes::Buf parsing)
//! - Ping/Pong/Close are surfaced for lifecycle management

use crate::transport::subprotocol::ProtocolVersion;
use axum::extract::ws::Message;
use wsprism_core::{
    error::Result,
    protocol::{hot, text},
};

#[derive(Debug)]
pub enum Inbound {
    /// Ext lane JSON envelope with captured byte length (for policy).
    Text {
        env: text::Envelope,
        bytes_len: usize,
    },
    /// Hot lane binary frame with captured byte length (for policy).
    Hot {
        frame: hot::HotFrame,
        bytes_len: usize,
    },
    /// WS ping payload.
    Ping(Vec<u8>),
    /// WS pong payload.
    Pong(Vec<u8>),
    /// WS close control frame (no payload surfaced).
    Close,
}

/// Decode one WebSocket message for a session speaking `protocol`.
pub fn decode(msg: Message, protocol: ProtocolVersion) -> Result<Inbound> {
    match msg {
        Message::Text(s) => {
            let bytes_len = s.len();
            let env = match protocol {
                ProtocolVersion::V1 => text::decode_envelope(&s)?,
                ProtocolVersion::V2 => text::decode_envelope_v2(&s)?,
            };
            Ok(Inbound::Text { env, bytes_len })
        }
        Message::Binary(b) => {
            let bytes_len = b.len();
            let frame = match protocol {
                ProtocolVersion::V1 => hot::decode_hot_frame(bytes::Bytes::from(b))?,
                ProtocolVersion::V2 => hot::decode_hot_frame_v2(bytes::Bytes::from(b))?,
            };
            Ok(Inbound::Hot { frame, bytes_len })
        }
        Message::Ping(v) => Ok(Inbound::Ping(v)),
        Message::Pong(v) => Ok(Inbound::Pong(v)),
        Message::Close(_) => Ok(Inbound::Close),
    }
}

/// Same as [`decode`] for a tungstenite message (the experimental raw
/// transport reads with tungstenite directly, ADR 0007 stage 1).
pub fn decode_tungstenite(
    msg: tokio_tungstenite::tungstenite::Message,
    protocol: ProtocolVersion,
) -> Result<Inbound> {
    use tokio_tungstenite::tungstenite::Message as T;
    match msg {
        T::Text(s) => decode(Message::Text(s), protocol),
        T::Binary(b) => decode(Message::Binary(b), protocol),
        T::Ping(v) => Ok(Inbound::Ping(v)),
        T::Pong(v) => Ok(Inbound::Pong(v)),
        T::Close(_) => Ok(Inbound::Close),
        // Raw frames only appear when writing; never yielded by reads.
        T::Frame(_) => Ok(Inbound::Pong(Vec::new())),
    }
}
