//! Per-frame handling: decode → rate limit → policy → room ops / dispatch.

use std::ops::ControlFlow;

use axum::extract::ws::Message;
use tokio::time::Instant;
use wsprism_core::protocol::hot::HotFrame;
use wsprism_core::protocol::text::Envelope;

use super::close::{is_message_too_long, Exit};
use super::{Flow, Session};
use crate::policy::engine::{HotErrorMode, PolicyDecision};
use crate::realtime::{close_code, CloseSpec};
use crate::sys;
use crate::transport::codec::{decode, Inbound};

impl Session {
    pub(super) async fn on_incoming(
        &mut self,
        incoming: Option<Result<Message, axum::Error>>,
    ) -> Flow {
        let msg = match incoming {
            Some(Ok(msg)) => msg,
            Some(Err(e)) => {
                if is_message_too_long(e) {
                    self.decision("transport", "close", "frame_too_large");
                    return Flow::Break(Exit::TooBig);
                }
                return Flow::Break(Exit::Abort);
            }
            None => return Flow::Break(Exit::Peer),
        };
        self.last_activity = Instant::now();
        let decoded = match decode(msg) {
            Ok(d) => d,
            Err(e) => {
                self.metrics.decode_errors.inc(&[("tenant", self.tenant())]);
                return Flow::Break(Exit::Close(CloseSpec::new(
                    close_code::PROTOCOL_ERROR,
                    "decode_error",
                    Some(sys::error(
                        e.client_code().as_str(),
                        &e.to_string(),
                        self.trace_id(),
                    )),
                )));
            }
        };
        match decoded {
            // tungstenite answers pings itself; replying again would double-pong.
            Inbound::Ping(_) | Inbound::Pong(_) => Flow::Continue(()),
            Inbound::Close => Flow::Break(Exit::Peer),
            Inbound::Text { env, bytes_len } => self.on_text(env, bytes_len).await,
            Inbound::Hot { frame, bytes_len } => self.on_hot(frame, bytes_len).await,
        }
    }

    /// Per-connection rate limit, one budget shared by both lanes (S1).
    /// Returns false if the message must be dropped.
    fn take_token(&mut self, lane: &str) -> bool {
        let allowed = self.conn_limiter.as_mut().is_none_or(|lim| lim.allow());
        if !allowed {
            self.decision(lane, "drop", "conn_rate_limit");
        }
        allowed
    }

    /// Apply a policy decision: `Continue(true)` = process the message,
    /// `Continue(false)` = skip it, `Break` = end the session. `notify` says
    /// whether the client is told (false in Hot Lane `silent` mode).
    fn enforce(
        &self,
        lane: &str,
        decision: PolicyDecision,
        notify: bool,
    ) -> ControlFlow<Exit, bool> {
        match decision {
            PolicyDecision::Pass => ControlFlow::Continue(true),
            PolicyDecision::Drop => {
                self.decision(lane, "drop", "policy");
                ControlFlow::Continue(false)
            }
            PolicyDecision::Reject { code, msg } => {
                // SAFE LABEL: code.as_str()
                self.decision(lane, "reject", code.as_str());
                if notify {
                    self.reply_error(code.as_str(), msg)?;
                }
                ControlFlow::Continue(false)
            }
            PolicyDecision::Close { code, msg } => {
                // SAFE LABEL: code.as_str()
                self.decision(lane, "close", code.as_str());
                let notice = notify.then(|| sys::error(code.as_str(), msg, self.trace_id()));
                ControlFlow::Break(Exit::Close(CloseSpec::new(
                    close_code::POLICY,
                    "policy",
                    notice,
                )))
            }
        }
    }

    async fn on_text(&mut self, env: Envelope, bytes_len: usize) -> Flow {
        if !self.take_token("ext") {
            return Flow::Continue(());
        }
        let decision = self.policy.check_text(bytes_len, &env.svc, &env.msg_type);
        if !self.enforce("ext", decision, true)? {
            return Flow::Continue(());
        }
        self.dispatch_text(env).await
    }

    async fn dispatch_text(&mut self, env: Envelope) -> Flow {
        // Label with the registered (static) service name, never the raw svc (S5).
        let svc = self
            .dispatcher
            .text_service_name(&env.svc)
            .unwrap_or("unknown");
        let start = Instant::now();
        let res = self.dispatcher.dispatch_text(&mut self.ctx, env).await;
        // Always measure Ext lane
        self.metrics.dispatch_duration.observe(
            &[("tenant", self.tenant()), ("lane", "ext")],
            start.elapsed(),
        );
        match res {
            Ok(()) => Flow::Continue(()),
            Err(e) => {
                self.metrics.service_errors.inc(&[
                    ("tenant", self.tenant()),
                    ("lane", "ext"),
                    ("svc", svc),
                ]);
                self.reply_error(e.client_code().as_str(), &e.to_string())
            }
        }
    }

    async fn on_hot(&mut self, frame: HotFrame, bytes_len: usize) -> Flow {
        // S1: same per-connection budget as the Ext lane.
        if !self.take_token("hot") {
            return Flow::Continue(());
        }
        let notify = matches!(self.policy.hot_error_mode(), HotErrorMode::SysError);
        let decision = self.policy.check_hot(bytes_len, frame.svc_id, frame.opcode);
        if !self.enforce("hot", decision, notify)? {
            return Flow::Continue(());
        }
        if self.policy.hot_requires_active_room() && self.ctx.active_room().is_none() {
            if notify {
                return self.reply_error("BAD_REQUEST", "no active room");
            }
            return Flow::Continue(());
        }

        // Hot Lane latency sampling (1/1024)
        self.hot_op_counter = self.hot_op_counter.wrapping_add(1);
        let start = (self.hot_op_counter & 1023 == 0).then(Instant::now);

        let res = self.dispatcher.dispatch_hot(&mut self.ctx, frame).await;
        if let Some(s) = start {
            self.metrics
                .dispatch_duration
                .observe(&[("tenant", self.tenant()), ("lane", "hot")], s.elapsed());
        }
        match res {
            Ok(()) => Flow::Continue(()),
            Err(e) => {
                self.metrics
                    .service_errors
                    .inc(&[("tenant", self.tenant()), ("lane", "hot")]);
                if notify {
                    return self.reply_error(e.client_code().as_str(), &e.to_string());
                }
                Flow::Continue(())
            }
        }
    }
}
