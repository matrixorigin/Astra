/// Report of a tool selection decision — captures what was selected and why.
/// Designed for journal persistence and quality analysis.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SelectionReport {
    /// Tool names that were selected for the LLM request.
    pub tools_selected: Vec<String>,
    /// Number of tools selected.
    pub selected_count: u32,
    /// Token budget used by selected dynamic tools.
    pub budget_used: u32,
    /// Token budget that was available.
    pub budget_total: u32,
}

/// Feedback from a completed turn — what the LLM actually used.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SelectionFeedback {
    pub tools_used: Vec<String>,
    pub unused_count: u32,
    pub precision: f64,
    pub recall: f64,
}

impl SelectionReport {
    /// Compare selection against actual LLM usage to produce feedback.
    pub fn feedback(&self, tools_used: &[String]) -> SelectionFeedback {
        let selected_set: std::collections::HashSet<&str> =
            self.tools_selected.iter().map(|s| s.as_str()).collect();
        let used_set: std::collections::HashSet<&str> =
            tools_used.iter().map(|s| s.as_str()).collect();

        let hits = used_set.intersection(&selected_set).count();
        let precision = if selected_set.is_empty() {
            1.0
        } else {
            hits as f64 / selected_set.len() as f64
        };
        let recall = if used_set.is_empty() {
            1.0
        } else {
            hits as f64 / used_set.len() as f64
        };

        let unused_count = selected_set
            .len()
            .saturating_sub(selected_set.intersection(&used_set).count())
            as u32;

        SelectionFeedback {
            tools_used: tools_used.to_vec(),
            unused_count,
            precision,
            recall,
        }
    }
}

// ─── Tool Quality Tracker ───────────────────────────────────────────────────

/// Tracks per-tool quality scores across a session, enabling feedback-driven
/// tool selection. Tools that consistently produce good results get boosted;
/// tools that are selected but never used get penalized.
#[derive(Debug, Clone, Default)]
pub struct ToolQualityTracker {
    /// tool_name → (total_selections, times_actually_used, quality_sum)
    scores: std::collections::HashMap<String, ToolQualityEntry>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ToolQualityEntry {
    pub selections: u32,
    pub uses: u32,
    pub quality_sum: f64,
}

impl ToolQualityEntry {
    /// Effectiveness score: how often this tool is actually used when selected.
    /// Range: 0.0 to 1.0.
    pub fn use_rate(&self) -> f64 {
        if self.selections == 0 {
            return 0.5; // neutral for untracked tools
        }
        self.uses as f64 / self.selections as f64
    }

    /// Average quality score from tool_quality_assessments.
    /// Range: 0.0 to 1.0. Returns 0.5 (neutral) if no quality data.
    pub fn avg_quality(&self) -> f64 {
        if self.uses == 0 {
            return 0.5;
        }
        self.quality_sum / self.uses as f64
    }

    /// Combined boost factor for tool selection scoring.
    /// > 1.0 means boost, < 1.0 means penalize. Range: 0.5 to 1.5.
    pub fn boost_factor(&self) -> f64 {
        if self.selections < 3 {
            return 1.0; // not enough data
        }
        let use_factor = self.use_rate(); // 0.0-1.0
        let quality_factor = self.avg_quality(); // 0.0-1.0
        // Weighted: 60% use rate + 40% quality
        let raw = use_factor * 0.6 + quality_factor * 0.4;
        // Map [0, 1] → [0.5, 1.5]
        0.5 + raw
    }
}

impl ToolQualityTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that tools were selected for an LLM request.
    pub fn record_selection(&mut self, tools: &[String]) {
        for tool in tools {
            self.scores.entry(tool.clone()).or_default().selections += 1;
        }
    }

    /// Record feedback from a completed turn.
    pub fn record_feedback(&mut self, feedback: &SelectionFeedback) {
        for tool in &feedback.tools_used {
            self.scores.entry(tool.clone()).or_default().uses += 1;
        }
    }

    /// Record quality assessment for a specific tool.
    pub fn record_quality(&mut self, tool: &str, quality_score: f64) {
        let entry = self.scores.entry(tool.to_string()).or_default();
        entry.quality_sum += quality_score.clamp(0.0, 1.0);
    }

    /// Get the boost factor for a tool. > 1.0 = boost, < 1.0 = penalize.
    pub fn boost_factor(&self, tool: &str) -> f64 {
        self.scores
            .get(tool)
            .map(|e| e.boost_factor())
            .unwrap_or(1.0)
    }

