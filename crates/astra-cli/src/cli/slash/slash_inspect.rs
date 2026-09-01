use crate::cli::session::session_state::SessionState;
use crossterm::style::Stylize;

/// A fact shown by the workbench inspector.  The status is deliberately part
/// of the payload rather than encoded in its rendered text: an absent trace
/// and a degraded persistence path must not look like an observed value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InspectorFact {
    pub(crate) label: String,
    pub(crate) value: String,
    pub(crate) status: InspectorFactStatus,
}

impl InspectorFact {
    fn observed(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            status: InspectorFactStatus::Observed,
        }
    }

    fn not_recorded(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            status: InspectorFactStatus::NotRecorded,
        }
    }

    fn degraded(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            status: InspectorFactStatus::Degraded,
        }
    }
}

/// Whether a fact was observed now, is unavailable in the canonical state, or
/// records a known degradation.  This is evidence metadata, not a policy or
/// control-flow signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InspectorFactStatus {
    Observed,
    NotRecorded,
    Degraded,
}

/// A coherent inspector dimension with a declared source and freshness.  The
/// same report is valid in CLI + Server, Server Only, and Edge + Server: a
/// missing remote artifact is represented as missing evidence, never as an
/// invented local fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InspectorSection {
    pub(crate) title: String,
    pub(crate) source: String,
    pub(crate) facts: Vec<InspectorFact>,
}

/// Typed workbench inspection payload.  Rendering is a separate TUI concern;
/// this model is also suitable for a line-mode or future API projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkbenchInspection {
    pub(crate) sections: Vec<InspectorSection>,
}

/// Build the first high-value introspection dimensions available in every
/// interactive deployment mode. Harness snapshots enrich this report when
/// present, but never determine whether the user can inspect the session.
///
/// These are direct state facts, not inferred health claims: unavailable or
/// not-yet-recorded data is shown as such instead of being rewritten as an
/// empty/healthy result.
pub(crate) fn inspect_workbench(state: &SessionState) -> WorkbenchInspection {
    let persistence = match state.session_persistence_error.as_deref() {
        Some(error) => InspectorFact::degraded("Persistence", error),
        None => InspectorFact::observed("Persistence", "no local error recorded"),
    };
    let state_section = InspectorSection {
        title: "State".into(),
        source: "current client session state · captured now".into(),
        facts: vec![
            state.session_id.as_deref().map_or_else(
                || InspectorFact::not_recorded("Session", "not created yet"),
                |session_id| InspectorFact::observed("Session", session_id),
            ),
            state.run_id.as_deref().map_or_else(
                || InspectorFact::not_recorded("Run", "no active run"),
                |run_id| InspectorFact::observed("Run", run_id),
            ),
            state.model.as_deref().map_or_else(
                || InspectorFact::not_recorded("Model", "not selected"),
                |model| InspectorFact::observed("Model", model),
            ),
            InspectorFact::observed("Turn", state.turn.to_string()),
            InspectorFact::observed("Permission", state.perm_manager.mode().to_string()),
            persistence,
        ],
    };

    let mut mcp_facts = match state.mcp_manager.try_read() {
        Ok(manager) => {
            let connection_count = manager.connection_count();
            let mut facts = vec![InspectorFact::observed(
                "MCP connections",
                connection_count.to_string(),
            )];
            let mut servers = manager.connected_servers();
            servers.sort_unstable();
            for server in servers {
                let Some(connection) = manager.get(server) else {
                    continue;
                };
                facts.push(InspectorFact::observed(
                    format!("MCP · {server}"),
                    format!("connected · {} tools", connection.tools().len()),
                ));
            }
            facts
        }
        Err(_) => vec![InspectorFact::not_recorded(
            "MCP connections",
            "live manager busy; retry /inspect for a fresh capability snapshot",
        )],
    };
    mcp_facts.extend([
        InspectorFact::observed(
            "Active system skills",
            state.active_system_skills.len().to_string(),
        ),
        InspectorFact::observed(
            "Deferred tools",
            state.activated_deferred_tool_names.len().to_string(),
        ),
    ]);
    let capability_section = InspectorSection {
        title: "Capability & provider".into(),
        source: "current client configuration and live MCP manager · captured now".into(),
        facts: mcp_facts,
    };

    let context_section = InspectorSection {
        title: "Context".into(),
        source: "cumulative session counters; assembly trace is last recorded evidence".into(),
        facts: vec![
            InspectorFact::observed("Conversation turns", state.history.len().to_string()),
            InspectorFact::observed("Prompt tokens", state.total_prompt_tokens.to_string()),
            InspectorFact::observed(
                "Completion tokens",
                state.total_completion_tokens.to_string(),
            ),
            InspectorFact::observed(
                "Prompt cache reads",
                state.total_cache_read_tokens.to_string(),
            ),
            if state.latest_context_assembly_trace.is_some() {
                InspectorFact::observed("Assembly trace", "available · use /context for details")
            } else {
                InspectorFact::not_recorded("Assembly trace", "not recorded yet")
            },
        ],
    };

    let harness_facts = match render_snapshot_summary(state) {
        Ok(summary) => summary
            .lines()
            .map(|line| InspectorFact::observed("Runtime snapshot", line))
            .collect(),
        Err(reason) => vec![InspectorFact::not_recorded("Runtime snapshot", reason)],
    };
    let harness_section = InspectorSection {
        title: "Harness evidence".into(),
        source: "latest runtime snapshot; unavailable is not a health claim".into(),
        facts: harness_facts,
    };

    WorkbenchInspection {
        sections: vec![
            state_section,
            capability_section,
            context_section,
            harness_section,
        ],
    }
}

