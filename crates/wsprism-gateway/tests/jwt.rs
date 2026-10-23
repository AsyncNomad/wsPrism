//! JWT authenticator (ADR 0008 §5): signature with a pinned algorithm,
//! `exp`/`nbf`/`iss`/`aud`, tenant binding, permission claims, and the
//! session closing when the token expires.

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::*;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use ring::signature::KeyPair;
use serde_json::{json, Value};

const SECRET: &str = "0123456789abcdef0123456789abcdef-test-secret";

/// A file in a fresh per-test temp path.
fn temp_file(name: &str, contents: &[u8]) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "wsprism-jwt-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let p = dir.join(name);
    std::fs::write(&p, contents).expect("write temp file");
    p
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}

fn hs(claims: Value) -> String {
    hs_alg(Algorithm::HS256, claims)
}

fn hs_alg(alg: Algorithm, claims: Value) -> String {
    encode(
        &Header::new(alg),
        &claims,
        &EncodingKey::from_secret(SECRET.as_bytes()),
    )
    .expect("sign")
}

/// Tenant `a` (HS256, issuer + audience pinned) and tenant `b` (HS256,
/// tenant claim required). The secret file ends with a newline, as files
/// written by editors and `echo` do.
fn hs_yaml() -> String {
    let secret = temp_file("secret", format!("{SECRET}\n").as_bytes());
    format!(
        r#"
version: 1
tenants:
  - id: a
    auth:
      - kind: jwt
        algorithm: HS256
        secret_file: "{s}"
        issuer: "https://issuer.test"
        audience: "wsprism"
        leeway_s: 0
    policy: {{ ext_allowlist: ["room:*", "chat:*"] }}
  - id: b
    auth:
      - kind: jwt
        algorithm: HS256
        secret_file: "{s}"
        require_tenant: true
        leeway_s: 0
    policy: {{ ext_allowlist: ["room:*"] }}
"#,
        s = secret.display()
    )
}

fn claims_a(sub: &str, exp: u64) -> Value {
    json!({"sub": sub, "exp": exp, "iss": "https://issuer.test", "aud": "wsprism"})
}

async fn connect_token(
    addr: std::net::SocketAddr,
    tenant: &str,
    token: &str,
    proto: Option<&str>,
) -> Result<(Ws, Value), String> {
    let bearer = format!("Bearer {token}");
    let mut headers = vec![("authorization", bearer.as_str())];
    if let Some(p) = proto {
        headers.push(("sec-websocket-protocol", p));
    }
    let (mut ws, _) = try_connect_with(&ws_url(addr, tenant, None), &headers).await?;
    match next_event(&mut ws).await {
        Event::Json(v) if v["type"] == "authed" => Ok((ws, v)),
        other => Err(format!("expected sys.authed, got {other:?}")),
    }
}

fn is_401(r: &Result<(Ws, Value), String>) -> bool {
    matches!(r, Err(e) if e.contains("401"))
}

#[tokio::test]
async fn valid_hs256_token_admits_its_subject() {
    let gw = spawn(&hs_yaml()).await;
    let token = hs(claims_a("alice", now() + 60));
    let (_ws, authed) = connect_token(gw.addr, "a", &token, None)
        .await
        .expect("admitted");
    assert_eq!(authed["data"]["user"], "alice");
}

