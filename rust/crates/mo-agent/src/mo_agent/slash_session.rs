use std::io::Write;

use super::*;

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
        let show = sessions.len().min(10);
        for (i, sid) in sessions.iter().take(show).enumerate() {
            eprintln!(
                "  {}  {}",
                format!("[{}]", i + 1).cyan().bold(),
                sid.as_str().cyan()
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

pub(super) fn handle_session_command(arg: &str, state: &ReplState) {
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
            eprintln!("  {:<14} {}", "session_id:".dim(), sid.cyan());
            eprintln!("  {:<14} {}", "model:".dim(), mdl.cyan());
            eprintln!("  {:<14} {}", "turns:".dim(), state.turn.to_string().cyan());
            eprintln!(
                "  {:<14} {}",
                "explain:".dim(),
                state.explain.to_string().cyan()
            );
            eprintln!(
                "  {:<14} {}",
                "run_id:".dim(),
                state.run_id.as_deref().unwrap_or("none").cyan()
            );
            if let Some(ref j) = state.journal {
                eprintln!(
                    "  {:<14} {}",
                    "journal:".dim(),
                    j.path().display().to_string().cyan()
                );
            }
            eprintln!();
            eprintln!(
                "  {}",
                "Subcommands: /session history · errors · export · list".dim()
            );
            eprintln!();
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
                    "✓".green(),
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
                                            "✗".red(),
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
                                    "✗".red(),
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
                                    "✗".red(),
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
                                    "⚠".yellow(),
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
                eprintln!("{}", "  No journal files found.".dim());
            }
            Ok(sessions) => {
                eprintln!(
                    "\n{}",
                    "─── Session Journals ────────────────────────────".bold()
                );
                let current = state.session_id.as_deref().unwrap_or("");
                for sid in &sessions {
                    let marker = if sid == current { " ← current" } else { "" };
                    eprintln!("  {}{}", sid.as_str().cyan(), marker.green());
                }
                eprintln!("  {} sessions total", sessions.len());
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
                    "✓".green(),
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
                    "✓".green(),
                    sub_arg.cyan(),
                    target_sid.as_str().cyan()
                );
            }
            match session_journal::read_journal(&target_sid) {
                Ok(events) if events.is_empty() => {
                    eprintln!("{}", "  No journal entries to export.".dim());
                }
                Ok(events) => {
                    // Export as markdown
                    let mut md = format!("# Session: {target_sid}\n\n");
                    for evt in &events {
                        let ts_short = evt.ts.get(..19).unwrap_or(&evt.ts);
                        match evt.event_type {
                            session_journal::JournalEventType::SessionStart => {
                                md.push_str(&format!(
                                    "## Session Start\n- **Time:** {ts_short}\n- **Model:** {}\n\n",
                                    evt.model.as_deref().unwrap_or("default")
                                ));
                            }
                            session_journal::JournalEventType::Turn => {
                                md.push_str(&format!("### Turn {}\n- **Time:** {ts_short}\n- **Duration:** {}ms\n- **Tokens:** {}→{}\n- **Tools:** {}\n\n**User:** {}\n\n**Assistant:** {}\n\n---\n\n",
                                            evt.turn.unwrap_or(0),
                                            evt.duration_ms.unwrap_or(0),
                                            evt.tokens_in.unwrap_or(0),
                                            evt.tokens_out.unwrap_or(0),
                                            evt.tool_count.unwrap_or(0),
                                            evt.user_input.as_deref().unwrap_or(""),
                                            evt.assistant_output.as_deref().unwrap_or(""),
                                        ));
                            }
                            session_journal::JournalEventType::TurnError => {
                                md.push_str(&format!("### Turn {} ❌ Error\n- **Time:** {ts_short}\n- **Error:** {}\n\n---\n\n",
                                            evt.turn.unwrap_or(0),
                                            evt.error.as_deref().unwrap_or("unknown"),
                                        ));
                            }
                            session_journal::JournalEventType::Compact => {
                                md.push_str(&format!("### Compact\n- **Time:** {ts_short}\n- **Turns compacted:** {}\n- **Facts stored:** {}\n\n",
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
                                md.push_str(&format!("## Session End\n- **Time:** {ts_short}\n- **Total turns:** {}\n",
                                            evt.turn.unwrap_or(0),
                                        ));
                            }
                            _ => {}
                        }
                    }
                    let export_path =
                        format!("session-{}.md", &target_sid[..8.min(target_sid.len())]);
                    match std::fs::write(&export_path, &md) {
                        Ok(_) => eprintln!("  {} Exported to {}", "✓".green(), export_path.cyan()),
                        Err(e) => {
                            eprintln!("{}", format!("  ✗ Failed to write: {e}").red())
                        }
                    }
                }
                Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
            }
        }
        other => {
            eprintln!("{}", format!("  Unknown subcommand: {other}").red());
            eprintln!(
                "  {}",
                "Usage: /session [history|list|errors|export] [session_id]".dim()
            );
        }
    }
}
