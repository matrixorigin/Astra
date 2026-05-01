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

use astra_pipeline::ToolHealthEntry;
use astra_services::{LessonKind, NewLesson};

use crate::observability_integration::ObservabilitySession;

/// Minimal session-end signal bundle. The runtime already tracks every
/// field in `TurnState` / `ImprovementTracker`; this struct is just the
/// view the extractor needs, so the extractor stays independent of those
/// concrete types.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionSummary {
    /// `tool_name → failure_count` across the session.
    pub tool_failures: HashMap<String, u32>,
    /// Tools whose failures are predominantly transient infrastructure
    /// issues (ResourceLimit, Network, ServerError, Auth, StreamIdle,
    /// StreamTransport) rather than tool-inherent bugs (ToolInvalidArgs,
    /// ToolTimeout, ToolNotFound). The extractor will **not** create
    /// ToolDeprioritize lessons for these — it would be wrong to teach
    /// the agent to avoid `grep` because the network was flaky.
    pub transient_failure_tools: std::collections::HashSet<String>,
    /// Tools with failures in the aggregate count but NO detailed outcome
    /// records. We can't tell if these are transient or inherent, so the
    /// extractor skips them — better to miss a lesson than to falsely
    /// block a tool for a week.
    pub undetermined_failure_tools: std::collections::HashSet<String>,
    /// Number of stall events the pipeline detected.
    pub stall_events: u32,
    /// User-correction snippets recorded during the session.
    pub user_corrections: Vec<String>,
    /// Number of unmet postconditions on the session's last ActionPlan run.
    pub unmet_postconditions: u32,
    /// Tools that were successfully used ≥ SUCCESS_REHABILITATE_THRESHOLD
    /// times this session. Used to weaken stale ToolDeprioritize lessons:
    /// if the agent successfully used grep 5 times today, the "avoid grep"
    /// lesson from last week should lose confidence.
    pub rehabilitated_tools: std::collections::HashSet<String>,
}

// ── Thresholds (test-pinned; bump deliberately) ─────────────────────────────
//
// These are **post-session** thresholds for persisting durable lessons.
// They are deliberately lower than the **in-session** auto-invoke
// thresholds in `astra_skills::auto_invoke` (STALL_TRIGGER_COUNT=5,
// CORRECTION_TRIGGER_COUNT=5). Rationale: auto-invoke fires a
// diagnostic mid-session (high cost, interrupts flow), so it needs a
// higher bar. Lesson extraction runs once at session end (zero runtime
// cost), so it can be more sensitive.

