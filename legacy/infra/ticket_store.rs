use dashmap::DashMap;
use crate::error::{AppError, Result};

pub trait TicketStore: Send + Sync {
    fn consume_ticket(&self, ticket: &str) -> Result<String>;
}

pub struct InMemoryTicketStore {
    tickets: DashMap<String, String>,
}

impl InMemoryTicketStore {
    pub fn new() -> Self {
        let this = Self { tickets: DashMap::new() };
        this.tickets.insert("dev".into(), "user:dev".into());
        this
    }

    #[allow(dead_code)]
    pub fn insert(&self, ticket: impl Into<String>, user_id: impl Into<String>) {
        self.tickets.insert(ticket.into(), user_id.into());
    }
}

impl TicketStore for InMemoryTicketStore {
    fn consume_ticket(&self, ticket: &str) -> Result<String> {
        self.tickets.remove(ticket).map(|(_, uid)| uid).ok_or(AppError::AuthFailed)
    }
}
