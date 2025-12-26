use thiserror::Error;

pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("auth failed")]
    AuthFailed,

    #[error("websocket error: {0}")]
    WebSocket(String),

    #[error("timeout")]
    Timeout,

    #[error("internal error: {0}")]
    Internal(String),
}

impl AppError {
    pub fn client_code(&self) -> &'static str {
        match self {
            AppError::BadRequest(_) => "BAD_REQUEST",
            AppError::AuthFailed => "AUTH_FAILED",
            AppError::WebSocket(_) => "WEBSOCKET",
            AppError::Timeout => "TIMEOUT",
            AppError::Internal(_) => "INTERNAL",
        }
    }
}

// HTTP polish (non-WS handlers).
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match self {
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::AuthFailed => StatusCode::UNAUTHORIZED,
            AppError::Timeout => StatusCode::REQUEST_TIMEOUT,
            AppError::WebSocket(_) | AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = Json(json!({
            "error": self.client_code(),
            "message": self.to_string(),
        }));
        (status, body).into_response()
    }
}
