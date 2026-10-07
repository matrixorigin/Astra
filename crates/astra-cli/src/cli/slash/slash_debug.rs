use crate::cli::session::session_state::SessionState;
use crate::cli::stream::stream_render;
use crate::cli::theme;
use crossterm::style::Stylize;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

/// Interactive debug inspector for session turns.
///
/// Data sources (in priority order):
/// Each source is scoped to the attached CLI profile or account owner.
/// 1. Heavy checkpoints: owner-scoped `step_checkpoints/*-heavy.json`
///    → full messages array (the actual LLM input/output)
/// 2. Journal JSONL: the same owner's `<id>.jsonl`
///    → turn summaries, tool calls, timing, token counts
///
/// **Per-turn view:** journal turn *T* is paired with the *T*-th heavy checkpoint file (sorted by
/// numeric prefix). UI and JSON dumps show **message delta** (suffix after shared prefix with the
/// previous heavy snapshot), not the entire accumulated history. If there are fewer heavy files
/// than journal turns, the latest heavy file is used and a warning is recorded.
/// Used by the non-interactive `astra debug` bridge in `command_router`.
/// Interactive users inspect live session evidence with native `/inspect`.
pub(crate) fn handle_debug_command(arg: &str, state: &SessionState) {
    let session_id = if arg.is_empty() {
        match &state.session_id {
            Some(id) => id.clone(),
            None => {
                eprintln!(
                    "{}",
                    "  No active session. Usage: /debug <session_id>".yellow()
                );
                return;
            }
        }
    } else {
        match resolve_session_id(arg.trim()) {
            Ok(id) => id,
            Err(error) => {
                eprintln!("  {} {}", theme::icon_err(), error);
                return;
            }
        }
    };

    let sources = match crate::cli::journal_digest::read_attached_journal_sources(&session_id) {
        Ok(sources) => sources,
        Err(error) => {
            eprintln!("  {} {}", theme::icon_err(), error);
            return;
        }
    };
    let store = astra_services::local_session_artifact_store();
    let mut found = false;
    for source in sources {
        let base = match astra_services::SessionArtifactStore::session_dir_for_owner(
            &store,
            &source.owner,
            &session_id,
        ) {
            Ok(base) => base,
            Err(error) => {
                eprintln!("  {} {}", theme::icon_err(), error);
                return;
            }
        };
        let checkpoints = list_heavy_checkpoints(&base);
        let turns = project_journal_turns(&source.events);
        if turns.is_empty() && checkpoints.is_empty() {
            continue;
        }
        found = true;
        eprintln!("\n  Source: {}", source.path.display());
        if !inspect_debug_source(
            &session_id,
            &source.owner,
            &source.events,
            &turns,
            &checkpoints,
        ) {
            return;
        }
    }
    if !found {
        eprintln!(
            "{}",
            format!("  No data found for session {session_id}").yellow()
        );
    }
}

fn inspect_debug_source(
    session_id: &str,
    owner: &astra_services::OwnerScope,
    events: &[astra_services::session_journal::JournalEvent],
    turns: &[TurnSummary],
    checkpoints: &[PathBuf],
) -> bool {
    if !turns.is_empty() && !checkpoints.is_empty() && turns.len() != checkpoints.len() {
        eprintln!(
            "  {}",
            format!(
                "Note: {} journal turns vs {} heavy checkpoints — pairing by index; deltas use adjacent heavy files.",
                turns.len(),
                checkpoints.len()
            )
            .dim()
        );
    }

    // ── Overview ──
    print_overview(session_id, turns, checkpoints);

    // If journal has no turns but checkpoints exist, offer checkpoint-only inspection.
    if turns.is_empty() {
        eprintln!(
            "\n  {}",
            "No journal turns (journal may not have been initialized).".dim()
        );
        eprintln!(
            "  {} checkpoints available — inspecting latest segment.",
            checkpoints.len().to_string().green()
        );
        if let Some(view) =
            build_turn_messages_view(owner, session_id, checkpoints.len(), checkpoints)
        {
            let stub = TurnSummary {
                journal_turn: None,
                user_input: view
                    .delta
                    .iter()
                    .chain(view.full.iter())
                    .find_map(human_user_text)
                    .unwrap_or("(unknown)")
                    .to_string(),
                tokens_in: 0,
                tokens_out: 0,
                duration_ms: 0,
                ttft_ms: 0,
                tool_count: view
                    .delta
                    .iter()
                    .filter(|m| m.get("role").and_then(|v| v.as_str()) == Some("tool"))
                    .count(),
                tools_used: Vec::new(),
                tool_calls: Vec::new(),
                llm_rounds: Vec::new(),
                interruptions: Vec::new(),
            };
            inspect_turn(1, &stub, Some(&view), session_id, owner);
        } else {
            eprintln!("  {}", "Failed to load checkpoint data.".yellow());
        }
        return true;
    }

    // ── Interactive loop ──
    loop {
        eprint!(
            "\n  Which turn? [1-{}, bp, ct, cs, n for next source, q to quit]: ",
            turns.len().max(1)
        );
        io::stderr().flush().ok();
        let Some(line) = read_line() else {
            return false;
        };
        let line = line.trim().to_lowercase();
        match line.as_str() {
            "q" | "quit" => return false,
            "n" | "next" => return true,
            "bp" | "breakpoints" => {
                show_breakpoints(owner, session_id);
                continue;
            }
            "cs" | "snapshots" => {
                show_composite_snapshots(owner, session_id);
                continue;
            }
            "ct" | "corrections" => {
                show_correction_timeline(events);
                continue;
            }
            _ => {}
        }
        let Ok(turn_n) = line.parse::<usize>() else {
            eprintln!(
                "  {}",
                "Invalid input — enter a turn number, bp, ct, cs, n, or q".yellow()
            );
            continue;
        };
        if turn_n == 0 || turn_n > turns.len() {
            eprintln!("  {}", format!("Turn {turn_n} not found").yellow());
            continue;
        }

        let view = build_turn_messages_view(owner, session_id, turn_n, checkpoints);
        if view.is_none() {
            eprintln!(
                "  {}",
                "No heavy checkpoint messages for this turn.".yellow()
            );
            continue;
        }
        inspect_turn(turn_n, &turns[turn_n - 1], view.as_ref(), session_id, owner);
    }
}

