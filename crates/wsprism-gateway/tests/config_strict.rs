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

#[test]
fn reject_reserved_tenant_id_prefix() {
    let bad = r#"
version: 1
tenants:
  - id: "_unknown"
"#;
    let err = config::load_from_str(bad).expect_err("must fail");
    assert!(err.to_string().contains("reserved"), "{err}");
}

/// Tenant ids become parts of internal keys and metric labels, so they are
/// restricted to `[A-Za-z0-9_-]{1,64}`: an id like `a::b` could otherwise
/// alias another tenant's keys (`a` + room `b::r` vs `a::b` + room `r`).
#[test]
fn reject_tenant_id_outside_charset() {
    for id in ["a::b", "has space", "dots.are.out", &"x".repeat(65)] {
        let yaml = format!("version: 1\ntenants:\n  - id: \"{id}\"\n");
        let err = config::load_from_str(&yaml).expect_err(&format!("must reject {id:?}"));
        assert_eq!(err.client_code().as_str(), "BAD_REQUEST", "{id:?}");
    }
    let ok = "version: 1\ntenants:\n  - id: \"Acme-1_b\"\n";
    config::load_from_str(ok).expect("charset-conforming id must parse");
}
