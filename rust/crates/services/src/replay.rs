use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{Row, query};
use uuid::Uuid;

use astra_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};

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
        if let Some(ref p) = self.pool {
            return Ok(p.get().clone());
        }
        connect_matrixone(&self.matrixone).await
    }
}

#[async_trait]
impl ReplayService for DatabaseReplayService {
    async fn replay_session(
        &self,
        user_id: String,
        session_id: String,
        request: ReplaySessionRequestData,
    ) -> Result<ReplayResponse, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let session_row = query("SELECT user_id FROM agent_sessions WHERE session_id = ?")
            .bind(&session_id)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?;

        let session_row = session_row
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Session not found"))?;
        let owner: String = session_row.try_get("user_id").map_err(internal_error)?;
        if owner != user_id {
            return Err(error_response(StatusCode::FORBIDDEN, "Not authorized"));
        }

        let count_row =
            query("SELECT COUNT(*) AS cnt FROM agent_events WHERE session_id = ? AND user_id = ?")
                .bind(&session_id)
                .bind(&user_id)
                .fetch_one(&pool)
                .await
                .map_err(internal_error)?;
        let events_replayed: i64 = count_row.try_get("cnt").unwrap_or(0);

        let replay_id = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();

        Ok(ReplayResponse {
            replay_id,
            session_id,
            status: "completed".into(),
            events_replayed,
            sandbox_name: request.sandbox_name,
            mock_mode: request.mock_mode,
            created_at: now,
        })
    }

    async fn compare_replay(
        &self,
        user_id: String,
        session_id: String,
    ) -> Result<ComparisonResponse, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let session_row = query("SELECT user_id FROM agent_sessions WHERE session_id = ?")
            .bind(&session_id)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?;

        let session_row = session_row
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Session not found"))?;
        let owner: String = session_row.try_get("user_id").map_err(internal_error)?;
        if owner != user_id {
            return Err(error_response(StatusCode::FORBIDDEN, "Not authorized"));
        }

        let counts = query(
            "SELECT \
               COUNT(CASE WHEN event_type != 'replay' THEN 1 END) AS original_cnt, \
               COUNT(CASE WHEN event_type = 'replay' THEN 1 END) AS replay_cnt \
             FROM agent_events WHERE session_id = ? AND user_id = ?",
        )
        .bind(&session_id)
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .map_err(internal_error)?;
        let original_event_count: i64 = counts.try_get("original_cnt").unwrap_or(0);
        let replay_event_count: i64 = counts.try_get("replay_cnt").unwrap_or(0);

        let difference = (original_event_count - replay_event_count).abs();
        let is_match = difference == 0;
        let compared_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();

        Ok(ComparisonResponse {
            session_id,
            original_event_count,
            replay_event_count,
            difference,
            is_match,
            compared_at,
        })
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
