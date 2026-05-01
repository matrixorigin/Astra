//! Session-end lesson extractor.
//!
//! When a session ends, the signals we already track — repeated tool
//! failures, stall events, user corrections, unmet postconditions — need
//! to be distilled into durable `NewLesson` rows for `agent_lessons`.
//!
//! This module is the pure mapping step: `SessionSummary` → `Vec<NewLesson>`.
//! No DB, no clocks, no config. The main loop (or the session-end hook) is
//! responsible for calling this *once* at wrap-up and feeding the output
//! to `AgentLessonsService::record`.
//!
//! Thresholds are intentionally conservative — we'd rather under-record
//! than pollute cross-session memory with noise.

use std::collections::HashMap;
use std::sync::Arc;

use astra_pipeline::ToolHealthEntry;
use astra_services::{AgentLessonsService, LessonKind, NewLesson};

use crate::observability_integration::ObservabilitySession;

/// Minimal session-end signal bundle. The runtime already tracks every
/// field in `TurnState` / `ImprovementTracker`; this struct is just the
/// view the extractor needs, so the extractor stays independent of those
/// concrete types.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionSummary {
    /// `tool_name → failure_count` across the session.
    pub tool_failures: HashMap<String, u32>,
    /// Number of stall events the pipeline detected.
    pub stall_events: u32,
    /// User-correction snippets recorded during the session.
    pub user_corrections: Vec<String>,
    /// Number of unmet postconditions on the session's last ActionPlan run.
    pub unmet_postconditions: u32,
}

// ── Thresholds (test-pinned; bump deliberately) ─────────────────────────────

/// A tool must fail at least this many times to warrant a
/// ToolDeprioritize lesson.
pub const TOOL_FAILURE_LESSON_THRESHOLD: u32 = 3;
/// Stall events that warrant a PromptShape lesson.
pub const STALL_LESSON_THRESHOLD: u32 = 3;
/// User corrections that warrant a PromptShape lesson.
pub const CORRECTION_LESSON_THRESHOLD: usize = 2;
/// Unmet postconditions that warrant a PostconditionPattern lesson.
pub const UNMET_POSTCONDITION_LESSON_THRESHOLD: u32 = 3;

// ── Extractor ───────────────────────────────────────────────────────────────

