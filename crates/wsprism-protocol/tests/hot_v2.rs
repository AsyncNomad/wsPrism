//! Protocol v2 Hot frames (ADR 0006 §3, R7):
//! `[kind=0x01][flags][chan u32][svc][op][key u32?][seq u32?][sender u32?][payload]`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bytes::Bytes;
use wsprism_protocol::protocol::hot::{
    decode_hot_frame_v2, encode_hot_frame_v2, HotOutV2, HOT_V2_FLAG_KEY, HOT_V2_FLAG_SENDER,
    HOT_V2_FLAG_SEQ, KIND_BATCH, KIND_HOT,
};

fn frame(flags: u8, chan: u32, tail: &[u8]) -> Bytes {
    let mut b = vec![KIND_HOT, flags];
    b.extend_from_slice(&chan.to_le_bytes());
    b.extend_from_slice(&[1, 7]); // svc, op
    b.extend_from_slice(tail);
    Bytes::from(b)
}

#[test]
fn minimal_frame_has_channel_svc_op_and_payload() {
    let f = decode_hot_frame_v2(frame(0, 42, b"hi")).unwrap();
    assert_eq!((f.chan, f.svc_id, f.opcode), (Some(42), 1, 7));
    assert_eq!((f.key, f.seq), (None, None));
    assert_eq!(&f.payload[..], b"hi");
}

#[test]
fn key_and_seq_are_read_in_order() {
    let mut tail = 5u32.to_le_bytes().to_vec();
    tail.extend_from_slice(&9u32.to_le_bytes());
    tail.extend_from_slice(b"p");
    let f = decode_hot_frame_v2(frame(HOT_V2_FLAG_KEY | HOT_V2_FLAG_SEQ, 3, &tail)).unwrap();
    assert_eq!((f.key, f.seq), (Some(5), Some(9)));
    assert_eq!(&f.payload[..], b"p");
}

#[test]
fn malformed_frames_are_rejected() {
    // Clients never set `sender` (the server stamps it).
    assert!(decode_hot_frame_v2(frame(HOT_V2_FLAG_SENDER, 1, &[0; 8])).is_err());
    // Unknown flag bits: field layout unknown, so reject.
    assert!(decode_hot_frame_v2(frame(0x80, 1, b"")).is_err());
    // Flag says key present but bytes are missing.
    assert!(decode_hot_frame_v2(frame(HOT_V2_FLAG_KEY, 1, &[1, 2])).is_err());
    // Too short for the header.
    assert!(decode_hot_frame_v2(Bytes::from_static(&[KIND_HOT, 0, 1, 0])).is_err());
    // Batch frames only go server -> client; other kinds are reserved.
    assert!(decode_hot_frame_v2(Bytes::from_static(&[KIND_BATCH, 0, 0, 0, 0, 0, 0, 0])).is_err());
    assert!(decode_hot_frame_v2(Bytes::from_static(&[0x00, 0, 0, 0, 0, 0, 0, 0])).is_err());
    assert!(decode_hot_frame_v2(Bytes::new()).is_err());
}

#[test]
fn server_frame_encoding_carries_sender_and_is_shared_by_all_recipients() {
    let out = HotOutV2 {
        chan: 42,
        svc_id: 1,
        opcode: 7,
        key: Some(5),
        seq: Some(9),
        sender: 17,
        payload: Bytes::from_static(b"state"),
    };
    let b = encode_hot_frame_v2(&out);
    assert_eq!(b[0], KIND_HOT);
    assert_eq!(b[1], HOT_V2_FLAG_KEY | HOT_V2_FLAG_SEQ | HOT_V2_FLAG_SENDER);
    assert_eq!(&b[2..6], &42u32.to_le_bytes());
    assert_eq!(&b[6..8], &[1, 7]);
    assert_eq!(&b[8..12], &5u32.to_le_bytes());
    assert_eq!(&b[12..16], &9u32.to_le_bytes());
    assert_eq!(&b[16..20], &17u32.to_le_bytes());
    assert_eq!(&b[20..], b"state");
}
