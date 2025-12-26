//! JSON test vector loader shared by Hot/Ext tests.

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct TestVector {
    pub description: String,
    pub frame: FrameData,
    #[serde(default)]
    pub expect: Option<serde_json::Value>,
    #[serde(default)]
    pub expect_error: Option<ExpectError>,
}

#[derive(Debug, Deserialize)]
pub struct ExpectError {
    pub code: String,
}

#[derive(Debug, Deserialize)]
pub struct FrameData {
    pub encoding: String,
    pub data: String,
}

impl FrameData {
    pub fn decode(&self) -> Vec<u8> {
        match self.encoding.as_str() {
            "base64" => base64::decode(&self.data).expect("invalid base64 in test vector"),
            "hex" => hex::decode(&self.data).expect("invalid hex in test vector"),
            other => panic!("unsupported encoding: {other}"),
        }
    }
}
