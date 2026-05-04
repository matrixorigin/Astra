//! Bridge between ReflectStage output and persistent feedback memory.
//!
//! Closes the reflect → memory gap: when ReflectStage diagnoses a pattern
//! and produces a Reflection, this module converts it into a
//! `StructuredFeedback` suitable for cross-session persistence.

use astra_turn_types::StructuredFeedback;

use crate::stages::reflect::FailureCategory;
use crate::state::Reflection;

/// Minimum confidence to consider a reflection worth persisting.
const PERSIST_CONFIDENCE_THRESHOLD: f64 = 0.4;

/// Convert a single `Reflection` + its diagnosed `FailureCategory` into
/// a `StructuredFeedback` for storage in `FeedbackStore` / Memoria.
///
/// Returns `None` when the reflection is too vague or low-confidence
/// to be useful as a durable lesson.
pub fn reflection_to_feedback(
    reflection: &Reflection,
    category: FailureCategory,
) -> Option<StructuredFeedback> {
    // Not worth persisting if confidence is too low
    if reflection.confidence < PERSIST_CONFIDENCE_THRESHOLD {
        return None;
    }

    // Not worth persisting if there's no actionable suggestion
    let rule = reflection.what_to_try.trim();
    if rule.is_empty() {
        return None;
    }

    Some(StructuredFeedback {
        rule: rule.to_string(),
        reason: reflection.why.clone(),
        apply_when: category_to_apply_when(category),
        source_signal: "reflect_stage".to_string(),
        confidence: reflection.confidence,
    })
}

/// Extract durable lessons from a session's accumulated reflections.
///
/// Deduplicates by `what_to_try`, keeps only high-confidence actionable
/// reflections, and returns them as `StructuredFeedback` ready for storage.
pub fn session_lessons(reflections: &[(Reflection, FailureCategory)]) -> Vec<StructuredFeedback> {
    let mut seen_rules = std::collections::HashSet::new();
    let mut lessons = Vec::new();

    for (refl, cat) in reflections {
        if let Some(fb) = reflection_to_feedback(refl, *cat) {
            let key = fb.rule.to_lowercase();
            if seen_rules.insert(key) {
                lessons.push(fb);
            }
        }
    }

    lessons
}