#[cfg(feature = "harness")]
/// Headless handler for `/inspect`.
///
/// The TUI renders the same snapshot as a workbench panel rather than taking
/// over the terminal. This printer remains for non-interactive CLI use.
pub(crate) fn handle_inspect_command(arg: &str, state: &SessionState) {
    use astra_harness::SnapshotSink;

    let trimmed = arg.trim();
    if trimmed == "cache" {
        print_cache(state);
        return;
    }

    let snapshot = match state.harness_sink.latest() {
        Some(s) => s,
        None => {
            eprintln!("{}", "  No harness snapshot available yet.".dim());
            return;
        }
    };

    match trimmed {
        "" | "all" => print_full_snapshot(&snapshot),
        "budget" => print_budget(&snapshot),
        "tools" => print_tools(&snapshot),
        "context" => print_context(&snapshot),
        "json" => print_json(&snapshot),
        "diff" => print_diff(state),
        "trace" => print_trace(state),
        "forensics" => print_forensics(state),
        _ if trimmed.starts_with("history") => {
            let n = trimmed
                .strip_prefix("history")
                .unwrap_or("")
                .trim()
                .parse::<usize>()
                .unwrap_or(5);
            print_history(state, n);
        }
        _ if trimmed.starts_with("export") => {
            let path = trimmed.strip_prefix("export").unwrap_or("").trim();
            export_trace(state, path);
        }
        other => {
            eprintln!(
                "{}",
                format!("  Unknown /inspect subcommand: {other}").yellow()
            );
            eprintln!(
                "  Usage: /inspect [budget|tools|context|cache|json|diff|history N|trace|forensics|export path]"
            );
        }
    }
}

/// Render the current harness snapshot for the workbench inspector.
#[cfg(feature = "harness")]
pub(crate) fn render_snapshot_summary(state: &SessionState) -> Result<String, String> {
    use astra_harness::SnapshotSink;

    state
        .harness_sink
        .latest()
        .map(|snapshot| format_snapshot_summary(&snapshot))
        .ok_or_else(|| "No runtime snapshot is available yet. Send a message first.".to_string())
}

/// Render the feature-unavailable state without coupling the TUI to a build
/// feature. The caller presents this as normal workbench feedback.
#[cfg(not(feature = "harness"))]
pub(crate) fn render_snapshot_summary(_state: &SessionState) -> Result<String, String> {
    Err("Runtime inspection is disabled in this build.".to_string())
}

