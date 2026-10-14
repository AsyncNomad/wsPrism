//! Admission: sid takeover, per-user session limit, and registration.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::extract::ws::Message;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};
use wsprism_core::error::{Result, WsPrismError};

use super::{Session, SessionIdentity};
use crate::app_state::AppState;
use crate::obs::metrics::GatewayMetrics;
use crate::policy::engine::OnExceed;
use crate::realtime::core::Connection;
use crate::realtime::{close_code, CloseSpec, RealtimeCore, RealtimeCtx};
use crate::sys;

/// Per-connection counter making internal session keys unique (S4).
static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

pub(super) enum Admission {
    Admitted(Box<Session>),
    /// Not admitted: close the socket with this reason.
    Rejected(CloseSpec),
}

/// RAII guard that tears down session and presence entries on exit.
pub(super) struct SessionCleanup {
    core: Arc<RealtimeCore>,
    tenant_id: String,
    user_key: String,
    session_key: String,
    metrics: Arc<GatewayMetrics>,
}

impl Drop for SessionCleanup {
    fn drop(&mut self) {
        // C2: whoever removes the registry entry owns the gauge decrement.
        // If this session was already evicted (kick), the evictor did it.
        let removed = self
            .core
            .sessions
            .remove_session(&self.user_key, &self.session_key)
            .is_some();
        // Presence cleanup is unconditional: an evicted session may have
        // re-joined rooms before its loop ended.
        self.core
            .presence
            .cleanup_session(&self.tenant_id, &self.user_key, &self.session_key);
        if removed {
            self.metrics
                .ws_active_sessions
                .dec(&[("tenant", &self.tenant_id)]);
        }
        tracing::debug!(s=%self.session_key, "session raii cleanup done");
    }
}

/// Evict a live session: unregister it (owning the gauge decrement, see C2),
/// clear its presence, then tell it why and close it.
fn evict_session(
    core: &RealtimeCore,
    metrics: &GatewayMetrics,
    tenant: &str,
    user_key: &str,
    victim_key: &str,
    reason: &str,
    trace_id: &str,
) {
    let Some(conn) = core.sessions.remove_session(user_key, victim_key) else {
        return; // already gone (raced with its own cleanup or another evictor)
    };
    core.presence.cleanup_session(tenant, user_key, victim_key);
    metrics.ws_active_sessions.dec(&[("tenant", tenant)]);
    // C8: ask the victim's loop to stop and close itself (notice + 1008).
    let _ = conn.ctl.try_send(CloseSpec::new(
        close_code::POLICY,
        reason,
        Some(sys::kicked(reason, trace_id)),
    ));
}

impl Session {
    /// Apply session policies and register the connection. On success the
    /// `sys.authed` greeting is already queued.
    pub(super) fn admit(
        app: &AppState,
        id: SessionIdentity,
        out_tx: mpsc::Sender<Message>,
        ctl_tx: mpsc::Sender<CloseSpec>,
    ) -> Result<Admission> {
        let policy = app
            .tenant_policy(&id.tenant)
            .ok_or_else(|| WsPrismError::BadRequest("unknown tenant".into()))?;
        let t_cfg = app
            .tenant(&id.tenant)
            .ok_or_else(|| WsPrismError::BadRequest("unknown tenant".into()))?;
        let core = app.realtime();
        let metrics = app.metrics();
        let user_key = format!("{}::{}", id.tenant, id.user_id);
        // S4: the internal key is unique per connection. The client-facing
        // `sid` is client-controlled and may repeat (e.g. a reconnecting tab),
        // so it must never be the registry/presence key, or two connections
        // would alias.
        let conn_id = NEXT_CONN.fetch_add(1, Ordering::Relaxed);
        let session_key = format!("{}::{}::{}#{:x}", id.tenant, id.user_id, id.sid, conn_id);

        // S4: a reconnect with an in-use sid takes over: the old connection is
        // evicted before the session limit is evaluated.
        if let Some(prev) = core.sessions.find_session_by_sid(&user_key, &id.sid) {
            evict_session(
                &core,
                &metrics,
                &id.tenant,
                &user_key,
                &prev,
                "replaced",
                &id.trace_id,
            );
        }

        let sp = policy.session_policy();
        if core.sessions.count_user_sessions(&user_key) >= sp.max_sessions_per_user as usize {
            metrics.policy_decisions.inc(&[
                ("tenant", &id.tenant),
                ("lane", "session"),
                ("decision", "reject"),
                ("reason", "max_user_sessions"),
            ]);
            match sp.on_exceed {
                OnExceed::Deny => {
                    return Ok(Admission::Rejected(CloseSpec::new(
                        close_code::POLICY,
                        "too_many_sessions",
                        Some(sys::error(
                            "TOO_MANY_SESSIONS",
                            "limit exceeded",
                            &id.trace_id,
                        )),
                    )));
                }
                OnExceed::KickOldest => {
                    if let Some(victim) = core.sessions.oldest_session(&user_key) {
                        evict_session(
                            &core,
                            &metrics,
                            &id.tenant,
                            &user_key,
                            &victim,
                            "max_sessions_exceeded",
                            &id.trace_id,
                        );
                    }
                }
            }
        }

        if let Err(e) = core.sessions.try_insert(
            id.tenant.clone(),
            user_key.clone(),
            session_key.clone(),
            id.sid.clone(),
            Connection {
                tx: out_tx.clone(),
                ctl: ctl_tx,
            },
            t_cfg.limits.max_sessions_total,
        ) {
            return Ok(Admission::Rejected(CloseSpec::new(
                close_code::TRY_AGAIN_LATER,
                "capacity",
                Some(sys::error(
                    e.client_code().as_str(),
                    &e.to_string(),
                    &id.trace_id,
                )),
            )));
        }
        metrics.ws_active_sessions.inc(&[("tenant", &id.tenant)]);
        let cleanup = SessionCleanup {
            core: core.clone(),
            tenant_id: id.tenant.clone(),
            user_key: user_key.clone(),
            session_key: session_key.clone(),
            metrics: metrics.clone(),
        };
        // The queue is empty and only this task consumes it, so this cannot be full.
        out_tx
            .try_send(Message::Text(sys::authed(
                &id.tenant,
                &id.user_id,
                &id.sid,
                &id.trace_id,
            )))
            .map_err(|_| WsPrismError::Internal("outbound queue unavailable".into()))?;

        let gw = &app.cfg().gateway;
        let ctx = RealtimeCtx::new(
            id.tenant,
            id.user_id,
            id.sid,
            session_key,
            id.trace_id,
            Arc::new(t_cfg.limits.clone()),
            core,
        );
        Ok(Admission::Admitted(Box::new(Session {
            ctx,
            conn_limiter: policy.new_connection_limiter(),
            policy,
            dispatcher: app.dispatcher(),
            metrics,
            out_tx,
            writer_timeout: Duration::from_millis(gw.writer_send_timeout_ms),
            ping_interval: Duration::from_millis(gw.ping_interval_ms),
            idle_timeout: Duration::from_millis(gw.idle_timeout_ms),
            last_activity: Instant::now(),
            hot_op_counter: 0,
            _cleanup: cleanup,
        })))
    }
}
