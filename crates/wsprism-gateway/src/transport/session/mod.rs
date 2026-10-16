//! One WebSocket session after the upgrade: admission, the event loop, and
//! teardown.
//!
//! - `admit`: sid takeover, per-user session limit, registry registration.
//! - `inbound`: per-frame decode → rate limit → policy → room ops / dispatch.
//! - `close`: why a session ends (`Exit`) and how its connection is torn down.
//! - `io`: the transport seam (`SessionTx` / `SessionRx`); `ws_io` is the
//!   WebSocket implementation. Nothing else here knows the socket type.
//!
//! Handlers return a `Flow`. The event loop only routes events to them and
//! never awaits the session's own outbound queue (C9).

mod admit;
mod close;
mod inbound;
mod io;
mod ws_io;

use std::future::Future;
use std::ops::ControlFlow;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::time::{timeout, Duration};
use wsprism_core::error::{ClientCode, Result, WsPrismError};

use crate::app_state::AppState;
use crate::auth::Permissions;
use crate::dispatch::Dispatcher;
use crate::obs::metrics::GatewayMetrics;
use crate::policy::engine::ConnRateLimiter;
use crate::policy::TenantPolicyRuntime;
use crate::realtime::core::{now_ms, CloseRequest, SessionSignals};
use crate::realtime::{close_code, CloseSpec, HotOut, OutFrame, RealtimeCtx};
use crate::sys;
use crate::transport::subprotocol::ProtocolVersion;

use admit::{Admission, SessionCleanup};
use close::Exit;
use io::{SessionRx, SessionTx};
pub(crate) use ws_io::split as split_websocket;

/// Outcome of handling one event: keep going, or end the session.
type Flow = ControlFlow<Exit>;

/// Most queued frames written per flush. Coalescing a burst into one flush
/// cuts per-message write cost under load; the cap bounds how long one write
/// can delay the loop's other events (control, inbound, idle). Frames are fed
/// straight from the queue, so coalescing keeps no buffer of its own.
const MAX_WRITE_BATCH: usize = 64;

/// Who is connected, fixed for the connection's lifetime.
pub(crate) struct SessionIdentity {
    /// Wire protocol negotiated at the handshake (close codes differ, ADR 0006).
    pub protocol: ProtocolVersion,
    pub tenant: String,
    pub user_id: String,
    /// Room permissions from the authenticator (ADR 0008 §2).
    pub permissions: Permissions,
    /// Credential expiry: the session is closed then (`auth_expired`).
    pub expires_at: Option<std::time::SystemTime>,
    /// Client-facing session id (client-provided or generated; not unique).
    pub sid: String,
    pub trace_id: String,
}

/// Run a session on an established connection until it ends.
pub(crate) async fn run<Tx: SessionTx, Rx: SessionRx>(
    app: AppState,
    id: SessionIdentity,
    (mut tx, mut rx): (Tx, Rx),
) -> Result<()> {
    let gw = &app.cfg().gateway;
    let writer_timeout = Duration::from_millis(gw.writer_send_timeout_ms);
    let protocol = id.protocol;
    let (out_tx, mut out_rx) = mpsc::channel(gw.session_queue_capacity);

    let mut session = match Session::admit(&app, id, out_tx)? {
        Admission::Admitted(session) => session,
        Admission::Rejected(spec) => {
            close::close_session(
                &mut tx,
                &mut rx,
                &mut out_rx,
                spec,
                protocol,
                writer_timeout,
            )
            .await;
            return Ok(());
        }
    };

    let exit = session.event_loop(&mut tx, &mut rx, &mut out_rx).await;

    // The session stays registered until the close completes: the drain
    // logic in main waits on the registry, and anything routed to us meanwhile
    // is rejected by the closed queue. Dropping `session` unregisters it.
    close::finish(
        exit,
        &mut tx,
        &mut rx,
        &mut out_rx,
        protocol,
        writer_timeout,
    )
    .await;
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
    /// Close requests from other tasks and liveness for the sweeper (which
    /// also sends keep-alive pings and enforces the idle timeout).
    signals: Arc<SessionSignals>,
    /// Reused buffer for draining the Hot slot (empty until first used).
    hot_buf: Vec<Arc<HotOut>>,
    /// Hot slot pacing (`policy.hot_tick_hz`, v2-4): `None` = send on arrival.
    hot_tick: Option<Duration>,
    /// This session's offset within the tick period, so sessions don't all
    /// flush at the same instant.
    hot_tick_phase_ms: u64,
    conn_limiter: Option<ConnRateLimiter>,
    /// Hot Lane latency sampling counter (1/1024).
    hot_op_counter: u64,
    /// Unregisters the session when dropped.
    _cleanup: SessionCleanup,
}

impl Session {
    async fn event_loop<Tx: SessionTx, Rx: SessionRx>(
        &mut self,
        tx: &mut Tx,
        rx: &mut Rx,
        out_rx: &mut mpsc::Receiver<OutFrame>,
    ) -> Exit {
        // Held outside `self` so the select! below can borrow `self` mutably.
        let signals = Arc::clone(&self.signals);
        // Hot slot pacing: one reusable timer per paced session, armed only
        // while the slot holds data (idle sessions never wake for it).
        let mut tick_sleep = self
            .hot_tick
            .map(|_| Box::pin(tokio::time::sleep(Duration::ZERO)));
        let mut hot_due = false;
        loop {
            let flow = tokio::select! {
                // C8: kick / replace / drain requests from other tasks.
                // C8: kick / replace / drain / slow consumer / idle requests.
                req = signals.closed() => Flow::Break(Exit::Close(self.close_spec(req))),
                out = out_rx.recv() => self.write(tx, out_rx, out).await,
                _ = signals.hot.ready(), if !hot_due => match (self.hot_tick, tick_sleep.as_mut()) {
                    (Some(period), Some(sleep)) => {
                        sleep.as_mut().reset(self.next_hot_tick(period));
                        hot_due = true;
                        Flow::Continue(())
                    }
                    _ => self.write_hot(tx, &signals).await,
                },
                () = async {
                    if let Some(sleep) = tick_sleep.as_mut() {
                        sleep.as_mut().await;
                    }
                }, if hot_due => {
                    hot_due = false;
                    self.write_hot(tx, &signals).await
                },
                incoming = rx.recv() => self.on_incoming(incoming).await,
            };
            if let ControlFlow::Break(exit) = flow {
                return exit;
            }
        }
    }

