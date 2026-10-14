//! Browser `Origin` policy (cross-site WebSocket hijacking protection).
//!
//! Browsers attach cookies/credentials to WebSocket upgrades from any page,
//! so a malicious site could open a socket as the visiting user (CSWSH). The
//! `Origin` header, which browsers always send and pages cannot forge, is
//! checked against the tenant's `allowed_origins`. Requests without `Origin`
//! come from non-browser clients (apps, game engines, servers), for which
//! CSWSH does not apply, and are allowed.
//!
//! Patterns: exact origin `https://app.example.com[:port]`, subdomain
//! wildcard `https://*.example.com[:port]` (not the bare domain), or `*`.
//! Default (no patterns): every browser origin is denied.

use wsprism_core::error::{Result, WsPrismError};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Pattern {
    Any,
    /// `scheme://host[:port]`, lowercased.
    Exact(String),
    /// `scheme://` + `.suffix[:port]`, lowercased: matches any subdomain.
    Subdomains {
        scheme: String,
        suffix: String,
    },
}

/// Compiled `allowed_origins` of one tenant.
#[derive(Debug, Clone, Default)]
pub struct OriginPolicy {
    patterns: Vec<Pattern>,
}

impl OriginPolicy {
    pub fn compile(entries: &[String]) -> Result<Self> {
        let patterns = entries
            .iter()
            .map(|e| parse_pattern(e))
            .collect::<Result<_>>()?;
        Ok(Self { patterns })
    }

    /// True if no browser origin is allowed (the default).
    pub fn denies_all_browsers(&self) -> bool {
        self.patterns.is_empty()
    }

    /// `origin` is the request's `Origin` header, if any.
    pub fn allows(&self, origin: Option<&str>) -> bool {
        let Some(origin) = origin else {
            return true; // non-browser client
        };
        let origin = origin.trim().to_ascii_lowercase();
        self.patterns.iter().any(|p| match p {
            Pattern::Any => true,
            Pattern::Exact(o) => *o == origin,
            Pattern::Subdomains { scheme, suffix } => origin
                .strip_prefix(scheme.as_str())
                .and_then(|rest| rest.strip_suffix(suffix.as_str()))
                // At least one label, and only host characters before the suffix.
                .is_some_and(|label| !label.is_empty() && !label.contains(['/', ':', '@'])),
        })
    }
}

fn parse_pattern(raw: &str) -> Result<Pattern> {
    let invalid = || {
        WsPrismError::BadRequest(format!(
            "policy.allowed_origins: invalid origin pattern {raw:?} (expected scheme://host[:port], scheme://*.domain[:port] or *)"
        ))
    };
    let p = raw.trim().to_ascii_lowercase();
    if p == "*" {
        return Ok(Pattern::Any);
    }
    let (scheme, host_port) = p.split_once("://").ok_or_else(invalid)?;
    if scheme.is_empty() || host_port.is_empty() || host_port.contains(['/', '@', '?', '#']) {
        return Err(invalid());
    }
    match host_port.strip_prefix("*.") {
        Some(domain) if !domain.is_empty() && !domain.contains('*') => Ok(Pattern::Subdomains {
            scheme: format!("{scheme}://"),
            suffix: format!(".{domain}"),
        }),
        Some(_) => Err(invalid()),
        None if host_port.contains('*') => Err(invalid()),
        None => Ok(Pattern::Exact(p)),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn policy(entries: &[&str]) -> OriginPolicy {
        OriginPolicy::compile(&entries.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn default_denies_browsers_but_allows_non_browser_clients() {
        let p = policy(&[]);
        assert!(p.denies_all_browsers());
        assert!(!p.allows(Some("https://app.example.com")));
        assert!(p.allows(None));
    }

    #[test]
    fn exact_match_is_case_insensitive_and_port_sensitive() {
        let p = policy(&["https://App.Example.com"]);
        assert!(p.allows(Some("https://app.example.com")));
        assert!(!p.allows(Some("http://app.example.com")));
        assert!(!p.allows(Some("https://app.example.com:8443")));
        assert!(!p.allows(Some("https://app.example.com.evil.io")));
    }

    #[test]
    fn subdomain_wildcard_excludes_bare_domain_and_lookalikes() {
        let p = policy(&["https://*.example.com"]);
        assert!(p.allows(Some("https://a.example.com")));
        assert!(p.allows(Some("https://a.b.example.com")));
        assert!(!p.allows(Some("https://example.com")));
        assert!(!p.allows(Some("https://evilexample.com")));
        assert!(!p.allows(Some("https://a.example.com:444")));
        assert!(!p.allows(Some("http://a.example.com")));
    }

    #[test]
    fn star_allows_any_origin_including_null() {
        let p = policy(&["*"]);
        assert!(p.allows(Some("null")));
        assert!(p.allows(Some("https://whatever.io")));
    }

    #[test]
    fn invalid_patterns_fail_to_compile() {
        for bad in [
            "example.com",
            "https://",
            "https://a.com/path",
            "https://*",
            "https://a.*.com",
            "https://*.*.com",
        ] {
            assert!(
                OriginPolicy::compile(&[bad.to_string()]).is_err(),
                "{bad} should be rejected"
            );
        }
    }
}
