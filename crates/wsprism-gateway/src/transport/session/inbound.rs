//! Per-frame handling: decode → rate limit → policy → room ops / dispatch.

use std::ops::ControlFlow;

use tokio::time::Instant;
use wsprism_protocol::error::ClientCode;
use wsprism_protocol::protocol::hot::HotFrame;
use wsprism_protocol::protocol::text::Envelope;

use super::close::Exit;
use super::io::RecvError;
use super::{Flow, Session};
use crate::policy::engine::{HotErrorMode, PolicyDecision};
use crate::transport::codec::Inbound;
use wsprism_engine::spi::sys;
use wsprism_engine::spi::{close_code, CloseSpec};

impl Session {
    pub(super) async fn on_incoming(
        &mut self,
        incoming: Option<Result<Inbound, RecvError>>,
    ) -> Flow {
        let decoded = match incoming {
            None => return Flow::Break(Exit::Peer),
            Some(Err(RecvError::TooBig)) => {
                self.decision("transport", "close", "frame_too_large");
                return Flow::Break(Exit::TooBig);
            }
            Some(Err(RecvError::Broken)) => return Flow::Break(Exit::Abort),
            Some(Err(RecvError::Decode(e))) => {
                self.signals.touch();
                self.metrics.decode_errors.inc(&[("tenant", self.tenant())]);
                return Flow::Break(Exit::Close(CloseSpec::new(
                    close_code::PROTOCOL_ERROR,
                    "decode_error",
                    Some(sys::error_from(&e, self.trace_id()).to_string()),
                )));
            }
            Some(Ok(decoded)) => decoded,
        };
        self.signals.touch();
        match decoded {
            // tungstenite answers pings itself; replying again would double-pong.
            Inbound::Ping | Inbound::Pong => Flow::Continue(()),
            Inbound::Close => Flow::Break(Exit::Peer),
            // Replies produced while handling a request carry its seq.
            Inbound::Text { env, bytes_len } => {
                self.ctx.begin_request(env.seq);
                let flow = self.on_text(env, bytes_len).await;
                self.ctx.end_request();
                flow
            }
            Inbound::Hot { frame, bytes_len } => {
                self.ctx.begin_request(frame.seq.map(u64::from));
                let flow = self.on_hot(frame, bytes_len).await;
                self.ctx.end_request();
                flow
            }
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
                    self.reply_code(code, msg)?;
                }
                ControlFlow::Continue(false)
            }
            PolicyDecision::Close { code, msg } => {
                // SAFE LABEL: code.as_str()
                self.decision(lane, "close", code.as_str());
                let notice = notify.then(|| {
                    sys::reply_to(
                        sys::error(code, msg, self.trace_id()),
                        self.ctx.request_seq(),
                    )
                    .to_string()
                });
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
        let rpc = self
            .rpc
            .as_ref()
            .filter(|r| r.handles(&env.svc, &env.msg_type))
            .cloned();
        let svc = match rpc {
            Some(_) => "rpc",
            None => self
                .dispatcher
                .text_service_name(&env.svc)
                .unwrap_or("unknown"),
        };
        let start = Instant::now();
        // RPC only starts the call here; the backend's answer arrives later.
        let res = match rpc {
            Some(rpc) => rpc.call(&self.ctx, env),
            None => self.dispatcher.dispatch_text(&mut self.ctx, env).await,
        };
        // Always measure Ext lane
        self.ext_duration.observe(start.elapsed());
        match res {
            Ok(()) => Flow::Continue(()),
            Err(e) => {
                self.metrics.service_errors.inc(&[
                    ("tenant", self.tenant()),
                    ("lane", "ext"),
                    ("svc", svc),
                ]);
                self.reply_error(&e)
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
        if frame.chan.is_some() {
            // v2: the frame names a room id, which must be one this session
            // joined (ADR 0006 R5, R7).
            if self.ctx.hot_target(&frame).is_none() {
                self.decision("hot", "drop", "unknown_channel");
                if notify {
                    return self.reply_code(ClientCode::NotAllowed, "not a member of that room");
                }
                return Flow::Continue(());
            }
        } else if self.policy.hot_requires_active_room() && self.ctx.active_room().is_none() {
            if notify {
                return self.reply_code(ClientCode::BadRequest, "no active room");
            }
            return Flow::Continue(());
        }

        // Hot Lane latency sampling (1/1024)
        self.hot_op_counter = self.hot_op_counter.wrapping_add(1);
        let start = (self.hot_op_counter & 1023 == 0).then(Instant::now);

        let res = self.dispatcher.dispatch_hot(&mut self.ctx, frame).await;
        if let Some(s) = start {
            self.hot_duration.observe(s.elapsed());
        }
        match res {
            Ok(()) => Flow::Continue(()),
            Err(e) => {
                self.metrics
                    .service_errors
                    .inc(&[("tenant", self.tenant()), ("lane", "hot")]);
                if notify {
                    return self.reply_error(&e);
                }
                Flow::Continue(())
            }
        }
    }
}
