//! `Sec-WebSocket-Protocol` handling: protocol version negotiation and the
//! browser-friendly ticket carrier.
//!
//! Browsers cannot set headers on a WebSocket upgrade, so a client may pass
//! its ticket as an offered subprotocol `wsprism.auth.<token>` alongside a
//! protocol version, e.g. `Sec-WebSocket-Protocol: wsprism.v1, wsprism.auth.abc`.
//! The server answers with the version only; the auth entry is never echoed.
//! Tokens must be valid HTTP token characters (base64url without padding and
//! JWTs qualify).

use axum::http::{header::SEC_WEBSOCKET_PROTOCOL, HeaderMap};

/// Protocol version 1.
pub const V1: &str = "wsprism.v1";
/// Protocol v2 with JSON control messages (ADR 0006). `wsprism.v2.bin` is reserved.
pub const V2_JSON: &str = "wsprism.v2.json";
/// Versions this gateway speaks, most preferred first.
const SUPPORTED: &[&str] = &[V2_JSON, V1];

pub use wsprism_protocol::protocol::ProtocolVersion;

/// The session's protocol from the negotiated subprotocol (`None` = no
/// header = v1).
pub fn version_of(selected: Option<&str>) -> ProtocolVersion {
    match selected {
        Some(V2_JSON) => ProtocolVersion::V2,
        _ => ProtocolVersion::V1,
    }
}
/// Prefix of the subprotocol entry carrying a ticket.
const AUTH_PREFIX: &str = "wsprism.auth.";

/// The subprotocols a client offered.
pub struct Offered<'a> {
    entries: Vec<&'a str>,
}

/// Outcome of version negotiation.
#[derive(Debug, PartialEq, Eq)]
pub enum Negotiated {
    /// No subprotocol offered: legacy client, v1 implied, nothing to echo.
    Implicit,
    /// Select and echo this version.
    Selected(&'static str),
    /// Versions were offered but none is supported.
    Unsupported,
}

impl<'a> Offered<'a> {
    pub fn parse(headers: &'a HeaderMap) -> Self {
        let entries = headers
            .get_all(SEC_WEBSOCKET_PROTOCOL)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .collect();
        Self { entries }
    }

    /// Pick the protocol version. The auth entry alone does not count as an
    /// offer: browsers require the server to echo one offered entry, and the
    /// auth entry must never be echoed.
    pub fn negotiate(&self) -> Negotiated {
        if self.entries.is_empty() {
            return Negotiated::Implicit;
        }
        SUPPORTED
            .iter()
            .find(|v| self.entries.contains(v))
            .map_or(Negotiated::Unsupported, |v| Negotiated::Selected(v))
    }

    /// Ticket carried as `wsprism.auth.<token>`, if any.
    pub fn auth_token(&self) -> Option<&'a str> {
        self.entries
            .iter()
            .find_map(|e| e.strip_prefix(AUTH_PREFIX))
            .filter(|t| !t.is_empty())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn offered(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(SEC_WEBSOCKET_PROTOCOL, value.parse().unwrap());
        h
    }

    #[test]
    fn no_header_is_implicit_v1() {
        let h = HeaderMap::new();
        let o = Offered::parse(&h);
        assert_eq!(o.negotiate(), Negotiated::Implicit);
        assert_eq!(o.auth_token(), None);
    }

    #[test]
    fn selects_v1_and_extracts_token() {
        let h = offered("chat, wsprism.v1, wsprism.auth.eyJh.bGc.sig");
        let o = Offered::parse(&h);
        assert_eq!(o.negotiate(), Negotiated::Selected(V1));
        assert_eq!(o.auth_token(), Some("eyJh.bGc.sig"));
    }

    #[test]
    fn auth_entry_alone_is_not_a_supported_offer() {
        let h = offered("wsprism.auth.abc");
        assert_eq!(Offered::parse(&h).negotiate(), Negotiated::Unsupported);
    }

    #[test]
    fn unknown_versions_are_unsupported() {
        let h = offered("wsprism.v9");
        assert_eq!(Offered::parse(&h).negotiate(), Negotiated::Unsupported);
    }
}
