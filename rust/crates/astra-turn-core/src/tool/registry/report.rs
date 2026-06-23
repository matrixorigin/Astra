/// Report of a tool surface decision — captures what was visible.
/// Designed for journal persistence and quality analysis.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolSurfaceReport {
    /// Tool names visible to the LLM request.
    pub visible_tools: Vec<String>,
    /// Number of tools visible.
    pub visible_count: u32,
    /// Token cost used by the visible tools in this report.
    ///
    /// Final payload reports use the full visible tool surface so this field
    /// has the same denominator as `visible_count`.
    pub budget_used: u32,
    /// Token budget that was available.
    pub budget_total: u32,
}

/// Feedback from a completed turn — what the LLM actually used.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolSurfaceFeedback {
    pub tools_used: Vec<String>,
    pub unused_count: u32,
    pub precision: f64,
    pub recall: f64,
}

impl ToolSurfaceReport {
    /// Compare the visible surface against actual LLM usage to produce feedback.
    pub fn feedback(&self, tools_used: &[String]) -> ToolSurfaceFeedback {
        let visible_set: std::collections::HashSet<&str> =
            self.visible_tools.iter().map(|s| s.as_str()).collect();
        let used_set: std::collections::HashSet<&str> =
            tools_used.iter().map(|s| s.as_str()).collect();

        let hits = used_set.intersection(&visible_set).count();
        let precision = if visible_set.is_empty() {
            1.0
        } else {
            hits as f64 / visible_set.len() as f64
        };
        let recall = if used_set.is_empty() {
            1.0
        } else {
            hits as f64 / used_set.len() as f64
        };

        let unused_count = visible_set
            .len()
            .saturating_sub(visible_set.intersection(&used_set).count())
            as u32;

        ToolSurfaceFeedback {
            tools_used: tools_used.to_vec(),
            unused_count,
            precision,
            recall,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ToolSurfaceReport::feedback ──

    #[test]
    fn feedback_perfect_match() {
        let report = ToolSurfaceReport {
            visible_tools: vec!["bash".into(), "grep".into()],
            visible_count: 2,
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
        let report = ToolSurfaceReport {
            visible_tools: vec!["bash".into(), "grep".into(), "glob".into()],
            visible_count: 3,
            budget_used: 75,
            budget_total: 800,
        };
        let fb = report.feedback(&["bash".into()]);
        assert!((fb.precision - 1.0 / 3.0).abs() < 0.01);
        assert_eq!(fb.recall, 1.0);
        assert_eq!(fb.unused_count, 2);
    }

    #[test]
    fn feedback_llm_used_tool_not_visible() {
        let report = ToolSurfaceReport {
            visible_tools: vec!["bash".into()],
            visible_count: 1,
            budget_used: 25,
            budget_total: 800,
        };
        let fb = report.feedback(&["bash".into(), "grep".into()]);
        assert_eq!(fb.precision, 1.0);
        assert!((fb.recall - 0.5).abs() < 0.01);
    }

    #[test]
    fn feedback_empty_usage() {
        let report = ToolSurfaceReport {
            visible_tools: vec!["bash".into()],
            visible_count: 1,
            budget_used: 25,
            budget_total: 800,
        };
        let fb = report.feedback(&[]);
        assert_eq!(fb.recall, 1.0);
        assert_eq!(fb.unused_count, 1);
    }
}
