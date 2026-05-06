//! Budget-adaptive runtime introspection for the `introspect` pinned tool.
//!
//! The LLM calls `introspect` to query its own session state — token pressure,
//! cache efficiency, tool health, active alerts, and working memory. Output
//! detail scales with available context budget so the tool never wastes tokens
//! on verbose diagnostics when the model is under pressure.

use serde::{Deserialize, Serialize};

/// Input snapshot provided by the runtime to the introspect renderer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntrospectSnapshot {
    pub token_pressure: f64,
    pub cache_hit_ratio: f64,
    pub turns_completed: u32,
    pub turns_remaining: u32,
    pub compaction_tier: String,
    pub alerts: Vec<String>,
    pub tool_health: Vec<ToolHealthEntry>,
    pub working_memory_summary: String,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

/// Per-tool health entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolHealthEntry {
    pub name: String,
    pub calls: u32,
    pub errors: u32,
    pub avg_ms: u64,
}

/// Output detail level — chosen by budget or explicit arg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntrospectDetail {
    /// Full diagnostics (~500-800 tokens output).
    Full,
    /// Key metrics + top alerts (~150-250 tokens).
    Summary,
    /// One-liner (~30-50 tokens).
    Minimal,
}

impl IntrospectDetail {
    /// Auto-select detail level from remaining token budget.
    pub fn from_budget(remaining_tokens: u32) -> Self {
        if remaining_tokens > 5000 {
            Self::Full
        } else if remaining_tokens > 2000 {
            Self::Summary
        } else {
            Self::Minimal
        }
    }

    /// Parse from tool argument string.
    pub fn from_arg(arg: &str) -> Self {
        match arg.trim().to_ascii_lowercase().as_str() {
            "full" | "detailed" | "verbose" => Self::Full,
            "summary" | "brief" => Self::Summary,
            "minimal" | "min" | "one-liner" => Self::Minimal,
            _ => Self::Summary,
        }
    }
}

/// Render the introspect output at the requested detail level.
pub fn render_introspect(snapshot: &IntrospectSnapshot, detail: IntrospectDetail) -> String {
    match detail {
        IntrospectDetail::Minimal => render_minimal(snapshot),
        IntrospectDetail::Summary => render_summary(snapshot),
        IntrospectDetail::Full => render_full(snapshot),
    }
}

fn render_minimal(s: &IntrospectSnapshot) -> String {
    format!(
        "pressure={:.0}% cache={:.0}% turns={}/{} alerts={} tier={}",
        s.token_pressure * 100.0,
        s.cache_hit_ratio * 100.0,
        s.turns_completed,
        s.turns_completed + s.turns_remaining,
        s.alerts.len(),
        s.compaction_tier,
    )
}

fn render_summary(s: &IntrospectSnapshot) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "## Session Health\n\
         Pressure: {:.0}% | Cache: {:.0}% | Turns: {}/{} | Tier: {}\n\
         Tokens: {}in + {}out ({}cached_read, {}cached_create)\n",
        s.token_pressure * 100.0,
        s.cache_hit_ratio * 100.0,
        s.turns_completed,
        s.turns_completed + s.turns_remaining,
        s.compaction_tier,
        s.total_input_tokens,
        s.total_output_tokens,
        s.cache_read_tokens,
        s.cache_creation_tokens,
    ));
    if !s.alerts.is_empty() {
        out.push_str("Alerts:\n");
        for alert in s.alerts.iter().take(3) {
            out.push_str("- ");
            out.push_str(alert);
            out.push('\n');
        }
        if s.alerts.len() > 3 {
            out.push_str(&format!("  (+{} more)\n", s.alerts.len() - 3));
        }
    }
    if !s.working_memory_summary.is_empty() {
        out.push_str(&s.working_memory_summary);
        out.push('\n');
    }
    out.trim_end().to_string()
}

