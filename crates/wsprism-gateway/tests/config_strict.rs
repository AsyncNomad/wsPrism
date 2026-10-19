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

/// ADR 0008 §1: `tenants[].auth` is an ordered list of authenticators; an
/// unknown kind is a config error, not silently ignored.
#[test]
fn auth_chain_config_is_strict() {
    let ok = "version: 1\ntenants:\n  - id: t\n    auth:\n      - kind: dev\n";
    let cfg = config::load_from_str(ok).expect("explicit dev auth parses");
    assert_eq!(cfg.tenants[0].auth.len(), 1);
    let bad = "version: 1\ntenants:\n  - id: t\n    auth:\n      - kind: magic\n";
    assert!(
        config::load_from_str(bad).is_err(),
        "unknown authenticator kind"
    );
}

#[test]
fn jwt_config_is_strict() {
    let cases = [
        ("no key", "{ kind: jwt, algorithm: HS256 }"),
        (
            "two secrets",
            "{ kind: jwt, algorithm: HS256, secret_file: a, secret_env: B }",
        ),
        (
            "public key for hmac",
            "{ kind: jwt, algorithm: HS256, secret_env: B, public_key_file: k }",
        ),
        (
            "secret for rsa",
            "{ kind: jwt, algorithm: RS256, secret_env: B }",
        ),
        (
            "unknown algorithm",
            "{ kind: jwt, algorithm: none, secret_env: B }",
        ),
        (
            "leeway too large",
            "{ kind: jwt, algorithm: HS256, secret_env: B, leeway_s: 301 }",
        ),
        (
            "unknown field",
            "{ kind: jwt, algorithm: HS256, secret_env: B, algorithms: [HS256] }",
        ),
    ];
    for (name, link) in cases {
        let yaml = format!("version: 1\ntenants:\n  - id: a\n    auth: [{link}]\n");
        assert!(
            config::load_from_str(&yaml).is_err(),
            "{name} must be rejected"
        );
    }
}

#[test]
fn jwt_keys_are_loaded_and_checked_at_startup() {
    let dir = std::env::temp_dir().join(format!("wsprism-jwtcfg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let short = dir.join("short");
    std::fs::write(&short, "too-short\n").unwrap();
    let not_pem = dir.join("not.pem");
    std::fs::write(&not_pem, "hello").unwrap();
    let build = |link: String| {
        let yaml = format!("version: 1\ntenants:\n  - id: a\n    auth: [{link}]\n");
        wsprism_gateway::Gateway::builder()
            .config(config::load_from_str(&yaml).unwrap())
            .build()
    };
    let cases = [
        (
            "missing file",
            "{ kind: jwt, algorithm: HS256, secret_file: /nonexistent/wsprism }".to_string(),
        ),
        (
            "short secret",
            format!(
                "{{ kind: jwt, algorithm: HS256, secret_file: \"{}\" }}",
                short.display()
            ),
        ),
        (
            "unset env",
            "{ kind: jwt, algorithm: HS256, secret_env: WSPRISM_TEST_UNSET_SECRET }".to_string(),
        ),
        (
            "bad pem",
            format!(
                "{{ kind: jwt, algorithm: ES256, public_key_file: \"{}\" }}",
                not_pem.display()
            ),
        ),
    ];
    for (name, link) in cases {
        assert!(build(link).is_err(), "{name} must fail at startup");
    }
}
