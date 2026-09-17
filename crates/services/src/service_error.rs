//! Shared service-level error types.
//!
//! Replaces `Result<_, String>` with structured errors so callers can branch
//! on failure categories instead of parsing display strings.

use std::fmt;

/// Error returned by service-level operations.
///
/// Each variant carries a machine-readable `kind` tag and a human-readable
/// `message`. The `source` field preserves the underlying error when available
/// for diagnostic chaining.
#[derive(Debug)]
pub struct ServiceError {
    /// Machine-readable error category.
    pub kind: ServiceErrorKind,
    /// Human-readable description of what went wrong.
    pub message: String,
    /// Optional underlying error (boxed to avoid recursive size explosion).
    pub source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

/// Machine-readable categories for [`ServiceError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServiceErrorKind {
    /// Resource not found (task, contract, subtask).
    NotFound,
    /// Validation failed (invalid input, malformed state).
    Invalid,
    /// Persistence failure (DB, file I/O).
    Persistence,
    /// Network or HTTP failure.
    Network,
    /// Verification failed (criteria not met).
    Verification,
    // ── Conflict (non-retryable) ─────────────────────
    /// Permanent conflict (duplicate creation, immutable state transition).
    /// Maps to `InvalidRequest` — the caller must fix the approach, not retry.
    Conflict,

    // ── Conflict (transient, retryable) ──────────────
    /// Transient conflict (concurrent update, optimistic lock failure).
    /// Maps to `ServerError` — safe to retry with backoff.
    ConflictTransient,
    /// Internal / unexpected error.
    Internal,
}

impl ServiceErrorKind {
    /// Stable machine-readable tag for logs and external adapters.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::Invalid => "invalid",
            Self::Persistence => "persistence",
            Self::Network => "network",
            Self::Verification => "verification",
            Self::Conflict => "conflict",
            Self::ConflictTransient => "conflict_transient",
            Self::Internal => "internal",
        }
    }

    /// Map to the runtime [`ErrorKind`](astra_core::error_kind::ErrorKind) for
    /// unified error classification across the service boundary.
    ///
    /// Mappings preserve retryability semantics:
    /// - `Conflict` → `InvalidRequest`: permanent conflicts (duplicate creation,
    ///   illegal transitions) must not be blindly retried.
    /// - `ConflictTransient` → `ServerError`: transient conflicts (concurrent
    ///   update, optimistic lock failures) are safe to retry with backoff.
    /// - `Internal` → `ServerError`: internal errors may be transient (e.g. pool
    ///   exhaustion, OOM under load); retry with backoff before surfacing to user.
    /// - `Verification` → `InvalidRequest`: verification failures mean the task
    ///   output didn't meet criteria — the approach must be fixed, not retried as-is.
    /// - `NotFound` / `Invalid` / `Persistence` / `Network` map to the obvious
    ///   runtime counterparts.
    pub fn to_error_kind(self) -> astra_core::error_kind::ErrorKind {
        use astra_core::error_kind::ErrorKind;
        match self {
            Self::NotFound => ErrorKind::ToolNotFound,
            Self::Invalid => ErrorKind::ToolInvalidArgs,
            Self::Persistence => ErrorKind::DatabaseError,
            Self::Network => ErrorKind::Network,
            Self::Verification => ErrorKind::InvalidRequest,
            Self::Conflict => ErrorKind::InvalidRequest,
            Self::ConflictTransient => ErrorKind::ServerError,
            Self::Internal => ErrorKind::ServerError,
        }
    }
}

impl ServiceError {
    /// Construct a new error with the given kind and message.
    pub fn new(kind: ServiceErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    /// Construct a new error with an underlying source.
    pub fn with_source(
        kind: ServiceErrorKind,
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }

    /// Shorthand for `NotFound` errors.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ServiceErrorKind::NotFound, message)
    }

    /// Shorthand for `Invalid` errors (validation failures).
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ServiceErrorKind::Invalid, message)
    }

    /// Shorthand for `Persistence` errors.
    pub fn persistence(message: impl Into<String>) -> Self {
        Self::new(ServiceErrorKind::Persistence, message)
    }

    /// Shorthand for `Internal` errors.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ServiceErrorKind::Internal, message)
    }

    /// Shorthand for `Conflict` errors (permanent state machine violations).
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(ServiceErrorKind::Conflict, message)
    }

    /// Shorthand for `ConflictTransient` errors (concurrent update / retryable).
    pub fn conflict_transient(message: impl Into<String>) -> Self {
        Self::new(ServiceErrorKind::ConflictTransient, message)
    }
}

impl fmt::Display for ServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.kind.as_str(), self.message)?;
        if let Some(src) = &self.source {
            write!(f, ": {src}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ServiceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|s| s.as_ref() as &(dyn std::error::Error + 'static))
    }
}

/// Convenience alias used throughout the services crate.
pub type ServiceResult<T> = Result<T, ServiceError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_error_kind_has_stable_tags_without_http_semantics() {
        let cases = [
            (ServiceErrorKind::NotFound, "not_found"),
            (ServiceErrorKind::Invalid, "invalid"),
            (ServiceErrorKind::Persistence, "persistence"),
            (ServiceErrorKind::Network, "network"),
            (ServiceErrorKind::Verification, "verification"),
            (ServiceErrorKind::Conflict, "conflict"),
            (ServiceErrorKind::ConflictTransient, "conflict_transient"),
            (ServiceErrorKind::Internal, "internal"),
        ];

        for (kind, tag) in cases {
            assert_eq!(kind.as_str(), tag);
        }
    }

    #[test]
    fn conflict_is_permanent_not_retryable_via_error_kind() {
        // Permanent conflicts (duplicate creation, illegal state) map to
        // InvalidRequest — the caller must fix the approach.
        let ek = ServiceErrorKind::Conflict.to_error_kind();
        assert!(
            !ek.is_retryable(),
            "permanent Conflict must not be retryable"
        );
        assert_eq!(ek, astra_core::error_kind::ErrorKind::InvalidRequest);
    }

    #[test]
    fn conflict_transient_is_retryable_via_error_kind() {
        // Transient conflicts (concurrent update) map to ServerError — safe
        // to retry with backoff.
        let ek = ServiceErrorKind::ConflictTransient.to_error_kind();
        assert!(
            ek.is_retryable(),
            "transient ConflictTransient must be retryable"
        );
        assert_eq!(ek, astra_core::error_kind::ErrorKind::ServerError);
    }
}
