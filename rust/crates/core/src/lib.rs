use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    Json,
    http::{HeaderMap, StatusCode},
};
use serde::Serialize;
use sqlx::{MySql, Pool, mysql::MySqlPoolOptions};

pub mod composite_snapshot;
pub mod config;
pub mod log;
pub mod runtime_limits;
pub use config::*;
pub use runtime_limits::{DEV_MATRIXONE_PASSWORD, RuntimeLimits, warn_default_credentials_once};
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

/// Create a one-shot connection pool (legacy — prefer `SharedPool` for production).
pub async fn connect_matrixone(settings: &MatrixOneSettings) -> Result<Pool<MySql>, sqlx::Error> {
    MySqlPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(2))
        .connect(&settings.database_url())
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
            .connect(&settings.database_url())
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

#[derive(Serialize, PartialEq, Eq)]
pub struct ErrorResponse {
    pub detail: String,
}

pub fn error_response(
    status: StatusCode,
    detail: impl Into<String>,
) -> (StatusCode, Json<ErrorResponse>) {
    (
        status,
        Json(ErrorResponse {
            detail: detail.into(),
        }),
    )
}

pub fn internal_error(error: impl ToString) -> (StatusCode, Json<ErrorResponse>) {
    error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
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
}
