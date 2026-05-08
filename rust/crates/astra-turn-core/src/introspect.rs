//! Budget-adaptive runtime introspection for the `introspect` pinned tool.
//!
//! The LLM calls `introspect` to query its own session state — token pressure,
//! cache efficiency, tool health, active alerts, and working memory. Output
//! detail scales with available context budget so the tool never wastes tokens
//! on verbose diagnostics when the model is under pressure.

pub mod cache_diagnosis;

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

    // ── Task #46: enhanced self-awareness ──
    /// Summary of the most recent LLM rounds (in-memory ring). Available
    /// regardless of `full_llm_capture` setting. Populated by
    /// `AgenticLoopState.recent_rounds`. Feeds `subtopic=recent`.
    #[serde(default)]
    pub recent_rounds: Vec<RoundSnapshotEntry>,
    /// Currently-pending volatile injections scheduled for the next LLM
    /// call (tool-health warnings, working-set snapshots, stall nudges,
    /// …). Lets the agent answer "what runtime nudges am I about to
    /// see?". Feeds `subtopic=volatile`.
    #[serde(default)]
    pub volatile_pending: Vec<VolatileSnapshotEntry>,
    /// Current stall / loop-guard telemetry — nudge count, event log,
    /// circuit breaker state. Feeds `subtopic=stall`.
    #[serde(default)]
    pub stall_state: StallSnapshotSummary,
}

/// Per-round summary surfaced through `introspect(subtopic=recent)`.
/// Mirrors `RecentRoundSummary` in the runtime but serializes cleanly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoundSnapshotEntry {
    pub turn: u32,
    pub round: u32,
    pub provider: String,
    pub model: String,
    pub prompt_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub completion_tokens: u64,
    pub tool_calls_returned: u32,
    pub tool_call_names: Vec<String>,
    pub duration_ms: u64,
    pub finish_reason: Option<String>,
}

/// Single entry in the volatile lane at introspect time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VolatileSnapshotEntry {
    /// Kind as a short string ("WorkingSet", "StallNudge", …). Keeps the
    /// core crate dependency-free from the runtime's `VolatileKind`.
    pub kind: String,
    /// Content preview — full text (the renderers may truncate at
    /// display time based on detail level).
    pub content: String,
    /// Round the injection was produced in.
    pub round_index: u32,
}

/// Stall / loop-guard state at introspect time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StallSnapshotSummary {
    pub nudge_count: u32,
    pub events: Vec<String>,
    /// Total circuit-breaker introspection emissions this turn.
    pub introspection_count: u32,
    pub forced_execution_escalation: bool,
    pub forced_parallel_batching: bool,
    pub forced_completion_soft_stop: bool,
    pub forced_redundant_reads_corrective: bool,
    pub forced_cache_waste_corrective: bool,
    pub forced_exploration_family_phase2: bool,
    pub forced_exploration_family_corrective: bool,
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

/// Render `subtopic=recent` — the in-memory ring of recent LLM rounds.
/// Compact table per round with tokens + tool counts + timing.
pub fn render_recent_rounds(s: &IntrospectSnapshot) -> String {
    if s.recent_rounds.is_empty() {
        return "## Recent Rounds\n(No rounds recorded yet in this turn.)".to_string();
    }
    let mut out = String::from(
        "## Recent Rounds (most recent last)\n\
         | t_r | provider | in | cached | cc_w | out | tools | dur_ms | finish |\n\
         |-----|----------|----|--------|------|-----|-------|--------|--------|\n",
    );
    for r in &s.recent_rounds {
        let provider = if r.provider.is_empty() {
            "-"
        } else {
            r.provider.as_str()
        };
        let tools_label = if r.tool_call_names.is_empty() {
            "-".to_string()
        } else {
            r.tool_call_names.join(",")
        };
        let finish = r.finish_reason.as_deref().unwrap_or("-");
        out.push_str(&format!(
            "| t{}_r{} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            r.turn,
            r.round,
            provider,
            r.prompt_tokens,
            r.cache_read_tokens,
            r.cache_creation_tokens,
            r.completion_tokens,
            tools_label,
            r.duration_ms,
            finish,
        ));
    }
    // Summary stats so the LLM gets a one-liner without re-totalling.
    let total_in: u64 = s.recent_rounds.iter().map(|r| r.prompt_tokens).sum();
    let total_cached: u64 = s.recent_rounds.iter().map(|r| r.cache_read_tokens).sum();
    let billable: u64 = total_in + total_cached;
    let pct = if billable > 0 {
        (total_cached as f64 / billable as f64) * 100.0
    } else {
        0.0
    };
    out.push_str(&format!(
        "\nRing: {} rounds, cache_hit={:.0}% ({}/{} billable).\n",
        s.recent_rounds.len(),
        pct,
        total_cached,
        billable,
    ));
    out
}

