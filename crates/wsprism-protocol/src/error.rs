//! Shared error types across wsPrism crates.
//!
//! These errors form the stable surface area that the gateway can turn into
//! client-facing responses (e.g., `sys.error` envelopes) while keeping internal
//! implementation details encapsulated.
//!
//! Design goal: All fallible operations in core should return `WsPrismError`
//! rather than panic. Combined with crate-level lint denies for panic/unwrap,
//! this keeps production code resilient to malformed or hostile input.

use thiserror::Error;

/// Stable client-facing error codes.
///
/// These codes are used in outward-facing responses and logging to decouple
/// client behavior from internal error variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientCode {
    /// Invalid input / malformed message.
    BadRequest,
    /// Auth failed.
    AuthFailed,
    /// Rate limited.
    RateLimited,
    /// Payload too large.
    PayloadTooLarge,
    /// Not allowed by policy.
    NotAllowed,
    /// Resource exhausted
    ResourceExhausted,
    /// Unsupported protocol version.
    UnsupportedVersion,
    /// Per-user session limit reached (`on_exceed: deny`).
    TooManySessions,
    /// The client could not keep up with its outbound queue.
    SlowConsumer,
    /// No activity within the idle timeout.
    Timeout,
    /// A backend the gateway depends on did not answer in time (RPC proxy).
    Unavailable,
    /// Internal server error.
    Internal,
}

impl ClientCode {
    /// String representation used in JSON responses.
    pub fn as_str(self) -> &'static str {
        match self {
            ClientCode::BadRequest => "BAD_REQUEST",
            ClientCode::AuthFailed => "AUTH_FAILED",
            ClientCode::RateLimited => "RATE_LIMITED",
            ClientCode::PayloadTooLarge => "PAYLOAD_TOO_LARGE",
            ClientCode::NotAllowed => "NOT_ALLOWED",
            ClientCode::ResourceExhausted => "RESOURCE_EXHAUSTED",
            ClientCode::UnsupportedVersion => "UNSUPPORTED_VERSION",
            ClientCode::TooManySessions => "TOO_MANY_SESSIONS",
            ClientCode::SlowConsumer => "SLOW_CONSUMER",
            ClientCode::Timeout => "TIMEOUT",
            ClientCode::Unavailable => "UNAVAILABLE",
            ClientCode::Internal => "INTERNAL",
        }
    }
}

/// Convenient result alias for core operations.
pub type Result<T> = std::result::Result<T, WsPrismError>;

/// Unified error type used by core and gateway layers.
#[derive(Debug, Error)]
pub enum WsPrismError {
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("auth failed")]
    AuthFailed,
    #[error("rate limited")]
    RateLimited,
    #[error("payload too large")]
    PayloadTooLarge,
    #[error("not allowed: {0}")]
    NotAllowed(String),
    #[error("resource exhausted: {0}")]
    ResourceExhausted(String),
    #[error("unsupported protocol version")]
    UnsupportedVersion,
    #[error("internal: {0}")]
    Internal(String),
}

impl WsPrismError {
    /// Message safe to show to clients: validation details are kept, but
    /// internal errors never expose their cause, and the `Display` prefixes
    /// meant for logs (`bad request: ...`) are dropped.
    pub fn client_message(&self) -> &str {
        match self {
            WsPrismError::BadRequest(m)
            | WsPrismError::NotAllowed(m)
            | WsPrismError::ResourceExhausted(m) => m,
            WsPrismError::AuthFailed => "authentication failed",
            WsPrismError::RateLimited => "rate limited",
            WsPrismError::PayloadTooLarge => "payload too large",
            WsPrismError::UnsupportedVersion => "unsupported protocol version",
            WsPrismError::Internal(_) => "internal error",
        }
    }

    /// Map internal error to a stable client-facing code.
    pub fn client_code(&self) -> ClientCode {
        match self {
            WsPrismError::BadRequest(_) => ClientCode::BadRequest,
            WsPrismError::AuthFailed => ClientCode::AuthFailed,
            WsPrismError::RateLimited => ClientCode::RateLimited,
            WsPrismError::PayloadTooLarge => ClientCode::PayloadTooLarge,
            WsPrismError::NotAllowed(_) => ClientCode::NotAllowed,
            WsPrismError::ResourceExhausted(_) => ClientCode::ResourceExhausted,
            WsPrismError::UnsupportedVersion => ClientCode::UnsupportedVersion,
            WsPrismError::Internal(_) => ClientCode::Internal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_errors_never_expose_their_cause() {
        let e = WsPrismError::Internal("db password=hunter2 at 10.0.0.5".into());
        assert_eq!(e.client_message(), "internal error");
        assert_eq!(e.client_code(), ClientCode::Internal);
    }

    #[test]
    fn client_messages_drop_the_log_prefix() {
        let e = WsPrismError::BadRequest("chat.send requires room".into());
        assert_eq!(e.to_string(), "bad request: chat.send requires room");
        assert_eq!(e.client_message(), "chat.send requires room");
    }
}
