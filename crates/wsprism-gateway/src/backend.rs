//! Outbound HTTP to tenant backends (connect proxy, RPC proxy; ADR 0008 §6).
//!
//! One client per process (connection pooling, TLS session reuse). It never
//! follows redirects (a token must not be forwarded to a host the config
//! didn't name) and ignores `HTTP(S)_PROXY` environment variables (where
//! backend traffic goes is the config's decision, not the environment's).

use std::time::Duration;

use bytes::{Bytes, BytesMut};
use reqwest::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use wsprism_protocol::error::{Result, WsPrismError};

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
    /// `url` without user info and query values, for logs.
    shown: String,
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
            shown: shown_url(&url),
            url,
            timeout: Duration::from_millis(timeout_ms),
            bearer,
        })
    }
}

impl Endpoint {
    /// For logs: never the credential, nor a password or query values the
    /// URL itself may carry (`https://user:pw@host/jwks?key=...`).
    pub fn url(&self) -> &str {
        &self.shown
    }
}

/// `url` with the user info dropped and every query value replaced by `***`
/// (names kept: they help to tell endpoints apart).
fn shown_url(url: &reqwest::Url) -> String {
    let mut u = url.clone();
    let _ = u.set_username("");
    let _ = u.set_password(None);
    if u.query().is_some() {
        let names: Vec<String> = u.query_pairs().map(|(k, _)| k.into_owned()).collect();
        u.query_pairs_mut()
            .clear()
            .extend_pairs(names.iter().map(|k| (k.as_str(), "***")));
    }
    u.set_fragment(None);
    u.to_string()
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

    /// GET the endpoint; the status and (bounded) body, within its timeout.
    pub async fn get(&self, ep: &Endpoint) -> std::result::Result<(u16, Bytes), CallError> {
        let mut req = self.client.get(ep.url.clone());
        if let Some(b) = &ep.bearer {
            req = req.header(AUTHORIZATION, b.clone());
        }
        let call = async {
            let resp = req.send().await.map_err(|_| CallError::Transport)?;
            read_bounded(resp).await
        };
        tokio::time::timeout(ep.timeout, call)
            .await
            .unwrap_or(Err(CallError::Timeout))
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
            let resp = req.send().await.map_err(|_| CallError::Transport)?;
            read_bounded(resp).await
        };
        tokio::time::timeout(ep.timeout, call)
            .await
            .unwrap_or(Err(CallError::Timeout))
    }
}

/// Status and body, at most [`MAX_RESPONSE_BYTES`].
async fn read_bounded(mut resp: reqwest::Response) -> std::result::Result<(u16, Bytes), CallError> {
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
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn logged_urls_hide_credentials_and_query_values() {
        let ep = Endpoint::new(
            "jwks",
            "https://svc:s3cret@auth.example.com/keys?api_key=abc&v=2#x",
            1000,
            None,
        )
        .unwrap();
        assert_eq!(ep.url(), "https://auth.example.com/keys?api_key=***&v=***");
        let plain = Endpoint::new("rpc", "http://10.0.0.1:8080/rpc", 1000, None).unwrap();
        assert_eq!(plain.url(), "http://10.0.0.1:8080/rpc");
    }
}
