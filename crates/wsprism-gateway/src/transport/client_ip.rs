//! Client IP resolution behind reverse proxies / load balancers.
//!
//! Behind a load balancer every TCP peer is the balancer, so per-IP limits
//! would be shared by all clients. If (and only if) the peer is a configured
//! trusted proxy, the client address is taken from `X-Forwarded-For` using
//! the "rightmost untrusted" rule: walk the list from the right, skip
//! trusted proxies, and stop at the first address that is not trusted.
//! Entries left of that point are client-controlled and ignored, so a client
//! cannot spoof its address by prepending fake entries.

use std::net::IpAddr;
use std::str::FromStr;

use axum::http::HeaderMap;
use ipnet::IpNet;
use wsprism_core::error::{Result, WsPrismError};

const X_FORWARDED_FOR: &str = "x-forwarded-for";

/// Compiled `gateway.trusted_proxies`.
#[derive(Debug, Clone, Default)]
pub struct TrustedProxies {
    nets: Vec<IpNet>,
}

impl TrustedProxies {
    /// Parse entries like `10.0.0.0/8`, `::1`, `192.168.1.10`.
    pub fn parse(entries: &[String]) -> Result<Self> {
        let nets = entries
            .iter()
            .map(|e| {
                IpNet::from_str(e)
                    .or_else(|_| IpAddr::from_str(e).map(IpNet::from))
                    .map_err(|_| {
                        WsPrismError::BadRequest(format!(
                            "gateway.trusted_proxies: invalid IP or CIDR: {e}"
                        ))
                    })
            })
            .collect::<Result<_>>()?;
        Ok(Self { nets })
    }

    fn trusts(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        self.nets.iter().any(|n| n.contains(&ip))
    }

    /// The client's address for a connection from `peer`.
    pub fn client_ip(&self, peer: IpAddr, headers: &HeaderMap) -> IpAddr {
        let mut client = canonical(peer);
        if !self.trusts(client) {
            return client;
        }
        // All XFF headers in order, as one comma-separated list.
        let hops: Vec<&str> = headers
            .get_all(X_FORWARDED_FOR)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .collect();
        for hop in hops.iter().rev() {
            if !self.trusts(client) {
                break;
            }
            match IpAddr::from_str(hop) {
                Ok(ip) => client = canonical(ip),
                // Malformed entry: nothing to its left can be trusted either.
                Err(_) => break,
            }
        }
        client
    }
}

/// IPv4-mapped IPv6 (`::ffff:a.b.c.d`) → IPv4, so one rule covers both forms.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn proxies(list: &[&str]) -> TrustedProxies {
        TrustedProxies::parse(&list.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    fn xff(values: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in values {
            h.append(X_FORWARDED_FOR, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn untrusted_peer_is_the_client_and_xff_is_ignored() {
        let p = proxies(&["10.0.0.0/8"]);
        let got = p.client_ip(ip("203.0.113.9"), &xff(&["1.2.3.4"]));
        assert_eq!(got, ip("203.0.113.9"));
    }

    #[test]
    fn trusted_peer_uses_rightmost_untrusted_hop() {
        let p = proxies(&["10.0.0.0/8"]);
        // client -> 10.0.0.2 (inner proxy) -> 10.0.0.1 (peer)
        let got = p.client_ip(ip("10.0.0.1"), &xff(&["198.51.100.7, 10.0.0.2"]));
        assert_eq!(got, ip("198.51.100.7"));
    }

    #[test]
    fn spoofed_left_entries_are_ignored() {
        let p = proxies(&["10.0.0.1"]);
        // The client sent "XFF: 6.6.6.6"; the proxy appended the real address.
        let got = p.client_ip(ip("10.0.0.1"), &xff(&["6.6.6.6, 198.51.100.7"]));
        assert_eq!(got, ip("198.51.100.7"));
    }

    #[test]
    fn multiple_headers_are_one_list() {
        let p = proxies(&["10.0.0.1"]);
        let got = p.client_ip(ip("10.0.0.1"), &xff(&["6.6.6.6", "198.51.100.7"]));
        assert_eq!(got, ip("198.51.100.7"));
    }

    #[test]
    fn malformed_hop_stops_the_walk() {
        let p = proxies(&["10.0.0.0/8"]);
        let got = p.client_ip(ip("10.0.0.1"), &xff(&["198.51.100.7, garbage"]));
        assert_eq!(got, ip("10.0.0.1"));
    }

    #[test]
    fn trusted_peer_without_xff_is_the_client() {
        let p = proxies(&["127.0.0.1"]);
        assert_eq!(
            p.client_ip(ip("127.0.0.1"), &HeaderMap::new()),
            ip("127.0.0.1")
        );
    }

    #[test]
    fn ipv4_mapped_ipv6_matches_ipv4_rules() {
        let p = proxies(&["127.0.0.1"]);
        let got = p.client_ip(ip("::ffff:127.0.0.1"), &xff(&["198.51.100.7"]));
        assert_eq!(got, ip("198.51.100.7"));
    }

    #[test]
    fn invalid_entries_are_rejected_at_load() {
        assert!(TrustedProxies::parse(&["not-an-ip".into()]).is_err());
        assert!(TrustedProxies::parse(&["10.0.0.0/33".into()]).is_err());
    }
}
