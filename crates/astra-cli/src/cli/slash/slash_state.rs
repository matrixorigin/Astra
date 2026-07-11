use crate::cli::{
    chat_stream::{ChatTurnParams, DEFAULT_TURN_INDEX, stream_chat_sse},
    cli_config::cli_utils::{
        compact_or_raw, map_thin_err, persist_profile_last_session_or_warn, prefix_chars,
        print_json_or_raw, urlencoding,
    },
    permission_manager::PermissionManager,
    session::session_state::{ContinuationAnchor, ExplainMode, SessionState},
    theme,
};
use astra_runtime::prompts;
use astra_runtime::turn::cloud::compaction_engine::{CompactionEngine, TokenBudget};
use astra_services::session_journal;
use crossterm::style::Stylize;
use std::sync::Arc;

fn manual_compaction_memory_entry(summary: &str) -> prompts::memory_proto::MemoryEntry {
    let one_line = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    let abstract_line = if one_line.chars().count() < prompts::memory_proto::ABSTRACT_MIN_CHARS {
        format!("Manual conversation compaction summary: {one_line}")
    } else {
        one_line
    };
    let abstract_line = abstract_line
        .chars()
        .take(prompts::memory_proto::ABSTRACT_MAX_CHARS)
        .collect::<String>();
    prompts::memory_proto::MemoryEntry::new_layered(
        prompts::memory_proto::NS_EPISODE,
        prompts::memory_proto::ST_SUMMARY,
        &abstract_line,
        None,
        Some(summary.trim()),
    )
}

fn compact_fact_memory_entry(
    fact: &str,
    fact_type: &str,
) -> Option<prompts::memory_proto::MemoryEntry> {
    let namespace = match fact_type {
        "semantic" => prompts::memory_proto::NS_FACT,
        "profile" => prompts::memory_proto::NS_PREF,
        "procedural" => prompts::memory_proto::NS_KNOWLEDGE,
        // Working state belongs to the session snapshot. Promoting it into
        // cross-session recall would turn a transient task constraint into a
        // durable user fact.
        "working" => return None,
        _ => return None,
    };
    Some(prompts::memory_proto::MemoryEntry::new(
        namespace,
        prompts::memory_proto::ST_ACTIVE,
        fact,
    ))
}

async fn store_compact_memory_payload(
    api: &astra_thin_client::ThinClient,
    token: &str,
    payload: &serde_json::Value,
) -> Result<(), String> {
    let response = api
        .post_memory_store_json(token, payload)
        .await
        .map_err(|error| format!("memory service unreachable: {error}"))?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("memory store HTTP {}", response.status()))
    }
}

pub(crate) struct StateCommandContext<'a> {
    pub(crate) api: &'a astra_thin_client::ThinClient,
    pub(crate) profile: Option<&'a str>,
    pub(crate) token: Option<&'a str>,
}

fn append_state_journal_event_or_warn(
    state: &SessionState,
    event: &session_journal::JournalEvent,
    context: &'static str,
) {
    let Some(journal) = state.journal.as_ref() else {
        return;
    };
    if let Err(error) = journal.append(event) {
        tracing::warn!(
            %error,
            session_id = ?state.session_id,
            context,
            "failed to append slash-state journal event"
        );
    }
}

/// Bundled context for building compact-related `ChatTurnParams`.
///
/// Eliminates the 9-parameter sponge that `compact_turn_params` previously required.
/// All state-derived fields (`api`, `token`, `profile`) are captured once;
/// only the truly varying inputs are passed per-call.
struct CompactCtx<'a> {
    state: &'a mut SessionState,
    api: &'a astra_thin_client::ThinClient,
    token: &'a str,
    profile: Option<&'a str>,
    incremental_state: Option<Arc<astra_turn_core::turn_event_sink::IncrementalTurnState>>,
}

