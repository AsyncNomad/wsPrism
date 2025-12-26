use std::sync::Arc;

use crate::infra::TicketStore;
use crate::realtime::core::{Dispatcher, RealtimeCore};

#[derive(Clone)]
pub struct AppState {
    pub core: Arc<RealtimeCore>,
    pub dispatcher: Arc<Dispatcher>,
    pub ticket_store: Arc<dyn TicketStore>,
}

impl AppState {
    pub fn new(dispatcher: Dispatcher, ticket_store: Arc<dyn TicketStore>) -> Self {
        let dispatcher = Arc::new(dispatcher);
        let core = Arc::new(RealtimeCore::new());
        Self { core, dispatcher, ticket_store }
    }
}
