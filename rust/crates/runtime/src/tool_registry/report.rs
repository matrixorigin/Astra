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
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SelectionFeedback {
    pub tools_used: Vec<String>,
    pub unused_count: u32,
    pub precision: f64,
}

impl SelectionReport {
    /// Compare selection against actual LLM usage to produce feedback.
    #[allow(dead_code)]
    pub fn feedback(&self, tools_used: &[String]) -> SelectionFeedback {
        let selected_set: std::collections::HashSet<&str> =
            self.tools_selected.iter().map(|s| s.as_str()).collect();
        let used_set: std::collections::HashSet<&str> =
            tools_used.iter().map(|s| s.as_str()).collect();

        let hits = used_set.intersection(&selected_set).count();
        let precision = if used_set.is_empty() {
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
        }
    }
}
