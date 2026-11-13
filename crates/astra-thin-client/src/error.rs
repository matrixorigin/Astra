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
    /// Retry advice from rate limiting or a server error.
    #[error("HTTP {status}: {body}")]
    SyncOutboxBackpressure {
        status: reqwest::StatusCode,
        body: String,
        retry_after_seconds: Option<u64>,
    },
    #[error("SSE parse error: {0}")]
    SseParse(String),
    #[error(
        "incompatible Astra Server interaction protocol (client requires {expected}, server returned {actual}); restart or upgrade the Server before starting another turn"
    )]
    IncompatibleRuntime { expected: String, actual: String },
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("invalid provider interaction response: {0}")]
    InvalidProviderInteractionResponse(String),
    #[error("HTTP response exceeded the {limit} byte capture limit")]
    ResponseTooLarge { limit: usize },
    #[error("Server admission deadline expired before request dispatch")]
    AdmissionDeadlineExpired,
    #[error("tool-result callback delivery deadline expired")]
    CallbackDeadlineExpired,
    #[error(
        "credential refresh did not settle before the callback stopped waiting and may still complete; original callback: {0}"
    )]
    CallbackAuthRefreshUnconfirmed(#[source] Box<ThinClientError>),
    #[error(
        "tool-result retry could not obtain credentials: {auth}; previous callback: {previous}"
    )]
    CallbackRetryAuthFailed {
        previous: Box<ThinClientError>,
        #[source]
        auth: Box<ThinClientError>,
    },
    #[error("the original authenticated owner could not be confirmed before tool-result delivery")]
    CallbackOwnerUnconfirmed,
    #[error(
        "the original authenticated owner could not be confirmed for further delivery; previous callback: {0}"
    )]
    CallbackOwnerUnconfirmedAfterAttempt(#[source] Box<ThinClientError>),
    #[error("tool-result delivery task failed: {0}")]
    CallbackTaskFailed(String),
    #[error("server acknowledged the tool result, but local bookkeeping failed: {0}")]
    CallbackBookkeepingFailed(String),
    #[error("tool-result retry was cancelled before delivery")]
    CallbackRetryCancelled,
    #[error(
        "session {session_id} cancellation has not been confirmed complete: {reason}. Retry `astra session cancel {session_id}`; keep the session history"
    )]
    SessionCancellationPending { session_id: String, reason: String },
    #[error("invalid session cancellation response: {0}")]
    InvalidSessionCancellationResponse(String),
    #[error("expected JSON object in SSE data line, got: {0}")]
    InvalidSseJson(serde_json::Value),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

impl ThinClientError {
    /// Keep a previous delivery failure when the original owner can no longer be confirmed.
    pub fn callback_owner_unconfirmed(previous: Option<Self>) -> Self {
        match previous {
            Some(previous) => Self::CallbackOwnerUnconfirmedAfterAttempt(Box::new(previous)),
            None => Self::CallbackOwnerUnconfirmed,
        }
    }

    /// A read-only cancellation observation found an expired execution owner.
    /// Recovery must use the existing mutation command, never a hidden GET write.
    pub fn cancellation_recovery_required(&self) -> bool {
        matches!(self, Self::Api { status, body }
            if *status == reqwest::StatusCode::CONFLICT
                && serde_json::from_str::<serde_json::Value>(body).ok()
                    .and_then(|value| value.get("error_code").and_then(serde_json::Value::as_str).map(str::to_owned))
                    .as_deref() == Some("cancellation_recovery_required"))
    }

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
    fn admission_deadline_expiry_is_not_a_retriable_transport_error() {
        assert!(!ThinClientError::AdmissionDeadlineExpired.is_transport());
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
