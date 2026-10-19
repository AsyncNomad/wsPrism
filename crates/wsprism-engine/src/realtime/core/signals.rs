//! Per-session control shared with other tasks: close requests (kick,
//! replace, drain, slow consumer, idle) and liveness timestamps read by the
//! sweeper (performance-architecture §3.3, §5).
//!
//! Replaces a capacity-1 `mpsc` channel (which allocates a 32-slot block up
//! front) and the two per-session timers (ping, idle) that woke every
//! session's task once a second.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Instant;

use tokio::sync::Notify;

use super::hot_slot::{HotSlot, DEFAULT_SLOT_CAP};
use crate::realtime::types::CloseSpec;

/// Milliseconds since process start (monotonic).
pub fn now_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Why another task wants a session to end.
#[derive(Debug, Clone)]
pub enum CloseRequest {
    /// Close with this code / reason / notice (kick, replace, drain, slow consumer).
    Spec(CloseSpec),
    /// No inbound activity for `idle_timeout_ms`. The session builds the
    /// notice itself, since it carries the session's trace id.
    Idle,
}

/// Per-session control shared with other tasks (close requests, liveness, Hot slot).
#[derive(Debug)]
pub struct SessionSignals {
    close: Mutex<Option<CloseRequest>>,
    notify: Notify,
    last_activity_ms: AtomicU64,
    last_ping_ms: AtomicU64,
    /// When the credential expires (`now_ms` scale; `u64::MAX` = never).
    expires_at_ms: AtomicU64,
    /// Requests being answered by another task (`DeferredReply`).
    deferred: AtomicU32,
    /// Latest keyed Hot frame per `(sender, svc, key)` (T0-3).
    pub hot: HotSlot,
}

impl SessionSignals {
    /// Signals for a new session.
    pub fn new() -> Arc<Self> {
        let now = now_ms();
        Arc::new(Self {
            close: Mutex::new(None),
            notify: Notify::new(),
            last_activity_ms: AtomicU64::new(now),
            last_ping_ms: AtomicU64::new(now),
            expires_at_ms: AtomicU64::new(u64::MAX),
            deferred: AtomicU32::new(0),
            hot: HotSlot::new(DEFAULT_SLOT_CAP),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Option<CloseRequest>> {
        // The guarded value is plain data; a poisoned lock is still usable.
        self.close.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Ask the session to close (C8). The first request wins; later ones are
    /// ignored and return false. Never blocks on the session.
    pub fn request_close(&self, req: CloseRequest) -> bool {
        {
            let mut pending = self.lock();
            if pending.is_some() {
                return false;
            }
            *pending = Some(req);
        }
        // Stores a permit if the session isn't waiting right now, so the
        // request is seen on its next `closed().await`.
        self.notify.notify_one();
        true
    }

    /// Resolves with the pending close request (session task only).
    pub async fn closed(&self) -> CloseRequest {
        loop {
            if let Some(req) = self.lock().take() {
                return req;
            }
            self.notify.notified().await;
        }
    }

    /// Record inbound activity (any frame, including pongs).
    pub fn touch(&self) {
        self.last_activity_ms.store(now_ms(), Ordering::Relaxed);
    }

    /// Milliseconds since the last inbound activity, as of `now`.
    pub fn idle_ms(&self, now: u64) -> u64 {
        now.saturating_sub(self.last_activity_ms.load(Ordering::Relaxed))
    }

    /// Close the session at `deadline_ms` (credential expiry, ADR 0008 §1).
    pub fn set_expiry(&self, deadline_ms: u64) {
        self.expires_at_ms.store(deadline_ms, Ordering::Relaxed);
    }

    /// True once the credential has expired, as of `now`.
    pub fn expired(&self, now: u64) -> bool {
        now >= self.expires_at_ms.load(Ordering::Relaxed)
    }

    /// Reserve one deferred answer if fewer than `max` are outstanding.
    pub fn try_begin_deferred(&self, max: u32) -> bool {
        let mut n = self.deferred.load(Ordering::Acquire);
        while n < max {
            match self
                .deferred
                .compare_exchange_weak(n, n + 1, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(now) => n = now,
            }
        }
        false
    }

    /// Release a slot taken by `try_begin_deferred`.
    pub fn end_deferred(&self) {
        self.deferred.fetch_sub(1, Ordering::AcqRel);
    }

    /// True (and the ping clock restarted) if a keep-alive ping is due.
    pub fn take_ping_due(&self, now: u64, interval_ms: u64) -> bool {
        let last = self.last_ping_ms.load(Ordering::Relaxed);
        now.saturating_sub(last) >= interval_ms
            && self
                .last_ping_ms
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::realtime::types::close_code;

    #[tokio::test]
    async fn first_close_request_wins_and_is_not_lost_without_a_waiter() {
        let s = SessionSignals::new();
        let spec = CloseSpec::new(close_code::POLICY, "replaced", None);
        assert!(s.request_close(CloseRequest::Spec(spec)));
        assert!(
            !s.request_close(CloseRequest::Idle),
            "second request ignored"
        );
        // Requested before anyone awaited: still delivered.
        match s.closed().await {
            CloseRequest::Spec(spec) => assert_eq!(spec.reason, "replaced"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn deferred_answers_are_bounded_and_released() {
        let s = SessionSignals::new();
        assert!(s.try_begin_deferred(2));
        assert!(s.try_begin_deferred(2));
        assert!(!s.try_begin_deferred(2), "budget of 2 exhausted");
        s.end_deferred();
        assert!(s.try_begin_deferred(2), "released slot is reusable");
    }

    #[test]
    fn ping_is_due_once_per_interval() {
        let s = SessionSignals::new();
        let t0 = now_ms();
        assert!(!s.take_ping_due(t0, 5_000));
        assert!(s.take_ping_due(t0 + 5_000, 5_000));
        assert!(!s.take_ping_due(t0 + 5_001, 5_000), "clock restarted");
    }
}
