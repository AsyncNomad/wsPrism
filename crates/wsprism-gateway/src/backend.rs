//! Outbound HTTP to tenant backends (connect proxy, RPC proxy; ADR 0008 §6).
//!
//! One client per process (connection pooling, TLS session reuse). It never
//! follows redirects (a token must not be forwarded to a host the config
//! didn't name) and ignores `HTTP(S)_PROXY` environment variables (where
//! backend traffic goes is the config's decision, not the environment's).

use std::time::Duration;

use bytes::{Bytes, BytesMut};
use reqwest::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use wsprism_core::error::{Result, WsPrismError};

/// Largest backend response read; anything bigger is a failed call.
pub const MAX_RESPONSE_BYTES: usize = 1 << 20;

/// Why a call produced no response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallError {
    Timeout,
    /// Connection refused / reset, TLS failure, ...
    Transport,
    TooLarge,
}

#[derive(Clone)]
pub struct Backend {
    client: reqwest::Client,
}

/// One configured backend URL with its timeout and credential.
pub struct Endpoint {
    url: reqwest::Url,
    timeout: Duration,
    bearer: Option<HeaderValue>,
}

impl Endpoint {
    /// `secret_env` names an environment variable read now (startup), whose
    /// value is sent as `Authorization: Bearer <value>`.
    pub fn new(what: &str, url: &str, timeout_ms: u64, secret_env: Option<&str>) -> Result<Self> {
        let url = reqwest::Url::parse(url)
            .map_err(|e| WsPrismError::BadRequest(format!("{what}.url: {e}")))?;
        let bearer = match secret_env {
            None => None,
            Some(var) => {
                let secret = std::env::var(var).map_err(|_| {
                    WsPrismError::BadRequest(format!("{what}.secret_env {var} is not set"))
                })?;
                let mut v = HeaderValue::from_str(&format!("Bearer {secret}")).map_err(|_| {
                    WsPrismError::BadRequest(format!(
                        "{what}.secret_env {var}: not a valid header value"
                    ))
                })?;
                v.set_sensitive(true);
                Some(v)
            }
        };
        Ok(Self {
            url,
            timeout: Duration::from_millis(timeout_ms),
            bearer,
        })
    }
}

impl Endpoint {
    /// For logs (never includes the credential).
    pub fn url(&self) -> &str {
        self.url.as_str()
    }
}

impl Backend {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| WsPrismError::Internal(format!("http client: {e}")))?;
        Ok(Self { client })
    }

    /// POST `body` as JSON; the status and (bounded) body, all within the
    /// endpoint's timeout.
    pub async fn post_json(
        &self,
        ep: &Endpoint,
        body: &serde_json::Value,
    ) -> std::result::Result<(u16, Bytes), CallError> {
        let body = serde_json::to_vec(body).map_err(|_| CallError::Transport)?;
        let mut req = self
            .client
            .post(ep.url.clone())
            .header(CONTENT_TYPE, "application/json")
            .body(body);
        if let Some(b) = &ep.bearer {
            req = req.header(AUTHORIZATION, b.clone());
        }
        let call = async {
            let mut resp = req.send().await.map_err(|_| CallError::Transport)?;
            let status = resp.status().as_u16();
            if resp
                .content_length()
                .is_some_and(|n| n > MAX_RESPONSE_BYTES as u64)
            {
                return Err(CallError::TooLarge);
            }
            let mut buf = BytesMut::new();
            while let Some(chunk) = resp.chunk().await.map_err(|_| CallError::Transport)? {
                if buf.len() + chunk.len() > MAX_RESPONSE_BYTES {
                    return Err(CallError::TooLarge);
                }
                buf.extend_from_slice(&chunk);
            }
            Ok((status, buf.freeze()))
        };
        tokio::time::timeout(ep.timeout, call)
            .await
            .unwrap_or(Err(CallError::Timeout))
    }
}
