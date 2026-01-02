use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;

use wsprism_core::error::{Result, WsPrismError};
use wsprism_core::protocol::hot::HotFrame;
use wsprism_core::protocol::text::Envelope;

use crate::realtime::RealtimeCtx;

/// Text services (Ext Lane). Can be extended by WASM later.
#[async_trait]
pub trait TextService: Send + Sync {
    fn svc(&self) -> &'static str;
    async fn handle(&self, ctx: RealtimeCtx, env: Envelope) -> Result<()>;
}

/// Binary services (Hot Lane). **Native only** (no WASM/script).
#[async_trait]
pub trait BinaryService: Send + Sync {
    fn svc_id(&self) -> u8;
    async fn handle_binary(&self, ctx: RealtimeCtx, frame: HotFrame) -> Result<()>;
}

/// Registry and dispatcher for Text (Ext lane) and Binary (Hot lane) services.
#[derive(Default)]
pub struct Dispatcher {
    text: DashMap<&'static str, Arc<dyn TextService>>,
    hot: DashMap<u8, Arc<dyn BinaryService>>,
}

impl Dispatcher {
    pub fn new() -> Self {
        Self {
            text: DashMap::new(),
            hot: DashMap::new(),
        }
    }

    pub fn register_text(&self, svc: Arc<dyn TextService>) {
        self.text.insert(svc.svc(), svc);
    }

    pub fn register_hot(&self, svc: Arc<dyn BinaryService>) {
        self.hot.insert(svc.svc_id(), svc);
    }

    pub fn registered_text_svcs(&self) -> Vec<&'static str> {
        self.text.iter().map(|e| *e.key()).collect()
    }

    pub fn registered_hot_svcs(&self) -> Vec<u8> {
        self.hot.iter().map(|e| *e.key()).collect()
    }

    pub async fn dispatch_text(&self, ctx: RealtimeCtx, env: Envelope) -> Result<()> {
        let svc = env.svc.as_str();
        let handler = self
            .text
            .get(svc)
            .ok_or_else(|| WsPrismError::BadRequest(format!("unknown svc: {svc}")))?
            .value()
            .clone();
        handler.handle(ctx, env).await
    }

    pub async fn dispatch_hot(&self, ctx: RealtimeCtx, frame: HotFrame) -> Result<()> {
        let sid = frame.svc_id;
        let handler = self
            .hot
            .get(&sid)
            .ok_or_else(|| WsPrismError::BadRequest(format!("unknown hot svc_id: {sid}")))?
            .value()
            .clone();
        handler.handle_binary(ctx, frame).await
    }
}