fn human_user_text(message: &serde_json::Value) -> Option<&str> {
    astra_turn_types::is_human_user_message(message)
        .then(|| message.get("content").and_then(serde_json::Value::as_str))
        .flatten()
}

// ── Overview ──────────────────────────────────────────────────────────────────

fn print_overview(session_id: &str, turns: &[TurnSummary], checkpoints: &[PathBuf]) {
    let short_id = &session_id[..8.min(session_id.len())];
    eprintln!(
        "\n  🔍 session {} ({} turns, {} checkpoints)\n",
        short_id.magenta(),
        turns.len().to_string().green(),
        checkpoints.len().to_string().dim(),
    );
    for (i, t) in turns.iter().enumerate() {
        let tn = i + 1;
        let tools_str = if t.tools_used.is_empty() {
            "(no tools)".dim().to_string()
        } else {
            t.tools_used.join(", ").dim().to_string()
        };
        eprintln!(
            "  {}  {:.1}s  {}→{}tok  tools: {}",
            format!("T{tn}").bold(),
            t.duration_ms as f64 / 1000.0,
            t.tokens_in,
            t.tokens_out,
            tools_str,
        );
    }
}

// ── Turn messages (heavy checkpoints) ───────────────────────────────────────

/// Heavy snapshots for one journal turn: full history at end of segment plus message delta vs previous heavy file.
struct TurnMessagesView {
    delta: Vec<serde_json::Value>,
    full: Vec<serde_json::Value>,
    after_path: PathBuf,
    before_path: Option<PathBuf>,
    warning: Option<String>,
}

fn checkpoint_numeric_prefix(path: &Path) -> Option<u32> {
    path.file_name()?.to_str()?.split_once('-')?.0.parse().ok()
}

fn load_messages_from_heavy_path(
    owner: &astra_services::OwnerScope,
    session_id: &str,
    path: &Path,
) -> Option<Vec<serde_json::Value>> {
    astra_pipeline::step_checkpoint::read_heavy_checkpoint(
        owner.id(),
        session_id,
        checkpoint_numeric_prefix(path)?,
    )
    .ok()?
    .map(|checkpoint| checkpoint.messages)
}

fn message_delta(
    before: &[serde_json::Value],
    after: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    let mut i = 0;
    let n = before.len().min(after.len());
    while i < n && before[i] == after[i] {
        i += 1;
    }
    crate::cli::history_work::record_measured_work(
        astra_core::history_work::HistoryWorkSite::CliDebugHistoryDeltaComparison,
        0,
        i.saturating_add(usize::from(i < n)),
    );
    crate::cli::history_work::clone_json_history(
        astra_core::history_work::HistoryWorkSite::CliDebugHistoryDeltaClone,
        &after[i..],
    )
}

fn build_turn_messages_view(
    owner: &astra_services::OwnerScope,
    session_id: &str,
    turn_n: usize,
    checkpoints: &[PathBuf],
) -> Option<TurnMessagesView> {
    if checkpoints.is_empty() {
        return None;
    }
    let warning = if turn_n > checkpoints.len() {
        Some(format!(
            "Journal ordinal {} exceeds {} heavy checkpoint(s); using latest heavy file and delta vs the previous one.",
            turn_n,
            checkpoints.len()
        ))
    } else {
        None
    };
    let after_idx = if turn_n > checkpoints.len() {
        checkpoints.len() - 1
    } else {
        turn_n - 1
    };
    let after_path = checkpoints.get(after_idx)?.clone();
    let full = load_messages_from_heavy_path(owner, session_id, &after_path)?;
    let (before_path, before_msgs) = if after_idx > 0 {
        let bp = checkpoints[after_idx - 1].clone();
        let bm = load_messages_from_heavy_path(owner, session_id, &bp)?;
        (Some(bp), bm)
    } else {
        (None, Vec::new())
    };
    let delta = message_delta(&before_msgs, &full);
    Some(TurnMessagesView {
        delta,
        full,
        after_path,
        before_path,
        warning,
    })
}

// ── Turn inspector ───────────────────────────────────────────────────────────

fn inspect_turn(
    turn_n: usize,
    summary: &TurnSummary,
    view: Option<&TurnMessagesView>,
    session_id: &str,
    owner: &astra_services::OwnerScope,
) {
    let journal_tag = summary
        .journal_turn
        .map(|t| format!(" (journal #{t})"))
        .unwrap_or_default();
    eprintln!(
        "\n  {}{} — {} tool calls, {:.1}s, {}→{}tok",
        format!("Turn {turn_n}").bold(),
        journal_tag.dim(),
        summary.tool_count.to_string().magenta(),
        summary.duration_ms as f64 / 1000.0,
        summary.tokens_in.to_string().dim(),
        summary.tokens_out.to_string().dim(),
    );

    if let Some(w) = view.and_then(|v| v.warning.as_deref()) {
        eprintln!("  {}", w.yellow());
    }

    let has_msgs = view.is_some();
    eprintln!(
        "  {} input    — LLM input ({} only){}",
        "[1]".magenta(),
        "delta".green(),
        if has_msgs { "" } else { " (no checkpoint)" }
    );
    eprintln!(
        "  {} output   — LLM response ({})",
        "[2]".magenta(),
        "delta".green()
    );
    eprintln!(
        "  {} tools    — tool calls + results ({})",
        "[3]".magenta(),
        "delta".green()
    );
    eprintln!(
        "  {} injected — runtime-injected ({})",
        "[4]".magenta(),
        "delta".green()
    );
    eprintln!(
        "  {} json     — structured delta dump (pretty) → /tmp",
        "[5]".magenta()
    );
    eprintln!(
        "  {} full json — entire snapshot after this segment → /tmp",
        "[7]".magenta()
    );
    eprintln!("  {} summary  — journal turn summary", "[6]".magenta());

    loop {
        eprint!("  What to inspect? [1-7, b to go back]: ");
        io::stderr().flush().ok();
        let Some(line) = read_line() else { return };
        let line = line.trim().to_lowercase();
        if line == "b" || line == "back" {
            return;
        }
        match line.as_str() {
            "1" => show_input(view),
            "2" => show_output(view),
            "3" => show_tools(view, summary),
            "4" => show_injected(view),
            "5" => dump_turn_json(view, summary, session_id, owner, turn_n, false),
            "6" => show_summary(summary),
            "7" => dump_turn_json(view, summary, session_id, owner, turn_n, true),
            _ => eprintln!("  {}", "Invalid choice".yellow()),
        }
    }
}

