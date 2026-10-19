//! JWT keys from the issuer's JWKS URL (ADR 0012 §1): picked by `kid`,
//! refetched once on an unknown `kid` (rate-limited), unusable keys
//! ignored, 503 while never loaded. The JWKS server is local (127.0.0.1).

mod common;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine;
use common::*;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::{json, Value};

/// An Ed25519 key: PKCS#8 for signing, JWK for the set.
struct Key {
    pkcs8: Vec<u8>,
    jwk: Value,
}

fn key(kid: &str) -> Key {
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
        .unwrap()
        .as_ref()
        .to_vec();
    let pair = Ed25519KeyPair::from_pkcs8(&pkcs8).unwrap();
    let x = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(pair.public_key().as_ref());
    Key {
        pkcs8,
        jwk: json!({"kty": "OKP", "crv": "Ed25519", "x": x, "kid": kid, "alg": "EdDSA", "use": "sig"}),
    }
}

fn token(k: &Key, kid: &str, sub: &str) -> String {
    let mut h = Header::new(Algorithm::EdDSA);
    h.kid = Some(kid.into());
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 60;
    encode(
        &h,
        &json!({"sub": sub, "exp": exp}),
        &EncodingKey::from_ed_der(&k.pkcs8),
    )
    .unwrap()
}

#[derive(Clone, Default)]
struct Jwks {
    /// `None` = answer 500.
    set: Arc<Mutex<Option<Value>>>,
    fetches: Arc<AtomicUsize>,
}

async fn jwks(State(s): State<Jwks>) -> (StatusCode, Json<Value>) {
    s.fetches.fetch_add(1, Ordering::SeqCst);
    match s.set.lock().unwrap().clone() {
        Some(v) => (StatusCode::OK, Json(v)),
        None => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({}))),
    }
}

