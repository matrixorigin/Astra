//! Agent-level lessons — persistent cross-session memory of what worked and
//! what didn't in prior sessions, scoped by `(user_id, persona, workload_tag)`.
//!
//! ## Why this exists
//!
//! The in-session self-model loop closes within a single chat (boosted tools,
//! blocked tools, unmet postconditions). When the user starts a new session
//! the agent forgets everything it learned. Memoria handles *user*-level
//! memory; `agent_lessons` handles *agent × workload*-level lessons — the
//! things the runtime concludes about its own behaviour (this tool is slow
//! on this scenario, that postcondition shape rarely holds, etc.).
//!
//! ## Scope
//!
//! A lesson is addressed by `(user_id, persona, workload_tag)`:
//! - `persona` is the agent's configured role (generic / code-review /
//!   plan-exec / debugger / …).
//! - `workload_tag` is an optional narrower bucket. `NULL` means "general";
//!   a concrete value scopes the lesson to matching sessions.
//!
//! ## Lifecycle
//!
//! - **Record**: upsert-by-content. If a lesson with the same
//!   `(user_id, persona, workload_tag, kind, trigger_signal, action)` already
//!   exists, its `hit_count` is incremented and `updated_at` refreshed —
//!   never a new row. This keeps the table from ballooning under repeated
//!   signals.
//! - **Load**: newest-first by `updated_at DESC`, bounded. Caller usually
//!   wants top-N for prompt injection.
//! - **Prune**: age-out by `updated_at`. A lesson that keeps getting hits
//!   stays fresh; stale lessons (last hit > N days ago) are deleted.
//!
//! `hit_count` is deliberately tracked but not yet weighted into ranking —
//! that comes in the cross-session lesson-pickup E2E.

use astra_core::{MatrixOneSettings, SharedPool, connect_matrixone};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{MySql, Pool, Row, query};
use uuid::Uuid;

// ── Types ───────────────────────────────────────────────────────────────────

/// Classifier for what a lesson is teaching the agent to do next time.
/// Stable string tags (snake_case) so DB rows and JSON are self-describing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LessonKind {
    /// Avoid this tool for this scope — it failed or was slow last time.
    ToolDeprioritize,
    /// Prefer this tool for this scope — it worked well last time.
    ToolBoost,
    /// The system prompt / context shape that led to success.
    PromptShape,
    /// A postcondition pattern that kept failing — restructure the plan.
    PostconditionPattern,
    /// A recovery recipe for a specific error signature.
    ErrorRecovery,
}

impl LessonKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ToolDeprioritize => "tool_deprioritize",
            Self::ToolBoost => "tool_boost",
            Self::PromptShape => "prompt_shape",
            Self::PostconditionPattern => "postcondition_pattern",
            Self::ErrorRecovery => "error_recovery",
        }
    }

    pub fn parse_tag(tag: &str) -> Option<Self> {
        match tag {
            "tool_deprioritize" => Some(Self::ToolDeprioritize),
            "tool_boost" => Some(Self::ToolBoost),
            "prompt_shape" => Some(Self::PromptShape),
            "postcondition_pattern" => Some(Self::PostconditionPattern),
            "error_recovery" => Some(Self::ErrorRecovery),
            _ => None,
        }
    }
}

/// A persisted lesson.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lesson {
    pub id: String,
    pub user_id: String,
    pub persona: String,
    pub workload_tag: Option<String>,
    pub kind: LessonKind,
    /// Short human-readable description of what triggered this lesson
    /// (e.g. `"3 consecutive ToolMisuse on grep"`). ≤255 chars.
    pub trigger_signal: String,
    /// Short imperative of what to do next time
    /// (e.g. `"deprioritize grep for regex-heavy tasks"`). ≤1024 chars.
    pub action: String,
    pub confidence: f64,
    pub hit_count: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Payload for `record`. Id / timestamps are assigned by the DAO.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewLesson {
    pub user_id: String,
    pub persona: String,
    pub workload_tag: Option<String>,
    pub kind: LessonKind,
    pub trigger_signal: String,
    pub action: String,
    /// Defaults to `DEFAULT_LESSON_CONFIDENCE` when unset.
    pub confidence: Option<f64>,
}

