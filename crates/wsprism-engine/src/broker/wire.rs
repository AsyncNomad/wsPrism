//! The node-to-node frame format (ADR 0011 §7). Brokers that leave the
//! process (Redis, ...) send room frames in this form.
//!
//! ```text
//! u8  version (1)
//! u8  kind: 1 text, 2 binary, 3 hot, 4 kept text (history, ADR 0011 §3)
//! u16 origin node slot
//! u8  qos: 0 lossy, 1 reliable
//! u16 room key length, room key (UTF-8, `tenant::room`)
//! u16 except length (0xFFFF = none), except session key
//! body:
//!   text / binary: the rest
//!   kept text: u8 n, n ASCII digits (the offset), the text — the offset
//!     comes right before the text so the broker (a Redis script) can put
//!     it in between the two parts from `encode_kept`
//!   hot: u32 v1 length, v1 payload, u32 v2 length, v2 frame,
//!        u8 keyed, [u32 sender, u8 svc, u32 key]
//! ```
//!
//! All integers are little-endian. Rolling upgrades: a node sends the
//! highest version every node reads; unknown versions are dropped (and
//! logged by the broker) — deploy readers first, writers after.

use std::fmt;
use std::sync::Arc;

use bytes::{Buf, BufMut, Bytes, BytesMut};

use super::BrokerFrame;
use crate::realtime::types::{HotOut, OutFrame, QoS, Utf8Text};

/// The version this build writes and reads.
pub const VERSION: u8 = 1;

const KIND_TEXT: u8 = 1;
const KIND_BINARY: u8 = 2;
const KIND_HOT: u8 = 3;
const KIND_KEPT: u8 = 4;
const NO_EXCEPT: u16 = u16::MAX;

/// A frame that cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError(String);

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "broker frame: {}", self.0)
    }
}

impl std::error::Error for WireError {}

fn err(m: impl Into<String>) -> WireError {
    WireError(m.into())
}

/// A text frame to keep, in two parts: everything before the offset, and
/// the text after it. The broker writes `head`, the offset (`u8` length +
/// ASCII digits), then `text`. `None` if `f` is not text.
pub fn encode_kept(f: &BrokerFrame) -> Option<(Bytes, Bytes)> {
    let OutFrame::Text(t) = &f.body else {
        return None;
    };
    let mut head = BytesMut::new();
    put_header(&mut head, f, KIND_KEPT);
    Some((head.freeze(), Bytes::copy_from_slice(t.as_bytes())))
}

fn put_header(b: &mut BytesMut, f: &BrokerFrame, kind: u8) {
    b.put_u8(VERSION);
    b.put_u8(kind);
    b.put_u16_le(f.origin);
    b.put_u8(u8::from(matches!(f.qos, QoS::Reliable)));
    put_str(b, &f.room_key);
    match &f.except {
        Some(e) => put_str(b, e),
        None => b.put_u16_le(NO_EXCEPT),
    }
}

/// Encode a room frame. `None` for frames that never cross nodes (pings,
/// batches).
pub fn encode(f: &BrokerFrame) -> Option<Bytes> {
    if let (Some(offset), Some((head, text))) = (f.offset, encode_kept(f)) {
        let digits = offset.to_string();
        let mut b = BytesMut::with_capacity(head.len() + 1 + digits.len() + text.len());
        b.extend_from_slice(&head);
        b.put_u8(digits.len() as u8);
        b.extend_from_slice(digits.as_bytes());
        b.extend_from_slice(&text);
        return Some(b.freeze());
    }
    let (kind, body_len) = match &f.body {
        OutFrame::Text(t) => (KIND_TEXT, t.as_bytes().len()),
        OutFrame::Binary(b) => (KIND_BINARY, b.len()),
        OutFrame::Hot(h) => (KIND_HOT, 4 + h.v1.len() + 4 + h.v2.len() + 10),
        OutFrame::Ping | OutFrame::Batch(_) => return None,
    };
    let except = f.except.as_deref().unwrap_or("");
    let mut b = BytesMut::with_capacity(9 + f.room_key.len() + except.len() + body_len);
    put_header(&mut b, f, kind);
    match &f.body {
        OutFrame::Text(t) => b.extend_from_slice(t.as_bytes()),
        OutFrame::Binary(x) => b.extend_from_slice(x),
        OutFrame::Hot(h) => {
            b.put_u32_le(h.v1.len() as u32);
            b.extend_from_slice(&h.v1);
            b.put_u32_le(h.v2.len() as u32);
            b.extend_from_slice(&h.v2);
            match h.slot_key {
                Some((_, sender, svc, key)) => {
                    b.put_u8(1);
                    b.put_u32_le(sender);
                    b.put_u8(svc);
                    b.put_u32_le(key);
                }
                None => b.put_u8(0),
            }
        }
        OutFrame::Ping | OutFrame::Batch(_) => {}
    }
    Some(b.freeze())
}

/// Room keys and session keys are far below 64 KiB (room names are at most
/// 128 bytes); longer strings are cut, which cannot happen in practice.
fn put_str(b: &mut BytesMut, s: &str) {
    let n = s.len().min(usize::from(NO_EXCEPT - 1));
    b.put_u16_le(n as u16);
    b.extend_from_slice(&s.as_bytes()[..n]);
}

fn get_str(b: &mut Bytes) -> Result<Option<Arc<str>>, WireError> {
    need(b, 2)?;
    let n = b.get_u16_le();
    if n == NO_EXCEPT {
        return Ok(None);
    }
    let n = usize::from(n);
    need(b, n)?;
    let s = std::str::from_utf8(&b[..n]).map_err(|_| err("string is not UTF-8"))?;
    let s = Arc::from(s);
    b.advance(n);
    Ok(Some(s))
}