    /// Write `first` plus whatever else is already queued (up to
    /// `MAX_WRITE_BATCH` frames) with a single flush, under one slow-consumer
    /// timeout. `None` means the queue closed.
    async fn write<Tx: SessionTx>(
        &self,
        tx: &mut Tx,
        out_rx: &mut mpsc::Receiver<OutFrame>,
        first: Option<OutFrame>,
    ) -> Flow {
        let Some(first) = first else {
            return Flow::Break(Exit::Abort);
        };
        let burst = async {
            tx.feed(first).await?;
            for _ in 1..MAX_WRITE_BATCH {
                let Ok(frame) = out_rx.try_recv() else { break };
                tx.feed(frame).await?;
            }
            tx.flush().await
        };
        self.finish_burst(burst).await
    }

    /// Send everything waiting in the Hot slot (latest value per key, T0-3)
    /// with a single flush.
    async fn write_hot<Tx: SessionTx>(&mut self, tx: &mut Tx, signals: &SessionSignals) -> Flow {
        let mut frames = std::mem::take(&mut self.hot_buf);
        signals.hot.drain_into(&mut frames);
        if frames.is_empty() {
            self.hot_buf = frames;
            return Flow::Continue(());
        }
        let burst = async {
            for f in frames.drain(..) {
                tx.feed(OutFrame::Hot(f)).await?;
            }
            tx.flush().await
        };
        let flow = self.finish_burst(burst).await;
        // Keep the buffer for the next drain, but not an outsized one.
        frames.clear();
        frames.shrink_to(MAX_WRITE_BATCH);
        self.hot_buf = frames;
        flow
    }

    /// The next tick boundary for this session's phase.
    fn next_hot_tick(&self, period: Duration) -> tokio::time::Instant {
        let p = (period.as_millis() as u64).max(1);
        let now = now_ms();
        let since = now.wrapping_sub(self.hot_tick_phase_ms) % p;
        tokio::time::Instant::now() + Duration::from_millis(p - since)
    }

    /// Drive a write burst under the slow-consumer deadline.
    async fn finish_burst(&self, burst: impl Future<Output = std::result::Result<(), ()>>) -> Flow {
        // Poll once without a timer: with room in the socket buffer the burst
        // completes immediately, and registering a timeout per write was a
        // measurable cost (performance-architecture §3.6). Arm the
        // slow-consumer deadline only if the write actually blocks.
        let mut burst = std::pin::pin!(burst);
        let result = match futures_util::poll!(burst.as_mut()) {
            std::task::Poll::Ready(r) => Ok(r),
            std::task::Poll::Pending => timeout(self.writer_timeout, burst).await,
        };
        match result {
            Ok(Ok(())) => Flow::Continue(()),
            Ok(Err(())) => Flow::Break(Exit::Abort),
            Err(_) => {
                self.metrics
                    .writer_timeouts
                    .inc(&[("tenant", self.tenant())]);
                Flow::Break(Exit::Abort)
            }
        }
    }

    /// The close spec for a request from another task. Idle closes are built
    /// here because the notice carries this session's trace id.
    fn close_spec(&self, req: CloseRequest) -> CloseSpec {
        match req {
            CloseRequest::Spec(spec) => spec,
            CloseRequest::Idle => CloseSpec::new(
                close_code::NORMAL,
                "idle_timeout",
                Some(sys::error(ClientCode::Timeout, "idle", self.trace_id()).to_string()),
            ),
        }
    }

    /// Queue a reply from the session's own loop without awaiting (C9). This
    /// loop is the queue's only consumer, so awaiting a full queue would
    /// freeze the session forever. A full queue means the client can't keep up
    /// with its own replies plus broadcasts, so it is closed as a slow consumer.
    /// Correlated with the request being handled, if it carried a `seq`.
    fn reply(&self, msg: Value) -> Flow {
        let text = sys::reply_to(msg, self.ctx.request_seq()).to_string();
        match self.out_tx.try_send(OutFrame::text(text)) {
            Ok(()) => Flow::Continue(()),
            Err(TrySendError::Full(_)) => {
                self.decision("transport", "close", "slow_consumer");
                Flow::Break(Exit::Close(CloseSpec::new(
                    close_code::POLICY,
                    "slow_consumer",
                    Some(
                        sys::error(
                            ClientCode::SlowConsumer,
                            "outbound queue full",
                            self.trace_id(),
                        )
                        .to_string(),
                    ),
                )))
            }
            Err(TrySendError::Closed(_)) => Flow::Break(Exit::Abort),
        }
    }

    fn reply_error(&self, e: &WsPrismError) -> Flow {
        self.reply(sys::error_from(e, self.trace_id()))
    }

    fn reply_code(&self, code: ClientCode, msg: &str) -> Flow {
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
