use std::io::Write;

use astra_core::{DriftCause, EvidenceType};
use astra_runtime::turn::decision_explainer::{DriftDetector, FocusDriftAnalysis};
use astra_services::{ForkSessionOptions, fork_local_session, session_journal, session_workspace};
use chrono::{DateTime, Utc};

use super::*;
use crate::repl_runtime;

/// `/home/foo/bar` → `~/bar` when under the user home dir (readability).
fn tilde_path(abs: &str) -> String {
    let Some(home) = dirs::home_dir() else {
        return abs.to_string();
    };
    let home = home.to_string_lossy();
    let home = home.trim_end_matches('/');
    if abs == home {
        return "~".to_string();
    }
    let prefix = format!("{home}/");
    if let Some(rest) = abs.strip_prefix(&prefix) {
        return format!("~/{rest}");
    }
    abs.to_string()
}

/// Short relative age from RFC3339 `updated_at` (for scan-friendly lists).
fn rel_updated_label(iso: &str) -> Option<String> {
    let dt = DateTime::parse_from_rfc3339(iso).ok()?.with_timezone(&Utc);
    let secs = Utc::now().signed_duration_since(dt).num_seconds();
    let secs = secs.max(0);
    if secs < 60 {
        return Some("just now".to_string());
    }
    if secs < 3600 {
        return Some(format!("{}m ago", secs / 60));
    }
    if secs < 86_400 {
        return Some(format!("{}h ago", secs / 3600));
    }
    if secs < 86_400 * 7 {
        return Some(format!("{}d ago", secs / 86_400));
    }
    Some(format!("{}d ago", secs / 86_400))
}

fn format_u64_grouped(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}

/// One-line hint for session lists: cwd, git, turns (from `workspace.yaml` if present).
fn workspace_summary_line(sid: &str) -> String {
    match session_workspace::read_workspace(sid) {
        Ok(ws) => {
            let mut parts: Vec<String> = Vec::new();
            let cwd = tilde_path(ws.cwd.as_str());
            parts.push(ellipsize(&cwd, 56));
            match (&ws.git_branch, &ws.git_head) {
                (Some(b), Some(h)) => parts.push(format!("{b} @ {h}")),
                (Some(b), None) => parts.push(b.clone()),
                (None, Some(h)) => parts.push(format!("@ {h}")),
                (None, None) => {}
            }
            if ws.turn_count > 0 {
                parts.push(format!("{} turns", ws.turn_count));
            }
            if ws.status != "active" {
                parts.push(ws.status.clone());
            }
            if let Some(lbl) = rel_updated_label(ws.updated_at.as_str()) {
                parts.push(lbl);
            }
            parts.join(" · ")
        }
        Err(_) => "journal only (no workspace.yaml)".to_string(),
    }
}

/// Resolve parent session id and optional label for `/session fork`.
fn parse_fork_source(arg: &str, state: &ReplState) -> Result<(String, Option<String>), String> {
    let arg = arg.trim();
    if arg.is_empty() {
        return state
            .session_id
            .clone()
            .filter(|s| !s.is_empty())
            .map(|s| Ok((s, None)))
            .unwrap_or_else(|| {
                Err(
                    "no active session — use `/session fork <parent_session_id> [label]`"
                        .to_string(),
                )
            });
    }
    let parts: Vec<&str> = arg.split_whitespace().collect();
    let head = parts[0];
    let tail = if parts.len() > 1 {
        Some(parts[1..].join(" "))
    } else {
        None
    };
    match session_journal::resolve_session_id(head) {
        Ok(sid) => Ok((sid, tail)),
        Err(_) => state
            .session_id
            .clone()
            .filter(|s| !s.is_empty())
            .map(|sid| Ok((sid, Some(arg.to_string()))))
            .unwrap_or_else(|| {
                Err(format!(
                    "unknown session id or prefix '{head}' (and no active session to fork from)"
                ))
            }),
    }
}

fn ellipsize(s: &str, max_chars: usize) -> String {
    let t: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        format!("{t}…")
    } else {
        t
    }
}

/// Print persisted workspace context (`workspace.yaml`).
fn print_workspace_metadata(ws: &session_workspace::WorkspaceMetadata, sid: &str) {
    eprintln!("  {}", "— workspace (persisted) —".dim());
    eprintln!(
        "  {:<16} {}",
        "cwd:".dim(),
        tilde_path(ws.cwd.as_str()).as_str().cyan()
    );
    let git_line = match (&ws.git_branch, &ws.git_head) {
        (Some(b), Some(h)) => format!("{b} @ {h}"),
        (Some(b), None) => b.clone(),
        (None, Some(h)) => format!("(detached) @ {h}"),
        (None, None) => "(no git at session start)".to_string(),
    };
    eprintln!("  {:<16} {}", "git:".dim(), git_line.cyan());
    if let Some(ref root) = ws.git_root
        && root != &ws.cwd
    {
        eprintln!(
            "  {:<16} {}",
            "repo root:".dim(),
            tilde_path(root.as_str()).as_str().dim()
        );
    }
    if let Some(ref p) = ws.parent_session_id {
        eprintln!(
            "  {:<16} {}",
            "forked from:".dim(),
            format!("{p} (turn {} on parent)", ws.forked_at_turn.unwrap_or(0)).cyan()
        );
        if let Some(ref n) = ws.fork_note {
            eprintln!("  {:<16} {}", "fork note:".dim(), n.as_str().cyan());
        }
    }
    if let Some(ref c) = ws.correlation_id {
        eprintln!("  {:<16} {}", "correlation:".dim(), c.as_str().cyan());
    }
    if let Some(ref r) = ws.agent_role {
        eprintln!("  {:<16} {}", "agent role:".dim(), r.as_str().cyan());
    }
    let started = ws.created_at.get(..19).unwrap_or(ws.created_at.as_str());
    eprintln!("  {:<16} {}", "started:".dim(), started.cyan());
    let saved = ws.updated_at.get(..19).unwrap_or(ws.updated_at.as_str());
    let ago = rel_updated_label(ws.updated_at.as_str())
        .map(|a| format!(" · {a}"))
        .unwrap_or_default();
    eprintln!(
        "  {:<16} {}{}",
        "last saved:".dim(),
        saved.cyan(),
        ago.dim()
    );
    eprintln!("  {:<16} {}", "status:".dim(), ws.status.as_str().cyan());
    if let Some(ref sum) = ws.summary {
        eprintln!("  {:<16} {}", "summary:".dim(), ellipsize(sum, 80).dim());
    }
    if ws.turn_count > 0 || ws.total_tokens_in > 0 || ws.total_tokens_out > 0 {
        eprintln!(
            "  {:<16} {} turns · {} prompt + {} completion tokens",
            "logged:".dim(),
            ws.turn_count.to_string().cyan(),
            format_u64_grouped(ws.total_tokens_in).as_str().cyan(),
            format_u64_grouped(ws.total_tokens_out).as_str().cyan(),
        );
    }
    if let Some(ref goal) = ws.plan_goal {
        eprintln!(
            "  {:<16} {}",
            "plan goal:".dim(),
            ellipsize(goal, 72).cyan()
        );
    }
    if ws.plan_execution_rounds > 0 {
        eprintln!(
            "  {:<16} {}",
            "plan rounds:".dim(),
            ws.plan_execution_rounds.to_string().cyan()
        );
    }
    if let Some(ref trace) = ws.last_context_trace {
        eprintln!(
            "  {:<16} {}",
            "last trace:".dim(),
            ellipsize(&trace.preview(), 96).dim()
        );
    }
    if !ws.checkpoints.is_empty() {
        let preview: Vec<String> = ws
            .checkpoints
            .iter()
            .take(6)
            .map(|t| format!("T{t}"))
            .collect();
        let joined = preview.join(", ");
        let tail = if ws.checkpoints.len() > 6 {
            format!(" … (+{} more)", ws.checkpoints.len() - 6)
        } else {
            String::new()
        };
        eprintln!(
            "  {:<16} {}{}",
            "checkpoints:".dim(),
            joined.cyan(),
            tail.dim()
        );
    }
    let ws_path = session_workspace::workspace_dir_for(sid).join("workspace.yaml");
    let ws_disp = ws_path.display().to_string();
    eprintln!(
        "  {:<16} {}",
        "workspace.yaml:".dim(),
        tilde_path(&ws_disp).as_str().dim()
    );
    eprintln!();
}

/// Handle `/session list [--all|--active|--completed] [--here|--project] [search_term]`
///
/// Sorting:
/// - Default: most recent first (by mtime)
/// - Shows up to 20 sessions unless `--all` specified
///
/// Filtering:
/// - `--active`: only active sessions
/// - `--completed`: only completed sessions
/// - `--here`: only sessions from current directory
/// - `--project`: only sessions from current git project (any branch)
///
/// Search:
/// - Matches session ID prefix, cwd, git branch, or summary
fn handle_session_list(sub_arg: &str, state: &ReplState) {
    // Parse options
    let mut show_all = false;
    let mut filter_active = false;
    let mut filter_completed = false;
    let mut filter_here = false;
    let mut filter_project = false;
    let mut search_term: Option<String> = None;

    for part in sub_arg.split_whitespace() {
        match part {
            "--all" | "-a" => show_all = true,
            "--active" => filter_active = true,
            "--completed" | "--done" => filter_completed = true,
            "--here" | "--cwd" => filter_here = true,
            "--project" | "--repo" => filter_project = true,
            _ if !part.starts_with('-') => search_term = Some(part.to_lowercase()),
            other => {
                eprintln!("{}", format!("  Unknown option: {other}").red());
                eprintln!(
                    "  {}",
                    "Usage: /session list [--all|--active|--completed|--here|--project] [search]"
                        .dim()
                );
                return;
            }
        }
    }

    // Get current cwd and git root for filtering
    let current_cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let current_git_root = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

    // Load sessions by recency
    let limit = if show_all { 500 } else { 50 }; // scan more than display for filtering
    let sessions = match session_journal::list_sessions_by_time(limit) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}", format!("  ✗ {e}").red());
            return;
        }
    };

    if sessions.is_empty() {
        eprintln!("{}", "  No journal files yet.".dim());
        eprintln!(
            "  {}",
            "Chat once to create a session, or check ~/.astra/sessions.".dim()
        );
        return;
    }

    // Build session list with metadata for filtering/searching
    struct SessionEntry {
        sid: String,
        ws: Option<session_workspace::WorkspaceMetadata>,
        hint: String,
    }

    let mut entries: Vec<SessionEntry> = sessions
        .iter()
        .map(|sid| {
            let ws = session_workspace::read_workspace(sid).ok();
            let hint = workspace_summary_line(sid);
            SessionEntry {
                sid: sid.clone(),
                ws,
                hint,
            }
        })
        .collect();

    // Apply filters
    if filter_active {
        entries.retain(|e| e.ws.as_ref().map(|w| w.status == "active").unwrap_or(false));
    }
    if filter_completed {
        entries.retain(|e| {
            e.ws.as_ref()
                .map(|w| w.status == "completed")
                .unwrap_or(false)
        });
    }
    if filter_here {
        entries.retain(|e| e.ws.as_ref().map(|w| w.cwd == current_cwd).unwrap_or(false));
    }
    if filter_project {
        if let Some(ref root) = current_git_root {
            entries.retain(|e| {
                e.ws.as_ref()
                    .and_then(|w| w.git_root.as_ref())
                    .map(|r| r == root)
                    .unwrap_or(false)
            });
        } else {
            eprintln!(
                "  {}",
                "Not in a git repository — --project filter ignored.".yellow()
            );
        }
    }

    // Apply search
    if let Some(ref term) = search_term {
        entries.retain(|e| {
            // Match session ID prefix
            if e.sid.to_lowercase().starts_with(term) {
                return true;
            }
            // Match cwd, git branch, or summary
            if let Some(ref ws) = e.ws {
                if ws.cwd.to_lowercase().contains(term) {
                    return true;
                }
                if let Some(ref b) = ws.git_branch {
                    if b.to_lowercase().contains(term) {
                        return true;
                    }
                }
                if let Some(ref s) = ws.summary {
                    if s.to_lowercase().contains(term) {
                        return true;
                    }
                }
            }
            false
        });
    }

    // Limit display
    let display_limit = if show_all { entries.len() } else { 20 };
    let total = entries.len();
    let showing = total.min(display_limit);

    if entries.is_empty() {
        let mut filter_desc = Vec::new();
        if filter_active {
            filter_desc.push("active");
        }
        if filter_completed {
            filter_desc.push("completed");
        }
        if filter_here {
            filter_desc.push("this directory");
        }
        if filter_project {
            filter_desc.push("this project");
        }
        let desc = if filter_desc.is_empty() {
            String::new()
        } else {
            format!(" ({})", filter_desc.join(", "))
        };
        eprintln!("  {}", format!("No sessions match{desc}.").dim());
        return;
    }

    // Display header
    eprintln!(
        "\n{}",
        "─── Session Journals ────────────────────────────"
            .bold()
            .cyan()
    );
    let sort_info = "sorted by recent";
    let filter_info = {
        let mut parts = Vec::new();
        if filter_active {
            parts.push("active only");
        }
        if filter_completed {
            parts.push("completed only");
        }
        if filter_here {
            parts.push("this dir");
        }
        if filter_project {
            parts.push("this project");
        }
        if let Some(ref t) = search_term {
            parts.push(t);
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!(" · filter: {}", parts.join(", "))
        }
    };
    eprintln!("  {}", format!("{sort_info}{filter_info}").dim());

    // Display entries with numbered shortcuts
    let current = state.session_id.as_deref().unwrap_or("");
    let mut shortcuts: Vec<String> = Vec::new();
    for (idx, entry) in entries.iter().take(display_limit).enumerate() {
        let marker = if entry.sid == current {
            " ← current"
        } else {
            ""
        };
        // Show abbreviated session ID (first 8 chars) for cleaner display
        let sid_short = if entry.sid.len() > 8 {
            &entry.sid[..8]
        } else {
            &entry.sid
        };
        // Show numbered shortcut for first 9 entries
        let num_label = if idx < 9 {
            shortcuts.push(entry.sid.clone());
            format!("[{}] ", idx + 1)
        } else {
            "    ".to_string()
        };
        eprintln!(
            "  {}{}  {}{}",
            num_label.dim(),
            sid_short.cyan(),
            entry.hint.as_str().dim(),
            marker.green()
        );
    }

    // Store shortcuts for /session switch
    if !shortcuts.is_empty() {
        LAST_SESSION_LIST.with(|cell| {
            *cell.borrow_mut() = shortcuts;
        });
        eprintln!("  {}", "Tip: /session switch <N> to resume by number".dim());
    }

    // Summary
    if total > showing {
        eprintln!(
            "  {} sessions match (showing {}; use --all for more)",
            total.to_string().dim(),
            showing
        );
    } else {
        eprintln!(
            "  {} {}",
            total.to_string().dim(),
            if total == 1 { "session" } else { "sessions" }
        );
    }
    eprintln!();
}

