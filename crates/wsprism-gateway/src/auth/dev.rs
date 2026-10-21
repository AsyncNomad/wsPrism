//! Development authenticator: `dev` -> user `dev`, `dev:<id>` or `dev.<id>`
//! -> user `<id>` (`dev.` because `:` is not allowed in a WebSocket
//! subprotocol, so browsers can only send that form)
//! (`<id>` in `[A-Za-z0-9_-]{1,64}`). Admits anyone who knows the
//! convention — for local development and benchmarks only.

use async_trait::async_trait;

use super::{AuthError, AuthRequest, Authenticator, Identity};

pub struct DevAuthenticator;

#[async_trait]
impl Authenticator for DevAuthenticator {
    fn kind(&self) -> &'static str {
        "dev"
    }

    async fn authenticate(&self, req: &AuthRequest<'_>) -> Result<Identity, AuthError> {
        match req.token.strip_prefix("dev") {
            Some("") => Ok(Identity::user("dev")),
            Some(rest) => match rest.strip_prefix(':').or_else(|| rest.strip_prefix('.')) {
                Some(id) if crate::ident::is_safe_id(id) => Ok(Identity::user(id)),
                Some(_) => Err(AuthError::Rejected("malformed dev ticket")),
                None => Err(AuthError::NotMine),
            },
            None => Err(AuthError::NotMine),
        }
    }
}