fn show_input(view: Option<&TurnMessagesView>) {
    let Some(v) = view else {
        eprintln!("  {}", "No checkpoint data available".yellow());
        return;
    };
    if v.delta.is_empty() {
        eprintln!(
            "  {}",
            "(delta empty — no new messages vs previous heavy checkpoint)".dim()
        );
        return;
    }
    let msgs = v.delta.as_slice();
    eprintln!("\n  {}", "── LLM Input (delta) ──".bold().magenta());
    for m in msgs {
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("?");
        if role == "assistant" || role == "tool" {
            continue;
        }
        let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let tag = format!("[{role}]").magenta();
        let preview = truncate(content, 300);
        eprintln!("  {tag} {preview}");
    }
    eprintln!();
}

fn show_output(view: Option<&TurnMessagesView>) {
    let Some(v) = view else {
        eprintln!("  {}", "No checkpoint data available".yellow());
        return;
    };
    if v.delta.is_empty() {
        eprintln!(
            "  {}",
            "(delta empty — no new messages vs previous heavy checkpoint)".dim()
        );
        return;
    }
    let msgs = v.delta.as_slice();
    eprintln!("\n  {}", "── LLM Output (delta) ──".bold().magenta());
    for m in msgs {
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("?");
        if role != "assistant" {
            continue;
        }
        // Reasoning content
        if let Some(reasoning) = m.get("reasoning_content").and_then(|v| v.as_str())
            && !reasoning.is_empty()
        {
            eprintln!("  {} {}", "[thinking]".dim(), truncate(reasoning, 500));
        }
        // Text content
        if let Some(content) = m.get("content").and_then(|v| v.as_str())
            && !content.is_empty()
        {
            eprintln!("  {} {}", "[text]".green(), truncate(content, 500));
        }
        // Tool calls
        if let Some(tc) = m.get("tool_calls").and_then(|v| v.as_array()) {
            let names: Vec<&str> = tc
                .iter()
                .filter_map(|t| {
                    t.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                })
                .collect();
            if !names.is_empty() {
                eprintln!("  {} {}", "[tool_calls]".yellow(), names.join(", "));
            }
        }
    }
    eprintln!();
}

fn show_tools(view: Option<&TurnMessagesView>, summary: &TurnSummary) {
    eprintln!("\n  {}", "── Tool Calls ──".bold().magenta());
    // From journal (always available)
    for tc in &summary.tool_calls {
        let status = if tc.ok {
            theme::icon_ok()
        } else {
            theme::icon_err()
        };
        let display_name =
            stream_render::format_tool_display_from_preview(&tc.name, tc.args_preview.as_deref());
        eprintln!(
            "  {status} {} {}",
            display_name.magenta(),
            format!("({}B→{}B)", tc.input_bytes, tc.output_bytes).dim(),
        );
    }
    // From checkpoint delta (this segment only)
    if let Some(v) = view {
        eprintln!("\n  {}", "── Tool Results (checkpoint delta) ──".dim());
        for m in &v.delta {
            let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("?");
            if role != "tool" {
                continue;
            }
            let name = m
                .get("name")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    // Fall back to tool_call_id (e.g. "git:0" → "git")
                    m.get("tool_call_id")
                        .and_then(|v| v.as_str())
                        .and_then(|id| id.split(':').next())
                })
                .unwrap_or("?");
            let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
            eprintln!(
                "  {} {}",
                format!("[{name}]").magenta(),
                truncate(content, 200)
            );
        }
    }
    eprintln!();
}

fn show_injected(view: Option<&TurnMessagesView>) {
    let Some(v) = view else {
        eprintln!("  {}", "No checkpoint data available".yellow());
        return;
    };
    if v.delta.is_empty() {
        eprintln!(
            "  {}",
            "(delta empty — no new messages vs previous heavy checkpoint)".dim()
        );
        return;
    }
    let msgs = v.delta.as_slice();
    eprintln!("\n  {}", "── Injected Messages (delta) ──".bold().magenta());
    let mut found = false;
    for m in msgs {
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("?");
        let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
        if astra_turn_types::is_runtime_owned_message(m) {
            found = true;
            eprintln!(
                "  {} {}",
                format!("[{role}]").yellow(),
                truncate(content, 400)
            );
        }
    }
    if !found {
        eprintln!("  {}", "(none detected)".dim());
    }
    eprintln!();
}