#[cfg(feature = "harness")]
pub(crate) fn format_snapshot_summary(s: &astra_harness::RuntimeSnapshot) -> String {
    let turns = match s.turns_limit {
        Some(limit) => format!("{} / {}", s.turns_used, limit),
        None => format!("{}", s.turns_used),
    };
    let total = s
        .context_total_tokens
        .map(|t| format_tokens(t as u64))
        .unwrap_or_else(|| "-".into());
    let budget = s
        .context_budget_tokens
        .map(|t| format_tokens(t as u64))
        .unwrap_or_else(|| "unlimited".into());
    let util = s
        .context_utilization
        .map(|u| format!("{:.1}%", u * 100.0))
        .unwrap_or_else(|| "-".into());
    let unique_tools = if s.unique_tools_used.is_empty() {
        "-".to_string()
    } else {
        s.unique_tools_used.join(", ")
    };
    let last = s.last_tool_called.as_deref().unwrap_or("-");

    let mut out = String::new();
    out.push_str("─── Harness Snapshot ────────────────────────────\n");
    out.push_str(&format!("  {:<20} {}\n", "Turns:", turns));
    out.push_str(&format!(
        "  {:<20} {}\n",
        "Tokens (session):",
        format_tokens(s.tokens_used_session)
    ));
    out.push_str(&format!(
        "  {:<20} {}\n",
        "Elapsed:",
        format_duration(s.elapsed_millis)
    ));
    out.push('\n');
    out.push_str(&format!("  {:<20} {}\n", "Context tokens:", total));
    out.push_str(&format!("  {:<20} {}\n", "Context budget:", budget));
    out.push_str(&format!("  {:<20} {}\n", "Utilization:", util));
    out.push_str(&format!(
        "  {:<20} {}\n",
        "Messages:", s.context_message_count
    ));
    out.push('\n');
    out.push_str(&format!(
        "  {:<20} {}\n",
        "Tool calls:", s.tool_calls_this_session
    ));
    out.push_str(&format!("  {:<20} {}\n", "Unique tools:", unique_tools));
    out.push_str(&format!("  {:<20} {}", "Last tool:", last));
    if s.consecutive_same_tool > 1 {
        out.push_str(&format!(
            "\n  {:<20} {} (consecutive)",
            "Same tool streak:", s.consecutive_same_tool
        ));
    }
    out
}

#[cfg(feature = "harness")]
fn print_cache(state: &SessionState) {
    let Some(session_id) = state.session_id.as_deref() else {
        eprintln!(
            "{}",
            "  No active session. Start or resume a session first.".yellow()
        );
        return;
    };
    let rounds = super::slash_cache::load_cache_rounds(session_id);
    eprintln!(
        "{}",
        super::slash_cache::render_cache_diagnosis(session_id, &rounds)
    );
}

#[cfg(not(feature = "harness"))]
pub(crate) fn handle_inspect_command(_arg: &str, _state: &SessionState) {
    eprintln!(
        "{}",
        "  Harness feature is disabled. Rebuild with `--features harness` to enable /inspect."
            .yellow()
    );
}

#[cfg(feature = "harness")]
fn print_full_snapshot(s: &astra_harness::RuntimeSnapshot) {
    eprintln!(
        "\n{}",
        "─── Harness Snapshot ────────────────────────────".bold()
    );
    print_budget(s);
    eprintln!();
    print_context(s);
    eprintln!();
    print_tools(s);
}

#[cfg(feature = "harness")]
fn print_budget(s: &astra_harness::RuntimeSnapshot) {
    let turns = match s.turns_limit {
        Some(limit) => format!("{} / {}", s.turns_used, limit),
        None => format!("{}", s.turns_used),
    };
    let tokens = format_tokens(s.tokens_used_session);
    let elapsed = format_duration(s.elapsed_millis);

    eprintln!("  {:<20} {}", "Turns:".bold(), turns);
    eprintln!("  {:<20} {}", "Tokens (session):".bold(), tokens);
    eprintln!("  {:<20} {}", "Elapsed:".bold(), elapsed);
}

