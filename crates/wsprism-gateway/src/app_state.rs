//! Shared application state for wsPrism Gateway.
//!
//! Sprint 3 Updated:
//! - Wire RealtimeCore + Dispatcher, and register built-in services.
//! - Make startup errors explicit (Result instead of panic).

use std::collections::HashMap;
use std::sync::Arc;

use wsprism_protocol::error::{Result, WsPrismError};

use crate::auth::{AuthChain, TicketStore};
use crate::backend::Backend;
use crate::config::schema::BrokerConfig;
use crate::gateway::Extensions;
use crate::obs::metrics::GatewayMetrics;
use crate::rpc::RpcProxy;
use crate::{
    config::{GatewayConfig, TenantConfig},
    policy,
};
use wsprism_engine::broker::OutboxLimits;
use wsprism_engine::services::{ChatService, HotRelayService, RoomService};
use wsprism_engine::spi::Dispatcher;
use wsprism_engine::spi::{HistoryConfig, HistoryRule, RealtimeCore};
use wsprism_engine::{RoomLimits, RoomPattern};
// Sprint 5
use crate::transport::client_ip::TrustedProxies;
use crate::transport::handshake::HandshakeDefender;

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
    /// Per-tenant authentication chains (ADR 0008 §1).
    tenant_auth: HashMap<String, Arc<AuthChain>>,
    /// SHA-256 of each tenant API key -> tenant id (server API, ADR 0008 §3).
    api_keys: HashMap<[u8; 32], String>,
    /// Outstanding one-time tickets (ADR 0008 §4).
    tickets: Arc<TicketStore>,
    /// Per-tenant RPC proxy (`tenants[].rpc`, ADR 0008 §6).
    tenant_rpc: HashMap<String, Arc<RpcProxy>>,
    /// Per-tenant room limits, shared by every session of the tenant.
    room_limits: HashMap<String, Arc<RoomLimits>>,
    tenant_policy: HashMap<String, Arc<policy::TenantPolicyRuntime>>,
    /// tenant id -> index into `cfg.tenants` (O(1) lookup per upgrade).
    tenant_index: HashMap<String, usize>,
}

