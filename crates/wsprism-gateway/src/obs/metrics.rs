use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Minimal gateway metrics (Prometheus text exposition).
///
/// Sprint 4: Provide basic operability signals without adding new dependencies.
#[derive(Default)]
pub struct GatewayMetrics {
    // lifecycle
    ws_upgrades_total: AtomicU64,
    ws_sessions_accepted_total: AtomicU64,
    ws_sessions_closed_total: AtomicU64,
    ws_sessions_active: AtomicU64,

    // ingress/errors
    decode_error_total: AtomicU64,
    policy_drop_total: AtomicU64,
    policy_reject_total: AtomicU64,
    policy_close_total: AtomicU64,
    unknown_service_total: AtomicU64,

    // writer/hardening
    writer_send_timeout_total: AtomicU64,
    kicked_total: AtomicU64,

    // ops
    draining: AtomicBool,
    draining_initiated_total: AtomicU64,
}

impl GatewayMetrics {
    pub fn inc_ws_upgrades(&self) {
        self.ws_upgrades_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn on_session_open(&self) {
        self.ws_sessions_accepted_total.fetch_add(1, Ordering::Relaxed);
        self.ws_sessions_active.fetch_add(1, Ordering::Relaxed);
    }

    pub fn on_session_close(&self) {
        self.ws_sessions_closed_total.fetch_add(1, Ordering::Relaxed);
        // expected to never underflow (open/close are paired by design)
        self.ws_sessions_active.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn inc_decode_error(&self) {
        self.decode_error_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_policy_drop(&self) {
        self.policy_drop_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_policy_reject(&self) {
        self.policy_reject_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_policy_close(&self) {
        self.policy_close_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_unknown_service(&self) {
        self.unknown_service_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_writer_send_timeout(&self) {
        self.writer_send_timeout_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_kicked(&self) {
        self.kicked_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Enter draining mode.
    /// Returns true if this call flipped the state from false -> true.
    pub fn set_draining(&self) -> bool {
        let prev = self.draining.swap(true, Ordering::SeqCst);
        if !prev {
            self.draining_initiated_total.fetch_add(1, Ordering::Relaxed);
        }
        !prev
    }

    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Relaxed)
    }

    pub fn render(&self, extra: &[(&str, u64)]) -> String {
        let mut out = String::new();

        // ---- ws lifecycle
        out.push_str("# HELP wsprism_ws_upgrades_total Total HTTP->WS upgrade attempts.\n");
        out.push_str("# TYPE wsprism_ws_upgrades_total counter\n");
        out.push_str(&format!(
            "wsprism_ws_upgrades_total {}\n",
            self.ws_upgrades_total.load(Ordering::Relaxed)
        ));

        out.push_str("# HELP wsprism_ws_sessions_active Currently active WebSocket sessions.\n");
        out.push_str("# TYPE wsprism_ws_sessions_active gauge\n");
        out.push_str(&format!(
            "wsprism_ws_sessions_active {}\n",
            self.ws_sessions_active.load(Ordering::Relaxed)
        ));

        out.push_str("# HELP wsprism_ws_sessions_accepted_total Total accepted WebSocket sessions.\n");
        out.push_str("# TYPE wsprism_ws_sessions_accepted_total counter\n");
        out.push_str(&format!(
            "wsprism_ws_sessions_accepted_total {}\n",
            self.ws_sessions_accepted_total.load(Ordering::Relaxed)
        ));

        out.push_str("# HELP wsprism_ws_sessions_closed_total Total closed WebSocket sessions.\n");
        out.push_str("# TYPE wsprism_ws_sessions_closed_total counter\n");
        out.push_str(&format!(
            "wsprism_ws_sessions_closed_total {}\n",
            self.ws_sessions_closed_total.load(Ordering::Relaxed)
        ));

        // ---- ingress / errors
        out.push_str("# HELP wsprism_decode_error_total Total inbound decode errors.\n");
        out.push_str("# TYPE wsprism_decode_error_total counter\n");
        out.push_str(&format!(
            "wsprism_decode_error_total {}\n",
            self.decode_error_total.load(Ordering::Relaxed)
        ));

        out.push_str("# HELP wsprism_policy_drop_total Total policy drops.\n");
        out.push_str("# TYPE wsprism_policy_drop_total counter\n");
        out.push_str(&format!(
            "wsprism_policy_drop_total {}\n",
            self.policy_drop_total.load(Ordering::Relaxed)
        ));

        out.push_str("# HELP wsprism_policy_reject_total Total policy rejects.\n");
        out.push_str("# TYPE wsprism_policy_reject_total counter\n");
        out.push_str(&format!(
            "wsprism_policy_reject_total {}\n",
            self.policy_reject_total.load(Ordering::Relaxed)
        ));

        out.push_str("# HELP wsprism_policy_close_total Total policy closes.\n");
        out.push_str("# TYPE wsprism_policy_close_total counter\n");
        out.push_str(&format!(
            "wsprism_policy_close_total {}\n",
            self.policy_close_total.load(Ordering::Relaxed)
        ));

        out.push_str("# HELP wsprism_unknown_service_total Total unknown service dispatch errors.\n");
        out.push_str("# TYPE wsprism_unknown_service_total counter\n");
        out.push_str(&format!(
            "wsprism_unknown_service_total {}\n",
            self.unknown_service_total.load(Ordering::Relaxed)
        ));

        // ---- hardening
        out.push_str("# HELP wsprism_writer_send_timeout_total Total outbound writer send timeouts.\n");
        out.push_str("# TYPE wsprism_writer_send_timeout_total counter\n");
        out.push_str(&format!(
            "wsprism_writer_send_timeout_total {}\n",
            self.writer_send_timeout_total.load(Ordering::Relaxed)
        ));

        out.push_str("# HELP wsprism_kicked_total Total sessions kicked by policy.\n");
        out.push_str("# TYPE wsprism_kicked_total counter\n");
        out.push_str(&format!(
            "wsprism_kicked_total {}\n",
            self.kicked_total.load(Ordering::Relaxed)
        ));

        // ---- ops
        out.push_str("# HELP wsprism_draining Whether the gateway is in draining mode.\n");
        out.push_str("# TYPE wsprism_draining gauge\n");
        out.push_str(&format!(
            "wsprism_draining {}\n",
            if self.is_draining() { 1 } else { 0 }
        ));

        out.push_str("# HELP wsprism_draining_initiated_total Number of times draining was initiated.\n");
        out.push_str("# TYPE wsprism_draining_initiated_total counter\n");
        out.push_str(&format!(
            "wsprism_draining_initiated_total {}\n",
            self.draining_initiated_total.load(Ordering::Relaxed)
        ));

        // ---- extra counters (egress, etc)
        for (name, val) in extra {
            out.push_str(&format!("{} {}\n", name, val));
        }

        out
    }
}
