//! One WebSocket session after the upgrade: admission, the event loop, and
//! teardown.
//!
//! - `admit`: sid takeover, per-user session limit, registry registration.
//! - `inbound`: per-frame decode → rate limit → policy → room ops / dispatch.
//! - `close`: why a session ends (`Exit`) and how its socket is torn down.
//! - `sys`: gateway `sys.*` envelopes.
//!
//! Handlers return a `Flow`. The event loop only routes events to them and
//! never awaits the session's own outbound queue (C9).

mod admit;
mod close;
mod inbound;
mod wire;

use std::ops::ControlFlow;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::time::{timeout, Duration, Instant};
use wsprism_core::error::Result;

use crate::app_state::AppState;
use crate::dispatch::Dispatcher;
use crate::obs::metrics::GatewayMetrics;
use crate::policy::engine::ConnRateLimiter;
use crate::policy::TenantPolicyRuntime;
use crate::realtime::{close_code, CloseSpec, OutFrame, RealtimeCtx};
use crate::sys;

use admit::{Admission, SessionCleanup};
use close::Exit;

/// Outcome of handling one event: keep going, or end the session.
type Flow = ControlFlow<Exit>;

/// Who is connected, fixed for the connection's lifetime.
pub(crate) struct SessionIdentity {
    pub tenant: String,
    pub user_id: String,
    /// Client-facing session id (client-provided or generated; not unique).
    pub sid: String,
    pub trace_id: String,
}

/// Run a session on an upgraded socket until it ends.
pub(crate) async fn run(app: AppState, id: SessionIdentity, socket: WebSocket) -> Result<()> {
    let gw = &app.cfg().gateway;
    let writer_timeout = Duration::from_millis(gw.writer_send_timeout_ms);
    let (out_tx, mut out_rx) = mpsc::channel(gw.session_queue_capacity);
    let (ctl_tx, mut ctl_rx) = mpsc::channel::<CloseSpec>(1);
    let (mut ws_tx, mut ws_rx) = socket.split();

    let mut session = match Session::admit(&app, id, out_tx, ctl_tx)? {
        Admission::Admitted(session) => session,
        Admission::Rejected(spec) => {
            close::close_session(&mut ws_tx, &mut ws_rx, &mut out_rx, spec, writer_timeout).await;
            return Ok(());
        }
    };

    let exit = session
        .event_loop(&mut ws_tx, &mut ws_rx, &mut out_rx, &mut ctl_rx)
        .await;

    // The session stays registered until the close completes: the drain
    // logic in main waits on the registry, and anything routed to us meanwhile
    // is rejected by the closed queue. Dropping `session` unregisters it.
    close::finish(exit, &mut ws_tx, &mut ws_rx, &mut out_rx, writer_timeout).await;
    drop(session);
    Ok(())
}

/// State of one admitted session, owned by its task.
struct Session {
    /// Identity + session-scoped state, lent to services as `&mut` (one per
    /// session, not rebuilt per message).
    ctx: RealtimeCtx,
    policy: Arc<TenantPolicyRuntime>,
    dispatcher: Arc<Dispatcher>,
    metrics: Arc<GatewayMetrics>,
    out_tx: mpsc::Sender<OutFrame>,
    writer_timeout: Duration,
    ping_interval: Duration,
    idle_timeout: Duration,
    last_activity: Instant,
    conn_limiter: Option<ConnRateLimiter>,
    /// Hot Lane latency sampling counter (1/1024).
    hot_op_counter: u64,
    /// Unregisters the session when dropped.
    _cleanup: SessionCleanup,
}

impl Session {
    async fn event_loop(
        &mut self,
        ws_tx: &mut SplitSink<WebSocket, Message>,
        ws_rx: &mut SplitStream<WebSocket>,
        out_rx: &mut mpsc::Receiver<OutFrame>,
        ctl_rx: &mut mpsc::Receiver<CloseSpec>,
    ) -> Exit {
        let mut ping_tick = tokio::time::interval(self.ping_interval);
        let mut idle_tick = tokio::time::interval(Duration::from_millis(1000));
        loop {
            let flow = tokio::select! {
                // C8: kick / replace / drain requests from other tasks.
                Some(spec) = ctl_rx.recv() => Flow::Break(Exit::Close(spec)),
                out = out_rx.recv() => self.write(ws_tx, out).await,
                incoming = ws_rx.next() => self.on_incoming(incoming).await,
                _ = ping_tick.tick() => self.ping(),
                _ = idle_tick.tick() => self.check_idle(),
            };
            if let ControlFlow::Break(exit) = flow {
                return exit;
            }
        }
    }

    /// Write one queued message to the socket (slow-consumer protection).
    async fn write(
        &self,
        ws_tx: &mut SplitSink<WebSocket, Message>,
        out: Option<OutFrame>,
    ) -> Flow {
        let Some(frame) = out else {
            return Flow::Break(Exit::Abort);
        };
        match timeout(self.writer_timeout, ws_tx.send(wire::to_message(frame))).await {
            Ok(Ok(())) => Flow::Continue(()),
            Ok(Err(_)) => Flow::Break(Exit::Abort),
            Err(_) => {
                self.metrics
                    .writer_timeouts
                    .inc(&[("tenant", self.tenant())]);
                Flow::Break(Exit::Abort)
            }
        }
    }

    /// Keep-alive ping is optional: skip it if the queue is full (C9).
    fn ping(&self) -> Flow {
        let _ = self.out_tx.try_send(OutFrame::Ping);
        Flow::Continue(())
    }

    fn check_idle(&self) -> Flow {
        if self.last_activity.elapsed() >= self.idle_timeout {
            return Flow::Break(Exit::Close(CloseSpec::new(
                close_code::NORMAL,
                "idle_timeout",
                Some(sys::error("TIMEOUT", "idle", self.trace_id())),
            )));
        }
        Flow::Continue(())
    }

    /// Queue a reply from the session's own loop without awaiting (C9). This
    /// loop is the queue's only consumer, so awaiting a full queue would
    /// freeze the session forever. A full queue means the client can't keep up
    /// with its own replies plus broadcasts, so it is closed as a slow consumer.
    fn reply(&self, text: String) -> Flow {
        match self.out_tx.try_send(OutFrame::text(text)) {
            Ok(()) => Flow::Continue(()),
            Err(TrySendError::Full(_)) => {
                self.decision("transport", "close", "slow_consumer");
                Flow::Break(Exit::Close(CloseSpec::new(
                    close_code::POLICY,
                    "slow_consumer",
                    Some(sys::error(
                        "SLOW_CONSUMER",
                        "outbound queue full",
                        self.trace_id(),
                    )),
                )))
            }
            Err(TrySendError::Closed(_)) => Flow::Break(Exit::Abort),
        }
    }

    fn reply_error(&self, code: &str, msg: &str) -> Flow {
        self.reply(sys::error(code, msg, self.trace_id()))
    }

    /// Count a policy decision. Label values are static or from a fixed set (S5).
    fn decision(&self, lane: &str, decision: &str, reason: &str) {
        self.metrics.policy_decisions.inc(&[
            ("tenant", self.tenant()),
            ("lane", lane),
            ("decision", decision),
            ("reason", reason),
        ]);
    }

    fn tenant(&self) -> &str {
        self.ctx.tenant()
    }

    fn trace_id(&self) -> &str {
        &self.ctx.trace_id
    }
}
