//! Gateway `sys.*` envelopes sent to clients.

use serde_json::json;

pub(crate) fn authed(tenant: &str, user: &str, sid: &str, trace_id: &str) -> String {
    json!({ "v": 1, "svc": "sys", "type": "authed", "data": { "tenant": tenant, "user": user, "sid": sid }, "trace_id": trace_id }).to_string()
}

pub(crate) fn error(code: &str, msg: &str, trace_id: &str) -> String {
    json!({ "v": 1, "svc": "sys", "type": "error", "data": { "code": code, "msg": msg }, "trace_id": trace_id }).to_string()
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