#[cfg(feature = "harness")]
fn print_context(s: &astra_harness::RuntimeSnapshot) {
    let total = s
        .context_total_tokens
        .map(|t| format_tokens(t as u64))
        .unwrap_or_else(|| "—".into());
    let budget = s
        .context_budget_tokens
        .map(|t| format_tokens(t as u64))
        .unwrap_or_else(|| "unlimited".into());
    let util = s
        .context_utilization
        .map(|u| format!("{:.1}%", u * 100.0))
        .unwrap_or_else(|| "—".into());

    eprintln!("  {:<20} {}", "Context tokens:".bold(), total);
    eprintln!("  {:<20} {}", "Context budget:".bold(), budget);
    eprintln!("  {:<20} {}", "Utilization:".bold(), util);
    eprintln!("  {:<20} {}", "Messages:".bold(), s.context_message_count);
}

#[cfg(feature = "harness")]
fn print_tools(s: &astra_harness::RuntimeSnapshot) {
    let last = s.last_tool_called.as_deref().unwrap_or("—");
    eprintln!(
        "  {:<20} {}",
        "Tool calls:".bold(),
        s.tool_calls_this_session
    );
    eprintln!(
        "  {:<20} {}",
        "Unique tools:".bold(),
        if s.unique_tools_used.is_empty() {
            "—".to_string()
        } else {
            s.unique_tools_used.join(", ")
        }
    );
    eprintln!("  {:<20} {}", "Last tool:".bold(), last);
    if s.consecutive_same_tool > 1 {
        eprintln!(
            "  {:<20} {} (consecutive)",
            "Same tool streak:".bold(),
            s.consecutive_same_tool
        );
    }
}

#[cfg(feature = "harness")]
fn print_history(state: &SessionState, n: usize) {
    use astra_harness::SnapshotSink;

    let history = state.harness_sink.history(n);
    if history.is_empty() {
        eprintln!("{}", "  No snapshot history yet.".dim());
        return;
    }

    eprintln!(
        "\n{}",
        format!(
            "─── Snapshot History (last {}) ──────────────────",
            history.len()
        )
        .bold()
    );

    for (i, snap) in history.iter().enumerate() {
        let age = snap
            .captured_at_unix_millis
            .saturating_sub(snap.session_start_unix_millis);
        eprintln!(
            "  {} turn={:<3} tokens={:<8} tools={:<3} msgs={:<3} elapsed={}",
            if i == 0 {
                "→".green().to_string()
            } else {
                " ".to_string()
            },
            snap.turn_number,
            format_tokens(snap.tokens_used_session),
            snap.tool_calls_this_session,
            snap.context_message_count,
            format_duration(age),
        );
    }
}

#[cfg(feature = "harness")]
fn print_diff(state: &SessionState) {
    use astra_harness::{SnapshotDiff, SnapshotSink};

    let history = state.harness_sink.history(2);
    if history.len() < 2 {
        eprintln!("{}", "  Need at least 2 snapshots for diff.".dim());
        return;
    }

    let diff = SnapshotDiff::between(&history[1], &history[0]);

    if diff.is_empty() {
        eprintln!("{}", "  No changes between last two snapshots.".dim());
        return;
    }

    eprintln!(
        "\n{}",
        format!(
            "─── Diff (turn {} → {}) ─────────────────────────",
            diff.from_turn, diff.to_turn
        )
        .bold()
    );

    if diff.turns_delta != 0 {
        eprintln!("  {:<20} {:+}", "Turns:".bold(), diff.turns_delta);
    }
    if diff.tokens_delta != 0 {
        eprintln!("  {:<20} {:+}", "Tokens:".bold(), diff.tokens_delta);
    }
    if diff.elapsed_delta_millis != 0 {
        eprintln!(
            "  {:<20} {:+}ms",
            "Elapsed:".bold(),
            diff.elapsed_delta_millis
        );
    }
    if diff.tool_calls_delta != 0 {
        eprintln!("  {:<20} {:+}", "Tool calls:".bold(), diff.tool_calls_delta);
    }
    if !diff.new_tools.is_empty() {
        eprintln!(
            "  {:<20} {}",
            "New tools:".bold(),
            diff.new_tools.join(", ")
        );
    }
    if let Some(delta) = diff.context_utilization_delta {
        eprintln!("  {:<20} {:+.1}%", "Utilization:".bold(), delta * 100.0);
    }
    if diff.consecutive_same_tool_changed {
        eprintln!(
            "  {:<20} {}",
            "Stall signal:".bold(),
            "consecutive_same_tool changed".yellow()
        );
    }
}