fn dump_turn_json(
    view: Option<&TurnMessagesView>,
    summary: &TurnSummary,
    session_id: &str,
    owner: &astra_services::OwnerScope,
    turn_n: usize,
    full_snapshot: bool,
) {
    let Some(v) = view else {
        eprintln!("  {}", "No checkpoint data available".yellow());
        return;
    };
    let short = &session_id[..8.min(session_id.len())];
    let suffix = if full_snapshot { "-full" } else { "" };
    let path = std::env::temp_dir().join(format!(
        "debug-{short}-{}-turn{turn_n}{suffix}.json",
        uuid::Uuid::new_v4()
    ));
    let dump_messages = if full_snapshot { &v.full } else { &v.delta };
    crate::cli::history_work::record_json_history(
        astra_core::history_work::HistoryWorkSite::CliDebugDumpPayloadClone,
        dump_messages,
    );

    let payload = if full_snapshot {
        serde_json::json!({
            "schema": "astra-debug-turn-full-v1",
            "session_id": session_id,
            "owner_id": owner.id(),
            "inspect": {
                "journal_turn_ordinal": turn_n,
                "journal_turn_field": summary.journal_turn,
                "checkpoint_after": file_name_str(&v.after_path),
                "checkpoint_before": v.before_path.as_ref().and_then(|p| file_name_str(p)),
                "message_count": v.full.len(),
            },
            "warning": v.warning,
            "messages": v.full,
        })
    } else {
        serde_json::json!({
            "schema": "astra-debug-turn-delta-v1",
            "session_id": session_id,
            "owner_id": owner.id(),
            "inspect": {
                "journal_turn_ordinal": turn_n,
                "journal_turn_field": summary.journal_turn,
                "checkpoint_after": file_name_str(&v.after_path),
                "checkpoint_before": v.before_path.as_ref().and_then(|p| file_name_str(p)),
                "delta_message_count": v.delta.len(),
                "full_message_count": v.full.len(),
            },
            "warning": v.warning,
            "journal_turn_summary": {
                "user_input": summary.user_input,
                "tokens_in": summary.tokens_in,
                "tokens_out": summary.tokens_out,
                "duration_ms": summary.duration_ms,
                "ttft_ms": summary.ttft_ms,
                "tool_count": summary.tool_count,
                "tools_used": summary.tools_used,
                "llm_rounds": summary.llm_rounds.iter().map(|round| serde_json::json!({
                    "round": round.round,
                    "agentic_step": round.agentic_step,
                    "source": round.source,
                    "run_id": round.run_id,
                    "finish_reason": round.finish_reason,
                    "tool_calls_returned": round.tool_calls_returned,
                })).collect::<Vec<_>>(),
                "interruptions": summary.interruptions.iter().map(|interruption| serde_json::json!({
                    "kind": interruption.kind,
                    "resumable": interruption.resumable,
                    "agentic_step": interruption.agentic_step,
                    "tool_calls_completed": interruption.tool_calls_completed,
                    "turns_completed": interruption.turns_completed,
                    "remaining_turns": interruption.remaining_turns,
                })).collect::<Vec<_>>(),
            },
            "messages_delta": v.delta,
        })
    };

    match serde_json::to_string_pretty(&payload) {
        Ok(s) => {
            crate::cli::history_work::record_existing_buffer(
                astra_core::history_work::HistoryWorkSite::CliDebugDumpSerialization,
                s.as_bytes(),
                dump_messages.len(),
            );
            match std::fs::write(&path, s) {
                Ok(()) => eprintln!(
                    "  {} {}",
                    theme::icon_ok(),
                    format!("Written to {}", path.display()).dim()
                ),
                Err(e) => eprintln!("  {} {}", theme::icon_err(), e),
            }
        }
        Err(e) => eprintln!("  {} {}", theme::icon_err(), e),
    }
}

fn file_name_str(p: &Path) -> Option<String> {
    p.file_name().map(|n| n.to_string_lossy().into_owned())
}

fn show_summary(summary: &TurnSummary) {
    eprintln!("\n  {}", "── Journal Summary ──".bold().magenta());
    eprintln!("  user:     {}", truncate(&summary.user_input, 120));
    eprintln!("  tokens:   {}→{}", summary.tokens_in, summary.tokens_out);
    eprintln!("  duration: {:.1}s", summary.duration_ms as f64 / 1000.0);
    eprintln!("  ttft:     {}ms", summary.ttft_ms);
    eprintln!(
        "  tools:    {} calls ({})",
        summary.tool_count,
        summary.tools_used.join(", ")
    );
    if !summary.llm_rounds.is_empty() {
        eprintln!("  llm:      {} round(s)", summary.llm_rounds.len());
        for round in &summary.llm_rounds {
            let source = round.source.as_deref().unwrap_or("agentic_loop");
            let finish_reason = round.finish_reason.as_deref().unwrap_or("-");
            let step = round
                .agentic_step
                .map(|value| format!(" step={value}"))
                .unwrap_or_default();
            let run = round
                .run_id
                .as_deref()
                .map(|value| format!(" run={value}"))
                .unwrap_or_default();
            eprintln!(
                "            r{}{} {} finish={} tools={}{}",
                round.round.unwrap_or(0),
                step,
                source,
                finish_reason,
                round.tool_calls_returned,
                run,
            );
        }
    }
    if !summary.interruptions.is_empty() {
        for interruption in &summary.interruptions {
            let step = interruption
                .agentic_step
                .map(|value| format!(" step={value}"))
                .unwrap_or_default();
            eprintln!(
                "  stop:     {}{} resumable={} tools={} turns={} remaining={}",
                interruption.kind,
                step,
                interruption.resumable,
                interruption.tool_calls_completed,
                interruption.turns_completed,
                interruption.remaining_turns,
            );
        }
    }
    eprintln!();
}

// ── Data loading ─────────────────────────────────────────────────────────────

/// Resolve a (possibly short) session ID to a full UUID by prefix match.
fn resolve_session_id(input: &str) -> Result<String, String> {
    let (local, account) = crate::cli::cli_config::cli_utils::attached_journal_owners()?;
    let mut matches = std::collections::BTreeSet::new();
    for owner in std::iter::once(&local).chain(account.as_ref()) {
        for id in astra_services::session_journal::list_sessions_for_owner(owner)
            .map_err(|error| error.to_string())?
        {
            if id == input {
                return Ok(id);
            }
            if id.starts_with(input) {
                matches.insert(id);
            }
        }
    }
    match matches.len() {
        0 => Ok(input.to_string()), // Full IDs also support checkpoint-only inspection.
        1 => Ok(matches.into_iter().next().expect("one match")),
        _ => Err(format!(
            "session prefix '{input}' is ambiguous among attached journals"
        )),
    }
}

#[derive(Debug)]
struct TurnSummary {
    /// `turn` field from the journal line when present (1-based session turn counter).
    journal_turn: Option<u32>,
    user_input: String,
    tokens_in: u64,
    tokens_out: u64,
    duration_ms: u64,
    ttft_ms: u64,
    tool_count: usize,
    tools_used: Vec<String>,
    tool_calls: Vec<ToolCallSummary>,
    llm_rounds: Vec<LlmRoundSummary>,
    interruptions: Vec<InterruptionSummary>,
}

