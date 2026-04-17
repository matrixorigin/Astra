//! Learning Quality Gate — filters out low-value or derivable signals before
//! they enter the learning pipeline.
//!
//! Filters out low-value or derivable signals: code patterns,
//! architecture, git history, and file structure are derivable from the
//! codebase and should not pollute the learning system.

use crate::contracts::TurnLearningOutcome;

/// Reasons a learning outcome was rejected by the quality gate.
#[derive(Debug, Clone, PartialEq)]
pub enum GateRejection {
    /// Query is a trivial/generic request that doesn't carry learning signal.
    TrivialQuery,
    /// The outcome only used read-only tools — derivable from the codebase.
    DerivableFromCode,
    /// No tools were used — nothing to learn from.
    InsufficientToolChain,
    /// Quality score is in the ambiguous middle zone — not clearly good or bad.
    AmbiguousQuality,
}

/// Ambiguous quality lower bound (exclusive).
const AMBIGUOUS_QUALITY_LOW: f64 = 0.35;
/// Ambiguous quality upper bound (exclusive).
const AMBIGUOUS_QUALITY_HIGH: f64 = 0.65;

/// Tools that only read/observe — outcomes using exclusively these tools
/// are derivable from the codebase and don't carry learning signal.
static READ_ONLY_TOOLS: &[&str] = &[
    "read_file",
    "list_dir",
    "glob",
    "grep",
    "find_definition",
    "find_references",
    "symbols",
    "hover_info",
    "git_log",
    "git_diff",
    "git_status",
    "git_show",
];

