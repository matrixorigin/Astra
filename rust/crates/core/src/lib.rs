use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    Json,
    http::{HeaderMap, StatusCode},
};
use serde::{Deserialize, Serialize};
use sqlx::{MySql, Pool, mysql::MySqlPoolOptions};

pub mod composite_snapshot;
pub mod confidence;
pub mod config;
pub mod drift;
pub mod error_kind;
pub mod log;
pub mod runtime_limits;

/// Re-export for [`crate::agent_*!`] macros (call sites do not need a direct `tracing` dependency).
#[doc(hidden)]
pub use tracing;
pub mod session_env_overlay;
pub mod sync_poison;
pub use confidence::ConfidenceInterval;
pub use config::*;
pub use drift::{DriftCause, DriftEvidence, EvidenceType};
pub use error_kind::{ClassifiedError, ErrorKind, classify_tool_output};
pub use runtime_limits::{MAX_TOOL_ROUNDS_DEFAULT, RuntimeLimits};
#[cfg(any(test, feature = "dev-defaults"))]
pub use runtime_limits::{DEV_MATRIXONE_PASSWORD, warn_default_credentials_once};
pub use sqlx;

/// Base directory name for per-agent git worktrees under `std::env::temp_dir()`.
///
/// Shared between worktree creation (runtime) and path validation (CLI)
/// to keep the two in sync.
pub const WORKTREE_BASE_DIR: &str = "mo-agent-worktrees";

/// Return the canonical worktree base path: `<temp_dir>/mo-agent-worktrees`.
pub fn worktree_base_path() -> PathBuf {
    std::env::temp_dir().join(WORKTREE_BASE_DIR)
}

// ─── Run Status Constants ───────────────────────────────────────────────────

pub const STATUS_RUNNING: &str = "running";
pub const STATUS_COMPLETED: &str = "completed";
pub const STATUS_FAILED: &str = "failed";
pub const STATUS_PAUSED: &str = "paused";
pub const STATUS_CANCELLED: &str = "cancelled";
pub const STATUS_WAITING: &str = "waiting";
pub const STATUS_VERIFICATION_FAILED: &str = "verification_failed";

// ─── Sub-Run State Machine ──────────────────────────────────────────────────

/// Compile-time-enforced lifecycle states for delegation sub-runs.
///
/// ```text
/// Created ──► Running ──┬──► Completed
///                       ├──► Failed
///                       ├──► Paused ──► Running (resume)
///                       ├──► Cancelled
///                       └──► VerificationFailed
/// ```
///
/// All transitions are validated via [`SubRunState::try_transition`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubRunState {
    Created,
    Running,
    Completed,
    Failed,
    Paused,
    Cancelled,
    VerificationFailed,
}

impl SubRunState {
    /// Attempt a state transition.  Returns `Ok(to)` on a legal transition,
    /// `Err(InvalidTransition)` if the transition is not allowed.
    pub fn try_transition(self, to: SubRunState) -> Result<SubRunState, InvalidTransition> {
        if self.can_transition_to(to) {
            Ok(to)
        } else {
            Err(InvalidTransition { from: self, to })
        }
    }

    /// Check whether transitioning from `self` → `to` is legal.
    pub fn can_transition_to(self, to: SubRunState) -> bool {
        matches!(
            (self, to),
            (SubRunState::Created, SubRunState::Running)
                | (SubRunState::Running, SubRunState::Completed)
                | (SubRunState::Running, SubRunState::Failed)
                | (SubRunState::Running, SubRunState::Paused)
                | (SubRunState::Running, SubRunState::Cancelled)
                | (SubRunState::Running, SubRunState::VerificationFailed)
                | (SubRunState::Paused, SubRunState::Running)
                | (SubRunState::Paused, SubRunState::Cancelled)
        )
    }

    /// Whether the state is terminal (no further transitions possible).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            SubRunState::Completed
                | SubRunState::Failed
                | SubRunState::Cancelled
                | SubRunState::VerificationFailed
        )
    }

    /// Whether the sub-run completed successfully.
    pub fn is_success(self) -> bool {
        self == SubRunState::Completed
    }

    /// Convert to the canonical string constant (backward-compatible).
    pub fn as_str(self) -> &'static str {
        match self {
            SubRunState::Created => "created",
            SubRunState::Running => STATUS_RUNNING,
            SubRunState::Completed => STATUS_COMPLETED,
            SubRunState::Failed => STATUS_FAILED,
            SubRunState::Paused => STATUS_PAUSED,
            SubRunState::Cancelled => STATUS_CANCELLED,
            SubRunState::VerificationFailed => STATUS_VERIFICATION_FAILED,
        }
    }

    /// Parse from a status string (backward-compatible).
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<SubRunState> {
        match s {
            "created" => Some(SubRunState::Created),
            "running" => Some(SubRunState::Running),
            "completed" => Some(SubRunState::Completed),
            "failed" => Some(SubRunState::Failed),
            "paused" => Some(SubRunState::Paused),
            "cancelled" => Some(SubRunState::Cancelled),
            "verification_failed" => Some(SubRunState::VerificationFailed),
            _ => None,
        }
    }
}

