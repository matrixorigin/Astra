//! Budget-adaptive runtime introspection for the always-load `introspect` tool.
//!
//! The LLM calls `introspect` to query its own session state — token pressure,
//! cache efficiency, tool health, active alerts, and working memory. Output
//! depth follows the normalized observation-plane request.

pub mod cache_diagnosis;
mod observation;
mod request;

use serde::{Deserialize, Serialize};

use crate::injection_tracking::{ChannelFreshness, ChannelStatus, InjectionChannel};
use astra_core::{ObservationDepth, ObservationFacet};
pub use observation::{IntrospectReport, build_introspect_report};
pub use request::{IntrospectDepth, IntrospectFormat, IntrospectRequest};

/// Input snapshot provided by the runtime to the introspect renderer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntrospectSnapshot {
    /// Concrete model selected for this turn. This is the authoritative
    /// self-identity fact for "what model am I?" questions; callers must not
    /// infer it from recent rounds or defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_model: Option<String>,
    pub token_pressure: f64,
    pub cache_hit_ratio: f64,
    pub turns_completed: u32,
    pub turns_remaining: u32,
    pub compaction_tier: String,
    pub alerts: Vec<String>,
    pub tool_health: Vec<ToolHealthEntry>,
    pub working_memory_summary: String,
    /// Host-provided lifecycle context. In the CLI this is the same
    /// turn-start plan/task/session block injected into the prompt; it is not
    /// a live mid-turn projection of mutations that happened after the round
    /// began.
    #[serde(default)]
    pub lifecycle_summary: String,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,

    // ── Task #46: enhanced self-awareness ──
    /// Summary of the most recent LLM rounds (in-memory ring). Available
    /// regardless of `full_llm_capture` setting. Populated by
    /// `AgenticLoopState.recent_rounds`. Feeds `facet=recent`.
    #[serde(default)]
    pub recent_rounds: Vec<RoundSnapshotEntry>,
    /// Currently-pending volatile injections scheduled for the next LLM
    /// call (tool-health warnings, working-set snapshots, stall nudges,
    /// …). Lets the agent answer "what runtime nudges am I about to
    /// see?". Feeds `facet=volatile`.
    #[serde(default)]
    pub volatile_pending: Vec<VolatileSnapshotEntry>,
    /// Current stall / loop-guard telemetry — nudge count, event log,
    /// circuit breaker state. Feeds `facet=stall`.
    #[serde(default)]
    pub stall_state: StallSnapshotSummary,
    /// Per-channel freshness of runtime-injected prompt signals
    /// (recent_failing_tests, outcome_bias, lessons, volatile_pending).
    /// Populated by the agentic loop from `AgenticLoopState.injection_history`
    /// at the start of each round. Feeds `facet=noise`. Empty when
    /// the runtime has not yet observed any round.
    #[serde(default)]
    pub injection_freshness: Vec<ChannelFreshness>,
    /// Current round index at snapshot time — used to interpret
    /// `rounds_alive` in the freshness report. 0 when unknown.
    #[serde(default)]
    pub current_round: u32,

    /// Recent tool errors with previews — feeds `facet=errors`.
    #[serde(default)]
    pub tool_errors: Vec<ToolErrorEntry>,

    /// Bridge circuit breaker state — surfaced in stall/full renders.
    #[serde(default)]
    pub circuit_breaker: Option<CircuitBreakerSnapshot>,
}

/// Per-round summary surfaced through `introspect(facet=recent)`.
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
    /// Correction labels fired this turn (e.g. "execution_escalation",
    /// "parallel_batching_force", "cache_waste_corrective", …).
    #[serde(default)]
    pub forced_corrections: Vec<String>,
    /// How many drift nudges injected this session (persists across turns).
    #[serde(default)]
    pub drift_nudge_count: usize,
    /// Round index of last drift correction.
    #[serde(default)]
    pub last_drift_correction_round: usize,
}

/// Per-tool health entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolHealthEntry {
    pub name: String,
    pub calls: u32,
    pub errors: u32,
    pub avg_ms: u64,
    #[serde(default)]
    pub avoidance_advised: bool,
    #[serde(default)]
    pub consecutive_failures: u32,
    #[serde(default)]
    pub last_failure_category: Option<String>,
}

/// Recent tool error entry for `facet=errors`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolErrorEntry {
    pub tool: String,
    pub signature_hint: String,
    pub failure_category: Option<String>,
    pub error_preview: Option<String>,
    pub at_epoch: u64,
    /// Raw error message (not classified, not truncated).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error_message: String,
    /// File path for read_file/write_file/str_replace errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    /// Line range for read_file errors (e.g., "200-400").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_range: Option<String>,
    /// Turn index when the error occurred.
    #[serde(default)]
    pub turn: u32,
    /// Round index within the turn.
    #[serde(default)]
    pub round: u32,
}

/// Bridge circuit breaker state snapshot.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CircuitBreakerSnapshot {
    pub state: String,
    pub failure_count: u64,
    pub success_count: u64,
    pub consecutive_failures: u64,
}

/// Text output depth chosen from the normalized observation-plane request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IntrospectTextDepth {
    /// Full diagnostics (~500-800 tokens output).
    Full,
    /// Key metrics + top alerts (~150-250 tokens).
    Summary,
    /// One-liner (~30-50 tokens).
    Hint,
}

