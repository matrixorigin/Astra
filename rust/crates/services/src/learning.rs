use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use mo_agent_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};
use serde::{Deserialize, Serialize};
use sqlx::query;

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct LearningFeedbackRequestData {
    pub event_id: String,
    pub user_id: String,
    pub satisfaction_score: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LearningFeedbackRecord {
    pub status: String,
    pub message: String,
}

pub type ServiceResult<T> = Result<T, (StatusCode, Json<ErrorResponse>)>;

// ── Trait ─────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait LearningFeedbackService: Send + Sync {
    async fn submit_feedback(
        &self,
        request: LearningFeedbackRequestData,
    ) -> ServiceResult<LearningFeedbackRecord>;
}

// ── Database implementation ──────────────────────────────────────────────────

pub struct DatabaseLearningFeedbackService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseLearningFeedbackService {
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
impl LearningFeedbackService for DatabaseLearningFeedbackService {
    async fn submit_feedback(
        &self,
        request: LearningFeedbackRequestData,
    ) -> ServiceResult<LearningFeedbackRecord> {
        let pool = self
            .get_pool()
            .await
            .map_err(|e| internal_error(format!("DB connect: {e}")))?;

        // Verify event exists AND belongs to the requesting user (via session ownership)
        let owned = query(
            "SELECT 1 FROM skill_selection_events e \
             JOIN agent_sessions s ON e.session_id = s.session_id \
             WHERE e.event_id = ? AND s.user_id = ? LIMIT 1",
        )
        .bind(&request.event_id)
        .bind(&request.user_id)
        .fetch_optional(&pool)
        .await
        .map_err(|e| internal_error(format!("ownership check: {e}")))?;

        if owned.is_none() {
            return Err(error_response(StatusCode::NOT_FOUND, "Event not found"));
        }

        let score = request.satisfaction_score.unwrap_or(0);
        query("UPDATE skill_selection_events SET user_feedback_score = ? WHERE event_id = ?")
            .bind(score)
            .bind(&request.event_id)
            .execute(&pool)
            .await
            .map_err(|e| internal_error(format!("update feedback: {e}")))?;

        Ok(LearningFeedbackRecord {
            status: "success".to_string(),
            message: format!("Feedback recorded for event {}", request.event_id),
        })
    }
}

// ── Unconfigured ─────────────────────────────────────────────────────────────

pub struct UnconfiguredLearningFeedbackService;

#[async_trait]
impl LearningFeedbackService for UnconfiguredLearningFeedbackService {
    async fn submit_feedback(
        &self,
        _: LearningFeedbackRequestData,
    ) -> ServiceResult<LearningFeedbackRecord> {
        Err(internal_error("learning feedback service not configured"))
    }
}