/// A tool must fail at least this many times to warrant a
/// ToolDeprioritize lesson.
pub const TOOL_FAILURE_LESSON_THRESHOLD: u32 = 3;
/// A tool must succeed at least this many times in one session to
/// rehabilitate (weaken) an existing ToolDeprioritize lesson.
pub const SUCCESS_REHABILITATE_THRESHOLD: usize = 3;
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
    // SAFETY: skip tools whose failures are predominantly transient infra
    // issues (network, resource limit, etc.). Creating a ToolDeprioritize
    // lesson for "grep timed out because the network was flaky" would
    // teach the agent to avoid grep for 7+ days — exactly the cascading
    // block scenario that caused outages before this guard.
    let mut tools: Vec<(&String, &u32)> = summary
        .tool_failures
        .iter()
        .filter(|&(name, c)| {
            *c >= TOOL_FAILURE_LESSON_THRESHOLD
                && !summary.transient_failure_tools.contains(name.as_str())
                && !summary.undetermined_failure_tools.contains(name.as_str())
        })
        .collect();
    tools.sort_by_key(|(name, _)| name.as_str());
    for (name, count) in tools {
        out.push(NewLesson {
            user_id: user_id.to_string(),
            persona: persona.to_string(),
            workload_tag: workload_tag.map(str::to_string),
            kind: LessonKind::ToolDeprioritize,
            trigger_signal: format!("tool_failures:{name}"),
            action: format!(
                "consider alternatives to `{name}` — failed {count} times last session due to tool-specific issues",
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
            trigger_signal: "stall_events".to_string(),
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
        out.push(NewLesson {
            user_id: user_id.to_string(),
            persona: persona.to_string(),
            workload_tag: workload_tag.map(str::to_string),
            kind: LessonKind::PromptShape,
            trigger_signal: "user_corrections".to_string(),
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
            trigger_signal: "unmet_postconditions".to_string(),
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
    let mut transient_failure_tools = std::collections::HashSet::new();
    // Tools with failures in the aggregate count but NO detailed outcome
    // records — we can't classify these as inherent or transient, so we
    // err on the safe side: skip lesson creation rather than risk a false
    // ToolDeprioritize that blocks the tool for days.
    let mut undetermined_failure_tools = std::collections::HashSet::new();

    for entry in tool_health {
        if entry.total_failures > 0 {
            tool_failures.insert(entry.name.clone(), entry.total_failures as u32);
        }
        // Check recent_outcomes: if ≥50% of failures have a transient
        // failure_category, mark this tool as transient-dominated.
        let failed_outcomes: Vec<&astra_pipeline::ToolOutcome> = entry
            .recent_outcomes
            .iter()
            .flat_map(|cache| cache.outcomes.iter())
            .filter(|o| !o.success)
            .collect();
        if failed_outcomes.is_empty() {
            // No outcome data → can't classify → undetermined → skip.
            if entry.total_failures > 0 {
                undetermined_failure_tools.insert(entry.name.clone());
            }
        } else {
            let transient_count = failed_outcomes
                .iter()
                .filter(|o| {
                    o.failure_category
                        .as_deref()
                        .map(is_transient_error_kind)
                        .unwrap_or(false)
                })
                .count();
            if transient_count * 2 >= failed_outcomes.len() {
                transient_failure_tools.insert(entry.name.clone());
            }
        }
    }

    let (user_corrections, unmet_postconditions, stall_events) = match obs {
        None => (Vec::new(), 0, 0),
        Some(session) => (
            session.recent_correction_excerpts.clone(),
            session.unmet_postcondition_count,
            session.stall_event_count,
        ),
    };

    // Tools that were successfully used enough times this session to
    // rehabilitate stale ToolDeprioritize lessons.
    let mut rehabilitated_tools = std::collections::HashSet::new();
    for entry in tool_health {
        let successes = entry.total_calls.saturating_sub(entry.total_failures);
        if successes >= SUCCESS_REHABILITATE_THRESHOLD {
            rehabilitated_tools.insert(entry.name.clone());
        }
    }

    SessionSummary {
        tool_failures,
        transient_failure_tools,
        undetermined_failure_tools,
        stall_events,
        user_corrections,
        unmet_postconditions,
        rehabilitated_tools,
    }
}

/// Transient infrastructure error kinds that should NOT produce
/// ToolDeprioritize lessons. These are environmental failures, not
/// evidence that the tool itself is broken.
///
/// Differs from [`astra_core::ErrorKind::is_retryable`]: that method
/// controls immediate retry (network/rate-limit/server-error only),
/// while this function controls **lesson suppression** (wider: also
/// includes `auth` and `resource_limit` because a bad token or fork
/// bomb is not the tool's fault).
fn is_transient_error_kind(tag: &str) -> bool {
    matches!(
        tag,
        "resource_limit"
            | "network"
            | "server_error"
            | "auth"
            | "rate_limit"
            | "stream_idle"
            | "stream_transport"
    )
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
        assert_eq!(out[0].trigger_signal, "tool_failures:grep");
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
        assert_eq!(names[0], "tool_failures:bash");
        assert_eq!(names[1], "tool_failures:grep");
        assert_eq!(names[2], "tool_failures:rg");
    }

    #[test]
    fn stall_at_threshold_yields_prompt_shape() {
        let mut s = base_summary();
        s.stall_events = 3;
        let out = extract_lessons(&s, "u", "p", None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, LessonKind::PromptShape);
        assert_eq!(out[0].trigger_signal, "stall_events");
    }

    #[test]
    fn corrections_at_threshold_yield_prompt_shape() {
        let mut s = base_summary();
        s.user_corrections = vec!["use rg not grep".into(), "limit to src/ only".into()];
        let out = extract_lessons(&s, "u", "p", None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, LessonKind::PromptShape);
        assert_eq!(out[0].trigger_signal, "user_corrections");
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
            out[0].trigger_signal.len() <= 255,
            "trigger_signal byte length must stay within DAO's MAX_TRIGGER_SIGNAL_LEN"
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

    #[test]
    fn trigger_signals_are_stable_bucket_keys() {
        // R3 contract: trigger_signal must NOT contain volatile counts or
        // correction snippets. It must be a stable key so the DAO's upsert-
        // by-content dedup works across sessions with different counts.
        let mut s = base_summary();
        s.tool_failures.insert("grep".into(), 3);
        s.stall_events = 5;
        s.user_corrections = vec!["a".into(), "b".into()];
        s.unmet_postconditions = 4;
        let lessons = extract_lessons(&s, "u", "p", None);

        for l in &lessons {
            // No digits in trigger_signal — counts go in action only.
            assert!(
                !l.trigger_signal.chars().any(|c| c.is_ascii_digit()),
                "trigger_signal {:?} contains a digit — volatile count leaked into key",
                l.trigger_signal
            );
        }

        // Changing the counts must produce the SAME trigger_signals
        // (different action text is fine — action is not part of the key).
        let mut s2 = base_summary();
        s2.tool_failures.insert("grep".into(), 10); // different count
        s2.stall_events = 20;
        s2.user_corrections = vec!["x".into(), "y".into()]; // different snippets
        s2.unmet_postconditions = 99;
        let lessons2 = extract_lessons(&s2, "u", "p", None);

        let keys1: Vec<(&str, &str)> = lessons
            .iter()
            .map(|l| (l.kind.as_str(), l.trigger_signal.as_str()))
            .collect();
        let keys2: Vec<(&str, &str)> = lessons2
            .iter()
            .map(|l| (l.kind.as_str(), l.trigger_signal.as_str()))
            .collect();
        assert_eq!(
            keys1, keys2,
            "trigger_signals must be identical across sessions with different counts"
        );
    }

    // ── P5: summarise_from_runtime ──────────────────────────────────────────
    //
    // The runtime-side adapter that turns already-tracked session data
    // (ToolHealthEntry rows + optional ObservabilitySession) into a
    // SessionSummary. Callers at session-end call this once and feed the
    // result to extract_lessons — zero bookkeeping of their own.

    use astra_pipeline::ToolHealthEntry;

    /// ToolHealthEntry with inherent failure outcomes (tool_timeout).
    /// Use this when the test goes through `summarise_from_runtime` so the
    /// transient/undetermined filter doesn't suppress the tool.
    fn health_inherent(name: &str, failures: usize, total: usize) -> ToolHealthEntry {
        use astra_pipeline::tool_health_types::{ToolOutcome, ToolOutcomeCacheEntry};
        let mut entry = health(name, failures, total);
        entry.recent_outcomes = vec![ToolOutcomeCacheEntry {
            signature: format!("{name} *"),
            outcomes: (0..failures)
                .map(|_| ToolOutcome {
                    success: false,
                    latency_ms: 0,
                    result_hash: 0,
                    at_epoch: 0,
                    failure_category: Some("tool_timeout".into()),
                })
                .collect(),
        }];
        entry
    }

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

    #[test]
    fn summarise_propagates_stall_and_unmet_counts_from_observability() {
        // P7a: the two new counters must flow through to the extractor so
        // session-end can emit PromptShape / PostconditionPattern lessons.
        use crate::observability_integration::ObservabilitySession;
        let mut obs = ObservabilitySession::new_simple("s-p7a");
        obs.record_stall_event();
        obs.record_stall_event();
        obs.record_stall_event();
        obs.record_unmet_postconditions(5);

        let s = summarise_from_runtime(&[], Some(&obs));
        assert_eq!(s.stall_events, 3);
        assert_eq!(s.unmet_postconditions, 5);
    }

    #[test]
    fn summarise_with_stalls_and_unmet_yields_all_lesson_kinds_end_to_end() {
        // Full stack: observability counters → summary → extractor must
        // produce ToolDeprioritize + PromptShape (stalls) + PostconditionPattern.
        use crate::observability_integration::ObservabilitySession;
        let entries = vec![health_inherent("grep", 3, 5)];
        let mut obs = ObservabilitySession::new_simple("s-p7a-e2e");
        obs.record_stall_event();
        obs.record_stall_event();
        obs.record_stall_event();
        obs.record_unmet_postconditions(4);

        let summary = summarise_from_runtime(&entries, Some(&obs));
        let lessons = extract_lessons(&summary, "u1", "generic", None);

        let kinds: std::collections::HashSet<LessonKind> = lessons.iter().map(|l| l.kind).collect();
        assert!(kinds.contains(&LessonKind::ToolDeprioritize));
        assert!(kinds.contains(&LessonKind::PromptShape));
        assert!(kinds.contains(&LessonKind::PostconditionPattern));
    }
}