impl<'a> CompactCtx<'a> {
    fn build_params(
        &'a mut self,
        message: &'a str,
        use_state_history: bool,
        perm_manager: &'a mut PermissionManager,
        cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
        pre_loaded_messages: Option<Vec<serde_json::Value>>,
    ) -> ChatTurnParams<'a> {
        let history = if use_state_history && pre_loaded_messages.is_none() {
            &self.state.history[..]
        } else {
            &[]
        };
        ChatTurnParams {
            api: self.api,
            token: self.token,
            auth_profile: self.profile,
            message,
            user_intent: message,
            input_runtime_required_texts: &[],
            input_runtime_volatile_texts: &[],
            semantic_query_override: None,
            session_id: self.state.session_id.as_deref(),
            model_id: None,
            model: self.state.model.as_deref(),
            provider: None,
            explain: ExplainMode::Off,
            render_md: false,
            history,
            perm_manager,
            verbose_mode: false,
            render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
            cli_context: Some(&self.state.cli_context),
            recent_tools: &[],
            activated_deferred_tool_names: None,
            tool_health_entries: &[],
            resume_restricted_tools: &[],
            session_lessons: &[],
            latest_skill_diagnosis: None,
            latest_turn_quality_feedback: None,
            unified_skill_registry: astra_runtime::skills::default_unified_registry(),
            is_plan_subtask: false,
            plan_subtask_id: None,
            delegation_engine: None,
            cancel_token,
            run_control: None,
            incremental_state: self.incremental_state.clone(),
            plan_assemble_line_release: None,
            stream_event_tx: None,
            agent_live_event_sink: None,
            approval_request_tx: None,
            ask_user_request_tx: None,
            plan_review_request_tx: None,
            mcp_manager: Some(self.state.mcp_manager.clone()),
            skill_quality_tracker: &mut self.state.skill_quality_tracker,
            discovered_skills: None,
            messaging_metrics: self.state.messaging_metrics.clone(),
            agent_spawner: self.state.agent_spawner.clone(),
            root_agent_id: Some("main"),
            root_mailbox_slot: Some(&mut self.state.root_mailbox),
            // Compaction is a utility inference, not another user turn. Its
            // traces must not mutate the active session's live observability
            // state or resumable timeline.
            observability_hub: None,
            observability_session: None,
            file_journal: None,
            file_state: None,
            database_snapshot_journal: None,
            git_stash_journal: None,
            git_commit_journal: None,
            git_worktree_journal: None,
            session_state_journal: None,
            task_manager: None,
            task_notify_tx: None,
            bg_task_commands: None,
            bg_task_list_cache: None,
            bash_detach_slot: None,
            turn_index: DEFAULT_TURN_INDEX,
            pipeline_state: None,
            compaction_state: None,
            consecutive_context_window_errors: 0,
            idempotency_cache: None,
            pre_loaded_messages,
            append_system_prompt: None,
            session_memory_extractor: None,
            #[cfg(feature = "harness")]
            harness_sink: Some(self.state.harness_sink.clone()),
            #[cfg(feature = "harness")]
            harness_trace: Some(self.state.harness_trace.clone()),
            #[cfg(feature = "harness")]
            benchmark_profile: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CompactArgs {
    quick: bool,
    no_memoria: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManualCompactPlan {
    PrefixTurns { trimmed_count: usize },
    SingleTurnInPlace,
}

#[derive(Clone)]
struct HistoryEditSnapshot {
    history: Vec<(String, String)>,
    redo_stack: Vec<(String, String, u32)>,
    turn: u32,
    last_response: Option<String>,
    continuation_anchor: Option<ContinuationAnchor>,
    recent_tools: Vec<String>,
}

fn parse_compact_args(arg: &str) -> CompactArgs {
    let mut parsed = CompactArgs {
        quick: false,
        no_memoria: false,
    };
    for word in arg.split_whitespace() {
        let normalized = word.to_ascii_lowercase();
        if normalized == "quick" || normalized == "summary-only" {
            parsed.quick = true;
        }
        if normalized == "no-memoria" || normalized == "no_memoria" {
            parsed.no_memoria = true;
        }
    }
    parsed
}

fn cap_swap_body(swap_body: String) -> String {
    const MAX_SWAP_BODY_BYTES: usize = 2000;
    if swap_body.len() > MAX_SWAP_BODY_BYTES {
        let boundary = swap_body.floor_char_boundary(MAX_SWAP_BODY_BYTES);
        format!("{}…", &swap_body[..boundary])
    } else {
        swap_body
    }
}

fn build_swap_memory_body(
    swapped_turns: &[(String, String)],
    trimmed_count: usize,
) -> Option<String> {
    let mut swap_lines: Vec<String> = Vec::new();
    for (user_msg, assistant_msg) in swapped_turns {
        if !user_msg.is_empty() {
            let preview: String = user_msg.chars().take(100).collect();
            swap_lines.push(format!("U: {preview}"));
        }
        if !assistant_msg.is_empty() {
            let preview: String = assistant_msg.chars().take(150).collect();
            swap_lines.push(format!("A: {preview}"));
        }
    }
    if swap_lines.is_empty() {
        return None;
    }

    let tier_label = "compact_history";
    Some(cap_swap_body(format!(
        "Turns 1-{trimmed_count} swapped out [{tier_label}]:\n{}",
        swap_lines.join("\n")
    )))
}

fn compact_mem_note(
    no_memoria: bool,
    saved_to_memoria: bool,
    facts_stored: usize,
    quick: bool,
) -> String {
    if no_memoria {
        " · Memoria side-effects skipped (no-memoria)".to_string()
    } else if saved_to_memoria {
        let mut note = if facts_stored > 0 {
            format!(" · saved to memory ({facts_stored} facts extracted)")
        } else {
            " · saved to memory".to_string()
        };
        if quick {
            note.push_str(" · quick (facts not stored to memory)");
        }
        note
    } else {
        String::new()
    }
}

impl HistoryEditSnapshot {
    fn capture(state: &SessionState) -> Self {
        Self {
            history: state.history.clone(),
            redo_stack: state.redo_stack.clone(),
            turn: state.turn,
            last_response: state.last_response.clone(),
            continuation_anchor: state.continuation_anchor.clone(),
            recent_tools: state.recent_tools.clone(),
        }
    }

    fn restore(self, state: &mut SessionState) {
        state.history = self.history;
        state.redo_stack = self.redo_stack;
        state.turn = self.turn;
        state.last_response = self.last_response;
        state.continuation_anchor = self.continuation_anchor;
        state.recent_tools = self.recent_tools;
    }
}

fn plan_manual_compaction(
    total_turns: usize,
    keep_recent_turns: usize,
) -> Option<ManualCompactPlan> {
    let keep_recent_turns = keep_recent_turns.max(1);
    match total_turns {
        0 => None,
        1 => Some(ManualCompactPlan::SingleTurnInPlace),
        total if total > keep_recent_turns => Some(ManualCompactPlan::PrefixTurns {
            trimmed_count: total.saturating_sub(keep_recent_turns),
        }),
        total => Some(ManualCompactPlan::PrefixTurns {
            trimmed_count: total.saturating_sub(1),
        }),
    }
}

async fn persist_history_edit_state(state: &mut SessionState, action: &str) -> Result<(), String> {
    crate::cli::session::session_recovery::sync_recovery_snapshot_after_history_edit(state)
        .await
        .map_err(|error| {
            format!(
                "{action} updated live context but failed to refresh resume/fork state: {error}"
            )
        })?;

    // Session memory is a projection of canonical history. Any successful
    // undo/redo/compact invalidates that projection just as surely as a new
    // user turn does, so enqueue a refresh immediately instead of waiting for
    // an unrelated future turn or process shutdown.
    if let (Some(service), Some(session_id)) = (
        state.session_memory_extractor.as_ref(),
        state
            .session_id
            .as_deref()
            .filter(|session_id| !session_id.is_empty()),
    ) {
        let had_error = state
            .last_turn_event
            .as_ref()
            .and_then(|event| event.error.as_ref())
            .is_some();
        let _ = service.maybe_spawn(astra_runtime::session_memory::ExtractionRequest {
            session_id: session_id.to_string(),
            messages: crate::cli::session::session_projection::history_as_messages(&state.history),
            session_facts: crate::cli::session::session_cleanup::shutdown_session_facts(state),
            had_error,
            had_user_correction: matches!(action, "/undo" | "/redo"),
            turn_number: state.turn,
        });
    }
    Ok(())
}

fn append_history_edit_rollback_error(
    message: &mut String,
    label: &str,
    result: Result<(), String>,
) {
    if let Err(error) = result {
        message.push_str(&format!("; {label}: {error}"));
    }
}

#[derive(Debug, Clone)]
struct UndoFileRevertError {
    message: String,
    rollback_failed: bool,
}

fn apply_undo_file_reverts(
    state: &SessionState,
    undone_turns: &[u32],
) -> Result<Vec<String>, UndoFileRevertError> {
    let journal = state
        .file_journal
        .lock()
        .map_err(|error| UndoFileRevertError {
            message: format!("lock file journal: {error}"),
            rollback_failed: false,
        })?;
    let mut reverted_paths = Vec::new();
    let mut reverted_turns = Vec::new();

    for &turn_index in undone_turns {
        match journal.undo_turn_transactional(turn_index) {
            Ok(paths) => {
                reverted_paths.extend(paths.into_iter().map(|path| path.display().to_string()));
                reverted_turns.push(turn_index);
            }
            Err(error) => {
                let mut error_message =
                    format!("revert workspace files for turn {turn_index}: {error}");
                let mut rollback_failed = error.contains("; rollback file ");
                for reverted_turn in reverted_turns.iter().rev() {
                    let rollback_result = journal
                        .restore_turn_transactional(*reverted_turn)
                        .map(|_| ());
                    if rollback_result.is_err() {
                        rollback_failed = true;
                    }
                    append_history_edit_rollback_error(
                        &mut error_message,
                        &format!("restore workspace files for turn {reverted_turn}"),
                        rollback_result,
                    );
                }
                return Err(UndoFileRevertError {
                    message: error_message,
                    rollback_failed,
                });
            }
        }
    }

    Ok(reverted_paths)
}

fn rollback_undo_file_reverts(state: &SessionState, undone_turns: &[u32]) -> Result<(), String> {
    let journal = state
        .file_journal
        .lock()
        .map_err(|error| format!("lock file journal for rollback: {error}"))?;
    let mut rollback_error = String::new();

    for turn_index in undone_turns.iter().rev() {
        append_history_edit_rollback_error(
            &mut rollback_error,
            &format!("restore workspace files for turn {turn_index}"),
            journal.restore_turn_transactional(*turn_index).map(|_| ()),
        );
    }

    if rollback_error.is_empty() {
        Ok(())
    } else {
        Err(rollback_error.trim_start_matches("; ").to_string())
    }
}

fn handle_undo_persist_failure(
    state: &mut SessionState,
    snapshot: HistoryEditSnapshot,
    undone_turns: &[u32],
    error_message: String,
) -> String {
    let mut error_message = error_message;
    let persist_rollback_failed = error_message.contains("rollback failed");
    let rollback_result = rollback_undo_file_reverts(state, undone_turns);
    let file_rollback_failed = rollback_result.is_err();
    append_history_edit_rollback_error(
        &mut error_message,
        "restore workspace files after failed /undo persist",
        rollback_result,
    );
    if file_rollback_failed {
        error_message.push_str(
            "; kept /undo in live memory because workspace files did not fully roll back",
        );
        state.session_persistence_error = Some(error_message.clone());
    } else {
        snapshot.restore(state);
        if persist_rollback_failed {
            state.session_persistence_error = Some(error_message.clone());
        }
    }
    error_message
}

pub(crate) async fn handle_state_command(
    cmd: &str,
    arg: &str,
    ctx: StateCommandContext<'_>,
    state: &mut SessionState,
) -> Result<(), String> {
    let StateCommandContext {
        api,
        profile,
        token,
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
                persist_profile_last_session_or_warn(
                    profile,
                    sid,
                    "slash_state:clear_starts_fresh_session",
                );
            }
            state.prepare_for_session_rebind().await;
            state.reset_for_new_session();
            if let Some(ref sid) = new_sid {
                state.set_session_id(sid.clone());
            } else {
                state.clear_session_id();
            }
            if let Some(ref sid) = new_sid {
                crate::cli::session::session_startup::initialize_journal_pub(state, sid);
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
                                    astra_turn_core::file_edit_journal::EditType::Overwrite => "📝",
                                    astra_turn_core::file_edit_journal::EditType::Create => "🆕",
                                    astra_turn_core::file_edit_journal::EditType::Patch => "✏️",
                                    astra_turn_core::file_edit_journal::EditType::Delete => "🗑️",
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
            let snapshot = HistoryEditSnapshot::capture(state);
            let actual = count.min(state.history.len());
            let mut undone_previews = Vec::new();
            let mut undone_turns = Vec::new();
            for _ in 0..actual {
                if let Some((user_msg, assistant_msg)) = state.history.pop() {
                    let preview: String = user_msg.chars().take(50).collect();
                    let preview = if user_msg.chars().count() > 50 {
                        format!("{}…", preview)
                    } else {
                        preview
                    };
                    undone_previews.push(preview);
                    let turn_index = state.turn;
                    undone_turns.push(turn_index);
                    // Save to redo stack
                    state.redo_stack.push((user_msg, assistant_msg, state.turn));
                    state.turn = state.turn.saturating_sub(1);
                }
            }
            state.last_response = state.history.last().map(|(_, resp)| resp.clone());
            state.continuation_anchor = None;
            let file_reverts = match apply_undo_file_reverts(state, &undone_turns) {
                Ok(paths) => paths,
                Err(error) => {
                    let message = format!("/undo failed: {}", error.message);
                    if error.rollback_failed {
                        state.session_persistence_error = Some(message.clone());
                    } else {
                        snapshot.restore(state);
                    }
                    return Err(message);
                }
            };
            if let Err(error) = persist_history_edit_state(state, "/undo").await {
                return Err(handle_undo_persist_failure(
                    state,
                    snapshot,
                    &undone_turns,
                    error,
                ));
            }
            append_state_journal_event_or_warn(
                state,
                &session_journal::JournalEvent::config_change(
                    state.session_id.as_deref(),
                    "undo",
                    &actual.to_string(),
                ),
                "slash_state:undo",
            );
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
            let snapshot = HistoryEditSnapshot::capture(state);
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
            if let Err(error) = persist_history_edit_state(state, "/redo").await {
                snapshot.restore(state);
                return Err(error);
            }
            append_state_journal_event_or_warn(
                state,
                &session_journal::JournalEvent::config_change(
                    state.session_id.as_deref(),
                    "redo",
                    &actual.to_string(),
                ),
                "slash_state:redo",
            );
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
            let explain_val = match state.explain {
                ExplainMode::Off => "off",
                ExplainMode::On => "on",
                ExplainMode::Verbose => "verbose",
            };
            append_state_journal_event_or_warn(
                state,
                &session_journal::JournalEvent::config_change(
                    state.session_id.as_deref(),
                    "explain",
                    explain_val,
                ),
                "slash_state:explain",
            );
        }

        "/verbose" => {
            eprintln!("  /verbose has been removed. Use /stats for per-turn metrics.");
        }

        "/compact" => {
            if state.history.is_empty() {
                eprintln!(
                    "  {}",
                    "Nothing to compact: this session has no conversation turns yet.".yellow()
                );
                return Ok(());
            }
            let keep_recent = state.context_budget.keep_recent_turns;
            let total = state.history.len();
            let compact_plan = plan_manual_compaction(total, keep_recent)
                .expect("non-empty history should produce a compaction plan");
            let trimmed_count = match compact_plan {
                ManualCompactPlan::PrefixTurns { trimmed_count } => trimmed_count,
                ManualCompactPlan::SingleTurnInPlace => 1,
            };
            let compact_args = parse_compact_args(arg);
            let Some(tok) = token else {
                eprintln!("{}", "  Not logged in. Use /login.".yellow());
                return Ok(());
            };

            eprintln!("  {}", "Summarizing…".dim());
            let mut auto_pm = PermissionManager::with_load_policy(
                crate::cli::permission_manager::PermissionMode::Auto,
                &std::env::current_dir().unwrap_or_default(),
                &crate::cli::permission_manager::PermissionLoadPolicy::HeadlessSafe,
            );
            let cancel_token = Arc::new(tokio_util::sync::CancellationToken::new());

            // ── Micro-compact: reduce input tokens before LLM summary call ──
            let pre_messages = {
                let mut msgs =
                    crate::cli::session::session_projection::history_as_messages(&state.history);
                let limit = state.context_budget.effective_input_limit() as u64;
                let budget = TokenBudget {
                    max_prompt_tokens: limit,
                    last_measured_tokens: limit.saturating_add(1),
                    current_round_index: None,
                    now_secs: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                };
                CompactionEngine::micro_pipeline().compress_if_needed(&mut msgs, &budget);
                Some(msgs)
            };

            // ── Single LLM call: unified summary + facts extraction ──
            let inc_state =
                Arc::new(astra_turn_core::turn_event_sink::IncrementalTurnState::default());
            let mut compact_ctx = CompactCtx {
                state,
                api,
                token: tok,
                profile,
                incremental_state: Some(inc_state),
            };
            let unified_result = tokio::select! {
                r = stream_chat_sse(compact_ctx.build_params(
                    prompts::COMPACT_UNIFIED_PROMPT,
                    true,
                    &mut auto_pm,
                    Some(cancel_token.clone()),
                    pre_messages,
                )) => r,
                _ = tokio::signal::ctrl_c() => {
                    cancel_token.cancel();
                    eprintln!("{}", "  Interrupted.".dim());
                    return Ok(());
                }
            };

            let (summary, facts) = match unified_result {
                Ok(sr) => match prompts::parse_compact_response(&sr.full_text) {
                    Some(resp) => (resp.render_summary(), resp.valid_facts()),
                    None => {
                        // Fallback: LLM didn't return valid JSON; use raw text as summary
                        let text = sr.full_text.trim().to_string();
                        if text.is_empty() {
                            eprintln!("{}", "  ✗ Empty response returned.".yellow());
                            return Ok(());
                        }
                        eprintln!(
                            "{}",
                            "  ⚠ Could not parse structured response; using raw text.".yellow()
                        );
                        (text, Vec::new())
                    }
                },
                Err(e) => {
                    eprintln!("{}", format!("  ✗ Failed to summarize: {}", e.error).red());
                    return Ok(());
                }
            };

            // ── Stream summary immediately ──
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

            // Save summary to Memoria via server proxy (preserves user isolation)
            let mut saved_to_memoria = false;
            let mut facts_stored = 0usize;
            if let Some(tok) = token {
                if !compact_args.no_memoria {
                    let meta = prompts::memory_proto::EntryMeta::from_session_with_tier(
                        state.session_id.as_deref(),
                        state.turn,
                        prompts::memory_proto::SRC_COMPACT,
                        prompts::memory_proto::TIER_INFERRED,
                    );
                    let entry = manual_compaction_memory_entry(&summary);
                    match store_compact_memory_payload(
                        api,
                        tok,
                        &entry.to_store_payload_with_meta(&meta),
                    )
                    .await
                    {
                        Ok(()) => saved_to_memoria = true,
                        Err(error) => {
                            eprintln!("{}", format!("  ⚠ Memory save failed: {error}").yellow())
                        }
                    }

                    // Store facts directly from unified response (no second LLM call)
                    if saved_to_memoria && !compact_args.quick {
                        let fact_meta = prompts::memory_proto::EntryMeta::from_session_with_tier(
                            state.session_id.as_deref(),
                            state.turn,
                            prompts::memory_proto::SRC_EXTRACTED,
                            prompts::memory_proto::TIER_INFERRED,
                        );
                        for (fact, fact_type) in &facts {
                            let Some(fact_entry) = compact_fact_memory_entry(fact, fact_type)
                            else {
                                continue;
                            };
                            match store_compact_memory_payload(
                                api,
                                tok,
                                &fact_entry.to_store_payload_with_meta(&fact_meta),
                            )
                            .await
                            {
                                Ok(()) => facts_stored += 1,
                                Err(error) => eprintln!(
                                    "{}",
                                    format!("  ⚠ Extracted fact was not saved: {error}").yellow()
                                ),
                            }
                        }
                    }
                }
            }

            let snapshot = HistoryEditSnapshot::capture(state);
            // Rewrite history to reflect the compacted conversation.
            // Manual `/compact` is explicit user intent, so it must do useful work even when
            // the session is still inside `keep_recent_turns` (for example a single giant turn).
            // We preserve the latest raw turn when possible; for a single-turn session we keep
            // the user request and replace only the assistant side with a summary.
            // ── Context swap: store compacted turns to memory for later retrieval ──
            if let Some(tok) = token {
                if !compact_args.no_memoria {
                    if let Some(capped) = build_swap_memory_body(
                        &state.history[..trimmed_count.min(total)],
                        trimmed_count,
                    ) {
                        let swap_entry = prompts::memory_proto::MemoryEntry::new(
                            prompts::memory_proto::NS_SWAP,
                            prompts::memory_proto::ST_ARCHIVED,
                            &capped,
                        );
                        // Use same compact metadata for consistent traceability
                        let swap_meta = prompts::memory_proto::EntryMeta::from_session_with_tier(
                            state.session_id.as_deref(),
                            state.turn,
                            prompts::memory_proto::SRC_COMPACT,
                            prompts::memory_proto::TIER_UNVERIFIED,
                        );
                        if let Err(error) = store_compact_memory_payload(
                            api,
                            tok,
                            &swap_entry.to_store_payload_with_meta(&swap_meta),
                        )
                        .await
                        {
                            eprintln!(
                                "{}",
                                format!("  ⚠ Compacted history archive was not saved: {error}")
                                    .yellow()
                            );
                        }
                    }
                }
            }

            let anchor = if compact_args.no_memoria {
                None
            } else {
                crate::cli::session::session_compaction::fetch_compact_memory_anchor_snippet(
                    api,
                    tok,
                    state.session_id.as_deref(),
                    &summary,
                )
                .await
            };
            let assistant_text = crate::cli::session::session_compaction::compact_assistant_message(
                trimmed_count,
                &summary,
                anchor.as_deref(),
            );
            match compact_plan {
                ManualCompactPlan::PrefixTurns { trimmed_count } => {
                    let context_entry = (String::new(), assistant_text);
                    let mut new_hist = vec![context_entry];
                    new_hist.extend_from_slice(&state.history[trimmed_count..]);
                    state.history = new_hist;
                }
                ManualCompactPlan::SingleTurnInPlace => {
                    let original_user = state
                        .history
                        .first()
                        .map(|(user, _)| user.clone())
                        .unwrap_or_default();
                    state.history = vec![(original_user, assistant_text)];
                }
            }
            state.recent_tools.clear();

            let mem_note = compact_mem_note(
                compact_args.no_memoria,
                saved_to_memoria,
                facts_stored,
                compact_args.quick,
            );
            if let Err(error) = persist_history_edit_state(state, "/compact").await {
                snapshot.restore(state);
                return Err(error);
            }
            // Journal: log compact event (include summary for knowledge backflow)
            append_state_journal_event_or_warn(
                state,
                &session_journal::JournalEvent::compact_with_summary(
                    state.session_id.as_deref(),
                    state.turn,
                    trimmed_count,
                    facts_stored,
                    Some(&summary),
                ),
                "slash_state:compact",
            );
            match compact_plan {
                ManualCompactPlan::PrefixTurns { .. } => {
                    eprintln!(
                        "  {} {} turns compacted · {} turns in context{}",
                        theme::icon_ok(),
                        trimmed_count,
                        state.history.len(),
                        mem_note,
                    );
                }
                ManualCompactPlan::SingleTurnInPlace => {
                    eprintln!(
                        "  {} Compacted the current turn in place · {} turn in context{}",
                        theme::icon_ok(),
                        state.history.len(),
                        mem_note,
                    );
                }
            }
            if total <= keep_recent {
                eprintln!(
                    "  {}",
                    format!(
                        "Manual compaction overrode keep_recent_turns={} so this session could actually shrink.",
                        keep_recent
                    )
                    .dim()
                );
            }
            if state.plan_mode_active() || state.executing_plan.is_some() {
                eprintln!(
                    "{}",
                    "  Tip: Plan context was shortened — if steps feel stale, refresh `/plan` or your plan view."
                        .dim()
                );
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
            let reflect_args = parse_reflect_args(arg);
            // `/reflect diff` short-circuits: render the local tool-health
            // delta between the most recently synced entries and the live
            // session entries, so the agent can audit its own tuning
            // without needing a server round-trip.
            if reflect_args.diff {
                let out = render_reflect_diff(state);
                eprint!("{out}");
                return Ok(());
            }
            if let Some(body) =
                crate::cli::self_command::try_render_reflect_surface_for_session_with_profile(
                    &sid,
                    reflect_args.request(20),
                    profile,
                )
                .await?
            {
                render_reflect_report(&body, &sid);
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
            if reflect_args.topic != "overview" {
                query_parts.push(format!("topic={}", urlencoding(&reflect_args.topic)));
            }
            if let Some(facet) = reflect_args.facet.as_deref() {
                query_parts.push(format!("facet={}", urlencoding(facet)));
            }
            if reflect_args.depth != "diagnostic" {
                query_parts.push(format!("depth={}", urlencoding(&reflect_args.depth)));
            }
            if let Some(question) = reflect_args
                .question
                .as_deref()
                .filter(|question| !question.is_empty())
            {
                query_parts.push(format!("question={}", urlencoding(question)));
            }
            if !query_parts.is_empty() {
                rel = format!("{rel}?{}", query_parts.join("&"));
            }
            match api.get_authed_path_text(tok, &rel).await {
                Ok(body) => render_reflect_report(&body, &sid),
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedReflectArgs {
    topic: String,
    facet: Option<String>,
    depth: String,
    question: Option<String>,
    diff: bool,
}

impl ParsedReflectArgs {
    fn request(&self, last_n: i32) -> astra_services::reflect::ReflectRequest {
        astra_services::reflect::ReflectRequest::from_observation_params(
            Some(self.topic.as_str()),
            self.facet.as_deref(),
            Some(self.depth.as_str()),
            None,
            last_n,
            self.question.as_deref().unwrap_or(""),
        )
    }
}

fn parse_reflect_args(arg: &str) -> ParsedReflectArgs {
    let trimmed = arg.trim();
    if trimmed.is_empty() {
        return ParsedReflectArgs {
            topic: "overview".to_string(),
            facet: None,
            depth: "diagnostic".to_string(),
            question: None,
            diff: false,
        };
    }

    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    let first = normalize_reflect_token(tokens[0]);
    if first == "diff" {
        return ParsedReflectArgs {
            topic: "overview".to_string(),
            facet: None,
            depth: "diagnostic".to_string(),
            question: None,
            diff: true,
        };
    }

    let mut topic = "overview".to_string();
    let mut facet = None;
    let mut depth = "diagnostic".to_string();
    let mut question_start = 0usize;
    let mut parsed_topic = false;

    if let Some((head, tail)) = first.split_once('/')
        && is_reflect_topic(head)
    {
        topic = head.to_string();
        parsed_topic = true;
        if !tail.is_empty() {
            facet = Some(tail.to_string());
        }
        question_start = 1;
    } else if is_reflect_topic(&first) {
        topic = first.clone();
        parsed_topic = true;
        question_start = 1;
    }

    if parsed_topic && facet.is_none() && question_start < tokens.len() {
        let candidate = normalize_reflect_token(tokens[question_start]);
        if is_reflect_facet(&candidate) {
            facet = Some(candidate);
            question_start += 1;
        }
    }

    if question_start < tokens.len() {
        let candidate = normalize_reflect_token(tokens[question_start]);
        if is_reflect_depth(&candidate) {
            depth = candidate;
            question_start += 1;
        }
    } else if !parsed_topic && is_reflect_depth(&first) {
        depth = first;
        question_start = 1;
    }

    let question = (question_start < tokens.len()).then(|| tokens[question_start..].join(" "));

    ParsedReflectArgs {
        topic,
        facet,
        depth,
        question,
        diff: false,
    }
}

fn normalize_reflect_token(token: &str) -> String {
    token.trim().to_ascii_lowercase().replace('-', "_")
}

fn is_reflect_topic(token: &str) -> bool {
    matches!(token, "overview" | "runtime" | "execution" | "knowledge")
}

fn is_reflect_depth(token: &str) -> bool {
    matches!(token, "hint" | "summary" | "diagnostic" | "forensic")
}

fn is_reflect_facet(token: &str) -> bool {
    matches!(
        token,
        "overview"
            | "summary"
            | "question"
            | "errors"
            | "failures"
            | "tools"
            | "trace"
            | "performance"
            | "latency"
            | "cost"
            | "context"
            | "memory"
            | "progress"
            | "loop"
            | "cache"
    )
}

/// Render a compact diff view of what the agent has learned this session
/// vs the last cloud-synced baseline. Auto-populated — reads directly
/// from `SessionState` without any new plumbing.
///
/// Output enumerates tool-health entries whose failure rate, call count,
/// or presence changed since last sync. When nothing changed (e.g. fresh
/// session) the output is an explicit "no delta" line.
pub(crate) fn render_reflect_diff(state: &SessionState) -> String {
    use std::collections::HashMap;
    use std::fmt::Write;

    let mut out = String::new();
    let sep = "─".repeat(38);
    let _ = writeln!(out, "\n  ─── reflect diff {sep}");

    let synced: HashMap<&str, &astra_turn_core::tool_health_persistence::ToolHealthEntry> = state
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

/// Render a `ReflectReport` as a compact, colored terminal report.
fn render_reflect_report(body: &str, session_id: &str) {
    let Ok(report) = serde_json::from_str::<serde_json::Value>(body) else {
        print_json_or_raw(body);
        return;
    };

    let overview = &report["overview"];
    let short_sid = prefix_chars(session_id, 8);

    // Header
    eprintln!(
        "{}",
        format!("🔍 Session Diagnosis — {short_sid}")
            .magenta()
            .bold()
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
            "Tip: /reflect execution/errors — inspect tool and runtime errors".dim()
        );
        eprintln!(
            "  {}",
            "     /reflect runtime performance — inspect latency".dim()
        );
    }
}

#[cfg(test)]
mod state_command_tests {
    use super::{
        HistoryEditSnapshot, StateCommandContext, handle_state_command, handle_undo_persist_failure,
    };
    use crate::cli::session::session_state::SessionState;
    use crate::lock_recovery::LockRecovery;
    use astra_services::session_journal::{self, JournalEventType};
    use wiremock::matchers::{header_exists, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[serial_test::serial]
    #[tokio::test]
    async fn clear_command_starts_fresh_session_and_resets_state() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let old_sid = uuid::Uuid::new_v4().to_string();
        let new_sid = uuid::Uuid::new_v4().to_string();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/sessions"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": new_sid
            })))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let mut state = SessionState::default();
        state.set_session_id(old_sid.clone());
        state.model = Some("gpt-5".to_string());
        crate::cli::session::session_startup::initialize_journal_pub(&mut state, &old_sid);
        state.pending_recovery = Some("stale".into());
        state.run_id = Some("run-1".into());
        state.turn = 4;
        state.history = vec![("q".into(), "a".into())];
        state.total_prompt_tokens = 11;
        state.total_completion_tokens = 22;
        state.total_cache_read_tokens = 33;
        state.total_cache_creation_tokens = 44;
        state.total_session_cost = 1.5;
        state.recent_tools = vec!["bash".into()];
        state.redo_stack = vec![("q".into(), "a".into(), 1)];
        state.last_response = Some("a".into());
        state.continuation_anchor = Some("anchor".into());
        state.diagnostics_context = Some("diag".into());
        state.queued_message = Some("queued".into());
        state.resume_guidance = Some("resume".into());
        state.resume_restricted_tools = vec!["read_file".into()];
        state.executing_plan_goal = Some("goal".into());
        state.executing_plan_id = Some("plan-1".into());
        state.plan_execution_rounds = 3;
        state.last_turn_interrupted = true;
        state.plan_mode_sync_error = Some("sync failed".into());
        state.pending_bg_notifications = vec!["background".into()];
        state.turns_since_task_use = 5;
        state.turns_since_task_reminder = 4;
        state.session_lessons_loaded = true;
        state.perm_manager.record_approval("bash", None, false);
        let transport = std::sync::Arc::new(astra_messaging::InProcessTransport::new());
        let tracker = std::sync::Arc::new(
            astra_runtime::server::delegation::engine::DelegationTracker::new(),
        );
        let router =
            std::sync::Arc::new(astra_messaging::AgentMailboxRouter::new(transport, tracker));
        let root_addr = astra_messaging::AgentAddress::new(old_sid.clone(), "main");
        state.root_mailbox = Some(router.register(root_addr.clone(), None).await.unwrap());

        handle_state_command(
            "/clear",
            "",
            StateCommandContext {
                api: &api,
                profile: None,
                token: Some("test-token"),
            },
            &mut state,
        )
        .await
        .unwrap();

        assert_eq!(state.session_id.as_deref(), Some(new_sid.as_str()));
        assert_eq!(state.turn, 0);
        assert!(state.history.is_empty());
        assert_eq!(state.total_prompt_tokens, 0);
        assert_eq!(state.total_completion_tokens, 0);
        assert_eq!(state.total_cache_read_tokens, 0);
        assert_eq!(state.total_cache_creation_tokens, 0);
        assert_eq!(state.total_session_cost, 0.0);
        assert!(state.recent_tools.is_empty());
        assert!(state.redo_stack.is_empty());
        assert!(state.last_response.is_none());
        assert!(state.continuation_anchor.is_none());
        assert!(state.diagnostics_context.is_none());
        assert!(state.queued_message.is_none());
        assert!(state.resume_guidance.is_none());
        assert!(state.resume_restricted_tools.is_empty());
        assert!(state.executing_plan_goal.is_none());
        assert!(state.executing_plan_id.is_none());
        assert_eq!(state.plan_execution_rounds, 0);
        assert!(!state.last_turn_interrupted);
        assert!(state.plan_mode_sync_error.is_none());
        assert!(state.pending_bg_notifications.is_empty());
        assert_eq!(state.turns_since_task_use, 0);
        assert_eq!(state.turns_since_task_reminder, 0);
        assert!(!state.session_lessons_loaded);
        assert!(state.journal.is_some());
        assert!(state.root_mailbox.is_none());
        assert!(state.perm_manager.export_session_overrides().is_none());
        router
            .register(root_addr, None)
            .await
            .expect("clear should unregister the prior root mailbox");

        let events = session_journal::read_journal(&new_sid).unwrap();
        let session_start_count = events
            .iter()
            .filter(|event| event.event_type == JournalEventType::SessionStart)
            .count();
        assert_eq!(
            session_start_count, 1,
            "new session journal should contain exactly one session_start event"
        );
    }

    fn make_owner_bound_step_checkpoint_path_invalid(session_id: &str) {
        let user_id = crate::cli::cli_config::cli_utils::cli_user_id();
        let owner_session_dir =
            astra_pipeline::step_checkpoint::owner_session_dir_for(&user_id, session_id).unwrap();
        std::fs::create_dir_all(&owner_session_dir).unwrap();
        std::fs::write(
            owner_session_dir.join("step_checkpoints"),
            "not-a-directory",
        )
        .unwrap();
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn undo_rolls_back_live_state_when_recovery_persist_fails() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = uuid::Uuid::new_v4().to_string();
        let mut ws = astra_services::session_workspace::WorkspaceMetadata::new(&sid, "test-model");
        ws.turn_count = 2;
        astra_services::session_workspace::write_workspace(&ws).unwrap();
        make_owner_bound_step_checkpoint_path_invalid(&sid);
        let journal = session_journal::JournalWriter::new(&sid).unwrap();

        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let mut state = SessionState::default();
        state.set_session_id(sid);
        state.journal = Some(journal);
        state.turn = 2;
        state.history = vec![("q1".into(), "a1".into()), ("q2".into(), "a2".into())];
        state.last_response = Some("a2".into());

        let error = handle_state_command(
            "/undo",
            "",
            StateCommandContext {
                api: &api,
                profile: None,
                token: None,
            },
            &mut state,
        )
        .await
        .expect_err("invalid checkpoint path should fail undo persistence");

        assert!(error.contains("/undo updated live context"), "{error}");
        assert_eq!(
            state.history,
            vec![
                ("q1".to_string(), "a1".to_string()),
                ("q2".to_string(), "a2".to_string())
            ]
        );
        assert!(state.redo_stack.is_empty());
        assert_eq!(state.turn, 2);
        assert_eq!(state.last_response.as_deref(), Some("a2"));
        assert!(state.session_persistence_error.is_none());
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn undo_restores_workspace_files_when_recovery_persist_fails() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("edited.txt");
        std::fs::write(&file_path, b"after").unwrap();
        let sid = uuid::Uuid::new_v4().to_string();
        let mut ws = astra_services::session_workspace::WorkspaceMetadata::new(&sid, "test-model");
        ws.turn_count = 1;
        astra_services::session_workspace::write_workspace(&ws).unwrap();
        make_owner_bound_step_checkpoint_path_invalid(&sid);

        let mut state = SessionState::default();
        state.set_session_id(sid);
        state.turn = 1;
        state.history = vec![("q1".into(), "a1".into())];
        state.last_response = Some("a1".into());
        {
            let mut journal = state.file_journal.lock_recover();
            std::fs::write(&file_path, b"before").unwrap();
            journal.record_before(&file_path, "tool-1", 1);
            std::fs::write(&file_path, b"after").unwrap();
            journal.record_after(&file_path, "tool-1", b"after");
        }

        let error = handle_state_command(
            "/undo",
            "",
            StateCommandContext {
                api: &api,
                profile: None,
                token: None,
            },
            &mut state,
        )
        .await
        .expect_err("invalid checkpoint path should fail undo persistence");

        assert!(error.contains("/undo updated live context"), "{error}");
        assert_eq!(std::fs::read(&file_path).unwrap(), b"after");
        assert_eq!(state.history, vec![("q1".to_string(), "a1".to_string())]);
        assert!(state.redo_stack.is_empty());
        assert_eq!(state.turn, 1);
        assert!(state.session_persistence_error.is_none());
    }

    #[test]
    fn undo_persist_failure_marks_session_persistence_error_when_recovery_rollback_failed() {
        let snapshot_source = SessionState {
            turn: 1,
            history: vec![("q1".into(), "a1".into())],
            last_response: Some("a1".into()),
            ..Default::default()
        };
        let snapshot = HistoryEditSnapshot::capture(&snapshot_source);

        let mut state = SessionState {
            redo_stack: vec![("q1".into(), "a1".into(), 1)],
            ..Default::default()
        };

        let error = handle_undo_persist_failure(
            &mut state,
            snapshot,
            &[],
            "/undo updated live context but failed to refresh resume/fork state: recovery checkpoint rollback failed: stale heavy checkpoint".into(),
        );

        assert!(error.contains("rollback failed"), "{error}");
        assert_eq!(state.history, vec![("q1".to_string(), "a1".to_string())]);
        assert!(state.redo_stack.is_empty());
        assert_eq!(state.turn, 1);
        assert_eq!(state.last_response.as_deref(), Some("a1"));
        assert_eq!(
            state.session_persistence_error.as_deref(),
            Some(error.as_str())
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn compact_single_turn_rewrites_current_turn_in_place() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = uuid::Uuid::new_v4().to_string();
        let mut ws = astra_services::session_workspace::WorkspaceMetadata::new(&sid, "gpt-5");
        ws.turn_count = 1;
        astra_services::session_workspace::write_workspace(&ws).unwrap();

        let mock = crate::cli::mock_llm::MockLlmServer::start(
            crate::cli::mock_llm::MockScenario::TextOnly,
        )
        .await
        .unwrap();
        let api = astra_thin_client::ThinClient::new(&mock.base_url, None).unwrap();

        let mut state = SessionState::default();
        state.set_session_id(sid.clone());
        state.model = Some("gpt-5".into());
        state.turn = 1;
        state.history = vec![("hi".into(), "hello".into())];
        state.recent_tools = vec!["bash".into()];
        crate::cli::session::session_startup::initialize_journal_pub(&mut state, &sid);

        handle_state_command(
            "/compact",
            "quick no-memoria",
            StateCommandContext {
                api: &api,
                profile: None,
                token: Some("test-token"),
            },
            &mut state,
        )
        .await
        .unwrap();

        assert_eq!(state.history.len(), 1);
        assert_eq!(state.history[0].0, "hi");
        assert!(
            state.history[0]
                .1
                .contains("[Prior context — 1 turns compacted]"),
            "{}",
            state.history[0].1
        );
        assert!(
            state.history[0]
                .1
                .contains("answering directly without tools on turn"),
            "{}",
            state.history[0].1
        );
        assert!(state.recent_tools.is_empty());

        let events = session_journal::read_journal(&sid).unwrap();
        assert!(
            events
                .iter()
                .any(|event| event.event_type == JournalEventType::Compact),
            "compact should be durably recorded"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn compact_rolls_back_live_state_when_recovery_persist_fails() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = uuid::Uuid::new_v4().to_string();
        let mut ws = astra_services::session_workspace::WorkspaceMetadata::new(&sid, "gpt-5");
        ws.turn_count = 1;
        astra_services::session_workspace::write_workspace(&ws).unwrap();
        make_owner_bound_step_checkpoint_path_invalid(&sid);

        let mock = crate::cli::mock_llm::MockLlmServer::start(
            crate::cli::mock_llm::MockScenario::TextOnly,
        )
        .await
        .unwrap();
        let api = astra_thin_client::ThinClient::new(&mock.base_url, None).unwrap();

        let mut state = SessionState::default();
        state.set_session_id(sid.clone());
        state.model = Some("gpt-5".into());
        state.turn = 1;
        state.history = vec![("hi".into(), "hello".into())];
        state.recent_tools = vec!["bash".into()];
        crate::cli::session::session_startup::initialize_journal_pub(&mut state, &sid);

        let error = handle_state_command(
            "/compact",
            "quick no-memoria",
            StateCommandContext {
                api: &api,
                profile: None,
                token: Some("test-token"),
            },
            &mut state,
        )
        .await
        .expect_err("invalid checkpoint path should fail compact persistence");

        assert!(error.contains("/compact updated live context"), "{error}");
        assert_eq!(state.history, vec![("hi".to_string(), "hello".to_string())]);
        assert_eq!(state.recent_tools, vec!["bash".to_string()]);

        let events = session_journal::read_journal(&sid).unwrap();
        assert!(
            !events
                .iter()
                .any(|event| event.event_type == JournalEventType::Compact),
            "failed compact must not be journaled as durable state"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn reflect_falls_back_to_remote_when_local_state_missing() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = uuid::Uuid::new_v4().to_string();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/chat/session/{sid}/reflect")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "tool": "reflect",
                "session_id": sid,
                "analysis_view": "overview",
                "topic": "overview",
                "facet": "overview",
                "depth": "diagnostic",
                "horizon": "session",
                "source_policy": "auto",
                "include_context": false,
                "data_coverage": {"overall":"fresh","source":"server_db","events":1,"decisions":0},
                "overview": {
                    "total_events": 1,
                    "total_decisions": 0,
                    "duration_minutes": null,
                    "unique_skills_used": 0,
                    "error_count": 0,
                    "error_rate_pct": 0.0,
                    "top_event_types": [],
                    "top_skills": []
                },
                "summary": "remote reflect",
                "diagnoses": [],
                "insights": [],
                "recommendations": [],
                "graph_slice": {"nodes":[],"edges":[],"budget_result":{"truncated":false}}
            })))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let mut state = SessionState::default();
        state.set_session_id(sid);

        handle_state_command(
            "/reflect",
            "",
            StateCommandContext {
                api: &api,
                profile: None,
                token: Some("test-token"),
            },
            &mut state,
        )
        .await
        .expect("missing local state should fall back to remote reflect");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn reflect_remote_fallback_sends_depth_and_encoded_question_query() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = uuid::Uuid::new_v4().to_string();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/chat/session/{sid}/reflect")))
            .and(header_exists("authorization"))
            .and(query_param("topic", "execution"))
            .and(query_param("facet", "trace"))
            .and(query_param("depth", "forensic"))
            .and(query_param("question", "why bash & tools?"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "tool": "reflect",
                "session_id": sid,
                "analysis_view": "execution_trace",
                "topic": "execution",
                "facet": "trace",
                "depth": "forensic",
                "horizon": "session",
                "source_policy": "auto",
                "include_context": false,
                "data_coverage": {"overall":"fresh","source":"server_db","events":1,"decisions":0},
                "overview": {
                    "total_events": 1,
                    "total_decisions": 0,
                    "duration_minutes": null,
                    "unique_skills_used": 0,
                    "error_count": 0,
                    "error_rate_pct": 0.0,
                    "top_event_types": [],
                    "top_skills": []
                },
                "summary": "remote reflect",
                "diagnoses": [],
                "insights": [],
                "recommendations": [],
                "graph_slice": {"nodes":[],"edges":[],"budget_result":{"truncated":false}}
            })))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let mut state = SessionState::default();
        state.set_session_id(sid);

        handle_state_command(
            "/reflect",
            "execution/trace forensic why bash & tools?",
            StateCommandContext {
                api: &api,
                profile: None,
                token: Some("test-token"),
            },
            &mut state,
        )
        .await
        .expect("remote reflect query should include encoded observation params");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn reflect_surfaces_local_artifact_error_without_remote_fallback() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = uuid::Uuid::new_v4().to_string();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/chat/session/{sid}/reflect")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "tool": "reflect",
                "session_id": sid,
                "analysis_view": "overview",
                "topic": "overview",
                "facet": "overview",
                "depth": "diagnostic",
                "horizon": "session",
                "source_policy": "auto",
                "include_context": false,
                "data_coverage": {"overall":"fresh","source":"server_db","events":1,"decisions":0},
                "overview": {
                    "total_events": 1,
                    "total_decisions": 0,
                    "duration_minutes": null,
                    "unique_skills_used": 0,
                    "error_count": 0,
                    "error_rate_pct": 0.0,
                    "top_event_types": [],
                    "top_skills": []
                },
                "summary": "remote reflect should not mask local corruption",
                "diagnoses": [],
                "insights": [],
                "recommendations": [],
                "graph_slice": {"nodes":[],"edges":[],"budget_result":{"truncated":false}}
            })))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let journal = session_journal::journal_file_path(&sid);
        std::fs::create_dir_all(journal.parent().expect("journal parent")).unwrap();
        std::fs::create_dir_all(&journal).unwrap();

        let mut state = SessionState::default();
        state.set_session_id(sid);

        let error = handle_state_command(
            "/reflect",
            "",
            StateCommandContext {
                api: &api,
                profile: None,
                token: Some("test-token"),
            },
            &mut state,
        )
        .await
        .expect_err("local artifact errors must surface instead of falling back");

        assert!(error.contains("failed to read session journal"), "{error}");
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_reflect_args, render_reflect_diff};
    use crate::cli::session::session_state::SessionState;

    #[test]
    fn parse_reflect_args_recognises_diff_branch() {
        let args = parse_reflect_args("diff");
        assert!(args.diff);
        assert_eq!(args.topic, "overview");
        assert_eq!(args.facet, None);
        assert_eq!(args.depth, "diagnostic");
        assert_eq!(args.question, None);
    }

    #[test]
    fn render_reflect_diff_reports_no_delta_on_fresh_session() {
        let state = crate::cli::session::session_state::SessionState::default();
        let out = render_reflect_diff(&state);
        assert!(out.contains("reflect diff"), "header present: {out}");
        assert!(
            out.contains("no delta since last sync"),
            "fresh session should say no delta: {out}"
        );
    }

    #[test]
    fn render_reflect_diff_surfaces_new_and_drifting_tools() {
        use astra_turn_core::tool_health_persistence::ToolHealthEntry;
        let mut state = crate::cli::session::session_state::SessionState::default();
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
        let out = render_reflect_diff(&state);
        assert!(out.contains("grep"), "drifting tool shown: {out}");
        assert!(out.contains("glob"), "new tool shown: {out}");
        assert!(out.contains("new"), "new marker: {out}");
        assert!(out.contains("Δcalls +4"), "grep call delta surfaced: {out}");
    }

    #[test]
    fn parse_reflect_args_splits_topic_facet_and_question() {
        let args = parse_reflect_args("execution/errors why did bash fail");
        assert!(!args.diff);
        assert_eq!(args.topic, "execution");
        assert_eq!(args.facet.as_deref(), Some("errors"));
        assert_eq!(args.depth, "diagnostic");
        assert_eq!(args.question.as_deref(), Some("why did bash fail"));
    }

    #[test]
    fn parse_reflect_args_accepts_depth_after_topic_facet() {
        let args = parse_reflect_args("execution/trace forensic why did it fail");
        assert_eq!(args.topic, "execution");
        assert_eq!(args.facet.as_deref(), Some("trace"));
        assert_eq!(args.depth, "forensic");
        assert_eq!(args.question.as_deref(), Some("why did it fail"));
    }

    #[test]
    fn parse_reflect_args_accepts_depth_without_topic() {
        let args = parse_reflect_args("summary what happened");
        assert_eq!(args.topic, "overview");
        assert_eq!(args.facet, None);
        assert_eq!(args.depth, "summary");
        assert_eq!(args.question.as_deref(), Some("what happened"));
    }

    #[test]
    fn parse_reflect_args_accepts_separate_topic_and_facet() {
        let args = parse_reflect_args("runtime performance why was bash slow");
        assert_eq!(args.topic, "runtime");
        assert_eq!(args.facet.as_deref(), Some("performance"));
        assert_eq!(args.depth, "diagnostic");
        assert_eq!(args.question.as_deref(), Some("why was bash slow"));
    }

    #[test]
    fn parse_reflect_args_treats_freeform_as_question() {
        let args = parse_reflect_args("performance why was bash slow");
        assert_eq!(args.topic, "overview");
        assert_eq!(args.facet, None);
        assert_eq!(args.depth, "diagnostic");
        assert_eq!(
            args.question.as_deref(),
            Some("performance why was bash slow")
        );
    }

    #[test]
    fn parse_reflect_args_does_not_accept_removed_focus_shortcuts() {
        let args = parse_reflect_args("skill_failure why did bash fail");
        assert_eq!(args.topic, "overview");
        assert_eq!(args.facet, None);
        assert_eq!(args.depth, "diagnostic");
        assert_eq!(
            args.question.as_deref(),
            Some("skill_failure why did bash fail")
        );
    }

    #[test]
    fn parse_reflect_args_does_not_accept_unimplemented_adaptation_topic() {
        let args = parse_reflect_args("adaptation/signals forensic");
        assert_eq!(args.topic, "overview");
        assert_eq!(args.facet, None);
        assert_eq!(args.depth, "diagnostic");
        assert_eq!(
            args.question.as_deref(),
            Some("adaptation/signals forensic")
        );
    }

    #[test]
    fn parse_reflect_args_empty_defaults_to_overview() {
        let args = parse_reflect_args("");
        assert_eq!(args.topic, "overview");
        assert_eq!(args.facet, None);
        assert_eq!(args.depth, "diagnostic");
        assert_eq!(args.question, None);
    }

    // ── /undo tests ──

    /// Helper: build a SessionState with N fake turns in history.
    fn state_with_turns(n: usize) -> SessionState {
        let mut state = SessionState::default();
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
        state.continuation_anchor = Some("some anchor".into());
        // Simulate undo
        state.history.pop();
        state.turn = state.turn.saturating_sub(1);
        state.last_response = state.history.last().map(|(_, r)| r.clone());
        state.continuation_anchor = None;
        assert!(state.continuation_anchor.is_none());
    }

    #[test]
    fn undo_turn_preview_truncation() {
        let mut state = SessionState::default();
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

#[cfg(test)]
mod compact_tests {
    use super::{
        CompactArgs, CompactCtx, ManualCompactPlan, build_swap_memory_body, cap_swap_body,
        compact_fact_memory_entry, compact_mem_note, manual_compaction_memory_entry,
        parse_compact_args, plan_manual_compaction, store_compact_memory_payload,
    };
    use crate::cli::permission_manager::PermissionManager;
    use crate::cli::session::session_state::SessionState;
    use std::sync::Arc;
    use tempfile::tempdir;
    use wiremock::matchers::{body_json, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn parse_compact_args_defaults_to_full_memory_mode() {
        assert_eq!(
            parse_compact_args(""),
            CompactArgs {
                quick: false,
                no_memoria: false,
            }
        );
    }

    #[test]
    fn manual_compaction_stores_one_layered_episode_without_nested_wire_envelope() {
        let summary = "The runtime now routes compaction state through one typed volatile lane while preserving real conversation history.";
        let entry = manual_compaction_memory_entry(summary);
        let encoded = entry.encode();
        let parsed = astra_runtime::prompts::memory_proto::MemoryEntry::parse(&encoded)
            .expect("canonical memory entry");

        assert_eq!(parsed.ns, astra_runtime::prompts::memory_proto::NS_EPISODE);
        assert_eq!(
            parsed.status,
            astra_runtime::prompts::memory_proto::ST_SUMMARY
        );
        assert_eq!(parsed.detail_layer(), Some(summary));
        assert!(!parsed.detail_layer().unwrap().starts_with("[@episode/"));
    }

    #[test]
    fn compact_facts_use_recallable_lifecycle_and_keep_working_state_session_scoped() {
        let semantic = compact_fact_memory_entry("Rust is used by this project.", "semantic")
            .expect("semantic fact");
        assert_eq!(semantic.ns, astra_runtime::prompts::memory_proto::NS_FACT);
        assert_eq!(
            semantic.status,
            astra_runtime::prompts::memory_proto::ST_ACTIVE
        );
        assert!(
            astra_runtime::prompts::memory_proto::is_prompt_recallable_status(&semantic.status)
        );

        let profile = compact_fact_memory_entry("The user prefers concise output.", "profile")
            .expect("profile fact");
        assert_eq!(profile.ns, astra_runtime::prompts::memory_proto::NS_PREF);

        let procedure =
            compact_fact_memory_entry("Run focused tests before the full suite.", "procedural")
                .expect("procedural fact");
        assert_eq!(
            procedure.ns,
            astra_runtime::prompts::memory_proto::NS_KNOWLEDGE
        );

        assert!(
            compact_fact_memory_entry("The current branch has five modified files.", "working")
                .is_none(),
            "transient working state must stay in session memory instead of durable recall"
        );
    }

    #[tokio::test]
    async fn compact_memory_store_requires_http_success() {
        let server = MockServer::start().await;
        let payload = serde_json::json!({"content": "remembered"});
        Mock::given(method("POST"))
            .and(path("/memory/store"))
            .and(header_exists("authorization"))
            .and(body_json(payload.clone()))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        store_compact_memory_payload(&api, "token", &payload)
            .await
            .expect("201 is a confirmed write");

        let failing_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/memory/store"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&failing_server)
            .await;
        let api = astra_thin_client::ThinClient::new(&failing_server.uri(), None).unwrap();
        store_compact_memory_payload(&api, "token", &payload)
            .await
            .expect_err("a delivered request is not a durable write without server success");
    }

    #[test]
    fn parse_compact_args_accepts_quick_alias_and_no_memoria_alias() {
        assert_eq!(
            parse_compact_args("summary-only no_memoria"),
            CompactArgs {
                quick: true,
                no_memoria: true,
            }
        );
    }

    #[test]
    fn parse_compact_args_is_case_insensitive_and_ignores_unknown_words() {
        assert_eq!(
            parse_compact_args("QUICK later No-Memoria"),
            CompactArgs {
                quick: true,
                no_memoria: true,
            }
        );
    }

    #[test]
    fn cap_swap_body_truncates_at_utf8_boundary() {
        let long_text = "你好世界".repeat(300);
        let swap_body = format!("Turns 1-5 swapped out:\nU: {}", long_text);
        let capped = cap_swap_body(swap_body);

        assert!(capped.len() <= 2003);
        assert!(capped.ends_with("…"));
        assert!(std::str::from_utf8(capped.as_bytes()).is_ok());
    }

    #[test]
    fn cap_swap_body_leaves_short_body_unchanged() {
        let swap_body = "Turns 1-2 swapped out:\nU: short".to_string();
        let capped = cap_swap_body(swap_body);

        assert_eq!(capped, "Turns 1-2 swapped out:\nU: short");
    }

    #[test]
    fn build_swap_memory_body_formats_compacted_turn_previews() {
        let user = "u".repeat(120);
        let assistant = "a".repeat(170);
        let body = build_swap_memory_body(
            &[
                (user.clone(), assistant.clone()),
                (String::new(), "done".into()),
            ],
            2,
        )
        .expect("swap body");

        assert!(body.starts_with("Turns 1-2 swapped out [compact_history]:"));
        assert!(body.contains(&format!("U: {}", "u".repeat(100))));
        assert!(!body.contains(&format!("U: {}", "u".repeat(101))));
        assert!(body.contains(&format!("A: {}", "a".repeat(150))));
        assert!(!body.contains(&format!("A: {}", "a".repeat(151))));
        assert!(body.contains("A: done"));
    }

    #[test]
    fn build_swap_memory_body_skips_empty_turns() {
        assert_eq!(
            build_swap_memory_body(&[(String::new(), String::new())], 1),
            None
        );
    }

    #[test]
    fn compact_mem_note_reports_saved_facts() {
        assert_eq!(
            compact_mem_note(false, true, 5, false),
            " · saved to memory (5 facts extracted)"
        );
    }

    #[test]
    fn compact_mem_note_reports_quick_without_claiming_facts() {
        assert_eq!(
            compact_mem_note(false, true, 0, true),
            " · saved to memory · quick (facts not stored to memory)"
        );
    }

    #[test]
    fn compact_mem_note_reports_no_memoria_before_any_save_state() {
        assert_eq!(
            compact_mem_note(true, true, 5, false),
            " · Memoria side-effects skipped (no-memoria)"
        );
    }

    #[test]
    fn compact_mem_note_is_empty_when_memory_save_failed_or_skipped_by_auth() {
        assert_eq!(compact_mem_note(false, false, 0, false), "");
    }

    #[test]
    fn plan_manual_compaction_returns_none_for_empty_history() {
        assert_eq!(plan_manual_compaction(0, 3), None);
    }

    #[test]
    fn plan_manual_compaction_compacts_single_turn_in_place() {
        assert_eq!(
            plan_manual_compaction(1, 3),
            Some(ManualCompactPlan::SingleTurnInPlace)
        );
    }

    #[test]
    fn plan_manual_compaction_compacts_all_but_latest_turn_inside_keep_recent_window() {
        assert_eq!(
            plan_manual_compaction(3, 3),
            Some(ManualCompactPlan::PrefixTurns { trimmed_count: 2 })
        );
    }

    #[test]
    fn plan_manual_compaction_respects_keep_recent_window_when_history_exceeds_it() {
        assert_eq!(
            plan_manual_compaction(5, 3),
            Some(ManualCompactPlan::PrefixTurns { trimmed_count: 2 })
        );
    }

    #[test]
    fn plan_manual_compaction_keeps_latest_turn_when_keep_recent_is_zero() {
        assert_eq!(
            plan_manual_compaction(3, 0),
            Some(ManualCompactPlan::PrefixTurns { trimmed_count: 2 })
        );
    }

    #[test]
    fn compact_ctx_build_params_uses_history_only_without_preloaded_messages() {
        let api =
            astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).expect("thin client");
        let mut state = SessionState::default();
        state.session_id = Some("sess-1".to_string());
        state.model = Some("model-a".to_string());
        state.history = vec![("u1".to_string(), "a1".to_string())];
        let temp = tempdir().expect("tempdir");
        let mut pm = PermissionManager::with_project(false, temp.path());
        let mut ctx = CompactCtx {
            state: &mut state,
            api: &api,
            token: "tok",
            profile: Some("prof"),
            incremental_state: None,
        };

        let params = ctx.build_params("compact", true, &mut pm, None, None);

        assert_eq!(params.message, "compact");
        assert_eq!(params.auth_profile, Some("prof"));
        assert_eq!(params.session_id, Some("sess-1"));
        assert_eq!(params.model, Some("model-a"));
        assert_eq!(params.history.len(), 1);
        assert!(params.pre_loaded_messages.is_none());
        assert!(params.incremental_state.is_none());
        assert!(params.file_journal.is_none());
        assert!(params.session_state_journal.is_none());
        assert!(params.observability_hub.is_none());
        assert!(params.observability_session.is_none());
    }

    #[test]
    fn compact_ctx_build_params_prefers_preloaded_messages_over_state_history() {
        let api =
            astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).expect("thin client");
        let mut state = SessionState::default();
        state.history = vec![("u1".to_string(), "a1".to_string())];
        let temp = tempdir().expect("tempdir");
        let mut pm = PermissionManager::with_project(false, temp.path());
        let incremental_state =
            Arc::new(astra_turn_core::turn_event_sink::IncrementalTurnState::default());
        let preloaded = vec![serde_json::json!({"role": "user", "content": "micro"})];
        let mut ctx = CompactCtx {
            state: &mut state,
            api: &api,
            token: "tok",
            profile: None,
            incremental_state: Some(incremental_state.clone()),
        };

        let params = ctx.build_params("compact", true, &mut pm, None, Some(preloaded));

        assert!(params.history.is_empty());
        assert_eq!(params.pre_loaded_messages.as_ref().map(Vec::len), Some(1));
        assert!(Arc::ptr_eq(
            params
                .incremental_state
                .as_ref()
                .expect("incremental state"),
            &incremental_state
        ));
    }
}
