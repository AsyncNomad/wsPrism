//! How session logs show identities (`gateway.log_identities`): user ids,
//! client session ids and IP addresses are personal data under GDPR and
//! similar rules, so operators can hash them or leave them out.

use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use wsprism_protocol::error::{Result, WsPrismError};

use crate::config::schema::LogIdentitiesMode;

/// Shortest HMAC key accepted from `log_identity_key_env`.
const MIN_KEY_BYTES: usize = 16;
/// Hex characters of the HMAC kept in logs (64 bits: enough to tell people
/// apart in logs, not an index to look them up by).
const SHOWN_HEX: usize = 16;

/// See the module docs.
pub enum LogIdentities {
    Plain,
    Hashed(hmac::Key),
    Off,
}

impl LogIdentities {
    /// `key_env` names the variable holding the HMAC key (`hashed` only):
    /// the same key on every node and across restarts keeps the hashes of
    /// one person equal. Without it, a random key per process.
    pub fn new(mode: LogIdentitiesMode, key_env: Option<&str>) -> Result<Self> {
        let bad = |m: String| WsPrismError::BadRequest(m);
        match (mode, key_env) {
            (LogIdentitiesMode::Plain, None) => Ok(Self::Plain),
            (LogIdentitiesMode::Off, None) => Ok(Self::Off),
            (LogIdentitiesMode::Plain | LogIdentitiesMode::Off, Some(_)) => Err(bad(
                "gateway.log_identity_key_env needs log_identities: hashed".into(),
            )),
            (LogIdentitiesMode::Hashed, Some(var)) => {
                let key = std::env::var(var)
                    .map_err(|_| bad(format!("gateway.log_identity_key_env {var} is not set")))?;
                if key.len() < MIN_KEY_BYTES {
                    return Err(bad(format!(
                        "gateway.log_identity_key_env {var}: at least {MIN_KEY_BYTES} bytes"
                    )));
                }
                Ok(Self::Hashed(hmac::Key::new(
                    hmac::HMAC_SHA256,
                    key.as_bytes(),
                )))
            }
            (LogIdentitiesMode::Hashed, None) => {
                let mut key = [0u8; 32];
                SystemRandom::new().fill(&mut key).map_err(|_| {
                    WsPrismError::Internal("log identity key: system random failed".into())
                })?;
                Ok(Self::Hashed(hmac::Key::new(hmac::HMAC_SHA256, &key)))
            }
        }
    }

    /// The value to log, or `None` to leave the field out.
    pub fn show(&self, value: &str) -> Option<String> {
        match self {
            Self::Plain => Some(value.to_string()),
            Self::Off => None,
            Self::Hashed(key) => {
                let tag = hmac::sign(key, value.as_bytes());
                let hex: String = tag.as_ref().iter().map(|b| format!("{b:02x}")).collect();
                Some(format!("h:{}", &hex[..SHOWN_HEX]))
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn plain_hashed_and_off() {
        assert_eq!(
            LogIdentities::new(LogIdentitiesMode::Plain, None)
                .unwrap()
                .show("alice")
                .as_deref(),
            Some("alice")
        );
        assert_eq!(
            LogIdentities::new(LogIdentitiesMode::Off, None)
                .unwrap()
                .show("alice"),
            None
        );
        let h = LogIdentities::new(LogIdentitiesMode::Hashed, None).unwrap();
        let a = h.show("alice").unwrap();
        assert!(a.starts_with("h:") && a.len() == 2 + SHOWN_HEX, "{a}");
        assert!(!a.contains("alice"));
        assert_eq!(h.show("alice"), Some(a.clone()), "stable within a key");
        assert_ne!(h.show("bob"), Some(a.clone()));
        // A random key per process: another instance hashes differently.
        let other = LogIdentities::new(LogIdentitiesMode::Hashed, None).unwrap();
        assert_ne!(other.show("alice"), Some(a));
    }

    #[test]
    fn a_key_is_only_for_hashed_and_must_be_set() {
        assert!(LogIdentities::new(LogIdentitiesMode::Plain, Some("X")).is_err());
        assert!(LogIdentities::new(
            LogIdentitiesMode::Hashed,
            Some("WSPRISM_TEST_SURELY_UNSET_VAR")
        )
        .is_err());
    }
}