/// Default confidence score for a freshly-recorded lesson.
pub const DEFAULT_LESSON_CONFIDENCE: f64 = 0.6;
/// Max `trigger_signal` length, enforced by `NewLesson::validate`.
pub const MAX_TRIGGER_SIGNAL_LEN: usize = 255;
/// Max `action` length.
pub const MAX_ACTION_LEN: usize = 1024;

impl NewLesson {
    /// Reject payloads that would violate the schema or carry nonsensical
    /// values. Validation runs before any SQL is emitted.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.user_id.is_empty() {
            return Err("user_id must not be empty");
        }
        if self.persona.is_empty() {
            return Err("persona must not be empty");
        }
        if self.trigger_signal.is_empty() {
            return Err("trigger_signal must not be empty");
        }
        if self.trigger_signal.chars().count() > MAX_TRIGGER_SIGNAL_LEN {
            return Err("trigger_signal exceeds MAX_TRIGGER_SIGNAL_LEN");
        }
        if self.action.is_empty() {
            return Err("action must not be empty");
        }
        if self.action.chars().count() > MAX_ACTION_LEN {
            return Err("action exceeds MAX_ACTION_LEN");
        }
        if let Some(c) = self.confidence
            && (!(0.0..=1.0).contains(&c) || c.is_nan())
        {
            return Err("confidence must be in [0.0, 1.0]");
        }
        Ok(())
    }
}

// ── Service trait ───────────────────────────────────────────────────────────

#[async_trait]
pub trait AgentLessonsService: Send + Sync {
    /// Record (or upsert-by-content) a lesson. If an existing row matches on
    /// `(user_id, persona, workload_tag, kind, trigger_signal, action)`,
    /// increment its `hit_count` and refresh `updated_at`; otherwise insert.
    /// Returns the persisted row.
    async fn record(&self, new: NewLesson) -> Result<Lesson, sqlx::Error>;

    /// Load up to `limit` most-recently-updated lessons for
    /// `(user_id, persona, workload_tag)`. When `workload_tag` is `None`
    /// the query matches rows where `workload_tag IS NULL` *and* rows whose
    /// stored tag is `None` — general lessons. When `workload_tag` is
    /// `Some(x)`, the query returns rows with exactly that tag *plus* any
    /// general lessons (NULL), so the caller gets both workload-specific and
    /// broadly applicable knowledge in one pass.
    async fn load_recent(
        &self,
        user_id: &str,
        persona: &str,
        workload_tag: Option<&str>,
        limit: u32,
    ) -> Result<Vec<Lesson>, sqlx::Error>;

    /// Increment a lesson's `hit_count` — used when the caller adopts the
    /// lesson in a new session. Returns the updated hit count.
    async fn record_hit(&self, lesson_id: &str) -> Result<i64, sqlx::Error>;

    /// Delete rows whose `updated_at` is older than `max_age_days`. Returns
    /// the deleted row count.
    async fn prune(&self, user_id: &str, max_age_days: u32) -> Result<u64, sqlx::Error>;
}

// ── DB implementation ───────────────────────────────────────────────────────

pub struct DatabaseAgentLessonsService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseAgentLessonsService {
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

    async fn get_pool(&self) -> Result<Pool<MySql>, sqlx::Error> {
        if let Some(ref p) = self.pool {
            return Ok(p.get().clone());
        }
        connect_matrixone(&self.matrixone).await
    }
}

