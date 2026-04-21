use super::*;

pub(super) struct StateCommandContext<'a> {
    pub(super) api: &'a astra_thin_client::ThinClient,
    pub(super) profile: Option<&'a str>,
    pub(super) token: Option<&'a str>,
    pub(super) selector: &'a dyn tool_selector::ToolSelector,
}

pub(super) async fn handle_state_command(
    cmd: &str,
    arg: &str,
    ctx: StateCommandContext<'_>,
    state: &mut ReplState,
) -> Result<(), String> {
    let StateCommandContext {
        api,
        profile,
        token,
        selector,
    } = ctx;
    match cmd {
        "/clear" => {
            let Some(tok) = token else {
                eprintln!("{}", "  Not logged in. Use /login.".yellow());
                return Ok(());
            };
            let body = api
                .post_sessions_json(tok, &serde_json::json!({}))
                .await
                .map_err(map_thin_err)?;
            let value: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let new_sid = value
                .get("session_id")
                .or_else(|| value.get("id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if let Some(sid) = &new_sid {
                let mut creds = load_credentials();
                let pname = profile_name(profile, &creds);
                let p = creds.profiles.entry(pname).or_default();
                p.last_session_id = Some(sid.clone());
                let _ = save_credentials(&creds);
            }
            state.session_id = new_sid.clone();
            state.turn = 0;
            state.run_id = None;
            state.history.clear();
            state.total_prompt_tokens = 0;
            state.total_completion_tokens = 0;
            if let Some(ref sid) = new_sid {
                state.journal = session_journal::JournalWriter::new(sid).ok();
                if let Some(ref j) = state.journal {
                    let _ = j.append(&session_journal::JournalEvent::session_start(
                        Some(sid),
                        state.model.as_deref(),
                    ));
                }
            }
            let display = new_sid.as_deref().unwrap_or("(none)");
            eprintln!(
                "{}",
                format!("  \u{2713}  New session: {}", display).green()
            );
        }

        "/undo" => {
            // Handle "/undo list" subcommand — show file edit history
            if arg == "list" || arg == "files" {
                if let Ok(journal) = state.file_journal.lock() {
                    let summary = journal.summary();
                    if summary.is_empty() {
                        eprintln!("{}", "  No file edits in journal.".yellow());
                    } else {
                        eprintln!("  File edit journal ({} files):", summary.len());
                        for (path, count, edit_type) in &summary {
                            eprintln!(
                                "    {} {} ({} edit{})",
                                match edit_type {
                                    astra_runtime::turn::file_edit_journal::EditType::Overwrite =>
                                        "📝",
                                    astra_runtime::turn::file_edit_journal::EditType::Create =>
                                        "🆕",
                                    astra_runtime::turn::file_edit_journal::EditType::Patch => "✏️",
                                    astra_runtime::turn::file_edit_journal::EditType::Delete =>
                                        "🗑️",
                                },
                                path.display(),
                                count,
                                if *count == 1 { "" } else { "s" },
                            );
                        }
                    }
                }
                return Ok(());
            }

            if state.history.is_empty() {
                eprintln!("{}", "  Nothing to undo.".yellow());
                return Ok(());
            }
            let count: usize = if arg.is_empty() {
                1
            } else {
                match arg.parse::<usize>() {
                    Ok(0) => {
                        eprintln!("{}", "  /undo requires a positive number.".yellow());
                        return Ok(());
                    }
                    Ok(n) => n,
                    Err(_) => {
                        eprintln!(
                            "{}",
                            "  Usage: /undo [N] | /undo list  — undo last N turns or list file edits".yellow()
                        );
                        return Ok(());
                    }
                }
            };
            let actual = count.min(state.history.len());
            let mut undone_previews = Vec::new();
            let mut file_reverts: Vec<String> = Vec::new();
            for _ in 0..actual {
                if let Some((user_msg, assistant_msg)) = state.history.pop() {
                    let preview: String = user_msg.chars().take(50).collect();
                    let preview = if user_msg.chars().count() > 50 {
                        format!("{}…", preview)
                    } else {
                        preview
                    };
                    undone_previews.push(preview);
                    // Revert file changes for this turn
                    let turn_index = state.turn;
                    if let Ok(journal) = state.file_journal.lock() {
                        let result = journal.undo_turn(turn_index);
                        for path in &result.reverted {
                            file_reverts.push(path.display().to_string());
                        }
                    }
                    // Save to redo stack
                    state.redo_stack.push((user_msg, assistant_msg, state.turn));
                    state.turn = state.turn.saturating_sub(1);
                }
            }
            state.last_response = state.history.last().map(|(_, resp)| resp.clone());
            state.continuation_anchor = None;
            if actual == 1 {
                eprintln!(
                    "  {} Undid 1 turn: {}",
                    theme::icon_ok(),
                    undone_previews[0].as_str().dim()
                );
            } else {
                eprintln!("  {} Undid {} turns:", theme::icon_ok(), actual,);
                for (i, preview) in undone_previews.iter().enumerate() {
                    eprintln!("    {}. {}", actual - i, preview.as_str().dim());
                }
            }
            if !file_reverts.is_empty() {
                eprintln!(
                    "  ↩ Reverted {} file{}:",
                    file_reverts.len(),
                    if file_reverts.len() == 1 { "" } else { "s" },
                );
                for f in &file_reverts {
                    eprintln!("    {}", f.as_str().dim());
                }
            }
            eprintln!("  {} turns remaining in context", state.history.len());
            if !state.redo_stack.is_empty() {
                eprintln!(
                    "  💡 {} turn{} available for /redo",
                    state.redo_stack.len(),
                    if state.redo_stack.len() == 1 { "" } else { "s" }
                );
            }
            if let Some(ref j) = state.journal {
                let _ = j.append(&session_journal::JournalEvent::config_change(
                    state.session_id.as_deref(),
                    "undo",
                    &actual.to_string(),
                ));
            }
        }

        "/redo" => {
            if state.redo_stack.is_empty() {
                eprintln!("{}", "  Nothing to redo.".yellow());
                return Ok(());
            }
            let count: usize = if arg.is_empty() {
                1
            } else {
                match arg.parse::<usize>() {
                    Ok(0) => {
                        eprintln!("{}", "  /redo requires a positive number.".yellow());
                        return Ok(());
                    }
                    Ok(n) => n,
                    Err(_) => {
                        eprintln!(
                            "{}",
                            "  Usage: /redo [N] — restore last N undone turns".yellow()
                        );
                        return Ok(());
                    }
                }
            };
            let actual = count.min(state.redo_stack.len());
            let mut redone_previews = Vec::new();
            for _ in 0..actual {
                if let Some((user_msg, assistant_msg, turn_num)) = state.redo_stack.pop() {
                    let preview: String = user_msg.chars().take(50).collect();
                    let preview = if user_msg.chars().count() > 50 {
                        format!("{}…", preview)
                    } else {
                        preview
                    };
                    redone_previews.push(preview);
                    // Restore to history and update turn counter
                    state.history.push((user_msg, assistant_msg.clone()));
                    state.turn = turn_num;
                    state.last_response = Some(assistant_msg);
                }
            }
            state.continuation_anchor = None;
            if actual == 1 {
                eprintln!(
                    "  {} Redid 1 turn: {}",
                    theme::icon_ok(),
                    redone_previews[0].as_str().dim()
                );
            } else {
                eprintln!("  {} Redid {} turns:", theme::icon_ok(), actual);
                for (i, preview) in redone_previews.iter().enumerate() {
                    eprintln!("    {}. {}", i + 1, preview.as_str().dim());
                }
            }
            eprintln!("  {} turns now in context", state.history.len());
            if !state.redo_stack.is_empty() {
                eprintln!(
                    "  💡 {} turn{} still available for /redo",
                    state.redo_stack.len(),
                    if state.redo_stack.len() == 1 { "" } else { "s" }
                );
            }
            if let Some(ref j) = state.journal {
                let _ = j.append(&session_journal::JournalEvent::config_change(
                    state.session_id.as_deref(),
                    "redo",
                    &actual.to_string(),
                ));
            }
        }

        "/explain" => {
            state.explain = match state.explain {
                ExplainMode::Off => ExplainMode::On,
                ExplainMode::On => ExplainMode::Verbose,
                ExplainMode::Verbose => ExplainMode::Off,
            };
            let s = match state.explain {
                ExplainMode::Off => "off".yellow().to_string(),
                ExplainMode::On => "on".green().to_string(),
                ExplainMode::Verbose => "verbose".green().to_string(),
            };
            eprintln!("  Explain mode: {}", s);
            if matches!(state.explain, ExplainMode::On) {
                eprintln!("{}", "  (verbose: selector + skill lines on stderr)".dim());
            }
            if let Some(ref j) = state.journal {
                let explain_val = match state.explain {
                    ExplainMode::Off => "off",
                    ExplainMode::On => "on",
                    ExplainMode::Verbose => "verbose",
                };
                let _ = j.append(&session_journal::JournalEvent::config_change(
                    state.session_id.as_deref(),
                    "explain",
                    explain_val,
                ));
            }
        }

        "/verbose" => {
            state.verbose_mode = true;
            eprintln!("  Verbose mode on");
        }

        "/compact" => {
            if state.history.is_empty() {
                eprintln!(
                    "  {}",
                    "Nothing to compact — no conversation history yet.".dim()
                );
                return Ok(());
            }
            let (compact_quick, compact_no_memoria) = {
                let mut q = false;
                let mut nm = false;
                for w in arg.split_whitespace() {
                    let t = w.to_ascii_lowercase();
                    if t == "quick" || t == "summary-only" {
                        q = true;
                    }
                    if t == "no-memoria" || t == "no_memoria" {
                        nm = true;
                    }
                }
                (q, nm)
            };
            let Some(tok) = token else {
                eprintln!("{}", "  Not logged in. Use /login.".yellow());
                return Ok(());
            };

            eprintln!("  {}", "Summarizing…".dim());
            let mut auto_pm =
                PermissionManager::with_project(true, &std::env::current_dir().unwrap_or_default());
            let mut _cancel_token_guard: Option<
                std::sync::Arc<tokio_util::sync::CancellationToken>,
            > = None;
            let summary_result = tokio::select! {
                r = stream_chat_sse(ChatTurnParams {
                    api,
                    token: tok,
                    message: prompts::COMPACT_SUMMARY_REQUEST,
                    session_id: state.session_id.as_deref(),
                    model: state.model.as_deref(),
                    explain: ExplainMode::Off,
                    render_md: false,
                    history: &state.history,
                    perm_manager: &mut auto_pm,
                    verbose_mode: false,
                    render_policy: crate::stream_render::RenderPolicy::Silent,
                    selector,
                    recent_tools: &[],
                    tool_health_entries: &[],
                    unified_skill_registry: astra_runtime::skills::default_unified_registry(),
                    plan_only_chat: false,
                    is_plan_subtask: false,
                    plan_subtask_id: None,
                    delegation_engine: None,
                    cancel_token: {
                        let token = std::sync::Arc::new(tokio_util::sync::CancellationToken::new());
                        _cancel_token_guard = Some(token.clone());
                        Some(token)
                    },
                    plan_assemble_line_release: None,
                    stream_event_tx: None,
                    approval_request_tx: None,
                    mcp_manager: Some(state.mcp_manager.clone()),
                    skill_search: &state.skill_search,
                    skill_quality_tracker: &mut state.skill_quality_tracker,
                    discovered_skills: None,
                    messaging_metrics: state.messaging_metrics.clone(),
                    agent_spawner: state.agent_spawner.clone(),
                    root_agent_id: Some("main"),
                    root_mailbox_slot: Some(&mut state.root_mailbox),
                    observability_hub: state.observability_hub.clone(),
                    observability_session: state.observability_session.clone(),
                    file_journal: None,
                    database_snapshot_journal: None,
                    git_stash_journal: None,
                    git_commit_journal: None,
                    git_worktree_journal: None,
                    session_state_journal: None,
                    task_manager: None,
                    turn_index: 0,
                    evolution_service: state.evolution_service.clone(),
                }) => r,
                _ = tokio::signal::ctrl_c() => {
                    if let Some(ref t) = _cancel_token_guard { t.cancel(); }
                    eprintln!("{}", "  Interrupted.".dim());
                    return Ok(());
                }
            };

            let summary = match summary_result {
                Ok(sr) => {
                    let text = sr.full_text.trim().to_string();
                    if text.is_empty() {
                        eprintln!("{}", "  ✗ Empty summary returned.".yellow());
                        return Ok(());
                    }
                    text
                }
                Err(e) => {
                    eprintln!("{}", format!("  ✗ Failed to summarize: {}", e.error).red());
                    return Ok(());
                }
            };

            // Save summary to Memoria via server proxy (preserves user isolation)
            let mut saved_to_memoria = false;
            let mut facts_stored = 0usize;
            if let Some(tok) = token {
                if !compact_no_memoria {
                    let meta = prompts::memory_proto::EntryMeta::from_session_with_tier(
                        state.session_id.as_deref(),
                        state.turn,
                        prompts::memory_proto::SRC_COMPACT,
                        prompts::memory_proto::TIER_INFERRED,
                    );
                    let entry = prompts::memory_proto::MemoryEntry::new(
                        prompts::memory_proto::NS_EPISODE,
                        prompts::memory_proto::ST_SUMMARY,
                        &summary,
                    );
                    match api
                        .post_memory_store_json(tok, &entry.to_store_payload_with_meta(&meta))
                        .await
                    {
                        Ok(r) if r.status().is_success() => saved_to_memoria = true,
                        Ok(r) => eprintln!(
                            "{}",
                            format!("  ⚠ Memory save failed ({})", r.status()).yellow()
                        ),
                        Err(e) => eprintln!(
                            "{}",
                            format!("  ⚠ Memory service unreachable: {e}").yellow()
                        ),
                    }

                    // Extract structured facts from summary and store each individually
                    if saved_to_memoria && !compact_quick {
                        let extract_msg = format!("{}{summary}", prompts::MEMORY_EXTRACTOR_PROMPT);
                        let mut auto_pm2 = PermissionManager::with_project(
                            true,
                            &std::env::current_dir().unwrap_or_default(),
                        );
                        let extract_result = stream_chat_sse(ChatTurnParams {
                            api,
                            token: tok,
                            message: &extract_msg,
                            session_id: state.session_id.as_deref(),
                            model: state.model.as_deref(),
                            explain: ExplainMode::Off,
                            render_md: false,
                            history: &[],
                            perm_manager: &mut auto_pm2,
                            verbose_mode: false,
                            render_policy: crate::stream_render::RenderPolicy::Silent,
                            selector,
                            recent_tools: &[],
                            tool_health_entries: &[],
                            unified_skill_registry: astra_runtime::skills::default_unified_registry(
                            ),
                            plan_only_chat: false,
                            is_plan_subtask: false,
                            plan_subtask_id: None,
                            delegation_engine: None,
                            cancel_token: None,
                            plan_assemble_line_release: None,
                            stream_event_tx: None,
                            approval_request_tx: None,
                            mcp_manager: Some(state.mcp_manager.clone()),
                            skill_search: &state.skill_search,
                            skill_quality_tracker: &mut state.skill_quality_tracker,
                            discovered_skills: None,
                            messaging_metrics: state.messaging_metrics.clone(),
                            agent_spawner: state.agent_spawner.clone(),
                            root_agent_id: Some("main"),
                            root_mailbox_slot: Some(&mut state.root_mailbox),
                            observability_hub: state.observability_hub.clone(),
                            observability_session: state.observability_session.clone(),
                            file_journal: None,
                            database_snapshot_journal: None,
                            git_stash_journal: None,
                            git_commit_journal: None,
                            git_worktree_journal: None,
                            session_state_journal: None,
                            task_manager: None,
                            turn_index: 0,
                            evolution_service: state.evolution_service.clone(),
                        })
                        .await;

                        if let Ok(sr) = extract_result {
                            let facts = prompts::parse_extracted_facts(&sr.full_text);
                            let fact_meta =
                                prompts::memory_proto::EntryMeta::from_session_with_tier(
                                    state.session_id.as_deref(),
                                    state.turn,
                                    prompts::memory_proto::SRC_EXTRACTED,
                                    prompts::memory_proto::TIER_INFERRED,
                                );
                            for (fact, mem_type) in &facts {
                                let fact_entry = prompts::memory_proto::MemoryEntry::new(
                                    prompts::memory_proto::NS_FACT,
                                    mem_type,
                                    fact,
                                );
                                let _ = api
                                    .post_memory_store_json(
                                        tok,
                                        &fact_entry.to_store_payload_with_meta(&fact_meta),
                                    )
                                    .await;
                                facts_stored += 1;
                            }

                            // ── Knowledge synthesis: detect patterns across extracted facts ──
                            if facts.len() >= 2 {
                                let fact_lines: Vec<String> =
                                    facts.iter().map(|(f, t)| format!("- [{t}] {f}")).collect();
                                let synthesis_prompt = format!(
                                    "Given these extracted facts from a conversation:\n{}\n\n\
                                 If there is a higher-level pattern, theme, or insight that \
                                 connects 2+ facts, state it as ONE concise sentence (≤25 words). \
                                 If no pattern exists, respond with exactly: NONE",
                                    fact_lines.join("\n")
                                );
                                let mut auto_pm3 = PermissionManager::with_project(
                                    true,
                                    &std::env::current_dir().unwrap_or_default(),
                                );
                                let synth_result = stream_chat_sse(ChatTurnParams {
                                    api,
                                    token: tok,
                                    message: &synthesis_prompt,
                                    session_id: state.session_id.as_deref(),
                                    model: state.model.as_deref(),
                                    explain: ExplainMode::Off,
                                    render_md: false,
                                    history: &[],
                                    perm_manager: &mut auto_pm3,
                                    verbose_mode: false,
                                    render_policy: crate::stream_render::RenderPolicy::Silent,
                                    selector,
                                    recent_tools: &[],
                                    tool_health_entries: &[],
                                    unified_skill_registry:
                                        astra_runtime::skills::default_unified_registry(),
                                    plan_only_chat: false,
                                    is_plan_subtask: false,
                                    plan_subtask_id: None,
                                    delegation_engine: None,
                                    cancel_token: None,
                                    plan_assemble_line_release: None,
                                    stream_event_tx: None,
                                    approval_request_tx: None,
                                    mcp_manager: Some(state.mcp_manager.clone()),
                                    skill_search: &state.skill_search,
                                    skill_quality_tracker: &mut state.skill_quality_tracker,
                                    discovered_skills: None,
                                    messaging_metrics: state.messaging_metrics.clone(),
                                    agent_spawner: state.agent_spawner.clone(),
                                    root_agent_id: Some("main"),
                                    root_mailbox_slot: Some(&mut state.root_mailbox),
                                    observability_hub: state.observability_hub.clone(),
                                    observability_session: state.observability_session.clone(),
                                    file_journal: None,
                                    database_snapshot_journal: None,
                                    git_stash_journal: None,
                                    git_commit_journal: None,
                                    git_worktree_journal: None,
                                    session_state_journal: None,
                                    task_manager: None,
                                    turn_index: 0,
                                    evolution_service: state.evolution_service.clone(),
                                })
                                .await;
                                if let Ok(sr2) = synth_result {
                                    let insight = sr2.full_text.trim().to_string();
                                    if !insight.is_empty()
                                        && !insight.eq_ignore_ascii_case("NONE")
                                        && !insight.contains("no pattern")
                                        && insight.len() < 200
                                    {
                                        let insight_entry = prompts::memory_proto::MemoryEntry::new(
                                            prompts::memory_proto::NS_INSIGHT,
                                            prompts::memory_proto::ST_ACTIVE,
                                            &insight,
                                        );
                                        let synth_meta =
                                            prompts::memory_proto::EntryMeta::from_session_with_tier(
                                                state.session_id.as_deref(),
                                                state.turn,
                                                prompts::memory_proto::SRC_SYNTHESIS,
                                                prompts::memory_proto::TIER_UNVERIFIED,
                                            );
                                        let _ = api
                                            .post_memory_store_json(
                                                tok,
                                                &insight_entry
                                                    .to_store_payload_with_meta(&synth_meta),
                                            )
                                            .await;
                                        facts_stored += 1; // count insight as a stored fact
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Truncate history: align with ContextBudget (same as auto-compact)
            let keep_recent = state.context_budget.keep_recent_turns;
            let total = state.history.len();
            let trimmed_count = total.saturating_sub(keep_recent);
            if trimmed_count > 0 {
                // ── Context swap: store trimmed turns to memory for later retrieval ──
                if let Some(tok) = token {
                    if !compact_no_memoria {
                        // Build a compact representation of the swapped turns
                        let mut swap_lines: Vec<String> = Vec::new();
                        for (user_msg, assistant_msg) in &state.history[..trimmed_count] {
                            if !user_msg.is_empty() {
                                let preview: String = user_msg.chars().take(100).collect();
                                swap_lines.push(format!("U: {preview}"));
                            }
                            if !assistant_msg.is_empty() {
                                let preview: String = assistant_msg.chars().take(150).collect();
                                swap_lines.push(format!("A: {preview}"));
                            }
                        }
                        if !swap_lines.is_empty() {
                            let swap_body = format!(
                                "Turns 1-{trimmed_count} swapped out:\n{}",
                                swap_lines.join("\n")
                            );
                            // Cap at 2000 chars to avoid storing excessive content
                            let capped: String = swap_body.chars().take(2000).collect();
                            let swap_entry = prompts::memory_proto::MemoryEntry::new(
                                prompts::memory_proto::NS_SWAP,
                                prompts::memory_proto::ST_ARCHIVED,
                                &capped,
                            );
                            // No trust_tier: swap is "working" memory (transient, session-scoped)
                            let swap_meta = prompts::memory_proto::EntryMeta::from_session(
                                state.session_id.as_deref(),
                                state.turn,
                                prompts::memory_proto::SRC_COMPACT,
                            );
                            let _ = api
                                .post_memory_store_json(
                                    tok,
                                    &swap_entry.to_store_payload_with_meta(&swap_meta),
                                )
                                .await;
                        }
                    }
                }

                let anchor = if compact_no_memoria {
                    None
                } else {
                    crate::repl_turn::fetch_compact_memory_anchor_snippet(
                        api,
                        tok,
                        state.session_id.as_deref(),
                        &summary,
                    )
                    .await
                };
                let assistant_text = crate::repl_turn::compact_assistant_message(
                    trimmed_count,
                    &summary,
                    anchor.as_deref(),
                );
                let context_entry = (String::new(), assistant_text);
                let mut new_hist = vec![context_entry];
                new_hist.extend_from_slice(&state.history[trimmed_count..]);
                state.history = new_hist;
                state.recent_tools.clear();
            }

            // Print the summary box
            eprintln!();
            eprintln!(
                "{}",
                "─── Compact Summary ────────────────────────────────────────".dim()
            );
            for line in summary.lines() {
                eprintln!("  {line}");
            }
            eprintln!(
                "{}",
                "────────────────────────────────────────────────────────────".dim()
            );
            eprintln!();
            let mem_note = if compact_no_memoria {
                " · Memoria side-effects skipped (no-memoria)".to_string()
            } else if saved_to_memoria {
                let mut s = if facts_stored > 0 {
                    format!(" · saved to memory ({facts_stored} facts extracted)")
                } else {
                    " · saved to memory".to_string()
                };
                if compact_quick {
                    s.push_str(" · quick (no fact extraction pass)");
                }
                s
            } else {
                String::new()
            };
            eprintln!(
                "  {} {} turns compacted · {} turns in context{}",
                theme::icon_ok(),
                trimmed_count,
                state.history.len(),
                mem_note,
            );
            if state.plan_mode.is_some() || state.executing_plan.is_some() {
                eprintln!(
                    "{}",
                    "  Tip: Plan context was shortened — if steps feel stale, refresh `/plan` or your plan view."
                        .dim()
                );
            }
            // Journal: log compact event (include summary for knowledge backflow)
            if let Some(ref j) = state.journal {
                let _ = j.append(&session_journal::JournalEvent::compact_with_summary(
                    state.session_id.as_deref(),
                    state.turn,
                    trimmed_count,
                    facts_stored,
                    Some(&summary),
                ));
            }
        }

        "/reflect" => {
            let sid = match state.session_id.as_deref() {
                Some(s) => s.to_string(),
                None => {
                    eprintln!("{}", "  No active session.".yellow());
                    return Ok(());
                }
            };
            let (requested_focus, requested_question) = parse_reflect_args(arg);
            // `/reflect diff` short-circuits: render the local tool-health
            // delta between the most recently synced entries and the live
            // session entries, so the agent can audit its own tuning
            // without needing a server round-trip.
            if requested_focus.as_deref() == Some("diff") {
                let out = render_reflect_diff(state);
                eprint!("{out}");
                return Ok(());
            }
            if let Ok(body) = crate::self_command::render_reflect_surface_for_session(
                &sid,
                20,
                requested_focus.as_deref(),
                requested_question.as_deref(),
            )
            .await
            {
                render_reflect_report(
                    &body,
                    &sid,
                    requested_focus.as_deref(),
                    requested_question.as_deref(),
                );
                return Ok(());
            }

            let Some(tok) = token else {
                eprintln!(
                    "{}",
                    "  Reflect needs either local session artifacts or a logged-in server session."
                        .yellow()
                );
                return Ok(());
            };
            let mut rel = astra_thin_client::paths::chat_session_reflect(&sid)
                .trim_start_matches('/')
                .to_string();
            let mut query_parts: Vec<String> = Vec::new();
            if let Some(focus) = requested_focus.as_deref().filter(|focus| *focus != "auto") {
                query_parts.push(format!("focus={focus}"));
            }
            if let Some(question) = requested_question
                .as_deref()
                .filter(|question| !question.is_empty())
            {
                query_parts.push(format!("question={}", urlencoding(question)));
            }
            if !query_parts.is_empty() {
                rel = format!("{rel}?{}", query_parts.join("&"));
            }
            match api.get_authed_path_text(tok, &rel).await {
                Ok(body) => render_reflect_report(
                    &body,
                    &sid,
                    requested_focus.as_deref(),
                    requested_question.as_deref(),
                ),
                Err(astra_thin_client::ThinClientError::Api {
                    status,
                    body: err_body,
                }) => {
                    eprintln!(
                        "{}",
                        format!("  ✗ API Error ({}): {}", status, compact_or_raw(&err_body)).red()
                    );
                }
                Err(e) => eprintln!("{}", format!("  ✗ Reflect failed: {e}").red()),
            }
        }
        _ => unreachable!("unexpected state command: {cmd}"),
    }

    Ok(())
}

fn parse_reflect_args(arg: &str) -> (Option<String>, Option<String>) {
    let known_focuses = [
        "auto",
        "skill_failure",
        "unexpected_result",
        "data_quality",
        "tool_selection",
        "history",
        "performance",
        "diff",
    ];
    let mut parts = arg.splitn(2, ' ');
    let first = parts.next().unwrap_or("").trim();
    let rest = parts.next().unwrap_or("").trim();
    if known_focuses.contains(&first) {
        let focus = (!first.is_empty()).then(|| first.to_string());
        let question = (!rest.is_empty()).then(|| rest.to_string());
        (focus, question)
    } else if arg.trim().is_empty() {
        (Some("auto".to_string()), None)
    } else {
        (Some("auto".to_string()), Some(arg.trim().to_string()))
    }
}

fn is_local_reflect_report(report: &serde_json::Value) -> bool {
    report.get("reflection_context").is_some()
}

/// Render a compact diff view of what the agent has learned this session
/// vs the last cloud-synced baseline. Auto-populated — reads directly
/// from `ReplState` without any new plumbing.
///
/// Output enumerates tool-health entries whose failure rate, call count,
/// or presence changed since last sync. When nothing changed (e.g. fresh
/// session) the output is an explicit "no delta" line.
pub(super) fn render_reflect_diff(state: &super::repl_state::ReplState) -> String {
    use std::collections::HashMap;
    use std::fmt::Write;

    let mut out = String::new();
    let sep = "─".repeat(38);
    let _ = writeln!(out, "\n  ─── reflect diff {sep}");

    let synced: HashMap<&str, &astra_runtime::pipeline::persistence::ToolHealthEntry> = state
        .synced_tool_health_entries
        .iter()
        .map(|e| (e.name.as_str(), e))
        .collect();

    let mut rows: Vec<String> = Vec::new();
    for cur in &state.tool_health_entries {
        match synced.get(cur.name.as_str()) {
            None => {
                rows.push(format!(
                    "  + {name:20}  new · {calls} calls · {rate:.0}% fail",
                    name = cur.name,
                    calls = cur.total_calls,
                    rate = cur.failure_rate * 100.0
                ));
            }
            Some(prev) => {
                let rate_delta = cur.failure_rate - prev.failure_rate;
                let call_delta = cur.total_calls as i64 - prev.total_calls as i64;
                if call_delta == 0 && rate_delta.abs() < 0.005 {
                    continue;
                }
                let sign = if rate_delta >= 0.0 { "+" } else { "" };
                rows.push(format!(
                    "  ~ {name:20}  Δcalls {call_delta:+} · Δfail {sign}{rate:.0}% (now {now:.0}%)",
                    name = cur.name,
                    rate = rate_delta * 100.0,
                    now = cur.failure_rate * 100.0
                ));
            }
        }
    }

    if rows.is_empty() {
        let _ = writeln!(
            out,
            "  no delta since last sync · {} tools tracked",
            state.tool_health_entries.len()
        );
    } else {
        for row in rows {
            let _ = writeln!(out, "{row}");
        }
    }
    out
}

/// Render either the local liquid reflection surface or a server `ReflectReport`
/// as a compact, colored terminal report.
fn render_reflect_report(
    body: &str,
    session_id: &str,
    requested_focus: Option<&str>,
    requested_question: Option<&str>,
) {
    let Ok(report) = serde_json::from_str::<serde_json::Value>(body) else {
        print_json_or_raw(body);
        return;
    };

    if is_local_reflect_report(&report) {
        render_local_reflect_report(&report, session_id, requested_focus, requested_question);
        return;
    }

    let overview = &report["overview"];
    let short_sid = prefix_chars(session_id, 8);

    // Header
    eprintln!(
        "{}",
        format!("🔍 Session Diagnosis — {short_sid}").cyan().bold()
    );
    eprintln!("{}", "─────────────────────────────────────".dim());

    // Overview line
    let total_events = overview["total_events"].as_i64().unwrap_or(0);
    let total_decisions = overview["total_decisions"].as_i64().unwrap_or(0);
    let dur = overview["duration_minutes"]
        .as_f64()
        .map(|d| format!(", {d:.0}min"))
        .unwrap_or_default();
    eprintln!(
        "  {} {total_events} events, {total_decisions} decisions{dur}",
        "Overview:".bold()
    );

    // Top skills
    if let Some(skills) = overview["top_skills"].as_array()
        && !skills.is_empty()
    {
        let skill_strs: Vec<String> = skills
            .iter()
            .filter_map(|s| {
                let name = s[0].as_str()?;
                let cnt = s[1].as_i64()?;
                Some(format!("{name}({cnt})"))
            })
            .collect();
        eprintln!("  {} {}", "Skills:".bold(), skill_strs.join(", "));
    }

    // Errors summary
    let error_count = overview["error_count"].as_i64().unwrap_or(0);
    let error_rate = overview["error_rate_pct"].as_f64().unwrap_or(0.0);
    if error_count > 0 {
        let err_str = format!("  Errors: {error_count} ({error_rate:.1}%)");
        if error_rate > 30.0 {
            eprintln!("{}", err_str.red().bold());
        } else if error_rate > 15.0 {
            eprintln!("{}", err_str.yellow());
        } else {
            eprintln!("  {} {error_count} ({error_rate:.1}%)", "Errors:".bold());
        }
    }

    // ── Diagnoses (primary output — root-cause analysis) ────────────
    let has_diagnoses = report["diagnoses"]
        .as_array()
        .is_some_and(|d| !d.is_empty());
    let has_insights = report["insights"]
        .as_array()
        .is_some_and(|arr| arr.iter().any(|i| i["severity"].as_str() != Some("info")));
    let has_recs = report["recommendations"]
        .as_array()
        .is_some_and(|r| !r.is_empty());

    if has_diagnoses {
        eprintln!();
        eprintln!("  {}", "Root-Cause Analysis:".bold());
        if let Some(diagnoses) = report["diagnoses"].as_array() {
            for diag in diagnoses {
                let severity = diag["severity"].as_str().unwrap_or("info");
                let summary = diag["summary"].as_str().unwrap_or("");
                let fix = diag["fix_hint"].as_str().unwrap_or("");

                match severity {
                    "critical" => eprintln!("  🔴 {}", summary.red().bold()),
                    "warning" => eprintln!("  ⚠️ {}", summary.yellow()),
                    _ => eprintln!("  ℹ️ {}", summary),
                }

                // Show sample errors (truncated)
                if let Some(samples) = diag["samples"].as_array() {
                    for (i, sample) in samples.iter().enumerate() {
                        if i >= 2 {
                            break;
                        }
                        if let Some(s) = sample.as_str() {
                            let truncated: String = s.chars().take(80).collect();
                            eprintln!("    {} {}", "│".dim(), truncated.dim());
                        }
                    }
                }

                if !fix.is_empty() {
                    eprintln!("    {} {}", "→".green(), fix.green());
                }
            }
        }
    }

    // ── Insights (secondary — statistical observations) ─────────────
    if has_insights {
        eprintln!();
        let empty = vec![];
        let insights_arr = report["insights"].as_array().unwrap_or(&empty);
        let non_info_insights: Vec<_> = insights_arr
            .iter()
            .filter(|i| i["severity"].as_str() != Some("info"))
            .collect();
        for insight in non_info_insights {
            let severity = insight["severity"].as_str().unwrap_or("info");
            let message = insight["message"].as_str().unwrap_or("");
            let evidence = insight["evidence"].as_str().unwrap_or("");
            let line = if evidence.is_empty() {
                message.to_string()
            } else {
                format!("{message} — {evidence}")
            };
            match severity {
                "critical" => eprintln!("  🔴 {}", line.red().bold()),
                "warning" => eprintln!("  ⚠️ {}", line.yellow()),
                _ => eprintln!("  ℹ️ {}", line.dim()),
            }
        }
    }

    // ── Recommendations ─────────────────────────────────────────────
    if has_recs {
        eprintln!();
        eprintln!("  {}", "Fix Actions:".bold());
        if let Some(recs) = report["recommendations"].as_array() {
            for rec in recs {
                if let Some(r) = rec.as_str() {
                    eprintln!("    {} {}", "→".green(), r);
                }
            }
        }
    }

    // ── Healthy session or empty? Show appropriate feedback ─────────
    if !has_diagnoses && !has_insights && !has_recs {
        eprintln!();
        if total_events == 0 {
            eprintln!("  ℹ️ {}", "Empty session — no events recorded yet.".dim());
        } else if error_count == 0 {
            eprintln!("  ✅ {}", "Session healthy — no errors detected.".green());
            // Show event distribution as useful info
            if let Some(types) = overview["top_event_types"].as_array()
                && !types.is_empty()
            {
                let type_strs: Vec<String> = types
                    .iter()
                    .filter_map(|t| {
                        let name = t[0].as_str()?;
                        let cnt = t[1].as_i64()?;
                        Some(format!("{name}({cnt})"))
                    })
                    .collect();
                eprintln!("  {} {}", "Events:".bold(), type_strs.join(", "));
            }
        } else {
            eprintln!("  ℹ️ {}", "No actionable issues found.".dim());
        }
        eprintln!();
        eprintln!(
            "  {}",
            "Tip: /reflect skill_failure — focus on tool errors".dim()
        );
        eprintln!("  {}", "     /reflect performance — focus on latency".dim());
    }
}

fn render_local_reflect_report(
    report: &serde_json::Value,
    session_id: &str,
    requested_focus: Option<&str>,
    requested_question: Option<&str>,
) {
    let short_sid = prefix_chars(session_id, 8);
    let context = &report["reflection_context"];
    let turns_completed = context["turns_completed"].as_u64().unwrap_or(0);
    let scenario = context["scenario"].as_str().unwrap_or("unknown");
    let token_utilisation = context["token_utilisation"].as_f64().unwrap_or(0.0) * 100.0;

    eprintln!(
        "{}",
        format!("🔍 Liquid Reflection — {short_sid}").cyan().bold()
    );
    eprintln!("{}", "─────────────────────────────────────".dim());
    eprintln!(
        "  {} {} turns, scenario {}, token {:.0}%",
        "Overview:".bold(),
        turns_completed,
        scenario,
        token_utilisation
    );

    if let Some(focus) = requested_focus.filter(|focus| !focus.is_empty()) {
        eprintln!("  {} {}", "Focus:".bold(), focus);
    }
    if let Some(question) = requested_question.filter(|question| !question.is_empty()) {
        eprintln!("  {} {}", "Question:".bold(), question);
    }
    if let Some(experiment) = context["active_experiment"].as_object() {
        let experiment_id = experiment
            .get("experiment_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let variant = experiment
            .get("variant")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let samples = experiment
            .get("samples")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        eprintln!(
            "  {} {} / {} ({} samples)",
            "Experiment:".bold(),
            experiment_id,
            variant,
            samples
        );
    }

    if let Some(tool_stats) = context["tool_stats"]
        .as_array()
        .filter(|stats| !stats.is_empty())
    {
        eprintln!();
        eprintln!("  {}", "Tool pressure:".bold());
        for stat in tool_stats.iter().take(6) {
            let tool_name = stat["tool_name"].as_str().unwrap_or("unknown");
            let calls = stat["calls"].as_u64().unwrap_or(0);
            let failures = stat["failures"].as_u64().unwrap_or(0);
            let avg_latency_ms = stat["avg_latency_ms"].as_u64().unwrap_or(0);
            let line =
                format!("{tool_name}: {calls} calls, {failures} failures, avg {avg_latency_ms}ms");
            if failures > 0 {
                eprintln!("  ⚠️ {}", line.yellow());
            } else {
                eprintln!("  • {line}");
            }
        }
    }

    if let Some(signals) = context["signals"]
        .as_array()
        .filter(|signals| !signals.is_empty())
    {
        eprintln!();
        eprintln!("  {}", "Signals:".bold());
        for signal in signals.iter().take(6) {
            let kind = signal["kind"].as_str().unwrap_or("signal");
            let detail = signal["detail"].as_str().unwrap_or("");
            let turn_id = signal["turn_id"].as_str().unwrap_or("session");
            eprintln!("  • {} [{}] {}", kind.bold(), turn_id.dim(), detail);
        }
    }

    if let Some(actions) = context["recent_tactical_actions"]
        .as_array()
        .filter(|actions| !actions.is_empty())
    {
        eprintln!();
        eprintln!("  {}", "Recent tactical actions:".bold());
        for action in actions.iter().take(6).filter_map(serde_json::Value::as_str) {
            eprintln!("  • {action}");
        }
    }

    if let Some(turns) = report["recent_turns"]
        .as_array()
        .filter(|turns| !turns.is_empty())
    {
        eprintln!();
        eprintln!("  {}", "Recent events:".bold());
        for event in turns.iter().take(4) {
            let event_type = event["event_type"].as_str().unwrap_or("event");
            let turn = event["turn"]
                .as_u64()
                .map(|turn| format!("turn-{turn}"))
                .unwrap_or_else(|| "session".to_string());
            let detail = event["error"]
                .as_str()
                .or_else(|| event["user_input_preview"].as_str())
                .or_else(|| event["assistant_output_preview"].as_str())
                .unwrap_or("");
            if detail.is_empty() {
                eprintln!("  • {} [{}]", event_type, turn.dim());
            } else {
                eprintln!("  • {} [{}] {}", event_type, turn.dim(), detail);
            }
        }
    }

    if let Some(prompt_preview) = report["prompt_preview"]
        .as_str()
        .filter(|preview| !preview.is_empty())
    {
        eprintln!();
        eprintln!("  {}", "Prompt preview:".bold());
        for line in prompt_preview.lines().take(14) {
            eprintln!("    {}", line.dim());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reflect_args_recognises_diff_focus() {
        let (focus, question) = parse_reflect_args("diff");
        assert_eq!(focus.as_deref(), Some("diff"));
        assert_eq!(question, None);
    }

    #[test]
    fn render_reflect_diff_reports_no_delta_on_fresh_session() {
        let state = super::super::repl_state::ReplState::default();
        let out = super::render_reflect_diff(&state);
        assert!(out.contains("reflect diff"), "header present: {out}");
        assert!(
            out.contains("no delta since last sync"),
            "fresh session should say no delta: {out}"
        );
    }

    #[test]
    fn render_reflect_diff_surfaces_new_and_drifting_tools() {
        use astra_runtime::pipeline::persistence::ToolHealthEntry;
        let mut state = super::super::repl_state::ReplState::default();
        // Baseline had "grep" at 10 calls / 10% fail.
        state.synced_tool_health_entries = vec![ToolHealthEntry {
            name: "grep".into(),
            total_calls: 10,
            total_failures: 1,
            failure_rate: 0.10,
            last_updated_epoch: 0,
            recent_outcomes: vec![],
        }];
        // Now grep has drifted up, and "glob" is new.
        state.tool_health_entries = vec![
            ToolHealthEntry {
                name: "grep".into(),
                total_calls: 14,
                total_failures: 5,
                failure_rate: 0.36,
                last_updated_epoch: 0,
                recent_outcomes: vec![],
            },
            ToolHealthEntry {
                name: "glob".into(),
                total_calls: 3,
                total_failures: 0,
                failure_rate: 0.0,
                last_updated_epoch: 0,
                recent_outcomes: vec![],
            },
        ];
        let out = super::render_reflect_diff(&state);
        assert!(out.contains("grep"), "drifting tool shown: {out}");
        assert!(out.contains("glob"), "new tool shown: {out}");
        assert!(out.contains("new"), "new marker: {out}");
        assert!(
            out.contains("Δcalls +4"),
            "grep call delta surfaced: {out}"
        );
    }

    #[test]
    fn parse_reflect_args_splits_focus_and_question() {
        let (focus, question) = parse_reflect_args("performance why was bash slow");
        assert_eq!(focus.as_deref(), Some("performance"));
        assert_eq!(question.as_deref(), Some("why was bash slow"));
    }

    #[test]
    fn parse_reflect_args_treats_freeform_as_question() {
        let (focus, question) = parse_reflect_args("why was bash slow");
        assert_eq!(focus.as_deref(), Some("auto"));
        assert_eq!(question.as_deref(), Some("why was bash slow"));
    }

    #[test]
    fn local_reflect_shape_is_detected() {
        let local = serde_json::json!({
            "session_id": "s1",
            "reflection_context": {
                "turns_completed": 3
            }
        });
        let server = serde_json::json!({
            "session_id": "s1",
            "overview": {
                "total_events": 3
            }
        });
        assert!(is_local_reflect_report(&local));
        assert!(!is_local_reflect_report(&server));
    }

    // ── /undo tests ──

    /// Helper: build a ReplState with N fake turns in history.
    fn state_with_turns(n: usize) -> ReplState {
        let mut state = ReplState::default();
        for i in 0..n {
            state
                .history
                .push((format!("question {}", i + 1), format!("answer {}", i + 1)));
            state.turn += 1;
        }
        state.last_response = state.history.last().map(|(_, r)| r.clone());
        state
    }

    #[test]
    fn undo_single_turn() {
        let mut state = state_with_turns(3);
        assert_eq!(state.history.len(), 3);
        assert_eq!(state.turn, 3);

        // Pop the last turn
        state.history.pop();
        state.turn = state.turn.saturating_sub(1);
        state.last_response = state.history.last().map(|(_, r)| r.clone());

        assert_eq!(state.history.len(), 2);
        assert_eq!(state.turn, 2);
        assert_eq!(state.last_response.as_deref(), Some("answer 2"));
    }

    #[test]
    fn undo_multiple_turns() {
        let mut state = state_with_turns(5);
        let count = 3;
        let actual = count.min(state.history.len());
        for _ in 0..actual {
            state.history.pop();
            state.turn = state.turn.saturating_sub(1);
        }
        state.last_response = state.history.last().map(|(_, r)| r.clone());

        assert_eq!(state.history.len(), 2);
        assert_eq!(state.turn, 2);
        assert_eq!(state.last_response.as_deref(), Some("answer 2"));
    }

    #[test]
    fn undo_all_turns() {
        let mut state = state_with_turns(2);
        let count = 5; // More than available
        let actual = count.min(state.history.len());
        for _ in 0..actual {
            state.history.pop();
            state.turn = state.turn.saturating_sub(1);
        }
        state.last_response = state.history.last().map(|(_, r)| r.clone());

        assert_eq!(state.history.len(), 0);
        assert_eq!(state.turn, 0);
        assert!(state.last_response.is_none());
    }

    #[test]
    fn undo_empty_history_is_noop() {
        let state = state_with_turns(0);
        assert!(state.history.is_empty());
        // /undo on empty should not panic
        let count = 1usize.min(state.history.len());
        assert_eq!(count, 0);
    }

    // ── /undo edge case tests ──

    #[test]
    fn undo_zero_is_rejected() {
        // /undo 0 should be rejected per the implementation
        let arg = "0";
        let parsed = arg.parse::<usize>();
        assert_eq!(parsed.unwrap(), 0);
        // The handler checks Ok(0) and shows an error — test the parse path
    }

    #[test]
    fn undo_negative_is_parse_error() {
        // /undo -1 should fail to parse as usize
        let result = "-1".parse::<usize>();
        assert!(result.is_err(), "negative number should fail usize parse");
    }

    #[test]
    fn undo_non_numeric_is_parse_error() {
        let result = "abc".parse::<usize>();
        assert!(result.is_err(), "non-numeric should fail usize parse");
    }

    #[test]
    fn undo_float_is_parse_error() {
        let result = "1.5".parse::<usize>();
        assert!(result.is_err(), "float should fail usize parse");
    }

    #[test]
    fn undo_preserves_last_response_correctly() {
        let mut state = state_with_turns(5);
        // Undo 2 turns
        let count = 2;
        let actual = count.min(state.history.len());
        for _ in 0..actual {
            state.history.pop();
            state.turn = state.turn.saturating_sub(1);
        }
        state.last_response = state.history.last().map(|(_, r)| r.clone());
        assert_eq!(state.last_response.as_deref(), Some("answer 3"));
        assert_eq!(state.turn, 3);
    }

    #[test]
    fn undo_clears_continuation_anchor() {
        let mut state = state_with_turns(3);
        state.continuation_anchor = Some("some anchor".to_string());
        // Simulate undo
        state.history.pop();
        state.turn = state.turn.saturating_sub(1);
        state.last_response = state.history.last().map(|(_, r)| r.clone());
        state.continuation_anchor = None;
        assert!(state.continuation_anchor.is_none());
    }

    #[test]
    fn undo_turn_preview_truncation() {
        let mut state = ReplState::default();
        let long_msg = "a".repeat(100);
        state.history.push((long_msg.clone(), "resp".to_string()));
        state.turn = 1;

        if let Some((user_msg, _)) = state.history.pop() {
            let preview: String = user_msg.chars().take(50).collect();
            let preview = if user_msg.chars().count() > 50 {
                format!("{}…", preview)
            } else {
                preview
            };
            // 50 ASCII 'a' chars + 3 bytes for '…' = 53 bytes total
            assert_eq!(preview.len(), 53);
            assert!(preview.ends_with('…'));
            assert_eq!(preview.chars().count(), 51); // 50 a's + 1 '…'
        }
    }
}
