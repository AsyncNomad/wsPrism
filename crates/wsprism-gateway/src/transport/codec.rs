//! Decode-once codec for transport layer (Sprint 1/2).
//!
//! - Text frames => Envelope (lazy `RawValue` for data)
//! - Binary frames => HotFrame (panic-free bytes::Buf parsing)
//! - Ping/Pong/Close are surfaced for lifecycle management

use axum::extract::ws::Message;
use wsprism_core::{
    error::{Result, WsPrismError},
    protocol::{hot, text},
};

#[derive(Debug)]
pub enum Inbound {
    Text { env: text::Envelope, bytes_len: usize },
    Hot { frame: hot::HotFrame, bytes_len: usize },
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close,
}

pub fn decode(msg: Message) -> Result<Inbound> {
    match msg {
        Message::Text(s) => {
            let bytes_len = s.as_bytes().len();
            let env: text::Envelope = serde_json::from_str(&s)
                .map_err(|e| WsPrismError::BadRequest(format!("invalid envelope json: {e}")))?;
            Ok(Inbound::Text { env, bytes_len })
        }
        Message::Binary(b) => {
            let bytes_len = b.len();
            let frame = hot::decode_hot_frame(bytes::Bytes::from(b))?;
            Ok(Inbound::Hot { frame, bytes_len })
        }
        Message::Ping(v) => Ok(Inbound::Ping(v)),
        Message::Pong(v) => Ok(Inbound::Pong(v)),
        Message::Close(_) => Ok(Inbound::Close),
    }
}
