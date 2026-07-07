//! Bridge between ReflectStage output and persistent feedback memory.
//!
//! Converts structured reflection diagnoses into [`StructuredFeedback`]
//! suitable for cross-session persistence via [`FeedbackStore`].
//!
//! The types that were previously scattered across `state.rs` and
//! `stages/reflect.rs` are now consolidated here — they exist only to
//! serve the reflection-to-feedback conversion used by `astra-cli`.

use astra_turn_types::StructuredFeedback;

// ─── FailureCategory ─────────────────────────────────────────────────────────

/// Root cause category for structured diagnosis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCategory {
    /// Specific tools keep failing (HTTP errors, permission denied, etc.)
    ToolFailures,
    /// Agent repeats the same tool calls without new information.
    Stall,
    /// Tools succeed but produce no useful output toward the goal.
    NoProgress,
    /// Catch-all: insufficient progress without a clear single cause.
    General,
}

// ─── Reflection ──────────────────────────────────────────────────────────────

/// Structured self-correction data from a reflection pass.
#[derive(Debug, Clone)]
pub struct Reflection {
    /// What happened that triggered reflection.
    pub what_happened: String,
    /// Root cause analysis.
    pub why: String,
    /// Proposed corrective action.
    pub what_to_try: String,
    /// Confidence in the proposed correction (0.0-1.0).
    pub confidence: f64,
    /// Strategy adjustments to apply.
    pub strategy_delta: StrategyDelta,
}

/// Adjustments to apply after reflection.
#[derive(Debug, Clone, Default)]
pub struct StrategyDelta {
    /// Tools to add to blocked list.
    pub block_tools: Vec<String>,
    /// Tools to try that weren't in the original selection.
    pub add_tools: Vec<String>,
    /// Additional context to inject into the next prompt.
    pub inject_context: Option<String>,
    /// Whether to widen tool surface (lower threshold).
    pub widen_surface: bool,
}

// ─── Conversion ──────────────────────────────────────────────────────────────

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

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(fb.apply_when.contains("tool call fails"));
    }

    #[test]
    fn skips_low_confidence_reflections() {
        let refl = make_reflection("advice", "reason", 0.1);
        assert!(reflection_to_feedback(&refl, FailureCategory::General).is_none());
    }

    #[test]
    fn skips_empty_advice() {
        let refl = make_reflection("", "reason", 0.8);
        assert!(reflection_to_feedback(&refl, FailureCategory::General).is_none());
    }

    #[test]
    fn skips_whitespace_only_advice() {
        let refl = make_reflection("   ", "reason", 0.8);
        assert!(reflection_to_feedback(&refl, FailureCategory::General).is_none());
    }

    // ── session_lessons ────────────────────────────────────────────

    #[test]
    fn session_lessons_deduplicates_by_rule() {
        let refl_a = make_reflection("advice a", "reason 1", 0.8);
        let refl_a2 = make_reflection("advice a", "reason 2", 0.6);
        let refl_b = make_reflection("advice b", "reason 3", 0.8);
        let lessons = session_lessons(&[
            (refl_a, FailureCategory::ToolFailures),
            (refl_a2, FailureCategory::Stall),
            (refl_b, FailureCategory::General),
        ]);
        assert_eq!(lessons.len(), 2);
        let rules: Vec<&str> = lessons.iter().map(|l| l.rule.as_str()).collect();
        assert!(rules.contains(&"advice a"));
        assert!(rules.contains(&"advice b"));
    }

    #[test]
    fn session_lessons_filters_low_confidence() {
        let refl = make_reflection("good advice", "reason", 0.2);
        let lessons = session_lessons(&[(refl, FailureCategory::General)]);
        assert!(lessons.is_empty());
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
