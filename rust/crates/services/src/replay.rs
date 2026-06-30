use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{Row, query};
use uuid::Uuid;

use astra_core::{ErrorResponse, MatrixOneSettings, SharedPool, error_response, internal_error};

use crate::storage::agent_session_exists_for_user;

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

trait ReplayCountRow {
    fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error>;
}

impl ReplayCountRow for sqlx::mysql::MySqlRow {
    fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
        self.try_get::<i64, _>(column)
    }
}

fn replay_count_column(
    row: &impl ReplayCountRow,
    column: &str,
) -> Result<i64, (StatusCode, Json<ErrorResponse>)> {
    row.i64_column(column)
        .map_err(|e| internal_error(format!("replay count decode column `{column}`: {e}")))
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
        request: ReplaySessionRequestData,
    ) -> Result<ReplayResponse, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let session_row = query(
            "SELECT event_count FROM agent_sessions
             WHERE session_id = ? AND user_id = ? LIMIT 1",
        )
        .bind(&session_id)
        .bind(&user_id)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Session not found"))?;
        let events_replayed: i64 = session_row.try_get("event_count").map_err(internal_error)?;

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

        if !agent_session_exists_for_user(&pool, &session_id, &user_id)
            .await
            .map_err(internal_error)?
        {
            return Err(error_response(StatusCode::NOT_FOUND, "Session not found"));
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
        let original_event_count = replay_count_column(&counts, "original_cnt")?;
        let replay_event_count = replay_count_column(&counts, "replay_cnt")?;

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

    struct FakeReplayCountRow {
        failed_column: Option<&'static str>,
    }

    impl FakeReplayCountRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
            }
        }
    }

    impl ReplayCountRow for FakeReplayCountRow {
        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }

            match column {
                "original_cnt" => Ok(7),
                "replay_cnt" => Ok(5),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }
    }

    #[test]
    fn replay_count_column_preserves_database_values() {
        let row = FakeReplayCountRow::complete();

        assert_eq!(replay_count_column(&row, "original_cnt").unwrap(), 7);
        assert_eq!(replay_count_column(&row, "replay_cnt").unwrap(), 5);
    }

    #[test]
    fn replay_count_column_fails_loudly_on_decode_errors() {
        for column in ["original_cnt", "replay_cnt"] {
            let row = FakeReplayCountRow::fail_on(column);
            let (status, Json(body)) = replay_count_column(&row, column).unwrap_err();
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
            assert!(
                body.detail.contains(&format!("decode column `{column}`")),
                "error should identify failed column: {:?}",
                body.detail
            );
        }
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
