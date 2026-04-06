//! Error types for HTTP transport and SSE parsing.

use thiserror::Error;

/// Failures surfaced by the thin client.
#[derive(Debug, Error)]
pub enum ThinClientError {
    #[error("invalid base URL: {0}")]
    InvalidBaseUrl(String),
    #[error("invalid Authorization header value")]
    InvalidAuthHeader,
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    /// Successful transport but non-2xx status (caller may format like CLI `read_api_error`).
    #[error("HTTP {status}: {body}")]
    Api {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("SSE parse error: {0}")]
    SseParse(String),
    #[error("expected JSON object in SSE data line, got: {0}")]
    InvalidSseJson(serde_json::Value),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}
