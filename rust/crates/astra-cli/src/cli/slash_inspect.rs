#![allow(unused_imports)]
use super::*;

#[cfg(feature = "harness")]
pub(super) fn handle_inspect_command(arg: &str, state: &ReplState) {
    use astra_harness::SnapshotSink;

    let snapshot = match state.harness_sink.latest() {
        Some(s) => s,
        None => {
            eprintln!("{}", "  No harness snapshot available yet.".dim());
            return;
        }
    };

    let trimmed = arg.trim();
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
                "  Usage: /inspect [budget|tools|context|json|diff|history N|trace|forensics|export path]"
            );
        }
    }
}

#[cfg(feature = "harness")]
pub(super) fn format_snapshot_summary(s: &astra_harness::RuntimeSnapshot) -> String {
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

#[cfg(not(feature = "harness"))]
pub(super) fn handle_inspect_command(_arg: &str, _state: &ReplState) {
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
fn print_history(state: &ReplState, n: usize) {
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
fn print_diff(state: &ReplState) {
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
fn print_trace(state: &ReplState) {
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
fn print_forensics(state: &ReplState) {
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
fn export_trace(state: &ReplState, path_arg: &str) {
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
