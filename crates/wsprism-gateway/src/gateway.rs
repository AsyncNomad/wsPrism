//! The public entry point (ADR 0009 §3): build a gateway from a config plus
//! your own services and authenticators, then serve it.
//!
//! ```no_run
//! # async fn run() -> wsprism_protocol::Result<()> {
//! wsprism_gateway::Gateway::builder()
//!     .config_file("wsprism.yaml")?
//!     // .text_service(MyService::new())
//!     // .auth_plugin("corp_sso", |options| Ok(Arc::new(CorpSso::from(options)?)))
//!     .build()?
//!     .serve()
//!     .await
//! # }
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use tokio::net::TcpListener;
use wsprism_engine::broker::{Broker, OutboxLimits};
use wsprism_engine::{HotService, TextService};
use wsprism_protocol::error::{Result, WsPrismError};

use crate::app_state::AppState;
use crate::auth::Authenticator;
use crate::config::{self, GatewayConfig};

/// Builds an authenticator from the `options` of a `kind: plugin` entry in
/// `tenants[].auth` (called once per entry at build time).
pub type AuthPluginFactory =
    Arc<dyn Fn(&serde_json::Value) -> Result<Arc<dyn Authenticator>> + Send + Sync>;

/// What the builder adds to the built-in services and authenticators.
#[derive(Default)]
pub(crate) struct Extensions {
    pub(crate) text: Vec<Arc<dyn TextService>>,
    pub(crate) hot: Vec<Arc<dyn HotService>>,
    pub(crate) auth_plugins: HashMap<String, AuthPluginFactory>,
    /// `None` = the single-node `MemoryBroker`.
    pub(crate) broker: Option<(Arc<dyn Broker>, OutboxLimits)>,
}

/// Collects config and extensions; [`GatewayBuilder::build`] validates
/// everything together.
#[derive(Default)]
#[must_use]
pub struct GatewayBuilder {
    config: Option<GatewayConfig>,
    ext: Extensions,
}

impl GatewayBuilder {
    pub fn config(mut self, cfg: GatewayConfig) -> Self {
        self.config = Some(cfg);
        self
    }

    /// Load and validate a YAML config file.
    pub fn config_file(self, path: impl AsRef<Path>) -> Result<Self> {
        let cfg = config::load_from_file(&path.as_ref().to_string_lossy())?;
        Ok(self.config(cfg))
    }

    /// Add an Ext-lane service. Its `svc()` must not be a built-in name
    /// (`room`, `chat`, `sys`) or another service's.
    pub fn text_service(mut self, svc: impl TextService + 'static) -> Self {
        self.ext.text.push(Arc::new(svc));
        self
    }

    /// Add a Hot-lane service. Its `svc_id()` must not be 1 (the built-in
    /// relay) or another service's.
    pub fn hot_service(mut self, svc: impl HotService + 'static) -> Self {
        self.ext.hot.push(Arc::new(svc));
        self
    }

    /// Make `kind: plugin` + `name: <name>` available in `tenants[].auth`.
    pub fn auth_plugin<F>(mut self, name: impl Into<String>, factory: F) -> Self
    where
        F: Fn(&serde_json::Value) -> Result<Arc<dyn Authenticator>> + Send + Sync + 'static,
    {
        self.ext.auth_plugins.insert(name.into(), Arc::new(factory));
        self
    }

    /// Route room traffic through `broker` (ADR 0011). **Experimental**:
    /// the broker trait changes until the Redis broker ships (R3); for this
    /// repository's tests only.
    #[doc(hidden)]
    pub fn broker(mut self, broker: Arc<dyn Broker>, limits: OutboxLimits) -> Self {
        self.ext.broker = Some((broker, limits));
        self
    }

    /// Validate the config against the registered extensions, compile
    /// policies and load keys. Nothing is bound yet.
    pub fn build(self) -> Result<Gateway> {
        let cfg = self.config.ok_or_else(|| {
            WsPrismError::BadRequest("no config given to the gateway builder".into())
        })?;
        Ok(Gateway {
            state: AppState::with_extensions(cfg, self.ext)?,
        })
    }
}

/// A built gateway: validated, ready to serve. Cheap to clone (shared state).
#[derive(Clone)]
pub struct Gateway {
    state: AppState,
}

impl Gateway {
    pub fn builder() -> GatewayBuilder {
        GatewayBuilder::default()
    }

    pub fn config(&self) -> &GatewayConfig {
        self.state.cfg()
    }

    /// Bind `gateway.listen` (and `gateway.api.listen` if set) and serve
    /// until Ctrl-C / SIGTERM, then drain.
    pub async fn serve(self) -> Result<()> {
        let gw = &self.config().gateway;
        let listen: SocketAddr = gw.listen.parse().map_err(|e| {
            WsPrismError::BadRequest(format!("gateway.listen must be host:port: {e}"))
        })?;
        let listener = TcpListener::bind(listen)
            .await
            .map_err(|e| WsPrismError::BadRequest(format!("failed to bind {listen}: {e}")))?;
        let api = match &gw.api {
            None => None,
            Some(api) => Some(TcpListener::bind(&api.listen).await.map_err(|e| {
                WsPrismError::BadRequest(format!(
                    "failed to bind gateway.api.listen {}: {e}",
                    api.listen
                ))
            })?),
        };
        let api_listen = gw.api.as_ref().map_or("off", |a| a.listen.as_str());
        tracing::info!(%listen, api = %api_listen, "wsprism-gateway serving");
        self.serve_on(listener, api, shutdown_signal())
            .await
            .map_err(|e| WsPrismError::BadRequest(format!("server failed: {e}")))
    }

    /// Serve on listeners you bound (embedding, tests) until `shutdown`
    /// completes, then drain. `api` serves the server API if given.
    pub async fn serve_on(
        self,
        listener: TcpListener,
        api: Option<TcpListener>,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        crate::server::serve_with_api(listener, api, self.state, shutdown).await
    }

    /// The Prometheus exposition served at `/metrics`.
    pub fn metrics_text(&self) -> String {
        self.state.metrics().render(&self.state.metrics_extra())
    }

    /// Connected sessions across all tenants.
    pub fn session_count(&self) -> usize {
        self.state.realtime().sessions.len_sessions()
    }

    /// Enter draining mode now: readiness turns 503, new upgrades are
    /// refused and connected sessions are asked to close (1001 / 4103).
    pub fn drain(&self) {
        self.state.enter_draining();
        self.state.realtime().best_effort_shutdown_all("draining");
    }

    /// Internal state, for this crate's tests and benchmarks only.
    #[cfg(feature = "internals")]
    #[doc(hidden)]
    pub fn __state(&self) -> &AppState {
        &self.state
    }
}

/// Ctrl-C, or SIGTERM on unix.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::warn!(
                    "cannot install a SIGTERM handler ({e}); only Ctrl-C stops the gateway"
                );
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}