fn render_full(s: &IntrospectSnapshot) -> String {
    let mut out = render_summary(s);
    out.push('\n');

    if !s.tool_health.is_empty() {
        out.push_str("\n## Tool Health\n");
        out.push_str("| Tool | Calls | Errors | Avg ms |\n");
        out.push_str("|------|-------|--------|--------|\n");
        for t in &s.tool_health {
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                t.name, t.calls, t.errors, t.avg_ms
            ));
        }
    }

    if s.alerts.len() > 3 {
        out.push_str("\n## All Alerts\n");
        for alert in &s.alerts {
            out.push_str("- ");
            out.push_str(alert);
            out.push('\n');
        }
    }

    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_snapshot() -> IntrospectSnapshot {
        IntrospectSnapshot {
            token_pressure: 0.72,
            cache_hit_ratio: 0.65,
            turns_completed: 8,
            turns_remaining: 12,
            compaction_tier: "Normal".into(),
            alerts: vec![
                "cache_regression: hit rate dropped 20% in 3 turns".into(),
                "tool_health: bash error rate >30%".into(),
            ],
            tool_health: vec![
                ToolHealthEntry { name: "bash".into(), calls: 15, errors: 5, avg_ms: 2300 },
                ToolHealthEntry { name: "read_file".into(), calls: 22, errors: 0, avg_ms: 12 },
                ToolHealthEntry { name: "grep".into(), calls: 8, errors: 1, avg_ms: 45 },
            ],
            working_memory_summary: "Goal: implement streaming resume".into(),
            total_input_tokens: 145_000,
            total_output_tokens: 12_000,
            cache_read_tokens: 95_000,
            cache_creation_tokens: 8_000,
        }
    }

    #[test]
    fn minimal_is_single_line() {
        let output = render_introspect(&sample_snapshot(), IntrospectDetail::Minimal);
        assert!(!output.contains('\n'), "minimal must be a single line: {output}");
        assert!(output.contains("pressure=72%"));
        assert!(output.contains("cache=65%"));
        assert!(output.contains("turns=8/20"));
        assert!(output.contains("alerts=2"));
    }

    #[test]
    fn summary_includes_key_metrics_and_top_alerts() {
        let output = render_introspect(&sample_snapshot(), IntrospectDetail::Summary);
        assert!(output.contains("## Session Health"));
        assert!(output.contains("cache_regression"));
        assert!(output.contains("Goal: implement streaming resume"));
        // Should NOT contain full tool table
        assert!(!output.contains("| Tool |"));
    }

    #[test]
    fn full_includes_tool_health_table() {
        let output = render_introspect(&sample_snapshot(), IntrospectDetail::Full);
        assert!(output.contains("## Tool Health"));
        assert!(output.contains("| bash |"));
        assert!(output.contains("| read_file |"));
    }

    #[test]
    fn detail_from_budget_selects_correctly() {
        assert_eq!(IntrospectDetail::from_budget(10000), IntrospectDetail::Full);
        assert_eq!(IntrospectDetail::from_budget(3000), IntrospectDetail::Summary);
        assert_eq!(IntrospectDetail::from_budget(1000), IntrospectDetail::Minimal);
    }

    #[test]
    fn detail_from_arg_parses_variants() {
        assert_eq!(IntrospectDetail::from_arg("full"), IntrospectDetail::Full);
        assert_eq!(IntrospectDetail::from_arg("brief"), IntrospectDetail::Summary);
        assert_eq!(IntrospectDetail::from_arg("min"), IntrospectDetail::Minimal);
        assert_eq!(IntrospectDetail::from_arg("unknown"), IntrospectDetail::Summary);
    }

    #[test]
    fn empty_snapshot_renders_without_panic() {
        let empty = IntrospectSnapshot::default();
        let min = render_introspect(&empty, IntrospectDetail::Minimal);
        assert!(min.contains("pressure=0%"));
        let full = render_introspect(&empty, IntrospectDetail::Full);
        assert!(!full.contains("## Tool Health")); // empty tool_health = no table
    }

    #[test]
    fn many_alerts_truncated_in_summary_shown_in_full() {
        let mut s = sample_snapshot();
        s.alerts = (0..10).map(|i| format!("alert-{i}")).collect();
        let summary = render_introspect(&s, IntrospectDetail::Summary);
        assert!(summary.contains("(+7 more)"));
        let full = render_introspect(&s, IntrospectDetail::Full);
        assert!(full.contains("## All Alerts"));
        assert!(full.contains("alert-9"));
    }
}