#[cfg(feature = "harness")]
fn print_json(s: &astra_harness::RuntimeSnapshot) {
    match serde_json::to_string_pretty(s) {
        Ok(json) => eprintln!("{json}"),
        Err(e) => eprintln!("{}", format!("  Failed to serialize snapshot: {e}").red()),
    }
}

#[cfg(feature = "harness")]
fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{n}")
    }
}

#[cfg(feature = "harness")]
fn print_trace(state: &SessionState) {
    let trace = match state.harness_trace.read() {
        Ok(t) => t,
        Err(_) => {
            eprintln!("{}", "  Failed to read trace.".red());
            return;
        }
    };

    if trace.records.is_empty() {
        eprintln!("{}", "  No trace records yet.".dim());
        return;
    }

    eprintln!(
        "\n{}",
        format!(
            "─── Session Trace ({} records, {} turns) ───────",
            trace.record_count(),
            trace.total_turns,
        )
        .bold()
    );
    eprintln!(
        "  {:<14} {:<20} {:<10} {:<8} {}",
        "Turn".bold(),
        "Hook".bold(),
        "Tokens".bold(),
        "Tools".bold(),
        "Time".bold(),
    );

    for r in &trace.records {
        eprintln!(
            "  {:<14} {:<20} {:<10} {:<8} {}",
            r.turn,
            format!("{:?}", r.point),
            format_tokens(r.snapshot.tokens_used_session),
            r.snapshot.tool_calls_this_session,
            format_duration(r.monotonic_millis_since_session),
        );
    }
}

#[cfg(feature = "harness")]
fn print_forensics(state: &SessionState) {
    let trace = match state.harness_trace.read() {
        Ok(t) => t,
        Err(_) => {
            eprintln!("{}", "  Failed to read trace.".red());
            return;
        }
    };

    if trace.records.is_empty() {
        eprintln!("{}", "  No trace data for forensics.".dim());
        return;
    }

    let summary = trace.forensics_summary();

    eprintln!(
        "\n{}",
        "─── Forensics Summary ──────────────────────────".bold()
    );
    eprintln!(
        "  {:<24} {}",
        "Session:".bold(),
        summary.session_id.as_deref().unwrap_or("(none)")
    );
    eprintln!("  {:<24} {}", "Total turns:".bold(), summary.total_turns);
    eprintln!(
        "  {:<24} {}",
        "Total records:".bold(),
        summary.total_records
    );
    eprintln!(
        "  {:<24} {}",
        "Total tokens:".bold(),
        format_tokens(summary.total_tokens)
    );
    eprintln!(
        "  {:<24} {}",
        "Total tool calls:".bold(),
        summary.total_tool_calls
    );
    if let Some(peak) = summary.peak_context_utilization {
        eprintln!("  {:<24} {:.1}%", "Peak utilization:".bold(), peak * 100.0);
    }
    if summary.peak_consecutive_same_tool > 0 {
        eprintln!(
            "  {:<24} {}",
            "Peak stall streak:".bold(),
            summary.peak_consecutive_same_tool
        );
    }

    if !summary.warnings.is_empty() {
        eprintln!(
            "\n  {} ({}):",
            "Warnings".yellow().bold(),
            summary.warnings.len()
        );
        for w in &summary.warnings {
            eprintln!("    turn {}: [{:?}] {}", w.turn, w.kind, w.message);
        }
    } else {
        eprintln!("\n  {}", "No warnings detected.".green());
    }
}

