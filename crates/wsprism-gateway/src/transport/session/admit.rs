//! Admission: sid takeover, per-user session limit, and registration.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::time::Duration;
use wsprism_protocol::error::{ClientCode, Result, WsPrismError};

use super::{Session, SessionIdentity};
use crate::app_state::AppState;
use crate::obs::metrics::GatewayMetrics;
use crate::policy::engine::OnExceed;
use wsprism_engine::broker::{Control, OnFull, SessionClaim};
use wsprism_engine::spi::sys;
use wsprism_engine::spi::{close_code, CloseSpec, OutFrame, RealtimeCore};
use wsprism_engine::spi::{now_ms, CloseRequest, Connection, CtxIdentity, SessionSignals};
use wsprism_engine::RealtimeCtx;

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
    /// Tenant `presence_events`: tell remaining members (ADR 0010).
    presence_events: bool,
    /// The cluster's session table has an entry for this session.
    shared: bool,
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
        self.core.remove_session_presence(
            &self.tenant_id,
            &self.user_key,
            &self.session_key,
            self.presence_events,
        );
        if removed {
            self.metrics
                .ws_active_sessions
                .dec(&[("tenant", &self.tenant_id)]);
        }
        if self.shared {
            release_shared(&self.core, &self.user_key, &self.session_key);
        }
        // Inside the session span (identities as `gateway.log_identities`
        // says); the session key would carry the user id and sid in clear.
        tracing::debug!("session raii cleanup done");
    }
}

/// Drop a session's entry from the cluster's session table, in the
/// background (idempotent; a lost release is cleaned up later, ADR 0011
/// R4-1).
fn release_shared(core: &Arc<RealtimeCore>, user_key: &str, session_key: &str) {
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let (core, user_key, session_key) =
        (core.clone(), user_key.to_string(), session_key.to_string());
    rt.spawn(async move {
        if let Some(table) = core.broker().sessions() {
            if let Err(e) = table.release(&user_key, &session_key).await {
                tracing::warn!("session table release failed: {e}");
            }
        }
    });
}

/// The cluster's session table decides sid replacement and the user's
/// session limit (ADR 0011 R4-1). `Err` = refuse the connection with `spec`.
async fn claim_shared(
    core: &RealtimeCore,
    metrics: &GatewayMetrics,
    claim: SessionClaim,
    tenant: &str,
    trace_id: &str,
    presence_events: bool,
) -> std::result::Result<(), CloseSpec> {
    let Some(table) = core.broker().sessions() else {
        return Ok(());
    };
    let claimed = table.claim(&claim).await.map_err(|e| {
        tracing::warn!("session table unavailable: {e}");
        CloseSpec::new(
            close_code::TRY_AGAIN_LATER,
            "unavailable",
            Some(
                sys::error(
                    ClientCode::Unavailable,
                    "session store unavailable",
                    trace_id,
                )
                .to_string(),
            ),
        )
    })?;
    for ev in &claimed.evicted {
        if ev.node == core.node_slot() {
            evict_session(
                core,
                metrics,
                Eviction {
                    tenant,
                    user_key: &claim.user_key,
                    victim_key: &ev.session_key,
                    reason: ev.reason,
                    trace_id,
                    presence_events,
                },
            );
        } else {
            let close = Control::Close {
                keys: vec![ev.session_key.clone()],
                reason: ev.reason.to_string(),
                message: ev.reason.to_string(),
            };
            if let Err(e) = table.send_control(ev.node, &close).await {
                tracing::warn!(
                    node = ev.node,
                    "could not reach a node to close a session: {e}"
                );
            }
        }
    }
    if claimed.admitted {
        Ok(())
    } else {
        Err(too_many_sessions(trace_id))
    }
}

fn too_many_sessions(trace_id: &str) -> CloseSpec {
    CloseSpec::new(
        close_code::POLICY,
        "too_many_sessions",
        Some(sys::error(ClientCode::TooManySessions, "limit exceeded", trace_id).to_string()),
    )
}