/// Render the normalized introspection request from a runtime snapshot.
///
/// CLI/Edge callers may intercept edge-only facets such as `cache` and
/// `session_memory` before reaching this function. Pure server callers return
/// an explicit unavailable surface for those facets instead of silently
/// degrading to the default session view.
pub fn render_introspect_request(
    snapshot: &IntrospectSnapshot,
    request: &IntrospectRequest,
) -> String {
    if request.format.is_json() {
        return observation::render_introspect_report_json(snapshot, request);
    }

    match request.facet {
        ObservationFacet::Session => {
            let depth = match request.depth {
                ObservationDepth::Hint => IntrospectTextDepth::Hint,
                ObservationDepth::Summary => IntrospectTextDepth::Summary,
                ObservationDepth::Diagnostic | ObservationDepth::Forensic => {
                    IntrospectTextDepth::Full
                }
            };
            render_introspect(snapshot, depth)
        }
        ObservationFacet::Recent | ObservationFacet::Trace => render_recent_rounds(snapshot),
        ObservationFacet::Volatile => render_volatile_pending(snapshot),
        ObservationFacet::Stall => render_stall_state(snapshot),
        ObservationFacet::Noise => render_injection_freshness(snapshot),
        ObservationFacet::Errors => render_errors(snapshot),
        ObservationFacet::Overview => render_all(snapshot),
        ObservationFacet::Cache | ObservationFacet::SessionMemory => {
            render_edge_local_unavailable(request)
        }
    }
}

fn render_edge_local_unavailable(request: &IntrospectRequest) -> String {
    let reason = if request.source_policy.allows_edge_local_artifacts() {
        "no CLI/Edge-local artifact provider is attached"
    } else {
        "requested source_policy does not allow CLI/Edge-local artifacts"
    };
    format!(
        "## Introspect Unavailable\n\
         facet={} source_policy={}\n\
         CLI/Edge-local session artifacts are not visible from this runtime: {}.",
        request.facet.as_str(),
        request.source_policy.as_str(),
        reason,
    )
}

/// Render the introspect output at the requested text depth.
fn render_introspect(snapshot: &IntrospectSnapshot, depth: IntrospectTextDepth) -> String {
    match depth {
        IntrospectTextDepth::Hint => render_hint(snapshot),
        IntrospectTextDepth::Summary => render_summary(snapshot),
        IntrospectTextDepth::Full => render_full(snapshot),
    }
}

fn render_hint(s: &IntrospectSnapshot) -> String {
    let mut out = format!(
        "pressure={:.0}% cache={:.0}% turns={}/{} alerts={} tier={}",
        s.token_pressure * 100.0,
        s.cache_hit_ratio * 100.0,
        s.turns_completed,
        s.turns_completed + s.turns_remaining,
        s.alerts.len(),
        s.compaction_tier,
    );
    if let Some(model) = s.current_model.as_deref() {
        out.push_str(" model=");
        out.push_str(model);
    }
    out
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
    if let Some(model) = s.current_model.as_deref() {
        out.push_str("Current model: ");
        out.push_str(model);
        out.push('\n');
    }
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
    if !s.lifecycle_summary.is_empty() {
        out.push_str(&s.lifecycle_summary);
        out.push('\n');
    }
    out.trim_end().to_string()
}

