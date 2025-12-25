use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use super::qos::QoS;

/// Inbound envelope: lazy data parsing (RawValue).
/// Rule: inbound is consumed (no Clone). Outbound is produced.
#[derive(Debug, Deserialize)]
pub struct Envelope {
    pub svc: String,
    #[serde(rename = "type")]
    pub msg_type: String,
    #[serde(default)]
    pub room: Option<String>,
    #[serde(default)]
    pub data: Option<Box<RawValue>>,
    #[serde(default)]
    pub seq: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OutgoingText {
    pub svc: String,
    #[serde(rename = "type")]
    pub msg_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room: Option<String>,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone)]
pub enum Payload {
    TextJson(OutgoingText),
    Binary(Bytes),
    Utf8Bytes(Bytes),
}

#[derive(Debug, Clone)]
pub struct Outgoing {
    pub qos: QoS,
    pub payload: Payload,
}
