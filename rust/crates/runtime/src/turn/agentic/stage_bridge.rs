//! Lightweight types for surfacing tool-selection strategy changes.
//!
//! The old rule-based pipeline bridge was retired, but the runtime still keeps
//! stable summary structs for reporting blocked/boosted/widened tool-selection
//! changes into observability and self-model rendering.

/// Summary of what the strategy-delta application actually changed in the
/// runtime state — used for logging / tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StrategyApplication {
    /// Tools newly inserted into `state.restricted_tools`.
    pub newly_blocked: Vec<String>,
    /// Tools already present (no-op inserts).
    pub already_blocked: Vec<String>,
    /// Whether `widen_selection` was requested by the strategy (and thus the
    /// one-shot `state.widen_selection_pending` flag was set).
    pub widen_requested: bool,
    /// Tools newly added to `state.boosted_tools`.
    pub newly_boosted: Vec<String>,
    /// Tools already present in `state.boosted_tools`.
    pub already_boosted: Vec<String>,
    /// Optional rich before/after snapshot of the affected skill surfaces.
    /// `None` on noop; populated when the application produced at least one
    /// newly-boosted/blocked entry or toggled widen_selection. P3.1.
    pub diff_entry: Option<SkillDiffEntry>,
}

/// P3.1: a before/after snapshot of a "skill" surface that a strategy-delta
/// application materially changed. Lightweight, stable-ordered, JSON-ready.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SkillDiffEntry {
    /// Logical subsystem whose configuration changed. Current only value is
    /// `"pipeline.tool_selection"` but kept as `String` for forward-compat.
    pub skill: String,
    pub before: DiffSnapshot,
    pub after: DiffSnapshot,
    /// Human-readable reason for the change (e.g. `"auto-reflection"`).
    pub reason: String,
}

/// Stable, sorted view of the three surfaces a strategy-delta can mutate.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiffSnapshot {
    pub blocked_tools: Vec<String>,
    pub boosted_tools: Vec<String>,
    pub widen_pending: bool,
}

impl SkillDiffEntry {
    /// Compact single-line rendering for prompts and slash views.
    pub fn summary_line(&self) -> String {
        let mut parts = Vec::new();
        let added_blocked: Vec<&String> = self
            .after
            .blocked_tools
            .iter()
            .filter(|t| !self.before.blocked_tools.contains(t))
            .collect();
        if !added_blocked.is_empty() {
            parts.push(format!("+blocked={added_blocked:?}"));
        }
        let added_boosted: Vec<&String> = self
            .after
            .boosted_tools
            .iter()
            .filter(|t| !self.before.boosted_tools.contains(t))
            .collect();
        if !added_boosted.is_empty() {
            parts.push(format!("+boosted={added_boosted:?}"));
        }
        if !self.before.widen_pending && self.after.widen_pending {
            parts.push("+widen".to_string());
        }
        let change = if parts.is_empty() {
            "noop".to_string()
        } else {
            parts.join(" ")
        };
        format!("{} ({}) — {}", self.skill, self.reason, change)
    }
}

impl StrategyApplication {
    pub fn is_noop(&self) -> bool {
        self.newly_blocked.is_empty() && !self.widen_requested && self.newly_boosted.is_empty()
    }

    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.newly_blocked.is_empty() {
            parts.push(format!("blocked={:?}", self.newly_blocked));
        }
        if !self.already_blocked.is_empty() {
            parts.push(format!("already_blocked={:?}", self.already_blocked));
        }
        if !self.newly_boosted.is_empty() {
            parts.push(format!("boosted={:?}", self.newly_boosted));
        }
        if !self.already_boosted.is_empty() {
            parts.push(format!("already_boosted={:?}", self.already_boosted));
        }
        if self.widen_requested {
            parts.push("widen_selection=true".to_string());
        }
        if parts.is_empty() {
            "noop".to_string()
        } else {
            parts.join(", ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strategy_application_summary_reflects_widen_and_boost() {
        // Pure logic test on StrategyApplication (no AgenticLoopState needed).
        let app = StrategyApplication {
            newly_blocked: vec!["a".to_string()],
            already_blocked: vec![],
            widen_requested: true,
            newly_boosted: vec!["grep".to_string(), "read".to_string()],
            already_boosted: vec!["ls".to_string()],
            diff_entry: None,
        };
        assert!(!app.is_noop());
        let s = app.summary();
        assert!(s.contains("blocked=[\"a\"]"), "summary: {s}");
        assert!(s.contains("boosted=[\"grep\", \"read\"]"), "summary: {s}");
        assert!(s.contains("already_boosted=[\"ls\"]"), "summary: {s}");
        assert!(s.contains("widen_selection=true"), "summary: {s}");

        let noop = StrategyApplication::default();
        assert!(noop.is_noop());
        assert_eq!(noop.summary(), "noop");
    }

    #[test]
    fn skill_diff_entry_summary_line_renders_additions() {
        let entry = SkillDiffEntry {
            skill: "pipeline.tool_selection".to_string(),
            before: DiffSnapshot {
                blocked_tools: vec!["old".to_string()],
                boosted_tools: vec![],
                widen_pending: false,
            },
            after: DiffSnapshot {
                blocked_tools: vec!["flaky_http".to_string(), "old".to_string()],
                boosted_tools: vec!["grep".to_string()],
                widen_pending: true,
            },
            reason: "auto-reflection".to_string(),
        };
        let line = entry.summary_line();
        assert!(line.contains("pipeline.tool_selection"), "line: {line}");
        assert!(line.contains("auto-reflection"), "line: {line}");
        assert!(line.contains("+blocked=[\"flaky_http\"]"), "line: {line}");
        assert!(line.contains("+boosted=[\"grep\"]"), "line: {line}");
        assert!(line.contains("+widen"), "line: {line}");
    }

    #[test]
    fn skill_diff_entry_summary_line_noop_is_marked() {
        let entry = SkillDiffEntry {
            skill: "pipeline.tool_selection".to_string(),
            before: DiffSnapshot::default(),
            after: DiffSnapshot::default(),
            reason: "auto-reflection".to_string(),
        };
        assert!(entry.summary_line().contains("noop"));
    }
}
