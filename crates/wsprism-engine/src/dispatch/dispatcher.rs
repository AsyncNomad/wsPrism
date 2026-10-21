use std::sync::Arc;

use async_trait::async_trait;
use std::collections::HashMap;

use wsprism_protocol::error::{Result, WsPrismError};
use wsprism_protocol::protocol::hot::HotFrame;
use wsprism_protocol::protocol::text::Envelope;

use crate::realtime::RealtimeCtx;

/// Text services (Ext Lane). Can be extended by WASM later.
#[async_trait]
pub trait TextService: Send + Sync {
    /// Service name: the `svc` field of the requests it handles (a static, bounded name; also the metric label).
    fn svc(&self) -> &'static str;
    /// Handle one request. An `Err` is sent to the client as `sys.error` (with `reply_to`).
    async fn handle(&self, ctx: &mut RealtimeCtx, env: Envelope) -> Result<()>;
}

/// Hot lane services (binary frames). **Native only** (no WASM/script).
#[async_trait]
pub trait HotService: Send + Sync {
    /// Hot service id: the `svc` byte of the frames it handles.
    fn svc_id(&self) -> u8;
    /// Handle one Hot frame. Must not block: it runs on the session task.
    async fn handle_hot(&self, ctx: &mut RealtimeCtx, frame: HotFrame) -> Result<()>;
}

/// Registry and dispatcher for Text (Ext lane) and Hot lane services.
///
/// Filled while the gateway is built (`&mut self`), then shared read-only:
/// lookups take no lock and clone no `Arc` (text: a map; Hot: a table
/// indexed by the service byte).
pub struct Dispatcher {
    text: HashMap<&'static str, Arc<dyn TextService>>,
    hot: Box<[Option<Arc<dyn HotService>>; 256]>,
}

impl Default for Dispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Dispatcher {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            text: HashMap::new(),
            hot: Box::new(std::array::from_fn(|_| None)),
        }
    }

    /// Register a text service (replaces one with the same name).
    pub fn register_text(&mut self, svc: Arc<dyn TextService>) {
        self.text.insert(svc.svc(), svc);
    }

    /// Register a Hot service (replaces one with the same id).
    pub fn register_hot(&mut self, svc: Arc<dyn HotService>) {
        let id = usize::from(svc.svc_id());
        self.hot[id] = Some(svc);
    }

    /// Names of the registered text services.
    pub fn registered_text_svcs(&self) -> Vec<&'static str> {
        self.text.keys().copied().collect()
    }

    /// The registered (static) name of text service `svc`, if any. Safe to
    /// use as a metric label, unlike the client-supplied string (S5).
    pub fn text_service_name(&self, svc: &str) -> Option<&'static str> {
        self.text.get_key_value(svc).map(|(k, _)| *k)
    }

    /// Ids of the registered Hot services.
    pub fn registered_hot_svcs(&self) -> Vec<u8> {
        (0..=u8::MAX)
            .filter(|&id| self.hot[usize::from(id)].is_some())
            .collect()
    }

    /// Route `env` to its text service (`BAD_REQUEST` if none).
    pub async fn dispatch_text(&self, ctx: &mut RealtimeCtx, env: Envelope) -> Result<()> {
        let Some(handler) = self.text.get(env.svc.as_str()) else {
            return Err(WsPrismError::BadRequest(format!(
                "unknown svc: {}",
                env.svc
            )));
        };
        handler.handle(ctx, env).await
    }

    /// Route `frame` to its Hot service (`BAD_REQUEST` if none).
    pub async fn dispatch_hot(&self, ctx: &mut RealtimeCtx, frame: HotFrame) -> Result<()> {
        let sid = frame.svc_id;
        let Some(handler) = &self.hot[usize::from(sid)] else {
            return Err(WsPrismError::BadRequest(format!(
                "unknown hot svc_id: {sid}"
            )));
        };
        handler.handle_hot(ctx, frame).await
    }
}
