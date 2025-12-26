//! Shared error type across wsPrism crates.

use thiserror::Error;

/// Client-facing error codes (stable API).
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
    /// Unsupported protocol version.
    UnsupportedVersion,
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
            ClientCode::UnsupportedVersion => "UNSUPPORTED_VERSION",
            ClientCode::Internal => "INTERNAL",
        }
    }
}

/// Shared result type.
pub type Result<T> = std::result::Result<T, WsPrismError>;

/// Unified error type used by core and gateway.
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
    #[error("unsupported protocol version")]
    UnsupportedVersion,
    #[error("internal: {0}")]
    Internal(String),
}

impl WsPrismError {
    /// Map internal error to a stable client-facing code.
    pub fn client_code(&self) -> ClientCode {
        match self {
            WsPrismError::BadRequest(_) => ClientCode::BadRequest,
            WsPrismError::AuthFailed => ClientCode::AuthFailed,
            WsPrismError::RateLimited => ClientCode::RateLimited,
            WsPrismError::PayloadTooLarge => ClientCode::PayloadTooLarge,
            WsPrismError::NotAllowed(_) => ClientCode::NotAllowed,
            WsPrismError::UnsupportedVersion => ClientCode::UnsupportedVersion,
            WsPrismError::Internal(_) => ClientCode::Internal,
        }
    }
}
