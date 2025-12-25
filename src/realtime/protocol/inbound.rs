use bytes::Bytes;
use crate::error::{AppError, Result};

use super::envelope::Envelope;

#[derive(Debug)]
pub enum Inbound {
    Text(Envelope),
    Binary(BinaryFrame),
}

/// Generic binary frame: header + opaque payload.
/// Format: [svc_id u8][opcode u8][payload bytes...]
#[derive(Debug, Clone)]
pub struct BinaryFrame {
    pub svc_id: u8,
    pub opcode: u8,
    pub payload: Bytes,
}

impl BinaryFrame {
    pub fn parse(buf: Bytes) -> Result<Self> {
        if buf.len() < 2 {
            return Err(AppError::BadRequest("binary frame too short".into()));
        }
        let svc_id = buf[0];
        let opcode = buf[1];
        let payload = buf.slice(2..);
        Ok(Self { svc_id, opcode, payload })
    }
}