#[cfg(feature = "harness")]
fn export_trace(state: &SessionState, path_arg: &str) {
    let trace = match state.harness_trace.read() {
        Ok(t) => t,
        Err(_) => {
            eprintln!("{}", "  Failed to read trace.".red());
            return;
        }
    };

    if trace.records.is_empty() {
        eprintln!("{}", "  No trace data to export.".dim());
        return;
    }

    let (policy, path_part) = if path_arg.contains("--full") {
        (
            astra_harness::PrivacyPolicy::Full,
            path_arg.replace("--full", ""),
        )
    } else if path_arg.contains("--metadata") {
        (
            astra_harness::PrivacyPolicy::MetadataOnly,
            path_arg.replace("--metadata", ""),
        )
    } else {
        (astra_harness::PrivacyPolicy::Redacted, path_arg.to_string())
    };
    let path_part = path_part.trim();

    let sanitized = trace.with_privacy(policy);

    let path = if path_part.is_empty() {
        let sid = trace
            .session_id
            .as_deref()
            .unwrap_or("none")
            .chars()
            .take(8)
            .collect::<String>();
        format!("harness_trace_{sid}.json")
    } else {
        path_part.to_string()
    };

    match sanitized.save_to_file(std::path::Path::new(&path)) {
        Ok(()) => eprintln!(
            "  {} {} ({} records, policy: {:?})",
            "Exported:".green(),
            path,
            sanitized.record_count(),
            policy,
        ),
        Err(e) => eprintln!("{}", format!("  Export failed: {e}").red()),
    }
}

#[cfg(feature = "harness")]
fn format_duration(millis: u64) -> String {
    if millis >= 60_000 {
        format!("{}m {}s", millis / 60_000, (millis % 60_000) / 1000)
    } else if millis >= 1_000 {
        format!("{:.1}s", millis as f64 / 1000.0)
    } else {
        format!("{millis}ms")
    }
}

#[cfg(test)]
mod tests {
    use super::{InspectorFactStatus, WorkbenchInspection, inspect_workbench};
    use crate::cli::session::session_state::SessionState;

    fn fact<'a>(
        report: &'a WorkbenchInspection,
        section_title: &str,
        label: &str,
    ) -> &'a super::InspectorFact {
        report
            .sections
            .iter()
            .find(|section| section.title == section_title)
            .and_then(|section| section.facts.iter().find(|fact| fact.label == label))
            .unwrap_or_else(|| panic!("missing {section_title}/{label} fact"))
    }

    #[tokio::test]
    async fn workbench_inspector_reports_session_facts_without_a_harness_snapshot() {
        let mut state = SessionState::default();
        state.session_id = Some("session-inspect".into());
        state.run_id = Some("run-inspect".into());
        state.model = Some("model-inspect".into());
        state.turn = 3;
        state.total_prompt_tokens = 120;
        state.total_completion_tokens = 45;

        let report = inspect_workbench(&state);

        assert_eq!(
            report
                .sections
                .iter()
                .map(|section| section.title.as_str())
                .collect::<Vec<_>>(),
            [
                "State",
                "Capability & provider",
                "Context",
                "Harness evidence",
            ]
        );
        assert_eq!(
            fact(&report, "State", "Session"),
            &super::InspectorFact::observed("Session", "session-inspect")
        );
        assert_eq!(
            fact(&report, "State", "Run"),
            &super::InspectorFact::observed("Run", "run-inspect")
        );
        assert_eq!(
            fact(&report, "State", "Model"),
            &super::InspectorFact::observed("Model", "model-inspect")
        );
        assert_eq!(
            fact(&report, "Context", "Prompt tokens"),
            &super::InspectorFact::observed("Prompt tokens", "120")
        );
        assert_eq!(
            fact(&report, "Capability & provider", "MCP connections"),
            &super::InspectorFact::observed("MCP connections", "0")
        );
    }

    #[tokio::test]
    async fn workbench_inspector_marks_absent_identity_and_trace_as_not_recorded() {
        let report = inspect_workbench(&SessionState::default());

        for (section, label) in [
            ("State", "Session"),
            ("State", "Run"),
            ("State", "Model"),
            ("Context", "Assembly trace"),
            ("Harness evidence", "Runtime snapshot"),
        ] {
            assert_eq!(
                fact(&report, section, label).status,
                InspectorFactStatus::NotRecorded,
                "{section}/{label} must remain an absence of evidence"
            );
        }
    }

    #[tokio::test]
    async fn workbench_inspector_reports_busy_mcp_as_unrecorded_without_waiting() {
        let state = SessionState::default();
        let _writer = state.mcp_manager.write().await;

        let report = inspect_workbench(&state);

        assert_eq!(
            fact(&report, "Capability & provider", "MCP connections").status,
            InspectorFactStatus::NotRecorded
        );
        assert!(
            fact(&report, "Capability & provider", "MCP connections")
                .value
                .contains("live manager busy")
        );
    }
}