fn need(b: &Bytes, n: usize) -> Result<(), WireError> {
    if b.remaining() < n {
        Err(err("truncated"))
    } else {
        Ok(())
    }
}

fn take(b: &mut Bytes) -> Result<Bytes, WireError> {
    need(b, 4)?;
    let n = b.get_u32_le() as usize;
    need(b, n)?;
    Ok(b.split_to(n))
}

/// Decode a frame written by [`encode`] (this version only).
pub fn decode(bytes: &Bytes) -> Result<BrokerFrame, WireError> {
    let mut b = bytes.clone();
    need(&b, 5)?;
    let version = b.get_u8();
    if version != VERSION {
        return Err(err(format!("unknown version {version}")));
    }
    let kind = b.get_u8();
    let origin = b.get_u16_le();
    let qos = if b.get_u8() == 1 {
        QoS::Reliable
    } else {
        QoS::Lossy
    };
    let room_key = get_str(&mut b)?.ok_or_else(|| err("no room key"))?;
    let except = get_str(&mut b)?;
    let mut offset = None;
    let body = match kind {
        KIND_KEPT => {
            need(&b, 1)?;
            let n = usize::from(b.get_u8());
            need(&b, n)?;
            let digits = std::str::from_utf8(&b[..n]).map_err(|_| err("bad offset"))?;
            offset = Some(digits.parse::<u64>().map_err(|_| err("bad offset"))?);
            b.advance(n);
            OutFrame::Text(Utf8Text::try_from_bytes(b).map_err(|_| err("text is not UTF-8"))?)
        }
        KIND_TEXT => {
            OutFrame::Text(Utf8Text::try_from_bytes(b).map_err(|_| err("text is not UTF-8"))?)
        }
        KIND_BINARY => OutFrame::Binary(b),
        KIND_HOT => {
            let v1 = take(&mut b)?;
            let v2 = take(&mut b)?;
            need(&b, 1)?;
            let slot_key = if b.get_u8() == 1 {
                need(&b, 9)?;
                let (sender, svc, key) = (b.get_u32_le(), b.get_u8(), b.get_u32_le());
                // Room id 0 here: the receiving node puts in its own.
                Some((0, sender, svc, key))
            } else {
                None
            };
            OutFrame::Hot(Arc::new(HotOut { v1, v2, slot_key }))
        }
        k => return Err(err(format!("unknown kind {k}"))),
    };
    let mut f = BrokerFrame::new(origin, &room_key, qos, body);
    f.except = except;
    f.offset = offset;
    Ok(f)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn roundtrip(f: &BrokerFrame) -> BrokerFrame {
        decode(&encode(f).unwrap()).unwrap()
    }

    #[test]
    fn frames_round_trip() {
        let f = BrokerFrame::new(
            7,
            "t::chat:1",
            QoS::Reliable,
            OutFrame::text("{\"a\":1}".into()),
        )
        .except(Some("t::u::s#3"));
        let g = roundtrip(&f);
        assert_eq!(
            (g.origin, &*g.tenant, &*g.room_key, g.except.as_deref()),
            (7, "t", "t::chat:1", Some("t::u::s#3"))
        );
        assert!(matches!(g.qos, QoS::Reliable));
        assert_eq!(g.body, f.body);

        let hot = OutFrame::Hot(Arc::new(HotOut {
            v1: Bytes::from_static(b"p"),
            v2: Bytes::from_static(b"\x01\x05\x09\x00\x00\x00\x01\x01p"),
            slot_key: Some((9, 4, 1, 77)),
        }));
        let g = roundtrip(&BrokerFrame::new(1, "t::r", QoS::Lossy, hot));
        match g.body {
            OutFrame::Hot(h) => {
                assert_eq!(&h.v1[..], b"p");
                assert_eq!(h.v2.len(), 9);
                assert_eq!(h.slot_key, Some((0, 4, 1, 77)), "room id is the receiver's");
            }
            other => panic!("{other:?}"),
        }
        assert!(g.except.is_none());
    }

    #[test]
    fn kept_text_carries_its_offset_between_the_parts() {
        let mut f = BrokerFrame::new(2, "t::h", QoS::Reliable, OutFrame::text("{\"m\":1}".into()));
        let (head, text) = encode_kept(&f).unwrap();
        // What the broker builds: head, u8 length + digits, text.
        let mut wire = head.to_vec();
        wire.push(3);
        wire.extend_from_slice(b"417");
        wire.extend_from_slice(&text);
        let g = decode(&Bytes::from(wire.clone())).unwrap();
        assert_eq!(g.offset, Some(417));
        assert_eq!(g.body, f.body);
        // encode() writes the same bytes for a frame that has its offset.
        f.offset = Some(417);
        assert_eq!(encode(&f).unwrap().to_vec(), wire);
        assert!(decode(&Bytes::from(wire[..head.len() + 2].to_vec())).is_err());
    }

    #[test]
    fn bad_frames_are_errors_not_panics() {
        let good = encode(&BrokerFrame::new(
            1,
            "t::r",
            QoS::Lossy,
            OutFrame::Binary(Bytes::from_static(b"xyz")),
        ))
        .unwrap();
        for n in 0..good.len() {
            let _ = decode(&good.slice(..n)); // must not panic
        }
        let mut v = good.to_vec();
        v[0] = 2;
        assert!(decode(&Bytes::from(v)).is_err(), "unknown version");
        assert!(encode(&BrokerFrame::new(1, "t::r", QoS::Lossy, OutFrame::Ping)).is_none());
    }
}
