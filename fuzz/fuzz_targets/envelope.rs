//! Ext Lane JSON envelope decoders, v1 (strict) and v2 (lenient), on
//! untrusted client text.
#![no_main]

use libfuzzer_sys::fuzz_target;
use wsprism_protocol::protocol::text::{decode_envelope, decode_envelope_v2, EXT_PROTOCOL_VERSION};

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        if let Ok(env) = decode_envelope(text) {
            assert_eq!(env.v, EXT_PROTOCOL_VERSION);
            // Anything strict v1 accepts, lenient v2 accepts with the same meaning.
            let v2 = decode_envelope_v2(text).expect("v2 accepts what v1 accepts");
            assert_eq!((&v2.svc, &v2.msg_type, &v2.room, v2.seq), (&env.svc, &env.msg_type, &env.room, env.seq));
        }
        if let Ok(env) = decode_envelope_v2(text) {
            assert_eq!(env.v, EXT_PROTOCOL_VERSION);
            assert_eq!(env.flags, 0);
        }
    }
});
