//! Gateway `sys.*` envelopes sent to clients (shared by the session loop
//! and built-in services).

use serde_json::json;
use wsprism_core::error::{ClientCode, WsPrismError};

pub(crate) fn authed(tenant: &str, user: &str, sid: &str, trace_id: &str) -> String {
    json!({ "v": 1, "svc": "sys", "type": "authed", "data": { "tenant": tenant, "user": user, "sid": sid }, "trace_id": trace_id }).to_string()
}

/// `sys.error` with a catalogued code (see `ClientCode` and PROTOCOL.md).
/// `trace_id` may be empty when the error isn't tied to a session span.
pub(crate) fn error(code: ClientCode, msg: &str, trace_id: &str) -> String {
    let mut v = json!({ "v": 1, "svc": "sys", "type": "error", "data": { "code": code.as_str(), "msg": msg } });
    if !trace_id.is_empty() {
        v["trace_id"] = json!(trace_id);
    }
    v.to_string()
}

/// `sys.error` for an error value: catalogued code + client-safe message.
pub(crate) fn error_from(e: &WsPrismError, trace_id: &str) -> String {
    error(e.client_code(), e.client_message(), trace_id)
}

pub(crate) fn kicked(reason: &str, trace_id: &str) -> String {
    json!({ "v": 1, "svc": "sys", "type": "kicked", "data": { "reason": reason }, "trace_id": trace_id }).to_string()
}

pub(crate) fn joined(room: &str, trace_id: &str) -> String {
    json!({ "v": 1, "svc": "sys", "type": "joined", "room": room, "trace_id": trace_id })
        .to_string()
}

pub(crate) fn left(room: &str, trace_id: &str) -> String {
    json!({ "v": 1, "svc": "sys", "type": "left", "room": room, "trace_id": trace_id }).to_string()
}
