//! Decode-once codec for the transport layer.
//!
//! - Text frames => Envelope (lazy `RawValue` for data)
//! - Binary frames => HotFrame (panic-free bytes::Buf parsing)
//! - Ping/Pong/Close are surfaced for lifecycle management

use crate::transport::subprotocol::ProtocolVersion;
use axum::extract::ws::Message;
use wsprism_protocol::{
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
    /// WS ping (the WebSocket layer answers it).
    Ping,
    /// WS pong (keep-alive answer).
    Pong,
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
        Message::Ping(_) => Ok(Inbound::Ping),
        Message::Pong(_) => Ok(Inbound::Pong),
        Message::Close(_) => Ok(Inbound::Close),
    }
}
