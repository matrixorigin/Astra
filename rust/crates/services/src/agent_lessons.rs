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
//!   `(user_id, persona, workload_tag, kind, trigger_signal)` already
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
#[non_exhaustive]
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
    /// A positive pattern learned from successful outcomes — the agent
    /// discovered something that works well for this user/project.
    SkillAcquired,
}

impl std::fmt::Display for LessonKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
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
            Self::SkillAcquired => "skill_acquired",
        }
    }

    pub fn parse_tag(tag: &str) -> Option<Self> {
        match tag {
            "tool_deprioritize" => Some(Self::ToolDeprioritize),
            "tool_boost" => Some(Self::ToolBoost),
            "prompt_shape" => Some(Self::PromptShape),
            "postcondition_pattern" => Some(Self::PostconditionPattern),
            "error_recovery" => Some(Self::ErrorRecovery),
            "skill_acquired" => Some(Self::SkillAcquired),
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

/// Prompt-bound projection of a persisted [`Lesson`].
///
/// Intentionally drops `id`, `confidence`, `hit_count`, and timestamps —
/// the LLM should read the *advice*, not the metadata. Callers that need
/// to track adoption (for `record_hit`) keep the `id` out-of-band.
///
/// Canonical home: this crate (next to [`Lesson`]). Runtime re-exports it
/// for backwards-compat so existing code that imports from `self_model`
/// continues to compile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LessonHint {
    pub kind: LessonKind,
    pub trigger_signal: String,
    /// Full action text — used when prompt space permits.
    pub action: String,
    /// Short summary (~15 tokens) for compact rendering under prompt
    /// pressure. Inspired by Memoria V2's abstract/overview/detail model.
    /// When `None`, the renderer falls back to `action`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload_tag: Option<String>,
}

impl LessonHint {
    #[must_use]
    pub fn from_lesson(l: &Lesson) -> Self {
        let action = sanitize_for_prompt(&l.action);
        let compact = make_compact(&action);
        Self {
            kind: l.kind,
            trigger_signal: sanitize_for_prompt(&l.trigger_signal),
            action,
            compact,
            workload_tag: l.workload_tag.clone(),
        }
    }
}

/// Generate a compact summary (~60 chars) from a full action string.
/// Returns `None` if the action is already short enough.
fn make_compact(action: &str) -> Option<String> {
    if action.len() <= 80 {
        return None;
    }
    let first_sentence = action
        .split_once(['.', '—', ';', '\n'])
        .map(|(s, _)| s.trim())
        .unwrap_or(action);
    if first_sentence.len() >= action.len() - 5 {
        return None;
    }
    Some(first_sentence.to_string())
}

/// Strip control characters, zero-width Unicode, and bidirectional
/// overrides from content before prompt injection. Covers:
/// - C0/C1 control codes (is_control) except newline
/// - Zero-width spaces/joiners (U+200B–U+200F)
/// - Bidi overrides and isolates (U+2028–U+202F)
/// - Word joiners and invisible separators (U+2060–U+2064)
/// - BOM (U+FEFF)
///
/// Public so `SkillDiagnosis::render_prompt_block` can reuse it for
/// LLM-generated findings/headlines.
pub fn sanitize_for_prompt(s: &str) -> String {
    s.chars()
        .filter(|c| {
            if c.is_control() && *c != '\n' {
                return false;
            }
            !is_invisible_unicode(*c)
        })
        .collect()
}

/// Comprehensive invisible/deceptive Unicode character filter.
fn is_invisible_unicode(c: char) -> bool {
    matches!(
        c,
        // Zero-width spaces and joiners
        '\u{200B}'..='\u{200F}'
        // Line/paragraph separators + bidi overrides/isolates
        | '\u{2028}'..='\u{202F}'
        // Word joiners and invisible operators
        | '\u{2060}'..='\u{2064}'
        // BOM
        | '\u{FEFF}'
        // Soft hyphen (renders as nothing unless line break)
        | '\u{00AD}'
        // Combining grapheme joiner
        | '\u{034F}'
        // Arabic letter mark
        | '\u{061C}'
        // Hangul fillers
        | '\u{115F}' | '\u{1160}' | '\u{3164}' | '\u{FFA0}'
        // Khmer vowel inherent
        | '\u{17B4}' | '\u{17B5}'
        // Mongolian vowel separator
        | '\u{180E}'
        // Tag characters (U+E0001–U+E007F)
        | '\u{E0001}'..='\u{E007F}'
    )
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

/// A lesson was shown to a session. Exposure is separate from usefulness:
/// loading a lesson is not counted as success until an outcome is recorded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LessonExposure {
    pub lesson_id: String,
    pub session_id: String,
    pub user_id: String,
    pub persona: String,
    pub workload_tag: Option<String>,
    pub adopted: bool,
}

/// Session-end outcome attached to all unresolved exposures in a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LessonOutcome {
    pub session_id: String,
    pub user_id: String,
    pub stall_events: u32,
    pub user_corrections: u32,
    pub tool_failures: u32,
    pub unmet_postconditions: u32,
    pub diagnosis_criteria_met: u32,
    pub diagnosis_criteria_failed: u32,
}

