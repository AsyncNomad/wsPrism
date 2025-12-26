//! Ext Lane envelope (JSON).
//!
//! The core stores `data` as `RawValue` to enable lazy parsing by services.

use serde::Deserialize;
use serde_json::value::RawValue;

/// Ext Lane envelope (Text frame).
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
