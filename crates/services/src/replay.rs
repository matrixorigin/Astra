use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};

use astra_core::{ErrorResponse, MatrixOneSettings, SharedPool, error_response, internal_error};

use crate::storage::agent_session_exists_for_user;

const REPLAY_UNAVAILABLE_DETAIL: &str =
    "Session replay is unavailable until durable replay reconstruction is implemented";

// ── Data types ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReplayResponse {
    pub replay_id: String,
    pub session_id: String,
    pub status: String,
    pub events_replayed: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_name: Option<String>,
    pub mock_mode: bool,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ComparisonResponse {
    pub session_id: String,
    pub original_event_count: i64,
    pub replay_event_count: i64,
    pub difference: i64,
    pub is_match: bool,
    pub compared_at: String,
}

// ── Trait ─────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait ReplayService: Send + Sync {
    async fn replay_session(
        &self,
        user_id: String,
        session_id: String,
        request: ReplaySessionRequestData,
    ) -> Result<ReplayResponse, (StatusCode, Json<ErrorResponse>)>;

    async fn compare_replay(
        &self,
        user_id: String,
        session_id: String,
    ) -> Result<ComparisonResponse, (StatusCode, Json<ErrorResponse>)>;
}

// ── Internal request data ────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct ReplaySessionRequestData {
    pub sandbox_name: Option<String>,
    pub mock_mode: bool,
}

// ── Database implementation ──────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct DatabaseReplayService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseReplayService {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
        }
    }
    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        crate::require_shared_pool(self.pool.as_ref(), "DatabaseReplayService", &self.matrixone)
    }
}

#[async_trait]
impl ReplayService for DatabaseReplayService {
    async fn replay_session(
        &self,
        user_id: String,
        session_id: String,
        _request: ReplaySessionRequestData,
    ) -> Result<ReplayResponse, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        if !agent_session_exists_for_user(&pool, &session_id, &user_id)
            .await
            .map_err(internal_error)?
        {
            return Err(error_response(StatusCode::NOT_FOUND, "Session not found"));
        }

        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            REPLAY_UNAVAILABLE_DETAIL,
        ))
    }

    async fn compare_replay(
        &self,
        user_id: String,
        session_id: String,
    ) -> Result<ComparisonResponse, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        if !agent_session_exists_for_user(&pool, &session_id, &user_id)
            .await
            .map_err(internal_error)?
        {
            return Err(error_response(StatusCode::NOT_FOUND, "Session not found"));
        }

        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            REPLAY_UNAVAILABLE_DETAIL,
        ))
    }
}

// ── Noop implementation ──────────────────────────────────────────────────────

pub struct UnconfiguredReplayService;

#[async_trait]
impl ReplayService for UnconfiguredReplayService {
    async fn replay_session(
        &self,
        _: String,
        _: String,
        _: ReplaySessionRequestData,
    ) -> Result<ReplayResponse, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("replay service not configured"))
    }
    async fn compare_replay(
        &self,
        _: String,
        _: String,
    ) -> Result<ComparisonResponse, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("replay service not configured"))
    }
}

// ── HTTP types ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ReplaySessionRequest {
    pub sandbox_name: Option<String>,
    #[serde(default = "default_mock_mode")]
    pub mock_mode: Option<bool>,
}

fn default_mock_mode() -> Option<bool> {
    Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_session_request_default_mock_mode() {
        let req: ReplaySessionRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(req.mock_mode, Some(true));
        assert!(req.sandbox_name.is_none());
    }

    #[test]
    fn replay_session_request_explicit_mock_false() {
        let req: ReplaySessionRequest = serde_json::from_str(r#"{"mock_mode": false}"#).unwrap();
        assert_eq!(req.mock_mode, Some(false));
    }

    #[test]
    fn replay_response_skip_serializing_none_sandbox() {
        let resp = ReplayResponse {
            replay_id: "r1".into(),
            session_id: "s1".into(),
            status: "ok".into(),
            events_replayed: 0,
            sandbox_name: None,
            mock_mode: true,
            created_at: "now".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("sandbox_name"));
    }
}
