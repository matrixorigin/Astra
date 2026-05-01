//! Session-bootstrap lesson loading.
//!
//! When a new session starts, the main loop needs a single one-liner to
//! pull cross-session lessons from `agent_lessons` and attach them to the
//! SelfModel so the LLM sees them on turn 1. This module is that seam.
//!
//! Kept deliberately small: no new types, no builders, just one async free
//! function. Test coverage is a focused contract test against a stub
//! `AgentLessonsService` — live-DB flow is already exercised by
//! `self_model_lessons_db_it.rs`.

use std::sync::Arc;

use astra_services::AgentLessonsService;

use crate::self_model::{LessonHint, SelfModel};

/// Default number of lessons attached to a fresh session. Matches the
/// `SelfModel` renderer's top-5 cap; one extra row is allowed so the
/// "… N more" overflow marker is accurate.
pub const DEFAULT_SESSION_BOOTSTRAP_LIMIT: u32 = 6;

/// Outcome of session-bootstrap lesson loading. Callers that want to
/// surface a user-visible indicator when learning is degraded can
/// inspect this instead of guessing from the lesson count alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LessonBootstrapStatus {
    /// Lessons loaded successfully (possibly zero if none exist yet).
    Loaded,
    /// DAO call failed — session started without carried-over lessons.
    /// Cross-session learning is effectively disabled for this session.
    DaoFailed,
    /// No DAO was provided (e.g., no MatrixOne configured). Learning
    /// was never attempted.
    Unconfigured,
}

/// Return type from [`attach_session_lessons`].
pub struct LessonBootstrapResult {
    pub model: SelfModel,
    pub status: LessonBootstrapStatus,
    /// Number of lessons attached (0 on failure or when none exist).
    pub lesson_count: usize,
}

