use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use mo_agent_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};
use serde::Serialize;
use sqlx::{Row, query};

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ReflectEvidence {
    pub focus: String,
    pub session_id: String,
    pub event_trail: Vec<ReflectEvent>,
    pub decisions: Vec<ReflectDecision>,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReflectEvent {
    pub event_id: String,
    pub event_type: String,
    pub skill_name: Option<String>,
    pub created_at: String,
    pub content_preview: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReflectDecision {
    pub decision_id: String,
    pub decision_type: String,
    pub model_used: Option<String>,
    pub created_at: String,
    pub output_preview: String,
}

pub type ServiceResult<T> = Result<T, (StatusCode, Json<ErrorResponse>)>;

// ── Trait ─────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait ReflectService: Send + Sync {
    async fn build_evidence(
        &self,
        user_id: &str,
        session_id: &str,
        focus: &str,
        last_n: i32,
        question: &str,
    ) -> ServiceResult<ReflectEvidence>;
}

// ── Database implementation ──────────────────────────────────────────────────

pub struct DatabaseReflectService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseReflectService {
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
impl ReflectService for DatabaseReflectService {
    async fn build_evidence(
        &self,
        user_id: &str,
        session_id: &str,
        focus: &str,
        last_n: i32,
        question: &str,
    ) -> ServiceResult<ReflectEvidence> {
        let pool = self
            .get_pool()
            .await
            .map_err(|e| internal_error(format!("DB connect: {e}")))?;

        // Verify session ownership
        let owner_check =
            query("SELECT 1 FROM agent_sessions WHERE session_id = ? AND user_id = ? LIMIT 1")
                .bind(session_id)
                .bind(user_id)
                .fetch_optional(&pool)
                .await
                .map_err(|e| internal_error(format!("session check: {e}")))?;

        if owner_check.is_none() {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                "Session not found or not owned by user",
            ));
        }

        // Fetch recent events with optional focus filter
        let event_type_filter = match focus {
            "skill_failure" => Some("tool_result"),
            "tool_selection" => Some("tool_call"),
            _ => None,
        };

        let events = if let Some(et) = event_type_filter {
            query(
                "SELECT event_id, event_type, skill_name, \
                 CAST(created_at AS CHAR) AS created_at, \
                 SUBSTRING(COALESCE(content, ''), 1, 200) AS content_preview \
                 FROM agent_events \
                 WHERE session_id = ? AND event_type = ? \
                 ORDER BY created_at DESC LIMIT ?",
            )
            .bind(session_id)
            .bind(et)
            .bind(last_n)
            .fetch_all(&pool)
            .await
        } else {
            query(
                "SELECT event_id, event_type, skill_name, \
                 CAST(created_at AS CHAR) AS created_at, \
                 SUBSTRING(COALESCE(content, ''), 1, 200) AS content_preview \
                 FROM agent_events \
                 WHERE session_id = ? \
                 ORDER BY created_at DESC LIMIT ?",
            )
            .bind(session_id)
            .bind(last_n)
            .fetch_all(&pool)
            .await
        }
        .map_err(|e| internal_error(format!("events query: {e}")))?;

        let event_trail: Vec<ReflectEvent> = events
            .iter()
            .map(|row| ReflectEvent {
                event_id: row.get::<String, _>("event_id"),
                event_type: row.get::<String, _>("event_type"),
                skill_name: row.get::<Option<String>, _>("skill_name"),
                created_at: row.get::<String, _>("created_at"),
                content_preview: row.get::<String, _>("content_preview"),
            })
            .collect();

        // Fetch decision audits for this session
        let decision_rows = query(
            "SELECT decision_id, decision_type, model_used, \
             CAST(created_at AS CHAR) AS created_at, \
             SUBSTRING(COALESCE(CAST(decision_output AS CHAR), '{}'), 1, 200) AS output_preview \
             FROM ctx_decision_audits \
             WHERE session_id = ? \
             ORDER BY created_at DESC LIMIT ?",
        )
        .bind(session_id)
        .bind(last_n)
        .fetch_all(&pool)
        .await
        .map_err(|e| internal_error(format!("decisions query: {e}")))?;

        let decisions: Vec<ReflectDecision> = decision_rows
            .iter()
            .map(|row| ReflectDecision {
                decision_id: row.get::<String, _>("decision_id"),
                decision_type: row.get::<String, _>("decision_type"),
                model_used: row.get::<Option<String>, _>("model_used"),
                created_at: row.get::<String, _>("created_at"),
                output_preview: row.get::<String, _>("output_preview"),
            })
            .collect();

        let summary = format!(
            "Session {session_id}: {n_events} events, {n_decisions} decisions (focus={focus}{q})",
            n_events = event_trail.len(),
            n_decisions = decisions.len(),
            q = if question.is_empty() {
                String::new()
            } else {
                format!(", question=\"{question}\"")
            },
        );

        Ok(ReflectEvidence {
            focus: focus.to_string(),
            session_id: session_id.to_string(),
            event_trail,
            decisions,
            summary,
        })
    }
}

// ── Unconfigured ─────────────────────────────────────────────────────────────

pub struct UnconfiguredReflectService;

#[async_trait]
impl ReflectService for UnconfiguredReflectService {
    async fn build_evidence(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: i32,
        _: &str,
    ) -> ServiceResult<ReflectEvidence> {
        Err(internal_error("reflect service not configured"))
    }
}
