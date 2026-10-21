//! Where a client's ticket is read from, per tenant `policy.ticket_sources`.
//!
//! Sources are tried in the configured order; the first one present wins.
//! The query string is off by default: URLs end up in proxy and access logs.

use axum::http::header::AUTHORIZATION;

use crate::config::schema::TicketSource;
use crate::transport::admission::HandshakeRequest;
use crate::transport::subprotocol::Offered;

/// The ticket presented through one of `sources`, if any.
pub fn find<'a>(
    sources: &[TicketSource],
    req: &HandshakeRequest<'a>,
    offered: &Offered<'a>,
) -> Option<&'a str> {
    sources.iter().find_map(|source| match source {
        TicketSource::Header => bearer(req),
        TicketSource::Subprotocol => offered.auth_token(),
        TicketSource::Query => req.query_ticket.filter(|t| !t.is_empty()),
    })
}

/// `Authorization: Bearer <token>`.
fn bearer<'a>(req: &HandshakeRequest<'a>) -> Option<&'a str> {
    let value = req.headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}
