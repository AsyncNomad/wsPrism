//! Shared application state for wsPrism Gateway.
//!
//! Sprint 3 Updated:
//! - Wire RealtimeCore + Dispatcher, and register built-in services.
//! - Make startup errors explicit (Result instead of panic).

use std::collections::HashMap;
use std::sync::Arc;

use wsprism_core::error::{Result, WsPrismError};

use crate::{config::GatewayConfig, policy};
use crate::dispatch::Dispatcher;
use crate::realtime::RealtimeCore;

// ✅ Sprint 3 Services
use crate::services::{ChatService, EchoBinaryService};

const FAIL_FAST_ON_MISMATCH: bool = false; // if changed to true, boot fails.

#[derive(Clone)]
pub struct AppState {
    inner: Arc<AppStateInner>,
    realtime: Arc<RealtimeCore>,
    dispatcher: Arc<Dispatcher>,
}

struct AppStateInner {
    cfg: GatewayConfig,
    tenant_policy: HashMap<String, Arc<policy::TenantPolicyRuntime>>,
}

impl AppState {
    /// Build application state.
    /// Returns Result so main can handle errors gracefully (no panic).
    pub fn new(cfg: GatewayConfig) -> Result<Self> {
        // 1) Compile tenant policy runtimes
        let mut tenant_policy = HashMap::new();
        for t in &cfg.tenants {
            let runtime = policy::TenantPolicyRuntime::new(
                t.id.clone(),
                t.limits.max_frame_bytes,
                &t.policy,
            )
            .map_err(|e| {
                // Panic 대신 명확한 에러 반환
                WsPrismError::BadRequest(format!(
                    "tenant policy compile failed (tenant={}): {e}",
                    t.id
                ))
            })?;

            tenant_policy.insert(t.id.clone(), Arc::new(runtime));
        }

        // 2) Create core components
        let realtime = Arc::new(RealtimeCore::new());
        let dispatcher = Dispatcher::new();

        // 3) Register built-in services (Sprint 3)
        
        // (1) ChatService ("chat")
        dispatcher.register_text(Arc::new(ChatService::new()));

        // (2) EchoBinaryService (svc_id: 1)
        dispatcher.register_hot(Arc::new(EchoBinaryService::new(1)));

        // allowlist <-> dispatcher sanity check
        {
            let text_svcs = dispatcher.registered_text_svcs();
            let hot_svcs = dispatcher.registered_hot_svcs();

            let exempt_text = ["room", "sys"]; // transport/internal

            for t in &cfg.tenants {
                // ext rules: "svc:type"
                for rule in &t.policy.ext_allowlist {
                    if let Some((svc, _ty)) = rule.split_once(':') {
                        if exempt_text.contains(&svc) { continue; }
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
            inner: Arc::new(AppStateInner { cfg, tenant_policy }),
            realtime,
            dispatcher: Arc::new(dispatcher),
        })
    }

    pub fn cfg(&self) -> &GatewayConfig {
        &self.inner.cfg
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
}
