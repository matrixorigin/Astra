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

use astra_services::{LessonKind, NewLesson};

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
}
