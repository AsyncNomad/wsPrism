//! JWT authenticator (ADR 0008 §5): `sub` is the user; the signature is
//! checked with one algorithm fixed in config (no `alg` confusion), plus
//! `exp`, `nbf`, and `iss` / `aud` when configured.
//!
//! Claims wsPrism reads besides the registered ones live under one
//! namespace object so they never collide with an issuer's own claims:
//! `"wsprism": {"tenant": "acme", "subscribe": [...], "publish": [...]}`.
//!
//! A token is "not mine" (next link) when it isn't a JWS compact token, or
//! its header names another algorithm or `kid` than this link's — so links
//! with different keys can be chained (key rotation, several issuers).
//! Keys come from a PEM file, an HMAC secret, or the issuer's JWKS URL
//! (picked by `kid`, cached and refreshed — see `jwks`, ADR 0012).

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use jsonwebtoken::errors::ErrorKind;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;

use super::jwks::JwksCache;
use super::{AuthError, AuthRequest, Authenticator, Identity, Permissions, TICKET_PREFIX};
use crate::backend::Backend;
use crate::config::schema::{JwtAlgorithm, JwtConfig};
use wsprism_protocol::error::{Result, WsPrismError};

/// Where verification keys come from.
enum Keys {
    Static(DecodingKey),
    Jwks(Arc<JwksCache>),
}

pub struct JwtAuthenticator {
    alg: Algorithm,
    kid: Option<String>,
    keys: Keys,
    validation: Validation,
    leeway: Duration,
    require_tenant: bool,
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
    exp: u64,
    #[serde(default)]
    wsprism: Option<WsprismClaims>,
}

#[derive(Deserialize)]
struct WsprismClaims {
    #[serde(default)]
    tenant: Option<String>,
    #[serde(default)]
    subscribe: Option<Vec<String>>,
    #[serde(default)]
    publish: Option<Vec<String>>,
}

fn algorithm(a: JwtAlgorithm) -> Algorithm {
    match a {
        JwtAlgorithm::HS256 => Algorithm::HS256,
        JwtAlgorithm::HS384 => Algorithm::HS384,
        JwtAlgorithm::HS512 => Algorithm::HS512,
        JwtAlgorithm::RS256 => Algorithm::RS256,
        JwtAlgorithm::RS384 => Algorithm::RS384,
        JwtAlgorithm::RS512 => Algorithm::RS512,
        JwtAlgorithm::PS256 => Algorithm::PS256,
        JwtAlgorithm::PS384 => Algorithm::PS384,
        JwtAlgorithm::PS512 => Algorithm::PS512,
        JwtAlgorithm::ES256 => Algorithm::ES256,
        JwtAlgorithm::ES384 => Algorithm::ES384,
        JwtAlgorithm::EdDSA => Algorithm::EdDSA,
    }
}

fn bad(msg: String) -> WsPrismError {
    WsPrismError::BadRequest(msg)
}

/// The HMAC secret: from a file (one trailing newline ignored) or an
/// environment variable. At least as long as the hash output (RFC 7518 §3.2).
fn load_secret(cfg: &JwtConfig) -> Result<Vec<u8>> {
    let secret = match (&cfg.secret_file, &cfg.secret_env) {
        (Some(path), None) => {
            let mut s =
                std::fs::read(path).map_err(|e| bad(format!("jwt secret_file {path}: {e}")))?;
            while matches!(s.last(), Some(b'\n' | b'\r')) {
                s.pop();
            }
            s
        }
        (None, Some(var)) => std::env::var(var)
            .map_err(|_| bad(format!("jwt secret_env {var} is not set")))?
            .into_bytes(),
        _ => {
            return Err(bad(
                "jwt: set exactly one of secret_file / secret_env".into()
            ))
        }
    };
    let min = match cfg.algorithm {
        JwtAlgorithm::HS384 => 48,
        JwtAlgorithm::HS512 => 64,
        _ => 32,
    };
    if secret.len() < min {
        return Err(bad(format!(
            "jwt {:?} secret must be at least {min} bytes (got {})",
            cfg.algorithm,
            secret.len()
        )));
    }
    Ok(secret)
}

fn load_public_key(cfg: &JwtConfig) -> Result<DecodingKey> {
    let path = cfg
        .public_key_file
        .as_deref()
        .ok_or_else(|| bad("jwt: public_key_file is required".into()))?;
    let pem = std::fs::read(path).map_err(|e| bad(format!("jwt public_key_file {path}: {e}")))?;
    let key = match cfg.algorithm {
        JwtAlgorithm::ES256 | JwtAlgorithm::ES384 => DecodingKey::from_ec_pem(&pem),
        JwtAlgorithm::EdDSA => DecodingKey::from_ed_pem(&pem),
        _ => DecodingKey::from_rsa_pem(&pem),
    };
    key.map_err(|e| {
        bad(format!(
            "jwt public_key_file {path}: not a {:?} PEM public key ({e})",
            cfg.algorithm
        ))
    })
}

