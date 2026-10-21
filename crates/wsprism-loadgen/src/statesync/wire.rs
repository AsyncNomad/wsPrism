//! State frames and relay batches.
//!
//! Frame: `[sender u32 LE][seq u32 LE][ts u64 LE][padding]`.
//! Batch: `[count u16 LE]` then `count` × `[len u16 LE][frame]`.

use bytes::{BufMut, Bytes, BytesMut};

pub const FRAME_HEADER: usize = 16;

pub fn frame(sender: u32, seq: u32, ts_ns: u64, size: usize) -> Bytes {
    let mut b = BytesMut::with_capacity(size.max(FRAME_HEADER));
    b.put_u32_le(sender);
    b.put_u32_le(seq);
    b.put_u64_le(ts_ns);
    b.resize(size.max(FRAME_HEADER), 0);
    b.freeze()
}

/// A decoded frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub sender: u32,
    pub seq: u32,
    pub ts_ns: u64,
}

pub fn header(f: &[u8]) -> Option<Header> {
    Some(Header {
        sender: u32::from_le_bytes(f.get(0..4)?.try_into().ok()?),
        seq: u32::from_le_bytes(f.get(4..8)?.try_into().ok()?),
        ts_ns: u64::from_le_bytes(f.get(8..16)?.try_into().ok()?),
    })
}

/// Frame headers in a batch (stops at the first malformed entry).
pub fn batch_headers(batch: &[u8]) -> Vec<Header> {
    let mut out = Vec::new();
    let Some(count) = batch.get(0..2).map(|c| u16::from_le_bytes([c[0], c[1]])) else {
        return out;
    };
    let mut at = 2;
    for _ in 0..count {
        let Some(len) = batch
            .get(at..at + 2)
            .map(|l| u16::from_le_bytes([l[0], l[1]]) as usize)
        else {
            break;
        };
        at += 2;
        let Some(f) = batch.get(at..at + len) else {
            break;
        };
        if let Some(h) = header(f) {
            out.push(h);
        }
        at += len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_round_trip() {
        let a = frame(1, 7, 42, 64);
        let b = frame(2, 8, 43, 16);
        let mut batch = BytesMut::new();
        batch.put_u16_le(2);
        for f in [&a, &b] {
            batch.put_u16_le(f.len() as u16);
            batch.extend_from_slice(f);
        }
        let hs = batch_headers(&batch);
        assert_eq!(
            hs,
            vec![
                Header {
                    sender: 1,
                    seq: 7,
                    ts_ns: 42
                },
                Header {
                    sender: 2,
                    seq: 8,
                    ts_ns: 43
                },
            ]
        );
        assert!(
            batch_headers(&batch[..10]).len() < 2,
            "truncated batch is not over-read"
        );
    }
}
