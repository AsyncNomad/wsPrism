//! Shared application state for wsPrism Gateway.
//!
//! Sprint 3 Updated:
//! - Wire RealtimeCore + Dispatcher, and register built-in services.
//! - Make startup errors explicit (Result instead of panic).

use std::collections::HashMap;
use std::sync::Arc;

use wsprism_core::error::{Result, WsPrismError};

use crate::dispatch::Dispatcher;
use crate::obs::metrics::GatewayMetrics;
use crate::realtime::RealtimeCore;
use crate::services::{ChatService, EchoBinaryService, RoomService};
use crate::{
    config::{GatewayConfig, TenantConfig},
    policy,
};
// Sprint 5
use crate::transport::client_ip::TrustedProxies;
use crate::transport::handshake::HandshakeDefender;

/// If true, the gateway fails fast on allowlist/dispatcher mismatches at boot.
const FAIL_FAST_ON_MISMATCH: bool = false;

/// Shared, clonable gateway application state (config + policy + runtimes).
#[derive(Clone)]
pub struct AppState {
    inner: Arc<AppStateInner>,
    realtime: Arc<RealtimeCore>,
    dispatcher: Arc<Dispatcher>,
    metrics: Arc<GatewayMetrics>,
    // Sprint 5
    handshake: Arc<HandshakeDefender>,
    trusted_proxies: Arc<TrustedProxies>,
}

struct AppStateInner {
    cfg: GatewayConfig,
    tenant_policy: HashMap<String, Arc<policy::TenantPolicyRuntime>>,
    /// tenant id -> index into `cfg.tenants` (O(1) lookup per upgrade).
    tenant_index: HashMap<String, usize>,
}

impl AppState {
    /// Build application state (config + compiled policies + runtimes).
    ///
    /// Returns `Result` so the binary can surface startup errors without panic.
    pub fn new(cfg: GatewayConfig) -> Result<Self> {
        let metrics = Arc::new(GatewayMetrics::default());
        // Sprint 5
        let handshake = Arc::new(HandshakeDefender::new(cfg.gateway.handshake_limit.clone()));
        let trusted_proxies = Arc::new(TrustedProxies::parse(&cfg.gateway.trusted_proxies)?);

        // 1) Compile tenant policy runtimes
        let mut tenant_policy = HashMap::new();
        for t in &cfg.tenants {
            let runtime =
                policy::TenantPolicyRuntime::new(t.id.clone(), t.limits.max_frame_bytes, &t.policy)
                    .map_err(|e| {
                        WsPrismError::BadRequest(format!(
                            "tenant policy compile failed (tenant={}): {e}",
                            t.id
                        ))
                    })?;

            if runtime.origins().denies_all_browsers() {
                tracing::warn!(
                    tenant = %t.id,
                    "policy.allowed_origins is empty: browser clients (requests with an Origin header) will be rejected with 403"
                );
            }
            tenant_policy.insert(t.id.clone(), Arc::new(runtime));
        }

        // 2) Create core components
        let realtime = Arc::new(RealtimeCore::new());
        let dispatcher = Dispatcher::new();

        // 3) Register built-in services (Sprint 3)
        dispatcher.register_text(Arc::new(RoomService::new()));
        dispatcher.register_text(Arc::new(ChatService::new()));
        dispatcher.register_hot(Arc::new(EchoBinaryService::new(1)));

        // allowlist <-> dispatcher sanity check
        {
            let text_svcs = dispatcher.registered_text_svcs();
            let hot_svcs = dispatcher.registered_hot_svcs();

            let exempt_text = ["sys"]; // reserved for gateway-originated messages

            for t in &cfg.tenants {
                // ext rules: "svc:type"
                for rule in &t.policy.ext_allowlist {
                    if let Some((svc, _ty)) = rule.split_once(':') {
                        if exempt_text.contains(&svc) {
                            continue;
                        }
                        if !text_svcs.contains(&svc) {
                            tracing::warn!(tenant=%t.id, rule=%rule, "ext_allowlist refers to unregistered text service");
                            if FAIL_FAST_ON_MISMATCH {
                                return Err(WsPrismError::BadRequest(format!(
                                    "tenant {} ext_allowlist references unregistered text service: {}",
                                    t.id, svc
                                )));
                            }
                        }
                    }
                }

                // hot rules: "sid:opcode"
                for rule in &t.policy.hot_allowlist {
                    if let Some((sid_s, _op)) = rule.split_once(':') {
                        if let Ok(sid) = sid_s.parse::<u8>() {
                            if !hot_svcs.contains(&sid) {
                                tracing::warn!(tenant=%t.id, rule=%rule, sid=%sid, "hot_allowlist refers to unregistered binary service");
                                if FAIL_FAST_ON_MISMATCH {
                                    return Err(WsPrismError::BadRequest(format!(
                                        "tenant {} hot_allowlist references unregistered hot service id: {}",
                                        t.id, sid
                                    )));
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(Self {
            inner: Arc::new(AppStateInner {
                tenant_index: cfg
                    .tenants
                    .iter()
                    .enumerate()
                    .map(|(i, t)| (t.id.clone(), i))
                    .collect(),
                cfg,
                tenant_policy,
            }),
            realtime,
            dispatcher: Arc::new(dispatcher),
            metrics,
            handshake,
            trusted_proxies,
        })
    }

    pub fn cfg(&self) -> &GatewayConfig {
        &self.inner.cfg
    }

    /// Configuration of a tenant by id.
    pub fn tenant(&self, tenant_id: &str) -> Option<&TenantConfig> {
        let i = *self.inner.tenant_index.get(tenant_id)?;
        self.inner.cfg.tenants.get(i)
    }

    pub fn tenant_policy(&self, tenant_id: &str) -> Option<Arc<policy::TenantPolicyRuntime>> {
        self.inner.tenant_policy.get(tenant_id).cloned()
    }

    pub fn resolve_ticket(&self, ticket: &str) -> Result<String> {
        match ticket {
            "dev" => Ok("user:dev".to_string()),
            _ => Err(WsPrismError::AuthFailed),
        }
    }

    pub fn realtime(&self) -> Arc<RealtimeCore> {
        Arc::clone(&self.realtime)
    }

    pub fn dispatcher(&self) -> Arc<Dispatcher> {
        Arc::clone(&self.dispatcher)
    }

    pub fn metrics(&self) -> Arc<GatewayMetrics> {
        Arc::clone(&self.metrics)
    }

    pub fn trusted_proxies(&self) -> &TrustedProxies {
        &self.trusted_proxies
    }

    pub fn handshake(&self) -> Arc<HandshakeDefender> {
        Arc::clone(&self.handshake)
    }

    pub fn is_draining(&self) -> bool {
        self.metrics.is_draining()
    }

    /// Enter draining mode (idempotent).
    pub fn enter_draining(&self) {
        self.metrics.set_draining();
    }

    /// Extra counters that are owned by other modules (egress drop/timeouts).
    pub fn metrics_extra(&self) -> Vec<(&'static str, u64)> {
        vec![
            (
                "wsprism_egress_drop_total",
                crate::realtime::core::egress_drop_count(),
            ),
            (
                "wsprism_egress_slow_consumer_closes_total",
                crate::realtime::core::egress_slow_consumer_count(),
            ),
        ]
    }
}
