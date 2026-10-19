//! JWKS key cache for the JWT authenticator (ADR 0012 §1).
//!
//! Keys are fetched on first use and refreshed in the background once older
//! than `jwks_refresh_s`; a token with an unknown `kid` triggers one
//! immediate refetch (an IdP may have just rotated), at most every
//! `jwks_min_refetch_s` so random `kid`s cannot make us hammer the IdP. A failed
//! fetch keeps the last good key set. Only signing keys whose type and
//! `alg` fit the link's pinned algorithm are kept; symmetric (`oct`) keys
//! never are.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::{AlgorithmParameters, JwkSet, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey};

use super::AuthError;
use crate::backend::{Backend, Endpoint};
use wsprism_protocol::error::Result;

type KeySet = Arc<HashMap<String, Arc<DecodingKey>>>;

pub(crate) struct JwksCache {
    backend: Backend,
    endpoint: Endpoint,
    alg: Algorithm,
    refresh: Duration,
    min_refetch: Duration,
    /// `None` until a fetch succeeded once.
    keys: RwLock<Option<KeySet>>,
    epoch: Instant,
    /// Milliseconds since `epoch` of the last fetch attempt / success.
    last_attempt_ms: AtomicU64,
    last_success_ms: AtomicU64,
    refreshing: AtomicBool,
}

impl JwksCache {
    pub(crate) fn new(
        backend: &Backend,
        url: &str,
        alg: Algorithm,
        refresh: Duration,
        min_refetch: Duration,
    ) -> Result<Self> {
        Ok(Self {
            backend: backend.clone(),
            endpoint: Endpoint::new("jwt jwks", url, 5_000, None)?,
            alg,
            refresh,
            min_refetch,
            keys: RwLock::new(None),
            epoch: Instant::now(),
            last_attempt_ms: AtomicU64::new(0),
            last_success_ms: AtomicU64::new(0),
            refreshing: AtomicBool::new(false),
        })
    }

    fn now_ms(&self) -> u64 {
        // +1 so "never" (0) is always older than now.
        self.epoch.elapsed().as_millis() as u64 + 1
    }

    fn current(&self) -> Option<KeySet> {
        self.keys.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Claim the right to fetch now if the last attempt is old enough.
    fn may_fetch(&self) -> bool {
        let now = self.now_ms();
        let last = self.last_attempt_ms.load(Ordering::Relaxed);
        let due = last == 0 || now.saturating_sub(last) >= self.min_refetch.as_millis() as u64;
        due && self
            .last_attempt_ms
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }

    /// The verification key for `kid`.
    pub(crate) async fn key_for(
        self: &Arc<Self>,
        kid: &str,
    ) -> std::result::Result<Arc<DecodingKey>, AuthError> {
        if self.current().is_none() && self.may_fetch() {
            self.fetch().await;
        }
        let Some(keys) = self.current() else {
            return Err(AuthError::Unavailable("jwks not loaded yet"));
        };
        self.refresh_if_stale();
        if let Some(k) = keys.get(kid) {
            return Ok(Arc::clone(k));
        }
        // Unknown kid: the IdP may have rotated. One rate-limited refetch.
        if self.may_fetch() {
            self.fetch().await;
            if let Some(k) = self.current().and_then(|ks| ks.get(kid).cloned()) {
                return Ok(k);
            }
        }
        Err(AuthError::NotMine)
    }

    fn refresh_if_stale(self: &Arc<Self>) {
        let age = self
            .now_ms()
            .saturating_sub(self.last_success_ms.load(Ordering::Relaxed));
        if age < self.refresh.as_millis() as u64 || self.refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            this.last_attempt_ms.store(this.now_ms(), Ordering::Relaxed);
            this.fetch().await;
            this.refreshing.store(false, Ordering::Release);
        });
    }

    /// Fetch and install the key set; on failure keep the old one.
    async fn fetch(&self) -> bool {
        let url = self.endpoint.url().to_string();
        let set = match self.backend.get(&self.endpoint).await {
            Ok((200, body)) => match serde_json::from_slice::<JwkSet>(&body) {
                Ok(set) => set,
                Err(e) => {
                    tracing::warn!(%url, error = %e, "jwks: not a JWK set; keeping the previous keys");
                    return false;
                }
            },
            Ok((status, _)) => {
                tracing::warn!(%url, status, "jwks: fetch failed; keeping the previous keys");
                return false;
            }
            Err(e) => {
                tracing::warn!(%url, error = ?e, "jwks: fetch failed; keeping the previous keys");
                return false;
            }
        };
        let keys = usable_keys(&set, self.alg);
        if keys.is_empty() {
            tracing::warn!(%url, "jwks: no usable signing key for the configured algorithm");
        }
        *self.keys.write().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(keys));
        self.last_success_ms.store(self.now_ms(), Ordering::Relaxed);
        true
    }
}

/// Signing keys with a `kid`, of a type that can verify `alg`, whose own
/// `alg` (if any) is `alg`. Symmetric keys are never taken from a URL.
fn usable_keys(set: &JwkSet, alg: Algorithm) -> HashMap<String, Arc<DecodingKey>> {
    let alg_name = format!("{alg:?}");
    set.keys
        .iter()
        .filter_map(|jwk| {
            let kid = jwk.common.key_id.clone()?;
            if matches!(
                jwk.common.public_key_use,
                Some(PublicKeyUse::Encryption) | Some(PublicKeyUse::Other(_))
            ) {
                return None;
            }
            if let Some(ka) = &jwk.common.key_algorithm {
                if format!("{ka:?}") != alg_name {
                    return None;
                }
            }
            let fits = match (&jwk.algorithm, alg) {
                (
                    AlgorithmParameters::RSA(_),
                    Algorithm::RS256
                    | Algorithm::RS384
                    | Algorithm::RS512
                    | Algorithm::PS256
                    | Algorithm::PS384
                    | Algorithm::PS512,
                ) => true,
                (AlgorithmParameters::EllipticCurve(_), Algorithm::ES256 | Algorithm::ES384) => {
                    true
                }
                (AlgorithmParameters::OctetKeyPair(_), Algorithm::EdDSA) => true,
                _ => false, // includes every `oct` (symmetric) key
            };
            if !fits {
                return None;
            }
            DecodingKey::from_jwk(jwk).ok().map(|k| (kid, Arc::new(k)))
        })
        .collect()
}