impl std::fmt::Display for SubRunState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when an illegal state transition is attempted.
#[derive(Debug, Clone)]
pub struct InvalidTransition {
    pub from: SubRunState,
    pub to: SubRunState,
}

impl std::fmt::Display for InvalidTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid sub-run state transition: {} → {}",
            self.from, self.to
        )
    }
}

impl std::error::Error for InvalidTransition {}

/// Create a one-shot connection pool (legacy — prefer `SharedPool` for production).
pub async fn connect_matrixone(settings: &MatrixOneSettings) -> Result<Pool<MySql>, sqlx::Error> {
    MySqlPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(2))
        .connect(&settings.database_url_with_password())
        .await
}

/// Shared connection pool that can be cloned cheaply across services.
#[derive(Clone, Debug)]
pub struct SharedPool {
    pool: Arc<Pool<MySql>>,
    settings: MatrixOneSettings,
}

impl SharedPool {
    pub async fn new(settings: &MatrixOneSettings) -> Result<Self, sqlx::Error> {
        let pool = MySqlPoolOptions::new()
            .max_connections(10)
            .min_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .idle_timeout(std::time::Duration::from_secs(300))
            .connect(&settings.database_url_with_password())
            .await?;
        Ok(Self {
            pool: Arc::new(pool),
            settings: settings.clone(),
        })
    }

    pub fn get(&self) -> &Pool<MySql> {
        &self.pool
    }

    pub fn settings(&self) -> &MatrixOneSettings {
        &self.settings
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
}

/// Standard JSON error envelope for HTTP APIs.
///
/// `detail` is the human-readable message. `error_code` and `request_id` are optional
/// in the wire format so older clients keep working; the server middleware fills
/// `request_id` when missing on 4xx/5xx JSON responses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorResponse {
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

impl ErrorResponse {
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            error_code: None,
            request_id: None,
        }
    }

    pub fn with_error_code(mut self, code: impl Into<String>) -> Self {
        self.error_code = Some(code.into());
        self
    }

    pub fn with_request_id(mut self, id: impl Into<String>) -> Self {
        self.request_id = Some(id.into());
        self
    }
}

pub fn error_response(
    status: StatusCode,
    detail: impl Into<String>,
) -> (StatusCode, Json<ErrorResponse>) {
    (status, Json(ErrorResponse::new(detail)))
}

/// Same as [`error_response`] but attaches a stable machine-oriented `error_code`.
pub fn error_response_coded(
    status: StatusCode,
    detail: impl Into<String>,
    error_code: impl Into<String>,
) -> (StatusCode, Json<ErrorResponse>) {
    (
        status,
        Json(ErrorResponse::new(detail).with_error_code(error_code)),
    )
}

pub fn internal_error(error: impl ToString) -> (StatusCode, Json<ErrorResponse>) {
    error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

pub fn internal_error_coded(
    error: impl ToString,
    error_code: impl Into<String>,
) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse::new(error.to_string()).with_error_code(error_code)),
    )
}

/// MySQL/MatrixOne duplicate-key errors may surface as vendor code 1062,
/// SQLSTATE 23000, or wrapped message-only errors.
pub fn is_duplicate_key_error(err: &sqlx::Error) -> bool {
    match err {
        sqlx::Error::Database(db_err) => {
            let message = db_err.message();
            matches!(db_err.code().as_deref(), Some("1062") | Some("23000"))
                || message.contains("Duplicate entry")
                || message.contains("ER_DUP_ENTRY")
        }
        _ => {
            let message = err.to_string();
            message.contains("Duplicate entry")
                && (message.contains("1062") || message.contains("ER_DUP_ENTRY"))
        }
    }
}

