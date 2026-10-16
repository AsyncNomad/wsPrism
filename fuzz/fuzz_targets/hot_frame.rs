//! Hot Lane binary frame decoders, v1 and v2, on untrusted client bytes.
#![no_main]

use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use wsprism_core::protocol::hot::{
    decode_hot_frame, decode_hot_frame_v2, encode_hot_frame_v2, HotOutV2, HOT_FLAG_SEQ_PRESENT,
    HOT_V2_FLAG_SENDER,
};

fuzz_target!(|data: &[u8]| {
    if let Ok(f) = decode_hot_frame(Bytes::copy_from_slice(data)) {
        // Header (4) + optional seq (4) + payload must account for every byte.
        let seq_len = if f.flags & HOT_FLAG_SEQ_PRESENT != 0 { 4 } else { 0 };
        assert_eq!(4 + seq_len + f.payload.len(), data.len());
        assert_eq!(f.seq.is_some(), seq_len == 4);
        assert_eq!(f.v, 1);
        assert!(f.chan.is_none() && f.key.is_none());
    }
    if let Ok(f) = decode_hot_frame_v2(Bytes::copy_from_slice(data)) {
        let chan = f.chan.expect("v2 frames name a room");
        // Header (8) + optional key/seq (4 each) + payload account for every byte.
        let opt = 4 * (f.key.is_some() as usize + f.seq.is_some() as usize);
        assert_eq!(8 + opt + f.payload.len(), data.len());
        assert_eq!(f.flags & HOT_V2_FLAG_SENDER, 0, "clients cannot set sender");
        // Re-encoding as a server frame keeps every field and the payload.
        let out = encode_hot_frame_v2(&HotOutV2 {
            chan,
            svc_id: f.svc_id,
            opcode: f.opcode,
            key: f.key,
            seq: f.seq,
            sender: 0,
            payload: f.payload.clone(),
        });
        assert!(out.ends_with(&f.payload));
        assert_eq!(out.len(), 8 + 4 + 4 * (f.key.is_some() as usize + f.seq.is_some() as usize) + f.payload.len());
    }
});
