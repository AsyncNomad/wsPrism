//! RPC proxy (ADR 0008 §6): Ext requests matching `tenants[].rpc.allow` are
//! answered by the tenant's backend over HTTP, so business logic can live
//! in any language.
//!
//! The session loop never waits for the backend (C1/C9): the call runs on
//! its own task and answers through a [`DeferredReply`]. Each session has at
//! most `max_in_flight` calls pending; beyond that the request is answered
//! `RATE_LIMITED` at once.
//!
//! Backend contract: `POST url {tenant, user, sid, svc, type, room, data}`
//! -> `200 {"data": ...}` (sent as `sys.reply`) or `200 {"error": {"code":
//! "NOT_FOUND", "msg": "..."}}` (sent as `sys.error` with that code). Any
//! other outcome is `UNAVAILABLE`; the backend's body is never forwarded.

use std::sync::Arc;

use serde_json::{json, Value};
use wsprism_core::error::{ClientCode, Result, WsPrismError};
use wsprism_core::protocol::text::Envelope;

use crate::backend::{Backend, Endpoint};
use crate::config::schema::RpcConfig;
use crate::policy::allowlist::{compile_ext_rules, is_ext_allowed, ExtRule};
use crate::realtime::core::DeferredReply;
use crate::realtime::RealtimeCtx;
use crate::sys;

/// Longest backend error message forwarded to a client.
const MAX_ERROR_MSG: usize = 512;

pub struct RpcProxy {
    backend: Backend,
    endpoint: Endpoint,
    rules: Vec<ExtRule>,
    max_in_flight: u32,
}

/// A backend's own error code: `[A-Z0-9_]{1,32}` (like the catalogued ones).
fn valid_code(c: &str) -> bool {
    (1..=32).contains(&c.len())
        && c.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

impl RpcProxy {
    /// `reserved`: service names the gateway handles itself; RPC may not
    /// claim them (a built-in service would otherwise be silently shadowed).
    pub fn from_config(cfg: &RpcConfig, backend: &Backend, reserved: &[&str]) -> Result<Self> {
        cfg.validate()?;
        let rules = compile_ext_rules(&cfg.allow)?;
        if let Some(r) = rules
            .iter()
            .find(|r| r.svc == "sys" || reserved.contains(&r.svc.as_str()))
        {
            return Err(WsPrismError::BadRequest(format!(
                "rpc.allow: {} is a built-in service",
                r.svc
            )));
        }
        Ok(Self {
            backend: backend.clone(),
            endpoint: Endpoint::new("rpc", &cfg.url, cfg.timeout_ms, cfg.secret_env.as_deref())?,
            rules,
            max_in_flight: cfg.max_in_flight,
        })
    }

    pub fn handles(&self, svc: &str, msg_type: &str) -> bool {
        is_ext_allowed(&self.rules, svc, msg_type)
    }

    /// True if some rule names service `svc` (boot-time allowlist check).
    pub fn covers_svc(&self, svc: &str) -> bool {
        self.rules.iter().any(|r| r.svc == svc)
    }

    /// Start a call for `env`; the answer arrives later. Errs only when the
    /// session's budget is exhausted.
    pub fn call(self: &Arc<Self>, ctx: &RealtimeCtx, env: Envelope) -> Result<()> {
        let reply = ctx
            .defer_reply(self.max_in_flight)
            .ok_or(WsPrismError::RateLimited)?;
        let body = json!({
            "tenant": ctx.tenant(),
            "user": ctx.user(),
            "sid": ctx.session_id(),
            "svc": env.svc,
            "type": env.msg_type,
            "room": env.room,
            "data": env.data,
        });
        let trace_id = ctx.trace_id.to_string();
        let this = Arc::clone(self);
        tokio::spawn(async move { this.round_trip(body, reply, &trace_id).await });
        Ok(())
    }

    async fn round_trip(&self, body: Value, reply: DeferredReply, trace_id: &str) {
        let unavailable =
            || sys::error(ClientCode::Unavailable, "rpc backend unavailable", trace_id);
        let url = self.endpoint.url();
        let answer = match self.backend.post_json(&self.endpoint, &body).await {
            Ok((200, bytes)) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(v) if v.get("error").is_some() => {
                    let e = &v["error"];
                    let code = e["code"]
                        .as_str()
                        .filter(|c| valid_code(c))
                        .unwrap_or("RPC_ERROR");
                    let msg = truncate(e["msg"].as_str().unwrap_or(""), MAX_ERROR_MSG);
                    sys::error_code(code, msg, trace_id)
                }
                Ok(mut v) if v.is_object() => sys::reply(v["data"].take(), trace_id),
                _ => {
                    tracing::warn!(%url, "rpc backend sent a malformed reply");
                    unavailable()
                }
            },
            Ok((status, _)) => {
                tracing::warn!(%url, status, "rpc backend answered with an error status");
                unavailable()
            }
            Err(e) => {
                tracing::warn!(%url, error = ?e, "rpc backend call failed");
                unavailable()
            }
        };
        reply.send(answer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_error_codes_are_validated_and_messages_bounded() {
        assert!(valid_code("NOT_FOUND"));
        assert!(!valid_code("not found"));
        assert!(!valid_code(""));
        assert!(!valid_code(&"A".repeat(33)));
        assert_eq!(truncate("héllo", 2), "h");
        assert_eq!(truncate("abc", 10), "abc");
    }
}
