//! Ext Lane envelope (JSON).
//!
//! The core stores `data` as `RawValue` to enable lazy parsing by downstream
//! services and plugins. Protocol v1 rejects unknown fields; v2 ignores them
//! so clients and servers can be upgraded independently (ADR 0006 §5).

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

/// Protocol v2 wire form: unknown fields are ignored, and `v` / `flags` are
/// optional and ignored (the version is negotiated per connection).
#[derive(Deserialize)]
struct EnvelopeV2 {
    svc: String,
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(default)]
    seq: Option<u64>,
    #[serde(default)]
    room: Option<String>,
    #[serde(default)]
    data: Option<Box<RawValue>>,
}

/// Parse a protocol v2 Ext envelope into the canonical [`Envelope`]
/// (`v` = [`EXT_PROTOCOL_VERSION`], `flags` = 0). Malformed JSON or a
/// missing `svc` / `type` is a `BadRequest`.
pub fn decode_envelope_v2(s: &str) -> Result<Envelope> {
    let e: EnvelopeV2 = serde_json::from_str(s)
        .map_err(|e| WsPrismError::BadRequest(format!("invalid envelope json: {e}")))?;
    Ok(Envelope {
        v: EXT_PROTOCOL_VERSION,
        svc: e.svc,
        msg_type: e.msg_type,
        flags: 0,
        seq: e.seq,
        room: e.room,
        data: e.data,
    })
}