impl JwtAuthenticator {
    /// Build from config, loading the key (startup / `--check-config` fail
    /// on a missing or malformed key).
    pub fn from_config(cfg: &JwtConfig, backend: &Backend) -> Result<Self> {
        cfg.validate()?;
        let alg = algorithm(cfg.algorithm);
        let keys = if cfg.algorithm.is_hmac() {
            Keys::Static(DecodingKey::from_secret(&load_secret(cfg)?))
        } else if let Some(url) = &cfg.jwks_url {
            Keys::Jwks(Arc::new(JwksCache::new(
                backend,
                url,
                alg,
                Duration::from_secs(cfg.jwks_refresh_s),
                Duration::from_secs(cfg.jwks_min_refetch_s),
                Duration::from_secs(cfg.jwks_max_stale_s),
            )?))
        } else {
            Keys::Static(load_public_key(cfg)?)
        };
        let mut validation = Validation::new(alg);
        validation.leeway = cfg.leeway_s;
        validation.validate_nbf = true;
        // jsonwebtoken only compares `iss` / `aud` when present, so a
        // configured one must also be required.
        let mut required = vec!["exp", "sub"];
        if let Some(iss) = &cfg.issuer {
            validation.set_issuer(&[iss]);
            required.push("iss");
        }
        match &cfg.audience {
            Some(aud) => {
                validation.set_audience(&[aud]);
                required.push("aud");
            }
            // Don't reject tokens that carry an `aud` we don't check.
            None => validation.validate_aud = false,
        }
        validation.set_required_spec_claims(&required);
        Ok(Self {
            alg,
            kid: cfg.kid.clone(),
            keys,
            validation,
            leeway: Duration::from_secs(cfg.leeway_s),
            require_tenant: cfg.require_tenant,
        })
    }
}

fn reason(kind: &ErrorKind) -> &'static str {
    match kind {
        ErrorKind::ExpiredSignature => "jwt expired",
        ErrorKind::ImmatureSignature => "jwt not yet valid",
        ErrorKind::InvalidSignature => "jwt bad signature",
        ErrorKind::InvalidIssuer => "jwt wrong issuer",
        ErrorKind::InvalidAudience => "jwt wrong audience",
        ErrorKind::MissingRequiredClaim(_) => "jwt missing claim",
        _ => "jwt malformed",
    }
}

#[async_trait]
impl Authenticator for JwtAuthenticator {
    fn kind(&self) -> &'static str {
        "jwt"
    }

    async fn authenticate(
        &self,
        req: &AuthRequest<'_>,
    ) -> std::result::Result<Identity, AuthError> {
        let token = req.token;
        if token.starts_with(TICKET_PREFIX) || token.bytes().filter(|&b| b == b'.').count() != 2 {
            return Err(AuthError::NotMine);
        }
        let header = jsonwebtoken::decode_header(token)
            .map_err(|_| AuthError::Rejected("jwt malformed header"))?;
        if header.alg != self.alg || (self.kid.is_some() && header.kid != self.kid) {
            return Err(AuthError::NotMine);
        }
        let jwks_key;
        let key = match &self.keys {
            Keys::Static(k) => k,
            Keys::Jwks(cache) => {
                // Without a kid the token can't be matched to a JWKS key:
                // probably another issuer's — let the next link try.
                let kid = header.kid.as_deref().ok_or(AuthError::NotMine)?;
                jwks_key = cache.key_for(kid).await?;
                &*jwks_key
            }
        };
        let claims = jsonwebtoken::decode::<Claims>(token, key, &self.validation)
            .map_err(|e| AuthError::Rejected(reason(e.kind())))?
            .claims;
        if !crate::ident::is_safe_id(&claims.sub) {
            return Err(AuthError::Rejected("jwt sub is not a valid user id"));
        }
        let ws = claims.wsprism.unwrap_or(WsprismClaims {
            tenant: None,
            subscribe: None,
            publish: None,
        });
        match ws.tenant.as_deref() {
            Some(t) if t != req.tenant => {
                return Err(AuthError::Rejected("jwt for another tenant"))
            }
            None if self.require_tenant => {
                return Err(AuthError::Rejected("jwt missing tenant claim"))
            }
            _ => {}
        }
        Ok(Identity {
            user_id: claims.sub,
            permissions: Permissions::from_lists(ws.subscribe, ws.publish),
            // The session may outlive `exp` by the tolerated skew, no more.
            expires_at: Some(UNIX_EPOCH + Duration::from_secs(claims.exp) + self.leeway),
        })
    }
}

/// Monotonic deadline (`now_ms` scale) for a wall-clock expiry.
pub fn expiry_deadline_ms(expires_at: SystemTime, now_ms: u64) -> u64 {
    let left = expires_at
        .duration_since(SystemTime::now())
        .unwrap_or(Duration::ZERO);
    now_ms.saturating_add(left.as_millis() as u64)
}
