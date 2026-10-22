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

/// Every config shipped in the repository must build (the reference
/// `wsprism.yaml` once allowlisted services that did not exist, unnoticed
/// while that was only a warning).
#[test]
fn shipped_configs_build() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for rel in [
        "wsprism.yaml",
        "bench/gateway.yaml",
        "examples/chat-and-state/gateway.yaml",
    ] {
        let path = root.join(rel);
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{rel}: {e}"));
        let cfg = config::load_from_str(&text)
            .unwrap_or_else(|e| panic!("{rel}: {}", e.client_message()));
        if let Err(e) = wsprism_gateway::Gateway::builder().config(cfg).build() {
            panic!("{rel}: {}", e.client_message());
        }
    }
}

/// The compose example's configuration (values from the environment; the
/// Redis URL is read at build time, so only parsing and validation here).
#[test]
fn compose_config_parses() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let text = std::fs::read_to_string(root.join("deploy/compose/wsprism.yaml")).unwrap();
    let env = |name: &str| (name == "DEMO_API_KEY_SHA256").then(|| "ab".repeat(32));
    let cfg =
        config::load_with_env(&text, &env).unwrap_or_else(|e| panic!("{}", e.client_message()));
    assert_eq!(cfg.tenants[0].id, "demo");
}

/// M6: `${VAR}` in values, from the environment (here a fixed lookup).
#[test]
fn placeholders_take_values_from_the_environment() {
    let env = |name: &str| match name {
        "PORT" => Some("9000".to_string()),
        "QUEUE" => Some("2048".to_string()),
        "TENANT" => Some("acme".to_string()),
        _ => None,
    };
    let yaml = r#"
version: 1
gateway:
  listen: "0.0.0.0:${PORT}"
  session_queue_capacity: ${QUEUE}
tenants:
  - id: "${TENANT}"
    limits: { max_frame_bytes: "${FRAME:-4096}" } # quoted inside { }
"#;
    let cfg = config::load_with_env(yaml, &env).expect("must parse");
    assert_eq!(cfg.gateway.listen, "0.0.0.0:9000");
    assert_eq!(cfg.gateway.session_queue_capacity, 2048);
    assert_eq!(cfg.tenants[0].id, "acme");
    assert_eq!(cfg.tenants[0].limits.max_frame_bytes, 4096);

    // Strictness is kept, and errors name the setting.
    let typo = yaml.replace("limits:", "limitz:");
    let err = config::load_with_env(&typo, &env).expect_err("unknown field");
    assert!(err.to_string().contains("tenants[0]"), "{err}");
    let missing = config::load_with_env(yaml, &|_| None).expect_err("PORT unset");
    assert!(err_has(&missing, "PORT") && err_has(&missing, "gateway.listen"));
}

fn err_has(e: &wsprism_protocol::error::WsPrismError, s: &str) -> bool {
    e.to_string().contains(s)
}

/// `replica_ack`: on by default with Sentinel only, `replicas: 0` turns it
/// off, and its bounds are checked.
#[test]
fn replica_ack_defaults_and_bounds() {
    let effective = |broker: &str| -> Result<Option<(u16, u64)>, String> {
        let yaml = format!("version: 1\ngateway:\n  broker: {broker}\ntenants:\n  - id: t\n");
        let cfg = config::load_from_str(&yaml).map_err(|e| e.to_string())?;
        match &cfg.gateway.broker {
            config::schema::BrokerConfig::Redis(r) => Ok(r
                .effective_replica_ack()
                .map(|a| (a.replicas, a.timeout_ms))),
            other => panic!("{other:?}"),
        }
    };
    let sentinel = r#"sentinel: { addresses: ["redis://s:26379"], master_name: m }"#;
    assert_eq!(effective(r#"{ kind: redis, url: "redis://r" }"#), Ok(None));
    assert_eq!(
        effective(&format!(
            r#"{{ kind: redis, url: "redis://r", {sentinel} }}"#
        )),
        Ok(Some((1, 500)))
    );
    assert_eq!(
        effective(&format!(
            r#"{{ kind: redis, url: "redis://r", {sentinel}, replica_ack: {{ replicas: 0 }} }}"#
        )),
        Ok(None)
    );
    assert_eq!(
        effective(
            r#"{ kind: redis, url: "redis://r", replica_ack: { replicas: 2, timeout_ms: 50 } }"#
        ),
        Ok(Some((2, 50)))
    );
    for bad in [
        "{ replicas: 17 }",
        "{ timeout_ms: 0 }",
        "{ timeout_ms: 10001 }",
    ] {
        let e = effective(&format!(
            r#"{{ kind: redis, url: "redis://r", replica_ack: {bad} }}"#
        ))
        .expect_err(bad);
        assert!(e.contains("replica_ack"), "{e}");
    }
}

/// `admission`: on by default (500 ms, 10,000), `max_latency_ms: 0` off,
/// bounds checked.
#[test]
fn admission_defaults_and_bounds() {
    let load = |admission: &str| {
        let yaml = format!(
            "version: 1\ngateway:\n  broker: {{ kind: redis, url: \"redis://r\"{admission} }}\ntenants:\n  - id: t\n"
        );
        config::load_from_str(&yaml).map_err(|e| e.to_string())
    };
    let cfg = load("").unwrap();
    let config::schema::BrokerConfig::Redis(r) = &cfg.gateway.broker else {
        panic!("redis broker")
    };
    assert_eq!(
        (r.admission.max_latency_ms, r.admission.max_pending_claims),
        (500, 10_000)
    );
    assert!(load(", admission: { max_latency_ms: 0 }").is_ok());
    for bad in [
        "{ max_latency_ms: 5 }",
        "{ max_latency_ms: 60001 }",
        "{ max_pending_claims: 0 }",
    ] {
        let e = load(&format!(", admission: {bad}")).expect_err(bad);
        assert!(e.contains("admission"), "{e}");
    }
}

/// Sentinel with TLS (0.2): `rediss://` sentinel addresses and URL, with
/// `tls`, are accepted; other schemes are not.
#[test]
fn sentinel_takes_tls() {
    let load = |addr: &str, url: &str, tls: &str| {
        let yaml = format!(
            "version: 1\ngateway:\n  broker: {{ kind: redis, url: \"{url}\", sentinel: {{ addresses: [\"{addr}\"], master_name: m }}{tls} }}\ntenants:\n  - id: t\n"
        );
        config::load_from_str(&yaml).map_err(|e| e.to_string())
    };
    assert!(load(
        "rediss://s:26379",
        "rediss://primary",
        ", tls: { ca_file: \"/x/ca.crt\" }"
    )
    .is_ok());
    assert!(load("rediss://s:26379", "redis://primary", "").is_ok());
    let e = load("http://s:26379", "redis://primary", "").unwrap_err();
    assert!(e.contains("sentinel.addresses"), "{e}");
}