    /// Get all tracked tool quality entries.
    pub fn all_entries(&self) -> &std::collections::HashMap<String, ToolQualityEntry> {
        &self.scores
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── SelectionReport::feedback ──

    #[test]
    fn feedback_perfect_match() {
        let report = SelectionReport {
            tools_selected: vec!["bash".into(), "grep".into()],
            selected_count: 2,
            budget_used: 50,
            budget_total: 800,
        };
        let fb = report.feedback(&["bash".into(), "grep".into()]);
        assert_eq!(fb.precision, 1.0);
        assert_eq!(fb.recall, 1.0);
        assert_eq!(fb.unused_count, 0);
    }

    #[test]
    fn feedback_partial_use() {
        let report = SelectionReport {
            tools_selected: vec!["bash".into(), "grep".into(), "glob".into()],
            selected_count: 3,
            budget_used: 75,
            budget_total: 800,
        };
        let fb = report.feedback(&["bash".into()]);
        assert!((fb.precision - 1.0 / 3.0).abs() < 0.01);
        assert_eq!(fb.recall, 1.0);
        assert_eq!(fb.unused_count, 2);
    }

    #[test]
    fn feedback_llm_used_unselected() {
        let report = SelectionReport {
            tools_selected: vec!["bash".into()],
            selected_count: 1,
            budget_used: 25,
            budget_total: 800,
        };
        let fb = report.feedback(&["bash".into(), "grep".into()]);
        assert_eq!(fb.precision, 1.0);
        assert!((fb.recall - 0.5).abs() < 0.01);
    }

    #[test]
    fn feedback_empty_usage() {
        let report = SelectionReport {
            tools_selected: vec!["bash".into()],
            selected_count: 1,
            budget_used: 25,
            budget_total: 800,
        };
        let fb = report.feedback(&[]);
        assert_eq!(fb.recall, 1.0);
        assert_eq!(fb.unused_count, 1);
    }

    // ── ToolQualityTracker ──

    #[test]
    fn tracker_neutral_for_unknown_tool() {
        let tracker = ToolQualityTracker::new();
        assert!((tracker.boost_factor("unknown") - 1.0).abs() < 0.01);
    }

    #[test]
    fn tracker_needs_min_selections() {
        let mut tracker = ToolQualityTracker::new();
        tracker.record_selection(&["bash".into()]);
        tracker.record_selection(&["bash".into()]);
        // Only 2 selections — not enough data
        assert!((tracker.boost_factor("bash") - 1.0).abs() < 0.01);
    }

    #[test]
    fn tracker_penalizes_unused_tool() {
        let mut tracker = ToolQualityTracker::new();
        for _ in 0..5 {
            tracker.record_selection(&["glob".into()]);
        }
        // Never used — should penalize
        let boost = tracker.boost_factor("glob");
        assert!(boost < 1.0, "unused tool should be penalized: {boost}");
    }

    #[test]
    fn tracker_boosts_effective_tool() {
        let mut tracker = ToolQualityTracker::new();
        for _ in 0..5 {
            tracker.record_selection(&["bash".into()]);
            tracker.record_feedback(&SelectionFeedback {
                tools_used: vec!["bash".into()],
                unused_count: 0,
                precision: 1.0,
                recall: 1.0,
            });
            tracker.record_quality("bash", 0.9);
        }
        let boost = tracker.boost_factor("bash");
        assert!(boost > 1.0, "effective tool should be boosted: {boost}");
    }

    #[test]
    fn tracker_quality_affects_boost() {
        let mut tracker = ToolQualityTracker::new();
        for _ in 0..5 {
            tracker.record_selection(&["grep".into()]);
            tracker.record_feedback(&SelectionFeedback {
                tools_used: vec!["grep".into()],
                unused_count: 0,
                precision: 1.0,
                recall: 1.0,
            });
        }
        // High quality
        for _ in 0..5 {
            tracker.record_quality("grep", 1.0);
        }
        let high_boost = tracker.boost_factor("grep");

        let mut tracker2 = ToolQualityTracker::new();
        for _ in 0..5 {
            tracker2.record_selection(&["grep".into()]);
            tracker2.record_feedback(&SelectionFeedback {
                tools_used: vec!["grep".into()],
                unused_count: 0,
                precision: 1.0,
                recall: 1.0,
            });
        }
        // Low quality
        for _ in 0..5 {
            tracker2.record_quality("grep", 0.2);
        }
        let low_boost = tracker2.boost_factor("grep");

        assert!(
            high_boost > low_boost,
            "high quality {high_boost} should beat low quality {low_boost}"
        );
    }
}
