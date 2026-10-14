//! Hot Lane binary frame decoder (untrusted client bytes).
#![no_main]

use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use wsprism_core::protocol::hot::{decode_hot_frame, HOT_FLAG_SEQ_PRESENT};

fuzz_target!(|data: &[u8]| {
    if let Ok(frame) = decode_hot_frame(Bytes::copy_from_slice(data)) {
        // Header (4) + optional seq (4) + payload must account for every byte.
        let seq_len = if frame.flags & HOT_FLAG_SEQ_PRESENT != 0 { 4 } else { 0 };
        assert_eq!(4 + seq_len + frame.payload.len(), data.len());
        assert_eq!(frame.seq.is_some(), seq_len == 4);
        assert_eq!(frame.v, 1);
    }
});