#[derive(Debug)]
struct ToolCallSummary {
    name: String,
    ok: bool,
    input_bytes: u64,
    output_bytes: u64,
    args_preview: Option<String>,
}

#[derive(Debug)]
struct LlmRoundSummary {
    round: Option<u32>,
    agentic_step: Option<u32>,
    source: Option<String>,
    run_id: Option<String>,
    finish_reason: Option<String>,
    tool_calls_returned: u64,
}

#[derive(Debug)]
struct InterruptionSummary {
    kind: String,
    resumable: bool,
    agentic_step: Option<u32>,
    tool_calls_completed: u64,
    turns_completed: u64,
    remaining_turns: u64,
}

#[cfg(test)]
fn load_journal_turns(path: &PathBuf) -> Vec<TurnSummary> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let entries = content
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect::<Vec<_>>();
    project_journal_turns(&entries)
}

fn project_journal_turns(
    entries: &[astra_services::session_journal::JournalEvent],
) -> Vec<TurnSummary> {
    use astra_services::session_journal::JournalEventType;
    let mut turns = Vec::new();
    let mut turn_index_by_id = std::collections::HashMap::new();
    for event in entries
        .iter()
        .filter(|event| event.event_type == JournalEventType::Turn)
    {
        let index = turns.len();
        turns.push(TurnSummary {
            journal_turn: event.turn,
            user_input: event.user_input.clone().unwrap_or_default(),
            tokens_in: event.tokens_in.unwrap_or(0),
            tokens_out: event.tokens_out.unwrap_or(0),
            duration_ms: event.duration_ms.unwrap_or(0),
            ttft_ms: event.ttft_ms.unwrap_or(0),
            tool_count: event.tool_count.unwrap_or(0) as usize,
            tools_used: event.tools_used.clone().unwrap_or_default(),
            tool_calls: event
                .tool_calls
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|call| ToolCallSummary {
                    name: call.name.clone(),
                    ok: call.ok,
                    input_bytes: u64::from(call.input_bytes.unwrap_or(0)),
                    output_bytes: u64::from(call.output_bytes.unwrap_or(0)),
                    args_preview: call.args_preview.clone(),
                })
                .collect(),
            llm_rounds: Vec::new(),
            interruptions: Vec::new(),
        });
        if let Some(turn) = event.turn {
            turn_index_by_id.insert(turn, index);
        }
    }
    for event in entries {
        let Some(index) = event.turn.and_then(|turn| turn_index_by_id.get(&turn)) else {
            continue;
        };
        match event.event_type {
            JournalEventType::LlmRound => {
                let text = |key| {
                    event
                        .metadata
                        .as_ref()?
                        .get(key)?
                        .as_str()
                        .map(str::to_owned)
                };
                turns[*index].llm_rounds.push(LlmRoundSummary {
                    round: event.round,
                    agentic_step: event.agentic_step,
                    source: text("source"),
                    run_id: text("run_id"),
                    finish_reason: text("finish_reason"),
                    tool_calls_returned: u64::from(event.tool_calls_returned.unwrap_or(0)),
                });
            }
            JournalEventType::InterruptionRecorded => {
                let interruption = event
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("interruption"));
                let number = |key| {
                    interruption
                        .and_then(|value| value.get(key))
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0)
                };
                turns[*index].interruptions.push(InterruptionSummary {
                    kind: interruption
                        .and_then(|value| value.get("kind"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                    resumable: interruption
                        .and_then(|value| value.get("resumable"))
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                    agentic_step: event.agentic_step,
                    tool_calls_completed: number("tool_calls_completed"),
                    turns_completed: number("turns_completed"),
                    remaining_turns: number("remaining_turns"),
                });
            }
            _ => {}
        }
    }
    turns
}

fn list_heavy_checkpoints(session_dir: &Path) -> Vec<PathBuf> {
    let cp_dir = session_dir.join("step_checkpoints");
    let Ok(entries) = std::fs::read_dir(&cp_dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with("-heavy.json"))
        })
        .collect();
    paths.sort_by_key(|p| checkpoint_numeric_prefix(p).unwrap_or(0));
    paths
}

// ── Breakpoints ─────────────────────────────────────────────────────────────

fn show_breakpoints(owner: &astra_services::OwnerScope, session_id: &str) {
    match astra_pipeline::step_checkpoint::read_breakpoint_index(owner.id(), session_id) {
        Ok(index) => {
            if index.breakpoints.is_empty() {
                eprintln!("  {}", "(no breakpoints)".dim());
                return;
            }
            eprintln!("\n  {}", "── Breakpoints ──".bold().magenta());
            for bp in &index.breakpoints {
                let short_id = &bp.breakpoint_id[..8.min(bp.breakpoint_id.len())];
                eprintln!(
                    "  {} turn {} — {} ({})",
                    short_id.magenta(),
                    bp.turn_number.to_string().green(),
                    bp.label,
                    bp.created_at.as_str().dim(),
                );
            }
            eprintln!();
        }
        Err(e) => eprintln!("  {} {}", theme::icon_err(), e),
    }
}

fn show_composite_snapshots(owner: &astra_services::OwnerScope, session_id: &str) {
    let index = match astra_pipeline::step_checkpoint::read_composite_snapshot_index(
        owner.id(),
        session_id,
    ) {
        Ok(index) => index,
        Err(error) => {
            eprintln!("  {} {}", theme::icon_err(), error);
            return;
        }
    };

    if index.snapshots.is_empty() {
        eprintln!("  {}", "No composite snapshots found.".dim());
        return;
    }

    eprintln!(
        "\n  {}",
        format!("─── Composite Snapshots ({}) ───", index.snapshots.len())
            .bold()
            .magenta()
    );

    for snap in &index.snapshots {
        let dims = snap.dimensions().join(", ");
        let label = snap.label.as_deref().unwrap_or("-");
        eprintln!(
            "  {} T{:<3} {} [{}]  {}",
            snap.snapshot_id[..8.min(snap.snapshot_id.len())].magenta(),
            snap.turn,
            label,
            dims.green(),
            snap.created_at.as_str().dim(),
        );
    }
    eprintln!();
}

