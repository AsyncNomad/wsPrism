//! Server API: how a backend talks to the gateway (ADR 0008 §3).
//!
//! Served on its own listener (`gateway.api.listen`), never on the public
//! WebSocket port. Every request carries `Authorization: Bearer <api key>`;
//! the key's SHA-256 selects the tenant, so a backend can only reach its own
//! tenant's rooms and users.

use std::sync::Arc;

use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{header::AUTHORIZATION, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use wsprism_protocol::protocol::hot::{encode_hot_frame_v2, HotOutV2};

use crate::app_state::AppState;
use crate::auth::{Identity, IssueError, Permissions, DEFAULT_TICKET_TTL, MAX_TICKET_TTL};
use wsprism_engine::spi::sys;
use wsprism_engine::spi::{close_code, CloseSpec, HotOut};
use wsprism_engine::spi::{CloseRequest, RealtimeCore};
use wsprism_engine::{Outgoing, Payload, QoS};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/publish", post(publish))
        .route("/api/v1/disconnect", post(disconnect))
        .route("/api/v1/tickets", post(tickets))
        .route("/api/v1/rooms/:room/presence", get(presence))
        // Operations on the internal listener (no API key: scrapers and
        // probes; the listener itself must not be public).
        .route("/metrics", get(crate::ops::metrics))
        .route("/healthz", get(crate::ops::healthz))
        .route("/readyz", get(crate::ops::readyz))
        .with_state(state)
}

/// SHA-256 of an API key, as configured (lowercase hex compared as bytes).
pub fn key_digest(key: &str) -> [u8; 32] {
    let d = ring::digest::digest(&ring::digest::SHA256, key.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    out
}

/// The tenant an API request acts for (from its key).
pub struct ApiTenant(pub String);

#[axum::async_trait]
impl FromRequestParts<AppState> for ApiTenant {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        app: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let unauthorized = || {
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "unauthorized"})),
            )
                .into_response()
        };
        let key = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, k)| k.trim())
            .filter(|k| !k.is_empty())
            .ok_or_else(unauthorized)?;
        // Lookup by digest: timing reveals nothing usable about the key.
        app.api_key_tenant(&key_digest(key))
            .map(ApiTenant)
            .ok_or_else(unauthorized)
    }
}

fn bad_request(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response()
}