fn category_to_apply_when(category: FailureCategory) -> String {
    match category {
        FailureCategory::ToolFailures => {
            "when a tool call fails or returns unexpected results".into()
        }
        FailureCategory::Stall => "when progress stalls or the same approach keeps failing".into(),
        FailureCategory::NoProgress => "when tools succeed but produce no useful output".into(),
        FailureCategory::General => "general guidance".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StrategyDelta;

    fn make_reflection(what_to_try: &str, why: &str, confidence: f64) -> Reflection {
        Reflection {
            what_happened: "test scenario".into(),
            why: why.into(),
            what_to_try: what_to_try.into(),
            confidence,
            strategy_delta: StrategyDelta::default(),
        }
    }

    // ── reflection_to_feedback ──────────────────────────────────────

    #[test]
    fn converts_reflection_to_feedback() {
        let refl = make_reflection(
            "Use read_file before str_replace",
            "Edited a file without reading it first",
            0.8,
        );
        let fb = reflection_to_feedback(&refl, FailureCategory::ToolFailures).unwrap();

        assert_eq!(fb.rule, "Use read_file before str_replace");
        assert_eq!(fb.reason, "Edited a file without reading it first");
        assert_eq!(fb.source_signal, "reflect_stage");
        assert!((fb.confidence - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn maps_category_to_apply_when() {
        let refl = make_reflection("retry with backoff", "API timed out", 0.7);
        let fb = reflection_to_feedback(&refl, FailureCategory::NoProgress).unwrap();
        assert!(
            fb.apply_when.contains("no useful output"),
            "apply_when should mention no useful output"
        );

        let fb2 = reflection_to_feedback(&refl, FailureCategory::Stall).unwrap();
        assert!(
            fb2.apply_when.contains("stall"),
            "apply_when should mention stall"
        );
    }

    #[test]
    fn low_confidence_returns_none() {
        let refl = make_reflection("try something", "not sure", 0.2);
        assert!(reflection_to_feedback(&refl, FailureCategory::General).is_none());
    }

    #[test]
    fn at_threshold_returns_some() {
        let refl = make_reflection("do X", "because Y", PERSIST_CONFIDENCE_THRESHOLD);
        assert!(reflection_to_feedback(&refl, FailureCategory::General).is_some());
    }

    #[test]
    fn empty_suggestion_returns_none() {
        let refl = make_reflection("", "something failed", 0.9);
        assert!(reflection_to_feedback(&refl, FailureCategory::ToolFailures).is_none());
    }

    #[test]
    fn whitespace_only_suggestion_returns_none() {
        let refl = make_reflection("   \n  ", "something failed", 0.9);
        assert!(reflection_to_feedback(&refl, FailureCategory::ToolFailures).is_none());
    }

    // ── session_lessons ─────────────────────────────────────────────

    #[test]
    fn extracts_lessons_from_session() {
        let pairs = vec![
            (
                make_reflection("Use targeted reads", "full file too large", 0.8),
                FailureCategory::ToolFailures,
            ),
            (
                make_reflection("Run tests after edits", "broke the build", 0.7),
                FailureCategory::NoProgress,
            ),
        ];
        let lessons = session_lessons(&pairs);
        assert_eq!(lessons.len(), 2);
    }

    #[test]
    fn deduplicates_by_rule() {
        let pairs = vec![
            (
                make_reflection("Use targeted reads", "file too large", 0.8),
                FailureCategory::ToolFailures,
            ),
            (
                make_reflection("use targeted reads", "same lesson again", 0.9),
                FailureCategory::ToolFailures,
            ),
        ];
        let lessons = session_lessons(&pairs);
        assert_eq!(lessons.len(), 1, "duplicate rules should be deduplicated");
    }

    #[test]
    fn filters_low_confidence_from_session() {
        let pairs = vec![
            (
                make_reflection("good advice", "solid reason", 0.8),
                FailureCategory::ToolFailures,
            ),
            (
                make_reflection("weak advice", "not sure", 0.1),
                FailureCategory::General,
            ),
        ];
        let lessons = session_lessons(&pairs);
        assert_eq!(lessons.len(), 1);
        assert_eq!(lessons[0].rule, "good advice");
    }

    #[test]
    fn empty_reflections_empty_lessons() {
        let lessons = session_lessons(&[]);
        assert!(lessons.is_empty());
    }

    #[test]
    fn multiple_categories_produce_distinct_apply_when() {
        let pairs = vec![
            (
                make_reflection("fix tools", "tool broke", 0.8),
                FailureCategory::ToolFailures,
            ),
            (
                make_reflection("fix stall", "stalled out", 0.8),
                FailureCategory::Stall,
            ),
        ];
        let lessons = session_lessons(&pairs);
        assert_eq!(lessons.len(), 2);
        assert_ne!(lessons[0].apply_when, lessons[1].apply_when);
    }

    // ── trim ────────────────────────────────────────────────────────

    #[test]
    fn trims_whitespace_from_rule() {
        let refl = make_reflection("  use targeted reads  ", "reason", 0.8);
        let fb = reflection_to_feedback(&refl, FailureCategory::General).unwrap();
        assert_eq!(fb.rule, "use targeted reads");
    }

    // ── category_to_apply_when exhaustive ──────────────────────────

    #[test]
    fn all_categories_produce_nonempty_apply_when() {
        let categories = [
            FailureCategory::ToolFailures,
            FailureCategory::Stall,
            FailureCategory::NoProgress,
            FailureCategory::General,
        ];
        for cat in categories {
            let s = category_to_apply_when(cat);
            assert!(
                !s.is_empty(),
                "category {:?} should have non-empty apply_when",
                cat
            );
        }
    }
}
