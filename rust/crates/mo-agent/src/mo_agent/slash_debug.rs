use super::*;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

/// Interactive debug inspector for session turns.
///
/// Data sources (in priority order):
/// 1. Heavy checkpoints: `~/.mo-agent/sessions/<id>/step_checkpoints/*-heavy.json`
///    → full messages array (the actual LLM input/output)
/// 2. Journal JSONL: `~/.mo-agent/sessions/<id>.jsonl`
///    → turn summaries, tool calls, timing, token counts
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

    // ── Overview ──
    print_overview(&session_id, &turns, &checkpoints);

    // If journal has no turns but checkpoints exist, offer checkpoint-only inspection.
    if turns.is_empty() {
        if checkpoints.is_empty() {
            eprintln!("\n  {}", "No turn data yet. Complete a conversation turn first.".dim());
            return;
        }
        eprintln!("\n  {}", "No journal turns (journal may not have been initialized).".dim());
        eprintln!("  {} checkpoints available — inspecting latest.", checkpoints.len().to_string().green());
        if let Some(messages) = load_checkpoint_messages(&checkpoints) {
            let stub = TurnSummary {
                user_input: messages.first()
                    .filter(|m| m.get("role").and_then(|v| v.as_str()) == Some("user"))
                    .and_then(|m| m.get("content").and_then(|v| v.as_str()))
                    .unwrap_or("(unknown)")
                    .to_string(),
                tokens_in: 0, tokens_out: 0, duration_ms: 0, ttft_ms: 0,
                tool_count: messages.iter().filter(|m| m.get("role").and_then(|v| v.as_str()) == Some("tool")).count(),
                tools_used: Vec::new(), tool_calls: Vec::new(), selector_strategy: None,
            };
            inspect_turn(1, &stub, Some(&messages));
        } else {
            eprintln!("  {}", "Failed to load checkpoint data.".yellow());
        }
        return;
    }

    // ── Interactive loop ──
    loop {
        eprint!("\n  Which turn? [1-{}, q to quit]: ", turns.len().max(1));
        io::stderr().flush().ok();
        let Some(line) = read_line() else { break };
        let line = line.trim().to_lowercase();
        if line == "q" || line == "quit" {
            break;
        }
        let Ok(turn_n) = line.parse::<usize>() else {
            eprintln!("  {}", "Invalid input".yellow());
            continue;
        };
        if turn_n == 0 || turn_n > turns.len() {
            eprintln!("  {}", format!("Turn {turn_n} not found").yellow());
            continue;
        }

        // Find the heavy checkpoint that covers this turn.
        let messages = load_checkpoint_messages(&checkpoints);

        inspect_turn(turn_n, &turns[turn_n - 1], messages.as_deref());
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

// ── Turn inspector ───────────────────────────────────────────────────────────

fn inspect_turn(turn_n: usize, summary: &TurnSummary, messages: Option<&[serde_json::Value]>) {
    eprintln!(
        "\n  {} — {} tool calls, {:.1}s, {}→{}tok",
        format!("Turn {turn_n}").bold(),
        summary.tool_count,
        summary.duration_ms as f64 / 1000.0,
        summary.tokens_in,
        summary.tokens_out,
    );

    let has_msgs = messages.is_some();
    eprintln!(
        "  {} input    — LLM input messages{}",
        "[1]".cyan(),
        if has_msgs { "" } else { " (no checkpoint)" }
    );
    eprintln!("  {} output   — LLM response", "[2]".cyan());
    eprintln!("  {} tools    — tool calls + results", "[3]".cyan());
    eprintln!("  {} injected — runtime-injected messages", "[4]".cyan());
    eprintln!("  {} json     — dump raw checkpoint to /tmp", "[5]".cyan());
    eprintln!("  {} summary  — journal turn summary", "[6]".cyan());

    loop {
        eprint!("  What to inspect? [1-6, b to go back]: ");
        io::stderr().flush().ok();
        let Some(line) = read_line() else { return };
        let line = line.trim().to_lowercase();
        if line == "b" || line == "back" {
            return;
        }
        match line.as_str() {
            "1" => show_input(messages),
            "2" => show_output(messages),
            "3" => show_tools(messages, summary),
            "4" => show_injected(messages),
            "5" => dump_json(messages, turn_n),
            "6" => show_summary(summary),
            _ => eprintln!("  {}", "Invalid choice".yellow()),
        }
    }
}

fn show_input(messages: Option<&[serde_json::Value]>) {
    let Some(msgs) = messages else {
        eprintln!("  {}", "No checkpoint data available".yellow());
        return;
    };
    eprintln!("\n  {}", "── LLM Input ──".bold());
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

fn show_output(messages: Option<&[serde_json::Value]>) {
    let Some(msgs) = messages else {
        eprintln!("  {}", "No checkpoint data available".yellow());
        return;
    };
    eprintln!("\n  {}", "── LLM Output ──".bold());
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

fn show_tools(messages: Option<&[serde_json::Value]>, summary: &TurnSummary) {
    eprintln!("\n  {}", "── Tool Calls ──".bold());
    // From journal (always available)
    for tc in &summary.tool_calls {
        let status = if tc.ok { "✓".green() } else { "✗".red() };
        let preview = tc.args_preview.as_deref().unwrap_or("");
        eprintln!(
            "  {status} {} {}  {}",
            tc.name.as_str().cyan(),
            format!("({}B→{}B)", tc.input_bytes, tc.output_bytes).dim(),
            truncate(preview, 80).dim(),
        );
    }
    // From checkpoint messages (full content)
    if let Some(msgs) = messages {
        eprintln!("\n  {}", "── Tool Results (from checkpoint) ──".dim());
        for m in msgs {
            let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("?");
            if role != "tool" {
                continue;
            }
            let name = m.get("name")
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

fn show_injected(messages: Option<&[serde_json::Value]>) {
    let Some(msgs) = messages else {
        eprintln!("  {}", "No checkpoint data available".yellow());
        return;
    };
    eprintln!("\n  {}", "── Injected Messages ──".bold());
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

fn dump_json(messages: Option<&[serde_json::Value]>, turn_n: usize) {
    let Some(msgs) = messages else {
        eprintln!("  {}", "No checkpoint data available".yellow());
        return;
    };
    let path = format!("/tmp/debug-turn{turn_n}.json");
    match std::fs::write(
        &path,
        serde_json::to_string_pretty(&msgs).unwrap_or_default(),
    ) {
        Ok(_) => eprintln!("  {} {}", "✓".green(), format!("Written to {path}").dim()),
        Err(e) => eprintln!("  {} {}", "✗".red(), e),
    }
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
        .join(".mo-agent")
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
        .join(".mo-agent")
        .join("sessions")
        .join(session_id)
}

fn session_journal_path(session_id: &str) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".mo-agent")
        .join("sessions")
        .join(format!("{session_id}.jsonl"))
}

#[derive(Debug)]
struct TurnSummary {
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
    paths.sort();
    paths
}

fn load_checkpoint_messages(checkpoints: &[PathBuf]) -> Option<Vec<serde_json::Value>> {
    // Use the last heavy checkpoint (it has the most complete message history).
    let path = checkpoints.last()?;
    let content = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    v.get("Heavy")
        .and_then(|h| h.get("messages"))
        .and_then(|m| m.as_array())
        .cloned()
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
}
