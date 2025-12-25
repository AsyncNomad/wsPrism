use std::{collections::HashMap, sync::{Arc, RwLock}};

use crate::{
    error::{AppError, Result},
    realtime::{
        core::RealtimeCtx,
        protocol::{envelope::Envelope, inbound::BinaryFrame},
    },
};

pub trait RealtimeService: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn handle<'a>(&'a self, ctx: RealtimeCtx, env: Envelope)
        -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>;
}

pub trait BinaryService: Send + Sync + 'static {
    fn svc_id(&self) -> u8;
    fn handle_binary<'a>(&'a self, ctx: RealtimeCtx, frame: BinaryFrame)
        -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>;
}

#[derive(Default)]
pub struct Dispatcher {
    services: RwLock<HashMap<&'static str, Arc<dyn RealtimeService>>>,
    bin_services: RwLock<HashMap<u8, Arc<dyn BinaryService>>>,
}

impl Dispatcher {
    pub fn new() -> Self { Self::default() }

    pub fn register<S: RealtimeService>(&self, service: S) {
        let mut guard = self.services.write().unwrap();
        guard.insert(service.name(), Arc::new(service));
    }

    pub fn register_binary_id<S: BinaryService>(&self, id: u8, service: S) {
        let mut guard = self.bin_services.write().unwrap();
        guard.insert(id, Arc::new(service));
    }

    pub async fn dispatch(&self, ctx: RealtimeCtx, env: Envelope) -> Result<()> {
        let svc = {
            let guard = self.services.read().unwrap();
            guard
                .get(env.svc.as_str())
                .cloned()
                .ok_or_else(|| AppError::BadRequest(format!("unknown service: {}", env.svc)))?
        };
        svc.handle(ctx, env).await
    }

    pub async fn dispatch_binary(&self, ctx: RealtimeCtx, frame: BinaryFrame) -> Result<()> {
        let svc = {
            let guard = self.bin_services.read().unwrap();
            guard
                .get(&frame.svc_id)
                .cloned()
                .ok_or_else(|| AppError::BadRequest(format!("unknown binary svc_id: {}", frame.svc_id)))?
        };
        svc.handle_binary(ctx, frame).await
    }
}
