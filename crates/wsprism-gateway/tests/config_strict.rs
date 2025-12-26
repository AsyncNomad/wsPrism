#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

use wsprism_gateway::config;

#[test]
fn deny_unknown_fields_nested() {
    let bad = r#"
version: 1
gateway:
  listen: "0.0.0.0:8080"
tenants:
  - id: "acme"
    limitz: { max_frame_bytes: 123 } # typo should fail
"#;

    let err = config::load_from_str(bad).expect_err("must fail");
    assert_eq!(err.client_code().as_str(), "BAD_REQUEST");
}

#[test]
fn ok_minimal_config() {
    let ok = r#"
version: 1
tenants:
  - id: "acme"
"#;
    let cfg = config::load_from_str(ok).expect("must parse");
    assert_eq!(cfg.version, 1);
    assert_eq!(cfg.tenants[0].id, "acme");
}