impl AppState {
    /// Build application state (config + compiled policies + runtimes).
    ///
    /// Returns `Result` so the binary can surface startup errors without panic.
    pub(crate) fn with_extensions(cfg: GatewayConfig, ext: Extensions) -> Result<Self> {
        let metrics = Arc::new(GatewayMetrics::default());
        // Sprint 5
        let handshake = Arc::new(HandshakeDefender::new(cfg.gateway.handshake_limit.clone()));
        let trusted_proxies = Arc::new(TrustedProxies::parse(&cfg.gateway.trusted_proxies)?);

        // 1) Compile tenant policy runtimes
        let mut tenant_policy = HashMap::new();
        for t in &cfg.tenants {
            let runtime = policy::TenantPolicyRuntime::new(t.limits.max_frame_bytes, &t.policy)
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

        // Server API keys (validated as 64 hex chars by the config).
        let mut api_keys = HashMap::new();
        for t in &cfg.tenants {
            for k in &t.api_keys {
                let mut digest = [0u8; 32];
                for (i, b) in digest.iter_mut().enumerate() {
                    *b = u8::from_str_radix(&k.sha256[2 * i..2 * i + 2], 16).map_err(|_| {
                        WsPrismError::BadRequest(format!(
                            "tenant {}: api key {} is not hex",
                            t.id, k.name
                        ))
                    })?;
                }
                if api_keys.insert(digest, t.id.clone()).is_some() {
                    return Err(WsPrismError::BadRequest(format!(
                        "api key {} (tenant {}) is configured more than once",
                        k.name, t.id
                    )));
                }
            }
        }

        if !cfg.gateway.public_metrics && cfg.gateway.api.is_none() {
            tracing::warn!(
                "metrics are not served: set gateway.api.listen (internal listener) or gateway.public_metrics"
            );
        }

        // Authentication chains.
        let tickets = Arc::new(TicketStore::default());
        let backend = Backend::new()?;
        let mut tenant_auth = HashMap::new();
        for t in &cfg.tenants {
            let chain = AuthChain::from_config(&t.auth, &tickets, &backend, &ext.auth_plugins)
                .map_err(|e| {
                    WsPrismError::BadRequest(format!("tenant {}: {}", t.id, e.client_message()))
                })?;
            if chain.is_dev_only() {
                tracing::warn!(
                    tenant = %t.id,
                    "tenant uses only the dev authenticator (anyone presenting `dev` is admitted); set tenants[].auth for production"
                );
            }
            tenant_auth.insert(t.id.clone(), Arc::new(chain));
        }

        // 2) Create core components
        let mut history = HistoryConfig::default();
        for t in &cfg.tenants {
            for h in &t.history {
                history.add(
                    &t.id,
                    HistoryRule::new(
                        RoomPattern::parse(&h.rooms),
                        h.size,
                        std::time::Duration::from_secs(h.ttl_s),
                        h.max_bytes,
                    ),
                );
            }
        }
        let broker = match ext.broker {
            Some(b) => Some(b),
            None => configured_broker(&cfg.gateway.broker)?,
        };
        let realtime = Arc::new(
            match broker {
                None => RealtimeCore::new(),
                Some((broker, limits)) => RealtimeCore::with_broker(broker, limits)?,
            }
            .with_history(history)?,
        );
        let mut dispatcher = Dispatcher::new();

        // 3) Register built-in services (Sprint 3)
        dispatcher.register_text(Arc::new(RoomService::new()));
        dispatcher.register_text(Arc::new(ChatService::new()));
        dispatcher.register_hot(Arc::new(HotRelayService::new(1)));

        // User services (GatewayBuilder): may not take a built-in or an
        // already registered name — a silent override would be worse.
        for svc in ext.text {
            let name = svc.svc();
            if name == "sys" || dispatcher.text_service_name(name).is_some() {
                return Err(WsPrismError::BadRequest(format!(
                    "text service `{name}` is a built-in or registered twice"
                )));
            }
            dispatcher.register_text(svc);
        }
        for svc in ext.hot {
            let id = svc.svc_id();
            if dispatcher.registered_hot_svcs().contains(&id) {
                return Err(WsPrismError::BadRequest(format!(
                    "hot service id {id} is a built-in or registered twice"
                )));
            }
            dispatcher.register_hot(svc);
        }

        // RPC proxies (may not claim a built-in service name).
        let room_limits = cfg
            .tenants
            .iter()
            .map(|t| (t.id.clone(), Arc::new(t.limits.room_limits())))
            .collect();
        let mut tenant_rpc = HashMap::new();
        for t in &cfg.tenants {
            if let Some(rpc) = &t.rpc {
                let proxy =
                    RpcProxy::from_config(rpc, &backend, &dispatcher.registered_text_svcs())
                        .map_err(|e| {
                            WsPrismError::BadRequest(format!(
                                "tenant {}: {}",
                                t.id,
                                e.client_message()
                            ))
                        })?;
                tenant_rpc.insert(t.id.clone(), Arc::new(proxy));
            }
        }

        // Allowlist <-> services: every allowlisted service must exist. A rule
        // naming nothing is almost always a typo that would otherwise only
        // show up as clients' requests failing (ADR 0009, user decision
        // 2026-10-19; it only warned before).
        {
            let text_svcs = dispatcher.registered_text_svcs();
            let hot_svcs = dispatcher.registered_hot_svcs();
            for t in &cfg.tenants {
                for rule in &t.policy.ext_allowlist {
                    let Some((svc, _)) = rule.split_once(':') else {
                        continue;
                    };
                    let by_rpc = tenant_rpc
                        .get(&t.id)
                        .is_some_and(|r: &Arc<RpcProxy>| r.covers_svc(svc));
                    // `sys` is reserved for gateway-originated messages.
                    if svc != "sys" && !text_svcs.contains(&svc) && !by_rpc {
                        return Err(WsPrismError::BadRequest(format!(
                            "tenant {}: ext_allowlist rule `{rule}` names service `{svc}`, which is not registered \
                             (built-ins: room, chat; add yours with GatewayBuilder::text_service or route it with tenants[].rpc)",
                            t.id
                        )));
                    }
                }
                for rule in &t.policy.hot_allowlist {
                    let Some(sid) = rule.split_once(':').and_then(|(s, _)| s.parse::<u8>().ok())
                    else {
                        continue;
                    };
                    if !hot_svcs.contains(&sid) {
                        return Err(WsPrismError::BadRequest(format!(
                            "tenant {}: hot_allowlist rule `{rule}` names hot service {sid}, which is not registered \
                             (built-in: 1; add yours with GatewayBuilder::hot_service)",
                            t.id
                        )));
                    }
                }
            }
        }

        Ok(Self {
            inner: Arc::new(AppStateInner {
                tenant_auth,
                api_keys,
                tickets,
                tenant_rpc,
                room_limits,
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

    /// The tenant whose API key has this SHA-256, if any.
    pub fn api_key_tenant(&self, digest: &[u8; 32]) -> Option<String> {
        self.inner.api_keys.get(digest).cloned()
    }

    /// The tenant's RPC proxy, if it has one.
    /// The tenant's room limits (unlimited for an unknown tenant).
    pub fn room_limits(&self, tenant_id: &str) -> Arc<RoomLimits> {
        self.inner
            .room_limits
            .get(tenant_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn rpc(&self, tenant_id: &str) -> Option<Arc<RpcProxy>> {
        self.inner.tenant_rpc.get(tenant_id).cloned()
    }

    /// Outstanding one-time tickets.
    pub fn tickets(&self) -> Arc<TicketStore> {
        Arc::clone(&self.inner.tickets)
    }

    /// The tenant's authentication chain.
    pub fn auth(&self, tenant_id: &str) -> Option<Arc<AuthChain>> {
        self.inner.tenant_auth.get(tenant_id).cloned()
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
        let c = self.realtime.counters();
        let mut v = vec![
            ("wsprism_egress_drop_total", c.drops()),
            (
                "wsprism_egress_slow_consumer_closes_total",
                c.slow_consumer_closes(),
            ),
            ("wsprism_hot_slot_overwritten_total", c.slot_overwrites()),
            ("wsprism_hot_slot_full_drops_total", c.slot_full_drops()),
        ];
        // Queued broker only (ADR 0011 B4).
        if let Some(b) = self.realtime.broker_counters() {
            v.extend([
                ("wsprism_broker_outbox_refused_total", b.refused()),
                ("wsprism_broker_retries_total", b.retries()),
                ("wsprism_broker_failed_total", b.failed()),
            ]);
        }
        v
    }
}

/// The broker `gateway.broker` asks for; `None` = one node (memory).
type ConfiguredBroker = Option<(Arc<dyn wsprism_engine::broker::Broker>, OutboxLimits)>;

fn configured_broker(cfg: &BrokerConfig) -> Result<ConfiguredBroker> {
    match cfg {
        BrokerConfig::Memory => Ok(None),
        #[cfg(feature = "redis")]
        BrokerConfig::Redis(r) => {
            let redis_cfg = wsprism_redis::RedisConfig::new(r.resolve_url()?)
                .prefix(r.prefix.clone())
                .pubsub_connections(r.pubsub_connections)
                .slot_ttl(std::time::Duration::from_secs(r.slot_ttl_s));
            let broker = wsprism_redis::RedisBroker::new(redis_cfg)
                .map_err(|e| WsPrismError::BadRequest(format!("gateway.broker: {e}")))?;
            Ok(Some((
                Arc::new(broker),
                OutboxLimits::new(r.outbox_capacity, r.outbox_per_tenant),
            )))
        }
        #[cfg(not(feature = "redis"))]
        BrokerConfig::Redis(_) => Err(WsPrismError::BadRequest(
            "gateway.broker.kind redis needs the gateway's `redis` feature".into(),
        )),
        #[allow(unreachable_patterns)]
        _ => Err(WsPrismError::BadRequest(
            "unsupported gateway.broker".into(),
        )),
    }
}