fn render_full(s: &IntrospectSnapshot) -> String {
    let mut out = render_summary(s);
    out.push('\n');

    if !s.tool_health.is_empty() {
        out.push_str("\n## Tool Health\n");
        out.push_str("| Tool | Calls | Errors | Avg ms | ConsecFail | Avoid | LastFail |\n");
        out.push_str("|------|-------|--------|--------|------------|-------|----------|\n");
        for t in &s.tool_health {
            let avoidance = if t.avoidance_advised { "YES" } else { "-" };
            let last_fail = t.last_failure_category.as_deref().unwrap_or("-");
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} |\n",
                t.name, t.calls, t.errors, t.avg_ms, t.consecutive_failures, avoidance, last_fail
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

/// Render `facet=recent` — the in-memory ring of recent LLM rounds.
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

/// Render `facet=volatile` — what's queued in the volatile lane
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

/// Render `facet=stall` — stall / loop-guard telemetry.
pub fn render_stall_state(s: &IntrospectSnapshot) -> String {
    let st = &s.stall_state;
    let any_forced = !st.forced_corrections.is_empty();
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
    if any_forced {
        out.push_str("\n### Forced corrections fired this turn\n");
        for correction in &st.forced_corrections {
            out.push_str("- ");
            out.push_str(correction);
            out.push('\n');
        }
    }
    if let Some(cb) = &s.circuit_breaker {
        out.push_str(&format!(
            "\n### Bridge Circuit Breaker\nstate={} failures={} successes={} consecutive_failures={}\n",
            cb.state, cb.failure_count, cb.success_count, cb.consecutive_failures,
        ));
    }
    out
}

/// Render `facet=errors` — recent tool failures with error previews.
pub fn render_errors(s: &IntrospectSnapshot) -> String {
    if s.tool_errors.is_empty() {
        return "## Recent Tool Errors\n(No failures recorded this session.)".to_string();
    }
    let mut out = String::from(
        "## Recent Tool Errors (newest first)\n\
         | Tool | Category | Turn | Age(s) | Preview |\n\
         |------|----------|------|--------|---------|\n",
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for e in &s.tool_errors {
        let age = now.saturating_sub(e.at_epoch);
        let cat = e.failure_category.as_deref().unwrap_or("-");
        let preview = e
            .error_preview
            .as_deref()
            .map(|p| p.replace('|', "\\|").replace('\n', " "))
            .unwrap_or_else(|| "-".to_string());
        let short: String = preview.chars().take(80).collect();
        let turn_str = if e.turn > 0 {
            format!("T{}R{}", e.turn, e.round)
        } else {
            "-".to_string()
        };
        out.push_str(&format!(
            "| {} | {} | {} | {}s | {} |\n",
            e.tool, cat, turn_str, age, short,
        ));
        // Detail line for file errors
        if let Some(ref path) = e.file_path {
            let range = e.file_range.as_deref().unwrap_or("");
            let msg = if e.error_message.is_empty() {
                "-"
            } else {
                &e.error_message
            };
            let msg_short: String = msg.chars().take(60).collect();
            out.push_str(&format!(
                "  → path: `{}{}` — {}\n",
                path,
                if range.is_empty() {
                    String::new()
                } else {
                    format!(":{range}")
                },
                msg_short.replace('\n', " "),
            ));
        } else if !e.error_message.is_empty() {
            let msg_short: String = e.error_message.chars().take(100).collect();
            out.push_str(&format!("  → {}\n", msg_short.replace('\n', " "),));
        }
    }
    if !s.tool_errors.is_empty() {
        out.push_str("\nSignature hints:\n");
        for e in s.tool_errors.iter().take(5) {
            if !e.signature_hint.is_empty() {
                out.push_str(&format!("- {}: {}\n", e.tool, e.signature_hint));
            }
        }
    }
    out
}

/// Render `facet=noise` — per-channel freshness of runtime-injected
/// prompt signals. Surfaces stale injections (e.g., a "Recent test
/// failures" entry that has been re-rendered unchanged for 58 rounds
/// — session f85a02bb). Operators and the model can use this to
/// distinguish fresh runtime context from signals that have aged out.
pub fn render_injection_freshness(s: &IntrospectSnapshot) -> String {
    if s.injection_freshness.is_empty() {
        return "## Injection Freshness\n(No injections tracked yet — runtime has not observed any round.)".to_string();
    }
    let mut out = String::from(&format!(
        "## Injection Freshness (round {})\n\
         | channel | status | first_seen | rounds_alive | preview |\n\
         |---------|--------|------------|--------------|---------|\n",
        s.current_round,
    ));
    let mut stale_count = 0usize;
    let mut tracked_count = 0usize;
    for entry in &s.injection_freshness {
        let (status_label, rounds_alive_str, first_seen_str) = match &entry.status {
            ChannelStatus::Untracked => ("untracked", "-".to_string(), "-".to_string()),
            ChannelStatus::Empty { first_seen_round } => {
                ("empty", "-".to_string(), format!("r{first_seen_round}"))
            }
            ChannelStatus::Fresh { rounds_alive } => {
                tracked_count += 1;
                (
                    "fresh",
                    rounds_alive.to_string(),
                    entry
                        .first_seen_round
                        .map(|r| format!("r{r}"))
                        .unwrap_or_else(|| "-".to_string()),
                )
            }
            ChannelStatus::Stale { rounds_alive } => {
                tracked_count += 1;
                stale_count += 1;
                (
                    "⚠ STALE",
                    rounds_alive.to_string(),
                    entry
                        .first_seen_round
                        .map(|r| format!("r{r}"))
                        .unwrap_or_else(|| "-".to_string()),
                )
            }
        };
        let preview = if entry.preview.is_empty() {
            "-".to_string()
        } else {
            entry.preview.replace('|', "\\|")
        };
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            channel_tag(entry.channel),
            status_label,
            first_seen_str,
            rounds_alive_str,
            preview,
        ));
    }
    out.push('\n');
    if tracked_count == 0 {
        out.push_str(
            "Summary: no channel has any non-empty content observed — runtime has not injected anything.\n",
        );
    } else if stale_count == 0 {
        out.push_str(&format!(
            "Summary: {tracked_count}/{tracked_count} tracked channels are fresh.\n",
        ));
    } else {
        out.push_str(&format!(
            "Summary: {stale_count}/{tracked_count} channels unchanged beyond stale threshold — these signals may no longer reflect current state; verify before acting on them.\n",
        ));
    }
    out
}

fn channel_tag(ch: InjectionChannel) -> &'static str {
    ch.tag()
}

/// Render `facet=all` — everything. Useful when debugging / when
/// the agent isn't sure which lens to pick. Same content as
/// `render_full` + recent/volatile/stall facets + injection freshness + errors.
pub fn render_all(s: &IntrospectSnapshot) -> String {
    let mut out = render_full(s);
    out.push_str("\n\n");
    out.push_str(&render_recent_rounds(s));
    out.push_str("\n\n");
    out.push_str(&render_volatile_pending(s));
    out.push_str("\n\n");
    out.push_str(&render_stall_state(s));
    out.push_str("\n\n");
    out.push_str(&render_injection_freshness(s));
    out.push_str("\n\n");
    out.push_str(&render_errors(s));
    out
}