/// Evaluate whether a learning outcome should enter the pipeline.
///
/// Returns `Ok(())` if the outcome passes the gate, or `Err(reason)` if rejected.
pub fn evaluate(outcome: &TurnLearningOutcome) -> Result<(), GateRejection> {
    // Reject outcomes with no tools used (nothing to learn from)
    if outcome.tools_used.is_empty() {
        return Err(GateRejection::InsufficientToolChain);
    }

    // Reject trivial queries (very short, no substance)
    let trimmed = outcome.query.trim();
    if trimmed.is_empty() || trimmed.chars().count() <= 5 {
        return Err(GateRejection::TrivialQuery);
    }

    // Reject outcomes that only used read-only tools (derivable from codebase)
    // unless the user corrected the agent (correction on a read is still valuable)
    if !outcome.was_corrected
        && outcome
            .tools_used
            .iter()
            .all(|t| READ_ONLY_TOOLS.iter().any(|ro| t == ro))
    {
        return Err(GateRejection::DerivableFromCode);
    }

    // Reject ambiguous quality unless there's a correction signal or user feedback
    if !outcome.was_corrected
        && outcome.quality > AMBIGUOUS_QUALITY_LOW
        && outcome.quality < AMBIGUOUS_QUALITY_HIGH
        && outcome.user_feedback_score.is_none()
    {
        return Err(GateRejection::AmbiguousQuality);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_outcome(
        query: &str,
        tools: &[&str],
        quality: f64,
        corrected: bool,
    ) -> TurnLearningOutcome {
        TurnLearningOutcome {
            query: query.into(),
            tools_selected: tools.iter().map(|s| s.to_string()).collect(),
            tools_used: tools.iter().map(|s| s.to_string()).collect(),
            success: quality > 0.3,
            quality,
            was_corrected: corrected,
            task_type_label: None,
            domain_hint_label: None,
            user_feedback_score: None,
            reward_hacking_risk: 0.0,
            reward_hacking_flags: Vec::new(),
            causal_support_score: 1.0,
            causal_support_flags: Vec::new(),
        }
    }

    // ── Happy path ──

    #[test]
    fn passes_normal_outcome() {
        let o = make_outcome(
            "refactor the auth module",
            &["read_file", "write_file"],
            0.85,
            false,
        );
        assert!(evaluate(&o).is_ok());
    }

    #[test]
    fn passes_low_quality_failure() {
        let o = make_outcome("deploy to staging", &["bash"], 0.2, false);
        assert!(evaluate(&o).is_ok());
    }

    #[test]
    fn passes_high_quality_success() {
        let o = make_outcome("fix the bug", &["read_file", "write_file"], 0.9, false);
        assert!(evaluate(&o).is_ok());
    }

    // ── Trivial query ──

    #[test]
    fn rejects_empty_query() {
        let o = make_outcome("", &["bash"], 0.8, false);
        assert_eq!(evaluate(&o), Err(GateRejection::TrivialQuery));
    }

    #[test]
    fn rejects_very_short_query() {
        let o = make_outcome("hi", &["bash"], 0.8, false);
        assert_eq!(evaluate(&o), Err(GateRejection::TrivialQuery));
    }

    #[test]
    fn rejects_short_chinese() {
        let o = make_outcome("你好", &["bash"], 0.8, false);
        assert_eq!(evaluate(&o), Err(GateRejection::TrivialQuery));
    }

    #[test]
    fn passes_six_char_query() {
        let o = make_outcome("fix it now", &["bash"], 0.8, false);
        assert!(evaluate(&o).is_ok());
    }

    // ── Derivable (read-only tools) ──

    #[test]
    fn rejects_read_only_tools() {
        let o = make_outcome("show me the code", &["read_file", "grep"], 0.9, false);
        assert_eq!(evaluate(&o), Err(GateRejection::DerivableFromCode));
    }

    #[test]
    fn rejects_git_read_only() {
        let o = make_outcome("show recent changes", &["git_log", "git_diff"], 0.8, false);
        assert_eq!(evaluate(&o), Err(GateRejection::DerivableFromCode));
    }

    #[test]
    fn allows_read_only_if_corrected() {
        let o = make_outcome("show me the code", &["read_file", "grep"], 0.9, true);
        assert!(evaluate(&o).is_ok());
    }

    #[test]
    fn allows_mixed_read_write_tools() {
        let o = make_outcome(
            "update the config",
            &["read_file", "write_file"],
            0.9,
            false,
        );
        assert!(evaluate(&o).is_ok());
    }

    #[test]
    fn allows_bash_not_in_read_only_list() {
        let o = make_outcome("run the tests", &["bash"], 0.8, false);
        assert!(evaluate(&o).is_ok());
    }

    // ── No tools ──

    #[test]
    fn rejects_no_tools() {
        let o = make_outcome("explain rust lifetimes", &[], 0.5, false);
        assert_eq!(evaluate(&o), Err(GateRejection::InsufficientToolChain));
    }

    // ── Ambiguous quality ──

    #[test]
    fn rejects_ambiguous_quality() {
        let o = make_outcome("update the config", &["write_file"], 0.5, false);
        assert_eq!(evaluate(&o), Err(GateRejection::AmbiguousQuality));
    }

    #[test]
    fn allows_ambiguous_quality_if_corrected() {
        let o = make_outcome("update the config", &["write_file"], 0.5, true);
        assert!(evaluate(&o).is_ok());
    }

    #[test]
    fn allows_ambiguous_quality_with_feedback() {
        let mut o = make_outcome("update the config", &["write_file"], 0.5, false);
        o.user_feedback_score = Some(80);
        assert!(evaluate(&o).is_ok());
    }

    #[test]
    fn boundary_quality_035_passes() {
        let o = make_outcome("do something", &["bash"], 0.35, false);
        assert!(evaluate(&o).is_ok());
    }

    #[test]
    fn boundary_quality_065_passes() {
        let o = make_outcome("do something", &["bash"], 0.65, false);
        assert!(evaluate(&o).is_ok());
    }

    #[test]
    fn just_above_035_rejected() {
        let o = make_outcome("do something", &["bash"], 0.36, false);
        assert_eq!(evaluate(&o), Err(GateRejection::AmbiguousQuality));
    }

    #[test]
    fn just_below_065_rejected() {
        let o = make_outcome("do something", &["bash"], 0.64, false);
        assert_eq!(evaluate(&o), Err(GateRejection::AmbiguousQuality));
    }

    // ── No false positives on legitimate queries ──

    #[test]
    fn does_not_reject_cat_in_normal_query() {
        // "cat" as substring should NOT trigger false positive
        let o = make_outcome("catalog the API endpoints", &["bash"], 0.8, false);
        assert!(evaluate(&o).is_ok());
    }

    #[test]
    fn does_not_reject_ok_in_longer_query() {
        let o = make_outcome(
            "ok let's refactor the auth module",
            &["write_file"],
            0.8,
            false,
        );
        assert!(evaluate(&o).is_ok());
    }
}
