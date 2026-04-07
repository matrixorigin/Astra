use std::io::Write;

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
            return Err("  No sessions found.".to_string());
        }
        eprintln!(
            "\n{}",
            "─── Available Sessions ──────────────────────────".bold()
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
                "─── Session ─────────────────────────────────────".bold()
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
                "Subcommands: /session history · errors · export · list · fork".dim()
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
                                            .unwrap_or("unknown")
                                            .chars()
                                            .take(80)
                                            .collect::<String>();
                                        eprintln!(
                                            "    {} {} ({}ms) {}",
                                            theme::icon_err(),
                                            tc.name,
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
                                    evt.error.as_deref().unwrap_or("unknown").red(),
                                );
                            }
                            session_journal::JournalEventType::Compact => {
                                eprintln!(
                                    "  {} {} compacted {} turns ({} facts)",
                                    ts_short.dim(),
                                    "⟳".yellow(),
                                    evt.turns_compacted.unwrap_or(0),
                                    evt.facts_stored.unwrap_or(0),
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
                                    evt.error.as_deref().unwrap_or("unknown error").red(),
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
                                    evt.stall_type.as_deref().unwrap_or("unknown").yellow(),
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
                                        format!("{inj} nudges, {avoid} tools restricted")
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
                                    "plan_complete" => "🎉",
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
                            session_journal::JournalEventType::DelegationSubRunCompleted => {
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
                            }
                            session_journal::JournalEventType::DelegationCompleted => {
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
                    eprintln!();
                }
                Err(e) => {
                    eprintln!("{}", format!("  ✗ Failed to read journal: {e}").red());
                }
            }
        }
        "list" => match session_journal::list_sessions() {
            Ok(sessions) if sessions.is_empty() => {
                eprintln!("{}", "  No journal files yet.".dim());
                eprintln!(
                    "  {}",
                    "Chat once to create a session, or check ~/.astra/sessions.".dim()
                );
            }
            Ok(sessions) => {
                eprintln!(
                    "\n{}",
                    "─── Session Journals ────────────────────────────".bold()
                );
                eprintln!(
                    "  {}",
                    "sorted A–Z · right column: cwd, git, turns (from workspace.yaml)".dim()
                );
                let current = state.session_id.as_deref().unwrap_or("");
                for sid in &sessions {
                    let marker = if sid == current { " ← current" } else { "" };
                    let hint = workspace_summary_line(sid);
                    eprintln!(
                        "  {}  {}{}",
                        sid.as_str().cyan(),
                        hint.dim(),
                        marker.green()
                    );
                }
                eprintln!(
                    "  {} {}",
                    sessions.len().to_string().dim(),
                    if sessions.len() == 1 {
                        "session total"
                    } else {
                        "sessions total"
                    }
                );
                eprintln!();
            }
            Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
        },
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
                        eprintln!("{}", "  No errors in this session. 🎉".green());
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
        other => {
            eprintln!("{}", format!("  Unknown subcommand: {other}").red());
            eprintln!(
                "  {}",
                "Usage: /session [history|list|errors|export|fork] [session_id] […]".dim()
            );
        }
    }
}

// ── Session export ──────────────────────────────────────────────────────────

/// Format tool call records as a summarized markdown block.
fn format_tool_calls_md(calls: &[session_journal::ToolCallRecord]) -> String {
    let mut out = String::new();
    out.push_str("\n<details>\n<summary>Tool calls</summary>\n\n");
    for tc in calls {
        let status = if tc.ok { "✓" } else { "✗" };
        out.push_str(&format!("- `{}` {status} ({}ms)", tc.name, tc.ms));
        if let Some(ref args) = tc.args_preview {
            out.push_str(&format!(" — {args}"));
        }
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
            out.push_str(&format!("  > ```\n  > {}\n  > ```\n", short.replace('\n', "\n  > ")));
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
                    evt.error.as_deref().unwrap_or("unknown"),
                ));
            }
            session_journal::JournalEventType::Compact => {
                md.push_str(&format!(
                    "### Compact\n- **Time:** {ts_short}\n- **Turns compacted:** {}\n- **Facts stored:** {}\n\n",
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

/// Handle top-level `/export` command — exports the active session as Markdown.
pub(super) fn handle_export_command(state: &ReplState) {
    let Some(ref sid) = state.session_id else {
        eprintln!("{}", "  No active session.".yellow());
        return;
    };
    export_session_markdown(sid);
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
        assert!(md.contains("`read_file` ✓"));
        assert!(md.contains("`bash` ✗"));
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
        assert!(block.contains("`grep` ✓ (10ms) — pattern in src/"));
    }
}