/// Default confidence score for a freshly-recorded lesson.
pub const DEFAULT_LESSON_CONFIDENCE: f64 = 0.6;
/// Max `trigger_signal` length, enforced by `NewLesson::validate`.
pub const MAX_TRIGGER_SIGNAL_LEN: usize = 255;
/// Max `action` length.
pub const MAX_ACTION_LEN: usize = 1024;
/// Maximum active lessons kept per user after prune. Prevents unbounded
/// growth from `trigger_signal` text variation ("3 failures" vs "5 failures").
pub const MAX_LESSONS_PER_USER: u32 = 100;

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
        if self.trigger_signal.len() > MAX_TRIGGER_SIGNAL_LEN {
            return Err("trigger_signal exceeds MAX_TRIGGER_SIGNAL_LEN");
        }
        if self.action.is_empty() {
            return Err("action must not be empty");
        }
        if self.action.len() > MAX_ACTION_LEN {
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
    /// `(user_id, persona, workload_tag, kind, trigger_signal)`,
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

    /// Record that a lesson was exposed to a session. Does not imply the
    /// lesson helped.
    async fn record_exposure(&self, exposure: LessonExposure) -> Result<(), sqlx::Error> {
        let _ = exposure;
        Ok(())
    }

    /// Record session outcome for exposed lessons and update confidence.
    async fn record_outcome(&self, outcome: LessonOutcome) -> Result<u64, sqlx::Error> {
        let _ = outcome;
        Ok(0)
    }
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

    /// Pool-only convenience constructor for callers that already hold a
    /// live [`SharedPool`] and don't want to thread [`MatrixOneSettings`]
    /// around solely for the fallback reconnect path. The fallback is
    /// unreachable while `pool` stays healthy, so the placeholder settings
    /// are never actually consumed.
    #[must_use]
    pub fn from_pool(pool: SharedPool) -> Self {
        Self {
            matrixone: MatrixOneSettings {
                host: String::new(),
                port: 0,
                user: String::new(),
                password: String::new(),
                database: String::new(),
            },
            pool: Some(pool),
        }
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

        let workload_key = new.workload_tag.as_deref().unwrap_or("");

        // SELECT-first upsert. MatrixOne does not support ON DUPLICATE KEY
        // UPDATE or REPLACE INTO for non-PK UNIQUE constraints (both return
        // error 1062). The UK still exists as a data-integrity safety net
        // and an index for the UPDATE/SELECT WHERE clause.
        let existing = query(
            "SELECT id FROM agent_lessons \
             WHERE user_id = ? AND persona = ? AND workload_key = ? \
               AND kind = ? AND trigger_signal = ? \
             LIMIT 1",
        )
        .bind(&new.user_id)
        .bind(&new.persona)
        .bind(workload_key)
        .bind(new.kind.as_str())
        .bind(&new.trigger_signal)
        .fetch_optional(&pool)
        .await?;

        if existing.is_some() {
            query(
                "UPDATE agent_lessons \
                 SET hit_count = hit_count + 1, \
                     action = ?, \
                     updated_at = CURRENT_TIMESTAMP(6) \
                 WHERE user_id = ? AND persona = ? AND workload_key = ? \
                   AND kind = ? AND trigger_signal = ?",
            )
            .bind(&new.action)
            .bind(&new.user_id)
            .bind(&new.persona)
            .bind(workload_key)
            .bind(new.kind.as_str())
            .bind(&new.trigger_signal)
            .execute(&pool)
            .await?;
        } else {
            let confidence = new.confidence.unwrap_or(DEFAULT_LESSON_CONFIDENCE);
            let insert_result = query(
                "INSERT INTO agent_lessons \
                     (id, user_id, persona, workload_tag, workload_key, kind, \
                      trigger_signal, action, confidence) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(Uuid::new_v4().to_string())
            .bind(&new.user_id)
            .bind(&new.persona)
            .bind(&new.workload_tag)
            .bind(workload_key)
            .bind(new.kind.as_str())
            .bind(&new.trigger_signal)
            .bind(&new.action)
            .bind(confidence)
            .execute(&pool)
            .await;
            if let Err(e) = insert_result {
                if e.as_database_error()
                    .is_some_and(|db| db.message().contains("Duplicate entry"))
                {
                    // TOCTOU race: another writer inserted between our
                    // SELECT and INSERT. The row exists now — fall through
                    // to fetch_by_content which will find it.
                } else {
                    return Err(e);
                }
            }
        }

        fetch_by_content(&pool, &new).await
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
                       AND status = 'active' \
                      ORDER BY confidence DESC, updated_at DESC, id ASC \
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
                        AND status = 'active' \
                      ORDER BY CASE WHEN workload_tag = ? THEN 0 ELSE 1 END, \
                               confidence DESC, updated_at DESC, id ASC \
                      LIMIT ?",
                )
                .bind(user_id)
                .bind(persona)
                .bind(tag)
                .bind(tag)
                .bind(i64::from(limit))
                .fetch_all(&pool)
                .await?
            }
        };

        let mut lessons = Vec::with_capacity(rows.len());
        for row in rows {
            match row_to_lesson(row) {
                Ok(l) => lessons.push(l),
                Err(e) => {
                    tracing::warn!(
                        target: "agent_lessons",
                        error = %e,
                        "skipping lesson row with unrecognised kind; \
                         binary may be older than the DB schema",
                    );
                }
            }
        }
        Ok(lessons)
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
        // All three prune steps run in a single transaction so callers
        // never see a partial-prune intermediate state and the returned
        // rows_affected count is semantically strong.
        let mut tx = pool.begin().await?;

        let retired = query(
            "UPDATE agent_lessons \
             SET status = 'retired', updated_at = CURRENT_TIMESTAMP(6) \
             WHERE user_id = ? AND status = 'active' \
               AND negative_outcome_count >= 5 \
               AND negative_outcome_count > positive_outcome_count",
        )
        .bind(user_id)
        .execute(&mut *tx)
        .await?;

        // Delete all retired rows immediately. Without this, the
        // retirement UPDATE refreshes updated_at, so the age-based
        // DELETE below would let retired rows survive another 30 days.
        let retired_deleted =
            query("DELETE FROM agent_lessons WHERE user_id = ? AND status = 'retired'")
                .bind(user_id)
                .execute(&mut *tx)
                .await?;

        // Tool-specific lessons (ToolDeprioritize/ToolBoost) get a shorter
        // TTL (7 days) because tool issues are often transient and a stale
        // "avoid grep" from a week-old resource-limit event can cripple the
        // agent. General lessons use the caller's max_age_days (typically 30).
        let tool_stale = query(
            "DELETE FROM agent_lessons \
             WHERE user_id = ? AND kind IN ('tool_deprioritize', 'tool_boost') \
               AND updated_at < DATE_SUB(CURRENT_TIMESTAMP(6), INTERVAL 7 DAY)",
        )
        .bind(user_id)
        .execute(&mut *tx)
        .await?;

        let stale = query(
            "DELETE FROM agent_lessons \
             WHERE user_id = ? AND updated_at < DATE_SUB(CURRENT_TIMESTAMP(6), INTERVAL ? DAY)",
        )
        .bind(user_id)
        .bind(i64::from(max_age_days))
        .execute(&mut *tx)
        .await?;

        // Row ceiling: keep at most MAX_LESSONS_PER_USER active rows per
        // user to prevent unbounded growth from trigger_signal text
        // variation. Deletes the oldest active rows first.
        let overflow = query(
            "DELETE FROM agent_lessons \
             WHERE user_id = ? AND status = 'active' AND id NOT IN ( \
               SELECT id FROM ( \
                 SELECT id FROM agent_lessons \
                 WHERE user_id = ? AND status = 'active' \
                 ORDER BY updated_at DESC LIMIT ? \
               ) AS keep \
             )",
        )
        .bind(user_id)
        .bind(user_id)
        .bind(i64::from(MAX_LESSONS_PER_USER))
        .execute(&mut *tx)
        .await?;

        // Exposure retention: delete exposure rows older than max_age_days.
        // Without this, the exposure table grows unboundedly.
        let _exposure_stale = query(
            "DELETE FROM agent_lesson_exposures \
             WHERE user_id = ? AND exposed_at < DATE_SUB(CURRENT_TIMESTAMP(6), INTERVAL ? DAY)",
        )
        .bind(user_id)
        .bind(i64::from(max_age_days))
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(retired.rows_affected()
            + retired_deleted.rows_affected()
            + tool_stale.rows_affected()
            + stale.rows_affected()
            + overflow.rows_affected())
    }

    async fn record_exposure(&self, exposure: LessonExposure) -> Result<(), sqlx::Error> {
        if exposure.lesson_id.is_empty()
            || exposure.session_id.is_empty()
            || exposure.user_id.is_empty()
            || exposure.persona.is_empty()
        {
            return Err(sqlx::Error::Protocol(
                "LessonExposure required fields must not be empty".into(),
            ));
        }
        let pool = self.get_pool().await?;
        let existing = query(
            "SELECT id FROM agent_lesson_exposures \
             WHERE lesson_id = ? AND session_id = ? LIMIT 1",
        )
        .bind(&exposure.lesson_id)
        .bind(&exposure.session_id)
        .fetch_optional(&pool)
        .await?;

        if existing.is_some() {
            if exposure.adopted {
                query(
                    "UPDATE agent_lesson_exposures \
                     SET adopted = 1 \
                     WHERE lesson_id = ? AND session_id = ?",
                )
                .bind(&exposure.lesson_id)
                .bind(&exposure.session_id)
                .execute(&pool)
                .await?;
            }
        } else {
            let insert_result = query(
                "INSERT INTO agent_lesson_exposures \
                     (id, lesson_id, session_id, user_id, persona, workload_tag, adopted) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(Uuid::new_v4().to_string())
            .bind(&exposure.lesson_id)
            .bind(&exposure.session_id)
            .bind(&exposure.user_id)
            .bind(&exposure.persona)
            .bind(&exposure.workload_tag)
            .bind(exposure.adopted as i8)
            .execute(&pool)
            .await;
            if let Err(e) = insert_result {
                if e.as_database_error()
                    .is_some_and(|db| db.message().contains("Duplicate entry"))
                {
                    // TOCTOU race — row was inserted between SELECT and INSERT.
                    // Silently succeed: exposure already recorded.
                } else {
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    async fn record_outcome(&self, outcome: LessonOutcome) -> Result<u64, sqlx::Error> {
        if outcome.session_id.is_empty() || outcome.user_id.is_empty() {
            return Err(sqlx::Error::Protocol(
                "LessonOutcome session_id/user_id must not be empty".into(),
            ));
        }
        let pool = self.get_pool().await?;
        let mut tx = pool.begin().await?;

        let confidence_delta = compute_confidence_delta(&outcome);
        let (pos_inc, neg_inc): (i64, i64) = if confidence_delta >= 0.0 {
            (1, 0)
        } else {
            (0, 1)
        };

        // Read unprocessed exposures. The outcome_recorded_at UPDATE below
        // acts as an optimistic lock: if two transactions race, the second
        // one's final UPDATE affects 0 rows (already timestamped) — the
        // confidence UPDATEs are idempotent for the same delta, and the
        // exposure marking is a no-op. FOR UPDATE is deliberately avoided
        // because MatrixOne's locking can exhaust the connection pool under
        // sequential test runs with shared pools.
        let locked_rows = query(
            "SELECT lesson_id, adopted FROM agent_lesson_exposures \
             WHERE session_id = ? AND user_id = ? AND outcome_recorded_at IS NULL",
        )
        .bind(&outcome.session_id)
        .bind(&outcome.user_id)
        .fetch_all(&mut *tx)
        .await?;

        if locked_rows.is_empty() {
            tx.commit().await?;
            return Ok(0);
        }

        // Split by adoption status: adopted lessons get full confidence
        // delta, non-adopted get half — they were exposed but the agent
        // didn't act on them, so the outcome signal is weaker.
        let mut adopted_ids = Vec::new();
        let mut passive_ids = Vec::new();
        for row in &locked_rows {
            let id: String = row.try_get("lesson_id")?;
            let adopted = row
                .try_get::<bool, _>("adopted")
                .or_else(|_| row.try_get::<i8, _>("adopted").map(|v| v != 0))
                .or_else(|_| {
                    // MatrixOne returns BOOLEAN as VARCHAR in some contexts
                    // (e.g. SELECT ... FOR UPDATE).
                    row.try_get::<String, _>("adopted")
                        .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
                })
                .unwrap_or(false);
            if adopted {
                adopted_ids.push(id);
            } else {
                passive_ids.push(id);
            }
        }

        let mut total_updated = 0u64;
        for (ids, delta_scale) in [(&adopted_ids, 1.0), (&passive_ids, 0.5)] {
            if ids.is_empty() {
                continue;
            }
            let scaled_delta = confidence_delta * delta_scale;
            let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");

            let update_sql = format!(
                "UPDATE agent_lessons \
                 SET positive_outcome_count = positive_outcome_count + ?, \
                     negative_outcome_count = negative_outcome_count + ?, \
                     confidence = CASE \
                         WHEN confidence + ? > 0.95 THEN 0.95 \
                         WHEN confidence + ? < 0.1 THEN 0.1 \
                         ELSE confidence + ? END, \
                     updated_at = CURRENT_TIMESTAMP(6) \
                 WHERE id IN ({placeholders})"
            );
            let mut q = query(&update_sql)
                .bind(pos_inc)
                .bind(neg_inc)
                .bind(scaled_delta)
                .bind(scaled_delta)
                .bind(scaled_delta);
            for id in ids {
                q = q.bind(id);
            }
            total_updated += q.execute(&mut *tx).await?.rows_affected();
        }

        // Retirement pass: reads the already-updated counters (visible
        // within the same transaction).
        let all_ids: Vec<&str> = adopted_ids
            .iter()
            .chain(passive_ids.iter())
            .map(String::as_str)
            .collect();
        let all_placeholders = all_ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let retire_sql = format!(
            "UPDATE agent_lessons \
             SET status = 'retired', updated_at = CURRENT_TIMESTAMP(6) \
             WHERE status = 'active' \
               AND negative_outcome_count >= 5 \
               AND negative_outcome_count > positive_outcome_count \
               AND id IN ({all_placeholders})"
        );
        let mut q = query(&retire_sql);
        for id in &all_ids {
            q = q.bind(id);
        }
        q.execute(&mut *tx).await?;

        // Mark exposures as recorded so retry won't double-apply deltas.
        query(
            "UPDATE agent_lesson_exposures \
             SET outcome_recorded_at = CURRENT_TIMESTAMP(6), \
                 stall_events = ?, user_corrections = ?, tool_failures = ?, \
                 unmet_postconditions = ?, diagnosis_criteria_met = ?, \
                 diagnosis_criteria_failed = ? \
             WHERE session_id = ? AND user_id = ? AND outcome_recorded_at IS NULL",
        )
        .bind(i64::from(outcome.stall_events))
        .bind(i64::from(outcome.user_corrections))
        .bind(i64::from(outcome.tool_failures))
        .bind(i64::from(outcome.unmet_postconditions))
        .bind(i64::from(outcome.diagnosis_criteria_met))
        .bind(i64::from(outcome.diagnosis_criteria_failed))
        .bind(&outcome.session_id)
        .bind(&outcome.user_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(total_updated)
    }
}

/// Fetch a lesson by its UNIQUE KEY columns. The query matches exactly
/// the UNIQUE KEY `(user_id, persona, workload_key, kind, trigger_signal)`
/// — `action` is deliberately excluded because the UK doesn't include it
/// and the action text may change between upserts.
async fn fetch_by_content(pool: &Pool<MySql>, new: &NewLesson) -> Result<Lesson, sqlx::Error> {
    let row = query(
        "SELECT id, user_id, persona, workload_tag, kind, trigger_signal, action, \
                confidence, hit_count, \
                CAST(created_at AS CHAR) AS created_at, \
                CAST(updated_at AS CHAR) AS updated_at \
         FROM agent_lessons \
         WHERE user_id = ? AND persona = ? AND workload_key = ? \
           AND kind = ? AND trigger_signal = ? \
         LIMIT 1",
    )
    .bind(&new.user_id)
    .bind(&new.persona)
    .bind(new.workload_tag.as_deref().unwrap_or(""))
    .bind(new.kind.as_str())
    .bind(&new.trigger_signal)
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
    workload_key VARCHAR(64) NOT NULL DEFAULT '',
    kind VARCHAR(32) NOT NULL,
    trigger_signal VARCHAR(255) NOT NULL,
    action VARCHAR(1024) NOT NULL,
    confidence DOUBLE NOT NULL DEFAULT 0.6,
    hit_count BIGINT NOT NULL DEFAULT 0,
    status VARCHAR(16) NOT NULL DEFAULT 'active',
    positive_outcome_count BIGINT NOT NULL DEFAULT 0,
    negative_outcome_count BIGINT NOT NULL DEFAULT 0,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    INDEX idx_agent_lessons_scope (user_id, persona, workload_tag, updated_at),
    INDEX idx_agent_lessons_user_updated (user_id, updated_at),
    INDEX idx_agent_lessons_status (status),
    UNIQUE KEY uniq_agent_lesson_content \
        (user_id, persona, workload_key, kind, trigger_signal)
)";

pub const AGENT_LESSON_EXPOSURES_DDL: &str = "CREATE TABLE IF NOT EXISTS agent_lesson_exposures (
    id VARCHAR(64) PRIMARY KEY,
    lesson_id VARCHAR(64) NOT NULL,
    session_id VARCHAR(64) NOT NULL,
    user_id VARCHAR(64) NOT NULL,
    persona VARCHAR(64) NOT NULL,
    workload_tag VARCHAR(64) NULL,
    adopted BOOLEAN NOT NULL DEFAULT FALSE,
    exposed_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    outcome_recorded_at DATETIME(6) NULL,
    stall_events INT NOT NULL DEFAULT 0,
    user_corrections INT NOT NULL DEFAULT 0,
    tool_failures INT NOT NULL DEFAULT 0,
    unmet_postconditions INT NOT NULL DEFAULT 0,
    diagnosis_criteria_met INT NOT NULL DEFAULT 0,
    diagnosis_criteria_failed INT NOT NULL DEFAULT 0,
    UNIQUE KEY uniq_lesson_session_exposure (lesson_id, session_id),
    INDEX idx_lesson_exposure_session (session_id, user_id),
    INDEX idx_lesson_exposure_lesson (lesson_id)
)";

// ── Confidence scoring ──────────────────────────────────────────────────────

/// Weight for diagnosis-specific criteria (direct evidence of THIS
/// lesson's effectiveness). Higher than noise because diagnosis criteria
/// are machine-checked postconditions, not ambient session signals.
const DIAGNOSIS_WEIGHT: f64 = 3.0;

/// Weight for session-wide noise signals (stalls, corrections, tool
/// failures, unmet postconditions). These are correlative — a stall
/// might have nothing to do with this lesson — so they contribute less.
const NOISE_WEIGHT: f64 = 1.0;

/// Maximum absolute confidence change per outcome. Keeps the score from
/// swinging wildly on a single session.
const MAX_CONFIDENCE_STEP: f64 = 0.1;

/// Compute a bounded confidence delta from a session outcome.
///
/// Positive → lesson helped (confidence goes up).
/// Negative → lesson didn't help (confidence goes down).
/// Zero → ambiguous session (no confidence change).
///
/// The weighting ensures diagnosis-specific criteria (met/failed) dominate
/// over ambient session noise (stalls, corrections). This means a lesson
/// whose postcondition was explicitly verified as met will gain confidence
/// even if the session had some stalls elsewhere.
#[must_use]
pub fn compute_confidence_delta(outcome: &LessonOutcome) -> f64 {
    let diagnosis_score = outcome.diagnosis_criteria_met as f64 * DIAGNOSIS_WEIGHT
        - outcome.diagnosis_criteria_failed as f64 * DIAGNOSIS_WEIGHT;

    // Cast each field to f64 individually to avoid u32 addition overflow.
    let noise_negative = outcome.stall_events as f64
        + outcome.user_corrections as f64
        + outcome.tool_failures as f64
        + outcome.unmet_postconditions as f64;

    let total_weight = DIAGNOSIS_WEIGHT + NOISE_WEIGHT; // 4.0
    let raw = (diagnosis_score - noise_negative * NOISE_WEIGHT) / total_weight;

    // Bounded learning rate: scale raw score to a small step, clamped.
    (raw * 0.05).clamp(-MAX_CONFIDENCE_STEP, MAX_CONFIDENCE_STEP)
}

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
            LessonKind::SkillAcquired,
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
            "action VARCHAR(1024) NOT NULL",
            "confidence DOUBLE",
            "hit_count BIGINT",
            "status VARCHAR(16)",
            "positive_outcome_count BIGINT",
            "negative_outcome_count BIGINT",
            "uniq_agent_lesson_content",
            "created_at DATETIME(6)",
            "updated_at DATETIME(6)",
            "idx_agent_lessons_scope",
            "idx_agent_lessons_user_updated",
            "idx_agent_lessons_status",
        ] {
            assert!(
                AGENT_LESSONS_DDL.contains(required),
                "DDL missing: {required}"
            );
        }
    }

    #[test]
    fn exposure_ddl_defines_outcome_columns_and_unique_session_key() {
        for required in [
            "agent_lesson_exposures",
            "lesson_id VARCHAR(64) NOT NULL",
            "session_id VARCHAR(64) NOT NULL",
            "outcome_recorded_at DATETIME(6) NULL",
            "diagnosis_criteria_met INT",
            "diagnosis_criteria_failed INT",
            "uniq_lesson_session_exposure",
            "idx_lesson_exposure_session",
        ] {
            assert!(
                AGENT_LESSON_EXPOSURES_DDL.contains(required),
                "exposure DDL missing: {required}"
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
    fn from_pool_constructor_compiles_without_settings() {
        // Type-level smoke: `DatabaseAgentLessonsService::from_pool` is the
        // ergonomic seam for runtime callers that already hold a
        // SharedPool (e.g., via MatrixCloudRuntime). This test simply
        // asserts the function exists and returns the right type without
        // connecting to a DB.
        fn _accepts_service(_svc: DatabaseAgentLessonsService) {}
        // Compile-time check only — we don't actually call this function.
        #[allow(dead_code)]
        fn _caller(pool: SharedPool) {
            _accepts_service(DatabaseAgentLessonsService::from_pool(pool));
        }
    }

    #[test]
    fn parse_mysql_datetime_accepts_matrixone_format() {
        let with_frac = parse_mysql_datetime("2026-05-01 12:34:56.123456").unwrap();
        assert_eq!(with_frac.format("%Y-%m-%d").to_string(), "2026-05-01");

        let without_frac = parse_mysql_datetime("2026-05-01 12:34:56").unwrap();
        assert_eq!(without_frac.format("%H:%M:%S").to_string(), "12:34:56");

        assert!(parse_mysql_datetime("not a date").is_err());
    }

    // ── R4: weighted confidence scoring ─────────────────────────────────────

    fn outcome(
        criteria_met: u32,
        criteria_failed: u32,
        stalls: u32,
        corrections: u32,
        tool_failures: u32,
        unmet: u32,
    ) -> LessonOutcome {
        LessonOutcome {
            session_id: "s".into(),
            user_id: "u".into(),
            stall_events: stalls,
            user_corrections: corrections,
            tool_failures,
            unmet_postconditions: unmet,
            diagnosis_criteria_met: criteria_met,
            diagnosis_criteria_failed: criteria_failed,
        }
    }

    #[test]
    fn confidence_delta_positive_when_criteria_met_dominates() {
        // 2 criteria met, no noise → strong positive signal.
        let d = compute_confidence_delta(&outcome(2, 0, 0, 0, 0, 0));
        assert!(d > 0.0, "criteria_met should yield positive delta, got {d}");
    }

    #[test]
    fn confidence_delta_negative_when_criteria_failed_dominates() {
        // 0 met, 2 failed → strong negative signal.
        let d = compute_confidence_delta(&outcome(0, 2, 0, 0, 0, 0));
        assert!(
            d < 0.0,
            "criteria_failed should yield negative delta, got {d}"
        );
    }

    #[test]
    fn confidence_delta_diagnosis_outweighs_noise() {
        // 1 criterion met (weight 3) vs 2 stalls (weight 1 each).
        // diagnosis_score = 3.0, noise = -2.0
        // raw = (3.0 - 2.0) / 4.0 = 0.25 → clamped 0.05 * 0.25 = 0.0125
        let d = compute_confidence_delta(&outcome(1, 0, 2, 0, 0, 0));
        assert!(d > 0.0, "1 met criterion should outweigh 2 stalls, got {d}");
    }

    #[test]
    fn confidence_delta_pure_noise_is_negative() {
        // No criteria, just session noise → negative.
        let d = compute_confidence_delta(&outcome(0, 0, 3, 2, 1, 0));
        assert!(d < 0.0, "pure noise should yield negative delta, got {d}");
    }

    #[test]
    fn confidence_delta_is_bounded() {
        // Extreme values must not exceed MAX_CONFIDENCE_STEP.
        let extreme_pos = compute_confidence_delta(&outcome(100, 0, 0, 0, 0, 0));
        assert!(
            extreme_pos <= MAX_CONFIDENCE_STEP + f64::EPSILON,
            "positive delta must be bounded: {extreme_pos}"
        );
        let extreme_neg = compute_confidence_delta(&outcome(0, 100, 50, 50, 50, 50));
        assert!(
            extreme_neg >= -MAX_CONFIDENCE_STEP - f64::EPSILON,
            "negative delta must be bounded: {extreme_neg}"
        );
    }

    #[test]
    fn confidence_delta_zero_outcome_is_zero() {
        // Empty outcome → no signal → no change.
        let d = compute_confidence_delta(&outcome(0, 0, 0, 0, 0, 0));
        assert!(
            d.abs() < f64::EPSILON,
            "empty outcome should be ~0, got {d}"
        );
    }

    #[test]
    fn confidence_delta_mixed_signals_balance() {
        // 1 met (weight 3) + 3 noise (weight 1 each) ≈ balanced.
        // diagnosis_score = 3.0, noise = -3.0
        // raw = (3.0 - 3.0) / 4.0 = 0.0
        let d = compute_confidence_delta(&outcome(1, 0, 1, 1, 1, 0));
        assert!(
            d.abs() < 0.01,
            "balanced signals should be near zero, got {d}"
        );
    }

    // ── Unhappy-path coverage audit ─────────────────────────────────────────

    #[test]
    fn validate_accepts_boundary_confidence_values() {
        let mut n = valid_new();
        n.confidence = Some(0.0);
        assert!(n.validate().is_ok(), "confidence 0.0 must be accepted");
        n.confidence = Some(1.0);
        assert!(n.validate().is_ok(), "confidence 1.0 must be accepted");
    }

    #[test]
    fn parse_tag_rejects_empty_and_wrong_case() {
        assert!(LessonKind::parse_tag("").is_none());
        assert!(LessonKind::parse_tag("ToolDeprioritize").is_none());
        assert!(LessonKind::parse_tag("TOOL_DEPRIORITIZE").is_none());
    }

    #[test]
    fn parse_mysql_datetime_rejects_empty_and_whitespace() {
        assert!(parse_mysql_datetime("").is_err());
        assert!(parse_mysql_datetime("   ").is_err());
        assert!(parse_mysql_datetime("  2026-05-01 12:00:00  ").is_ok());
    }

    #[test]
    fn compute_confidence_delta_extreme_u32_values() {
        let extreme = outcome(u32::MAX, u32::MAX, u32::MAX, u32::MAX, u32::MAX, u32::MAX);
        let d = compute_confidence_delta(&extreme);
        assert!(d.is_finite(), "must not be NaN/inf with u32::MAX inputs");
        assert!(d.abs() <= MAX_CONFIDENCE_STEP + f64::EPSILON);
    }

    #[test]
    fn sanitize_for_prompt_strips_control_chars() {
        let dirty = "normal text\x00\x01\x1b[31minjection\x1b[0m\nline two";
        let clean = sanitize_for_prompt(dirty);
        assert!(!clean.contains('\x00'));
        assert!(!clean.contains('\x01'));
        assert!(!clean.contains('\x1b'));
        assert!(clean.contains('\n'), "newlines must be preserved");
        assert!(clean.contains("normal text"));
        assert!(clean.contains("line two"));
    }

    #[test]
    fn sanitize_for_prompt_strips_zero_width_and_bidi_chars() {
        let dirty = "before\u{200B}zero\u{200D}width\u{200E}after";
        let clean = sanitize_for_prompt(dirty);
        assert_eq!(clean, "beforezerowidthafter");

        let bidi = "normal\u{202E}reversed\u{202C}text";
        let clean = sanitize_for_prompt(bidi);
        assert!(!clean.contains('\u{202E}'));
        assert!(!clean.contains('\u{202C}'));

        let separators = "line\u{2028}para\u{2029}end";
        let clean = sanitize_for_prompt(separators);
        assert!(!clean.contains('\u{2028}'));
        assert!(!clean.contains('\u{2029}'));

        let bom_and_joiners = "\u{FEFF}start\u{2060}mid\u{2064}end";
        let clean = sanitize_for_prompt(bom_and_joiners);
        assert_eq!(clean, "startmidend");
    }

    #[test]
    fn sanitize_strips_exotic_invisible_unicode() {
        let exotic = "a\u{00AD}b\u{034F}c\u{061C}d\u{115F}e\u{180E}f\u{E0001}g\u{E0020}h";
        let clean = sanitize_for_prompt(exotic);
        assert_eq!(clean, "abcdefgh", "all exotic invisible chars stripped");
    }

    #[test]
    fn sanitize_strips_tag_characters_range() {
        let tags: String = (0xE0001..=0xE007Fu32).filter_map(char::from_u32).collect();
        let input = format!("before{tags}after");
        let clean = sanitize_for_prompt(&input);
        assert_eq!(clean, "beforeafter");
    }

    #[test]
    fn lesson_hint_from_lesson_uses_enum_kind() {
        let lesson = Lesson {
            id: "id".into(),
            user_id: "u".into(),
            persona: "p".into(),
            workload_tag: None,
            kind: LessonKind::ToolDeprioritize,
            trigger_signal: "tool_failures:grep".into(),
            action: "avoid grep".into(),
            confidence: 0.6,
            hit_count: 0,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let hint = LessonHint::from_lesson(&lesson);
        assert_eq!(hint.kind, LessonKind::ToolDeprioritize);
        assert_eq!(hint.kind.as_str(), "tool_deprioritize");
    }

    #[test]
    fn lesson_kind_display_matches_as_str() {
        for kind in [
            LessonKind::ToolDeprioritize,
            LessonKind::ToolBoost,
            LessonKind::PromptShape,
            LessonKind::PostconditionPattern,
            LessonKind::ErrorRecovery,
            LessonKind::SkillAcquired,
        ] {
            assert_eq!(format!("{kind}"), kind.as_str());
        }
    }

    #[test]
    fn validate_uses_byte_length_not_char_count() {
        // CJK chars are 3 bytes each in UTF-8. 86 CJK chars = 258 bytes > 255.
        let cjk = "工".repeat(86);
        assert_eq!(cjk.chars().count(), 86); // well under 255 chars
        assert!(cjk.len() > MAX_TRIGGER_SIGNAL_LEN); // but overflows byte budget

        let mut n = valid_new();
        n.trigger_signal = cjk;
        assert!(
            n.validate().is_err(),
            "multibyte trigger_signal must be rejected by byte-length check"
        );
    }

    #[test]
    fn validate_action_byte_length_boundary() {
        // 342 CJK chars × 3 bytes = 1026 > 1024.
        let cjk = "字".repeat(342);
        assert!(cjk.len() > MAX_ACTION_LEN);

        let mut n = valid_new();
        n.action = cjk;
        assert!(
            n.validate().is_err(),
            "multibyte action must be rejected by byte-length check"
        );
    }

    #[test]
    fn ddl_index_matches_prune_query_pattern() {
        // prune() uses WHERE user_id = ? AND updated_at < ... so the index
        // must be (user_id, updated_at), not (user_id, created_at).
        assert!(
            AGENT_LESSONS_DDL.contains("idx_agent_lessons_user_updated (user_id, updated_at)"),
            "DDL must index (user_id, updated_at) for prune queries"
        );
        assert!(
            !AGENT_LESSONS_DDL.contains("idx_agent_lessons_user_created"),
            "old (user_id, created_at) index must not exist — no query uses it"
        );
    }
}