/// Render `subtopic=volatile` — what's queued in the volatile lane
/// right now (about to ride the next LLM call's preamble).
pub fn render_volatile_pending(s: &IntrospectSnapshot) -> String {
    if s.volatile_pending.is_empty() {
        return "## Volatile Lane\n(Empty — no pending runtime injections.)".to_string();
    }
    let mut out = String::from("## Volatile Lane (pending for next LLM call)\n");
    for (i, inj) in s.volatile_pending.iter().enumerate() {
        out.push_str(&format!(
            "- [{i}] **{kind}** (round {round}) — {preview}\n",
            i = i,
            kind = inj.kind,
            round = inj.round_index,
            preview = preview_line(&inj.content, 120),
        ));
    }
    out
}

/// Render `subtopic=stall` — stall / loop-guard telemetry.
pub fn render_stall_state(s: &IntrospectSnapshot) -> String {
    let st = &s.stall_state;
    let any_forced = st.forced_execution_escalation
        || st.forced_parallel_batching
        || st.forced_completion_soft_stop
        || st.forced_redundant_reads_corrective
        || st.forced_cache_waste_corrective
        || st.forced_exploration_family_phase2
        || st.forced_exploration_family_corrective;
    if st.nudge_count == 0 && st.events.is_empty() && !any_forced {
        return "## Stall / Loop-Guard\n(Healthy — no nudges, no forced corrections this turn.)"
            .to_string();
    }
    let mut out = String::from("## Stall / Loop-Guard\n");
    out.push_str(&format!(
        "Soft nudges: {} | Circuit-breaker introspections: {}\n",
        st.nudge_count, st.introspection_count,
    ));
    if !st.events.is_empty() {
        out.push_str("\n### Recent stall events\n");
        for e in &st.events {
            out.push_str("- ");
            out.push_str(e);
            out.push('\n');
        }
    }
    let mut forced: Vec<&str> = Vec::new();
    if st.forced_execution_escalation {
        forced.push("execution_escalation");
    }
    if st.forced_parallel_batching {
        forced.push("parallel_batching_force");
    }
    if st.forced_completion_soft_stop {
        forced.push("completion_soft_stop");
    }
    if st.forced_redundant_reads_corrective {
        forced.push("redundant_reads_corrective");
    }
    if st.forced_cache_waste_corrective {
        forced.push("cache_waste_corrective");
    }
    if st.forced_exploration_family_phase2 {
        forced.push("exploration_family_phase2");
    }
    if st.forced_exploration_family_corrective {
        forced.push("exploration_family_corrective");
    }
    if !forced.is_empty() {
        out.push_str("\n### Forced corrections fired this turn\n");
        for f in &forced {
            out.push_str("- ");
            out.push_str(f);
            out.push('\n');
        }
    }
    out
}

/// Render `subtopic=all` — everything. Useful when debugging / when
/// the agent isn't sure which lens to pick. Same content as
/// `render_full` + the three Task #46 subtopics.
pub fn render_all(s: &IntrospectSnapshot) -> String {
    let mut out = render_full(s);
    out.push_str("\n\n");
    out.push_str(&render_recent_rounds(s));
    out.push_str("\n\n");
    out.push_str(&render_volatile_pending(s));
    out.push_str("\n\n");
    out.push_str(&render_stall_state(s));
    out
}