/// Attach cross-session lessons for `(user_id, persona, workload_tag)` to
/// a freshly-constructed SelfModel. Intended to be called once per new
/// session, immediately after the base SelfModel is built.
///
/// - `workload_tag = None` pulls general (NULL-tag) lessons only.
/// - `workload_tag = Some(tag)` pulls tag-matching rows + general ones;
///   this mirrors `AgentLessonsService::load_recent`'s scope semantics.
///
/// DAO errors are swallowed (log-only) — lesson loading is best-effort.
/// A failure in cross-session memory must never block a new session from
/// starting. The [`LessonBootstrapResult::status`] tells the caller
/// whether learning is healthy so it can surface a hint if desired.
pub async fn attach_session_lessons(
    model: SelfModel,
    svc: Arc<dyn AgentLessonsService>,
    user_id: &str,
    persona: &str,
    workload_tag: Option<&str>,
    limit: u32,
) -> LessonBootstrapResult {
    let lessons = match svc.load_recent(user_id, persona, workload_tag, limit).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(
                target: "lesson_bootstrap",
                user_id = user_id,
                persona = persona,
                workload_tag = workload_tag.unwrap_or(""),
                error = %e,
                "load_recent failed; starting session without carried-over lessons",
            );
            return LessonBootstrapResult {
                model,
                status: LessonBootstrapStatus::DaoFailed,
                lesson_count: 0,
            };
        }
    };

    let count = lessons.len();
    let hints: Vec<LessonHint> = lessons.iter().map(LessonHint::from_lesson).collect();
    LessonBootstrapResult {
        model: model.with_lessons(hints),
        status: LessonBootstrapStatus::Loaded,
        lesson_count: count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::{Lesson, LessonKind, NewLesson};
    use async_trait::async_trait;
    use chrono::Utc;
    use std::sync::Mutex;

    // ── Stub DAO ────────────────────────────────────────────────────────────

    struct StubLessons {
        rows: Vec<Lesson>,
        fail: bool,
        load_calls: Mutex<Vec<(String, String, Option<String>, u32)>>,
    }

    impl StubLessons {
        fn with_rows(rows: Vec<Lesson>) -> Self {
            Self {
                rows,
                fail: false,
                load_calls: Mutex::new(Vec::new()),
            }
        }

        fn failing() -> Self {
            Self {
                rows: Vec::new(),
                fail: true,
                load_calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl AgentLessonsService for StubLessons {
        async fn record(&self, _: NewLesson) -> Result<Lesson, sqlx::Error> {
            unreachable!("not called by bootstrap")
        }

        async fn load_recent(
            &self,
            user_id: &str,
            persona: &str,
            workload_tag: Option<&str>,
            limit: u32,
        ) -> Result<Vec<Lesson>, sqlx::Error> {
            self.load_calls.lock().unwrap().push((
                user_id.to_string(),
                persona.to_string(),
                workload_tag.map(str::to_string),
                limit,
            ));
            if self.fail {
                return Err(sqlx::Error::Protocol("synthetic failure".into()));
            }
            Ok(self.rows.clone())
        }

        async fn record_hit(&self, _: &str) -> Result<i64, sqlx::Error> {
            unreachable!("not called by bootstrap")
        }

        async fn prune(&self, _: &str, _: u32) -> Result<u64, sqlx::Error> {
            unreachable!("not called by bootstrap")
        }
    }

    fn sample_lesson(kind: LessonKind, trigger: &str, action: &str) -> Lesson {
        Lesson {
            id: "uuid".into(),
            user_id: "u1".into(),
            persona: "generic".into(),
            workload_tag: None,
            kind,
            trigger_signal: trigger.into(),
            action: action.into(),
            confidence: 0.7,
            hit_count: 0,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn minimal_self_model() -> SelfModel {
        let empty = serde_json::json!({
            "capabilities": {
                "total_tools": 0, "tool_names": [], "tool_health": [],
                "deprioritized_tools": [], "pinned_tools": [], "skills": [],
                "boosted_tools": [], "widen_selection_pending": false,
                "outcome_memory": [],
            },
            "state": {
                "turn_number": 1, "token_budget": null, "scenario": null,
                "active_experiment": null, "session_elapsed_secs": 0,
                "correction_count": 0, "compression_count": 0,
            },
            "goals": {
                "goal": null, "session_goal": null, "plan_goal": null,
                "tracked_goal": null, "goal_source": "none",
                "tracking_status": "idle", "progress": null,
                "recent_milestones": [], "milestone_count": 0,
            },
            "recent_signals": [],
            "constraints": {
                "max_mutations_per_turn": 2, "config_drift_ceiling": 0.3,
                "min_tool_pool_size": 5, "token_reserve_fraction": 0.2,
            }
        });
        serde_json::from_value(empty).unwrap()
    }

    // ── Contract tests ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn attaches_loaded_lessons_to_self_model() {
        let svc = Arc::new(StubLessons::with_rows(vec![
            sample_lesson(LessonKind::ToolDeprioritize, "grep stalls", "switch to rg"),
            sample_lesson(LessonKind::PromptShape, "scope drift", "restate scope"),
        ]));
        let result = attach_session_lessons(
            minimal_self_model(),
            svc.clone(),
            "u1",
            "generic",
            None,
            DEFAULT_SESSION_BOOTSTRAP_LIMIT,
        )
        .await;

        assert_eq!(result.status, LessonBootstrapStatus::Loaded);
        assert_eq!(result.lesson_count, 2);
        assert_eq!(result.model.lessons.len(), 2);
        assert_eq!(
            result.model.lessons[0].kind,
            astra_services::LessonKind::ToolDeprioritize
        );
        assert_eq!(result.model.lessons[0].action, "switch to rg");
    }

    #[tokio::test]
    async fn propagates_workload_tag_to_dao() {
        let svc = Arc::new(StubLessons::with_rows(Vec::new()));
        let _ = attach_session_lessons(
            minimal_self_model(),
            svc.clone(),
            "u1",
            "code-review",
            Some("pr-review"),
            5,
        )
        .await;

        let calls = svc.load_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "u1");
        assert_eq!(calls[0].1, "code-review");
        assert_eq!(calls[0].2.as_deref(), Some("pr-review"));
        assert_eq!(calls[0].3, 5);
    }

    #[tokio::test]
    async fn dao_failure_returns_dao_failed_status() {
        let svc = Arc::new(StubLessons::failing());
        let base = minimal_self_model();
        let result = attach_session_lessons(base, svc, "u1", "generic", None, 5).await;
        assert_eq!(result.status, LessonBootstrapStatus::DaoFailed);
        assert_eq!(result.lesson_count, 0);
        assert!(
            result.model.lessons.is_empty(),
            "DAO failure must not corrupt the model; lessons vec must stay empty",
        );
    }

    #[tokio::test]
    async fn empty_result_set_yields_no_lessons() {
        let svc = Arc::new(StubLessons::with_rows(Vec::new()));
        let result =
            attach_session_lessons(minimal_self_model(), svc, "u1", "generic", None, 5).await;
        assert_eq!(result.status, LessonBootstrapStatus::Loaded);
        assert_eq!(result.lesson_count, 0);
        assert!(result.model.lessons.is_empty());
        let rendered = result.model.to_system_prompt_section();
        assert!(
            !rendered.contains("Lessons from prior sessions"),
            "empty lessons must produce no prompt block"
        );
    }

    #[tokio::test]
    async fn default_limit_is_six() {
        // Pinned so future tuning is intentional, not accidental.
        assert_eq!(DEFAULT_SESSION_BOOTSTRAP_LIMIT, 6);
    }
}
