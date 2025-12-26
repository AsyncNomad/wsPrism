//! Decode-once codec for transport layer.
//!
//! We keep decoding minimal:
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
    Text(text::Envelope),
    Hot(hot::HotFrame),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close,
    Other,
}

pub fn decode(msg: Message) -> Result<Inbound> {
    match msg {
        Message::Text(s) => {
            // Decode-once: parse only the Envelope header fields; `data` stays RawValue.
            let env: text::Envelope = serde_json::from_str(&s)
                .map_err(|e| WsPrismError::BadRequest(format!("invalid envelope json: {e}")))?;
            Ok(Inbound::Text(env))
        }
        Message::Binary(b) => {
            let frame = hot::decode_hot_frame(bytes::Bytes::from(b))?;
            Ok(Inbound::Hot(frame))
        }
        Message::Ping(v) => Ok(Inbound::Ping(v)),
        Message::Pong(v) => Ok(Inbound::Pong(v)),
        Message::Close(_) => Ok(Inbound::Close),
        _ => Ok(Inbound::Other),
    }
}
