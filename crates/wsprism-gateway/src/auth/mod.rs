//! Authentication: who a connecting client is (ADR 0008 §1).
//!
//! A tenant configures an ordered chain of [`Authenticator`]s
//! (`tenants[].auth`). Each one either recognises the token and decides
//! (`Ok(identity)` / [`AuthError::Rejected`]) or passes it on
//! ([`AuthError::NotMine`]) — e.g. one-time tickets carry a prefix, JWTs
//! have three dot-separated parts. New mechanisms (JWT, backend proxy) are
//! new implementations; admission and sessions don't change.

mod dev;
mod jwt;
mod permissions;
mod proxy;
mod ticket;

use std::net::IpAddr;
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;

use crate::config::schema::AuthConfig;

type BuildResult<T> = wsprism_protocol::error::Result<T>;

pub use dev::DevAuthenticator;
pub use jwt::{expiry_deadline_ms, JwtAuthenticator};
pub use permissions::{Permissions, RoomPattern};
pub use proxy::ProxyAuthenticator;
pub use ticket::{
    IssueError, TicketAuthenticator, TicketStore, DEFAULT_TICKET_TTL, MAX_TICKET_TTL, TICKET_PREFIX,
};

/// What an authenticator gets to see of a connection attempt.
#[derive(Debug, Clone, Copy)]
pub struct AuthRequest<'a> {
    pub tenant: &'a str,
    pub token: &'a str,
    pub client_ip: IpAddr,
}

/// Who the client is, as decided by an authenticator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub user_id: String,
    /// Room permissions (default: unrestricted).
    pub permissions: Permissions,
    /// When the credential stops being valid (`None` = no expiry).
    pub expires_at: Option<SystemTime>,
}

impl Identity {
    pub fn user(user_id: impl Into<String>) -> Self {
        Self {
            user_id: user_id.into(),
            permissions: Permissions::default(),
            expires_at: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// Not this authenticator's token format: try the next one.
    NotMine,
    /// This authenticator's format, but invalid (bad signature, expired,
    /// unknown ticket...). The reason is for logs, never sent to clients.
    Rejected(&'static str),
    /// The authenticator could not decide right now (e.g. backend down).
    Unavailable(&'static str),
}

#[async_trait]
pub trait Authenticator: Send + Sync {
    /// Short name for logs and metrics (static, bounded set).
    fn kind(&self) -> &'static str;
    async fn authenticate(&self, req: &AuthRequest<'_>) -> Result<Identity, AuthError>;
}

/// A tenant's authenticators, tried in order.
pub struct AuthChain {
    links: Vec<Arc<dyn Authenticator>>,
}

impl AuthChain {
    pub fn new(links: Vec<Arc<dyn Authenticator>>) -> Self {
        Self { links }
    }

    /// Build a tenant's chain from config. An empty `auth` list means the
    /// development stub (see [`AuthChain::is_dev_only`]).
    pub fn from_config(
        auth: &[AuthConfig],
        tickets: &Arc<TicketStore>,
        backend: &crate::backend::Backend,
    ) -> BuildResult<Self> {
        if auth.is_empty() {
            return Ok(Self::new(vec![Arc::new(DevAuthenticator)]));
        }
        let links = auth
            .iter()
            .map(|a| -> BuildResult<Arc<dyn Authenticator>> {
                Ok(match a {
                    AuthConfig::Dev => Arc::new(DevAuthenticator),
                    AuthConfig::Ticket => Arc::new(TicketAuthenticator::new(Arc::clone(tickets))),
                    AuthConfig::Jwt(j) => Arc::new(JwtAuthenticator::from_config(j)?),
                    AuthConfig::Proxy(p) => Arc::new(ProxyAuthenticator::from_config(p, backend)?),
                })
            })
            .collect::<BuildResult<_>>()?;
        Ok(Self::new(links))
    }

    /// True if some link has this kind (e.g. "ticket").
    pub fn has_kind(&self, kind: &str) -> bool {
        self.links.iter().any(|l| l.kind() == kind)
    }

    /// True if only the development stub can admit anyone — worth a loud
    /// warning outside development.
    pub fn is_dev_only(&self) -> bool {
        self.links.iter().all(|l| l.kind() == "dev")
    }

    /// The first decision in the chain. If every link passes, the token is
    /// rejected.
    pub async fn authenticate(&self, req: &AuthRequest<'_>) -> Result<Identity, AuthError> {
        for link in &self.links {
            match link.authenticate(req).await {
                Err(AuthError::NotMine) => continue,
                decided => return decided,
            }
        }
        Err(AuthError::Rejected("no authenticator accepted the token"))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    struct Fixed(&'static str, Result<Identity, AuthError>);

    #[async_trait]
    impl Authenticator for Fixed {
        fn kind(&self) -> &'static str {
            self.0
        }
        async fn authenticate(&self, _: &AuthRequest<'_>) -> Result<Identity, AuthError> {
            self.1.clone()
        }
    }

    fn req() -> AuthRequest<'static> {
        AuthRequest {
            tenant: "t",
            token: "x",
            client_ip: IpAddr::from([127, 0, 0, 1]),
        }
    }

    #[tokio::test]
    async fn chain_skips_not_mine_and_stops_at_the_first_decision() {
        let chain = AuthChain::new(vec![
            Arc::new(Fixed("a", Err(AuthError::NotMine))),
            Arc::new(Fixed("b", Ok(Identity::user("u")))),
            Arc::new(Fixed("c", Err(AuthError::Rejected("never reached")))),
        ]);
        assert_eq!(chain.authenticate(&req()).await, Ok(Identity::user("u")));

        let rejecting = AuthChain::new(vec![
            Arc::new(Fixed("a", Err(AuthError::Rejected("bad")))),
            Arc::new(Fixed("b", Ok(Identity::user("u")))),
        ]);
        assert_eq!(
            rejecting.authenticate(&req()).await,
            Err(AuthError::Rejected("bad"))
        );
    }

    #[tokio::test]
    async fn a_token_nobody_recognises_is_rejected() {
        let chain = AuthChain::new(vec![Arc::new(Fixed("a", Err(AuthError::NotMine)))]);
        assert!(matches!(
            chain.authenticate(&req()).await,
            Err(AuthError::Rejected(_))
        ));
    }

    #[test]
    fn empty_config_is_the_dev_stub_and_says_so() {
        assert!(AuthChain::from_config(
            &[],
            &Arc::default(),
            &crate::backend::Backend::new().unwrap()
        )
        .unwrap()
        .is_dev_only());
    }
}
