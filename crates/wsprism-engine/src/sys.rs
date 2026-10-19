//! Gateway `sys.*` envelopes sent to clients (shared by the session loop,
//! the realtime core and built-in services).
//!
//! Builders return JSON values so request correlation (`reply_to`) is added
//! in one place, [`reply_to`], before serialization.

use serde_json::{json, Value};
use wsprism_protocol::error::{ClientCode, WsPrismError};

/// Mark `msg` as the answer to the request with sequence number `seq`
/// (the client's `seq` on Ext envelopes / Hot frames). No-op for `None`.
pub fn reply_to(mut msg: Value, seq: Option<u64>) -> Value {
    if let Some(seq) = seq {
        msg["reply_to"] = json!(seq);
    }
    msg
}

/// `sys.authed`: the greeting after admission.
pub fn authed(tenant: &str, user: &str, sid: &str, trace_id: &str) -> Value {
    json!({ "v": 1, "svc": "sys", "type": "authed", "data": { "tenant": tenant, "user": user, "sid": sid }, "trace_id": trace_id })
}

/// `sys.error` with a catalogued code (see `ClientCode` and PROTOCOL.md).
/// `trace_id` may be empty when the error isn't tied to a session span.
pub fn error(code: ClientCode, msg: &str, trace_id: &str) -> Value {
    error_code(code.as_str(), msg, trace_id)
}

/// `sys.error` with a code outside the catalogue (an RPC backend's own
/// application error code; validated by the caller).
pub fn error_code(code: &str, msg: &str, trace_id: &str) -> Value {
    let mut v =
        json!({ "v": 1, "svc": "sys", "type": "error", "data": { "code": code, "msg": msg } });
    if !trace_id.is_empty() {
        v["trace_id"] = json!(trace_id);
    }
    v
}

/// `sys.error` for an error value: catalogued code + client-safe message.
pub fn error_from(e: &WsPrismError, trace_id: &str) -> Value {
    error(e.client_code(), e.client_message(), trace_id)
}

/// `sys.reply`: a backend's answer to a request (RPC proxy).
pub fn reply(data: Value, trace_id: &str) -> Value {
    json!({ "v": 1, "svc": "sys", "type": "reply", "data": data, "trace_id": trace_id })
}

/// `sys.kicked`: the session is being closed for `reason`.
pub fn kicked(reason: &str, trace_id: &str) -> Value {
    json!({ "v": 1, "svc": "sys", "type": "kicked", "data": { "reason": reason }, "trace_id": trace_id })
}

/// `sys.joined` (v1).
pub fn joined(room: &str, trace_id: &str) -> Value {
    json!({ "v": 1, "svc": "sys", "type": "joined", "room": room, "trace_id": trace_id })
}

/// Protocol v2 `sys.joined`: adds the room id (Hot `chan`) and this
/// membership's id (Hot `sender`), ADR 0006 §3.
pub fn joined_v2(room: &str, room_id: u32, member_id: u32, trace_id: &str) -> Value {
    json!({ "v": 1, "svc": "sys", "type": "joined", "room": room, "chan": room_id, "member_id": member_id, "trace_id": trace_id })
}

/// `sys.left`.
pub fn left(room: &str, trace_id: &str) -> Value {
    json!({ "v": 1, "svc": "sys", "type": "left", "room": room, "trace_id": trace_id })
}
