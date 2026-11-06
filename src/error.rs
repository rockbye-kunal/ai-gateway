use std::fmt;

use axum::{
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

/// An error returned to API clients in the OpenAI error format:
/// `{"error": {"message": ..., "type": ..., "code": ...}}`.
///
/// Messages are written for clients. Internal details (upstream URLs, raw
/// transport errors, provider credentials) are logged, never put here.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub kind: &'static str,
    pub code: &'static str,
    pub message: String,
    pub retry_after: Option<u64>,
}

impl ApiError {
    pub fn new(
        status: StatusCode,
        kind: &'static str,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self { status, kind, code, message: message.into(), retry_after: None }
    }

    pub fn with_retry_after(mut self, secs: u64) -> Self {
        self.retry_after = Some(secs.max(1));
        self
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request_error", "invalid_request", message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "authentication_error", "invalid_api_key", message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "permission_error", "model_not_allowed", message)
    }

    pub fn model_not_found(model: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "model_not_found",
            format!("model '{model}' is not configured on this gateway"),
        )
    }

    pub fn rate_limited(retry_after: u64) -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            "rate_limit_exceeded",
            "request rate limit exceeded for this API key",
        )
        .with_retry_after(retry_after)
    }

    pub fn budget_exceeded(retry_after: u64) -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            "budget_exceeded",
            "daily token budget exhausted for this API key",
        )
        .with_retry_after(retry_after)
    }

    pub fn overloaded() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "server_error",
            "gateway_overloaded",
            "gateway is at capacity, retry shortly",
        )
        .with_retry_after(1)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "server_error", "service_unavailable", message)
    }

    pub fn upstream(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, "server_error", "upstream_error", message)
    }

    pub fn internal_config(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "server_error", "gateway_misconfigured", message)
    }

    pub fn upstream_timeout() -> Self {
        Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            "server_error",
            "upstream_timeout",
            "upstream provider timed out",
        )
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({}): {}", self.status, self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({
            "error": { "message": self.message, "type": self.kind, "code": self.code }
        });
        let mut resp = (self.status, Json(body)).into_response();
        if let Some(secs) = self.retry_after {
            if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
                resp.headers_mut().insert(header::RETRY_AFTER, v);
            }
        }
        resp
    }
}
