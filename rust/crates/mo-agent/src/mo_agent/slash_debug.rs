use super::*;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

/// Interactive debug inspector for session turns.
///
/// Data sources (in priority order):
/// 1. Heavy checkpoints: `~/.astra/sessions/<id>/step_checkpoints/*-heavy.json`
///    → full messages array (the actual LLM input/output)
/// 2. Journal JSONL: `~/.astra/sessions/<id>.jsonl`
///    → turn summaries, tool calls, timing, token counts
///
/// **Per-turn view:** journal turn *T* is paired with the *T*-th heavy checkpoint file (sorted by
/// numeric prefix). UI and JSON dumps show **message delta** (suffix after shared prefix with the
/// previous heavy snapshot), not the entire accumulated history. If there are fewer heavy files
/// than journal turns, the latest heavy file is used and a warning is recorded.
pub(super) fn handle_debug_command(arg: &str, state: &ReplState) {
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
        resolve_session_id(arg.trim())
    };

    let base = session_dir(&session_id);
    let journal_path = session_journal_path(&session_id);

    // Load data sources.
    let turns = load_journal_turns(&journal_path);
    let checkpoints = list_heavy_checkpoints(&base);

    if turns.is_empty() && checkpoints.is_empty() {
        eprintln!(
            "{}",
            format!("  No data found for session {session_id}").yellow()
        );
        return;
    }

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
    print_overview(&session_id, &turns, &checkpoints);

    // If journal has no turns but checkpoints exist, offer checkpoint-only inspection.
    if turns.is_empty() {
        if checkpoints.is_empty() {
            eprintln!(
                "\n  {}",
                "No turn data yet. Complete a conversation turn first.".dim()
            );
            return;
        }
        eprintln!(
            "\n  {}",
            "No journal turns (journal may not have been initialized).".dim()
        );
        eprintln!(
            "  {} checkpoints available — inspecting latest segment.",
            checkpoints.len().to_string().green()
        );
        if let Some(view) = build_turn_messages_view(checkpoints.len(), &checkpoints) {
            let stub = TurnSummary {
                journal_turn: None,
                user_input: view
                    .delta
                    .iter()
                    .chain(view.full.iter())
                    .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("user"))
                    .and_then(|m| m.get("content").and_then(|v| v.as_str()))
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
                selector_strategy: None,
            };
            inspect_turn(1, &stub, Some(&view), &session_id);
        } else {
            eprintln!("  {}", "Failed to load checkpoint data.".yellow());
        }
        return;
    }

    // ── Interactive loop ──
    loop {
        eprint!(
            "\n  Which turn? [1-{}, bp, ct, cs, q to quit]: ",
            turns.len().max(1)
        );
        io::stderr().flush().ok();
        let Some(line) = read_line() else { break };
        let line = line.trim().to_lowercase();
        match line.as_str() {
            "q" | "quit" => break,
            "bp" | "breakpoints" => {
                show_breakpoints(&session_id);
                continue;
            }
            "cs" | "snapshots" => {
                show_composite_snapshots(&session_id);
                continue;
            }
            "ct" | "corrections" => {
                show_correction_timeline(&session_id);
                continue;
            }
            _ => {}
        }
        let Ok(turn_n) = line.parse::<usize>() else {
            eprintln!(
                "  {}",
                "Invalid input — enter a turn number, bp, ct, cs, or q".yellow()
            );
            continue;
        };
        if turn_n == 0 || turn_n > turns.len() {
            eprintln!("  {}", format!("Turn {turn_n} not found").yellow());
            continue;
        }

        let view = build_turn_messages_view(turn_n, &checkpoints);
        if view.is_none() {
            eprintln!(
                "  {}",
                "No heavy checkpoint messages for this turn.".yellow()
            );
            continue;
        }
        inspect_turn(turn_n, &turns[turn_n - 1], view.as_ref(), &session_id);
    }
}

// ── Overview ──────────────────────────────────────────────────────────────────