/// Who evicts whom, and why.
struct Eviction<'a> {
    tenant: &'a str,
    user_key: &'a str,
    victim_key: &'a str,
    reason: &'a str,
    trace_id: &'a str,
    presence_events: bool,
}

/// Evict a live session: unregister it (owning the gauge decrement, see C2),
/// clear its presence, then tell it why and close it.
fn evict_session(core: &RealtimeCore, metrics: &GatewayMetrics, e: Eviction<'_>) {
    let Eviction {
        tenant,
        user_key,
        victim_key,
        reason,
        trace_id,
        presence_events,
    } = e;
    let Some(conn) = core.sessions.remove_session(user_key, victim_key) else {
        return; // already gone (raced with its own cleanup or another evictor)
    };
    core.remove_session_presence(tenant, user_key, victim_key, presence_events);
    metrics.ws_active_sessions.dec(&[("tenant", tenant)]);
    // C8: ask the victim's loop to stop and close itself (notice + 1008).
    conn.signals
        .request_close(CloseRequest::Spec(CloseSpec::new(
            close_code::POLICY,
            reason,
            Some(sys::kicked(reason, trace_id).to_string()),
        )));
}

impl Session {
    /// Apply session policies and register the connection. On success the
    /// `sys.authed` greeting is already queued.
    pub(super) async fn admit(
        app: &AppState,
        id: SessionIdentity,
        out_tx: mpsc::Sender<OutFrame>,
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
        // The connection id is what engine warnings name (not personal data).
        tracing::Span::current().record("c", format_args!("{conn_id:x}"));

        let sp = policy.session_policy();
        let shared = core.broker().sessions().is_some();
        if shared {
            let claim = SessionClaim::new(
                &user_key,
                &session_key,
                &id.sid,
                sp.max_sessions_per_user,
                match sp.on_exceed {
                    OnExceed::KickOldest => OnFull::KickOldest,
                    _ => OnFull::Deny,
                },
            );
            if let Err(spec) = claim_shared(
                &core,
                &metrics,
                claim,
                &id.tenant,
                &id.trace_id,
                policy.presence_events(),
            )
            .await
            {
                if spec.reason == "too_many_sessions" {
                    metrics.policy_decisions.inc(&[
                        ("tenant", &id.tenant),
                        ("lane", "session"),
                        ("decision", "reject"),
                        ("reason", "max_user_sessions"),
                    ]);
                }
                return Ok(Admission::Rejected(spec));
            }
        } else {
            // S4: a reconnect with an in-use sid takes over: the old connection is
            // evicted before the session limit is evaluated.
            if let Some(prev) = core.sessions.find_session_by_sid(&user_key, &id.sid) {
                evict_session(
                    &core,
                    &metrics,
                    Eviction {
                        tenant: &id.tenant,
                        user_key: &user_key,
                        victim_key: &prev,
                        reason: "replaced",
                        trace_id: &id.trace_id,
                        presence_events: policy.presence_events(),
                    },
                );
            }

            if core.sessions.count_user_sessions(&user_key) >= sp.max_sessions_per_user as usize {
                metrics.policy_decisions.inc(&[
                    ("tenant", &id.tenant),
                    ("lane", "session"),
                    ("decision", "reject"),
                    ("reason", "max_user_sessions"),
                ]);
                match sp.on_exceed {
                    OnExceed::Deny => {
                        return Ok(Admission::Rejected(too_many_sessions(&id.trace_id)));
                    }
                    OnExceed::KickOldest => {
                        if let Some(victim) = core.sessions.oldest_session(&user_key) {
                            evict_session(
                                &core,
                                &metrics,
                                Eviction {
                                    tenant: &id.tenant,
                                    user_key: &user_key,
                                    victim_key: &victim,
                                    reason: "max_sessions_exceeded",
                                    trace_id: &id.trace_id,
                                    presence_events: policy.presence_events(),
                                },
                            );
                        }
                    }
                }
            }
        }

        let signals = SessionSignals::new();
        if let Some(exp) = id.expires_at {
            signals.set_expiry(crate::auth::expiry_deadline_ms(exp, now_ms()));
        }
        let conn = Connection {
            tx: out_tx.clone(),
            signals: signals.clone(),
        };
        if let Err(e) = core.sessions.try_insert(
            id.tenant.clone(),
            user_key.clone(),
            session_key.clone(),
            id.sid.clone(),
            conn.clone(),
            t_cfg.limits.max_sessions_total,
        ) {
            if shared {
                release_shared(&core, &user_key, &session_key);
            }
            return Ok(Admission::Rejected(CloseSpec::new(
                close_code::TRY_AGAIN_LATER,
                "capacity",
                Some(sys::error_from(&e, &id.trace_id).to_string()),
            )));
        }
        metrics.ws_active_sessions.inc(&[("tenant", &id.tenant)]);
        let cleanup = SessionCleanup {
            core: core.clone(),
            tenant_id: id.tenant.clone(),
            user_key: user_key.clone(),
            session_key: session_key.clone(),
            metrics: metrics.clone(),
            presence_events: policy.presence_events(),
            shared,
        };
        // The queue is empty and only this task consumes it, so this cannot be full.
        out_tx
            .try_send(OutFrame::text(
                sys::authed(
                    &id.tenant,
                    &id.user_id,
                    &id.sid,
                    super::refresh::expires_in_s(id.expires_at),
                    &id.trace_id,
                )
                .to_string(),
            ))
            .map_err(|_| WsPrismError::Internal("outbound queue unavailable".into()))?;

        let client_ip = id.client_ip;
        let gw = &app.cfg().gateway;
        let session_key_for_phase = session_key.clone();
        let ctx = RealtimeCtx::new(
            CtxIdentity {
                tenant: id.tenant.into(),
                user: id.user_id.into(),
                session_id: id.sid.into(),
                session_key: session_key.into(),
                trace_id: id.trace_id.into(),
                protocol: id.protocol,
                permissions: id.permissions,
                hot_echo_to_sender: policy.hot_echo_to_sender(),
                presence_events: policy.presence_events(),
            },
            app.room_limits(&t_cfg.id),
            conn,
            core,
        );
        let hot_tick = policy.hot_tick();
        Ok(Admission::Admitted(Box::new(Session {
            ctx,
            conn_limiter: policy.new_connection_limiter(),
            policy,
            dispatcher: app.dispatcher(),
            rpc: app.rpc(&t_cfg.id),
            ext_duration: metrics
                .dispatch_duration
                .handle(&[("tenant", &t_cfg.id), ("lane", "ext")]),
            hot_duration: metrics
                .dispatch_duration
                .handle(&[("tenant", &t_cfg.id), ("lane", "hot")]),
            metrics,
            out_tx,
            writer_timeout: Duration::from_millis(gw.writer_send_timeout_ms),
            signals,
            hot_buf: Vec::new(),
            hot_tick,
            hot_tick_phase_ms: tick_phase_ms(&session_key_for_phase, hot_tick),
            hot_op_counter: 0,
            auth: app
                .auth(&t_cfg.id)
                .ok_or_else(|| WsPrismError::BadRequest("unknown tenant".into()))?,
            client_ip,
            refresh_tx: None,
            refresh_pending: false,
            last_refresh: None,
            _cleanup: cleanup,
        })))
    }
}

/// A stable offset within the tick period for this session, so paced
/// sessions spread their flushes over the period instead of all at once.
fn tick_phase_ms(session_key: &str, tick: Option<Duration>) -> u64 {
    use std::hash::{Hash, Hasher};
    let Some(period) = tick else { return 0 };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    session_key.hash(&mut h);
    h.finish() % (period.as_millis() as u64).max(1)
}