// ── Correction Timeline ─────────────────────────────────────────────────────

fn show_correction_timeline(events: &[astra_services::session_journal::JournalEvent]) {
    let verdicts: Vec<_> = events
        .iter()
        .filter(|e| {
            e.event_type == astra_services::session_journal::JournalEventType::TurnGuardVerdict
        })
        .collect();

    if verdicts.is_empty() {
        eprintln!("  {}", "(no correction events)".dim());
        return;
    }

    eprintln!("\n  {}", "── Correction Timeline ──".bold().magenta());
    for evt in &verdicts {
        let turn = evt
            .turn
            .map(|t| t.to_string())
            .unwrap_or_else(|| "?".into());
        let severity = evt.stall_type.as_deref().unwrap_or("?");
        let meta = evt.metadata.as_ref();

        let injections = meta
            .and_then(|m| m.get("injections"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let avoid_tools = meta
            .and_then(|m| m.get("avoid_tools"))
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let advisory_threshold_reached = meta
            .and_then(|m| m.get("advisory_threshold_reached"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let avoid_reason = meta
            .and_then(|m| m.get("avoid_reason_summary"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let severity_colored = match severity {
            "critical" => severity.red().to_string(),
            "warning" => severity.yellow().to_string(),
            "info" => severity.dim().to_string(),
            _ => severity.to_string(),
        };

        let avoid_str = if avoid_tools.is_empty() {
            String::new()
        } else {
            format!(", avoid: [{}]", avoid_tools)
        };
        let stop_str = if advisory_threshold_reached {
            format!(" {}", "strong advisory".yellow())
        } else {
            String::new()
        };
        let reason_str = if avoid_reason.is_empty() {
            String::new()
        } else {
            format!("\n      {}", avoid_reason.dim())
        };

        eprintln!(
            "  T{} {} — {} TurnGuard observation(s), not model feedback{}{}{}",
            turn.bold(),
            severity_colored,
            injections,
            avoid_str,
            stop_str,
            reason_str,
        );
    }
    eprintln!();
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn read_line() -> Option<String> {
    let mut buf = String::new();
    io::stdin().lock().read_line(&mut buf).ok()?;
    if buf.is_empty() {
        return None;
    }
    Some(buf)
}

/// Produce a descriptive tool name for debug display.
/// `"skill"` → `"Skill <name>"` (extracted from args_preview JSON `skill_name` field).
/// MCP tools (`mcp_<server>_<tool>`) → `"MCP <server> <tool>"`.
fn truncate(s: &str, max: usize) -> String {
    let flat = s.replace('\n', "\\n");
    if flat.len() <= max {
        flat
    } else {
        // Find a char boundary at or before `max`.
        let end = flat.floor_char_boundary(max);
        format!("{}… [truncated, {} total]", &flat[..end], s.len())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_turn_messages_view, human_user_text, list_heavy_checkpoints, load_journal_turns,
        load_messages_from_heavy_path, message_delta, resolve_session_id, truncate,
    };
    use serde_json::json;
    use std::path::PathBuf;

    #[serial_test::serial]
    #[test]
    fn initialized_account_debug_rejects_foreign_sessions_and_cursors() {
        use astra_services::session_journal::JournalEvent;
        let _home = crate::test_utils::HomeGuard::temp();
        let _root = crate::test_utils::ProcessEnvGuard::remove("ASTRA_LOCAL_STATE_ROOT");
        let (_directory, _sessions) = crate::tests::isolated_sessions_dir();
        let _credentials = crate::tests::isolate_credentials();
        let _identity = crate::cli::cli_config::cli_utils::install_cli_profile_identity_for_test(
            "default",
            Some("debug-account"),
        )
        .unwrap();
        let state = crate::cli::session::session_runtime::initialize_session_state(
            None,
            None,
            &crate::cli::cli_config::cli_context::CliContext::default(),
        );
        assert_eq!(state.ingestion_user_id.as_deref(), Some("debug-account"));
        let (local, account) =
            crate::cli::cli_config::cli_utils::attached_journal_owners().unwrap();
        let account = account.unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let other_session = uuid::Uuid::new_v4().to_string();
        let unrelated = astra_services::OwnerScope::user("unrelated-account").unwrap();
        let turn = |session: &str, text: &str| {
            JournalEvent::turn(Some(session), 1, None, text, "done", 0, 12, 3, 2)
        };
        let path =
            astra_services::session_journal::journal_file_path_for_owner(&local, &session).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A file in an attached partition still cannot claim another session.
        std::fs::write(
            path,
            serde_json::to_vec(&turn(&other_session, "wrong session")).unwrap(),
        )
        .unwrap();
        assert!(crate::cli::journal_digest::read_attached_journal_sources(&session).is_err());
        let conversation = astra_turn_core::active_conversation::ActiveConversation::empty(
            unrelated.id(),
            &session,
        )
        .unwrap();
        let prepared = conversation
            .prepare_commit(
                1,
                None,
                vec![
                    json!({"role":"user", "content":"private"}),
                    json!({"role":"assistant", "content":"done"}),
                ],
            )
            .unwrap();
        let foreign_cursor = turn(&session, "private").with_conversation_commit(prepared.commit);
        let local_path =
            astra_services::session_journal::journal_file_path_for_owner(&local, &session).unwrap();
        std::fs::remove_file(&local_path).unwrap();
        let account_path =
            astra_services::session_journal::journal_file_path_for_owner(&account, &session)
                .unwrap();
        std::fs::create_dir_all(account_path.parent().unwrap()).unwrap();
        std::fs::write(account_path, serde_json::to_vec(&foreign_cursor).unwrap()).unwrap();
        assert!(
            crate::cli::journal_digest::read_attached_journal_sources(&session)
                .err()
                .unwrap()
                .contains("not attached")
        );
    }

    #[test]
    fn truncate_short() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long() {
        let s = "a".repeat(500);
        let t = truncate(&s, 100);
        assert!(t.contains("truncated"));
        assert!(t.contains("500 total"));
    }

    #[test]
    fn load_journal_empty_file() {
        let path = PathBuf::from("/tmp/nonexistent-debug-test.jsonl");
        assert!(load_journal_turns(&path).is_empty());
    }

    #[test]
    fn list_heavy_checkpoints_missing_dir() {
        let dir = PathBuf::from("/tmp/nonexistent-debug-session");
        assert!(list_heavy_checkpoints(&dir).is_empty());
    }

    // ── Bug fix: truncate must not panic on multi-byte chars ─────────────

    #[test]
    fn truncate_multibyte_no_panic() {
        // The `…` char is 3 bytes. Cutting at byte 80 inside it caused a panic.
        let s = format!("{}…rest", "x".repeat(79));
        let t = truncate(&s, 80);
        assert!(t.contains("truncated"));
        // Must not panic — that's the test.
    }

    // ── Bug fix: tool name from tool_call_id ─────────────────────────────

    #[test]
    fn tool_name_from_tool_call_id() {
        let msg = serde_json::json!({
            "role": "tool",
            "tool_call_id": "git:0",
            "content": "## main"
        });
        // Simulate the extraction logic from show_tools
        let name = msg
            .get("name")
            .and_then(|v| v.as_str())
            .or_else(|| {
                msg.get("tool_call_id")
                    .and_then(|v| v.as_str())
                    .and_then(|id| id.split(':').next())
            })
            .unwrap_or("?");
        assert_eq!(name, "git");
    }

    #[test]
    fn tool_name_falls_back_to_name_field() {
        let msg = serde_json::json!({
            "role": "tool",
            "name": "bash",
            "content": "ok"
        });
        let name = msg
            .get("name")
            .and_then(|v| v.as_str())
            .or_else(|| {
                msg.get("tool_call_id")
                    .and_then(|v| v.as_str())
                    .and_then(|id| id.split(':').next())
            })
            .unwrap_or("?");
        assert_eq!(name, "bash");
    }

    // ── Bug fix: short session ID resolution ─────────────────────────────

    #[serial_test::serial]
    #[test]
    fn resolve_session_id_no_match_returns_input() {
        // No sessions dir match → returns original input
        let result = resolve_session_id("zzz-nonexistent-prefix").unwrap();
        assert_eq!(result, "zzz-nonexistent-prefix");
    }

    #[serial_test::serial]
    #[test]
    fn resolve_session_id_exact_uuid_passthrough() {
        // Full UUID that doesn't exist → returns as-is (no crash)
        let fake = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        assert_eq!(resolve_session_id(fake).unwrap(), fake);
    }

    // ── Bug fix: load_journal_turns parses real entries ───────────────────

    #[test]
    fn load_journal_turns_parses_turn_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        std::fs::write(&path, concat!(
            r#"{"type":"session_start","ts":"2026-01-01T00:00:00Z","session_id":"s1"}"#, "\n",
            r#"{"type":"turn","ts":"2026-01-01T00:01:00Z","session_id":"s1","turn":1,"user_input":"hello","assistant_output":"hi","tool_count":2,"tokens_in":100,"tokens_out":50,"duration_ms":5000,"visible_tools":[],"tools_used":["bash","grep"],"budget_used":0,"budget_pressure":0.0,"ttft_ms":1000,"context_ms":200,"memoria_ms":5}"#, "\n",
        )).unwrap();
        let turns = load_journal_turns(&path);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].user_input, "hello");
        assert_eq!(turns[0].tokens_in, 100);
        assert_eq!(turns[0].tokens_out, 50);
        assert_eq!(turns[0].duration_ms, 5000);
        assert_eq!(turns[0].ttft_ms, 1000);
        assert_eq!(turns[0].tools_used, vec!["bash", "grep"]);
        assert_eq!(turns[0].journal_turn, Some(1));
    }

    #[test]
    fn load_journal_turns_skips_non_turn_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"session_start","ts":"2026-01-01T00:00:00Z","session_id":"s1"}"#,
                "\n",
                r#"{"type":"checkpoint","ts":"2026-01-01T00:01:00Z","session_id":"s1","turn":1}"#,
                "\n",
            ),
        )
        .unwrap();
        let turns = load_journal_turns(&path);
        assert!(turns.is_empty());
    }

    #[test]
    fn load_journal_turns_handles_missing_tool_calls() {
        // Turn with tool_count=0 and no tool_calls field — must not fail
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        std::fs::write(&path, concat!(
            r#"{"type":"turn","ts":"2026-01-01T00:01:00Z","session_id":"s1","turn":1,"user_input":"hi","assistant_output":"hello","tool_count":0,"tokens_in":50,"tokens_out":10,"duration_ms":1000,"visible_tools":[],"tools_used":[],"budget_used":0,"budget_pressure":0.0,"ttft_ms":500}"#, "\n",
        )).unwrap();
        let turns = load_journal_turns(&path);
        assert_eq!(turns.len(), 1);
        assert!(turns[0].tool_calls.is_empty());
        assert_eq!(turns[0].journal_turn, Some(1));
    }

    #[test]
    fn load_journal_turns_attaches_llm_rounds_and_interruptions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        std::fs::write(&path, concat!(
            r#"{"type":"llm_round","ts":"2026-01-01T00:00:00Z","session_id":"s1","turn":2,"agentic_step":4,"round":1,"tool_calls_returned":2,"metadata":{"source":"server_loop","run_id":"run-7","finish_reason":"tool_calls"}}"#, "\n",
            r#"{"type":"interruption_recorded","ts":"2026-01-01T00:00:01Z","session_id":"s1","turn":2,"agentic_step":4,"metadata":{"interruption":{"kind":"budget_exhausted","resumable":true,"tool_calls_completed":3,"turns_completed":4,"remaining_turns":0}}}"#, "\n",
            r#"{"type":"turn","ts":"2026-01-01T00:01:00Z","session_id":"s1","turn":2,"user_input":"continue","assistant_output":"done","tool_count":2,"tokens_in":100,"tokens_out":50,"duration_ms":5000,"visible_tools":[],"tools_used":["bash","grep"],"budget_used":0,"budget_pressure":0.0,"ttft_ms":1000}"#, "\n",
        )).unwrap();
        let turns = load_journal_turns(&path);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].llm_rounds.len(), 1);
        assert_eq!(turns[0].llm_rounds[0].agentic_step, Some(4));
        assert_eq!(
            turns[0].llm_rounds[0].source.as_deref(),
            Some("server_loop")
        );
        assert_eq!(turns[0].interruptions.len(), 1);
        assert_eq!(turns[0].interruptions[0].kind, "budget_exhausted");
        assert_eq!(turns[0].interruptions[0].tool_calls_completed, 3);
    }

    // ── Heavy checkpoint loading & deltas ───────────────────────────────

    fn write_debug_checkpoint(
        owner: &astra_services::OwnerScope,
        session: &str,
        number: u32,
        messages: &[serde_json::Value],
    ) -> PathBuf {
        let mut recorder =
            astra_pipeline::step_recorder::StepRecorder::new(owner.id(), session, "debug-task");
        recorder.begin_turn(number);
        let heavy = recorder
            .build_heavy_checkpoint(messages, 100, 10, &[], &[])
            .unwrap();
        astra_pipeline::step_checkpoint::write_step_checkpoint(
            owner.id(),
            session,
            number,
            &astra_pipeline::step_protocol::StepCheckpoint::Heavy(Box::new(heavy)),
        )
        .unwrap()
    }

    #[test]
    #[serial_test::serial]
    fn heavy_messages_require_current_owner_and_session_envelope() {
        let (_directory, _sessions) = crate::tests::isolated_sessions_dir();
        let owner = astra_services::OwnerScope::user("debug-owner").unwrap();
        let path = write_debug_checkpoint(
            &owner,
            "debug-session",
            1,
            &[json!({"role":"user","content":"test"})],
        );
        assert_eq!(
            load_messages_from_heavy_path(&owner, "debug-session", &path).unwrap(),
            vec![json!({"role":"user","content":"test"})]
        );
        let original: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for field in [
            "user_id",
            "session_id",
            "layout_version",
            "artifact_kind",
            "schema_version",
        ] {
            let mut invalid = original.clone();
            invalid[field] = if field == "schema_version" {
                json!(0)
            } else {
                json!("foreign")
            };
            std::fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
            assert!(load_messages_from_heavy_path(&owner, "debug-session", &path).is_none());
        }
        std::fs::write(&path, "not json").unwrap();
        assert!(load_messages_from_heavy_path(&owner, "debug-session", &path).is_none());
    }

    #[test]
    fn message_delta_strips_shared_prefix() {
        let a = vec![
            json!({"role":"user","content":"a"}),
            json!({"role":"assistant","content":"b"}),
        ];
        let b = vec![
            json!({"role":"user","content":"a"}),
            json!({"role":"assistant","content":"b"}),
            json!({"role":"user","content":"c"}),
        ];
        let d = message_delta(&a, &b);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0]["content"], "c");
    }

    #[test]
    fn list_heavy_checkpoints_sorts_by_numeric_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let cp = dir.path().join("step_checkpoints");
        std::fs::create_dir_all(&cp).unwrap();
        std::fs::write(cp.join("000010-heavy.json"), "{}").unwrap();
        std::fs::write(cp.join("000002-heavy.json"), "{}").unwrap();
        let listed = list_heavy_checkpoints(dir.path());
        assert_eq!(
            listed
                .iter()
                .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
                .collect::<Vec<_>>(),
            vec!["000002-heavy.json", "000010-heavy.json"]
        );
    }

    #[test]
    #[serial_test::serial]
    fn build_turn_messages_view_second_turn_is_delta_only() {
        let (_directory, _sessions) = crate::tests::isolated_sessions_dir();
        let owner = astra_services::OwnerScope::user("debug-owner").unwrap();
        let m0 = vec![json!({"role":"user","content":"hi"})];
        let m1 = vec![m0[0].clone(), json!({"role":"assistant","content":"yo"})];
        let p0 = write_debug_checkpoint(&owner, "debug-session", 1, &m0);
        let p1 = write_debug_checkpoint(&owner, "debug-session", 2, &m1);
        let cps = vec![p0.clone(), p1];
        let v = build_turn_messages_view(&owner, "debug-session", 2, &cps).expect("view");
        assert_eq!(v.delta, vec![m1[1].clone()]);
        assert_eq!(v.full, m1);
        assert_eq!(v.warning, None);
        std::fs::write(p0, "not json").unwrap();
        assert!(build_turn_messages_view(&owner, "debug-session", 2, &cps).is_none());
    }

    #[test]
    fn checkpoint_fallback_preview_excludes_runtime_user_frame() {
        let human = json!({"role": "user", "content": "real request"});
        let mut runtime = json!({"role": "user", "content": "runtime control"});
        astra_turn_types::mark_append_only_required_context(
            &mut runtime,
            "final_answer_settlement",
            astra_turn_types::RuntimeAuthorityLifetime::NextAssistantDecision,
        );

        assert_eq!(human_user_text(&human), Some("real request"));
        assert_eq!(human_user_text(&runtime), None);
    }

    // ── format_tool_display_from_preview ──────────────────────────────

    #[test]
    fn tool_display_skill() {
        use crate::cli::stream::stream_render::format_tool_display_from_preview;
        assert_eq!(
            format_tool_display_from_preview("skill", Some("code-review")),
            "Running skill: code-review"
        );
    }

    #[test]
    fn tool_display_mcp() {
        use crate::cli::stream::stream_render::format_tool_display_from_preview;
        assert_eq!(
            format_tool_display_from_preview("mcp_github_get_pr", None),
            "MCP github "
        );
    }

    #[test]
    fn tool_display_bash() {
        use crate::cli::stream::stream_render::format_tool_display_from_preview;
        assert_eq!(
            format_tool_display_from_preview("bash", Some("cargo test")),
            "$ cargo test"
        );
    }

    #[test]
    fn tool_display_read_file() {
        use crate::cli::stream::stream_render::format_tool_display_from_preview;
        assert_eq!(
            format_tool_display_from_preview("read_file", Some("src/main.rs")),
            "Reading: src/main.rs"
        );
    }
}
