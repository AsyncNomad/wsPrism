//! Ext Lane envelope vector tests.

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

use std::fs;

use wsprism_core::protocol::text::{decode_envelope, Envelope};

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