#[tokio::test]
async fn invalid_tokens_are_rejected() {
    let gw = spawn(&hs_yaml()).await;
    let exp = now() + 60;
    // Corrupt a character in the middle of the signature. (Changing the last
    // one is unreliable: it may already be that character, and its low bits
    // are padding that decoding ignores — the cause of a 1-in-~50 flake.)
    let good = hs(claims_a("alice", exp));
    let mid = good.rfind('.').unwrap() + 20;
    let mut bad_sig = good.clone();
    let swap = if &good[mid..=mid] == "A" { "B" } else { "A" };
    bad_sig.replace_range(mid..=mid, swap);
    assert_ne!(bad_sig, good);
    let other_key = encode(
        &Header::new(Algorithm::HS256),
        &claims_a("alice", exp),
        &EncodingKey::from_secret(b"another-secret-another-secret-another"),
    )
    .expect("sign");
    let cases = [
        ("bad signature", bad_sig),
        ("other key", other_key),
        ("expired", hs(claims_a("alice", now() - 5))),
        (
            "no exp",
            hs(json!({"sub": "alice", "iss": "https://issuer.test", "aud": "wsprism"})),
        ),
        (
            "no sub",
            hs(json!({"exp": exp, "iss": "https://issuer.test", "aud": "wsprism"})),
        ),
        (
            "wrong issuer",
            hs(json!({"sub": "alice", "exp": exp, "iss": "https://evil.test", "aud": "wsprism"})),
        ),
        (
            "wrong audience",
            hs(json!({"sub": "alice", "exp": exp, "iss": "https://issuer.test", "aud": "other"})),
        ),
        (
            "not yet valid",
            hs(
                json!({"sub": "alice", "exp": exp, "nbf": exp, "iss": "https://issuer.test", "aud": "wsprism"}),
            ),
        ),
        // jsonwebtoken skips iss/aud checks when the claim is absent; a
        // configured issuer/audience must therefore be required.
        (
            "no issuer",
            hs(json!({"sub": "alice", "exp": exp, "aud": "wsprism"})),
        ),
        (
            "no audience",
            hs(json!({"sub": "alice", "exp": exp, "iss": "https://issuer.test"})),
        ),
        (
            "unpinned algorithm",
            hs_alg(Algorithm::HS384, claims_a("alice", exp)),
        ),
        ("unsafe user id", hs(claims_a("a::b", exp))),
        ("garbage", "a.b.c".to_string()),
    ];
    for (name, token) in cases {
        let r = connect_token(gw.addr, "a", &token, None).await;
        assert!(
            is_401(&r),
            "{name}: expected 401, got {:?}",
            r.map(|(_, v)| v)
        );
    }
}

#[tokio::test]
async fn tenant_claim_binds_the_token_to_one_tenant() {
    let gw = spawn(&hs_yaml()).await;
    let exp = now() + 60;
    // Tenant b requires the claim.
    let untagged = hs(json!({"sub": "bob", "exp": exp}));
    assert!(is_401(&connect_token(gw.addr, "b", &untagged, None).await));
    let for_b = hs(json!({"sub": "bob", "exp": exp, "wsprism": {"tenant": "b"}}));
    connect_token(gw.addr, "b", &for_b, None)
        .await
        .expect("tagged for b");
    // A token tagged for another tenant is rejected even where the claim is optional.
    let mut for_b_at_a = claims_a("bob", exp);
    for_b_at_a["wsprism"] = json!({"tenant": "b"});
    assert!(is_401(
        &connect_token(gw.addr, "a", &hs(for_b_at_a), None).await
    ));
}

#[tokio::test]
async fn permission_claims_limit_rooms() {
    let gw = spawn(&hs_yaml()).await;
    let mut c = claims_a("carol", now() + 60);
    c["wsprism"] = json!({"subscribe": ["lobby", "match:*"], "publish": ["match:*"]});
    let (mut ws, _) = connect_token(gw.addr, "a", &hs(c), None)
        .await
        .expect("admitted");

    send_json(
        &mut ws,
        json!({"v":1,"svc":"room","type":"join","room":"lobby"}),
    )
    .await;
    send_json(
        &mut ws,
        json!({"v":1,"svc":"room","type":"join","room":"secret"}),
    )
    .await;
    send_json(
        &mut ws,
        json!({"v":1,"svc":"chat","type":"send","room":"lobby","data":{"msg":"hi"}}),
    )
    .await;
    let mut errors = 0;
    while let Event::Json(v) = next_event_within(&mut ws, Duration::from_millis(500)).await {
        if v["type"] == "error" {
            assert_eq!(v["data"]["code"], "NOT_ALLOWED", "{v}");
            errors += 1;
        }
    }
    assert_eq!(errors, 2, "join secret and send to lobby are denied");
}

