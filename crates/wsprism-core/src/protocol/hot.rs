//! Hot Lane binary frame parsing (panic-free).
//!
//! Parsing rules:
//! - Never index (`buf[0]`); always use `Buf` and `remaining()` checks.
//! - Never `unwrap()` / `expect()` / `panic!()` in production paths.
//! - Validate header lengths before reading optional fields.

use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::error::{Result, WsPrismError};

/// Hot Lane flag: seq (u32) is present.
pub const HOT_FLAG_SEQ_PRESENT: u8 = 0x01;

/// Parsed Hot Lane frame.
#[derive(Debug, Clone)]
pub struct HotFrame {
    /// Protocol version (must be 1).
    pub v: u8,
    /// Service id (routes to native BinaryService).
    pub svc_id: u8,
    /// Opcode within that service.
    pub opcode: u8,
    /// Feature flags (u8).
    pub flags: u8,
    /// Optional sequence number.
    pub seq: Option<u32>,
    /// v2: target room id (`None` for v1, which targets the active room).
    pub chan: Option<u32>,
    /// v2: conflation key (latest value per `(sender, svc, key)` wins).
    pub key: Option<u32>,
    /// Opaque payload (zero-copy).
    pub payload: Bytes,
}

/// Decode a Hot Lane frame from bytes.
///
/// Defensive against malformed input; returns structured errors instead of
/// panicking on short buffers or unsupported versions.
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
        chan: None,
        key: None,
        payload,
    })
}

// ---- Protocol v2 (ADR 0006 §2-§3, R7) ----

/// v2 binary frame kind: Hot state frame.
pub const KIND_HOT: u8 = 0x01;
/// v2 binary frame kind: batch of frames (server -> client only).
pub const KIND_BATCH: u8 = 0x02;
/// v2 Hot flag: `key` (u32) present.
pub const HOT_V2_FLAG_KEY: u8 = 0x01;
/// v2 Hot flag: `seq` (u32) present.
pub const HOT_V2_FLAG_SEQ: u8 = 0x02;
/// v2 Hot flag: `sender` (u32) present — set by the server only.
pub const HOT_V2_FLAG_SENDER: u8 = 0x04;
/// Flags a client may set.
const HOT_V2_CLIENT_FLAGS: u8 = HOT_V2_FLAG_KEY | HOT_V2_FLAG_SEQ;
/// kind + flags + chan + svc + op.
const HOT_V2_HEADER: usize = 1 + 1 + 4 + 1 + 1;

fn read_u32(buf: &mut Bytes, what: &str) -> Result<u32> {
    if buf.remaining() < 4 {
        return Err(WsPrismError::BadRequest(format!(
            "hot frame: {what} flag set but missing u32"
        )));
    }
    Ok(buf.get_u32_le())
}

/// Decode a client's v2 Hot frame:
/// `[kind=0x01][flags][chan u32][svc][op][key u32?][seq u32?][payload]`.
///
/// Rejects other kinds (batch is server -> client only; 0x00 is reserved),
/// the `sender` flag (the server stamps it) and unknown flag bits (their
/// fields' layout is unknown).
pub fn decode_hot_frame_v2(mut buf: Bytes) -> Result<HotFrame> {
    if buf.remaining() < HOT_V2_HEADER {
        return Err(WsPrismError::BadRequest("hot frame too short".into()));
    }
    let kind = buf.get_u8();
    if kind != KIND_HOT {
        return Err(WsPrismError::BadRequest(format!(
            "unexpected binary frame kind {kind:#04x}"
        )));
    }
    let flags = buf.get_u8();
    if flags & !HOT_V2_CLIENT_FLAGS != 0 {
        return Err(WsPrismError::BadRequest(format!(
            "hot frame: flags {flags:#04x} not allowed from clients"
        )));
    }
    let chan = buf.get_u32_le();
    let svc_id = buf.get_u8();
    let opcode = buf.get_u8();
    let key = if flags & HOT_V2_FLAG_KEY != 0 {
        Some(read_u32(&mut buf, "key")?)
    } else {
        None
    };
    let seq = if flags & HOT_V2_FLAG_SEQ != 0 {
        Some(read_u32(&mut buf, "seq")?)
    } else {
        None
    };
    let payload = buf.copy_to_bytes(buf.remaining());
    Ok(HotFrame {
        v: 2,
        svc_id,
        opcode,
        flags,
        seq,
        chan: Some(chan),
        key,
        payload,
    })
}

/// A Hot frame as the server sends it in v2 (the same bytes go to every
/// recipient: `chan` is a room id, not a per-connection alias).
#[derive(Debug, Clone)]
pub struct HotOutV2 {
    pub chan: u32,
    pub svc_id: u8,
    pub opcode: u8,
    pub key: Option<u32>,
    pub seq: Option<u32>,
    /// The publisher's member id in the room.
    pub sender: u32,
    pub payload: Bytes,
}

/// Encode a server -> client v2 Hot frame (one allocation).
pub fn encode_hot_frame_v2(f: &HotOutV2) -> Bytes {
    let mut flags = HOT_V2_FLAG_SENDER;
    let mut len = HOT_V2_HEADER + 4 + f.payload.len();
    if f.key.is_some() {
        flags |= HOT_V2_FLAG_KEY;
        len += 4;
    }
    if f.seq.is_some() {
        flags |= HOT_V2_FLAG_SEQ;
        len += 4;
    }
    let mut b = BytesMut::with_capacity(len);
    b.put_u8(KIND_HOT);
    b.put_u8(flags);
    b.put_u32_le(f.chan);
    b.put_u8(f.svc_id);
    b.put_u8(f.opcode);
    if let Some(k) = f.key {
        b.put_u32_le(k);
    }
    if let Some(s) = f.seq {
        b.put_u32_le(s);
    }
    b.put_u32_le(f.sender);
    b.extend_from_slice(&f.payload);
    b.freeze()
}
