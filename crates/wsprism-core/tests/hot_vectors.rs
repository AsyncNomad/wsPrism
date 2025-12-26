//! Hot Lane vector tests.

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

use std::fs;

use bytes::Bytes;

use wsprism_core::protocol::hot::decode_hot_frame;

mod vector_loader;
use vector_loader::TestVector;

fn load(name: &str) -> TestVector {
    let s = fs::read_to_string(format!("tests/vectors/{name}")).unwrap();
    serde_json::from_str(&s).unwrap()
}

#[test]
fn hot_vectors() {
    let files = [
        "hot_move.json",
        "hot_bad_version.json",
        "hot_seq_flag_missing_u32.json",
        "hot_too_short.json",
        "hot_payload_ok.json",
    ];

    for f in files {
        let v = load(f);
        let raw = v.frame.decode();
        let res = decode_hot_frame(Bytes::from(raw));

        if let Some(err) = v.expect_error {
            let e = res.expect_err("expected error");
            assert_eq!(e.client_code().as_str(), err.code, "vector={}", v.description);
            continue;
        }

        let frame = res.expect("expected ok frame");
        let ex = v.expect.expect("missing expect block");

        assert_eq!(frame.v as u64, ex["v"].as_u64().unwrap(), "vector={}", v.description);
        assert_eq!(frame.svc_id as u64, ex["svc_id"].as_u64().unwrap(), "vector={}", v.description);
        assert_eq!(frame.opcode as u64, ex["opcode"].as_u64().unwrap(), "vector={}", v.description);
        assert_eq!(frame.flags as u64, ex["flags"].as_u64().unwrap(), "vector={}", v.description);

        if ex.get("seq").is_some() && !ex["seq"].is_null() {
            assert_eq!(frame.seq.unwrap() as u64, ex["seq"].as_u64().unwrap(), "vector={}", v.description);
        } else {
            assert!(frame.seq.is_none(), "vector={}", v.description);
        }

        assert_eq!(frame.payload.len() as u64, ex["payload_len"].as_u64().unwrap(), "vector={}", v.description);
    }
}