async fn jwks_server(initial: Option<Value>) -> (SocketAddr, Jwks) {
    let state = Jwks::default();
    *state.set.lock().unwrap() = initial;
    let app = Router::new()
        .route("/jwks", get(jwks))
        .with_state(state.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    (addr, state)
}

fn yaml(addr: SocketAddr, min_refetch_s: u64) -> String {
    format!(
        "version: 1\ntenants:\n  - id: t\n    auth:\n      - {{ kind: jwt, algorithm: EdDSA, jwks_url: \"http://{addr}/jwks\", jwks_min_refetch_s: {min_refetch_s} }}\n"
    )
}

async fn connect_with(gw: &Gateway, tok: &str) -> Result<Value, String> {
    let bearer = format!("Bearer {tok}");
    let (mut ws, _) =
        try_connect_with(&ws_url(gw.addr, "t", None), &[("authorization", &bearer)]).await?;
    match next_event(&mut ws).await {
        Event::Json(v) if v["type"] == "authed" => Ok(v),
        other => Err(format!("{other:?}")),
    }
}

#[tokio::test]
async fn keys_are_picked_by_kid_and_rotation_is_picked_up() {
    let k1 = key("k1");
    let k2 = key("k2");
    let (addr, server) = jwks_server(Some(json!({"keys": [k1.jwk.clone()]}))).await;
    let gw = spawn(&yaml(addr, 1)).await;

    let v = connect_with(&gw, &token(&k1, "k1", "alice"))
        .await
        .expect("k1");
    assert_eq!(v["data"]["user"], "alice");
    assert_eq!(
        server.fetches.load(Ordering::SeqCst),
        1,
        "fetched on first use"
    );
    connect_with(&gw, &token(&k1, "k1", "bob"))
        .await
        .expect("cached");
    assert_eq!(
        server.fetches.load(Ordering::SeqCst),
        1,
        "served from the cache"
    );

    // The IdP rotates to k2. A token with the new kid triggers one refetch.
    *server.set.lock().unwrap() = Some(json!({"keys": [k2.jwk.clone()]}));
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await; // past min refetch
    connect_with(&gw, &token(&k2, "k2", "carol"))
        .await
        .expect("rotated key");
    assert_eq!(server.fetches.load(Ordering::SeqCst), 2);

    // A key signed by k2 but claiming kid k1 does not verify.
    let forged = token(&k2, "k1", "mallory");
    assert!(matches!(connect_with(&gw, &forged).await, Err(e) if e.contains("401")));
}

#[tokio::test]
async fn unknown_kids_cannot_hammer_the_idp() {
    let k1 = key("k1");
    let (addr, server) = jwks_server(Some(json!({"keys": [k1.jwk.clone()]}))).await;
    let gw = spawn(&yaml(addr, 30)).await;
    connect_with(&gw, &token(&k1, "k1", "alice"))
        .await
        .expect("k1");
    let stranger = key("zz");
    for _ in 0..5 {
        let r = connect_with(&gw, &token(&stranger, "zz", "x")).await;
        assert!(matches!(r, Err(e) if e.contains("401")));
    }
    assert_eq!(
        server.fetches.load(Ordering::SeqCst),
        1,
        "no refetch within the min interval"
    );
}

#[tokio::test]
async fn unusable_keys_are_ignored() {
    let k1 = key("k1");
    let mut wrong_alg = k1.jwk.clone();
    wrong_alg["alg"] = json!("RS256");
    let mut enc = k1.jwk.clone();
    enc["use"] = json!("enc");
    enc["kid"] = json!("k-enc");
    let oct = json!({"kty": "oct", "k": "c2VjcmV0", "kid": "k-oct", "alg": "HS256"});
    let (addr, _) = jwks_server(Some(json!({"keys": [wrong_alg, enc, oct]}))).await;
    let gw = spawn(&yaml(addr, 30)).await;
    for kid in ["k1", "k-enc"] {
        let r = connect_with(&gw, &token(&k1, kid, "alice")).await;
        assert!(matches!(&r, Err(e) if e.contains("401")), "{kid}: {r:?}");
    }
}

#[tokio::test]
async fn never_loaded_keys_mean_503_not_401() {
    let k1 = key("k1");
    let (addr, server) = jwks_server(None).await;
    let gw = spawn(&yaml(addr, 1)).await;
    let r = connect_with(&gw, &token(&k1, "k1", "alice")).await;
    assert!(matches!(&r, Err(e) if e.contains("503")), "{r:?}");
    // The IdP comes back: after the min interval the next token loads it.
    *server.set.lock().unwrap() = Some(json!({"keys": [k1.jwk.clone()]}));
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    connect_with(&gw, &token(&k1, "k1", "alice"))
        .await
        .expect("recovered");
}

#[test]
fn jwks_config_is_validated() {
    let bad = [
        ("http to a public host", "{ kind: jwt, algorithm: RS256, jwks_url: \"http://idp.example.com/jwks\" }"),
        ("jwks with hmac", "{ kind: jwt, algorithm: HS256, jwks_url: \"https://idp/jwks\" }"),
        ("two key sources", "{ kind: jwt, algorithm: RS256, jwks_url: \"https://idp/jwks\", public_key_file: k.pem }"),
        ("refresh too short", "{ kind: jwt, algorithm: RS256, jwks_url: \"https://idp/jwks\", jwks_refresh_s: 10 }"),
    ];
    for (name, link) in bad {
        let yaml = format!("version: 1\ntenants:\n  - id: t\n    auth: [{link}]\n");
        assert!(
            wsprism_gateway::config::load_from_str(&yaml).is_err(),
            "{name}"
        );
    }
    let ok = "version: 1\ntenants:\n  - id: t\n    auth: [{ kind: jwt, algorithm: RS256, jwks_url: \"https://idp.example.com/.well-known/jwks.json\" }]\n";
    wsprism_gateway::config::load_from_str(ok).expect("https jwks");
}
