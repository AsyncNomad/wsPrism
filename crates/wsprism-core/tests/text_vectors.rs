//! Ext Lane envelope vector tests.

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

use std::fs;

use wsprism_core::protocol::text::Envelope;

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