fn preview_line(text: &str, max: usize) -> String {
    let one_line: String = text
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .next()
        .unwrap_or("")
        .chars()
        .take(max)
        .collect();
    if one_line.len() < text.lines().next().map(str::len).unwrap_or(0) {
        format!("{one_line}…")
    } else {
        one_line
    }
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
                ToolHealthEntry {
                    name: "bash".into(),
                    calls: 15,
                    errors: 5,
                    avg_ms: 2300,
                },
                ToolHealthEntry {
                    name: "read_file".into(),
                    calls: 22,
                    errors: 0,
                    avg_ms: 12,
                },
                ToolHealthEntry {
                    name: "grep".into(),
                    calls: 8,
                    errors: 1,
                    avg_ms: 45,
                },
            ],
            working_memory_summary: "Goal: implement streaming resume".into(),
            total_input_tokens: 145_000,
            total_output_tokens: 12_000,
            cache_read_tokens: 95_000,
            cache_creation_tokens: 8_000,
            recent_rounds: Vec::new(),
            volatile_pending: Vec::new(),
            stall_state: StallSnapshotSummary::default(),
        }
    }

    #[test]
    fn minimal_is_single_line() {
        let output = render_introspect(&sample_snapshot(), IntrospectDetail::Minimal);
        assert!(
            !output.contains('\n'),
            "minimal must be a single line: {output}"
        );
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
        assert_eq!(
            IntrospectDetail::from_budget(3000),
            IntrospectDetail::Summary
        );
        assert_eq!(
            IntrospectDetail::from_budget(1000),
            IntrospectDetail::Minimal
        );
    }

    #[test]
    fn detail_from_arg_parses_variants() {
        assert_eq!(IntrospectDetail::from_arg("full"), IntrospectDetail::Full);
        assert_eq!(
            IntrospectDetail::from_arg("brief"),
            IntrospectDetail::Summary
        );
        assert_eq!(IntrospectDetail::from_arg("min"), IntrospectDetail::Minimal);
        assert_eq!(
            IntrospectDetail::from_arg("unknown"),
            IntrospectDetail::Summary
        );
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

    // ── Task #46: recent-rounds / volatile / stall renderers ──

    #[test]
    fn render_recent_rounds_empty_state_message() {
        let snap = IntrospectSnapshot::default();
        let out = render_recent_rounds(&snap);
        assert!(out.contains("No rounds recorded"), "got: {out}");
    }

    #[test]
    fn render_recent_rounds_tabulates_and_summarizes() {
        let mut snap = IntrospectSnapshot::default();
        snap.recent_rounds = vec![
            RoundSnapshotEntry {
                turn: 3,
                round: 0,
                provider: "anthropic".into(),
                model: "claude".into(),
                prompt_tokens: 100,
                cache_read_tokens: 7000,
                tool_calls_returned: 2,
                tool_call_names: vec!["bash".into(), "read_file".into()],
                duration_ms: 1500,
                finish_reason: Some("tool_calls".into()),
                ..Default::default()
            },
            RoundSnapshotEntry {
                turn: 3,
                round: 1,
                provider: "anthropic".into(),
                model: "claude".into(),
                prompt_tokens: 200,
                cache_read_tokens: 7300,
                tool_calls_returned: 0,
                tool_call_names: vec![],
                duration_ms: 900,
                finish_reason: Some("stop".into()),
                ..Default::default()
            },
        ];
        let out = render_recent_rounds(&snap);
        assert!(out.contains("t3_r0"));
        assert!(out.contains("t3_r1"));
        assert!(out.contains("bash,read_file"));
        // Summary line has cache-hit percentage.
        assert!(out.contains("Ring: 2 rounds"));
        assert!(out.contains("cache_hit"));
    }

    #[test]
    fn render_volatile_pending_empty_and_populated() {
        let empty = IntrospectSnapshot::default();
        assert!(render_volatile_pending(&empty).contains("Empty"));

        let mut snap = IntrospectSnapshot::default();
        snap.volatile_pending = vec![
            VolatileSnapshotEntry {
                kind: "WorkingSet".into(),
                content: "[working-set:v1]\ngoal: fix bug\nactive_files: src/foo.rs".into(),
                round_index: 2,
            },
            VolatileSnapshotEntry {
                kind: "StallNudge".into(),
                content: "⚠ REFLECTION: same read_file 3 times in a row".into(),
                round_index: 2,
            },
        ];
        let out = render_volatile_pending(&snap);
        assert!(out.contains("WorkingSet"));
        assert!(out.contains("StallNudge"));
        // Preview shows first line only.
        assert!(out.contains("[working-set:v1]"));
        assert!(out.contains("round 2"));
    }

    #[test]
    fn render_stall_state_healthy_and_triggered() {
        let healthy = IntrospectSnapshot::default();
        assert!(render_stall_state(&healthy).contains("Healthy"));

        let mut snap = IntrospectSnapshot::default();
        snap.stall_state.nudge_count = 2;
        snap.stall_state.introspection_count = 1;
        snap.stall_state.forced_parallel_batching = true;
        snap.stall_state.events = vec!["sig_stall @ turn 5".into()];
        let out = render_stall_state(&snap);
        assert!(out.contains("Soft nudges: 2"));
        assert!(out.contains("sig_stall @ turn 5"));
        assert!(out.contains("parallel_batching_force"));
    }

    #[test]
    fn render_all_includes_every_section() {
        let mut snap = sample_snapshot();
        snap.recent_rounds.push(RoundSnapshotEntry {
            turn: 1,
            round: 0,
            prompt_tokens: 10,
            cache_read_tokens: 50,
            ..Default::default()
        });
        snap.volatile_pending.push(VolatileSnapshotEntry {
            kind: "WorkingSet".into(),
            content: "[working-set:v1]".into(),
            round_index: 0,
        });
        snap.stall_state.nudge_count = 1;
        let out = render_all(&snap);
        assert!(out.contains("## Session Health"));
        assert!(out.contains("## Recent Rounds"));
        assert!(out.contains("## Volatile Lane"));
        assert!(out.contains("## Stall / Loop-Guard"));
    }
}
