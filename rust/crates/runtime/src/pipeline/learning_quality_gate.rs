//! Learning Quality Gate — filters out low-value or derivable signals before
//! they enter the learning pipeline.
//!
//! Inspired by Claude Code's "what not to save" rules: code patterns,
//! architecture, git history, and file structure are derivable from the
//! codebase and should not pollute the learning system.

use crate::turn::contracts::TurnLearningOutcome;

/// Reasons a learning outcome was rejected by the quality gate.
#[derive(Debug, Clone, PartialEq)]
pub enum GateRejection {
    /// Query is a trivial/generic request that doesn't carry learning signal.
    TrivialQuery,
    /// The outcome duplicates information derivable from code/git.
    DerivableFromCode,
    /// Tool chain is too short to carry meaningful pattern signal.
    InsufficientToolChain,
    /// Quality score is in the ambiguous middle zone — not clearly good or bad.
    AmbiguousQuality,
}

/// Patterns that indicate a query is about derivable information (code structure,
/// git history, file listing) rather than user preferences or domain knowledge.
static DERIVABLE_PATTERNS: &[&str] = &[
    "list files",
    "show directory",
    "what's in",
    "cat ",
    "read file",
    "git log",
    "git status",
    "git diff",
    "ls ",
    "find ",
    "grep ",
    "列出文件",
    "查看目录",
    "显示文件",
    "git 日志",
];

/// Trivial queries that don't carry learning signal.
static TRIVIAL_PATTERNS: &[&str] = &[
    "hello",
    "hi",
    "hey",
    "thanks",
    "thank you",
    "ok",
    "okay",
    "你好",
    "谢谢",
    "好的",
];

/// Evaluate whether a learning outcome should enter the pipeline.
///
/// Returns `Ok(())` if the outcome passes the gate, or `Err(reason)` if rejected.
pub fn evaluate(outcome: &TurnLearningOutcome) -> Result<(), GateRejection> {
    let query_lower = outcome.query.to_lowercase();

    // Reject trivial greetings/acknowledgments
    let trimmed = query_lower.trim();
    if trimmed.split_whitespace().count() <= 3
        && TRIVIAL_PATTERNS.iter().any(|p| trimmed.starts_with(p))
    {
        return Err(GateRejection::TrivialQuery);
    }

    // Reject queries about derivable information
    if DERIVABLE_PATTERNS.iter().any(|p| query_lower.contains(p)) && !outcome.was_corrected {
        return Err(GateRejection::DerivableFromCode);
    }

    // Reject outcomes with no tools used (nothing to learn from)
    if outcome.tools_used.is_empty() {
        return Err(GateRejection::InsufficientToolChain);
    }

    // Reject ambiguous quality (0.35–0.65) unless there's a correction signal
    if !outcome.was_corrected
        && outcome.quality > 0.35
        && outcome.quality < 0.65
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
    fn rejects_trivial_greeting() {
        let o = make_outcome("hello", &["bash"], 0.8, false);
        assert_eq!(evaluate(&o), Err(GateRejection::TrivialQuery));
    }

    #[test]
    fn rejects_trivial_chinese() {
        let o = make_outcome("你好", &["bash"], 0.8, false);
        assert_eq!(evaluate(&o), Err(GateRejection::TrivialQuery));
    }

    #[test]
    fn rejects_derivable_file_listing() {
        let o = make_outcome("list files in src/", &["bash"], 0.9, false);
        assert_eq!(evaluate(&o), Err(GateRejection::DerivableFromCode));
    }

    #[test]
    fn allows_derivable_if_corrected() {
        let o = make_outcome("list files in src/", &["bash"], 0.9, true);
        assert!(evaluate(&o).is_ok());
    }

    #[test]
    fn rejects_no_tools() {
        let o = make_outcome("explain rust lifetimes", &[], 0.5, false);
        assert_eq!(evaluate(&o), Err(GateRejection::InsufficientToolChain));
    }

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
    fn allows_low_quality_failure() {
        let o = make_outcome("deploy to staging", &["bash"], 0.2, false);
        assert!(evaluate(&o).is_ok());
    }

    #[test]
    fn allows_high_quality_success() {
        let o = make_outcome(
            "fix the bug in parser",
            &["read_file", "write_file"],
            0.9,
            false,
        );
        assert!(evaluate(&o).is_ok());
    }

    #[test]
    fn rejects_git_log_query() {
        let o = make_outcome("show me git log for last week", &["bash"], 0.8, false);
        assert_eq!(evaluate(&o), Err(GateRejection::DerivableFromCode));
    }

    #[test]
    fn rejects_chinese_derivable() {
        let o = make_outcome("查看目录结构", &["bash"], 0.8, false);
        assert_eq!(evaluate(&o), Err(GateRejection::DerivableFromCode));
    }
}
