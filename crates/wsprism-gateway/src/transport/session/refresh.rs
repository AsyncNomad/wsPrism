//! In-band token refresh (`sys.refresh`, ADR 0012 §2): a client replaces
//! its credential before it expires, without reconnecting.
//!
//! The token is authenticated on its own task (a connect-proxy call may
//! take seconds) and the outcome comes back to the session loop, which
//! alone may change the session's permissions and rooms.

use std::time::SystemTime;

use tokio::sync::oneshot;

use wsprism_engine::spi::now_ms;
use wsprism_engine::spi::sys;
use wsprism_protocol::error::ClientCode;
use wsprism_protocol::protocol::text::Envelope;

use super::{Flow, Session};
use crate::auth::{AuthError, AuthRequest, Identity};

/// Least time between two refresh attempts of one session: each attempt may
/// call the tenant's auth backend (connect proxy), so a client must not be
/// able to drive it at the connection's message rate.
const REFRESH_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// A finished refresh: the request's `seq` and the chain's verdict.
pub(crate) struct RefreshOutcome {
    seq: Option<u64>,
    result: Result<Identity, AuthError>,
}

/// A refresh in flight: the request it answers and its task's report.
pub(crate) struct Pending {
    seq: Option<u64>,
    rx: oneshot::Receiver<RefreshOutcome>,
}

/// Seconds until `t`, if the credential expires at all.
pub(crate) fn expires_in_s(t: Option<SystemTime>) -> Option<u64> {
    t.map(|t| {
        t.duration_since(SystemTime::now())
            .map(|d| d.as_secs())
            .unwrap_or(0)
    })
}

impl Session {
    /// `sys.refresh {"token": "..."}`: start authenticating the new token.
    pub(super) fn start_refresh(&mut self, env: Envelope) -> Flow {
        let token = env
            .data
            .as_ref()
            .and_then(|d| serde_json::from_str::<serde_json::Value>(d.get()).ok())
            .and_then(|v| v["token"].as_str().map(str::to_string))
            .filter(|t| !t.is_empty());
        let Some(token) = token else {
            return self.reply_code(ClientCode::BadRequest, "sys.refresh needs data.token");
        };
        if self.refresh.is_some() {
            return self.reply_code(ClientCode::RateLimited, "a refresh is already in progress");
        }
        let now = std::time::Instant::now();
        if self
            .last_refresh
            .is_some_and(|t| now.duration_since(t) < REFRESH_MIN_INTERVAL)
        {
            return self.reply_code(ClientCode::RateLimited, "refresh at most once per second");
        }
        self.last_refresh = Some(now);
        let (tx, rx) = oneshot::channel();
        self.refresh = Some(Pending { seq: env.seq, rx });
        let (auth, tenant, client_ip, seq) = (
            std::sync::Arc::clone(&self.auth),
            self.tenant().to_string(),
            self.client_ip,
            env.seq,
        );
        tokio::spawn(async move {
            let req = AuthRequest {
                tenant: &tenant,
                token: &token,
                client_ip,
            };
            let result = auth.authenticate(&req).await;
            let _ = tx.send(RefreshOutcome { seq, result });
        });
        Flow::Continue(())
    }

    /// The outcome of the refresh in flight; never resolves without one.
    /// A task that ended without reporting (an authenticator panicked)
    /// counts as unavailable, so the client's request is answered and later
    /// refreshes are not blocked.
    pub(super) async fn refresh_done(&mut self) -> RefreshOutcome {
        let Some(pending) = self.refresh.as_mut() else {
            return std::future::pending().await;
        };
        let seq = pending.seq;
        (&mut pending.rx).await.unwrap_or(RefreshOutcome {
            seq,
            result: Err(AuthError::Unavailable(
                "refresh task ended without an outcome",
            )),
        })
    }

    /// Apply a finished refresh (session task only).
    pub(super) fn apply_refresh(&mut self, outcome: RefreshOutcome) -> Flow {
        self.refresh = None;
        self.ctx.begin_request(outcome.seq);
        let flow = match outcome.result {
            Ok(identity) if identity.user_id != self.ctx.user() => {
                // A session cannot become someone else in-band.
                self.reply_code(ClientCode::AuthFailed, "refresh token is for another user")
            }
            Ok(identity) => {
                // Narrowed permissions apply at once: leave rooms the new
                // token no longer allows.
                for room in self.ctx.set_permissions(identity.permissions) {
                    self.ctx.leave_room(&room);
                    if self.ctx.active_room() == Some(room.as_str()) {
                        self.ctx.set_active_room(None);
                    }
                    let left = sys::left(&room, self.trace_id());
                    if let std::ops::ControlFlow::Break(exit) = self.reply(left) {
                        self.ctx.end_request();
                        return std::ops::ControlFlow::Break(exit);
                    }
                }
                self.signals.set_expiry(match identity.expires_at {
                    Some(t) => crate::auth::expiry_deadline_ms(t, now_ms()),
                    None => u64::MAX,
                });
                let msg = sys::refreshed(expires_in_s(identity.expires_at), self.trace_id());
                self.reply(msg)
            }
            Err(AuthError::Unavailable(_)) => {
                self.reply_code(ClientCode::Unavailable, "authentication unavailable, retry")
            }
            Err(_) => self.reply_code(ClientCode::AuthFailed, "refresh token rejected"),
        };
        self.ctx.end_request();
        flow
    }
}