fn print_overview(session_id: &str, turns: &[TurnSummary], checkpoints: &[PathBuf]) {
    let short_id = &session_id[..8.min(session_id.len())];
    eprintln!(
        "\n  🔍 session {} ({} turns, {} checkpoints)\n",
        short_id.cyan(),
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

fn load_messages_from_heavy_path(path: &Path) -> Option<Vec<serde_json::Value>> {
    let content = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    v.get("Heavy")
        .and_then(|h| h.get("messages"))
        .and_then(|m| m.as_array())
        .cloned()
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
    after[i..].to_vec()
}

fn build_turn_messages_view(turn_n: usize, checkpoints: &[PathBuf]) -> Option<TurnMessagesView> {
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
    let full = load_messages_from_heavy_path(&after_path)?;
    let (before_path, before_msgs) = if after_idx > 0 {
        let bp = checkpoints[after_idx - 1].clone();
        let bm = load_messages_from_heavy_path(&bp).unwrap_or_default();
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
) {
    let journal_tag = summary
        .journal_turn
        .map(|t| format!(" (journal #{t})"))
        .unwrap_or_default();
    eprintln!(
        "\n  {}{} — {} tool calls, {:.1}s, {}→{}tok",
        format!("Turn {turn_n}").bold(),
        journal_tag,
        summary.tool_count,
        summary.duration_ms as f64 / 1000.0,
        summary.tokens_in,
        summary.tokens_out,
    );

    if let Some(w) = view.and_then(|v| v.warning.as_deref()) {
        eprintln!("  {}", w.yellow());
    }

    let has_msgs = view.is_some();
    eprintln!(
        "  {} input    — LLM input ({} only){}",
        "[1]".cyan(),
        "delta".green(),
        if has_msgs { "" } else { " (no checkpoint)" }
    );
    eprintln!(
        "  {} output   — LLM response ({})",
        "[2]".cyan(),
        "delta".green()
    );
    eprintln!(
        "  {} tools    — tool calls + results ({})",
        "[3]".cyan(),
        "delta".green()
    );
    eprintln!(
        "  {} injected — runtime-injected ({})",
        "[4]".cyan(),
        "delta".green()
    );
    eprintln!(
        "  {} json     — structured delta dump (pretty) → /tmp",
        "[5]".cyan()
    );
    eprintln!(
        "  {} full json — entire snapshot after this segment → /tmp",
        "[7]".cyan()
    );
    eprintln!("  {} summary  — journal turn summary", "[6]".cyan());
    eprintln!("  {} fork     — fork session from this turn", "[f]".cyan());

    loop {
        eprint!("  What to inspect? [1-6, 7, f, b to go back]: ");
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
            "5" => dump_turn_json(view, summary, session_id, turn_n, false),
            "6" => show_summary(summary),
            "7" => dump_turn_json(view, summary, session_id, turn_n, true),
            "f" | "fork" => fork_from_turn(session_id, turn_n),
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
    eprintln!("\n  {}", "── LLM Input (delta) ──".bold());
    for m in msgs {
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("?");
        if role == "assistant" || role == "tool" {
            continue;
        }
        let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let tag = format!("[{role}]").cyan();
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
    eprintln!("\n  {}", "── LLM Output (delta) ──".bold());
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
    eprintln!("\n  {}", "── Tool Calls ──".bold());
    // From journal (always available)
    for tc in &summary.tool_calls {
        let status = if tc.ok {
            theme::icon_ok()
        } else {
            theme::icon_err()
        };
        let preview = tc.args_preview.as_deref().unwrap_or("");
        eprintln!(
            "  {status} {} {}  {}",
            tc.name.as_str().cyan(),
            format!("({}B→{}B)", tc.input_bytes, tc.output_bytes).dim(),
            truncate(preview, 80).dim(),
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
                    // Fall back to tool_call_id (e.g. "git_status:0" → "git_status")
                    m.get("tool_call_id")
                        .and_then(|v| v.as_str())
                        .and_then(|id| id.split(':').next())
                })
                .unwrap_or("?");
            let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
            eprintln!(
                "  {} {}",
                format!("[{name}]").cyan(),
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
    eprintln!("\n  {}", "── Injected Messages (delta) ──".bold());
    let mut found = false;
    for m in msgs {
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("?");
        let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
        // Detect runtime-injected messages by known markers.
        let is_injected = content.contains("VERIFICATION REQUIRED")
            || content.contains("strategy change")
            || content.contains("FACTUAL RETRY")
            || content.contains("⚠️");
        if is_injected {
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
    turn_n: usize,
    full_snapshot: bool,
) {
    let Some(v) = view else {
        eprintln!("  {}", "No checkpoint data available".yellow());
        return;
    };
    let short = &session_id[..8.min(session_id.len())];
    let suffix = if full_snapshot { "-full" } else { "" };
    let path = std::env::temp_dir().join(format!("debug-{short}-turn{turn_n}{suffix}.json"));

    let payload = if full_snapshot {
        serde_json::json!({
            "schema": "astra-debug-turn-full-v1",
            "session_id": session_id,
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
                "selector_strategy": summary.selector_strategy,
            },
            "messages_delta": v.delta,
        })
    };

    match serde_json::to_string_pretty(&payload) {
        Ok(s) => match std::fs::write(&path, s) {
            Ok(()) => eprintln!(
                "  {} {}",
                theme::icon_ok(),
                format!("Written to {}", path.display()).dim()
            ),
            Err(e) => eprintln!("  {} {}", theme::icon_err(), e),
        },
        Err(e) => eprintln!("  {} {}", theme::icon_err(), e),
    }
}

fn file_name_str(p: &Path) -> Option<String> {
    p.file_name().map(|n| n.to_string_lossy().into_owned())
}

fn show_summary(summary: &TurnSummary) {
    eprintln!("\n  {}", "── Journal Summary ──".bold());
    eprintln!("  user:     {}", truncate(&summary.user_input, 120));
    eprintln!("  tokens:   {}→{}", summary.tokens_in, summary.tokens_out);
    eprintln!("  duration: {:.1}s", summary.duration_ms as f64 / 1000.0);
    eprintln!("  ttft:     {}ms", summary.ttft_ms);
    eprintln!(
        "  tools:    {} calls ({})",
        summary.tool_count,
        summary.tools_used.join(", ")
    );
    if let Some(ref sel) = summary.selector_strategy {
        eprintln!("  selector: {}", sel);
    }
    eprintln!();
}

// ── Data loading ─────────────────────────────────────────────────────────────

/// Resolve a (possibly short) session ID to a full UUID by prefix match.
fn resolve_session_id(input: &str) -> String {
    let sessions_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".astra")
        .join("sessions");
    if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
        let matches: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                // Match directories (session data) by prefix
                if e.path().is_dir() && name.starts_with(input) {
                    Some(name)
                } else {
                    None
                }
            })
            .collect();
        if matches.len() == 1 {
            return matches.into_iter().next().unwrap();
        }
    }
    input.to_string()
}

fn session_dir(session_id: &str) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".astra")
        .join("sessions")
        .join(session_id)
}

fn session_journal_path(session_id: &str) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".astra")
        .join("sessions")
        .join(format!("{session_id}.jsonl"))
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
    selector_strategy: Option<String>,
}

