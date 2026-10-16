//! One-time tickets (ADR 0008 §4): a backend asks the server API for a
//! short-lived ticket for one user; the client presents it once.
//!
//! Tickets exist so long-lived credentials never travel in URLs (browsers
//! and WebTransport can only pass a credential in the URL or subprotocol).
//! Stored in process memory (single node; a shared store comes with M4).

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use dashmap::DashMap;
use ring::rand::{SecureRandom, SystemRandom};

use super::{AuthError, AuthRequest, Authenticator, Identity};

/// Every ticket starts with this, so the chain can route it.
pub const TICKET_PREFIX: &str = "wpt_";
pub const DEFAULT_TICKET_TTL: Duration = Duration::from_secs(30);
pub const MAX_TICKET_TTL: Duration = Duration::from_secs(300);
/// Outstanding tickets kept at most (memory bound against a runaway backend).
pub const MAX_TICKETS: usize = 1_000_000;

struct Entry {
    tenant: String,
    identity: Identity,
    expires: Instant,
}

#[derive(Debug, PartialEq, Eq)]
pub enum IssueError {
    /// Too many outstanding tickets.
    Full,
    /// The system random source failed.
    Random,
}

pub struct TicketStore {
    tickets: DashMap<String, Entry>,
    rng: SystemRandom,
}

impl Default for TicketStore {
    fn default() -> Self {
        Self {
            tickets: DashMap::new(),
            rng: SystemRandom::new(),
        }
    }
}

impl TicketStore {
    /// A new ticket for `identity` in `tenant`, valid for `ttl`.
    pub fn issue(
        &self,
        tenant: &str,
        identity: Identity,
        ttl: Duration,
    ) -> Result<String, IssueError> {
        if self.tickets.len() >= MAX_TICKETS {
            return Err(IssueError::Full);
        }
        let mut raw = [0u8; 32];
        self.rng.fill(&mut raw).map_err(|_| IssueError::Random)?;
        let token = format!(
            "{TICKET_PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
        );
        self.tickets.insert(
            token.clone(),
            Entry {
                tenant: tenant.to_string(),
                identity,
                expires: Instant::now() + ttl,
            },
        );
        Ok(token)
    }

    /// Consume `token`: the identity if it is live and issued for `tenant`.
    /// The ticket is gone afterwards either way (a misuse burns it).
    pub fn redeem(&self, tenant: &str, token: &str) -> Option<Identity> {
        let (_, e) = self.tickets.remove(token)?;
        (e.tenant == tenant && Instant::now() < e.expires).then_some(e.identity)
    }

    /// Drop expired tickets (called by the sweeper).
    pub fn sweep(&self, now: Instant) {
        self.tickets.retain(|_, e| e.expires > now);
    }

    pub fn len(&self) -> usize {
        self.tickets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tickets.is_empty()
    }
}

/// `kind: ticket` in a tenant's auth chain.
pub struct TicketAuthenticator {
    store: Arc<TicketStore>,
}

impl TicketAuthenticator {
    pub fn new(store: Arc<TicketStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Authenticator for TicketAuthenticator {
    fn kind(&self) -> &'static str {
        "ticket"
    }

    async fn authenticate(&self, req: &AuthRequest<'_>) -> Result<Identity, AuthError> {
        if !req.token.starts_with(TICKET_PREFIX) {
            return Err(AuthError::NotMine);
        }
        self.store
            .redeem(req.tenant, req.token)
            .ok_or(AuthError::Rejected(
                "unknown, used, expired or foreign ticket",
            ))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn tickets_are_single_use_tenant_bound_and_expire() {
        let s = TicketStore::default();
        let t = s
            .issue("a", Identity::user("u"), DEFAULT_TICKET_TTL)
            .unwrap();
        assert!(t.starts_with(TICKET_PREFIX) && t.len() > 40);
        assert_eq!(s.redeem("a", &t), Some(Identity::user("u")));
        assert_eq!(s.redeem("a", &t), None, "single use");

        let t = s
            .issue("a", Identity::user("u"), DEFAULT_TICKET_TTL)
            .unwrap();
        assert_eq!(s.redeem("b", &t), None, "other tenant");
        assert_eq!(s.redeem("a", &t), None, "and the misuse burned it");

        let t = s.issue("a", Identity::user("u"), Duration::ZERO).unwrap();
        assert_eq!(s.redeem("a", &t), None, "expired");
    }

    #[test]
    fn sweep_drops_only_expired_tickets() {
        let s = TicketStore::default();
        s.issue("a", Identity::user("old"), Duration::ZERO).unwrap();
        s.issue("a", Identity::user("live"), DEFAULT_TICKET_TTL)
            .unwrap();
        s.sweep(Instant::now() + Duration::from_millis(1));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn tickets_are_unpredictable() {
        let s = TicketStore::default();
        let a = s
            .issue("a", Identity::user("u"), DEFAULT_TICKET_TTL)
            .unwrap();
        let b = s
            .issue("a", Identity::user("u"), DEFAULT_TICKET_TTL)
            .unwrap();
        assert_ne!(a, b);
    }
}