// Thread-local storage for session list shortcuts
thread_local! {
    static LAST_SESSION_LIST: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Get session ID from shortcut number (1-indexed)
fn get_session_shortcut(num: usize) -> Option<String> {
    LAST_SESSION_LIST.with(|cell| cell.borrow().get(num.saturating_sub(1)).cloned())
}

/// Handle `/session switch <N>` - quick switch to session by number from last list
fn handle_session_switch(sub_arg: &str, state: &mut ReplState) {
    let arg = sub_arg.trim();

    if arg.is_empty() {
        eprintln!(
            "  {}",
            "Usage: /session switch <N> (use number from /session list)".dim()
        );
        return;
    }

    // Parse number
    let num: usize = match arg.parse() {
        Ok(n) if (1..=9).contains(&n) => n,
        _ => {
            eprintln!("{}", format!("  Invalid number: {arg} (use 1-9)").red());
            return;
        }
    };

    // Get session ID from shortcuts
    let session_id = match get_session_shortcut(num) {
        Some(sid) => sid,
        None => {
            eprintln!(
                "  {}",
                format!("No session at position {num}. Run /session list first.").yellow()
            );
            return;
        }
    };

    // Show preview and confirm
    let ws = session_workspace::read_workspace(&session_id).ok();

    let short_id = &session_id[..8.min(session_id.len())];
    let summary = ws
        .as_ref()
        .and_then(|w| w.summary.clone())
        .map(|s| {
            let truncated: String = s.chars().take(50).collect();
            if s.chars().count() > 50 {
                format!("{truncated}…")
            } else {
                truncated
            }
        })
        .unwrap_or_else(|| "(no summary)".to_string());

    let turns = ws
        .as_ref()
        .map(|w| w.turn_count)
        .unwrap_or_else(|| session_journal::count_turns(&session_id));

    eprintln!(
        "\n  {} {}  {}  {} turns",
        format!("[{num}]").cyan().bold(),
        short_id.cyan(),
        summary.dim(),
        turns
    );

    // Quick confirm
    eprint!("  {} ", "Switch to this session? [Y/n]:".bold());
    std::io::Write::flush(&mut std::io::stderr()).ok();
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_ok() {
        let answer = input.trim().to_lowercase();
        if !answer.is_empty() && answer != "y" && answer != "yes" {
            eprintln!("{}", "  Cancelled.".dim());
            return;
        }
    } else {
        return;
    }

    // Restore session state
    let st = crate::repl_runtime::session_state_from_journal(&session_id);
    state.session_id = Some(session_id.clone());
    state.journal = session_journal::JournalWriter::new(&session_id).ok();
    state.history = st.history;
    state.turn = st.turn;
    state.total_prompt_tokens = st.total_prompt_tokens;
    state.total_completion_tokens = st.total_completion_tokens;
    state.recent_tools = st.recent_tools;
    state.last_turn_event = None;
    state.run_id = None;

    eprintln!(
        "  {} Switched to session {} ({} turns loaded)",
        theme::icon_ok(),
        short_id.cyan(),
        state.turn
    );
}

pub(super) fn resolve_journal_target_session(
    sub_arg: &str,
    state: &ReplState,
    _missing_active_msg: &str,
) -> Result<(String, bool), String> {
    if !sub_arg.is_empty() {
        let requested = sub_arg.trim();
        let resolved =
            session_journal::resolve_session_id(requested).map_err(|e| format!("  ✗ {e}"))?;
        Ok((resolved.clone(), resolved != requested))
    } else if let Some(ref sid) = state.session_id {
        Ok((sid.clone(), false))
    } else {
        // No active session — list local journals and let user pick
        let sessions = session_journal::list_sessions_by_time(10).unwrap_or_default();
        if sessions.is_empty() {
            return Err("  No sessions found. Start a conversation to create one.".to_string());
        }
        eprintln!(
            "\n{}",
            "─── Available Sessions ──────────────────────────"
                .bold()
                .cyan()
        );
        eprintln!(
            "  {}",
            "newest first · path / git / turns from workspace.yaml".dim()
        );
        let show = sessions.len().min(10);
        for (i, sid) in sessions.iter().take(show).enumerate() {
            let hint = workspace_summary_line(sid);
            eprintln!(
                "  {}  {}  {}",
                format!("[{}]", i + 1).cyan().bold(),
                sid.as_str().cyan(),
                hint.dim()
            );
        }
        eprintln!();
        eprint!("  {} ", "Select (number or Enter to cancel):".bold());
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_ok()
            && let Ok(n) = input.trim().parse::<usize>()
            && n >= 1
            && n <= show
        {
            return Ok((sessions[n - 1].clone(), false));
        }
        Err("  Cancelled.".to_string())
    }
}

pub(super) fn handle_session_command(arg: &str, state: &mut ReplState) {
    let (sub_cmd, sub_arg) = match arg.find(char::is_whitespace) {
        Some(pos) => (arg[..pos].trim(), arg[pos..].trim()),
        None => (arg.trim(), ""),
    };
    match sub_cmd {
        "" => {
            // Show session info + available subcommands
            let sid = state.session_id.as_deref().unwrap_or("none");
            let mdl = state.model.as_deref().unwrap_or("default");
            eprintln!(
                "\n{}",
                "─── Session ─────────────────────────────────────"
                    .bold()
                    .cyan()
            );
            eprintln!("  {:<16} {}", "session_id:".dim(), sid.cyan());
            let persisted_ws = (sid != "none")
                .then(|| session_workspace::read_workspace(sid).ok())
                .flatten();
            if sid != "none" {
                if let Some(ref ws) = persisted_ws {
                    print_workspace_metadata(ws, sid);
                    if ws.model != mdl {
                        eprintln!("  {:<16} {}", "started as:".dim(), ws.model.as_str().dim());
                    }
                } else {
                    eprintln!(
                        "  {}",
                        "— no workspace.yaml yet (cwd/git after journal init) —".dim()
                    );
                    eprintln!();
                }
            } else {
                eprintln!();
            }
            eprintln!("  {}", "— this REPL —".dim());
            eprintln!("  {:<16} {}", "model:".dim(), mdl.cyan());
            if let Some(ref ws) = persisted_ws {
                if ws.turn_count != state.turn {
                    eprintln!(
                        "  {:<16} {} repl · {} logged",
                        "turns:".dim(),
                        state.turn.to_string().cyan(),
                        ws.turn_count.to_string().cyan()
                    );
                } else {
                    eprintln!("  {:<16} {}", "turns:".dim(), state.turn.to_string().cyan());
                }
            } else {
                eprintln!("  {:<16} {}", "turns:".dim(), state.turn.to_string().cyan());
            }
            eprintln!(
                "  {:<16} {}",
                "explain:".dim(),
                state.explain.to_string().cyan()
            );
            eprintln!(
                "  {:<16} {}",
                "run_id:".dim(),
                state.run_id.as_deref().unwrap_or("none").cyan()
            );
            if let Some(ref j) = state.journal {
                let jp = j.path().display().to_string();
                eprintln!(
                    "  {:<16} {}",
                    "journal:".dim(),
                    tilde_path(&jp).as_str().cyan()
                );
            }
            eprintln!();
            eprintln!(
                "  {}",
                "Subcommands: history · context · errors · export · list [--here|--project|--active|search] · fork · cleanup · verify · drift"
                    .dim()
            );
            eprintln!();
        }
        "fork" => {
            let (parent_id, label) = match parse_fork_source(sub_arg, state) {
                Ok(x) => x,
                Err(msg) => {
                    eprintln!("{}", msg.red());
                    return;
                }
            };
            match fork_local_session(ForkSessionOptions {
                parent_session_id: parent_id.clone(),
                new_session_id: None,
                label: label.clone(),
                forked_after_turn: None,
                data_branch: None,
                snapshot_spec: None,
            }) {
                Ok(res) => {
                    let new_sid = res.new_session_id.clone();
                    eprintln!(
                        "  {} New session {} (fork of {})",
                        theme::icon_ok(),
                        new_sid.as_str().cyan(),
                        parent_id.dim()
                    );
                    eprintln!(
                        "  {}",
                        format!(
                            "{} journal events copied (excl. session end/start)",
                            res.events_copied
                        )
                        .dim()
                    );
                    let st = repl_runtime::session_state_from_journal(&new_sid);
                    state.session_id = Some(new_sid.clone());
                    state.journal = session_journal::JournalWriter::new(&new_sid).ok();
                    state.history = st.history;
                    state.turn = st.turn;
                    state.total_prompt_tokens = st.total_prompt_tokens;
                    state.total_completion_tokens = st.total_completion_tokens;
                    state.recent_tools = st.recent_tools;
                    state.last_turn_event = None;
                    state.run_id = None;
                    eprintln!(
                        "  {}",
                        "REPL context is now the forked session (same history; new cloud lineage)."
                            .dim()
                    );
                }
                Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
            }
        }
        "history" => {
            // Read journal for this session or a specified session
            let (target_sid, resolved_prefix) = match resolve_journal_target_session(
                sub_arg,
                state,
                "  No active session. Use /session history <session_id>.",
            ) {
                Ok(value) => value,
                Err(msg) => {
                    eprintln!("{msg}");
                    return;
                }
            };
            if resolved_prefix {
                eprintln!(
                    "  {} Resolved {} → {}",
                    theme::icon_ok(),
                    sub_arg.cyan(),
                    target_sid.as_str().cyan()
                );
            }
            match session_journal::read_journal(&target_sid) {
                Ok(events) if events.is_empty() => {
                    eprintln!(
                        "{}",
                        format!("  No journal entries for session {target_sid}").dim()
                    );
                }
                Ok(events) => {
                    eprintln!(
                        "\n{}",
                        format!(
                            "─── Session Journal ({}) ─────────────────────",
                            &target_sid[..8.min(target_sid.len())]
                        )
                        .bold()
                    );
                    for evt in &events {
                        let ts_short = evt.ts.get(11..19).unwrap_or(&evt.ts);
                        match evt.event_type {
                            session_journal::JournalEventType::SessionStart => {
                                eprintln!(
                                    "  {} {} session started (model: {})",
                                    ts_short.dim(),
                                    "▶".green(),
                                    evt.model.as_deref().unwrap_or("default").cyan(),
                                );
                            }
                            session_journal::JournalEventType::Turn => {
                                let input_preview: String = evt
                                    .user_input
                                    .as_deref()
                                    .unwrap_or("")
                                    .chars()
                                    .take(60)
                                    .collect();
                                eprintln!(
                                    "  {} {} T{} {} ({}ms, {}+{} tokens, {} tools)",
                                    ts_short.dim(),
                                    "●".cyan(),
                                    evt.turn.unwrap_or(0),
                                    input_preview,
                                    evt.duration_ms.unwrap_or(0),
                                    evt.tokens_in.unwrap_or(0),
                                    evt.tokens_out.unwrap_or(0),
                                    evt.tool_count.unwrap_or(0),
                                );
                                // Show any failed tool calls for auditability
                                if let Some(calls) = &evt.tool_calls {
                                    for tc in calls.iter().filter(|c| !c.ok) {
                                        let err_preview = tc
                                            .error
                                            .as_deref()
                                            .unwrap_or("(no details)")
                                            .chars()
                                            .take(80)
                                            .collect::<String>();
                                        eprintln!(
                                            "    {} {} ({}ms) {}",
                                            theme::icon_err(),
                                            super::stream_render::format_tool_display_from_preview(
                                                &tc.name,
                                                tc.args_preview.as_deref(),
                                            ),
                                            tc.ms,
                                            err_preview.dim(),
                                        );
                                    }
                                }
                            }
                            session_journal::JournalEventType::TurnError => {
                                eprintln!(
                                    "  {} {} T{} error: {}",
                                    ts_short.dim(),
                                    theme::icon_err(),
                                    evt.turn.unwrap_or(0),
                                    evt.error.as_deref().unwrap_or("(no details)").red(),
                                );
                            }
                            session_journal::JournalEventType::Compact => {
                                let summary_note = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("compact_summary"))
                                    .and_then(|v| v.as_str())
                                    .map(|s| {
                                        let preview: String = s.chars().take(80).collect();
                                        if s.chars().count() > 80 {
                                            format!(" · {preview}…")
                                        } else {
                                            format!(" · {preview}")
                                        }
                                    })
                                    .unwrap_or_default();
                                eprintln!(
                                    "  {} {} compacted {} turns ({} facts){}",
                                    ts_short.dim(),
                                    "⟳".yellow(),
                                    evt.turns_compacted.unwrap_or(0),
                                    evt.facts_stored.unwrap_or(0),
                                    summary_note.dim(),
                                );
                            }
                            session_journal::JournalEventType::ConfigChange => {
                                eprintln!(
                                    "  {} {} {} → {}",
                                    ts_short.dim(),
                                    "⚙".dim(),
                                    evt.config_key.as_deref().unwrap_or("?"),
                                    evt.config_value.as_deref().unwrap_or("?").cyan(),
                                );
                            }
                            session_journal::JournalEventType::Error => {
                                eprintln!(
                                    "  {} {} {}",
                                    ts_short.dim(),
                                    theme::icon_err(),
                                    evt.error.as_deref().unwrap_or("(no details)").red(),
                                );
                            }
                            session_journal::JournalEventType::SessionEnd => {
                                eprintln!(
                                    "  {} {} session ended ({} turns total)",
                                    ts_short.dim(),
                                    "■".dim(),
                                    evt.turn.unwrap_or(0),
                                );
                            }
                            session_journal::JournalEventType::StallDetected => {
                                eprintln!(
                                    "  {} {} T{} stall: {}",
                                    ts_short.dim(),
                                    theme::icon_warn(),
                                    evt.turn.unwrap_or(0),
                                    evt.stall_type.as_deref().unwrap_or("(no details)").yellow(),
                                );
                            }
                            session_journal::JournalEventType::Checkpoint => {
                                let summary = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("summary"))
                                    .and_then(|s| s.as_str())
                                    .unwrap_or("checkpoint");
                                eprintln!(
                                    "  {} {} T{} checkpoint: {}",
                                    ts_short.dim(),
                                    "📌".green(),
                                    evt.turn.unwrap_or(0),
                                    summary,
                                );
                            }
                            session_journal::JournalEventType::TurnGuardVerdict => {
                                let severity = evt.stall_type.as_deref().unwrap_or("info");
                                let icon = match severity {
                                    "critical" => "🛑",
                                    "warning" => "⚠",
                                    _ => "ℹ",
                                };
                                let details = evt
                                    .metadata
                                    .as_ref()
                                    .map(|m| {
                                        let avoid = m
                                            .get("avoid_tools")
                                            .and_then(|v| v.as_array())
                                            .map(|a| a.len())
                                            .unwrap_or(0);
                                        let inj = m
                                            .get("injections")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0);
                                        let reason = m
                                            .get("avoid_reason_summary")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("");
                                        if reason.is_empty() {
                                            format!("{inj} nudges, {avoid} tools restricted")
                                        } else {
                                            format!(
                                                "{inj} nudges, {avoid} tools restricted ({reason})"
                                            )
                                        }
                                    })
                                    .unwrap_or_default();
                                eprintln!(
                                    "  {} {} T{} verdict[{}]: {}",
                                    ts_short.dim(),
                                    icon.yellow(),
                                    evt.turn.unwrap_or(0),
                                    severity.yellow(),
                                    details,
                                );
                            }
                            session_journal::JournalEventType::TurnEvaluation => {
                                let metadata = evt.metadata.as_ref();
                                let source = metadata
                                    .and_then(|m| m.get("source"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("runtime");
                                let quality = metadata
                                    .and_then(|m| m.get("quality"))
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0);
                                let confidence = metadata
                                    .and_then(|m| m.get("confidence"))
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0);
                                let success = metadata
                                    .and_then(|m| m.get("success"))
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false);
                                let signals = metadata
                                    .and_then(|m| m.get("signals"))
                                    .and_then(|v| v.as_array())
                                    .map(|signals| {
                                        signals
                                            .iter()
                                            .filter_map(|signal| {
                                                signal.get("kind").and_then(|kind| kind.as_str())
                                            })
                                            .take(2)
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    })
                                    .filter(|summary| !summary.is_empty())
                                    .map(|summary| format!(" [{summary}]"))
                                    .unwrap_or_default();
                                eprintln!(
                                    "  {} {} {}eval[{}]: q={:.2} conf={:.2}{}",
                                    ts_short.dim(),
                                    if success {
                                        "◎".green().to_string()
                                    } else {
                                        theme::icon_warn().to_string()
                                    },
                                    evt.turn.map(|turn| format!("T{turn} ")).unwrap_or_default(),
                                    source.dim(),
                                    quality,
                                    confidence,
                                    signals,
                                );
                            }
                            session_journal::JournalEventType::PlanProgress => {
                                let action = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("action"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("progress");
                                let subtask = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("subtask_title"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let pct = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("progress_pct"))
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                let icon = match action {
                                    "started" => "▶",
                                    "completed" => "✓",
                                    "plan_complete" => "✓",
                                    "plan_paused" => "⏸",
                                    "skipped" => "⏭",
                                    _ => "·",
                                };
                                eprintln!(
                                    "  {} {} T{} plan {}: {} ({}%)",
                                    ts_short.dim(),
                                    icon.cyan(),
                                    evt.turn.unwrap_or(0),
                                    action,
                                    subtask,
                                    pct,
                                );
                            }
                            session_journal::JournalEventType::PlanEdit => {
                                let action = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("action"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("edit");
                                eprintln!(
                                    "  {} {} plan edit: {}",
                                    ts_short.dim(),
                                    "✏".cyan(),
                                    action,
                                );
                            }
                            session_journal::JournalEventType::PlanLifecycle => {
                                let summary = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("summary"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("lifecycle");
                                eprintln!("  {} {} plan: {}", ts_short.dim(), "📋".cyan(), summary,);
                            }
                            session_journal::JournalEventType::GoalSteered => {
                                let source = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("source"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown");
                                let new_goal = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("new_goal"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("updated goal");
                                if let Some(previous_goal) = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("previous_goal"))
                                    .and_then(|v| v.as_str())
                                {
                                    eprintln!(
                                        "  {} {} goal steered ({}): {} -> {}",
                                        ts_short.dim(),
                                        "🎯".cyan(),
                                        source,
                                        previous_goal,
                                        new_goal,
                                    );
                                } else {
                                    eprintln!(
                                        "  {} {} goal steered ({}): {}",
                                        ts_short.dim(),
                                        "🎯".cyan(),
                                        source,
                                        new_goal,
                                    );
                                }
                            }
                            session_journal::JournalEventType::ApprovalRequired => {
                                let approval =
                                    evt.metadata.as_ref().and_then(|m| m.get("approval"));
                                let tool = approval
                                    .and_then(|m| m.get("tool_name"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let kind = approval
                                    .and_then(|m| m.get("approval_kind"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("standard");
                                eprintln!(
                                    "  {} {} approval required: {} ({})",
                                    ts_short.dim(),
                                    "⛔".yellow(),
                                    tool,
                                    kind,
                                );
                            }
                            session_journal::JournalEventType::ApprovalDecision => {
                                let approval =
                                    evt.metadata.as_ref().and_then(|m| m.get("approval"));
                                let tool = approval
                                    .and_then(|m| m.get("tool_name"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let decision = approval
                                    .and_then(|m| m.get("decision"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let reason = approval
                                    .and_then(|m| m.get("reason"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                if reason.is_empty() {
                                    eprintln!(
                                        "  {} {} approval decision: {} → {}",
                                        ts_short.dim(),
                                        "✓".green(),
                                        tool,
                                        decision,
                                    );
                                } else {
                                    eprintln!(
                                        "  {} {} approval decision: {} → {} ({})",
                                        ts_short.dim(),
                                        "✓".green(),
                                        tool,
                                        decision,
                                        reason.dim(),
                                    );
                                }
                            }
                            session_journal::JournalEventType::ApprovalTimeout => {
                                let approval =
                                    evt.metadata.as_ref().and_then(|m| m.get("approval"));
                                let tool = approval
                                    .and_then(|m| m.get("tool_name"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                eprintln!(
                                    "  {} {} approval timed out: {}",
                                    ts_short.dim(),
                                    theme::icon_warn(),
                                    tool,
                                );
                            }
                            session_journal::JournalEventType::ExecutionBoundaryOpened => {
                                let boundary = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("execution_boundary"));
                                let kind = boundary
                                    .and_then(|m| m.get("kind"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("boundary");
                                let transaction_id = boundary
                                    .and_then(|m| m.get("transaction_id"))
                                    .and_then(|v| v.as_str());
                                let label = match (kind, transaction_id) {
                                    ("tool_batch", Some(id)) => format!("transaction `{id}`"),
                                    ("tool_batch", None) => "transaction".to_string(),
                                    ("turn_rollback", _) => "turn rollback".to_string(),
                                    (other, Some(id)) => format!("{other} `{id}`"),
                                    (other, None) => other.to_string(),
                                };
                                eprintln!(
                                    "  {} {} boundary opened: {}",
                                    ts_short.dim(),
                                    "⟦".cyan(),
                                    label,
                                );
                            }
                            session_journal::JournalEventType::ExecutionBoundaryCommitted => {
                                let boundary = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("execution_boundary"));
                                let kind = boundary
                                    .and_then(|m| m.get("kind"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("boundary");
                                let transaction_id = boundary
                                    .and_then(|m| m.get("transaction_id"))
                                    .and_then(|v| v.as_str());
                                let label = match (kind, transaction_id) {
                                    ("tool_batch", Some(id)) => format!("transaction `{id}`"),
                                    ("tool_batch", None) => "transaction".to_string(),
                                    ("turn_rollback", _) => "turn rollback".to_string(),
                                    (other, Some(id)) => format!("{other} `{id}`"),
                                    (other, None) => other.to_string(),
                                };
                                eprintln!(
                                    "  {} {} boundary committed: {}",
                                    ts_short.dim(),
                                    "✓".green(),
                                    label,
                                );
                            }
                            session_journal::JournalEventType::ExecutionBoundaryAborted => {
                                let boundary = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("execution_boundary"));
                                let kind = boundary
                                    .and_then(|m| m.get("kind"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("boundary");
                                let transaction_id = boundary
                                    .and_then(|m| m.get("transaction_id"))
                                    .and_then(|v| v.as_str());
                                let label = match (kind, transaction_id) {
                                    ("tool_batch", Some(id)) => format!("transaction `{id}`"),
                                    ("tool_batch", None) => "transaction".to_string(),
                                    ("turn_rollback", _) => "turn rollback".to_string(),
                                    (other, Some(id)) => format!("{other} `{id}`"),
                                    (other, None) => other.to_string(),
                                };
                                let reason = boundary
                                    .and_then(|m| m.get("reason"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("aborted");
                                eprintln!(
                                    "  {} {} boundary aborted: {} ({})",
                                    ts_short.dim(),
                                    theme::icon_warn(),
                                    label,
                                    reason.dim(),
                                );
                            }
                            session_journal::JournalEventType::SessionFork => {
                                let parent = evt
                                    .session_lineage
                                    .as_ref()
                                    .map(|l| l.parent_session_id.as_str())
                                    .unwrap_or("?");
                                let note = evt.user_input.as_deref().unwrap_or("");
                                eprintln!(
                                    "  {} {} fork ← {} {}",
                                    ts_short.dim(),
                                    "⎇".cyan(),
                                    parent.cyan(),
                                    note.dim()
                                );
                            }
                            session_journal::JournalEventType::SyncMarker => {
                                let ver = evt
                                    .edge_policy
                                    .as_ref()
                                    .and_then(|p| p.cloud_policy_version.as_deref())
                                    .unwrap_or("-");
                                let corr = evt
                                    .coordination
                                    .as_ref()
                                    .and_then(|c| c.correlation_id.as_deref())
                                    .unwrap_or("-");
                                let note = evt.user_input.as_deref().unwrap_or("");
                                eprintln!(
                                    "  {} {} sync policy:{} corr:{} {}",
                                    ts_short.dim(),
                                    "⇄".dim(),
                                    ver.dim(),
                                    corr.dim(),
                                    note.dim()
                                );
                            }
                            session_journal::JournalEventType::DelegationStarted => {
                                let pattern = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("pattern"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let count = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("agent_count"))
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                eprintln!(
                                    "  {} {} delegation started ({}, {} agents)",
                                    ts_short.dim(),
                                    "⑂".cyan(),
                                    pattern,
                                    count,
                                );
                            }
                            session_journal::JournalEventType::DelegationSubRunStarted => {
                                let meta = evt.metadata.as_ref();
                                let agent = meta
                                    .and_then(|m| m.get("agent_id"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let run = meta
                                    .and_then(|m| m.get("sub_run_id"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let retry_of = meta
                                    .and_then(|m| m.get("retry_of"))
                                    .and_then(|v| v.as_str())
                                    .map(|run_id| format!(" (retry of {run_id})"))
                                    .unwrap_or_default();
                                eprintln!(
                                    "  {} {} sub-run {} started {}{}",
                                    ts_short.dim(),
                                    "↳".cyan(),
                                    agent,
                                    run.dim(),
                                    retry_of.dim(),
                                );
                            }
                            session_journal::JournalEventType::DelegationSubRunCompleted => {
                                let meta = evt.metadata.as_ref();
                                let agent = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("agent_id"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let status = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("status"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let icon = if status == "completed" { "✓" } else { "✗" };
                                eprintln!(
                                    "  {} {} sub-run {} → {}",
                                    ts_short.dim(),
                                    icon.cyan(),
                                    agent,
                                    status,
                                );
                                if let Some(preview) = meta
                                    .and_then(|m| m.get("output_preview"))
                                    .and_then(|v| v.as_str())
                                    .filter(|s| !s.is_empty())
                                {
                                    eprintln!("      {}", ellipsize(preview, 120).dim());
                                }
                                if let Some(error) = meta
                                    .and_then(|m| m.get("error"))
                                    .and_then(|v| v.as_str())
                                    .filter(|s| !s.is_empty())
                                {
                                    eprintln!("      {}", ellipsize(error, 120).red());
                                }
                            }
                            session_journal::JournalEventType::DelegationCompleted => {
                                let meta = evt.metadata.as_ref();
                                let succeeded = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("succeeded"))
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                let failed = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("failed"))
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                eprintln!(
                                    "  {} {} delegation done ({} ok, {} failed)",
                                    ts_short.dim(),
                                    "⑂".green(),
                                    succeeded,
                                    failed,
                                );
                                if let Some(preview) = meta
                                    .and_then(|m| m.get("aggregated_output_preview"))
                                    .and_then(|v| v.as_str())
                                    .filter(|s| !s.is_empty())
                                {
                                    eprintln!("      {}", ellipsize(preview, 120).cyan());
                                }
                            }
                            session_journal::JournalEventType::AdaptiveBaselinePromoted => {
                                let meta = evt.metadata.as_ref();
                                let task_type = meta
                                    .and_then(|m| m.get("task_type"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let domain = meta
                                    .and_then(|m| m.get("domain"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("any");
                                let variant = meta
                                    .and_then(|m| m.get("variant_id"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let experiment = meta
                                    .and_then(|m| m.get("experiment_id"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                eprintln!(
                                    "  {} {} adaptive baseline {} / {} → {} ({})",
                                    ts_short.dim(),
                                    "⚙".cyan(),
                                    task_type,
                                    domain,
                                    variant,
                                    experiment.dim(),
                                );
                            }
                            session_journal::JournalEventType::AgentTerminated => {
                                let m = evt.metadata.as_ref();
                                let agent = m
                                    .and_then(|x| x.get("agent_id"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let run = m
                                    .and_then(|x| x.get("run_id"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let status = m
                                    .and_then(|x| x.get("status"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let turns = m
                                    .and_then(|x| x.get("turns_completed"))
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                eprintln!(
                                    "  {} {} agent {} run {} → {} ({} turns)",
                                    ts_short.dim(),
                                    "⌁".dim(),
                                    agent.dim(),
                                    run.dim(),
                                    status.cyan(),
                                    turns,
                                );
                            }
                            session_journal::JournalEventType::VerificationCompleted => {
                                let scope = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("scope"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("subtask");
                                let passed = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("passed"))
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false);
                                let icon = if passed {
                                    theme::icon_ok()
                                } else {
                                    theme::icon_err()
                                };
                                eprintln!(
                                    "  {} {} {} verification {}",
                                    ts_short.dim(),
                                    icon,
                                    scope,
                                    if passed { "passed" } else { "failed" },
                                );
                            }
                            session_journal::JournalEventType::CompositeSnapshot => {
                                let snap_id = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("snapshot_id"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let components = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("components"))
                                    .and_then(|v| v.as_array())
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|v| v.as_str())
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    })
                                    .unwrap_or_default();
                                eprintln!(
                                    "  {} {} T{} snapshot {} [{}]",
                                    ts_short.dim(),
                                    "📸".green(),
                                    evt.turn.unwrap_or(0),
                                    snap_id,
                                    components,
                                );
                            }
                            session_journal::JournalEventType::ContextAssemblyRecorded => {
                                // Context assembly trace (M1 telemetry) — detailed view via /session context
                                let tokens = evt
                                    .context_assembly_trace
                                    .as_ref()
                                    .and_then(|t| t.get("token_budget"))
                                    .and_then(|tb| tb.get("total_tokens_used"))
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                eprintln!(
                                    "  {} {} T{} context trace ({} tokens)",
                                    ts_short.dim(),
                                    "📊".cyan(),
                                    evt.turn.unwrap_or(0),
                                    tokens,
                                );
                            }
                            session_journal::JournalEventType::DelegationRetry => {
                                let m = evt.metadata.as_ref();
                                let original = m
                                    .and_then(|x| x.get("original_run_id"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let retry = m
                                    .and_then(|x| x.get("retry_run_id"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let attempt = m
                                    .and_then(|x| x.get("attempt"))
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                let reason = m
                                    .and_then(|x| x.get("reason"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                eprintln!(
                                    "  {} {} retry #{} {} → {} {}",
                                    ts_short.dim(),
                                    "↻".yellow(),
                                    attempt,
                                    original.dim(),
                                    retry.dim(),
                                    reason.dim(),
                                );
                            }
                            session_journal::JournalEventType::DriftDetected => {
                                let severity = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("severity"))
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0);
                                let evidence_count = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("evidence_count"))
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                eprintln!(
                                    "  {} {} T{} drift detected (severity {:.2}, {} evidence)",
                                    ts_short.dim(),
                                    "↯".yellow(),
                                    evt.turn.unwrap_or(0),
                                    severity,
                                    evidence_count,
                                );
                            }
                            session_journal::JournalEventType::AdaptiveScenarioApplied => {
                                let scenario = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("scenario"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown");
                                let confidence = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("confidence"))
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0);
                                let n_changes = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("config_changes"))
                                    .and_then(|v| v.as_array())
                                    .map(|a| a.len())
                                    .unwrap_or(0);
                                eprintln!(
                                    "  {} {} T{} adaptive profile → {} (conf {:.2}, {} config changes)",
                                    ts_short.dim(),
                                    "⚙".cyan(),
                                    evt.turn.unwrap_or(0),
                                    scenario,
                                    confidence,
                                    n_changes,
                                );
                            }
                            session_journal::JournalEventType::AdaptivePerTurnApplied => {
                                let n_changes = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("changes"))
                                    .and_then(|v| v.as_array())
                                    .map(|a| a.len())
                                    .unwrap_or(0);
                                let triggers = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("triggers"))
                                    .and_then(|v| v.as_array())
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|t| t.as_str())
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    })
                                    .unwrap_or_default();
                                if n_changes > 0 {
                                    eprintln!(
                                        "  {} {} T{} per-turn adapt: {} changes ({})",
                                        ts_short.dim(),
                                        "↻".cyan(),
                                        evt.turn.unwrap_or(0),
                                        n_changes,
                                        triggers,
                                    );
                                }
                            }
                            session_journal::JournalEventType::AdaptiveExperimentEnrolled => {
                                let exp = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("experiment_name"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let variant = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("variant_id"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                eprintln!(
                                    "  {} {} T{} experiment enrolled: {} → variant {}",
                                    ts_short.dim(),
                                    "🧪".cyan(),
                                    evt.turn.unwrap_or(0),
                                    exp,
                                    variant,
                                );
                            }
                            session_journal::JournalEventType::AdaptiveTuningRuleTriggered => {
                                let rule = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("rule_name"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let n_changes = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("config_changes"))
                                    .and_then(|v| v.as_array())
                                    .map(|a| a.len())
                                    .unwrap_or(0);
                                eprintln!(
                                    "  {} {} T{} tuning rule: {} ({} changes)",
                                    ts_short.dim(),
                                    "⚡".yellow(),
                                    evt.turn.unwrap_or(0),
                                    rule,
                                    n_changes,
                                );
                            }
                            session_journal::JournalEventType::InterruptionRecorded => {
                                let kind = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("interruption"))
                                    .and_then(|v| v.get("kind"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown");
                                let resumable = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("interruption"))
                                    .and_then(|v| v.get("resumable"))
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false);
                                let icon = if resumable { "⏸" } else { "⛔" };
                                eprintln!(
                                    "  {} {} T{} interruption: {} (resumable={})",
                                    ts_short.dim(),
                                    icon.yellow(),
                                    evt.turn.unwrap_or(0),
                                    kind,
                                    resumable,
                                );
                            }
                        }
                    }
                    // Summary stats
                    let turns: Vec<_> = events
                        .iter()
                        .filter(|e| e.event_type == session_journal::JournalEventType::Turn)
                        .collect();
                    let errors: Vec<_> = events
                        .iter()
                        .filter(|e| e.event_type == session_journal::JournalEventType::TurnError)
                        .collect();
                    let approval_required = events
                        .iter()
                        .filter(|e| {
                            e.event_type == session_journal::JournalEventType::ApprovalRequired
                        })
                        .count();
                    let approval_decisions = events
                        .iter()
                        .filter(|e| {
                            e.event_type == session_journal::JournalEventType::ApprovalDecision
                        })
                        .count();
                    let approval_timeouts = events
                        .iter()
                        .filter(|e| {
                            e.event_type == session_journal::JournalEventType::ApprovalTimeout
                        })
                        .count();
                    let boundary_opened = events
                        .iter()
                        .filter(|e| {
                            e.event_type
                                == session_journal::JournalEventType::ExecutionBoundaryOpened
                        })
                        .count();
                    let boundary_committed = events
                        .iter()
                        .filter(|e| {
                            e.event_type
                                == session_journal::JournalEventType::ExecutionBoundaryCommitted
                        })
                        .count();
                    let boundary_aborted = events
                        .iter()
                        .filter(|e| {
                            e.event_type
                                == session_journal::JournalEventType::ExecutionBoundaryAborted
                        })
                        .count();
                    let total_tokens_in: u64 = turns.iter().filter_map(|e| e.tokens_in).sum();
                    let total_tokens_out: u64 = turns.iter().filter_map(|e| e.tokens_out).sum();
                    let total_tools: u32 = turns.iter().filter_map(|e| e.tool_count).sum();
                    let total_ms: u64 = turns.iter().filter_map(|e| e.duration_ms).sum();
                    eprintln!(
                        "\n  {} {} turns, {} errors, {}+{} tokens, {} tool calls, {:.1}s total",
                        "Summary:".bold(),
                        turns.len(),
                        errors.len(),
                        total_tokens_in,
                        total_tokens_out,
                        total_tools,
                        total_ms as f64 / 1000.0,
                    );
                    if approval_required > 0 || approval_decisions > 0 || approval_timeouts > 0 {
                        eprintln!(
                            "  {} {} required, {} decisions, {} timeouts",
                            "Approvals:".bold(),
                            approval_required,
                            approval_decisions,
                            approval_timeouts,
                        );
                    }
                    if boundary_opened > 0 || boundary_committed > 0 || boundary_aborted > 0 {
                        eprintln!(
                            "  {} {} opened, {} committed, {} aborted",
                            "Boundaries:".bold(),
                            boundary_opened,
                            boundary_committed,
                            boundary_aborted,
                        );
                    }
                    eprintln!();
                }
                Err(e) => {
                    eprintln!("{}", format!("  ✗ Failed to read journal: {e}").red());
                }
            }
        }
        "list" => {
            handle_session_list(sub_arg, state);
        }
        "context" => {
            // /session context [turn] [session_id]
            // Shows context assembly trace for a specific turn
            let parts: Vec<&str> = sub_arg.split_whitespace().collect();
            let (turn_str, session_arg) = match parts.len() {
                0 => (None, ""),
                1 => {
                    // Could be turn number or session id
                    if parts[0].parse::<u32>().is_ok() {
                        (Some(parts[0]), "")
                    } else {
                        (None, parts[0])
                    }
                }
                _ => (Some(parts[0]), parts[1]),
            };

            let (target_sid, resolved_prefix) =
                match resolve_journal_target_session(session_arg, state, "  No active session.") {
                    Ok(value) => value,
                    Err(msg) => {
                        eprintln!("{msg}");
                        return;
                    }
                };
            if resolved_prefix && !session_arg.is_empty() {
                eprintln!(
                    "  {} Resolved {} → {}",
                    theme::icon_ok(),
                    session_arg.cyan(),
                    target_sid.as_str().cyan()
                );
            }

            match session_journal::read_journal(&target_sid) {
                Ok(events) => {
                    // Find context assembly traces
                    let traces: Vec<_> = events
                        .iter()
                        .filter(|e| {
                            e.event_type
                                == session_journal::JournalEventType::ContextAssemblyRecorded
                        })
                        .collect();

                    if traces.is_empty() {
                        eprintln!(
                            "  {} {}",
                            "ℹ".cyan(),
                            "No context traces in this session. Enable telemetry to record traces."
                                .dim()
                        );
                        return;
                    }

                    // If no turn specified, show summary of all traces
                    let turn_filter: Option<u32> = turn_str.and_then(|s| s.parse().ok());

                    if let Some(turn) = turn_filter {
                        // Show detailed trace for specific turn
                        let trace_evt = traces.iter().find(|e| e.turn == Some(turn));

                        match trace_evt {
                            Some(evt) => {
                                print_context_trace_detail(evt, turn);
                            }
                            None => {
                                eprintln!("  {} No context trace for turn {}", "✗".red(), turn);
                                let available: Vec<_> =
                                    traces.iter().filter_map(|e| e.turn).collect();
                                if !available.is_empty() {
                                    eprintln!("  {} Available turns: {:?}", "ℹ".cyan(), available);
                                }
                            }
                        }
                    } else {
                        // No turn specified — show summary of all traces
                        eprintln!(
                            "\n{}",
                            format!(
                                "─── Context Traces ({}) ─────────────────────────",
                                traces.len()
                            )
                            .bold()
                            .cyan()
                        );
                        for evt in &traces {
                            let ts_short = evt.ts.get(11..19).unwrap_or(&evt.ts);
                            let t = evt.turn.unwrap_or(0);
                            let trace = evt.context_assembly_trace.as_ref();

                            let tokens_used = trace
                                .and_then(|t| t.get("token_budget"))
                                .and_then(|tb| tb.get("total_tokens_used"))
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            let tools_count = trace
                                .and_then(|t| t.get("tools"))
                                .and_then(|t| t.get("tools_selected"))
                                .and_then(|v| v.as_array())
                                .map(|a| a.len())
                                .unwrap_or(0);
                            let memory_count = trace
                                .and_then(|t| t.get("memory"))
                                .and_then(|m| m.get("memories_selected"))
                                .and_then(|v| v.as_array())
                                .map(|a| a.len())
                                .unwrap_or(0);

                            eprintln!(
                                "  {} T{:<3} {} tokens · {} tools · {} memories",
                                ts_short.dim(),
                                t,
                                format_u64_grouped(tokens_used).cyan(),
                                tools_count,
                                memory_count,
                            );
                        }
                        eprintln!();
                        eprintln!(
                            "  {}",
                            "Use /session context <turn> to see full trace".dim()
                        );
                        eprintln!();
                    }
                }
                Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
            }
        }
        "errors" => {
            let (target_sid, resolved_prefix) =
                match resolve_journal_target_session(sub_arg, state, "  No active session.") {
                    Ok(value) => value,
                    Err(msg) => {
                        eprintln!("{msg}");
                        return;
                    }
                };
            if resolved_prefix {
                eprintln!(
                    "  {} Resolved {} → {}",
                    theme::icon_ok(),
                    sub_arg.cyan(),
                    target_sid.as_str().cyan()
                );
            }
            match session_journal::read_journal(&target_sid) {
                Ok(events) => {
                    let errors: Vec<_> = events
                        .iter()
                        .filter(|e| {
                            matches!(
                                e.event_type,
                                session_journal::JournalEventType::TurnError
                                    | session_journal::JournalEventType::Error
                            )
                        })
                        .collect();
                    if errors.is_empty() {
                        eprintln!(
                            "  {} {}",
                            theme::icon_ok(),
                            "No errors in this session.".green()
                        );
                    } else {
                        eprintln!(
                            "\n{}",
                            format!(
                                "─── Errors ({}) ─────────────────────────────────",
                                errors.len()
                            )
                            .bold()
                        );
                        for err in &errors {
                            let ts_short = err.ts.get(11..19).unwrap_or(&err.ts);
                            eprintln!(
                                "  {} T{} {}",
                                ts_short.dim(),
                                err.turn.unwrap_or(0),
                                err.error.as_deref().unwrap_or("?").red(),
                            );
                        }
                        eprintln!();
                    }
                }
                Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
            }
        }
        "export" => {
            let (target_sid, resolved_prefix) =
                match resolve_journal_target_session(sub_arg, state, "  No active session.") {
                    Ok(value) => value,
                    Err(msg) => {
                        eprintln!("{msg}");
                        return;
                    }
                };
            if resolved_prefix {
                eprintln!(
                    "  {} Resolved {} → {}",
                    theme::icon_ok(),
                    sub_arg.cyan(),
                    target_sid.as_str().cyan()
                );
            }
            export_session_markdown(&target_sid);
        }
        "cleanup" => {
            handle_session_cleanup(sub_arg, state);
        }
        "switch" | "sw" => {
            handle_session_switch(sub_arg, state);
        }
        "verify" | "sync" | "status" => {
            handle_session_verify(state);
        }
        "drift" => {
            handle_session_drift(sub_arg, state);
        }
        "adaptive" | "profile" | "tuning" => {
            handle_session_adaptive(sub_arg, state);
        }
        "analyze" | "diag" => {
            handle_session_analyze(sub_arg, state);
        }
        other => {
            eprintln!("{}", format!("  Unknown subcommand: {other}").red());
            eprintln!(
                "  {}",
                "Usage: /session [list|switch|history|context|errors|export|fork|cleanup|verify|drift|adaptive|analyze] …"
                    .dim()
            );
        }
    }
}

// ── Context trace display ───────────────────────────────────────────────────

/// Print detailed context assembly trace for a specific turn.
fn print_context_trace_detail(evt: &session_journal::JournalEvent, turn: u32) {
    let trace = match evt.context_assembly_trace.as_ref() {
        Some(t) => t,
        None => {
            eprintln!("  {} No trace data for turn {}", "✗".red(), turn);
            return;
        }
    };

    eprintln!(
        "\n{}",
        format!("─── Context Assembly Trace (Turn {turn}) ─────────────────")
            .bold()
            .cyan()
    );

    // ─── Token Budget ───────────────────────────────────────────────────────
    if let Some(tb) = trace.get("token_budget") {
        eprintln!("\n  {}", "Token Budget".bold());
        let total = tb
            .get("total_tokens_used")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let limit = tb
            .get("context_limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let pct = if limit > 0 {
            (total as f64 / limit as f64 * 100.0) as u32
        } else {
            0
        };
        eprintln!(
            "    {} / {} ({pct}%)",
            format_u64_grouped(total).cyan(),
            format_u64_grouped(limit).dim()
        );

        // Breakdown
        if let Some(breakdown) = tb.get("breakdown").and_then(|b| b.as_object()) {
            for (key, val) in breakdown {
                let tokens = val.as_u64().unwrap_or(0);
                if tokens > 0 {
                    eprintln!(
                        "      {:<20} {}",
                        key.as_str().dim(),
                        format_u64_grouped(tokens)
                    );
                }
            }
        }
    }

    // ─── System Prompt ──────────────────────────────────────────────────────
    if let Some(sp) = trace.get("system_prompt") {
        eprintln!("\n  {}", "System Prompt".bold());
        let total = sp.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        eprintln!("    Total: {} tokens", format_u64_grouped(total).cyan());

        let base = sp
            .get("base_persona_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let env = sp
            .get("environment_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if base > 0 {
            eprintln!(
                "      {:<20} {}",
                "base_persona".dim(),
                format_u64_grouped(base)
            );
        }
        if env > 0 {
            eprintln!(
                "      {:<20} {}",
                "environment".dim(),
                format_u64_grouped(env)
            );
        }

        // Skills
        if let Some(skills) = sp.get("skills_injected").and_then(|s| s.as_array()) {
            if !skills.is_empty() {
                eprintln!("    Skills:");
                for skill in skills.iter().take(5) {
                    let name = skill.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    let tokens = skill.get("tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    eprintln!("      {} ({} tokens)", name.green(), tokens);
                }
                if skills.len() > 5 {
                    eprintln!("      … and {} more", skills.len() - 5);
                }
            }
        }

        // Memories
        if let Some(memories) = sp.get("repository_memories").and_then(|m| m.as_array()) {
            if !memories.is_empty() {
                eprintln!("    Repository Memories: {}", memories.len());
            }
        }
    }

    // ─── History Selection ──────────────────────────────────────────────────
    if let Some(hist) = trace.get("history") {
        eprintln!("\n  {}", "History Selection".bold());
        let total = hist
            .get("total_turns_available")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let retained = hist
            .get("turns_retained")
            .and_then(|t| t.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let compressed = hist
            .get("turns_compressed")
            .and_then(|t| t.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let dropped = hist
            .get("turns_dropped")
            .and_then(|t| t.as_array())
            .map(|a| a.len())
            .unwrap_or(0);

        eprintln!(
            "    {} available → {} retained, {} compressed, {} dropped",
            total,
            retained.to_string().green(),
            compressed.to_string().yellow(),
            dropped.to_string().red()
        );

        if let Some(ratio) = hist.get("compression_ratio").and_then(|v| v.as_f64()) {
            eprintln!("    Compression ratio: {:.1}%", ratio * 100.0);
        }
    }

    // ─── Memory Retrieval ───────────────────────────────────────────────────
    if let Some(mem) = trace.get("memory") {
        eprintln!("\n  {}", "Memory Retrieval".bold());
        if let Some(query) = mem.get("query").and_then(|v| v.as_str()) {
            let q_short = if query.len() > 60 {
                format!("{}…", &query[..query.floor_char_boundary(60)])
            } else {
                query.to_string()
            };
            eprintln!("    Query: \"{}\"", q_short.dim());
        }

        let candidates = mem
            .get("candidates_considered")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let selected = mem
            .get("memories_selected")
            .and_then(|m| m.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let total_tokens = mem
            .get("total_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        eprintln!(
            "    {} candidates → {} selected ({} tokens)",
            candidates,
            selected.to_string().green(),
            format_u64_grouped(total_tokens)
        );

        // Show top memories
        if let Some(memories) = mem.get("memories_selected").and_then(|m| m.as_array()) {
            for m in memories.iter().take(3) {
                let content = m
                    .get("content_preview")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let score = m
                    .get("relevance_score")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let short = if content.len() > 50 {
                    format!("{}…", &content[..content.floor_char_boundary(50)])
                } else {
                    content.to_string()
                };
                eprintln!("      [{:.2}] \"{}\"", score, short.dim());
            }
        }
    }

    // ─── Tool Selection ─────────────────────────────────────────────────────
    if let Some(tools) = trace.get("tools") {
        eprintln!("\n  {}", "Tool Selection".bold());
        let strategy = tools
            .get("strategy")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let confidence = tools
            .get("confidence")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let available = tools
            .get("tools_available")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        eprintln!(
            "    Strategy: {} (confidence: {:.2})",
            strategy.cyan(),
            confidence
        );

        if let Some(selected) = tools.get("tools_selected").and_then(|t| t.as_array()) {
            eprintln!(
                "    Selected ({}/{}): {}",
                selected.len(),
                available,
                selected
                    .iter()
                    .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
                    .take(10)
                    .collect::<Vec<_>>()
                    .join(", ")
                    .green()
            );
        }

        // Budget stats
        let budget_used = tools
            .get("budget_used")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if budget_used > 0 {
            eprintln!(
                "    Tool schemas: {} tokens",
                format_u64_grouped(budget_used)
            );
        }
    }

    // ─── Explanations ───────────────────────────────────────────────────────
    if let Some(explanations) = trace.get("explanations").and_then(|e| e.as_array()) {
        if !explanations.is_empty() {
            eprintln!("\n  {}", "Decision Explanations".bold());
            for exp in explanations.iter().take(5) {
                let decision = exp
                    .get("decision_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let reasoning = exp.get("reasoning").and_then(|v| v.as_str()).unwrap_or("?");
                eprintln!("    {} {}", decision.cyan(), reasoning.dim());
            }
        }
    }

    eprintln!();
}

// ── Session export ──────────────────────────────────────────────────────────

/// Format tool call records as a summarized markdown block.
fn format_tool_calls_md(calls: &[session_journal::ToolCallRecord]) -> String {
    let mut out = String::new();
    out.push_str("\n<details>\n<summary>Tool calls</summary>\n\n");
    for tc in calls {
        let status = if tc.ok { "✓" } else { "✗" };
        let display = super::stream_render::format_tool_display_from_preview(
            &tc.name,
            tc.args_preview.as_deref(),
        );
        out.push_str(&format!("- `{display}` {status} ({}ms)", tc.ms));
        out.push('\n');
        if let Some(ref err) = tc.error {
            out.push_str(&format!("  > Error: {err}\n"));
        }
        if let Some(ref preview) = tc.result_preview {
            // Show a short excerpt of the result
            let short = if preview.len() > 200 {
                format!("{}…", &preview[..preview.floor_char_boundary(200)])
            } else {
                preview.clone()
            };
            out.push_str(&format!(
                "  > ```\n  > {}\n  > ```\n",
                short.replace('\n', "\n  > ")
            ));
        }
    }
    out.push_str("\n</details>\n\n");
    out
}

/// Build a markdown export from journal events.
fn build_export_markdown(session_id: &str, events: &[session_journal::JournalEvent]) -> String {
    let mut md = format!("# Session: {session_id}\n\n");
    for evt in events {
        let ts_short = evt.ts.get(..19).unwrap_or(&evt.ts);
        match evt.event_type {
            session_journal::JournalEventType::SessionStart => {
                md.push_str(&format!(
                    "## Session Start\n- **Time:** {ts_short}\n- **Model:** {}\n\n",
                    evt.model.as_deref().unwrap_or("default")
                ));
            }
            session_journal::JournalEventType::Turn => {
                md.push_str(&format!(
                    "### Turn {}\n- **Time:** {ts_short}\n- **Duration:** {}ms\n- **Tokens:** {} → {}\n- **Tools used:** {}\n\n",
                    evt.turn.unwrap_or(0),
                    evt.duration_ms.unwrap_or(0),
                    evt.tokens_in.unwrap_or(0),
                    evt.tokens_out.unwrap_or(0),
                    evt.tool_count.unwrap_or(0),
                ));

                if let Some(ref input) = evt.user_input {
                    if !input.is_empty() {
                        md.push_str(&format!("**User:**\n\n{input}\n\n"));
                    }
                }

                // Tool call details (collapsed)
                if let Some(ref calls) = evt.tool_calls {
                    if !calls.is_empty() {
                        md.push_str(&format_tool_calls_md(calls));
                    }
                }

                if let Some(ref output) = evt.assistant_output {
                    if !output.is_empty() {
                        md.push_str(&format!("**Assistant:**\n\n{output}\n\n"));
                    }
                }
                md.push_str("---\n\n");
            }
            session_journal::JournalEventType::TurnError => {
                md.push_str(&format!(
                    "### Turn {} ❌ Error\n- **Time:** {ts_short}\n- **Error:** {}\n\n---\n\n",
                    evt.turn.unwrap_or(0),
                    evt.error.as_deref().unwrap_or("(no details)"),
                ));
            }
            session_journal::JournalEventType::Compact => {
                let summary_line = evt
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("compact_summary"))
                    .and_then(|v| v.as_str())
                    .map(|s| format!("- **Summary:** {s}\n"))
                    .unwrap_or_default();
                md.push_str(&format!(
                    "### Compact\n- **Time:** {ts_short}\n- **Turns compacted:** {}\n- **Facts stored:** {}\n{summary_line}\n",
                    evt.turns_compacted.unwrap_or(0),
                    evt.facts_stored.unwrap_or(0),
                ));
            }
            session_journal::JournalEventType::ConfigChange => {
                md.push_str(&format!(
                    "- ⚙️ {ts_short}: {} → {}\n",
                    evt.config_key.as_deref().unwrap_or("?"),
                    evt.config_value.as_deref().unwrap_or("?"),
                ));
            }
            session_journal::JournalEventType::SessionEnd => {
                md.push_str(&format!(
                    "## Session End\n- **Time:** {ts_short}\n- **Total turns:** {}\n",
                    evt.turn.unwrap_or(0),
                ));
            }
            session_journal::JournalEventType::ExecutionBoundaryOpened => {
                let boundary = evt
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("execution_boundary"));
                let kind = boundary
                    .and_then(|m| m.get("kind"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("boundary");
                let transaction_id = boundary
                    .and_then(|m| m.get("transaction_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("-");
                md.push_str(&format!(
                    "### Execution boundary opened\n- **Time:** {ts_short}\n- **Kind:** {kind}\n- **Transaction:** {transaction_id}\n\n"
                ));
            }
            session_journal::JournalEventType::ExecutionBoundaryCommitted => {
                let boundary = evt
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("execution_boundary"));
                let kind = boundary
                    .and_then(|m| m.get("kind"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("boundary");
                let transaction_id = boundary
                    .and_then(|m| m.get("transaction_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("-");
                md.push_str(&format!(
                    "### Execution boundary committed\n- **Time:** {ts_short}\n- **Kind:** {kind}\n- **Transaction:** {transaction_id}\n\n"
                ));
            }
            session_journal::JournalEventType::ExecutionBoundaryAborted => {
                let boundary = evt
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("execution_boundary"));
                let kind = boundary
                    .and_then(|m| m.get("kind"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("boundary");
                let transaction_id = boundary
                    .and_then(|m| m.get("transaction_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("-");
                let reason = boundary
                    .and_then(|m| m.get("reason"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("aborted");
                md.push_str(&format!(
                    "### Execution boundary aborted\n- **Time:** {ts_short}\n- **Kind:** {kind}\n- **Transaction:** {transaction_id}\n- **Reason:** {reason}\n\n"
                ));
            }
            session_journal::JournalEventType::SessionFork => {
                let parent = evt
                    .session_lineage
                    .as_ref()
                    .map(|l| l.parent_session_id.as_str())
                    .unwrap_or("?");
                md.push_str(&format!(
                    "### Session fork\n- **Time:** {ts_short}\n- **Parent:** {parent}\n- **Note:** {}\n\n",
                    evt.user_input.as_deref().unwrap_or(""),
                ));
            }
            session_journal::JournalEventType::SyncMarker => {
                md.push_str(&format!(
                    "### Sync marker\n- **Time:** {ts_short}\n- **Note:** {}\n\n",
                    evt.user_input.as_deref().unwrap_or(""),
                ));
            }
            session_journal::JournalEventType::AdaptiveBaselinePromoted => {
                let meta = evt.metadata.as_ref();
                let task_type = meta
                    .and_then(|m| m.get("task_type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let domain = meta
                    .and_then(|m| m.get("domain"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("any");
                let variant = meta
                    .and_then(|m| m.get("variant_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let experiment = meta
                    .and_then(|m| m.get("experiment_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                md.push_str(&format!(
                    "### Adaptive baseline promoted\n- **Time:** {ts_short}\n- **Scope:** {task_type} / {domain}\n- **Winner:** {variant}\n- **Experiment:** {experiment}\n\n"
                ));
            }
            session_journal::JournalEventType::AdaptiveScenarioApplied => {
                let meta = evt.metadata.as_ref();
                let scenario = meta
                    .and_then(|m| m.get("scenario"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let confidence = meta
                    .and_then(|m| m.get("confidence"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                md.push_str(&format!(
                    "### Adaptive scenario applied\n- **Time:** {ts_short}\n- **Scenario:** {scenario} (confidence {confidence:.2})\n\n"
                ));
            }
            session_journal::JournalEventType::AdaptivePerTurnApplied => {
                let n = evt
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("changes"))
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                if n > 0 {
                    md.push_str(&format!(
                        "### Per-turn adaptation (T{})\n- **Time:** {ts_short}\n- **Changes:** {n}\n\n",
                        evt.turn.unwrap_or(0),
                    ));
                }
            }
            _ => {}
        }
    }
    md
}

/// Export a session journal to a timestamped Markdown file in the current directory.
fn export_session_markdown(session_id: &str) {
    match session_journal::read_journal(session_id) {
        Ok(events) if events.is_empty() => {
            eprintln!("{}", "  No journal entries to export.".dim());
        }
        Ok(events) => {
            let md = build_export_markdown(session_id, &events);
            let now = chrono::Local::now();
            let export_path = format!("astra-session-{}.md", now.format("%Y%m%d-%H%M"));
            match std::fs::write(&export_path, &md) {
                Ok(_) => {
                    eprintln!("  {} Exported to {}", theme::icon_ok(), export_path.cyan())
                }
                Err(e) => eprintln!("{}", format!("  ✗ Failed to write: {e}").red()),
            }
        }
        Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
    }
}

// ── Session cleanup ─────────────────────────────────────────────────────────

/// Handle `/session cleanup [--days N] [--force] [--compress]`.
///
/// Default: show stale sessions (>30 days) and ask for confirmation.
/// `--days N` overrides the age threshold.
/// `--force` skips the confirmation prompt.
/// `--compress` archives completed journals to .jsonl.gz (instead of deleting).
fn handle_session_cleanup(arg: &str, state: &ReplState) {
    let tokens: Vec<&str> = arg.split_whitespace().collect();
    let mut max_days: u64 = 30;
    let mut force = false;
    let mut compress = false;

    let mut i = 0;
    while i < tokens.len() {
        match tokens[i] {
            "--days" | "-d" => {
                if i + 1 < tokens.len() {
                    match tokens[i + 1].parse::<u64>() {
                        Ok(d) if d > 0 => max_days = d,
                        _ => {
                            eprintln!(
                                "{}",
                                format!("  ✗ Invalid --days value: {}", tokens[i + 1]).red()
                            );
                            return;
                        }
                    }
                    i += 2;
                } else {
                    eprintln!("{}", "  ✗ --days requires a number".red());
                    return;
                }
            }
            "--force" | "-f" => {
                force = true;
                i += 1;
            }
            "--compress" | "-c" => {
                compress = true;
                i += 1;
            }
            other => {
                eprintln!("{}", format!("  ✗ Unknown flag: {other}").red());
                eprintln!(
                    "  {}",
                    "Usage: /session cleanup [--days N] [--force] [--compress]".dim()
                );
                return;
            }
        }
    }

    // If --compress with no --days, compress all completed sessions (not just stale)
    if compress {
        handle_compress(state, force);
        return;
    }

    let max_age = std::time::Duration::from_secs(max_days * 86400);
    let current_sid = state.session_id.as_deref();

    let stale = match session_journal::find_stale_sessions(max_age, current_sid) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}", format!("  ✗ Failed to scan sessions: {e}").red());
            return;
        }
    };

    if stale.is_empty() {
        eprintln!(
            "  {} No sessions older than {} days found.",
            theme::icon_ok(),
            max_days
        );
        return;
    }

    let total_bytes: u64 = stale.iter().map(|s| s.total_bytes).sum();
    eprintln!(
        "\n  Found {} session(s) older than {} days ({}):\n",
        stale.len().to_string().yellow(),
        max_days,
        human_bytes(total_bytes).yellow()
    );

    // Show at most 20 sessions in detail
    let show_count = stale.len().min(20);
    for info in &stale[..show_count] {
        let age = info
            .last_modified
            .elapsed()
            .map(|d| format_age_days(d))
            .unwrap_or_else(|_| "?".to_string());
        let short_id = if info.session_id.len() > 12 {
            &info.session_id[..12]
        } else {
            &info.session_id
        };
        eprintln!(
            "    {} {} turns, {}, {} ago",
            short_id.dim(),
            info.turns,
            human_bytes(info.total_bytes).dim(),
            age,
        );
    }
    if stale.len() > show_count {
        eprintln!(
            "    {}",
            format!("… and {} more", stale.len() - show_count).dim()
        );
    }
    eprintln!();

    if !force {
        eprint!("  Delete all {} sessions? [y/N] ", stale.len());
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_err()
            || !input.trim().eq_ignore_ascii_case("y")
        {
            eprintln!("  {}", "Cancelled.".dim());
            return;
        }
    }

    let mut deleted = 0u64;
    let mut freed = 0u64;
    let mut errors = 0u64;
    for info in &stale {
        match session_journal::delete_session(&info.session_id) {
            Ok(bytes) => {
                deleted += 1;
                freed += bytes;
            }
            Err(e) => {
                errors += 1;
                eprintln!(
                    "    {} {}…: {}",
                    theme::icon_err(),
                    &info.session_id[..info.session_id.len().min(12)],
                    e.to_string().red()
                );
            }
        }
    }

    eprintln!(
        "  {} Deleted {} session(s), freed {}",
        theme::icon_ok(),
        deleted.to_string().green(),
        human_bytes(freed).green()
    );
    if errors > 0 {
        eprintln!(
            "  {} {} session(s) could not be deleted",
            theme::icon_warn(),
            errors
        );
    }
}

/// Compress completed session journals to .jsonl.gz.
fn handle_compress(state: &ReplState, force: bool) {
    let current_sid = state.session_id.as_deref();
    let archivable = match session_journal::find_archivable_sessions(current_sid) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{}", format!("  ✗ Failed to scan: {e}").red());
            return;
        }
    };

    if archivable.is_empty() {
        eprintln!("  {} No completed sessions to compress.", theme::icon_ok());
        return;
    }

    let total_bytes: u64 = archivable.iter().map(|(_, b)| b).sum();
    eprintln!(
        "\n  Found {} completed session(s) to compress ({}):\n",
        archivable.len().to_string().yellow(),
        human_bytes(total_bytes).yellow()
    );

    if !force {
        eprint!("  Compress {} session journals? [y/N] ", archivable.len());
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_err()
            || !input.trim().eq_ignore_ascii_case("y")
        {
            eprintln!("  {}", "Cancelled.".dim());
            return;
        }
    }

    let mut compressed = 0u64;
    let mut saved = 0u64;
    let mut errors = 0u64;
    for (sid, _) in &archivable {
        match session_journal::archive_journal(sid) {
            Ok((orig, comp)) => {
                compressed += 1;
                saved += orig.saturating_sub(comp);
            }
            Err(e) => {
                errors += 1;
                let short = &sid[..sid.len().min(12)];
                eprintln!(
                    "    {} {short}…: {}",
                    theme::icon_err(),
                    e.to_string().red()
                );
            }
        }
    }

    eprintln!(
        "  {} Compressed {} session(s), saved {}",
        theme::icon_ok(),
        compressed.to_string().green(),
        human_bytes(saved).green()
    );
    if errors > 0 {
        eprintln!(
            "  {} {} session(s) could not be compressed",
            theme::icon_warn(),
            errors
        );
    }
}

fn human_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn format_age_days(d: std::time::Duration) -> String {
    let days = d.as_secs() / 86400;
    if days == 0 {
        "today".to_string()
    } else if days == 1 {
        "1 day".to_string()
    } else {
        format!("{days} days")
    }
}

// ── Session adaptive inspection ─────────────────────────────────────────────

/// Display current adaptive execution state: scenario, experiment, recent
/// per-turn adaptations, and tuning rule history from the session journal.
///
/// Usage: /session adaptive [--verbose]
fn handle_session_adaptive(_arg: &str, state: &ReplState) {
    use astra_services::session_journal;

    eprintln!(
        "\n{}",
        "─── Adaptive Execution State ─────────────────────"
            .bold()
            .cyan()
    );

    // 1. Current scenario + experiment from ObservabilitySession.
    if let Some(obs) = &state.observability_session {
        if let Ok(guard) = obs.read() {
            let scenario = guard
                .profile
                .current_scenario
                .map(|s| format!("{s:?}"))
                .unwrap_or_else(|| "none".to_string());
            eprintln!("  {} Scenario: {}", "▸".cyan(), scenario.bold());

            if let Some(exp) = &guard.active_experiment_id {
                let var = guard.active_variant.as_deref().unwrap_or("?");
                eprintln!("  {} Experiment: {} → variant {}", "▸".cyan(), exp, var);
            } else {
                eprintln!("  {} Experiment: {}", "▸".cyan(), "none".dim());
            }

            eprintln!("  {} Config snapshot:", "▸".cyan(),);
            eprintln!(
                "      token_budget.max_turn_input_tokens = {}",
                guard.config.token_budget.max_turn_input_tokens
            );
            eprintln!(
                "      memory.retrieval_top_k             = {}",
                guard.config.memory.retrieval_top_k
            );
            eprintln!(
                "      verification.strictness             = {:.3}",
                guard.config.verification.strictness
            );
            eprintln!(
                "      compression.compression_threshold   = {:.3}",
                guard.config.compression.compression_threshold
            );
            eprintln!(
                "      tool_selection.max_tools            = {}",
                guard.config.tool_selection.max_tools
            );
        }
    } else {
        eprintln!("  {}", "No active observability session.".dim());
    }

    // 2. Recent adaptive events from journal.
    let session_id = match &state.session_id {
        Some(id) => id.to_string(),
        None => {
            eprintln!("\n  {}", "No active session for journal lookup.".dim());
            eprintln!();
            return;
        }
    };

    match session_journal::read_journal(&session_id) {
        Ok(events) => {
            let adaptive_events: Vec<_> = events
                .iter()
                .filter(|e| {
                    matches!(
                        e.event_type,
                        session_journal::JournalEventType::AdaptiveScenarioApplied
                            | session_journal::JournalEventType::AdaptivePerTurnApplied
                            | session_journal::JournalEventType::AdaptiveExperimentEnrolled
                            | session_journal::JournalEventType::AdaptiveTuningRuleTriggered
                            | session_journal::JournalEventType::AdaptiveBaselinePromoted
                    )
                })
                .collect();

            if adaptive_events.is_empty() {
                eprintln!("\n  {}", "No adaptive events recorded yet.".dim());
            } else {
                eprintln!(
                    "\n  {} {} adaptive event(s) in journal:",
                    "▸".cyan(),
                    adaptive_events.len()
                );
                // Show last 10 events.
                let start = adaptive_events.len().saturating_sub(10);
                for evt in &adaptive_events[start..] {
                    let ts = chrono::DateTime::parse_from_rfc3339(&evt.ts)
                        .ok()
                        .map(|dt: chrono::DateTime<chrono::FixedOffset>| {
                            dt.format("%H:%M:%S").to_string()
                        })
                        .unwrap_or_else(|| "??:??:??".into());
                    let turn = evt.turn.unwrap_or(0);
                    match evt.event_type {
                        session_journal::JournalEventType::AdaptiveScenarioApplied => {
                            let scenario = evt
                                .metadata
                                .as_ref()
                                .and_then(|m| m.get("scenario"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("?");
                            let confidence = evt
                                .metadata
                                .as_ref()
                                .and_then(|m| m.get("confidence"))
                                .and_then(|v| v.as_f64())
                                .unwrap_or(0.0);
                            let n = evt
                                .metadata
                                .as_ref()
                                .and_then(|m| m.get("config_changes"))
                                .and_then(|v| v.as_array())
                                .map(|a| a.len())
                                .unwrap_or(0);
                            eprintln!(
                                "    {} T{:>2} {} profile → {} (conf {:.2}, {} changes)",
                                ts.dim(),
                                turn,
                                "⚙".cyan(),
                                scenario,
                                confidence,
                                n
                            );
                        }
                        session_journal::JournalEventType::AdaptivePerTurnApplied => {
                            let n = evt
                                .metadata
                                .as_ref()
                                .and_then(|m| m.get("changes"))
                                .and_then(|v| v.as_array())
                                .map(|a| a.len())
                                .unwrap_or(0);
                            let triggers = evt
                                .metadata
                                .as_ref()
                                .and_then(|m| m.get("triggers"))
                                .and_then(|v| v.as_array())
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|t| t.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                })
                                .unwrap_or_default();
                            eprintln!(
                                "    {} T{:>2} {} per-turn: {} changes ({})",
                                ts.dim(),
                                turn,
                                "↻".cyan(),
                                n,
                                triggers
                            );
                        }
                        session_journal::JournalEventType::AdaptiveExperimentEnrolled => {
                            let exp = evt
                                .metadata
                                .as_ref()
                                .and_then(|m| m.get("experiment_name"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("?");
                            let var = evt
                                .metadata
                                .as_ref()
                                .and_then(|m| m.get("variant_id"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("?");
                            eprintln!(
                                "    {} T{:>2} {} enrolled: {} → {}",
                                ts.dim(),
                                turn,
                                "🧪".cyan(),
                                exp,
                                var
                            );
                        }
                        session_journal::JournalEventType::AdaptiveTuningRuleTriggered => {
                            let rule = evt
                                .metadata
                                .as_ref()
                                .and_then(|m| m.get("rule_name"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("?");
                            eprintln!(
                                "    {} T{:>2} {} rule: {}",
                                ts.dim(),
                                turn,
                                "⚡".yellow(),
                                rule
                            );
                        }
                        session_journal::JournalEventType::AdaptiveBaselinePromoted => {
                            let scope = evt
                                .metadata
                                .as_ref()
                                .and_then(|m| m.get("task_type"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("?");
                            let var = evt
                                .metadata
                                .as_ref()
                                .and_then(|m| m.get("variant_id"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("?");
                            eprintln!(
                                "    {} T{:>2} {} baseline promoted: {} → {}",
                                ts.dim(),
                                turn,
                                "🏆".cyan(),
                                scope,
                                var
                            );
                        }
                        _ => {}
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("  {}", format!("Failed to read journal: {e}").red());
        }
    }

    eprintln!();
}

// ── Session verify / sync status ────────────────────────────────────────────

/// Analyze and display focus drift in the current session.
///
/// Usage: /session drift [--verbose]
///
/// Uses the DriftDetector to analyze the conversation history for signs of
/// focus drift caused by compression, topic shifts, or user corrections.
fn handle_session_drift(arg: &str, state: &ReplState) {
    let verbose = arg.contains("--verbose") || arg.contains("-v");

    eprintln!(
        "\n{}",
        "─── Focus Drift Analysis ─────────────────────────"
            .bold()
            .cyan()
    );

    // Collect recent user queries from history (first element of each tuple)
    let user_queries: Vec<String> = state
        .history
        .iter()
        .map(|(user_msg, _assistant_msg)| user_msg.clone())
        .collect();

    if user_queries.is_empty() {
        eprintln!("  {} No conversation history yet.", theme::icon_ok());
        eprintln!();
        return;
    }

    // Build detector inputs
    let original_query = state
        .drift_original_query
        .as_deref()
        .unwrap_or_else(|| user_queries.first().map(|s| s.as_str()).unwrap_or(""));
    let compressed_turns: Vec<u32> = state.drift_compressed_turns.clone();
    let user_corrections: Vec<u32> = state.drift_user_corrections.clone();

    // Run analysis — prefer ObservabilitySession (has trace data for richer analysis)
    let analysis: FocusDriftAnalysis = if let Some(ref obs) = state.observability_session {
        if let Ok(session) = obs.read() {
            session.check_drift_against(original_query)
        } else {
            let detector = DriftDetector::default();
            detector.analyze(
                original_query,
                &user_queries,
                &compressed_turns,
                &user_corrections,
            )
        }
    } else {
        let detector = DriftDetector::default();
        detector.analyze(
            original_query,
            &user_queries,
            &compressed_turns,
            &user_corrections,
        )
    };

    // Display results
    if analysis.drift_detected {
        eprintln!(
            "  {} Focus drift detected (severity: {:.0}%)",
            theme::icon_warn(),
            analysis.drift_severity * 100.0
        );

        if let Some(turn) = analysis.drift_turn {
            eprintln!(
                "    Likely drift began at turn {}",
                turn.to_string().yellow()
            );
        }

        // Show cause
        let cause_str = match &analysis.likely_cause {
            DriftCause::HistoryCompression { lost_context, .. } => {
                let ctx = if lost_context.is_empty() {
                    "history".to_string()
                } else if lost_context.len() <= 3 {
                    lost_context.join(", ")
                } else {
                    format!(
                        "{} and {} more",
                        lost_context[..3].join(", "),
                        lost_context.len() - 3
                    )
                };
                format!("History compression (lost: {})", ctx)
            }
            DriftCause::MemoryMiss {
                expected_but_not_retrieved,
                ..
            } => {
                if expected_but_not_retrieved.is_empty() {
                    "Memory miss (expected memories not retrieved)".to_string()
                } else {
                    format!(
                        "Memory miss (expected: {})",
                        expected_but_not_retrieved.join(", ")
                    )
                }
            }
            DriftCause::TopicShift {
                original_topic,
                new_topic,
                ..
            } => {
                format!("Topic shift ('{}' → '{}')", original_topic, new_topic)
            }
            DriftCause::TokenBudgetPressure {
                budget_available,
                budget_needed,
                ..
            } => {
                format!(
                    "Token budget pressure ({} needed vs {} available)",
                    budget_needed, budget_available
                )
            }
            DriftCause::AmbiguousInstruction { instruction, .. } => {
                format!("Ambiguous instruction: {}", instruction)
            }
            DriftCause::Unknown => "Unknown cause".to_string(),
        };
        eprintln!("    Cause: {}", cause_str.yellow());

        // Show recovery suggestion
        if !analysis.recovery_suggestion.is_empty() {
            eprintln!("\n  💡 {}", analysis.recovery_suggestion.green());
        }
    } else {
        eprintln!(
            "  {} No significant focus drift detected.",
            theme::icon_ok()
        );
    }

    // Show tracked data if verbose
    if verbose {
        eprintln!("\n  {}", "Tracked Data".dim());
        eprintln!("    {:<22} {}", "History turns:".dim(), user_queries.len());
        eprintln!(
            "    {:<22} {}",
            "Compressed turns:".dim(),
            if compressed_turns.is_empty() {
                "none".to_string()
            } else {
                format!("{:?}", compressed_turns)
            }
        );
        eprintln!(
            "    {:<22} {}",
            "User corrections:".dim(),
            if user_corrections.is_empty() {
                "none".to_string()
            } else {
                format!("{:?}", user_corrections)
            }
        );
        eprintln!(
            "    {:<22} \"{}\"",
            "Original query:".dim(),
            ellipsize(original_query, 50)
        );

        // Show evidence
        if !analysis.evidence.is_empty() {
            eprintln!("\n  {}", "Evidence".dim());
            for ev in &analysis.evidence {
                // ev is DriftEvidence { turn, evidence_type, description, confidence }
                let type_str = match &ev.evidence_type {
                    EvidenceType::ToolCallTopicChange => "topic change",
                    EvidenceType::UserCorrection => "user correction",
                    EvidenceType::ClarificationRequest => "clarification",
                    EvidenceType::TermDisappearance => "term lost",
                    EvidenceType::CompressionLoss => "compression",
                    EvidenceType::MemoryMismatch => "memory miss",
                };
                eprintln!(
                    "    • Turn {}: [{}] {} ({:.0}%)",
                    ev.turn,
                    type_str,
                    ellipsize(&ev.description, 50),
                    ev.confidence.point * 100.0
                );
            }
        }
    }

    // Show goal progress if available
    if let Some(ref obs_lock) = state.observability_session {
        if let Ok(obs) = obs_lock.read() {
            if let Some(progress) = obs.goal_progress() {
                eprintln!(
                    "\n{}",
                    "─── Goal Progress ────────────────────────────────"
                        .bold()
                        .cyan()
                );
                let pct = format!("{:.0}%", progress.completion_score * 100.0);
                let momentum_str = if progress.momentum > 0.3 {
                    "↑ positive".green().to_string()
                } else if progress.momentum < -0.3 {
                    "↓ struggling".red().to_string()
                } else {
                    "→ steady".dim().to_string()
                };
                eprintln!("  Completion: {}", pct.cyan().bold());
                eprintln!("  Momentum:   {momentum_str}");
                eprintln!("  Milestones: {}", progress.milestone_count);
                eprintln!("  {}", progress.summary.dim());
            }
        }
    }

    eprintln!();
}

// ── Session Analyze ─────────────────────────────────────────────────────────

/// `/session analyze [session_id]` — deep diagnostics for a session.
///
/// Reads the full journal + workspace and produces:
/// - Overview: model, duration, turns, total tokens
/// - Turn timeline with efficiency metrics
/// - Tool usage stats: frequency, success rate, blocked tools
/// - Token budget analysis: per-turn, cumulative, cache rate
/// - Issue detection: blocked tools, stalls, errors, latency spikes,
///   recording gaps, duplicate checkpoints
fn handle_session_analyze(arg: &str, state: &ReplState) {
    let (target_sid, resolved_prefix) = match resolve_journal_target_session(
        arg,
        state,
        "  No active session. Use /session analyze <session_id>.",
    ) {
        Ok(value) => value,
        Err(msg) => {
            eprintln!("{msg}");
            return;
        }
    };
    if resolved_prefix && !arg.is_empty() {
        eprintln!(
            "  {} Resolved {} → {}",
            theme::icon_ok(),
            arg.cyan(),
            target_sid.as_str().cyan()
        );
    }

    let events = match session_journal::read_journal(&target_sid) {
        Ok(e) if e.is_empty() => {
            eprintln!("{}", "  No journal entries.".dim());
            return;
        }
        Ok(e) => e,
        Err(e) => {
            eprintln!("{}", format!("  ✗ Failed to read journal: {e}").red());
            return;
        }
    };
    let ws = session_workspace::read_workspace(&target_sid).ok();

    // ── Collect turn events ─────────────────────────────────────────────────
    let turns: Vec<&session_journal::JournalEvent> = events
        .iter()
        .filter(|e| e.event_type == session_journal::JournalEventType::Turn)
        .collect();
    let errors: Vec<&session_journal::JournalEvent> = events
        .iter()
        .filter(|e| {
            matches!(
                e.event_type,
                session_journal::JournalEventType::TurnError
                    | session_journal::JournalEventType::Error
            )
        })
        .collect();
    let stalls: Vec<&session_journal::JournalEvent> = events
        .iter()
        .filter(|e| e.event_type == session_journal::JournalEventType::StallDetected)
        .collect();
    let checkpoints: Vec<&session_journal::JournalEvent> = events
        .iter()
        .filter(|e| e.event_type == session_journal::JournalEventType::Checkpoint)
        .collect();

    // ── Overview ────────────────────────────────────────────────────────────
    let sid_short = &target_sid[..8.min(target_sid.len())];
    eprintln!(
        "\n{}",
        format!("─── Session Analysis ({sid_short}) ──────────────────────────")
            .bold()
            .cyan()
    );

    let model = ws
        .as_ref()
        .map(|w| w.model.as_str())
        .or_else(|| events.first().and_then(|e| e.model.as_deref()))
        .unwrap_or("unknown");
    let total_tok_in: u64 = turns.iter().filter_map(|t| t.tokens_in).sum();
    let total_tok_out: u64 = turns.iter().filter_map(|t| t.tokens_out).sum();
    let total_ms: u64 = turns.iter().filter_map(|t| t.duration_ms).sum();
    let total_tools: u32 = turns.iter().filter_map(|t| t.tool_count).sum();
    let total_cache_read: u64 = turns.iter().filter_map(|t| t.cache_read_tokens).sum();
    let total_cache_create: u64 = turns.iter().filter_map(|t| t.cache_creation_tokens).sum();

    eprintln!("  {:<16} {}", "model:".dim(), model.cyan());
    eprintln!(
        "  {:<16} {} ({} prompt + {} completion)",
        "tokens:".dim(),
        format_u64_grouped(total_tok_in + total_tok_out).cyan(),
        format_u64_grouped(total_tok_in),
        format_u64_grouped(total_tok_out),
    );
    if total_cache_read > 0 || total_cache_create > 0 {
        let cache_pct = if total_tok_in > 0 {
            (total_cache_read as f64 / total_tok_in as f64 * 100.0) as u64
        } else {
            0
        };
        eprintln!(
            "  {:<16} {} read, {} created ({}% hit rate)",
            "cache:".dim(),
            format_u64_grouped(total_cache_read).green(),
            format_u64_grouped(total_cache_create),
            cache_pct,
        );
    }
    eprintln!(
        "  {:<16} {} turns, {} tool calls, {:.1}s total",
        "activity:".dim(),
        turns.len().to_string().cyan(),
        total_tools.to_string().cyan(),
        total_ms as f64 / 1000.0,
    );
    if let Some(ref w) = ws {
        if let Some(ref goal) = w.session_goal {
            let g: String = goal.chars().take(60).collect();
            eprintln!("  {:<16} {}", "goal:".dim(), g);
        }
    }

    // ── Turn Timeline ───────────────────────────────────────────────────────
    eprintln!(
        "\n{}",
        "  ── Turn Timeline ──────────────────────────────────────────".bold()
    );
    eprintln!(
        "  {:>4} {:>7} {:>8} {:>8} {:>5} {:>5}  {}",
        "Turn".dim(),
        "Time".dim(),
        "Tok-in".dim(),
        "Tok-out".dim(),
        "Tools".dim(),
        "Errs".dim(),
        "Input".dim(),
    );

    for evt in &turns {
        let turn_n = evt.turn.unwrap_or(0);
        let dur_s = evt.duration_ms.unwrap_or(0) as f64 / 1000.0;
        let tok_in = evt.tokens_in.unwrap_or(0);
        let tok_out = evt.tokens_out.unwrap_or(0);
        let tool_cnt = evt.tool_count.unwrap_or(0);

        // Count failed tool calls
        let err_cnt = evt
            .tool_calls
            .as_ref()
            .map(|calls| calls.iter().filter(|c| !c.ok).count())
            .unwrap_or(0);

        let input: String = evt
            .user_input
            .as_deref()
            .unwrap_or("")
            .chars()
            .take(40)
            .collect();

        // Color-code by severity
        let dur_str = format!("{dur_s:>6.1}s");
        let dur_colored = if dur_s > 120.0 {
            dur_str.red().to_string()
        } else if dur_s > 60.0 {
            dur_str.yellow().to_string()
        } else {
            dur_str.to_string()
        };
        let tok_str = format!("{tok_in:>8}");
        let tok_colored = if tok_in > 80_000 {
            tok_str.red().to_string()
        } else if tok_in > 40_000 {
            tok_str.yellow().to_string()
        } else {
            tok_str.to_string()
        };
        let err_str = if err_cnt > 0 {
            format!("{err_cnt:>5}").red().to_string()
        } else {
            format!("{err_cnt:>5}")
        };

        eprintln!(
            "  {:>4} {} {} {:>8} {:>5} {}  {}",
            format!("T{turn_n}"),
            dur_colored,
            tok_colored,
            tok_out,
            tool_cnt,
            err_str,
            input.dim(),
        );
    }

    // ── Tool Usage ──────────────────────────────────────────────────────────
    let mut tool_stats: std::collections::HashMap<String, (u32, u32, u64, u64)> =
        std::collections::HashMap::new(); // name -> (total, fails, total_ms, total_output_bytes)
    let mut blocked_calls: Vec<(u32, String, String)> = Vec::new(); // (turn, tool, reason)
    let mut recording_gaps: Vec<(u32, u32, usize)> = Vec::new(); // (turn, reported_count, recorded_count)

    for evt in &turns {
        let turn_n = evt.turn.unwrap_or(0);
        let reported = evt.tool_count.unwrap_or(0);
        let recorded = evt.tool_calls.as_ref().map(|c| c.len()).unwrap_or(0);
        if reported > 0 && recorded > 0 && (reported as usize) > recorded + 2 {
            recording_gaps.push((turn_n, reported, recorded));
        }

        if let Some(calls) = evt.tool_calls.as_ref() {
            for call in calls {
                let entry = tool_stats.entry(call.name.clone()).or_insert((0, 0, 0, 0));
                entry.0 += 1;
                if !call.ok {
                    entry.1 += 1;
                }
                entry.2 += call.ms;
                entry.3 += call.output_bytes.unwrap_or(0) as u64;

                if let Some(ref err) = call.error {
                    if err.starts_with("blocked_tool:") {
                        let reason: String = err
                            .strip_prefix("blocked_tool: ")
                            .unwrap_or(err)
                            .chars()
                            .take(80)
                            .collect();
                        blocked_calls.push((turn_n, call.name.clone(), reason));
                    }
                }
            }
        }
    }

    if !tool_stats.is_empty() {
        eprintln!(
            "\n{}",
            "  ── Tool Usage ─────────────────────────────────────────────".bold()
        );
        eprintln!(
            "  {:>20} {:>5} {:>5} {:>7} {:>8}  {}",
            "Tool".dim(),
            "Total".dim(),
            "Fail".dim(),
            "Avg ms".dim(),
            "Output".dim(),
            "Rate".dim(),
        );

        let mut sorted_tools: Vec<_> = tool_stats.iter().collect();
        sorted_tools.sort_by(|a, b| b.1.0.cmp(&a.1.0));

        for (name, (total, fails, total_ms, total_bytes)) in &sorted_tools {
            let avg_ms = if *total > 0 {
                total_ms / *total as u64
            } else {
                0
            };
            let rate = if *total > 0 {
                ((*total - fails) as f64 / *total as f64 * 100.0) as u32
            } else {
                0
            };
            let rate_str = format!("{rate}%");
            let rate_colored = if rate < 50 {
                rate_str.red().to_string()
            } else if rate < 80 {
                rate_str.yellow().to_string()
            } else {
                rate_str.green().to_string()
            };
            let bytes_str = if *total_bytes > 1_000_000 {
                format!("{:.1}MB", *total_bytes as f64 / 1_000_000.0)
            } else if *total_bytes > 1_000 {
                format!("{:.1}KB", *total_bytes as f64 / 1_000.0)
            } else {
                format!("{}B", total_bytes)
            };
            let fail_str = if *fails > 0 {
                format!("{:>5}", fails).red().to_string()
            } else {
                format!("{:>5}", fails)
            };
            let name_short: String = name.chars().take(20).collect();
            eprintln!(
                "  {:>20} {:>5} {} {:>6}ms {:>8}  {}",
                name_short, total, fail_str, avg_ms, bytes_str, rate_colored,
            );
        }
    }

    // ── Issue Detection ─────────────────────────────────────────────────────
    let mut issues: Vec<String> = Vec::new();

    // Blocked tool calls
    if !blocked_calls.is_empty() {
        let mut by_tool: std::collections::HashMap<String, Vec<u32>> =
            std::collections::HashMap::new();
        for (turn, tool, _reason) in &blocked_calls {
            by_tool.entry(tool.clone()).or_default().push(*turn);
        }
        for (tool, turns_list) in &by_tool {
            let turns_str: Vec<String> = turns_list.iter().map(|t| format!("T{t}")).collect();
            issues.push(format!(
                "🚫 {} blocked {} time(s) in {}",
                tool.as_str().red(),
                turns_list.len(),
                turns_str.join(", "),
            ));
        }
    }

    // Recording gaps (skill opacity)
    for &(turn, reported, recorded) in &recording_gaps {
        issues.push(format!(
            "👁 T{turn}: {} tool calls reported but only {} recorded (skill opacity: {} invisible)",
            reported,
            recorded,
            reported as usize - recorded,
        ));
    }

    // Stalls
    for evt in &stalls {
        let turn = evt.turn.unwrap_or(0);
        let stype = evt.stall_type.as_deref().unwrap_or("unknown");
        issues.push(format!("⚠ T{turn}: stall detected ({stype})"));
    }

    // Errors
    for evt in &errors {
        let turn = evt.turn.unwrap_or(0);
        let err: String = evt
            .error
            .as_deref()
            .unwrap_or("?")
            .chars()
            .take(80)
            .collect();
        issues.push(format!("❌ T{turn}: {err}"));
    }

    // High latency turns (>120s)
    for evt in &turns {
        let turn = evt.turn.unwrap_or(0);
        let dur_s = evt.duration_ms.unwrap_or(0) as f64 / 1000.0;
        if dur_s > 120.0 {
            let tok = evt.tokens_in.unwrap_or(0);
            let tools = evt.tool_count.unwrap_or(0);
            issues.push(format!(
                "🐌 T{turn}: {dur_s:.0}s latency ({} prompt tokens, {tools} tool calls)",
                format_u64_grouped(tok),
            ));
        }
    }

    // Duplicate checkpoints (same turn, multiple checkpoints)
    let mut ckpt_by_turn: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    for evt in &checkpoints {
        *ckpt_by_turn.entry(evt.turn.unwrap_or(0)).or_insert(0) += 1;
    }
    for (turn, count) in &ckpt_by_turn {
        if *count > 1 {
            issues.push(format!(
                "📌 T{turn}: {count} checkpoints (likely duplicated step_recorder + interval)"
            ));
        }
    }

    // High prompt token growth (>50K per turn after T1)
    let mut prev_tok_in: u64 = 0;
    for evt in &turns {
        let turn = evt.turn.unwrap_or(0);
        let tok_in = evt.tokens_in.unwrap_or(0);
        if turn > 1 && tok_in > prev_tok_in + 20_000 && tok_in > 60_000 {
            issues.push(format!(
                "📈 T{turn}: prompt tokens jumped to {} (+{})",
                format_u64_grouped(tok_in),
                format_u64_grouped(tok_in.saturating_sub(prev_tok_in)),
            ));
        }
        prev_tok_in = tok_in;
    }

    if !issues.is_empty() {
        eprintln!(
            "\n{}",
            format!(
                "  ── Issues ({}) ────────────────────────────────────────────",
                issues.len()
            )
            .bold()
            .yellow()
        );
        for issue in &issues {
            eprintln!("  {issue}");
        }
    } else {
        eprintln!("\n  {} {}", theme::icon_ok(), "No issues detected.".green());
    }

    // ── Efficiency Summary ──────────────────────────────────────────────────
    eprintln!(
        "\n{}",
        "  ── Efficiency ─────────────────────────────────────────────".bold()
    );

    let tok_per_tool = if total_tools > 0 {
        total_tok_in / total_tools as u64
    } else {
        0
    };
    eprintln!(
        "  {:<24} {} tokens/tool-call",
        "prompt efficiency:".dim(),
        format_u64_grouped(tok_per_tool),
    );

    let tok_per_turn = if !turns.is_empty() {
        total_tok_in / turns.len() as u64
    } else {
        0
    };
    eprintln!(
        "  {:<24} {} tokens/turn",
        "avg prompt per turn:".dim(),
        format_u64_grouped(tok_per_turn),
    );

    let avg_turn_ms = if !turns.is_empty() {
        total_ms / turns.len() as u64
    } else {
        0
    };
    eprintln!(
        "  {:<24} {:.1}s",
        "avg turn latency:".dim(),
        avg_turn_ms as f64 / 1000.0,
    );

    // Tool selection strategy distribution
    let mut strategy_counts: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();
    for evt in &turns {
        if let Some(ref strat) = evt.selector_strategy {
            *strategy_counts.entry(strat.clone()).or_insert(0) += 1;
        }
    }
    if !strategy_counts.is_empty() {
        let strat_parts: Vec<String> = strategy_counts
            .iter()
            .map(|(s, c)| format!("{s}×{c}"))
            .collect();
        eprintln!(
            "  {:<24} {}",
            "tool selection:".dim(),
            strat_parts.join(", "),
        );
    }

    // Budget pressure distribution
    let pressures: Vec<f64> = turns.iter().filter_map(|e| e.budget_pressure).collect();
    if !pressures.is_empty() {
        let max_p = pressures.iter().cloned().fold(0.0_f64, f64::max);
        let avg_p = pressures.iter().sum::<f64>() / pressures.len() as f64;
        let pressure_str = format!("avg {avg_p:.2}, max {max_p:.2}");
        let pressure_colored = if max_p > 0.7 {
            pressure_str.red().to_string()
        } else if max_p > 0.4 {
            pressure_str.yellow().to_string()
        } else {
            pressure_str.green().to_string()
        };
        eprintln!("  {:<24} {}", "budget pressure:".dim(), pressure_colored,);
    }

    eprintln!();
}

/// Show local journal vs cloud ingestion sync health.
fn handle_session_verify(state: &ReplState) {
    let sid = state.session_id.as_deref().unwrap_or("none");
    eprintln!(
        "\n{}",
        "─── Sync Health ─────────────────────────────────"
            .bold()
            .cyan()
    );

    // Local journal stats
    let journal_events = if sid != "none" {
        session_journal::read_journal(sid)
            .map(|evts| evts.len())
            .unwrap_or(0)
    } else {
        0
    };
    let journal_path = if sid != "none" {
        let p = session_journal::journal_file_path(sid);
        if p.exists() {
            let bytes = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            format!(
                "{} ({})",
                tilde_path(&p.display().to_string()),
                human_bytes(bytes)
            )
        } else {
            "not found".to_string()
        }
    } else {
        "—".to_string()
    };

    eprintln!("  {}", "Local journal".dim());
    eprintln!(
        "    {:<20} {}",
        "events:".dim(),
        journal_events.to_string().cyan()
    );
    eprintln!("    {:<20} {}", "file:".dim(), journal_path.dim());

    // Cloud ingestion stats
    if let Some(ref mc) = state.matrix_runtime {
        eprintln!();
        eprintln!("  {}", "Cloud ingestion".dim());
        if let Some(stats) = mc.ingestion_stats() {
            let lag = stats.events_received.saturating_sub(stats.events_flushed);
            eprintln!(
                "    {:<20} {}",
                "received:".dim(),
                stats.events_received.to_string().cyan()
            );
            eprintln!(
                "    {:<20} {}",
                "flushed:".dim(),
                stats.events_flushed.to_string().cyan()
            );
            eprintln!(
                "    {:<20} {}",
                "flushes:".dim(),
                stats.flush_count.to_string().cyan()
            );
            let overflow = mc.ingestion_overflow_count();
            if lag > 0 {
                eprintln!("    {:<20} {}", "pending:".dim(), lag.to_string().yellow());
            } else {
                eprintln!("    {:<20} {}", "pending:".dim(), "0 (synced)".green());
            }
            if overflow > 0 {
                eprintln!(
                    "    {:<20} {}",
                    "dropped:".dim(),
                    overflow.to_string().red()
                );
            }
            if stats.errors > 0 {
                eprintln!(
                    "    {:<20} {}",
                    "errors:".dim(),
                    stats.errors.to_string().red()
                );
                if let Some(ref last_err) = stats.last_error {
                    let truncated = if last_err.len() > 80 {
                        format!("{}…", last_err.chars().take(80).collect::<String>())
                    } else {
                        last_err.clone()
                    };
                    eprintln!("    {:<20} {}", "last error:".dim(), truncated.red());
                }
            }
        } else {
            eprintln!("    {}", "stats unavailable (lock contention)".dim());
        }
    } else {
        eprintln!();
        eprintln!("  {} Cloud not connected", theme::icon_warn());
    }

    // Session disk usage summary
    if sid != "none" {
        let sessions_dir = session_journal::local_sessions_dir();
        let all_sessions = session_journal::list_sessions().unwrap_or_default();
        let total_journals: u64 = all_sessions
            .iter()
            .filter_map(|s| {
                std::fs::metadata(sessions_dir.join(format!("{s}.jsonl")))
                    .ok()
                    .map(|m| m.len())
            })
            .sum();
        let compressed: usize = std::fs::read_dir(&sessions_dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl.gz"))
                    .count()
            })
            .unwrap_or(0);

        eprintln!();
        eprintln!("  {}", "Disk".dim());
        eprintln!(
            "    {:<20} {} active, {} archived",
            "sessions:".dim(),
            all_sessions.len().to_string().cyan(),
            compressed.to_string().cyan()
        );
        eprintln!(
            "    {:<20} {}",
            "journal total:".dim(),
            human_bytes(total_journals).cyan()
        );
    }

    eprintln!();
}

#[cfg(test)]
mod session_display_tests {
    use super::format_u64_grouped;

    #[test]
    fn format_u64_grouped_commas() {
        assert_eq!(format_u64_grouped(0), "0");
        assert_eq!(format_u64_grouped(999), "999");
        assert_eq!(format_u64_grouped(1000), "1,000");
        assert_eq!(format_u64_grouped(12_345_678), "12,345,678");
    }
}

#[cfg(test)]
mod export_tests {
    use super::*;
    use astra_services::session_journal::{JournalEvent, ToolCallRecord};

    /// Construct a JournalEvent from a JSON value — avoids listing all fields.
    fn evt_from_json(json: serde_json::Value) -> JournalEvent {
        serde_json::from_value(json).expect("valid JournalEvent JSON")
    }

    #[test]
    fn build_export_includes_session_start() {
        let evt = evt_from_json(serde_json::json!({
            "type": "session_start",
            "ts": "2025-01-15T10:30:00Z",
            "model": "gpt-4o",
        }));
        let md = build_export_markdown("abc123", &[evt]);
        assert!(md.contains("# Session: abc123"));
        assert!(md.contains("## Session Start"));
        assert!(md.contains("gpt-4o"));
    }

    #[test]
    fn build_export_turn_with_tool_calls() {
        let evt = evt_from_json(serde_json::json!({
            "type": "turn",
            "ts": "2025-01-15T10:31:00Z",
            "turn": 1,
            "duration_ms": 1500,
            "tokens_in": 100,
            "tokens_out": 50,
            "tool_count": 2,
            "user_input": "Hello",
            "assistant_output": "Hi there",
            "tool_calls": [
                {
                    "name": "read_file",
                    "ok": true,
                    "ms": 50,
                    "args_preview": "src/main.rs",
                    "result_preview": "fn main() { ... }",
                },
                {
                    "name": "bash",
                    "ok": false,
                    "ms": 200,
                    "error": "exit code 1",
                    "args_preview": "cargo test",
                },
            ],
        }));

        let md = build_export_markdown("test-sid", &[evt]);
        assert!(md.contains("### Turn 1"));
        assert!(md.contains("**User:**"));
        assert!(md.contains("Hello"));
        assert!(md.contains("<details>"));
        assert!(md.contains("`Reading: src/main.rs` ✓"));
        assert!(md.contains("`$ cargo test` ✗"));
        assert!(md.contains("exit code 1"));
        assert!(md.contains("**Assistant:**"));
        assert!(md.contains("Hi there"));
    }

    #[test]
    fn build_export_turn_without_tool_calls_omits_details() {
        let evt = evt_from_json(serde_json::json!({
            "type": "turn",
            "ts": "2025-01-15T10:31:00Z",
            "turn": 1,
            "user_input": "hi",
            "assistant_output": "hello",
        }));

        let md = build_export_markdown("sid", &[evt]);
        assert!(!md.contains("<details>"));
        assert!(md.contains("hello"));
    }

    #[test]
    fn format_tool_calls_md_produces_collapsed_block() {
        let calls = vec![ToolCallRecord {
            name: "grep".into(),
            ok: true,
            ms: 10,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: Some("pattern in src/".into()),
            result_preview: None,
        }];
        let block = format_tool_calls_md(&calls);
        assert!(block.contains("<details>"));
        assert!(block.contains("</details>"));
        assert!(block.contains("`Grep: pattern in src/` ✓ (10ms)"));
    }

    // ── /export edge case tests ──

    #[test]
    fn build_export_empty_events_only_header() {
        let md = build_export_markdown("empty-sid", &[]);
        assert!(md.contains("# Session: empty-sid"));
        // Should not contain any section headers
        assert!(!md.contains("## Session Start"));
        assert!(!md.contains("### Turn"));
    }

    #[test]
    fn build_export_turn_error_event() {
        let evt = evt_from_json(serde_json::json!({
            "type": "turn_error",
            "ts": "2025-01-15T10:31:00Z",
            "turn": 3,
            "error": "rate limit exceeded",
        }));
        let md = build_export_markdown("sid", &[evt]);
        assert!(md.contains("Turn 3 ❌ Error"));
        assert!(md.contains("rate limit exceeded"));
    }

    #[test]
    fn build_export_compact_event() {
        let evt = evt_from_json(serde_json::json!({
            "type": "compact",
            "ts": "2025-01-15T10:35:00Z",
            "turns_compacted": 8,
            "facts_stored": 2,
        }));
        let md = build_export_markdown("sid", &[evt]);
        assert!(md.contains("### Compact"));
        assert!(md.contains("Turns compacted:** 8"));
        assert!(md.contains("Facts stored:** 2"));
    }

    #[test]
    fn build_export_config_change_event() {
        let evt = evt_from_json(serde_json::json!({
            "type": "config_change",
            "ts": "2025-01-15T10:33:00Z",
            "config_key": "model",
            "config_value": "gpt-4o-mini",
        }));
        let md = build_export_markdown("sid", &[evt]);
        assert!(md.contains("⚙️"));
        assert!(md.contains("model → gpt-4o-mini"));
    }

    #[test]
    fn build_export_session_end_event() {
        let evt = evt_from_json(serde_json::json!({
            "type": "session_end",
            "ts": "2025-01-15T11:00:00Z",
            "turn": 15,
        }));
        let md = build_export_markdown("sid", &[evt]);
        assert!(md.contains("## Session End"));
        assert!(md.contains("Total turns:** 15"));
    }

    #[test]
    fn build_export_sync_marker_event() {
        let evt = evt_from_json(serde_json::json!({
            "type": "sync_marker",
            "ts": "2025-01-15T10:40:00Z",
            "user_input": "manual checkpoint",
        }));
        let md = build_export_markdown("sid", &[evt]);
        assert!(md.contains("### Sync marker"));
        assert!(md.contains("manual checkpoint"));
    }

    #[test]
    fn build_export_non_ascii_content_preserved() {
        let evt = evt_from_json(serde_json::json!({
            "type": "turn",
            "ts": "2025-01-15T10:31:00Z",
            "turn": 1,
            "user_input": "请帮我修改代码 🔧",
            "assistant_output": "好的，我已经修改了。",
        }));
        let md = build_export_markdown("sid", &[evt]);
        assert!(md.contains("请帮我修改代码 🔧"));
        assert!(md.contains("好的，我已经修改了。"));
    }

    #[test]
    fn build_export_multiple_event_types_in_order() {
        let events = vec![
            evt_from_json(serde_json::json!({
                "type": "session_start",
                "ts": "2025-01-15T10:30:00Z",
                "model": "gpt-4o",
            })),
            evt_from_json(serde_json::json!({
                "type": "turn",
                "ts": "2025-01-15T10:31:00Z",
                "turn": 1,
                "user_input": "hello",
                "assistant_output": "world",
            })),
            evt_from_json(serde_json::json!({
                "type": "turn_error",
                "ts": "2025-01-15T10:32:00Z",
                "turn": 2,
                "error": "timeout",
            })),
            evt_from_json(serde_json::json!({
                "type": "compact",
                "ts": "2025-01-15T10:33:00Z",
                "turns_compacted": 5,
                "facts_stored": 1,
            })),
            evt_from_json(serde_json::json!({
                "type": "session_end",
                "ts": "2025-01-15T11:00:00Z",
                "turn": 10,
            })),
        ];
        let md = build_export_markdown("multi", &events);
        // Check order: session_start before turns before session_end
        let start_pos = md.find("## Session Start").unwrap();
        let turn_pos = md.find("### Turn 1").unwrap();
        let error_pos = md.find("Turn 2 ❌").unwrap();
        let compact_pos = md.find("### Compact").unwrap();
        let end_pos = md.find("## Session End").unwrap();
        assert!(start_pos < turn_pos);
        assert!(turn_pos < error_pos);
        assert!(error_pos < compact_pos);
        assert!(compact_pos < end_pos);
    }

    #[test]
    fn build_export_turn_with_empty_user_input_omits_user() {
        let evt = evt_from_json(serde_json::json!({
            "type": "turn",
            "ts": "2025-01-15T10:31:00Z",
            "turn": 1,
            "user_input": "",
            "assistant_output": "some output",
        }));
        let md = build_export_markdown("sid", &[evt]);
        assert!(!md.contains("**User:**"));
        assert!(md.contains("**Assistant:**"));
    }

    #[test]
    fn build_export_ts_truncation() {
        // Timestamps longer than 19 chars are truncated
        let evt = evt_from_json(serde_json::json!({
            "type": "session_start",
            "ts": "2025-01-15T10:30:00.123456789Z",
            "model": "test",
        }));
        let md = build_export_markdown("sid", &[evt]);
        assert!(md.contains("2025-01-15T10:30:00"));
        // Should not include the fractional seconds
        assert!(!md.contains(".123456789Z"));
    }
}

// ═══════════════════════════════════════════════════════════ Resume ═══════

// ═══════════════════════════════════════════════════════════ Resume ═══════

pub(super) async fn handle_resume_command(arg: &str, profile: Option<&str>, state: &mut ReplState) {
    use astra_services::session_restore::{HybridRestoreService, SessionRestoreService};

    let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");
    let svc = match &state.matrix_runtime {
        Some(mc) => HybridRestoreService::new(mc.shared_pool().get().clone()),
        None => HybridRestoreService::local_only(),
    };

    // If no session_id given, list and let user pick
    let effective_arg;
    if arg.is_empty() {
        // Merge cloud + local sessions, deduplicate, sort by recency
        let cloud_sessions = svc
            .list_resumable_sessions(user_id)
            .await
            .unwrap_or_default();
        let local_ids = session_journal::list_sessions_by_time(20).unwrap_or_default();

        // Build merged map: session_id → RestoredSession (cloud wins on metadata)
        let mut merged: std::collections::HashMap<
            String,
            astra_services::session_restore::RestoredSession,
        > = std::collections::HashMap::new();

        // Insert local sessions first (lower priority)
        for sid in &local_ids {
            merged.entry(sid.clone()).or_insert_with(|| {
                astra_services::session_restore::RestoredSession {
                    session_id: sid.clone(),
                    turn_count: session_journal::count_turns(sid),
                    last_status: "local".to_string(),
                    ..Default::default()
                }
            });
        }

        // Cloud sessions override local (richer metadata: title, turn_count, status)
        for s in cloud_sessions {
            merged.insert(s.session_id.clone(), s);
        }

        // Sort by local file order (newest first), cloud-only sessions appended at front
        let mut result: Vec<_> = Vec::new();
        let mut seen = std::collections::HashSet::new();

        // Local order first (already sorted by mtime)
        for sid in &local_ids {
            if let Some(s) = merged.remove(sid) {
                seen.insert(sid.clone());
                result.push(s);
            }
        }
        // Remaining cloud-only sessions at the front (they're newer if not local)
        let mut cloud_only: Vec<_> = merged.into_values().collect();
        cloud_only.sort_by(|a, b| b.turn_count.cmp(&a.turn_count));
        result.splice(0..0, cloud_only);

        // Filter out empty sessions (0 turns = nothing to resume)
        result.retain(|s| s.turn_count > 0);

        if result.is_empty() {
            eprintln!("{}", "  No resumable sessions found.".dim());
            return;
        }

        let sessions = &result[..result.len().min(10)];

        // Enrich with local metadata (workspace + journal peek) — fast, no DB
        struct SessionDisplay {
            idx: usize,
            session_id: String,
            title: Option<String>,
            first_prompt: Option<String>,
            turn_count: u32,
            model: Option<String>,
            cwd_short: Option<String>,
            git_branch: Option<String>,
            source: String,
            has_plan: bool,
            age: String,
        }

        let mut items: Vec<SessionDisplay> = Vec::new();
        for (i, s) in sessions.iter().enumerate() {
            let peek = session_journal::peek_session_meta(&s.session_id);
            let ws = astra_services::session_workspace::read_workspace(&s.session_id).ok();

            // Title: cloud title > workspace summary > first prompt preview
            let title = s
                .title
                .clone()
                .or_else(|| ws.as_ref().and_then(|w| w.summary.clone()))
                .or_else(|| peek.as_ref().and_then(|p| p.first_prompt.clone()));

            let first_prompt = peek.as_ref().and_then(|p| p.first_prompt.clone());

            // Model: cloud > workspace > journal peek
            let model = s
                .model
                .clone()
                .or_else(|| ws.as_ref().map(|w| w.model.clone()))
                .or_else(|| peek.as_ref().and_then(|p| p.model.clone()));

            // cwd: shorten to last 2 path components
            let cwd_short = ws.as_ref().map(|w| {
                let parts: Vec<&str> = w.cwd.split('/').filter(|s| !s.is_empty()).collect();
                if parts.len() <= 2 {
                    w.cwd.clone()
                } else {
                    format!("…/{}", parts[parts.len() - 2..].join("/"))
                }
            });

            let git_branch = s
                .git_branch
                .clone()
                .or_else(|| ws.as_ref().and_then(|w| w.git_branch.clone()));

            let source = if s.restored_from_cloud {
                "☁".to_string()
            } else if s.last_status == "local" {
                "⊙".to_string()
            } else {
                s.last_status.clone()
            };

            let has_plan = ws.as_ref().is_some_and(|w| w.executing_plan_json.is_some());

            // Age: from workspace or journal timestamp
            let age = ws
                .as_ref()
                .map(|w| &w.updated_at)
                .or_else(|| peek.as_ref().and_then(|p| p.created_at.as_ref()))
                .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                .map(|dt| {
                    let dur = chrono::Utc::now().signed_duration_since(dt);
                    if dur.num_minutes() < 60 {
                        format!("{}m ago", dur.num_minutes())
                    } else if dur.num_hours() < 24 {
                        format!("{}h ago", dur.num_hours())
                    } else {
                        format!("{}d ago", dur.num_days())
                    }
                })
                .unwrap_or_default();

            items.push(SessionDisplay {
                idx: i + 1,
                session_id: s.session_id.clone(),
                title,
                first_prompt,
                turn_count: s.turn_count,
                model,
                cwd_short,
                git_branch,
                source,
                has_plan,
                age,
            });
        }

        eprintln!(
            "\n{}",
            "─── Resumable Sessions ──────────────────────────".bold()
        );
        for s in &items {
            // Line 1: [N]  title or first prompt  (age)
            let display_text = s
                .title
                .as_deref()
                .or(s.first_prompt.as_deref())
                .unwrap_or("(no prompt)");
            let display_truncated: String = display_text.chars().take(60).collect();
            let plan_badge = if s.has_plan { " 📋" } else { "" };
            eprintln!(
                "  {}  {}{}  {}",
                format!("[{}]", s.idx).cyan().bold(),
                display_truncated,
                plan_badge,
                s.age.as_str().dim(),
            );
            // Line 2: context details
            let short_id = &s.session_id[..8.min(s.session_id.len())];
            let model_str = s.model.as_deref().unwrap_or("?");
            let branch_str = s
                .git_branch
                .as_deref()
                .map(|b| format!(" {b}"))
                .unwrap_or_default();
            let cwd_str = s.cwd_short.as_deref().unwrap_or("");
            eprintln!(
                "      {} {} {} turns · {}{} {}",
                s.source.as_str().dim(),
                short_id.dim(),
                s.turn_count,
                model_str.dim(),
                branch_str.dim(),
                cwd_str.dim(),
            );
        }
        eprintln!();
        eprint!("  {} ", "Select (number or Enter to cancel):".bold());
        std::io::Write::flush(&mut std::io::stderr()).ok();
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_ok() {
            if let Ok(n) = input.trim().parse::<usize>() {
                if n >= 1 && n <= sessions.len() {
                    effective_arg = sessions[n - 1].session_id.clone();
                } else {
                    eprintln!("{}", "  Cancelled.".dim());
                    return;
                }
            } else {
                eprintln!("{}", "  Cancelled.".dim());
                return;
            }
        } else {
            return;
        }
    } else {
        effective_arg = arg.to_string();
    }
    let arg = effective_arg.as_str();

    // Resolve prefix via local journal first
    let session_id = match session_journal::resolve_session_id(arg) {
        Ok(resolved) => {
            if resolved != arg {
                eprintln!(
                    "  {} Resolved {} → {}",
                    theme::icon_ok(),
                    arg.cyan(),
                    resolved.as_str().cyan()
                );
            }
            resolved
        }
        Err(_) => arg.to_string(),
    };

    // Preview session before confirming resume (unless --yes flag or interactive picker was used)
    let skip_preview = std::env::var("ASTRA_RESUME_SKIP_PREVIEW")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false);

    // Show preview if user explicitly typed a session ID (not from picker)
    if !skip_preview && !arg.is_empty() {
        // Show session preview
        let ws = session_workspace::read_workspace(&session_id).ok();
        let peek = session_journal::peek_session_meta(&session_id);

        eprintln!(
            "\n{}",
            "─── Session Preview ─────────────────────────────"
                .bold()
                .cyan()
        );

        // Session ID
        let short_id = &session_id[..8.min(session_id.len())];
        eprintln!(
            "  {:<14} {}",
            "session:".dim(),
            format!("{short_id}…").cyan()
        );

        // Summary/Title
        let summary = ws
            .as_ref()
            .and_then(|w| w.summary.clone())
            .or_else(|| peek.as_ref().and_then(|p| p.first_prompt.clone()))
            .map(|s| {
                let truncated: String = s.chars().take(70).collect();
                if s.chars().count() > 70 {
                    format!("{truncated}…")
                } else {
                    truncated
                }
            })
            .unwrap_or_else(|| "(no summary)".to_string());
        eprintln!("  {:<14} {}", "summary:".dim(), summary);

        // Turn count
        if let Some(ref w) = ws {
            eprintln!(
                "  {:<14} {} turns",
                "progress:".dim(),
                w.turn_count.to_string().cyan()
            );
        } else if peek.is_some() {
            let turns = session_journal::count_turns(&session_id);
            eprintln!(
                "  {:<14} {} turns",
                "progress:".dim(),
                turns.to_string().cyan()
            );
        }

        // Model
        let model = ws
            .as_ref()
            .map(|w| w.model.clone())
            .or_else(|| peek.as_ref().and_then(|p| p.model.clone()))
            .unwrap_or_else(|| "?".to_string());
        eprintln!("  {:<14} {}", "model:".dim(), model.cyan());

        // Cwd + git branch
        if let Some(ref w) = ws {
            eprintln!(
                "  {:<14} {}",
                "directory:".dim(),
                tilde_path(&w.cwd).as_str().cyan()
            );
            if let Some(ref b) = w.git_branch {
                let head = w
                    .git_head
                    .as_deref()
                    .map(|h| &h[..7.min(h.len())])
                    .unwrap_or("");
                eprintln!(
                    "  {:<14} {} @ {}",
                    "branch:".dim(),
                    b.as_str().cyan(),
                    head.dim()
                );
            }
        }

        // Status
        if let Some(ref w) = ws {
            let status_icon = match w.status.as_str() {
                "active" => "🔄",
                "completed" => "✅",
                "error" => "❌",
                _ => "•",
            };
            eprintln!(
                "  {:<14} {} {}",
                "status:".dim(),
                status_icon,
                w.status.as_str().cyan()
            );
        }

        // Age
        let age = ws
            .as_ref()
            .map(|w| &w.updated_at)
            .or_else(|| peek.as_ref().and_then(|p| p.created_at.as_ref()))
            .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
            .map(|dt| {
                let dur = chrono::Utc::now().signed_duration_since(dt);
                if dur.num_minutes() < 60 {
                    format!("{}m ago", dur.num_minutes())
                } else if dur.num_hours() < 24 {
                    format!("{}h ago", dur.num_hours())
                } else {
                    format!("{}d ago", dur.num_days())
                }
            });
        if let Some(age) = age {
            eprintln!("  {:<14} {}", "last active:".dim(), age.dim());
        }

        // Plan status
        if let Some(ref w) = ws {
            if w.plan_goal.is_some() {
                let goal = w.plan_goal.as_deref().unwrap_or("");
                let goal_short: String = goal.chars().take(50).collect();
                eprintln!("  {:<14} 📋 {}", "plan:".dim(), goal_short.yellow());
            }
        }

        eprintln!();

        // Confirm
        eprint!("  {} ", "Resume this session? [Y/n]:".bold());
        std::io::Write::flush(&mut std::io::stderr()).ok();
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_ok() {
            let answer = input.trim().to_lowercase();
            if !answer.is_empty() && answer != "y" && answer != "yes" {
                eprintln!("{}", "  Cancelled.".dim());
                return;
            }
        } else {
            return;
        }
    }

    // Restore session
    match svc.restore_session(&session_id).await {
        Ok(Some(restored)) => {
            // Issue 1: Verify session belongs to current user
            // For cloud restore, the session should already have user_id check done in DB query
            // For local restore, we verify the session exists in user's journal
            if !restored.restored_from_cloud {
                // Local restore: verify user owns this session by checking journal exists
                if session_journal::read_journal(&session_id).is_err() {
                    eprintln!(
                        "{}",
                        format!(
                            "  {} Session {} not found or not owned by user",
                            theme::icon_err(),
                            arg
                        )
                        .red()
                    );
                    return;
                }
            }

            // Apply restored state
            state.session_id = Some(restored.session_id.clone());
            state.turn = restored.turn_count;
            state.total_prompt_tokens = restored.total_tokens_in;
            state.total_completion_tokens = restored.total_tokens_out;
            state.recent_tools = restored.recent_tools;

            // Merge step checkpoint data when the on-disk checkpoint matches current protocol.
            if let Ok(Some(step_restored)) =
                astra_runtime::pipeline::step_restore::restore_session(&restored.session_id)
            {
                let summary =
                    astra_runtime::pipeline::step_restore::restore_summary(&step_restored);
                // Merge blocked tools from checkpoint into health entries
                for tool in &step_restored.blocked_tools {
                    if !state.tool_health_entries.iter().any(|e| e.name == *tool) {
                        state.tool_health_entries.push(
                            astra_runtime::pipeline::persistence::ToolHealthEntry {
                                name: tool.clone(),
                                total_calls: 3,
                                total_failures: 3,
                                failure_rate: 1.0,
                                last_updated_epoch: 0, // synthetic — will be overridden by real data
                            },
                        );
                    }
                }
                if state.recent_tools.is_empty() {
                    state.recent_tools = step_restored.recent_tools;
                }
                eprintln!("  {} {}", "↻".cyan(), summary.dim());
            } else if let Ok(Some(heavy)) =
                astra_runtime::pipeline::step_checkpoint::read_latest_heavy_checkpoint(
                    &restored.session_id,
                )
            {
                // Fallback to raw local checkpoint if step_restore fails (e.g., version mismatch)
                if state.recent_tools.is_empty() {
                    state.recent_tools = heavy.recent_tools;
                }
            } else if let Some(ref mc) = state.matrix_runtime {
                // Cloud fallback: pull heavy checkpoint from MatrixOne
                // (different device, local files not available)
                let pool = mc.shared_pool().get();
                match astra_services::session_restore::pull_step_checkpoint_from_cloud(
                    pool,
                    &restored.session_id,
                )
                .await
                {
                    Ok(Some(state_json)) => {
                        match serde_json::from_str::<
                            astra_runtime::pipeline::step_protocol::StepCheckpoint,
                        >(&state_json)
                        {
                            Ok(astra_runtime::pipeline::step_protocol::StepCheckpoint::Heavy(
                                heavy,
                            )) => {
                                for tool in &heavy.blocked_tools {
                                    if !state.tool_health_entries.iter().any(|e| e.name == *tool) {
                                        state.tool_health_entries.push(
                                            astra_runtime::pipeline::persistence::ToolHealthEntry {
                                                name: tool.clone(),
                                                total_calls: 3,
                                                total_failures: 3,
                                                failure_rate: 1.0,
                                                last_updated_epoch: 0,
                                            },
                                        );
                                    }
                                }
                                if state.recent_tools.is_empty() {
                                    state.recent_tools = heavy.recent_tools;
                                }
                                // Restore conversation history from cloud checkpoint
                                if state.history.is_empty() && !heavy.messages.is_empty() {
                                    // Extract user/assistant pairs from messages for history
                                    let mut pairs = Vec::new();
                                    let mut last_user = String::new();
                                    for msg in &heavy.messages {
                                        let role =
                                            msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
                                        let content = msg
                                            .get("content")
                                            .and_then(|c| c.as_str())
                                            .unwrap_or("");
                                        match role {
                                            "user" => last_user = content.to_string(),
                                            "assistant" if !last_user.is_empty() => {
                                                pairs
                                                    .push((last_user.clone(), content.to_string()));
                                                last_user.clear();
                                            }
                                            _ => {}
                                        }
                                    }
                                    if !pairs.is_empty() {
                                        state.history = pairs;
                                    }
                                }
                                eprintln!("  {} Restored step checkpoint from cloud", "☁".cyan());
                            }
                            Ok(_) => {} // Light checkpoint — less useful, skip
                            Err(e) => {
                                eprintln!(
                                    "  {} Cloud checkpoint corrupted, skipping",
                                    theme::icon_warn()
                                );
                                eprintln!("{}", format!("     ({e})").dim());
                            }
                        }
                    }
                    Ok(None) => {} // No cloud checkpoint available
                    Err(e) => {
                        eprintln!("  {} Cloud checkpoint unavailable", theme::icon_warn());
                        eprintln!("{}", format!("     ({e})").dim());
                    }
                }
            }

            if let Some(ref m) = restored.model {
                state.model = Some(m.clone());
                state.cached_pricing = slash_stats::fallback_pricing(m);
                // M3: Use RuntimeConfig-driven context budget on session restore
                state.context_budget =
                    prompts::ContextBudget::from_runtime_config(&state.runtime_config, Some(m));
            }

            // Store learning snapshot for merge after handler returns
            // (pipeline modules are only accessible in run_chat_repl)
            if let Some(ref learning_json) = restored.learning_snapshot_json
                && !learning_json.is_empty()
            {
                state.learning_snapshot = Some(learning_json.clone());
            }

            // Issue 3: Restore conversation history from local journal
            // restore_history_from_journal already handles session segmentation (only reads after latest session_start)
            state.history = repl_runtime::restore_history_from_journal(&session_id);

            // Restore last turn event for /turn command
            if let Ok(events) = session_journal::read_journal(&session_id) {
                state.last_turn_event = events
                    .iter()
                    .rev()
                    .find(|e| e.event_type == session_journal::JournalEventType::Turn)
                    .cloned();
            }

            // Restore plan execution state from workspace snapshot
            if let Some(ref json) = restored.executing_plan_json {
                state.executing_plan = serde_json::from_str(json).ok();
            }
            if let Some(ref goal) = restored.plan_goal {
                state.executing_plan_goal = Some(goal.clone());
                repl_turn::steer_observability_goal(state, goal);
            }
            if let Some(ref json) = restored.plan_config_json {
                state.plan_execution_config = serde_json::from_str(json).ok();
            }
            state.plan_execution_rounds = restored.plan_execution_rounds;

            // Restore operator corrections stacked during plan pause
            state.plan_execution_corrections = restored.plan_corrections.clone();

            // Restore durable task contract if present
            if let Some(ref json) = restored.contract_json
                && let Ok(contract) = serde_json::from_str::<astra_services::TaskContract>(json)
            {
                let work_dir = std::env::current_dir().unwrap_or_default();
                let ingestion_sender = state
                    .matrix_runtime
                    .as_ref()
                    .and_then(|mc| mc.clone_ingestion_sender());
                let cloud_judge = state
                    .matrix_runtime
                    .as_ref()
                    .and_then(|mc| mc.create_cloud_llm_judge())
                    .map(|j| {
                        std::sync::Arc::new(j) as std::sync::Arc<dyn astra_services::LlmJudge>
                    });
                let learning = build_learning_bridge(state);

                let lifecycle = if let Some(pool) = state
                    .matrix_runtime
                    .as_ref()
                    .map(|mc| mc.shared_pool().get().clone())
                {
                    durable_bridge::create_cloud_lifecycle_full(
                        pool,
                        &work_dir,
                        ingestion_sender,
                        Some(&session_id),
                        state.ingestion_user_id.as_deref(),
                        cloud_judge,
                        learning,
                        None, // no server proxy during session restore
                    )
                } else {
                    let session_dir =
                        astra_services::session_workspace::workspace_dir_for(&session_id);
                    durable_bridge::create_local_lifecycle_full(
                        &session_dir,
                        &work_dir,
                        ingestion_sender,
                        Some(&session_id),
                        state.ingestion_user_id.as_deref(),
                        cloud_judge,
                        learning,
                        None, // no server proxy during session restore
                    )
                };
                state.durable_task_state = Some(durable_bridge::DurableTaskState {
                    contract,
                    lifecycle,
                    last_report: None,
                });
            }

            // Re-initialize journal for the resumed session
            repl_turn::initialize_journal_pub(state, &session_id);
            repl_turn::persist_last_session_id(profile, &session_id);
            if let Ok(mut ws) = astra_services::session_workspace::read_workspace(&session_id) {
                ws.turn_count = restored.turn_count;
                ws.total_tokens_in = restored.total_tokens_in;
                ws.total_tokens_out = restored.total_tokens_out;
                ws.status = restored.last_status.clone();
                if let Some(ref branch) = restored.git_branch {
                    ws.git_branch = Some(branch.clone());
                }
                if let Some(ref model) = restored.model {
                    ws.model = model.clone();
                }
                ws.executing_plan_json = restored.executing_plan_json.clone();
                ws.plan_goal = restored.plan_goal.clone();
                ws.plan_config_json = restored.plan_config_json.clone();
                ws.plan_execution_rounds = restored.plan_execution_rounds;
                ws.contract_json = restored.contract_json.clone();
                ws.plan_corrections = restored.plan_corrections.clone();
                ws.last_context_trace = restored.last_context_trace.clone();
                if let Err(e) = astra_services::session_workspace::write_workspace(&ws) {
                    eprintln!("  ⚠ workspace write failed during resume: {e}");
                }
            }

            let source = if restored.restored_from_cloud {
                "cloud"
            } else {
                "local"
            };
            eprintln!(
                "  {} Resumed session {} ({}, {} turns, {} checkpoints)",
                theme::icon_ok(),
                &session_id[..8.min(session_id.len())].cyan(),
                source,
                restored.turn_count,
                restored.checkpoint_count,
            );
            if let Some(ref trace) = restored.last_context_trace {
                let preview = trace.preview();
                if !preview.is_empty() {
                    eprintln!("    {} {}", "Last trace:".dim(), preview.dim());
                }
            }

            // Show paused plan banner
            if let Some(ref plan) = state.executing_plan {
                let done = plan.items_done();
                let total = plan.subtasks.len();
                let pct = plan.progress_pct();
                eprintln!(
                    "  {} Paused plan restored: {}/{} subtasks done ({}%)",
                    "📋".cyan(),
                    done,
                    total,
                    pct,
                );
                if let Some(ref goal) = state.executing_plan_goal {
                    eprintln!("    {} {}", "Goal:".dim(), goal.as_str().dim());
                }
                eprintln!(
                    "    {}",
                    "Say continue / resume / next / go to pick up; correct … / rewind N to adjust; slash lines keep the plan; any other line abandons it."
                        .dim()
                );
            }
        }
        Ok(None) => {
            // Service didn't find workspace/cloud data, but journal may exist.
            // Don't reuse the old session_id — server doesn't know it.
            // Restore history as context for a new session.
            match session_journal::read_journal(&session_id) {
                Ok(events) if !events.is_empty() => {
                    let turn_count = events
                        .iter()
                        .filter(|e| e.event_type == session_journal::JournalEventType::Turn)
                        .count() as u32;
                    // Restore last turn event for /turn command
                    state.last_turn_event = events
                        .iter()
                        .rev()
                        .find(|e| e.event_type == session_journal::JournalEventType::Turn)
                        .cloned();
                    state.session_id = None; // new session on next message
                    state.turn = turn_count;
                    state.history = repl_runtime::restore_history_from_journal(&session_id);
                    eprintln!(
                        "  {} Restored {} turns from journal {}. Next message starts a new session.",
                        theme::icon_ok(),
                        turn_count,
                        &session_id[..8.min(session_id.len())].cyan(),
                    );
                }
                _ => {
                    // Suggest similar session IDs/prefixes
                    let recent = session_journal::list_sessions_by_time(10).unwrap_or_default();
                    let suggestions = cli_output::suggest_sessions(arg, &recent);
                    let refs: Vec<&str> = suggestions.iter().map(|s| s.as_str()).collect();
                    cli_output::format_not_found_error(
                        "Session",
                        arg,
                        &refs,
                        Some("/resume to see available sessions"),
                    );
                }
            }
        }
        Err(e) => {
            let hint = if e.to_string().contains("not found") {
                "Use /resume to see available sessions."
            } else {
                "Check connection with /diagnostics, or try a different session."
            };
            eprintln!(
                "  {} {}",
                theme::icon_err(),
                format!("Resume failed: {e}").red()
            );
            eprintln!("{}", format!("  {hint}").dim());
        }
    }
}
