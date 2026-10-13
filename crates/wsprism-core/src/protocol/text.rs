//! Ext Lane envelope (JSON).
//!
//! The core stores `data` as `RawValue` to enable lazy parsing by downstream
//! services and plugins. Unknown fields are rejected to keep the contract
//! strict and predictable.

use serde::Deserialize;
use serde_json::value::RawValue;

use crate::error::{Result, WsPrismError};

/// Ext Lane protocol version accepted by this core.
pub const EXT_PROTOCOL_VERSION: u8 = 1;

/// Ext Lane envelope (Text frame).
///
/// This is the canonical JSON structure parsed on the server. Services may
/// choose to further deserialize `data` depending on `svc`/`type`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    /// Protocol version.
    pub v: u8,
    /// Service name (e.g., "chat").
    pub svc: String,
    /// Message type (field name is `type` in JSON).
    #[serde(rename = "type")]
    pub msg_type: String,
    /// Feature flags bitmask.
    #[serde(default)]
    pub flags: u32,
    /// Optional sequence number.
    #[serde(default)]
    pub seq: Option<u64>,
    /// Optional room id.
    #[serde(default)]
    pub room: Option<String>,
    /// Optional payload, stored as raw JSON (lazy parsing).
    #[serde(default)]
    pub data: Option<Box<RawValue>>,
}

/// Parse and validate an Ext Lane envelope from a text frame.
///
/// Rejects malformed JSON / unknown fields (`BadRequest`) and any protocol
/// version other than [`EXT_PROTOCOL_VERSION`] (`UnsupportedVersion`).
pub fn decode_envelope(s: &str) -> Result<Envelope> {
    let env: Envelope = serde_json::from_str(s)
        .map_err(|e| WsPrismError::BadRequest(format!("invalid envelope json: {e}")))?;
    if env.v != EXT_PROTOCOL_VERSION {
        return Err(WsPrismError::UnsupportedVersion);
    }
    Ok(env)
}