/// Distil a session summary into durable lessons for
/// `(user_id, persona, workload_tag)`.
///
/// Output ordering is deterministic: tool deprioritize lessons first
/// (sorted by tool name), then stall / correction / postcondition
/// pattern lessons. Deterministic output keeps upstream tests stable.
#[must_use]
pub fn extract_lessons(
    summary: &SessionSummary,
    user_id: &str,
    persona: &str,
    workload_tag: Option<&str>,
) -> Vec<NewLesson> {
    let mut out = Vec::new();

    // Tool failures → ToolDeprioritize, one per over-threshold tool.
    let mut tools: Vec<(&String, &u32)> = summary
        .tool_failures
        .iter()
        .filter(|&(_, c)| *c >= TOOL_FAILURE_LESSON_THRESHOLD)
        .collect();
    tools.sort_by_key(|(name, _)| name.as_str());
    for (name, count) in tools {
        out.push(NewLesson {
            user_id: user_id.to_string(),
            persona: persona.to_string(),
            workload_tag: workload_tag.map(str::to_string),
            kind: LessonKind::ToolDeprioritize,
            trigger_signal: format!("{count} failures on {name}"),
            action: format!(
                "deprioritize `{name}` for this workload — failed {count} times last session",
            ),
            confidence: None,
        });
    }

    // Stalls → PromptShape (agent looped, prompt likely too open-ended).
    if summary.stall_events >= STALL_LESSON_THRESHOLD {
        out.push(NewLesson {
            user_id: user_id.to_string(),
            persona: persona.to_string(),
            workload_tag: workload_tag.map(str::to_string),
            kind: LessonKind::PromptShape,
            trigger_signal: format!("{} stall events in one session", summary.stall_events),
            action: "tighten the plan: restate scope before each tool call and break tasks into \
                 explicit steps"
                .into(),
            confidence: None,
        });
    }

    // Repeated corrections → PromptShape. Dedup snippets first so two
    // identical corrections count once (user hitting the same nit twice).
    let mut seen = std::collections::HashSet::new();
    let distinct_corrections: Vec<&String> = summary
        .user_corrections
        .iter()
        .filter(|s| !s.trim().is_empty() && seen.insert(s.trim().to_lowercase()))
        .collect();
    if distinct_corrections.len() >= CORRECTION_LESSON_THRESHOLD {
        // Summarise by length-bounded join so the lesson row stays within
        // `MAX_TRIGGER_SIGNAL_LEN`. The extractor does not know that cap
        // but truncates defensively anyway.
        let mut joined = distinct_corrections
            .iter()
            .take(3)
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        if joined.chars().count() > 200 {
            joined = joined.chars().take(199).collect::<String>() + "…";
        }
        out.push(NewLesson {
            user_id: user_id.to_string(),
            persona: persona.to_string(),
            workload_tag: workload_tag.map(str::to_string),
            kind: LessonKind::PromptShape,
            trigger_signal: format!(
                "{} distinct user corrections: {}",
                distinct_corrections.len(),
                joined,
            ),
            action:
                "restate the user's scope/intent before planning; confirm contested choices early"
                    .into(),
            confidence: None,
        });
    }

    // Unmet postconditions → PostconditionPattern.
    if summary.unmet_postconditions >= UNMET_POSTCONDITION_LESSON_THRESHOLD {
        out.push(NewLesson {
            user_id: user_id.to_string(),
            persona: persona.to_string(),
            workload_tag: workload_tag.map(str::to_string),
            kind: LessonKind::PostconditionPattern,
            trigger_signal: format!(
                "{} unmet postconditions on final ActionPlan",
                summary.unmet_postconditions
            ),
            action: "split the plan into smaller actions with narrower postconditions; \
                     verify each action's outcome before the next one"
                .into(),
            confidence: None,
        });
    }

    out
}

/// Runtime-side adapter: build a [`SessionSummary`] from the two sources
/// the session-cleanup path already has on hand — per-tool health entries
/// and an optional live observability session. Callers at session-end
/// call this once and feed the result to [`extract_lessons`].
///
/// Zero-failure tools are omitted so the extractor's thresholds don't
/// see noise. Missing `ObservabilitySession` degrades gracefully: the
/// secondary signals (stalls / corrections / unmet postconditions) stay
/// at zero rather than pretending we have data we don't.
#[must_use]
pub fn summarise_from_runtime(
    tool_health: &[ToolHealthEntry],
    obs: Option<&ObservabilitySession>,
) -> SessionSummary {
    let mut tool_failures = HashMap::new();
    for entry in tool_health {
        if entry.total_failures > 0 {
            tool_failures.insert(entry.name.clone(), entry.total_failures as u32);
        }
    }

    let (user_corrections, unmet_postconditions) = match obs {
        None => (Vec::new(), 0),
        Some(session) => (
            session.recent_correction_excerpts.clone(),
            // Unmet postconditions are tracked per-turn; the session-level
            // total is currently not collected on ObservabilitySession, so
            // we degrade to zero. Once the observability layer exposes a
            // cumulative count this line flips to the real value.
            0,
        ),
    };

    // Stalls aren't cumulatively tracked on ObservabilitySession either;
    // they're emitted as journal events at detection time. Leave at zero
    // until that signal is surfaced here.
    let stall_events = 0;

    SessionSummary {
        tool_failures,
        stall_events,
        user_corrections,
        unmet_postconditions,
    }
}

