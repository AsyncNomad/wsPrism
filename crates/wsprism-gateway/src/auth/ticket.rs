//! One-time tickets (ADR 0008 §4): a backend asks the server API for a
//! short-lived ticket for one user; the client presents it once.
//!
//! Tickets exist so long-lived credentials never travel in URLs (browsers
//! and WebTransport can only pass a credential in the URL or subprotocol).
//! Stored in process memory, or — with a cluster broker that shares storage
//! (Redis) — in the broker, so a ticket issued by one node is redeemed on
//! any node, still exactly once (ADR 0011 §6). The shared store keys a
//! ticket by its SHA-256: reading Redis does not reveal usable tickets.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use dashmap::DashMap;
use ring::rand::{SecureRandom, SystemRandom};

use wsprism_engine::broker::Broker;
use wsprism_engine::Permissions;

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
    /// The shared store (cluster broker) cannot be reached.
    Unavailable,
}

/// The shared ticket store (cluster broker) cannot be reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreUnavailable;

impl std::fmt::Display for StoreUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ticket store unavailable")
    }
}

impl std::error::Error for StoreUnavailable {}

pub struct TicketStore {
    tickets: DashMap<String, Entry>,
    rng: SystemRandom,
    /// A broker with shared storage: tickets live there instead.
    shared: Option<Arc<dyn Broker>>,
}

impl Default for TicketStore {
    fn default() -> Self {
        Self {
            tickets: DashMap::new(),
            rng: SystemRandom::new(),
            shared: None,
        }
    }
}

/// The shared store's key for `token`.
fn shared_key(token: &str) -> String {
    let d = ring::digest::digest(&ring::digest::SHA256, token.as_bytes());
    let hex: String = d.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    format!("tk:{hex}")
}

impl TicketStore {
    /// Tickets in `broker`'s shared storage if it has one, else in memory.
    pub fn for_broker(broker: Option<Arc<dyn Broker>>) -> Self {
        Self {
            shared: broker.filter(|b| b.kv().is_some()),
            ..Self::default()
        }
    }

    fn new_token(&self) -> Result<String, IssueError> {
        let mut raw = [0u8; 32];
        self.rng.fill(&mut raw).map_err(|_| IssueError::Random)?;
        Ok(format!(
            "{TICKET_PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
        ))
    }

    /// A new ticket for `identity` in `tenant`, valid for `ttl`.
    pub async fn issue(
        &self,
        tenant: &str,
        identity: Identity,
        ttl: Duration,
    ) -> Result<String, IssueError> {
        let Some(kv) = self.shared.as_ref().and_then(|b| b.kv()) else {
            return self.issue_local(tenant, identity, ttl);
        };
        let token = self.new_token()?;
        let (subscribe, publish) = identity.permissions.to_lists();
        let value = serde_json::json!({
            "tenant": tenant, "user": identity.user_id, "subscribe": subscribe, "publish": publish,
        });
        match kv
            .put_once(&shared_key(&token), value.to_string().into(), ttl)
            .await
        {
            Ok(true) => Ok(token),
            Ok(false) => Err(IssueError::Random), // a 256-bit collision
            Err(e) => {
                tracing::warn!("ticket store: {e}");
                Err(IssueError::Unavailable)
            }
        }
    }

    /// Consume `token`: the identity if it is live and issued for `tenant`.
    /// The ticket is gone afterwards either way (a misuse burns it).
    /// `Err` = the shared store cannot be reached (fail closed).
    pub async fn redeem(
        &self,
        tenant: &str,
        token: &str,
    ) -> Result<Option<Identity>, StoreUnavailable> {
        let Some(kv) = self.shared.as_ref().and_then(|b| b.kv()) else {
            return Ok(self.redeem_local(tenant, token));
        };
        let value = kv.take(&shared_key(token)).await.map_err(|e| {
            tracing::warn!("ticket store: {e}");
            StoreUnavailable
        })?;
        let Some(value) = value else { return Ok(None) };
        #[derive(serde::Deserialize)]
        struct Stored {
            tenant: String,
            user: String,
            subscribe: Option<Vec<String>>,
            publish: Option<Vec<String>>,
        }
        let Ok(s) = serde_json::from_slice::<Stored>(&value) else {
            return Ok(None);
        };
        Ok((s.tenant == tenant).then(|| Identity {
            user_id: s.user,
            permissions: Permissions::from_lists(s.subscribe, s.publish),
            expires_at: None,
        }))
    }

    /// [`TicketStore::issue`] in this process's memory.
    fn issue_local(
        &self,
        tenant: &str,
        identity: Identity,
        ttl: Duration,
    ) -> Result<String, IssueError> {
        if self.tickets.len() >= MAX_TICKETS {
            return Err(IssueError::Full);
        }
        let token = self.new_token()?;
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

    /// [`TicketStore::redeem`] in this process's memory.
    fn redeem_local(&self, tenant: &str, token: &str) -> Option<Identity> {
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
        match self.store.redeem(req.tenant, req.token).await {
            Ok(Some(identity)) => Ok(identity),
            Ok(None) => Err(AuthError::Rejected(
                "unknown, used, expired or foreign ticket",
            )),
            // Fail closed: 503 + Retry-After (ADR 0011 §9).
            Err(StoreUnavailable) => Err(AuthError::Unavailable("ticket store unreachable")),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tickets_are_single_use_tenant_bound_and_expire() {
        let s = TicketStore::default();
        let t = s
            .issue("a", Identity::user("u"), DEFAULT_TICKET_TTL)
            .await
            .unwrap();
        assert!(t.starts_with(TICKET_PREFIX) && t.len() > 40);
        assert_eq!(s.redeem("a", &t).await.unwrap(), Some(Identity::user("u")));
        assert_eq!(s.redeem("a", &t).await.unwrap(), None, "single use");

        let t = s
            .issue("a", Identity::user("u"), DEFAULT_TICKET_TTL)
            .await
            .unwrap();
        assert_eq!(s.redeem("b", &t).await.unwrap(), None, "other tenant");
        assert_eq!(
            s.redeem("a", &t).await.unwrap(),
            None,
            "and the misuse burned it"
        );

        let t = s
            .issue("a", Identity::user("u"), Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(s.redeem("a", &t).await.unwrap(), None, "expired");
    }

    #[tokio::test]
    async fn sweep_drops_only_expired_tickets() {
        let s = TicketStore::default();
        s.issue("a", Identity::user("old"), Duration::ZERO)
            .await
            .unwrap();
        s.issue("a", Identity::user("live"), DEFAULT_TICKET_TTL)
            .await
            .unwrap();
        s.sweep(Instant::now() + Duration::from_millis(1));
        assert_eq!(s.len(), 1);
    }

    #[tokio::test]
    async fn tickets_are_unpredictable() {
        let s = TicketStore::default();
        let a = s
            .issue("a", Identity::user("u"), DEFAULT_TICKET_TTL)
            .await
            .unwrap();
        let b = s
            .issue("a", Identity::user("u"), DEFAULT_TICKET_TTL)
            .await
            .unwrap();
        assert_ne!(a, b);
    }
}
