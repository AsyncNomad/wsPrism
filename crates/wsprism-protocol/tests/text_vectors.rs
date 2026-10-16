//! Ext Lane envelope vector tests.

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

use std::fs;

use wsprism_protocol::protocol::text::{decode_envelope, Envelope};

fn load(name: &str) -> String {
    fs::read_to_string(format!("tests/vectors/{name}")).unwrap()
}

#[test]
fn parse_envelope_min() {
    let s = load("envelope_min.json");
    let env: Envelope = serde_json::from_str(&s).unwrap();
    assert_eq!(env.v, 1);
    assert_eq!(env.svc, "sys");
    assert_eq!(env.msg_type, "ping");
    assert!(env.data.is_none());
}

#[test]
fn parse_envelope_full() {
    let s = load("envelope_full.json");
    let env: Envelope = serde_json::from_str(&s).unwrap();
    assert_eq!(env.svc, "chat");
    assert_eq!(env.msg_type, "send");
    assert_eq!(env.seq, Some(123));
    assert_eq!(env.room.as_deref(), Some("party:1"));
    let raw = env.data.unwrap();
    assert!(raw.get().contains("\"text\""));
}

#[test]
fn reject_unsupported_envelope_version() {
    let err = decode_envelope(r#"{"v":2,"svc":"chat","type":"send"}"#).unwrap_err();
    assert_eq!(err.client_code().as_str(), "UNSUPPORTED_VERSION");
}

#[test]
fn decode_envelope_accepts_v1() {
    let env = decode_envelope(&load("envelope_full.json")).unwrap();
    assert_eq!(env.v, 1);
}

#[test]
fn v2_envelope_is_lenient_and_versionless() {
    use wsprism_protocol::protocol::text::decode_envelope_v2;
    let env =
        decode_envelope_v2(r#"{"svc":"chat","type":"send","room":"r","seq":7,"x":{"y":[1]}}"#)
            .unwrap();
    assert_eq!(env.svc, "chat");
    assert_eq!(env.msg_type, "send");
    assert_eq!(env.room.as_deref(), Some("r"));
    assert_eq!(env.seq, Some(7));
    // `v` and `flags` are accepted and ignored if a client still sends them.
    assert!(decode_envelope_v2(r#"{"v":9,"flags":3,"svc":"a","type":"b"}"#).is_ok());
    // Still a structured envelope: svc and type are required.
    assert!(decode_envelope_v2(r#"{"svc":"a"}"#).is_err());
}

/// serde derives also accept a JSON array as a struct (fields by position).
/// That undocumented form must be rejected by both decoders (found by fuzzing:
/// `[1,"",""]` decoded as v1 but meant something else to v2).
#[test]
fn positional_array_envelopes_are_rejected() {
    use wsprism_protocol::protocol::text::decode_envelope_v2;
    for text in [
        r#"[1,"chat","send"]"#,
        r#" [1,"",""]"#,
        r#"["chat","send"]"#,
        "7",
        r#""s""#,
    ] {
        assert!(decode_envelope(text).is_err(), "v1 accepted {text}");
        assert!(decode_envelope_v2(text).is_err(), "v2 accepted {text}");
    }
}