#[async_trait]
impl AgentLessonsService for DatabaseAgentLessonsService {
    async fn record(&self, new: NewLesson) -> Result<Lesson, sqlx::Error> {
        new.validate()
            .map_err(|e| sqlx::Error::Protocol(format!("NewLesson::validate: {e}")))?;
        let pool = self.get_pool().await?;

        // Upsert-by-content: look for a matching row first.
        let existing = query(
            "SELECT id FROM agent_lessons \
             WHERE user_id = ? AND persona = ? \
               AND ((workload_tag IS NULL AND ? IS NULL) OR workload_tag = ?) \
               AND kind = ? AND trigger_signal = ? AND action = ? \
             LIMIT 1",
        )
        .bind(&new.user_id)
        .bind(&new.persona)
        .bind(&new.workload_tag)
        .bind(&new.workload_tag)
        .bind(new.kind.as_str())
        .bind(&new.trigger_signal)
        .bind(&new.action)
        .fetch_optional(&pool)
        .await?;

        if let Some(row) = existing {
            let id: String = row.try_get("id")?;
            query(
                "UPDATE agent_lessons \
                 SET hit_count = hit_count + 1, updated_at = CURRENT_TIMESTAMP(6) \
                 WHERE id = ?",
            )
            .bind(&id)
            .execute(&pool)
            .await?;
            return fetch_by_id(&pool, &id).await;
        }

        let id = Uuid::new_v4().to_string();
        let confidence = new.confidence.unwrap_or(DEFAULT_LESSON_CONFIDENCE);
        query(
            "INSERT INTO agent_lessons \
                 (id, user_id, persona, workload_tag, kind, trigger_signal, action, confidence) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&new.user_id)
        .bind(&new.persona)
        .bind(&new.workload_tag)
        .bind(new.kind.as_str())
        .bind(&new.trigger_signal)
        .bind(&new.action)
        .bind(confidence)
        .execute(&pool)
        .await?;

        fetch_by_id(&pool, &id).await
    }

    async fn load_recent(
        &self,
        user_id: &str,
        persona: &str,
        workload_tag: Option<&str>,
        limit: u32,
    ) -> Result<Vec<Lesson>, sqlx::Error> {
        let pool = self.get_pool().await?;

        // Tag semantics: `None` → only general (NULL) lessons.
        // `Some(x)` → workload-specific (= x) OR general (NULL).
        let rows = match workload_tag {
            None => {
                query(
                    "SELECT id, user_id, persona, workload_tag, kind, trigger_signal, action, \
                            confidence, hit_count, \
                            CAST(created_at AS CHAR) AS created_at, \
                            CAST(updated_at AS CHAR) AS updated_at \
                     FROM agent_lessons \
                     WHERE user_id = ? AND persona = ? AND workload_tag IS NULL \
                     ORDER BY updated_at DESC \
                     LIMIT ?",
                )
                .bind(user_id)
                .bind(persona)
                .bind(i64::from(limit))
                .fetch_all(&pool)
                .await?
            }
            Some(tag) => {
                query(
                    "SELECT id, user_id, persona, workload_tag, kind, trigger_signal, action, \
                            confidence, hit_count, \
                            CAST(created_at AS CHAR) AS created_at, \
                            CAST(updated_at AS CHAR) AS updated_at \
                     FROM agent_lessons \
                     WHERE user_id = ? AND persona = ? \
                       AND (workload_tag = ? OR workload_tag IS NULL) \
                     ORDER BY updated_at DESC \
                     LIMIT ?",
                )
                .bind(user_id)
                .bind(persona)
                .bind(tag)
                .bind(i64::from(limit))
                .fetch_all(&pool)
                .await?
            }
        };

        rows.into_iter().map(row_to_lesson).collect()
    }

    async fn record_hit(&self, lesson_id: &str) -> Result<i64, sqlx::Error> {
        let pool = self.get_pool().await?;
        query(
            "UPDATE agent_lessons \
             SET hit_count = hit_count + 1, updated_at = CURRENT_TIMESTAMP(6) \
             WHERE id = ?",
        )
        .bind(lesson_id)
        .execute(&pool)
        .await?;
        let row = query("SELECT hit_count FROM agent_lessons WHERE id = ?")
            .bind(lesson_id)
            .fetch_one(&pool)
            .await?;
        row.try_get::<i64, _>("hit_count")
    }

    async fn prune(&self, user_id: &str, max_age_days: u32) -> Result<u64, sqlx::Error> {
        let pool = self.get_pool().await?;
        // Interval arithmetic via DATE_SUB — portable across MySQL/MatrixOne.
        let res = query(
            "DELETE FROM agent_lessons \
             WHERE user_id = ? AND updated_at < DATE_SUB(CURRENT_TIMESTAMP(6), INTERVAL ? DAY)",
        )
        .bind(user_id)
        .bind(i64::from(max_age_days))
        .execute(&pool)
        .await?;
        Ok(res.rows_affected())
    }
}

async fn fetch_by_id(pool: &Pool<MySql>, id: &str) -> Result<Lesson, sqlx::Error> {
    let row = query(
        "SELECT id, user_id, persona, workload_tag, kind, trigger_signal, action, \
                confidence, hit_count, \
                CAST(created_at AS CHAR) AS created_at, \
                CAST(updated_at AS CHAR) AS updated_at \
         FROM agent_lessons WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await?;
    row_to_lesson(row)
}

fn row_to_lesson(row: sqlx::mysql::MySqlRow) -> Result<Lesson, sqlx::Error> {
    let kind_s: String = row.try_get("kind")?;
    let kind = LessonKind::parse_tag(&kind_s)
        .ok_or_else(|| sqlx::Error::Decode(format!("unknown LessonKind tag: {kind_s}").into()))?;
    let created_s: String = row.try_get("created_at")?;
    let updated_s: String = row.try_get("updated_at")?;
    Ok(Lesson {
        id: row.try_get("id")?,
        user_id: row.try_get("user_id")?,
        persona: row.try_get("persona")?,
        workload_tag: row.try_get("workload_tag").ok(),
        kind,
        trigger_signal: row.try_get("trigger_signal")?,
        action: row.try_get("action")?,
        confidence: row.try_get("confidence")?,
        hit_count: row.try_get("hit_count")?,
        created_at: parse_mysql_datetime(&created_s)?,
        updated_at: parse_mysql_datetime(&updated_s)?,
    })
}

fn parse_mysql_datetime(s: &str) -> Result<DateTime<Utc>, sqlx::Error> {
    // MatrixOne renders DATETIME(6) as "YYYY-MM-DD HH:MM:SS.ffffff".
    let trimmed = s.trim();
    chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S"))
        .map(|ndt| ndt.and_utc())
        .map_err(|e| sqlx::Error::Decode(format!("parse_mysql_datetime({trimmed:?}): {e}").into()))
}

// ── Unconfigured fallback ───────────────────────────────────────────────────

pub struct UnconfiguredAgentLessonsService;

#[async_trait]
impl AgentLessonsService for UnconfiguredAgentLessonsService {
    async fn record(&self, _: NewLesson) -> Result<Lesson, sqlx::Error> {
        Err(sqlx::Error::Configuration(
            "agent lessons service not configured".into(),
        ))
    }

    async fn load_recent(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: u32,
    ) -> Result<Vec<Lesson>, sqlx::Error> {
        Ok(Vec::new())
    }

    async fn record_hit(&self, _: &str) -> Result<i64, sqlx::Error> {
        Err(sqlx::Error::Configuration(
            "agent lessons service not configured".into(),
        ))
    }

    async fn prune(&self, _: &str, _: u32) -> Result<u64, sqlx::Error> {
        Ok(0)
    }
}

// ── DDL ─────────────────────────────────────────────────────────────────────

/// DDL for the `agent_lessons` table. Called from `ensure_core_schema`.
pub const AGENT_LESSONS_DDL: &str = "CREATE TABLE IF NOT EXISTS agent_lessons (
    id VARCHAR(64) PRIMARY KEY,
    user_id VARCHAR(64) NOT NULL,
    persona VARCHAR(64) NOT NULL,
    workload_tag VARCHAR(64) NULL,
    kind VARCHAR(32) NOT NULL,
    trigger_signal VARCHAR(255) NOT NULL,
    action TEXT NOT NULL,
    confidence DOUBLE NOT NULL DEFAULT 0.6,
    hit_count BIGINT NOT NULL DEFAULT 0,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    INDEX idx_agent_lessons_scope (user_id, persona, workload_tag, updated_at),
    INDEX idx_agent_lessons_user_created (user_id, created_at)
)";

// ── Tests (pure logic) ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_new() -> NewLesson {
        NewLesson {
            user_id: "u1".into(),
            persona: "generic".into(),
            workload_tag: None,
            kind: LessonKind::ToolDeprioritize,
            trigger_signal: "3 consecutive stalls on grep".into(),
            action: "deprioritize grep for regex-heavy tasks".into(),
            confidence: Some(0.7),
        }
    }

    #[test]
    fn lesson_kind_tag_roundtrip() {
        for k in [
            LessonKind::ToolDeprioritize,
            LessonKind::ToolBoost,
            LessonKind::PromptShape,
            LessonKind::PostconditionPattern,
            LessonKind::ErrorRecovery,
        ] {
            assert_eq!(LessonKind::parse_tag(k.as_str()), Some(k));
        }
        assert!(LessonKind::parse_tag("unknown_kind").is_none());
    }

    #[test]
    fn validate_accepts_minimal_input() {
        assert!(valid_new().validate().is_ok());
    }

    #[test]
    fn validate_rejects_empty_required_fields() {
        for mutate in [
            |n: &mut NewLesson| n.user_id.clear(),
            |n: &mut NewLesson| n.persona.clear(),
            |n: &mut NewLesson| n.trigger_signal.clear(),
            |n: &mut NewLesson| n.action.clear(),
        ] {
            let mut n = valid_new();
            mutate(&mut n);
            assert!(n.validate().is_err(), "expected rejection");
        }
    }

    #[test]
    fn validate_rejects_oversized_fields() {
        let mut n = valid_new();
        n.trigger_signal = "x".repeat(MAX_TRIGGER_SIGNAL_LEN + 1);
        assert!(n.validate().is_err());

        let mut n = valid_new();
        n.action = "y".repeat(MAX_ACTION_LEN + 1);
        assert!(n.validate().is_err());
    }

    #[test]
    fn validate_rejects_confidence_out_of_range() {
        let mut n = valid_new();
        n.confidence = Some(1.5);
        assert!(n.validate().is_err());

        let mut n = valid_new();
        n.confidence = Some(-0.1);
        assert!(n.validate().is_err());

        let mut n = valid_new();
        n.confidence = Some(f64::NAN);
        assert!(n.validate().is_err());
    }

    #[test]
    fn validate_confidence_none_is_allowed() {
        let mut n = valid_new();
        n.confidence = None;
        assert!(n.validate().is_ok());
    }

    #[test]
    fn ddl_defines_required_columns_and_indexes() {
        // Pin the DDL's key surface so accidental drift shows up in a unit
        // test, not in production.
        for required in [
            "agent_lessons",
            "id VARCHAR(64) PRIMARY KEY",
            "user_id VARCHAR(64) NOT NULL",
            "persona VARCHAR(64) NOT NULL",
            "workload_tag VARCHAR(64) NULL",
            "kind VARCHAR(32) NOT NULL",
            "trigger_signal VARCHAR(255) NOT NULL",
            "action TEXT NOT NULL",
            "confidence DOUBLE",
            "hit_count BIGINT",
            "created_at DATETIME(6)",
            "updated_at DATETIME(6)",
            "idx_agent_lessons_scope",
            "idx_agent_lessons_user_created",
        ] {
            assert!(
                AGENT_LESSONS_DDL.contains(required),
                "DDL missing: {required}"
            );
        }
    }

    #[test]
    fn unconfigured_service_load_returns_empty() {
        // Non-DB codepath must be safe to call — the agent should not panic
        // just because cloud storage is unavailable.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let svc = UnconfiguredAgentLessonsService;
            let out = svc.load_recent("u", "p", None, 10).await.unwrap();
            assert!(out.is_empty());
            let pruned = svc.prune("u", 30).await.unwrap();
            assert_eq!(pruned, 0);
        });
    }

    #[test]
    fn parse_mysql_datetime_accepts_matrixone_format() {
        let with_frac = parse_mysql_datetime("2026-05-01 12:34:56.123456").unwrap();
        assert_eq!(with_frac.format("%Y-%m-%d").to_string(), "2026-05-01");

        let without_frac = parse_mysql_datetime("2026-05-01 12:34:56").unwrap();
        assert_eq!(without_frac.format("%H:%M:%S").to_string(), "12:34:56");

        assert!(parse_mysql_datetime("not a date").is_err());
    }
}
