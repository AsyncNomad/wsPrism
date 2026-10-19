//! Identifier rules for values that become parts of internal keys, metric
//! labels or log fields (tenant ids, user ids from every authenticator,
//! client session ids).
//!
//! Restricting them to `[A-Za-z0-9_-]{1,64}` keeps them free of the `::`
//! separator used in internal keys (see performance-architecture §2: keys
//! move to interned integers, after which this is a hygiene rule only).

/// Longest accepted identifier.
pub const MAX_ID_LEN: usize = 64;

/// True if `s` is 1..=64 characters of `[A-Za-z0-9_-]`.
pub fn is_safe_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_ID_LEN
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_the_safe_charset() {
        assert!(is_safe_id("Acme-1_b"));
        for bad in ["", "a::b", "a b", "a.b", "é", &"x".repeat(MAX_ID_LEN + 1)] {
            assert!(!is_safe_id(bad), "{bad:?}");
        }
        assert!(is_safe_id(&"x".repeat(MAX_ID_LEN)));
    }
}