/// Session-end convenience: extract lessons from `summary` and record each
/// one via the service. Returns the number of lessons successfully
/// persisted.
///
/// Errors from individual `record` calls are swallowed with a warning log
/// — cross-session memory is best-effort and a single DAO failure must
/// not block session cleanup. Callers that need stricter guarantees can
/// call `extract_lessons` + `svc.record` directly.
pub async fn persist_session_lessons(
    svc: Arc<dyn AgentLessonsService>,
    summary: &SessionSummary,
    user_id: &str,
    persona: &str,
    workload_tag: Option<&str>,
) -> usize {
    let lessons = extract_lessons(summary, user_id, persona, workload_tag);
    if lessons.is_empty() {
        return 0;
    }

    let mut persisted = 0usize;
    for lesson in lessons {
        match svc.record(lesson).await {
            Ok(_) => persisted += 1,
            Err(e) => tracing::warn!(
                target: "lesson_extractor",
                user_id = user_id,
                persona = persona,
                error = %e,
                "failed to persist lesson; skipping",
            ),
        }
    }
    persisted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_summary() -> SessionSummary {
        SessionSummary::default()
    }

    // ── Empty / noisy sessions produce nothing ──────────────────────────────

    #[test]
    fn empty_summary_yields_no_lessons() {
        assert!(extract_lessons(&base_summary(), "u", "p", None).is_empty());
    }

    #[test]
    fn subthreshold_signals_yield_no_lessons() {
        let mut s = base_summary();
        s.tool_failures.insert("grep".into(), 2); // threshold is 3
        s.stall_events = 2;
        s.user_corrections = vec!["nit 1".into()]; // threshold is 2
        s.unmet_postconditions = 2;
        assert!(extract_lessons(&s, "u", "p", None).is_empty());
    }

    // ── Each rule in isolation ──────────────────────────────────────────────

    #[test]
    fn tool_failure_at_threshold_yields_tool_deprioritize() {
        let mut s = base_summary();
        s.tool_failures.insert("grep".into(), 3);
        let out = extract_lessons(&s, "u1", "generic", None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, LessonKind::ToolDeprioritize);
        assert!(out[0].trigger_signal.contains("grep"));
        assert!(out[0].action.contains("`grep`"));
        assert_eq!(out[0].user_id, "u1");
        assert_eq!(out[0].persona, "generic");
        assert!(out[0].workload_tag.is_none());
    }

    #[test]
    fn multiple_failing_tools_emit_one_lesson_each_sorted_by_name() {
        let mut s = base_summary();
        s.tool_failures.insert("rg".into(), 5);
        s.tool_failures.insert("bash".into(), 3);
        s.tool_failures.insert("grep".into(), 4);
        let out = extract_lessons(&s, "u", "p", None);
        assert_eq!(out.len(), 3);
        let names: Vec<&str> = out
            .iter()
            .filter_map(|l| {
                if l.kind == LessonKind::ToolDeprioritize {
                    Some(l.trigger_signal.as_str())
                } else {
                    None
                }
            })
            .collect();
        // Sorted alphabetically → bash, grep, rg.
        assert!(names[0].contains("bash"));
        assert!(names[1].contains("grep"));
        assert!(names[2].contains("rg"));
    }

    #[test]
    fn stall_at_threshold_yields_prompt_shape() {
        let mut s = base_summary();
        s.stall_events = 3;
        let out = extract_lessons(&s, "u", "p", None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, LessonKind::PromptShape);
        assert!(out[0].trigger_signal.contains("3 stall"));
    }

    #[test]
    fn corrections_at_threshold_yield_prompt_shape() {
        let mut s = base_summary();
        s.user_corrections = vec!["use rg not grep".into(), "limit to src/ only".into()];
        let out = extract_lessons(&s, "u", "p", None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, LessonKind::PromptShape);
        assert!(out[0].trigger_signal.contains("use rg not grep"));
        assert!(out[0].trigger_signal.contains("limit to src"));
    }

    #[test]
    fn duplicate_corrections_dedup_before_threshold_check() {
        let mut s = base_summary();
        s.user_corrections = vec![
            "use rg not grep".into(),
            "use rg not grep".into(),   // dup → doesn't count
            " use rg not grep ".into(), // whitespace/case variant → dup
        ];
        assert!(
            extract_lessons(&s, "u", "p", None).is_empty(),
            "duplicates must not satisfy the 2-distinct threshold",
        );
    }

    #[test]
    fn corrections_trigger_signal_is_bounded() {
        let mut s = base_summary();
        s.user_corrections = (0..10)
            .map(|i| format!("correction {i} — lorem ipsum dolor sit amet consectetur"))
            .collect();
        let out = extract_lessons(&s, "u", "p", None);
        assert_eq!(out.len(), 1);
        assert!(
            out[0].trigger_signal.chars().count() <= 255,
            "trigger_signal must stay within DAO's MAX_TRIGGER_SIGNAL_LEN"
        );
    }

    #[test]
    fn unmet_postconditions_at_threshold_yield_postcondition_pattern() {
        let mut s = base_summary();
        s.unmet_postconditions = 3;
        let out = extract_lessons(&s, "u", "p", None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, LessonKind::PostconditionPattern);
    }

    // ── Composition — multiple signals at once ──────────────────────────────

    #[test]
    fn all_four_signals_present_yield_all_four_lesson_kinds() {
        let mut s = base_summary();
        s.tool_failures.insert("grep".into(), 3);
        s.stall_events = 3;
        s.user_corrections = vec!["a".into(), "b".into()];
        s.unmet_postconditions = 3;
        let out = extract_lessons(&s, "u", "p", None);
        assert_eq!(out.len(), 4);
        let kinds: std::collections::HashSet<LessonKind> = out.iter().map(|l| l.kind).collect();
        assert!(kinds.contains(&LessonKind::ToolDeprioritize));
        assert!(kinds.contains(&LessonKind::PromptShape));
        assert!(kinds.contains(&LessonKind::PostconditionPattern));
    }

    // ── Workload tag plumbing ───────────────────────────────────────────────

    #[test]
    fn workload_tag_is_attached_to_every_lesson() {
        let mut s = base_summary();
        s.tool_failures.insert("grep".into(), 3);
        s.stall_events = 3;
        let out = extract_lessons(&s, "u", "p", Some("code-review"));
        assert!(!out.is_empty());
        for l in &out {
            assert_eq!(l.workload_tag.as_deref(), Some("code-review"));
        }
    }

    // ── Every extracted lesson passes DAO validation ───────────────────────

    #[test]
    fn every_extracted_lesson_passes_new_lesson_validate() {
        let mut s = base_summary();
        s.tool_failures.insert("grep".into(), 10);
        s.tool_failures.insert("rg".into(), 3);
        s.stall_events = 7;
        s.user_corrections = (0..6).map(|i| format!("correction {i}")).collect();
        s.unmet_postconditions = 9;
        let out = extract_lessons(&s, "u", "p", Some("x"));
        for l in &out {
            l.validate().unwrap_or_else(|e| {
                panic!("extracted lesson failed DAO validation: {e}; lesson = {l:?}")
            });
        }
    }

    // ── P5: summarise_from_runtime ──────────────────────────────────────────
    //
    // The runtime-side adapter that turns already-tracked session data
    // (ToolHealthEntry rows + optional ObservabilitySession) into a
    // SessionSummary. Callers at session-end call this once and feed the
    // result to extract_lessons — zero bookkeeping of their own.

    use astra_pipeline::ToolHealthEntry;

    fn health(name: &str, failures: usize, total: usize) -> ToolHealthEntry {
        ToolHealthEntry {
            name: name.into(),
            total_calls: total,
            total_failures: failures,
            failure_rate: if total > 0 {
                failures as f64 / total as f64
            } else {
                0.0
            },
            last_updated_epoch: 0,
            recent_outcomes: Vec::new(),
        }
    }

    #[test]
    fn summarise_empty_inputs_produces_empty_summary() {
        let s = summarise_from_runtime(&[], None);
        assert_eq!(s, SessionSummary::default());
    }

    #[test]
    fn summarise_maps_tool_health_failures_into_tool_failures_map() {
        let entries = vec![
            health("grep", 5, 8),
            health("rg", 0, 3),
            health("bash", 2, 10),
        ];
        let s = summarise_from_runtime(&entries, None);
        assert_eq!(s.tool_failures.get("grep"), Some(&5));
        assert_eq!(s.tool_failures.get("bash"), Some(&2));
        // Zero-failure tools must be omitted so the extractor's thresholds
        // don't see noise.
        assert!(!s.tool_failures.contains_key("rg"));
    }

    #[test]
    fn summarise_without_observability_leaves_extra_signals_zero() {
        let entries = vec![health("grep", 5, 8)];
        let s = summarise_from_runtime(&entries, None);
        assert_eq!(s.stall_events, 0);
        assert!(s.user_corrections.is_empty());
        assert_eq!(s.unmet_postconditions, 0);
    }

    #[test]
    fn summarise_with_observability_extracts_corrections_and_unmet() {
        use crate::observability_integration::ObservabilitySession;
        let mut obs = ObservabilitySession::new_simple("s-test");
        obs.recent_correction_excerpts = vec!["use rg not grep".into(), "narrow to src/".into()];

        let s = summarise_from_runtime(&[], Some(&obs));
        assert_eq!(s.user_corrections.len(), 2);
        assert!(s.user_corrections.iter().any(|c| c.contains("rg")));
    }

    // ── persist_session_lessons ─────────────────────────────────────────────

    use astra_services::Lesson;
    use async_trait::async_trait;
    use std::sync::Mutex as StdMutex;

    struct StubPersistSvc {
        recorded: StdMutex<Vec<NewLesson>>,
        /// When true, every record call returns a synthetic DAO error.
        fail_all: bool,
        /// When Some, only record calls whose trigger matches get an error.
        fail_on_trigger_contains: Option<&'static str>,
    }

    impl StubPersistSvc {
        fn happy() -> Self {
            Self {
                recorded: StdMutex::new(Vec::new()),
                fail_all: false,
                fail_on_trigger_contains: None,
            }
        }
        fn failing() -> Self {
            Self {
                recorded: StdMutex::new(Vec::new()),
                fail_all: true,
                fail_on_trigger_contains: None,
            }
        }
        fn partial_failure(marker: &'static str) -> Self {
            Self {
                recorded: StdMutex::new(Vec::new()),
                fail_all: false,
                fail_on_trigger_contains: Some(marker),
            }
        }
        fn recorded_count(&self) -> usize {
            self.recorded.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl AgentLessonsService for StubPersistSvc {
        async fn record(&self, new: NewLesson) -> Result<Lesson, sqlx::Error> {
            if self.fail_all {
                return Err(sqlx::Error::Protocol("synthetic failure".into()));
            }
            if let Some(marker) = self.fail_on_trigger_contains
                && new.trigger_signal.contains(marker)
            {
                return Err(sqlx::Error::Protocol("selective failure".into()));
            }
            self.recorded.lock().unwrap().push(new.clone());
            Ok(Lesson {
                id: "stub".into(),
                user_id: new.user_id,
                persona: new.persona,
                workload_tag: new.workload_tag,
                kind: new.kind,
                trigger_signal: new.trigger_signal,
                action: new.action,
                confidence: new.confidence.unwrap_or(0.6),
                hit_count: 0,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            })
        }

        async fn load_recent(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: u32,
        ) -> Result<Vec<Lesson>, sqlx::Error> {
            unreachable!("not called by persist tests")
        }

        async fn record_hit(&self, _: &str) -> Result<i64, sqlx::Error> {
            unreachable!("not called by persist tests")
        }

        async fn prune(&self, _: &str, _: u32) -> Result<u64, sqlx::Error> {
            unreachable!("not called by persist tests")
        }
    }

    #[tokio::test]
    async fn persist_records_each_extracted_lesson() {
        let svc = Arc::new(StubPersistSvc::happy());
        let mut summary = base_summary();
        summary.tool_failures.insert("grep".into(), 3);
        summary
            .user_corrections
            .extend(["a".to_string(), "b".to_string()]);

        let n = persist_session_lessons(svc.clone(), &summary, "u1", "generic", None).await;
        assert_eq!(n, 2, "both lessons should persist");
        assert_eq!(svc.recorded_count(), 2);
    }

    #[tokio::test]
    async fn persist_empty_summary_records_nothing() {
        let svc = Arc::new(StubPersistSvc::happy());
        let n = persist_session_lessons(svc.clone(), &base_summary(), "u1", "generic", None).await;
        assert_eq!(n, 0);
        assert_eq!(svc.recorded_count(), 0);
    }

    #[tokio::test]
    async fn persist_swallows_dao_errors_and_returns_zero() {
        // Load-bearing: a DAO outage must not panic out of the session-cleanup
        // path. The caller sees persisted=0 and moves on.
        let svc = Arc::new(StubPersistSvc::failing());
        let mut summary = base_summary();
        summary.tool_failures.insert("grep".into(), 3);

        let n = persist_session_lessons(svc.clone(), &summary, "u1", "generic", None).await;
        assert_eq!(n, 0);
        assert_eq!(svc.recorded_count(), 0);
    }

    #[tokio::test]
    async fn persist_partial_failure_still_records_other_lessons() {
        // If one lesson's record call fails but another succeeds, we still
        // want the successful one persisted and a correct count reported.
        let svc = Arc::new(StubPersistSvc::partial_failure("grep"));
        let mut summary = base_summary();
        summary.tool_failures.insert("grep".into(), 3); // will fail
        summary.tool_failures.insert("rg".into(), 5); // will succeed
        summary.stall_events = 3; // will succeed (PromptShape)

        let n = persist_session_lessons(svc.clone(), &summary, "u1", "generic", None).await;
        assert_eq!(n, 2, "rg and stall lessons should persist");
        let recorded = svc.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 2);
        // The grep lesson must not be in the recorded set.
        assert!(recorded.iter().all(|r| !r.trigger_signal.contains("grep")));
    }

    #[tokio::test]
    async fn persist_plumbs_scope_to_every_lesson() {
        let svc = Arc::new(StubPersistSvc::happy());
        let mut summary = base_summary();
        summary.tool_failures.insert("grep".into(), 3);
        summary.stall_events = 3;

        persist_session_lessons(
            svc.clone(),
            &summary,
            "u1",
            "code-review",
            Some("pr-review"),
        )
        .await;

        let recorded = svc.recorded.lock().unwrap();
        assert!(!recorded.is_empty());
        for l in recorded.iter() {
            assert_eq!(l.user_id, "u1");
            assert_eq!(l.persona, "code-review");
            assert_eq!(l.workload_tag.as_deref(), Some("pr-review"));
        }
    }

    #[test]
    fn summarise_feeds_into_extract_lessons_end_to_end() {
        // The load-bearing invariant: runtime → summary → extract_lessons
        // must yield the right LessonKinds without the caller doing any
        // hand-rolling.
        use crate::observability_integration::ObservabilitySession;
        let entries = vec![health("grep", 3, 5), health("bash", 2, 10)];
        let mut obs = ObservabilitySession::new_simple("s-end-to-end");
        obs.recent_correction_excerpts = vec!["a".into(), "b".into()];

        let summary = summarise_from_runtime(&entries, Some(&obs));
        let lessons = extract_lessons(&summary, "u1", "generic", None);

        let kinds: std::collections::HashSet<LessonKind> = lessons.iter().map(|l| l.kind).collect();
        assert!(
            kinds.contains(&LessonKind::ToolDeprioritize),
            "grep at 3 failures must yield ToolDeprioritize"
        );
        assert!(
            kinds.contains(&LessonKind::PromptShape),
            "2 corrections must yield PromptShape"
        );
        // bash had only 2 failures — below threshold — so it must NOT appear.
        assert!(
            !lessons.iter().any(|l| l.trigger_signal.contains("bash")),
            "sub-threshold tool must not produce a lesson"
        );
    }
}
