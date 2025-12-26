//! Hot Lane binary frame parsing (panic-free).
//!
//! Parsing rules:
//! - Never index (`buf[0]`) — always use `Buf` and `remaining()` checks.
//! - Never `unwrap()` / `expect()` / `panic!()` in production paths.

use bytes::Buf;
use bytes::Bytes;

use crate::error::{Result, WsPrismError};

/// Hot Lane flag: seq (u32) is present.
pub const HOT_FLAG_SEQ_PRESENT: u8 = 0x01;

/// Parsed Hot Lane frame.
#[derive(Debug, Clone)]
pub struct HotFrame {
    /// Protocol version.
    pub v: u8,
    /// Service id (routes to native BinaryService).
    pub svc_id: u8,
    /// Opcode within that service.
    pub opcode: u8,
    /// Feature flags (u8).
    pub flags: u8,
    /// Optional sequence number.
    pub seq: Option<u32>,
    /// Opaque payload (zero-copy).
    pub payload: Bytes,
}

/// Decode a Hot Lane frame from bytes.
pub fn decode_hot_frame(mut buf: Bytes) -> Result<HotFrame> {
    // Minimum header: v, svc_id, opcode, flags
    if buf.remaining() < 4 {
        return Err(WsPrismError::BadRequest("hot frame too short".into()));
    }

    let v = buf.get_u8();
    if v != 1 {
        return Err(WsPrismError::UnsupportedVersion);
    }

    let svc_id = buf.get_u8();
    let opcode = buf.get_u8();
    let flags = buf.get_u8();

    let seq = if (flags & HOT_FLAG_SEQ_PRESENT) != 0 {
        if buf.remaining() < 4 {
            return Err(WsPrismError::BadRequest(
                "seq flag set but missing u32".into(),
            ));
        }
        Some(buf.get_u32_le())
    } else {
        None
    };

    // Remaining bytes are payload.
    let payload = buf.copy_to_bytes(buf.remaining());

    Ok(HotFrame {
        v,
        svc_id,
        opcode,
        flags,
        seq,
        payload,
    })
}