#[derive(Debug)]
struct ToolCallSummary {
    name: String,
    ok: bool,
    input_bytes: u64,
    output_bytes: u64,
    args_preview: Option<String>,
}

fn load_journal_turns(path: &PathBuf) -> Vec<TurnSummary> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            if v.get("type")?.as_str()? != "turn" {
                return None;
            }
            let tool_calls = v
                .get("tool_calls")
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|tc| {
                            Some(ToolCallSummary {
                                name: tc.get("name")?.as_str()?.to_string(),
                                ok: tc.get("ok")?.as_bool()?,
                                input_bytes: tc
                                    .get("input_bytes")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0),
                                output_bytes: tc
                                    .get("output_bytes")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0),
                                args_preview: tc
                                    .get("args_preview")
                                    .and_then(|v| v.as_str())
                                    .map(String::from),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(TurnSummary {
                journal_turn: v.get("turn").and_then(|t| t.as_u64()).map(|u| u as u32),
                user_input: v
                    .get("user_input")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                tokens_in: v.get("tokens_in").and_then(|v| v.as_u64()).unwrap_or(0),
                tokens_out: v.get("tokens_out").and_then(|v| v.as_u64()).unwrap_or(0),
                duration_ms: v.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0),
                ttft_ms: v.get("ttft_ms").and_then(|v| v.as_u64()).unwrap_or(0),
                tool_count: v.get("tool_count").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
                tools_used: v
                    .get("tools_used")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
                tool_calls,
                selector_strategy: v
                    .get("selector_strategy")
                    .and_then(|v| v.as_str())
                    .map(String::from),
            })
        })
        .collect()
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

