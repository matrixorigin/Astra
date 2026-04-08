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

impl ThinClientError {
    /// Returns true if this error is a transport-level failure (connection reset,
    /// timeout, DNS failure) that may succeed on retry.
    pub fn is_transport(&self) -> bool {
        match self {
            Self::Http(e) => e.is_connect() || e.is_timeout() || e.is_request(),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_base_url_display() {
        let err = ThinClientError::InvalidBaseUrl("bad://url".to_string());
        assert!(err.to_string().contains("bad://url"));
    }

    #[test]
    fn invalid_auth_header_display() {
        let err = ThinClientError::InvalidAuthHeader;
        assert!(err.to_string().contains("Authorization"));
    }

    #[test]
    fn api_error_display() {
        let err = ThinClientError::Api {
            status: reqwest::StatusCode::NOT_FOUND,
            body: "not found".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("404"));
        assert!(msg.contains("not found"));
    }

    #[test]
    fn sse_parse_display() {
        let err = ThinClientError::SseParse("bad frame".to_string());
        assert!(err.to_string().contains("bad frame"));
    }

    #[test]
    fn invalid_sse_json_display() {
        let err = ThinClientError::InvalidSseJson(serde_json::json!("not_object"));
        assert!(err.to_string().contains("not_object"));
    }

    #[test]
    fn is_transport_false_for_non_http_errors() {
        assert!(!ThinClientError::InvalidBaseUrl("x".into()).is_transport());
        assert!(!ThinClientError::InvalidAuthHeader.is_transport());
        assert!(!ThinClientError::SseParse("x".into()).is_transport());
    }
}
