//! Ext Lane JSON envelope decoder (untrusted client text).
#![no_main]

use libfuzzer_sys::fuzz_target;
use wsprism_core::protocol::text::{decode_envelope, EXT_PROTOCOL_VERSION};

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        if let Ok(env) = decode_envelope(text) {
            assert_eq!(env.v, EXT_PROTOCOL_VERSION);
        }
    }
});