fn show_breakpoints(session_id: &str) {
    match astra_runtime::pipeline::step_checkpoint::read_breakpoint_index(session_id) {
        Ok(index) => {
            if index.breakpoints.is_empty() {
                eprintln!("  {}", "(no breakpoints)".dim());
                return;
            }
            eprintln!("\n  {}", "── Breakpoints ──".bold());
            for bp in &index.breakpoints {
                let short_id = &bp.breakpoint_id[..8.min(bp.breakpoint_id.len())];
                eprintln!(
                    "  {} turn {} — {} ({})",
                    short_id.cyan(),
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

fn show_composite_snapshots(session_id: &str) {
    let index = astra_runtime::pipeline::step_checkpoint::read_composite_snapshot_index(session_id)
        .unwrap_or_default();

    if index.snapshots.is_empty() {
        eprintln!("  {}", "No composite snapshots found.".dim());
        return;
    }

    eprintln!(
        "\n  {}",
        format!("─── Composite Snapshots ({}) ───", index.snapshots.len()).bold()
    );

    for snap in &index.snapshots {
        let dims = snap.dimensions().join(", ");
        let label = snap.label.as_deref().unwrap_or("-");
        eprintln!(
            "  {} T{:<3} {} [{}]  {}",
            snap.snapshot_id[..8.min(snap.snapshot_id.len())].cyan(),
            snap.turn,
            label,
            dims.green(),
            snap.created_at.as_str().dim(),
        );
    }
    eprintln!();
}

// ── Correction Timeline ─────────────────────────────────────────────────────

fn show_correction_timeline(session_id: &str) {
    let events = match astra_services::session_journal::read_journal(session_id) {
        Ok(evts) => evts,
        Err(e) => {
            eprintln!("  {} {}", theme::icon_err(), e);
            return;
        }
    };

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

    eprintln!("\n  {}", "── Correction Timeline ──".bold());
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
        let force_stop = meta
            .and_then(|m| m.get("force_stop"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

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
        let stop_str = if force_stop {
            format!(" {}", "⛔ FORCE STOP".red())
        } else {
            String::new()
        };

        eprintln!(
            "  T{} {} — {} injection(s){}{}",
            turn.bold(),
            severity_colored,
            injections,
            avoid_str,
            stop_str,
        );
    }
    eprintln!();
}

// ── Fork from turn ──────────────────────────────────────────────────────────

fn fork_from_turn(session_id: &str, turn_n: usize) {
    let opts = astra_services::session_fork::ForkSessionOptions {
        parent_session_id: session_id.to_string(),
        new_session_id: None,
        label: Some(format!("debug-fork-at-turn-{turn_n}")),
        forked_after_turn: Some(turn_n as u32),
        data_branch: None,
        snapshot_spec: None,
    };
    match astra_services::session_fork::fork_local_session(opts) {
        Ok(result) => {
            let short = &result.new_session_id[..8.min(result.new_session_id.len())];
            eprintln!(
                "  {} Forked → {} ({} events copied, label: debug-fork-at-turn-{})",
                theme::icon_ok(),
                short.green(),
                result.events_copied,
                turn_n,
            );
        }
        Err(e) => eprintln!("  {} Fork failed: {}", theme::icon_err(), e),
    }
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
    use super::*;
    use serde_json::json;

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
            "tool_call_id": "git_status:0",
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
        assert_eq!(name, "git_status");
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

    #[test]
    fn resolve_session_id_no_match_returns_input() {
        // No sessions dir match → returns original input
        let result = resolve_session_id("zzz-nonexistent-prefix");
        assert_eq!(result, "zzz-nonexistent-prefix");
    }

    #[test]
    fn resolve_session_id_exact_uuid_passthrough() {
        // Full UUID that doesn't exist → returns as-is (no crash)
        let fake = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        assert_eq!(resolve_session_id(fake), fake);
    }

    // ── Bug fix: load_journal_turns parses real entries ───────────────────

    #[test]
    fn load_journal_turns_parses_turn_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        std::fs::write(&path, concat!(
            r#"{"type":"session_start","ts":"2026-01-01T00:00:00Z","session_id":"s1"}"#, "\n",
            r#"{"type":"turn","ts":"2026-01-01T00:01:00Z","session_id":"s1","turn":1,"user_input":"hello","assistant_output":"hi","tool_count":2,"tokens_in":100,"tokens_out":50,"duration_ms":5000,"tools_selected":[],"tools_used":["bash","grep"],"budget_used":0,"budget_pressure":0.0,"ttft_ms":1000,"context_ms":200,"selector_strategy":"tfidf","selector_ms":10,"memoria_ms":5}"#, "\n",
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
            r#"{"type":"turn","ts":"2026-01-01T00:01:00Z","session_id":"s1","turn":1,"user_input":"hi","assistant_output":"hello","tool_count":0,"tokens_in":50,"tokens_out":10,"duration_ms":1000,"tools_selected":[],"tools_used":[],"budget_used":0,"budget_pressure":0.0,"ttft_ms":500}"#, "\n",
        )).unwrap();
        let turns = load_journal_turns(&path);
        assert_eq!(turns.len(), 1);
        assert!(turns[0].tool_calls.is_empty());
        assert_eq!(turns[0].journal_turn, Some(1));
    }

    // ── Heavy checkpoint loading & deltas ───────────────────────────────

    #[test]
    fn load_messages_from_heavy_path_parses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("000001-heavy.json");
        std::fs::write(
            &path,
            r#"{"Heavy":{"light":{},"messages":[{"role":"user","content":"test"}]}}"#,
        )
        .unwrap();
        let msgs = load_messages_from_heavy_path(&path).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "test");
    }

    #[test]
    fn load_messages_from_heavy_path_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("000001-heavy.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(load_messages_from_heavy_path(&path).is_none());
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
    fn build_turn_messages_view_second_turn_is_delta_only() {
        let dir = tempfile::tempdir().unwrap();
        let m0 = vec![json!({"role":"user","content":"hi"})];
        let m1 = vec![
            json!({"role":"user","content":"hi"}),
            json!({"role":"assistant","content":"yo"}),
        ];
        let cp = dir.path().join("step_checkpoints");
        std::fs::create_dir_all(&cp).unwrap();
        let p0 = cp.join("000001-heavy.json");
        let p1 = cp.join("000002-heavy.json");
        std::fs::write(
            &p0,
            format!(
                r#"{{"Heavy":{{"light":{{}},"messages":{}}}}}"#,
                serde_json::to_string(&m0).unwrap()
            ),
        )
        .unwrap();
        std::fs::write(
            &p1,
            format!(
                r#"{{"Heavy":{{"light":{{}},"messages":{}}}}}"#,
                serde_json::to_string(&m1).unwrap()
            ),
        )
        .unwrap();
        let cps = list_heavy_checkpoints(dir.path());
        assert_eq!(cps.len(), 2);
        let v = build_turn_messages_view(2, &cps).expect("view");
        assert_eq!(v.delta.len(), 1);
        assert_eq!(v.full.len(), 2);
        assert_eq!(v.warning, None);
    }
}