pub fn current_unix_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn bearer_token(headers: &HeaderMap) -> Result<&str, (StatusCode, Json<ErrorResponse>)> {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.starts_with("Bearer "))
        .map(|value| &value["Bearer ".len()..])
        .filter(|value| !value.is_empty())
        .ok_or_else(|| error_response(StatusCode::UNAUTHORIZED, "Not authenticated"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- error_response ---

    #[test]
    fn error_response_status_and_detail() {
        let (status, Json(body)) = error_response(StatusCode::BAD_REQUEST, "bad input");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body.detail, "bad input");
    }

    #[test]
    fn error_response_from_string() {
        let (status, Json(body)) = error_response(StatusCode::NOT_FOUND, String::from("missing"));
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body.detail, "missing");
        assert!(body.error_code.is_none());
        assert!(body.request_id.is_none());
    }

    #[test]
    fn error_response_coded_sets_error_code() {
        let (status, Json(body)) =
            error_response_coded(StatusCode::BAD_REQUEST, "bad", "validation_failed");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body.detail, "bad");
        assert_eq!(body.error_code.as_deref(), Some("validation_failed"));
        assert!(body.request_id.is_none());
    }

    #[test]
    fn error_response_json_omits_empty_optional_fields() {
        let Json(body) = error_response(StatusCode::NOT_FOUND, "x").1;
        let v = serde_json::to_value(&body).expect("serialize");
        assert_eq!(v["detail"], "x");
        assert!(v.get("error_code").is_none());
        assert!(v.get("request_id").is_none());
    }

    // --- internal_error ---

    #[test]
    fn internal_error_wraps_to_string() {
        let (status, Json(body)) = internal_error("db failed");
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body.detail, "db failed");
    }

    #[test]
    fn internal_error_from_io_error() {
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "file gone");
        let (status, _) = internal_error(err);
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn duplicate_key_error_detects_protocol_wrappers() {
        let err = sqlx::Error::Protocol("1062: Duplicate entry 'test' for key".into());
        assert!(is_duplicate_key_error(&err));
    }

    #[test]
    fn duplicate_key_error_rejects_unrelated_protocol_wrappers() {
        let err = sqlx::Error::Protocol("connection reset by peer".into());
        assert!(!is_duplicate_key_error(&err));
    }

    // --- current_unix_seconds ---

    #[test]
    fn current_unix_seconds_positive() {
        let ts = current_unix_seconds();
        assert!(ts > 1_700_000_000.0); // after 2023
    }

    // --- bearer_token ---

    #[test]
    fn bearer_token_valid() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer abc123".parse().unwrap());
        assert_eq!(bearer_token(&headers).ok(), Some("abc123"));
    }

    #[test]
    fn bearer_token_missing_header() {
        let headers = HeaderMap::new();
        assert!(bearer_token(&headers).is_err());
    }

    #[test]
    fn bearer_token_wrong_prefix() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Basic abc".parse().unwrap());
        assert!(bearer_token(&headers).is_err());
    }

    #[test]
    fn bearer_token_empty_after_prefix() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer ".parse().unwrap());
        assert!(bearer_token(&headers).is_err());
    }

    #[test]
    fn bearer_token_with_spaces() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer token with spaces".parse().unwrap());
        assert_eq!(bearer_token(&headers).ok(), Some("token with spaces"));
    }

    // --- bearer_token edge cases ---

    #[test]
    fn bearer_token_lowercase_header_name() {
        // HeaderMap is case-insensitive
        let mut headers = HeaderMap::new();
        headers.insert("Authorization", "Bearer mytoken".parse().unwrap());
        assert_eq!(bearer_token(&headers).ok(), Some("mytoken"));
    }

    #[test]
    fn bearer_token_no_space_after_bearer() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearertoken".parse().unwrap());
        assert!(bearer_token(&headers).is_err());
    }

    #[test]
    fn bearer_token_double_space() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer  double".parse().unwrap());
        // starts_with("Bearer ") matches, then remainder is " double" (with leading space)
        assert_eq!(bearer_token(&headers).ok(), Some(" double"));
    }

    // --- error_response edge cases ---

    #[test]
    fn error_response_preserves_unicode() {
        let (status, Json(body)) = error_response(StatusCode::BAD_REQUEST, "无效请求");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body.detail, "无效请求");
    }

    #[test]
    fn error_response_empty_detail() {
        let (status, Json(body)) = error_response(StatusCode::NOT_FOUND, "");
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body.detail, "");
    }

    #[test]
    fn internal_error_always_500() {
        let (status, _) = internal_error("anything");
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        let (status2, _) = internal_error("");
        assert_eq!(status2, StatusCode::INTERNAL_SERVER_ERROR);
    }

    // --- SubRunState ---

    #[test]
    fn valid_transitions_succeed() {
        // Created → Running
        assert_eq!(
            SubRunState::Created
                .try_transition(SubRunState::Running)
                .unwrap(),
            SubRunState::Running
        );
        // Running → Completed
        assert_eq!(
            SubRunState::Running
                .try_transition(SubRunState::Completed)
                .unwrap(),
            SubRunState::Completed
        );
        // Running → Failed
        assert_eq!(
            SubRunState::Running
                .try_transition(SubRunState::Failed)
                .unwrap(),
            SubRunState::Failed
        );
        // Running → Paused
        assert_eq!(
            SubRunState::Running
                .try_transition(SubRunState::Paused)
                .unwrap(),
            SubRunState::Paused
        );
        // Running → Cancelled
        assert_eq!(
            SubRunState::Running
                .try_transition(SubRunState::Cancelled)
                .unwrap(),
            SubRunState::Cancelled
        );
        // Running → VerificationFailed
        assert_eq!(
            SubRunState::Running
                .try_transition(SubRunState::VerificationFailed)
                .unwrap(),
            SubRunState::VerificationFailed
        );
        // Paused → Running (resume)
        assert_eq!(
            SubRunState::Paused
                .try_transition(SubRunState::Running)
                .unwrap(),
            SubRunState::Running
        );
        // Paused → Cancelled
        assert_eq!(
            SubRunState::Paused
                .try_transition(SubRunState::Cancelled)
                .unwrap(),
            SubRunState::Cancelled
        );
    }

    #[test]
    fn invalid_transitions_fail() {
        // Created → Completed (must go through Running)
        assert!(
            SubRunState::Created
                .try_transition(SubRunState::Completed)
                .is_err()
        );
        // Completed → Running (terminal state)
        assert!(
            SubRunState::Completed
                .try_transition(SubRunState::Running)
                .is_err()
        );
        // Failed → Running (terminal state)
        assert!(
            SubRunState::Failed
                .try_transition(SubRunState::Running)
                .is_err()
        );
        // Cancelled → Running (terminal state)
        assert!(
            SubRunState::Cancelled
                .try_transition(SubRunState::Running)
                .is_err()
        );
        // Created → Paused (can't pause before running)
        assert!(
            SubRunState::Created
                .try_transition(SubRunState::Paused)
                .is_err()
        );
    }

    #[test]
    fn terminal_states_are_correct() {
        assert!(!SubRunState::Created.is_terminal());
        assert!(!SubRunState::Running.is_terminal());
        assert!(!SubRunState::Paused.is_terminal());
        assert!(SubRunState::Completed.is_terminal());
        assert!(SubRunState::Failed.is_terminal());
        assert!(SubRunState::Cancelled.is_terminal());
        assert!(SubRunState::VerificationFailed.is_terminal());
    }

    #[test]
    fn success_states() {
        assert!(SubRunState::Completed.is_success());
        assert!(!SubRunState::Failed.is_success());
        assert!(!SubRunState::Running.is_success());
        assert!(!SubRunState::VerificationFailed.is_success());
    }

    #[test]
    fn display_and_from_str_roundtrip() {
        for state in &[
            SubRunState::Created,
            SubRunState::Running,
            SubRunState::Completed,
            SubRunState::Failed,
            SubRunState::Paused,
            SubRunState::Cancelled,
            SubRunState::VerificationFailed,
        ] {
            let s = state.as_str();
            assert_eq!(SubRunState::from_str(s).unwrap(), *state);
        }
    }

    #[test]
    fn from_str_unknown_returns_none() {
        assert!(SubRunState::from_str("unknown_state").is_none());
    }

    #[test]
    fn can_transition_to_is_consistent_with_try() {
        let all = [
            SubRunState::Created,
            SubRunState::Running,
            SubRunState::Completed,
            SubRunState::Failed,
            SubRunState::Paused,
            SubRunState::Cancelled,
            SubRunState::VerificationFailed,
        ];
        for from in &all {
            for to in &all {
                assert_eq!(
                    from.can_transition_to(*to),
                    from.try_transition(*to).is_ok(),
                    "mismatch for {:?} → {:?}",
                    from,
                    to
                );
            }
        }
    }

    #[test]
    fn self_transition_created_to_created_fails() {
        assert!(
            SubRunState::Created
                .try_transition(SubRunState::Created)
                .is_err()
        );
    }
}