#[tokio::test]
async fn session_closes_when_the_token_expires() {
    let gw = spawn(&hs_yaml()).await;
    let token = hs(claims_a("dave", now() + 2));
    let (mut ws, _) = connect_token(gw.addr, "a", &token, Some("wsprism.v2.json"))
        .await
        .expect("admitted");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    loop {
        match next_event_within(&mut ws, deadline - tokio::time::Instant::now()).await {
            Event::Json(_) => continue,
            Event::Close(Some(f)) => {
                assert_eq!(u16::from(f.code), 4001);
                assert_eq!(f.reason, "auth_expired");
                break;
            }
            other => panic!("expected close 4001, got {other:?}"),
        }
    }
}

/// Ed25519 SubjectPublicKeyInfo PEM for a raw 32-byte public key.
fn ed25519_spki_pem(raw: &[u8]) -> String {
    use base64::Engine;
    let mut der = vec![
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    der.extend_from_slice(raw);
    format!(
        "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
        base64::engine::general_purpose::STANDARD.encode(der)
    )
}

#[tokio::test]
async fn eddsa_public_key_and_kid_rotation() {
    let rng = ring::rand::SystemRandom::new();
    let mut keys = Vec::new();
    for _ in 0..2 {
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).expect("keygen");
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("key");
        let pem = temp_file(
            "pub.pem",
            ed25519_spki_pem(pair.public_key().as_ref()).as_bytes(),
        );
        keys.push((pkcs8, pem));
    }
    // Two keys during a rotation: the token's `kid` picks the link.
    let yaml = format!(
        r#"
version: 1
tenants:
  - id: a
    auth:
      - {{ kind: jwt, algorithm: EdDSA, kid: old, public_key_file: "{}" }}
      - {{ kind: jwt, algorithm: EdDSA, kid: new, public_key_file: "{}" }}
"#,
        keys[0].1.display(),
        keys[1].1.display()
    );
    let gw = spawn(&yaml).await;
    let sign = |i: usize, kid: &str| {
        let mut h = Header::new(Algorithm::EdDSA);
        h.kid = Some(kid.into());
        encode(
            &h,
            &json!({"sub": "erin", "exp": now() + 60}),
            &EncodingKey::from_ed_der(keys[i].0.as_ref()),
        )
        .expect("sign")
    };
    connect_token(gw.addr, "a", &sign(0, "old"), None)
        .await
        .expect("old key");
    connect_token(gw.addr, "a", &sign(1, "new"), None)
        .await
        .expect("new key");
    assert!(
        is_401(&connect_token(gw.addr, "a", &sign(1, "old"), None).await),
        "kid/key mismatch"
    );
    assert!(
        is_401(&connect_token(gw.addr, "a", &sign(0, "other"), None).await),
        "unknown kid"
    );
}

#[tokio::test]
async fn jwt_and_dev_coexist_in_a_chain() {
    let secret = temp_file("secret", SECRET.as_bytes());
    let yaml = format!(
        r#"
version: 1
tenants:
  - id: a
    auth:
      - {{ kind: jwt, algorithm: HS256, secret_file: "{}" }}
      - {{ kind: dev }}
"#,
        secret.display()
    );
    let gw = spawn(&yaml).await;
    connect_token(gw.addr, "a", "dev:frank", None)
        .await
        .expect("dev passes the jwt link");
    let token = hs(json!({"sub": "gina", "exp": now() + 60}));
    let (_, authed) = connect_token(gw.addr, "a", &token, None)
        .await
        .expect("jwt");
    assert_eq!(authed["data"]["user"], "gina");
}

/// A token that expires in the very far future (a valid signature from
/// the issuer) is admitted: the expiry used to be computed with an
/// overflowing `SystemTime` addition, which panicked the handshake.
#[tokio::test]
async fn a_far_future_expiry_is_admitted_without_overflow() {
    let gw = spawn(&hs_yaml()).await;
    for exp in [u64::MAX, u64::MAX / 2, i64::MAX as u64] {
        let token = hs(claims_a("alice", exp));
        let (_ws, authed) = connect_token(gw.addr, "a", &token, None)
            .await
            .unwrap_or_else(|e| panic!("exp {exp}: {e}"));
        assert_eq!(authed["data"]["user"], "alice");
    }
}