/// The error response for an unacceptable room name, if it is one.
fn room_error(room: &str) -> Option<Response> {
    wsprism_engine::validate_room_name(room)
        .err()
        .map(|e| bad_request(e.client_message()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishReq {
    room: String,
    /// Ext message body (JSON). Exactly one of `data` / `hot`.
    #[serde(default)]
    data: Option<Value>,
    #[serde(default)]
    hot: Option<HotReq>,
    /// `reliable` (default for `data`) or `lossy` (default for `hot`).
    #[serde(default)]
    qos: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HotReq {
    svc: u8,
    op: u8,
    #[serde(default)]
    key: Option<u32>,
    /// Payload, standard base64.
    data_b64: String,
}

async fn publish(
    State(app): State<AppState>,
    ApiTenant(tenant): ApiTenant,
    Json(req): Json<PublishReq>,
) -> Response {
    if let Some(r) = room_error(&req.room) {
        return r;
    }
    let qos = match req.qos.as_deref() {
        None => None,
        Some("reliable") => Some(QoS::Reliable),
        Some("lossy") => Some(QoS::Lossy),
        Some(_) => return bad_request("qos must be reliable or lossy"),
    };
    let core = app.realtime();
    let room_key = RealtimeCore::room_key(&tenant, &req.room);
    let out = match (req.data, req.hot) {
        (Some(data), None) => Outgoing {
            qos: qos.unwrap_or(QoS::Reliable),
            payload: Payload::TextJson(json!({
                "v": 1, "svc": "server", "type": "publish", "room": req.room, "data": data
            })),
        },
        (None, Some(hot)) => {
            let Ok(payload) = base64::engine::general_purpose::STANDARD.decode(&hot.data_b64)
            else {
                return bad_request("hot.data_b64 is not valid base64");
            };
            let payload = bytes::Bytes::from(payload);
            // No room yet = nobody to deliver to.
            let Some(chan) = core.presence.room_id(&room_key) else {
                return Json(json!({"recipients": 0})).into_response();
            };
            let v2 = encode_hot_frame_v2(&HotOutV2 {
                chan,
                svc_id: hot.svc,
                opcode: hot.op,
                key: hot.key,
                seq: None,
                sender: 0, // the server
                payload: payload.clone(),
            });
            Outgoing {
                qos: qos.unwrap_or(QoS::Lossy),
                payload: Payload::Hot(Arc::new(HotOut {
                    v1: payload,
                    v2,
                    slot_key: hot.key.map(|k| (chan, 0, hot.svc, k)),
                })),
            }
        }
        _ => return bad_request("exactly one of data / hot is required"),
    };
    match core.publish_room(&room_key, out) {
        Ok(n) => Json(json!({"recipients": n})).into_response(),
        Err(e) => bad_request(e.client_message()),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DisconnectReq {
    user: String,
    /// Only the session using this client sid (default: all of the user's).
    #[serde(default)]
    sid: Option<String>,
    /// Shown to the client in `sys.kicked`.
    #[serde(default)]
    reason: Option<String>,
}

async fn disconnect(
    State(app): State<AppState>,
    ApiTenant(tenant): ApiTenant,
    Json(req): Json<DisconnectReq>,
) -> Response {
    let reason = req.reason.as_deref().unwrap_or("disconnected");
    if reason.len() > 123 {
        return bad_request("reason must be at most 123 bytes");
    }
    let user_key = format!("{tenant}::{}", req.user);
    let conns = app
        .realtime()
        .sessions
        .user_connections(&user_key, req.sid.as_deref());
    let notice = sys::kicked(reason, "").to_string();
    for c in &conns {
        c.signals.request_close(CloseRequest::Spec(CloseSpec::new(
            close_code::POLICY,
            "disconnected",
            Some(notice.clone()),
        )));
    }
    Json(json!({"closed": conns.len()})).into_response()
}

async fn presence(
    State(app): State<AppState>,
    ApiTenant(tenant): ApiTenant,
    Path(room): Path<String>,
) -> Response {
    if let Some(r) = room_error(&room) {
        return r;
    }
    let room_key = RealtimeCore::room_key(&tenant, &room);
    let prefix = format!("{tenant}::");
    let users: Vec<String> = app
        .realtime()
        .presence
        .user_keys_in(&room_key)
        .into_iter()
        .filter_map(|k| k.strip_prefix(&prefix).map(str::to_string))
        .collect();
    Json(json!({"room": room, "users": users})).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TicketReq {
    user: String,
    /// Seconds, 1..=300 (default 30).
    #[serde(default)]
    ttl_s: Option<u64>,
    #[serde(default)]
    permissions: Option<PermissionsReq>,
}

/// Room patterns (exact, or trailing `*`); an omitted list = unrestricted.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PermissionsReq {
    #[serde(default)]
    subscribe: Option<Vec<String>>,
    #[serde(default)]
    publish: Option<Vec<String>>,
}

async fn tickets(
    State(app): State<AppState>,
    ApiTenant(tenant): ApiTenant,
    Json(req): Json<TicketReq>,
) -> Response {
    if !crate::ident::is_safe_id(&req.user) {
        return bad_request("user must be 1-64 characters of [A-Za-z0-9_-]");
    }
    let ttl = match req.ttl_s {
        None => DEFAULT_TICKET_TTL,
        Some(s) if (1..=MAX_TICKET_TTL.as_secs()).contains(&s) => std::time::Duration::from_secs(s),
        Some(_) => return bad_request("ttl_s must be 1-300"),
    };
    if !app.auth(&tenant).is_some_and(|c| c.has_kind("ticket")) {
        return bad_request(
            "this tenant does not accept tickets (add {kind: ticket} to tenants[].auth)",
        );
    }
    let permissions = req
        .permissions
        .map(|p| Permissions::from_lists(p.subscribe, p.publish))
        .unwrap_or_default();
    let identity = Identity {
        user_id: req.user,
        permissions,
        expires_at: None,
    };
    match app.tickets().issue(&tenant, identity, ttl) {
        Ok(ticket) => {
            Json(json!({"ticket": ticket, "expires_in_s": ttl.as_secs()})).into_response()
        }
        Err(IssueError::Full) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "too many outstanding tickets"})),
        )
            .into_response(),
        Err(IssueError::Random) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "random source unavailable"})),
        )
            .into_response(),
    }
}