fn preview_line(text: &str, max: usize) -> String {
    let first_line = text.lines().next().unwrap_or("");
    let first_line_trimmed: String = first_line.trim().chars().take(max).collect();
    if first_line_trimmed.len() < first_line.trim().len() {
        format!("{first_line_trimmed}…")
    } else {
        first_line_trimmed
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use astra_core::EvidenceRef;

    fn sample_snapshot() -> IntrospectSnapshot {
        IntrospectSnapshot {
            current_model: Some("deepseek-v4-pro-official(thinking:high)".into()),
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
                    avoidance_advised: false,
                    consecutive_failures: 0,
                    last_failure_category: None,
                },
                ToolHealthEntry {
                    name: "read_file".into(),
                    calls: 22,
                    errors: 0,
                    avg_ms: 12,
                    avoidance_advised: false,
                    consecutive_failures: 0,
                    last_failure_category: None,
                },
                ToolHealthEntry {
                    name: "grep".into(),
                    calls: 8,
                    errors: 1,
                    avg_ms: 45,
                    avoidance_advised: true,
                    consecutive_failures: 3,
                    last_failure_category: Some("Timeout".into()),
                },
            ],
            working_memory_summary: "Goal: implement streaming resume".into(),
            lifecycle_summary:
                "### Turn-start session execution state\nresume pending: [plan-resume] goal=\"Fix auth\""
                    .into(),
            total_input_tokens: 145_000,
            total_output_tokens: 12_000,
            cache_read_tokens: 95_000,
            cache_creation_tokens: 8_000,
            recent_rounds: Vec::new(),
            volatile_pending: Vec::new(),
            stall_state: StallSnapshotSummary::default(),
            injection_freshness: Vec::new(),
            current_round: 0,
            tool_errors: Vec::new(),
            circuit_breaker: None,
        }
    }

    #[test]
    fn hint_is_single_line() {
        let output = render_introspect(&sample_snapshot(), IntrospectTextDepth::Hint);
        assert!(
            !output.contains('\n'),
            "hint must be a single line: {output}"
        );
        assert!(output.contains("pressure=72%"));
        assert!(output.contains("cache=65%"));
        assert!(output.contains("turns=8/20"));
        assert!(output.contains("alerts=2"));
        assert!(output.contains("model=deepseek-v4-pro-official(thinking:high)"));
    }

    #[test]
    fn summary_includes_key_metrics_and_top_alerts() {
        let output = render_introspect(&sample_snapshot(), IntrospectTextDepth::Summary);
        assert!(output.contains("## Session Health"));
        assert!(output.contains("cache_regression"));
        assert!(output.contains("Current model: deepseek-v4-pro-official(thinking:high)"));
        assert!(output.contains("Goal: implement streaming resume"));
        assert!(output.contains("### Turn-start session execution state"));
        // Should NOT contain full tool table
        assert!(!output.contains("| Tool |"));
    }

    #[test]
    fn full_includes_tool_health_table() {
        let output = render_introspect(&sample_snapshot(), IntrospectTextDepth::Full);
        assert!(output.contains("## Tool Health"));
        assert!(output.contains("| bash |"));
        assert!(output.contains("| read_file |"));
    }

    #[test]
    fn render_json_envelope_contains_shared_observation_shape() {
        let mut snap = sample_snapshot();
        snap.tool_errors.push(ToolErrorEntry {
            tool: "bash".into(),
            signature_hint: "command timed out".into(),
            failure_category: Some("tool_timeout".into()),
            error_preview: Some("command timed out after 30s".into()),
            at_epoch: 1,
            error_message: "command timed out after 30s".into(),
            file_path: None,
            file_range: None,
            turn: 5,
            round: 2,
        });

        let req = IntrospectRequest::from_args(&serde_json::json!({
            "facet": "execution/errors",
            "format": "json"
        }));
        let out = render_introspect_request(&snap, &req);
        let report: IntrospectReport = serde_json::from_str(&out).expect("json report");
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("json value");

        assert_eq!(report.schema_version, 1);
        assert_eq!(report.tool, "introspect");
        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["tool"], "introspect");
        assert_eq!(parsed["topic"], parsed["view"]["topic"]);
        assert_eq!(parsed["facet"], parsed["view"]["facet"]);
        assert_eq!(parsed["depth"], parsed["view"]["depth"]);
        assert_eq!(parsed["horizon"], parsed["view"]["horizon"]);
        assert_eq!(parsed["data_coverage"], parsed["view"]["data_coverage"]);
        assert_eq!(report.view.topic, "execution");
        assert_eq!(report.view.facet, "errors");
        assert!(report.summary.contains("recent tool errors"));
        assert!(
            report
                .observations
                .iter()
                .any(|observation| observation.ref_id
                    == "urn:astra:observation:local:introspect:execution:error:0"
                    && observation.kind == "tool_error:tool_timeout")
        );
        assert!(report.evidence.iter().any(
            |evidence| evidence.ref_id == "urn:astra:context:local:introspect:runtime_snapshot"
        ));
        assert_eq!(report.view.data_coverage.overall, "fresh");
        assert!(
            report
                .view
                .data_coverage
                .providers
                .contains_key("live_runtime")
        );
        assert!(!report.budget_result.truncated);
        assert_report_refs_are_valid(&report);
    }

    #[test]
    fn render_json_edge_only_facet_reports_unavailable_without_unrelated_runtime_noise() {
        let req = IntrospectRequest::from_args(&serde_json::json!({
            "facet": "session_memory",
            "format": "json"
        }));
        let out = render_introspect_request(&sample_snapshot(), &req);
        let report: IntrospectReport = serde_json::from_str(&out).expect("json report");

        assert_eq!(report.view.facet, "session_memory");
        assert_eq!(
            report.view.data_coverage.source,
            "edge_local_artifacts_unavailable"
        );
        assert_eq!(report.view.data_coverage.events, 0);
        assert!(
            report
                .view
                .data_coverage
                .warnings
                .iter()
                .any(|warning| warning.contains("Edge-local"))
        );
        assert_eq!(report.observations.len(), 1);
        assert_eq!(
            report.observations[0].kind, "data_surface_unavailable",
            "edge-only JSON must not mix unrelated runtime/tool observations: {report:?}"
        );
        assert!(report.evidence.is_empty());
        assert!(report.action_hints.is_empty());
        assert_eq!(
            report.view.data_coverage.providers["local_journal"].status,
            "missing"
        );
        assert_report_refs_are_valid(&report);
    }

    #[test]
    fn render_json_action_hints_reference_matching_tool_observation_only() {
        let req = IntrospectRequest::from_args(&serde_json::json!({
            "format": "json"
        }));
        let out = render_introspect_request(&sample_snapshot(), &req);
        let report: IntrospectReport = serde_json::from_str(&out).expect("json report");

        let hint = report
            .action_hints
            .iter()
            .find(|hint| hint.summary.contains("grep"))
            .expect("grep avoidance hint");
        assert_eq!(
            hint.observation_refs,
            vec!["urn:astra:observation:local:introspect:execution:tool:grep"]
        );
        assert!(
            !hint
                .observation_refs
                .iter()
                .any(|reference| reference.contains(":runtime:alert:")),
            "tool action hints must not cite unrelated runtime alerts: {hint:?}"
        );
        assert_report_refs_are_valid(&report);
    }

    #[test]
    fn render_json_hint_budget_truncates_without_dangling_action_refs() {
        let mut snap = sample_snapshot();
        snap.alerts.clear();
        snap.tool_health = (0..8)
            .map(|idx| ToolHealthEntry {
                name: format!("tool_{idx}"),
                calls: 10 + idx,
                errors: 3,
                avg_ms: 100,
                avoidance_advised: true,
                consecutive_failures: 3,
                last_failure_category: Some("timeout".into()),
            })
            .collect();

        let req = IntrospectRequest::from_args(&serde_json::json!({
            "depth": "hint",
            "format": "json"
        }));
        let out = render_introspect_request(&snap, &req);
        let report: IntrospectReport = serde_json::from_str(&out).expect("json report");

        assert_eq!(report.view.depth, "hint");
        assert_eq!(report.observations.len(), 3);
        assert_eq!(report.action_hints.len(), 2);
        assert!(report.budget_result.truncated);
        assert_eq!(report.budget_result.omitted.observations, 6);
        assert_eq!(report.budget_result.omitted.action_hints, 3);
        assert_report_refs_are_valid(&report);
    }

    #[test]
    fn render_json_cloud_only_with_context_reports_missing_providers() {
        let req = IntrospectRequest::from_args(&serde_json::json!({
            "source_policy": "cloud_only",
            "include_context": true,
            "format": "json"
        }));
        let out = render_introspect_request(&sample_snapshot(), &req);
        let report: IntrospectReport = serde_json::from_str(&out).expect("json report");

        assert_eq!(report.view.data_coverage.overall, "partial");
        assert_eq!(report.view.data_coverage.source, "cloud_runtime_snapshot");
        assert_eq!(
            report.view.data_coverage.providers["cloud_events"].status,
            "missing"
        );
        assert_eq!(
            report.view.data_coverage.providers["visible_context"].status,
            "missing"
        );
        assert!(
            report
                .view
                .data_coverage
                .warnings
                .iter()
                .any(|warning| warning.contains("include_context requested"))
        );
        assert_report_refs_are_valid(&report);
    }

    fn assert_report_refs_are_valid(report: &IntrospectReport) {
        let observation_ids = report
            .observations
            .iter()
            .map(|observation| observation.ref_id.as_str())
            .collect::<BTreeSet<_>>();
        for observation in &report.observations {
            EvidenceRef::parse(&observation.ref_id).unwrap_or_else(|err| {
                panic!("invalid observation ref {}: {err}", observation.ref_id)
            });
            for evidence_ref in &observation.evidence_refs {
                EvidenceRef::parse(evidence_ref).unwrap_or_else(|err| {
                    panic!("invalid observation evidence ref {evidence_ref}: {err}")
                });
            }
        }
        for evidence in &report.evidence {
            EvidenceRef::parse(&evidence.ref_id)
                .unwrap_or_else(|err| panic!("invalid evidence ref {}: {err}", evidence.ref_id));
        }
        for hint in &report.action_hints {
            for observation_ref in &hint.observation_refs {
                EvidenceRef::parse(observation_ref).unwrap_or_else(|err| {
                    panic!("invalid action hint ref {observation_ref}: {err}")
                });
                assert!(
                    observation_ids.contains(observation_ref.as_str()),
                    "action hint ref must point to a retained observation: {observation_ref}"
                );
            }
        }
    }

    #[test]
    fn unavailable_message_explains_missing_edge_provider() {
        let req = IntrospectRequest::from_args(&serde_json::json!({
            "facet": "session_memory"
        }));
        let out = render_introspect_request(&IntrospectSnapshot::default(), &req);
        assert!(out.contains("Introspect Unavailable"), "{out}");
        assert!(out.contains("facet=session_memory"), "{out}");
        assert!(out.contains("CLI/Edge-local session artifacts"), "{out}");
        assert!(
            out.contains("no CLI/Edge-local artifact provider is attached"),
            "{out}"
        );
    }

    #[test]
    fn unavailable_message_explains_source_policy_exclusion() {
        let req = IntrospectRequest::from_args(&serde_json::json!({
            "facet": "session_memory",
            "source_policy": "cloud_only"
        }));
        let out = render_introspect_request(&IntrospectSnapshot::default(), &req);
        assert!(out.contains("Introspect Unavailable"), "{out}");
        assert!(out.contains("source_policy=cloud_only"), "{out}");
        assert!(
            out.contains("requested source_policy does not allow CLI/Edge-local artifacts"),
            "{out}"
        );
    }

    #[test]
    fn empty_snapshot_renders_stable_empty_state_contract() {
        let empty = IntrospectSnapshot::default();
        let hint = render_introspect(&empty, IntrospectTextDepth::Hint);
        assert_eq!(hint, "pressure=0% cache=0% turns=0/0 alerts=0 tier=");
        let full = render_introspect(&empty, IntrospectTextDepth::Full);
        assert!(full.contains("## Session Health"));
        assert!(full.contains("Pressure: 0% | Cache: 0% | Turns: 0/0 | Tier:"));
        assert!(!full.contains("## Tool Health"));
        assert!(!full.contains("Current model:"));
    }

    #[test]
    fn many_alerts_truncated_in_summary_shown_in_full() {
        let mut s = sample_snapshot();
        s.alerts = (0..10).map(|i| format!("alert-{i}")).collect();
        let summary = render_introspect(&s, IntrospectTextDepth::Summary);
        assert!(summary.contains("(+7 more)"));
        let full = render_introspect(&s, IntrospectTextDepth::Full);
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
        let snap = IntrospectSnapshot {
            recent_rounds: vec![
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
            ],
            ..Default::default()
        };
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

        let snap = IntrospectSnapshot {
            volatile_pending: vec![VolatileSnapshotEntry {
                kind: "StallNudge".into(),
                content: "⚠ REFLECTION: same read_file 3 times in a row".into(),
                round_index: 2,
            }],
            ..Default::default()
        };
        let out = render_volatile_pending(&snap);
        assert!(out.contains("StallNudge"));
        assert!(out.contains("round 2"));
    }

    #[test]
    fn render_stall_state_healthy_and_triggered() {
        let healthy = IntrospectSnapshot::default();
        assert!(render_stall_state(&healthy).contains("Healthy"));

        let mut snap = IntrospectSnapshot::default();
        snap.stall_state.nudge_count = 2;
        snap.stall_state.introspection_count = 1;
        snap.stall_state.forced_corrections = vec!["parallel_batching_force".into()];
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
            kind: "StallNudge".into(),
            content: "⚠ stall nudge".into(),
            round_index: 0,
        });
        snap.stall_state.nudge_count = 1;
        snap.injection_freshness = vec![ChannelFreshness {
            channel: InjectionChannel::Lessons,
            status: ChannelStatus::Fresh { rounds_alive: 0 },
            preview: "lesson preview".into(),
            first_seen_round: Some(0),
        }];
        let out = render_all(&snap);
        assert!(out.contains("## Session Health"));
        assert!(out.contains("## Recent Rounds"));
        assert!(out.contains("## Volatile Lane"));
        assert!(out.contains("## Stall / Loop-Guard"));
        assert!(out.contains("## Injection Freshness"));
    }

    // ── Injection freshness (facet=noise) renderer ──

    #[test]
    fn render_injection_freshness_empty_emits_empty_state_message() {
        let snap = IntrospectSnapshot::default();
        let out = render_injection_freshness(&snap);
        assert!(
            out.contains("No injections tracked yet"),
            "empty state message missing: {out}"
        );
    }

    #[test]
    fn render_injection_freshness_marks_stale_channels() {
        let snap = IntrospectSnapshot {
            current_round: 58,
            injection_freshness: vec![
                ChannelFreshness {
                    channel: InjectionChannel::RecentFailingTests,
                    status: ChannelStatus::Stale { rounds_alive: 58 },
                    preview: "could not find Cargo.toml in /home/.../astra".into(),
                    first_seen_round: Some(0),
                },
                ChannelFreshness {
                    channel: InjectionChannel::OutcomeBias,
                    status: ChannelStatus::Fresh { rounds_alive: 2 },
                    preview: "bash ↑0.10 · git ↑0.10 · run_script ↓0.08".into(),
                    first_seen_round: Some(56),
                },
                ChannelFreshness {
                    channel: InjectionChannel::Lessons,
                    status: ChannelStatus::Untracked,
                    preview: String::new(),
                    first_seen_round: None,
                },
                ChannelFreshness {
                    channel: InjectionChannel::VolatilePending,
                    status: ChannelStatus::Empty {
                        first_seen_round: 57,
                    },
                    preview: String::new(),
                    first_seen_round: Some(57),
                },
            ],
            ..Default::default()
        };
        let out = render_injection_freshness(&snap);
        assert!(out.contains("## Injection Freshness (round 58)"), "{out}");
        assert!(
            out.contains("recent_failing_tests") && out.contains("⚠ STALE"),
            "stale channel must be flagged: {out}"
        );
        assert!(
            out.contains("outcome_bias") && out.contains("fresh"),
            "{out}"
        );
        assert!(
            out.contains("lessons") && out.contains("untracked"),
            "{out}"
        );
        assert!(
            out.contains("volatile_pending") && out.contains("empty"),
            "{out}"
        );
        assert!(
            out.contains("Cargo.toml"),
            "preview content must render: {out}"
        );
        assert!(
            out.contains("1/2 channels unchanged"),
            "summary should count 1 stale of 2 tracked (non-untracked, non-empty): {out}"
        );
    }

    #[test]
    fn render_injection_freshness_all_fresh_summary() {
        let snap = IntrospectSnapshot {
            current_round: 3,
            injection_freshness: vec![ChannelFreshness {
                channel: InjectionChannel::Lessons,
                status: ChannelStatus::Fresh { rounds_alive: 1 },
                preview: "recent lesson".into(),
                first_seen_round: Some(2),
            }],
            ..Default::default()
        };
        let out = render_injection_freshness(&snap);
        assert!(
            out.contains("1/1 tracked channels are fresh"),
            "all-fresh summary line missing: {out}"
        );
        assert!(
            !out.contains("⚠ STALE"),
            "no stale marker should appear: {out}"
        );
    }

    #[test]
    fn render_injection_freshness_escapes_pipe_in_preview() {
        let snap = IntrospectSnapshot {
            current_round: 2,
            injection_freshness: vec![ChannelFreshness {
                channel: InjectionChannel::VolatilePending,
                status: ChannelStatus::Fresh { rounds_alive: 0 },
                preview: "pipe | char in content".into(),
                first_seen_round: Some(2),
            }],
            ..Default::default()
        };
        let out = render_injection_freshness(&snap);
        assert!(
            out.contains("pipe \\| char"),
            "pipe character in preview must be escaped so the markdown table renders correctly: {out}"
        );
    }

    #[test]
    fn render_injection_freshness_only_empty_channels_reports_no_injection() {
        let snap = IntrospectSnapshot {
            current_round: 5,
            injection_freshness: vec![
                ChannelFreshness {
                    channel: InjectionChannel::Lessons,
                    status: ChannelStatus::Empty {
                        first_seen_round: 0,
                    },
                    preview: String::new(),
                    first_seen_round: Some(0),
                },
                ChannelFreshness {
                    channel: InjectionChannel::OutcomeBias,
                    status: ChannelStatus::Untracked,
                    preview: String::new(),
                    first_seen_round: None,
                },
            ],
            ..Default::default()
        };
        let out = render_injection_freshness(&snap);
        assert!(
            out.contains("runtime has not injected anything"),
            "no-injection summary missing: {out}"
        );
    }

    // ── Tests for render_errors ──────────────────────────────────────────

    #[test]
    fn render_errors_empty_reports_no_failures() {
        let snap = IntrospectSnapshot::default();
        let out = render_errors(&snap);
        assert!(
            out.contains("No failures recorded"),
            "empty errors should report no failures: {out}"
        );
    }

    #[test]
    fn render_errors_shows_tool_and_category() {
        let snap = IntrospectSnapshot {
            tool_errors: vec![ToolErrorEntry {
                tool: "bash".into(),
                signature_hint: "bash:ls -la".into(),
                failure_category: Some("Timeout".into()),
                error_preview: Some("command timed out".into()),
                at_epoch: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
                error_message: "command timed out".into(),
                file_path: None,
                file_range: None,
                turn: 3,
                round: 1,
            }],
            ..Default::default()
        };
        let out = render_errors(&snap);
        assert!(out.contains("bash"), "tool name missing: {out}");
        assert!(out.contains("Timeout"), "category missing: {out}");
        assert!(out.contains("command timed out"), "preview missing: {out}");
        assert!(
            out.contains("Signature hints"),
            "hints section missing: {out}"
        );
        assert!(out.contains("bash:ls -la"), "signature hint missing: {out}");
    }

    #[test]
    fn render_errors_truncates_preview_to_80_chars() {
        let long_preview = "x".repeat(200);
        let snap = IntrospectSnapshot {
            tool_errors: vec![ToolErrorEntry {
                tool: "read_file".into(),
                signature_hint: "read_file:/long/path".into(),
                failure_category: None,
                error_preview: Some(long_preview.clone()),
                at_epoch: 1000,
                error_message: long_preview.clone(),
                file_path: Some("/long/path/file.txt".into()),
                file_range: Some("200-400".into()),
                turn: 10,
                round: 3,
            }],
            ..Default::default()
        };
        let out = render_errors(&snap);
        // The rendered preview in the table should be at most 80 chars
        assert!(
            !out.contains(&long_preview),
            "full 200-char preview should not appear in render"
        );
    }

    // ── Tests for circuit breaker rendering ──────────────────────────────

    #[test]
    fn render_stall_with_circuit_breaker() {
        let snap = IntrospectSnapshot {
            stall_state: StallSnapshotSummary {
                nudge_count: 1,
                ..Default::default()
            },
            circuit_breaker: Some(CircuitBreakerSnapshot {
                state: "half_open".into(),
                failure_count: 5,
                success_count: 20,
                consecutive_failures: 3,
            }),
            ..Default::default()
        };
        let out = render_stall_state(&snap);
        assert!(out.contains("half_open"), "CB state missing: {out}");
        assert!(
            out.contains("consecutive_failures=3"),
            "CB consecutive missing: {out}"
        );
    }

    // ── Tests for enhanced tool health rendering ─────────────────────────

    #[test]
    fn render_full_shows_health_avoidance_tool() {
        let snap = sample_snapshot();
        let out = render_full(&snap);
        assert!(out.contains("YES"), "health avoidance YES missing: {out}");
        assert!(
            out.contains("Timeout"),
            "last failure category missing: {out}"
        );
        assert!(out.contains("ConsecFail"), "header missing: {out}");
    }

    // ── Unhappy-path tests ───────────────────────────────────────────────

    #[test]
    fn render_json_empty_snapshot_per_facet_no_crash() {
        let empty = IntrospectSnapshot::default();
        for facet_str in &["session", "recent", "volatile", "stall", "noise", "errors"] {
            let req = IntrospectRequest::from_args(&serde_json::json!({
                "facet": facet_str,
                "format": "json"
            }));
            let out = render_introspect_request(&empty, &req);
            let report: IntrospectReport = serde_json::from_str(&out).unwrap_or_else(|e| {
                panic!("facet={facet_str}: JSON deserialize failed: {e}: {out}")
            });
            assert!(
                !report.observations.iter().any(|o| o.severity == "critical"),
                "facet={facet_str}: empty snapshot should not produce critical observations"
            );
            assert_report_refs_are_valid(&report);
        }
    }

    #[test]
    fn render_json_budget_priority_warnings_survive() {
        let mut snap = sample_snapshot();
        snap.alerts.clear();
        snap.tool_health = (0..5)
            .map(|idx| ToolHealthEntry {
                name: format!("tool_{idx}"),
                calls: 10,
                errors: 3,
                avg_ms: 100,
                avoidance_advised: true,
                consecutive_failures: 3,
                last_failure_category: Some("timeout".into()),
            })
            .collect();
        snap.tool_health.push(ToolHealthEntry {
            name: "clean_tool".into(),
            calls: 5,
            errors: 0,
            avg_ms: 5,
            avoidance_advised: false,
            consecutive_failures: 0,
            last_failure_category: None,
        });

        let req = IntrospectRequest::from_args(&serde_json::json!({
            "depth": "summary",
            "format": "json"
        }));
        let out = render_introspect_request(&snap, &req);
        let report: IntrospectReport = serde_json::from_str(&out).expect("json report");

        // 1 health + 5 warning tool_health = 6 observations. Budget=8: fits.
        // Action hints budget=4, but we have 5 → truncated flag set.
        assert_eq!(report.observations.len(), 6);
        assert_eq!(report.action_hints.len(), 4);
        assert!(report.budget_result.truncated);
        assert_eq!(report.budget_result.omitted.action_hints, 1);
        let warning_obs: Vec<_> = report
            .observations
            .iter()
            .filter(|o| o.severity == "warning")
            .collect();
        assert_eq!(
            warning_obs.len(),
            5,
            "all warning tool_health observations should survive"
        );
        assert_report_refs_are_valid(&report);
    }

    #[test]
    fn render_json_budget_truncation_drops_info_before_warning() {
        let mut snap = sample_snapshot();
        // sample_snapshot already has: bash (errors=5), read_file (all=0, filtered), grep (avoidance=true)
        // Set alerts to avoid alert observation (would be warning)
        snap.alerts.clear();
        // Add 2 info + 4 warning tools = 6 new tools
        // Total tools passing filter: bash + grep + 2 info + 4 warning = 8 (fits take(8))
        // Observations: 1 health + 8 tool = 9 → budget=8 drops 1
        for idx in 0..2 {
            snap.tool_health.push(ToolHealthEntry {
                name: format!("info_tool_{idx}"),
                calls: 10,
                errors: 1,
                avg_ms: 100,
                avoidance_advised: false,
                consecutive_failures: 1,
                last_failure_category: None,
            });
        }
        for idx in 0..4 {
            snap.tool_health.push(ToolHealthEntry {
                name: format!("bad_tool_{idx}"),
                calls: 10,
                errors: 4,
                avg_ms: 100,
                avoidance_advised: true,
                consecutive_failures: 3,
                last_failure_category: Some("timeout".into()),
            });
        }

        let req = IntrospectRequest::from_args(&serde_json::json!({
            "depth": "summary",
            "format": "json"
        }));
        let out = render_introspect_request(&snap, &req);
        let report: IntrospectReport = serde_json::from_str(&out).expect("json report");

        // 1 health + 8 tool = 9 observations. Budget=8, priority sort drops health (info).
        // Remaining: 5 warnings (grep+4bad) + 3 info (bash+2info) = 8
        assert_eq!(
            report.observations.len(),
            8,
            "should hit summary budget of 8"
        );
        assert!(report.budget_result.truncated);
        let retained_warnings: Vec<_> = report
            .observations
            .iter()
            .filter(|o| o.severity == "warning")
            .collect();
        assert_eq!(
            retained_warnings.len(),
            5,
            "all 5 warning observations must survive, only info gets truncated"
        );
        assert!(report.budget_result.omitted.observations >= 1);
        assert_report_refs_are_valid(&report);
    }

    #[test]
    fn render_json_tool_errors_facet_isolated_from_tool_health() {
        let mut snap = sample_snapshot();
        snap.tool_errors.push(ToolErrorEntry {
            tool: "bash".into(),
            signature_hint: "bash:ls -la".into(),
            failure_category: Some("tool_timeout".into()),
            error_preview: Some("command timed out after 30s".into()),
            at_epoch: 1,
            error_message: "command timed out after 30s".into(),
            file_path: None,
            file_range: None,
            turn: 5,
            round: 2,
        });

        let req = IntrospectRequest::from_args(&serde_json::json!({
            "facet": "errors",
            "format": "json"
        }));
        let out = render_introspect_request(&snap, &req);
        let report: IntrospectReport = serde_json::from_str(&out).expect("json report");

        assert_eq!(report.facet, "errors");
        assert!(
            !report.observations.iter().any(|o| o.kind == "tool_health"),
            "errors facet must not contain tool_health observations"
        );
        assert!(
            report
                .observations
                .iter()
                .any(|o| o.kind == "tool_error:tool_timeout"),
            "errors facet must contain tool_error observations"
        );
        assert_report_refs_are_valid(&report);
    }
}
