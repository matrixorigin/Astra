use super::*;

pub(crate) fn delegation_from_heavy_checkpoint(
    heavy: &astra_pipeline::step_protocol::HeavyCheckpoint,
    context: &str,
) -> Option<astra_turn_core::conversation_log::DelegationCompact> {
    match (&heavy.delegation_id, &heavy.delegation_pattern) {
        (Some(id), Some(pattern)) => Some(astra_turn_core::conversation_log::DelegationCompact {
            id: id.clone(),
            pattern: pattern.clone(),
            completed_sub_runs: heavy.delegation_sub_run_summaries.clone(),
        }),
        (None, None) => None,
        _ => {
            astra_core::agent_warn!(
                "checkpoint",
                "{context}: partial delegation fields in heavy checkpoint; dropping delegation restore"
            );
            None
        }
    }
}

pub(crate) fn session_state_compact_from_heavy_checkpoint(
    heavy: &astra_pipeline::step_protocol::HeavyCheckpoint,
) -> astra_turn_core::conversation_log::SessionStateCompact {
    astra_turn_core::conversation_log::SessionStateCompact {
        blocked_tools: heavy.blocked_tools.clone(),
        recent_tools: heavy.recent_tools.clone(),
        approval_overrides: heavy.approval_overrides.clone(),
        compaction_tracker: heavy.compaction_state.clone(),
        budget_remaining_tokens: heavy.budget_remaining_tokens,
        budget_remaining_rounds: heavy.budget_remaining_rounds,
        consecutive_ctx_errors: heavy.consecutive_context_window_errors,
        delegation: delegation_from_heavy_checkpoint(
            heavy,
            "session_state_compact_from_heavy_checkpoint",
        ),
        interruption: heavy.interruption.clone(),
    }
}

pub(crate) async fn ensure_loaded_csl_state(
    state: &mut SessionState,
    sid: &str,
) -> Result<Option<astra_turn_core::conversation_log::SessionStateCompact>, String> {
    let needs_new_manager = state
        .csl_manager
        .as_ref()
        .is_none_or(|mgr| mgr.session_id() != sid);
    if needs_new_manager {
        let store = std::sync::Arc::new(
            astra_turn_core::conversation_log::file_store::FileCslStore::new(
                session_journal::local_sessions_dir(),
            ),
        );
        state.csl_manager = match astra_turn_core::conversation_log::manager::CslManager::new(
            store,
            sid.to_string(),
            Default::default(),
        ) {
            Ok(mgr) => Some(mgr),
            Err(e) => {
                return Err(format!("initialize CSL state for recovery sync: {e}"));
            }
        };
    }

    let Some(mgr) = state.csl_manager.as_mut() else {
        return Ok(None);
    };

    if mgr.last_seq() > 0 {
        return Ok(Some(mgr.last_session_state().clone()));
    }

    match mgr.load().await {
        Ok(Some(mat)) => Ok(Some(mat.session_state)),
        Ok(None) => Ok(None),
        Err(e) => Err(format!("load CSL state for recovery sync: {e}")),
    }
}

fn previous_session_state_for_history_sync(
    previous_heavy: Option<&astra_pipeline::step_protocol::HeavyCheckpoint>,
    csl_state: Option<astra_turn_core::conversation_log::SessionStateCompact>,
) -> astra_turn_core::conversation_log::SessionStateCompact {
    csl_state
        .or_else(|| previous_heavy.map(session_state_compact_from_heavy_checkpoint))
        .unwrap_or_default()
}

pub(crate) async fn load_previous_recovery_state(
    state: &mut SessionState,
    sid: &str,
) -> Result<
    (
        Option<astra_pipeline::step_protocol::HeavyCheckpoint>,
        astra_turn_core::conversation_log::SessionStateCompact,
    ),
    String,
> {
    let previous_heavy = astra_pipeline::step_checkpoint::read_latest_heavy_checkpoint(sid)
        .map_err(|e| format!("read latest heavy checkpoint: {e}"))?;
    let csl_state = ensure_loaded_csl_state(state, sid).await?;
    let prev_state = previous_session_state_for_history_sync(previous_heavy.as_ref(), csl_state);
    Ok((previous_heavy, prev_state))
}

/// Copy plan / durable-task fields from REPL into workspace before checkpointing.
pub(crate) fn sync_plan_fields_to_workspace(
    state: &SessionState,
    ws: &mut astra_services::session_workspace::WorkspaceMetadata,
) {
    ws.executing_plan_json = state
        .executing_plan
        .as_ref()
        .and_then(|p| serde_json::to_string(p).ok());
    ws.plan_goal = state.executing_plan_goal.clone();
    ws.plan_config_json = state
        .plan_execution_config
        .as_ref()
        .and_then(|c| serde_json::to_string(c).ok());
    ws.plan_execution_rounds = state.plan_execution_rounds;
    ws.contract_json = state
        .durable_task_state
        .as_ref()
        .and_then(|d| serde_json::to_string(&d.contract).ok());
    ws.plan_corrections = state.plan_execution_corrections.clone();
}

/// Sync session state fields to workspace for resume capability.
pub(crate) fn sync_session_state_to_workspace(
    state: &SessionState,
    ws: &mut astra_services::session_workspace::WorkspaceMetadata,
) {
    ws.last_persistence_error = state.session_persistence_error.clone();
    ws.pinned_skills = state.pinned_skills.iter().cloned().collect();
    ws.discovered_skills = state.discovered_skills.iter().cloned().collect();

    if let Some(obs) = &state.observability_session {
        if let Ok(guard) = obs.read() {
            ws.last_scenario_change_turn = guard.last_scenario_change_turn;
            ws.last_token_budget_direction = guard.last_token_budget_direction;
            ws.last_token_budget_change_turn = guard.last_token_budget_change_turn;
            ws.tuned_config_json = serde_json::to_string(&guard.config).ok();
        }
    }
}

fn context_trace_signal_from_trace(
    trace: &astra_turn_core::context_assembly_trace::ContextAssemblyTrace,
) -> astra_services::session_workspace::ContextTraceSignal {
    let tool_selection = (!trace.tools.selection_strategy.is_empty()
        || !trace.tools.tools_selected.is_empty()
        || trace.tools.tools_available > 0)
        .then(
            || astra_services::session_workspace::ContextTraceToolSelection {
                tools_available: trace.tools.tools_available,
                selected_tools: trace
                    .tools
                    .tools_selected
                    .iter()
                    .map(|tool| tool.tool_name.clone())
                    .collect(),
                selection_scope: "latest_round".to_string(),
                rejected_tools: trace.tools.tools_rejected.len(),
                strategy: trace.tools.selection_strategy.clone(),
                confidence: trace.tools.selection_confidence,
                latency_ms: trace.tools.selection_latency_ms,
            },
        );
    let memory = (!trace.memory.query.trim().is_empty()
        || !trace.memory.memories_selected.is_empty()
        || trace.memory.candidates_considered > 0)
        .then(
            || astra_services::session_workspace::ContextTraceMemorySignal {
                query: trace.memory.query.trim().chars().take(160).collect(),
                candidates_considered: trace.memory.candidates_considered,
                selected_memory_ids: trace
                    .memory
                    .memories_selected
                    .iter()
                    .map(|memory| memory.memory_id.clone())
                    .collect(),
                total_tokens: trace.memory.total_tokens,
                latency_ms: trace.memory.retrieval_latency_ms,
            },
        );
    let history = (trace.history.total_turns_available > 0
        || !trace.history.turns_retained.is_empty()
        || !trace.history.turns_compressed.is_empty()
        || !trace.history.turns_dropped.is_empty())
    .then_some(
        astra_services::session_workspace::ContextTraceHistorySignal {
            total_turns_available: trace.history.total_turns_available,
            retained_turns: trace.history.turns_retained.len(),
            compressed_turns: trace.history.turns_compressed.len(),
            dropped_turns: trace.history.turns_dropped.len(),
            compression_ratio: trace.history.compression_ratio,
            tokens_before: trace.history.tokens_before,
            tokens_after: trace.history.tokens_after,
        },
    );
    let budget = (trace.token_budget.max_tokens > 0 || trace.token_budget.total_used > 0)
        .then_some(
            astra_services::session_workspace::ContextTraceBudgetSignal {
                max_tokens: trace.token_budget.max_tokens,
                total_used: trace.token_budget.total_used,
                budget_pressure: trace.token_budget.budget_pressure,
                compression_triggered: trace.token_budget.compression_triggered,
            },
        );

    astra_services::session_workspace::ContextTraceSignal {
        turn_id: trace.turn_id.clone(),
        captured_at: Some(chrono::DateTime::<chrono::Utc>::from(trace.timestamp).to_rfc3339()),
        tool_selection,
        memory,
        history,
        budget,
        timing: None,
        explanations: trace
            .explanations
            .iter()
            .filter_map(|explanation| {
                let trimmed = explanation.reasoning.trim();
                (!trimmed.is_empty()).then(|| trimmed.chars().take(200).collect::<String>())
            })
            .collect(),
    }
}

fn latest_context_trace_signal(
    state: &SessionState,
) -> Option<astra_services::session_workspace::ContextTraceSignal> {
    if let Some(trace) = state.latest_context_assembly_trace.as_ref() {
        return Some(context_trace_signal_from_trace(trace));
    }
    let obs = state.observability_session.as_ref()?;
    let guard = obs.read().ok()?;
    astra_runtime::observability::latest_context_trace_signal(&guard)
}

pub(crate) fn sync_context_trace_to_workspace(
    state: &SessionState,
    ws: &mut astra_services::session_workspace::WorkspaceMetadata,
) {
    ws.last_context_trace = latest_context_trace_signal(state);
}

/// Next numeric id for `step_checkpoints/<NNNNNN>-*.json`.
pub(crate) fn next_step_checkpoint_number(sid: &str) -> Result<u32, String> {
    let existing = astra_pipeline::step_checkpoint::list_checkpoints(sid)
        .map_err(|e| format!("list step checkpoints: {e}"))?;
    Ok(existing
        .iter()
        .map(|(n, _)| *n)
        .max()
        .unwrap_or(0)
        .saturating_add(1))
}

/// Build heavy step checkpoint from current REPL history (OpenAI-style messages).
pub(crate) fn build_manual_heavy_step_checkpoint(
    state: &SessionState,
    sid: &str,
    session_state: &astra_turn_core::conversation_log::SessionStateCompact,
    previous_heavy: Option<&astra_pipeline::step_protocol::HeavyCheckpoint>,
) -> astra_pipeline::step_protocol::StepCheckpoint {
    use astra_pipeline::step_protocol::{
        ExecutionCursor, HeavyCheckpoint, LightCheckpoint, PROTOCOL_VERSION, StepCheckpoint,
        epoch_ms,
    };

    let mut messages = Vec::new();
    for (u, a) in &state.history {
        messages.push(serde_json::json!({ "role": "user", "content": u }));
        messages.push(serde_json::json!({ "role": "assistant", "content": a }));
    }

    let max_turns = state.cli_context.max_turns.unwrap_or(50u32);
    let now_ms = epoch_ms();
    let total_tok = state.total_prompt_tokens + state.total_completion_tokens;

    let light = LightCheckpoint {
        protocol_version: PROTOCOL_VERSION,
        cursor: ExecutionCursor::default(),
        step_id: "repl-manual".to_string(),
        task_id: state.run_id.clone().unwrap_or_else(|| "repl".to_string()),
        agent_id: sid.to_string(),
        progress: 1.0,
        total_tokens: total_tok,
        created_at: now_ms,
    };
    let heavy = HeavyCheckpoint {
        light,
        messages,
        budget_remaining_tokens: {
            if previous_heavy.is_some() || session_state.budget_remaining_tokens > 0 {
                session_state.budget_remaining_tokens
            } else {
                let limit = astra_core::RuntimeLimits::global().max_turn_input_tokens;
                if limit == 0 {
                    0
                } else {
                    limit.saturating_sub(state.total_prompt_tokens)
                }
            }
        },
        budget_remaining_rounds: if previous_heavy.is_some()
            || session_state.budget_remaining_rounds > 0
        {
            session_state.budget_remaining_rounds
        } else {
            max_turns.saturating_sub(state.turn)
        },
        blocked_tools: session_state.blocked_tools.clone(),
        recent_tools: state.recent_tools.clone(),
        memory_context: previous_heavy.and_then(|heavy| heavy.memory_context.clone()),
        delegation_id: session_state
            .delegation
            .as_ref()
            .map(|delegation| delegation.id.clone()),
        delegation_pattern: session_state
            .delegation
            .as_ref()
            .map(|delegation| delegation.pattern.clone()),
        delegation_sub_run_summaries: session_state
            .delegation
            .as_ref()
            .map(|delegation| delegation.completed_sub_runs.clone())
            .unwrap_or_default(),
        interruption: session_state.interruption.clone(),
        approval_overrides: session_state.approval_overrides.clone(),
        consecutive_context_window_errors: session_state.consecutive_ctx_errors,
        compaction_state: session_state.compaction_tracker.clone(),
        pipeline_state: previous_heavy.and_then(|heavy| heavy.pipeline_state.clone()),
        config_version_id: state.config_version_id.clone(),
    };
    StepCheckpoint::Heavy(Box::new(heavy))
}

/// Persist heavy JSON + composite snapshot index before any workspace/journal mutation.
pub(crate) fn persist_manual_heavy_and_composite(
    sid: &str,
    turn: u32,
    title: &str,
    next_step: u32,
    step_cp: &astra_pipeline::step_protocol::StepCheckpoint,
) -> Result<std::path::PathBuf, String> {
    use astra_pipeline::step_checkpoint::{
        read_composite_snapshot_index, write_composite_snapshot_index, write_step_checkpoint,
    };

    let heavy_path = write_step_checkpoint(sid, next_step, step_cp)
        .map_err(|e| format!("write heavy step checkpoint: {e}"))?;
    let composite_index_path = composite_index_path_for(sid);
    let composite_index_backup = read_optional_file_bytes(&composite_index_path)
        .map_err(|e| format!("backup composite snapshot index: {e}"))?;

    let mut snapshot =
        astra_core::composite_snapshot::CompositeSnapshotBuilder::new(sid.to_string(), turn)
            .label(format!("manual:{title}"))
            .session_state(format!("{next_step:06}-heavy.json"))
            .workspace_state(sid.to_string())
            .build();
    let mut index = read_composite_snapshot_index(sid)
        .map_err(|e| format!("read composite snapshot index: {e}"))?;
    index
        .append(&mut snapshot)
        .map_err(|e| format!("append snapshot version: {e}"))?;
    if let Err(e) = write_composite_snapshot_index(sid, &index) {
        let mut error_message = format!("write composite snapshot index: {e}");
        append_rollback_error(
            &mut error_message,
            "composite snapshot index",
            restore_optional_file_bytes(&composite_index_path, composite_index_backup),
        );
        append_rollback_error(
            &mut error_message,
            "heavy step checkpoint",
            match std::fs::remove_file(&heavy_path) {
                Ok(()) => sync_parent_dir(&heavy_path),
                Err(remove_error) if remove_error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(remove_error) => Err(format!(
                    "remove heavy file {}: {remove_error}",
                    heavy_path.display()
                )),
            },
        );
        return Err(error_message);
    }

    Ok(heavy_path)
}

fn fresh_workspace_metadata(
    state: &SessionState,
    sid: &str,
) -> astra_services::session_workspace::WorkspaceMetadata {
    astra_services::session_workspace::WorkspaceMetadata::new(
        sid,
        state.model.as_deref().unwrap_or("default"),
    )
}

pub(crate) fn session_workspace_git_root(session_id: Option<&str>) -> Option<String> {
    let session_id = session_id?;
    match astra_services::session_workspace::read_workspace_optional(session_id) {
        Ok(Some(workspace)) => workspace.git_root,
        Ok(None) => None,
        Err(error) => {
            astra_core::agent_warn!(
                "workspace",
                "failed to read workspace for git snapshot: {error}"
            );
            None
        }
    }
}

pub(crate) fn workspace_metadata_from_live_state_after_read_failure(
    state: &SessionState,
    sid: &str,
    error: &std::io::Error,
) -> astra_services::session_workspace::WorkspaceMetadata {
    match astra_services::session_workspace::backup_invalid_workspace_file(sid) {
        Ok(Some(backup_path)) => {
            astra_core::agent_warn!(
                "workspace",
                "rebuilding workspace metadata from live state after read failure: {error}; preserved previous workspace at {}",
                backup_path.display()
            );
        }
        Ok(None) => {
            astra_core::agent_warn!(
                "workspace",
                "rebuilding workspace metadata from live state after read failure: {error}"
            );
        }
        Err(backup_error) => {
            astra_core::agent_warn!(
                "workspace",
                "rebuilding workspace metadata from live state after read failure: {error}; failed to preserve prior workspace: {backup_error}"
            );
        }
    }
    fresh_workspace_metadata(state, sid)
}

pub(crate) fn workspace_metadata_from_live_state(
    state: &SessionState,
    sid: &str,
) -> astra_services::session_workspace::WorkspaceMetadata {
    let mut ws = match astra_services::session_workspace::read_workspace_optional(sid) {
        Ok(Some(ws)) => ws,
        Ok(None) => fresh_workspace_metadata(state, sid),
        Err(error) => workspace_metadata_from_live_state_after_read_failure(state, sid, &error),
    };
    let journal_state = match super::session_runtime::session_state_from_journal(sid) {
        Ok(state) => state,
        Err(error) => {
            astra_core::agent_warn!(
                "workspace",
                "rebuilding workspace metadata from live state without journal state: {error}"
            );
            super::session_runtime::RestoredSessionState::default()
        }
    };
    ws.turn_count = state.turn.max(ws.turn_count).max(journal_state.turn);
    ws.total_tokens_in = state
        .total_prompt_tokens
        .max(ws.total_tokens_in)
        .max(journal_state.total_prompt_tokens);
    ws.total_tokens_out = state
        .total_completion_tokens
        .max(ws.total_tokens_out)
        .max(journal_state.total_completion_tokens);
    ws.total_cache_read_tokens = state
        .total_cache_read_tokens
        .max(ws.total_cache_read_tokens)
        .max(journal_state.total_cache_read_tokens);
    ws.total_cache_creation_tokens = state
        .total_cache_creation_tokens
        .max(ws.total_cache_creation_tokens)
        .max(journal_state.total_cache_creation_tokens);
    ws.status = "active".to_string();
    ws.updated_at = chrono::Utc::now().to_rfc3339();
    match astra_services::session_checkpoint::read_checkpoint_turns(sid) {
        Ok(turns) => {
            if !turns.is_empty() || ws.checkpoints.is_empty() {
                ws.checkpoints = turns;
            }
        }
        Err(error) => {
            astra_core::agent_warn!(
                "workspace",
                "failed to read checkpoint index while rebuilding workspace: {error}"
            );
        }
    }
    sync_plan_fields_to_workspace(state, &mut ws);
    sync_context_trace_to_workspace(state, &mut ws);
    sync_session_state_to_workspace(state, &mut ws);
    ws
}

pub(crate) async fn rebuild_csl_from_history(
    state: &mut SessionState,
    sid: &str,
    messages: &[serde_json::Value],
    session_state: &astra_turn_core::conversation_log::SessionStateCompact,
) -> Result<(), String> {
    if state.csl_manager.is_none() {
        return Ok(());
    }

    let csl_path = csl_log_path_for(sid);
    let csl_backup = read_optional_file_bytes(&csl_path)?;
    if let Err(error) = write_full_csl_snapshot_atomic(sid, state.turn, messages, session_state) {
        let mut error_message = error;
        append_rollback_error(
            &mut error_message,
            "CSL snapshot",
            restore_optional_file_bytes(&csl_path, csl_backup),
        );
        return Err(error_message);
    }

    let store = std::sync::Arc::new(
        astra_turn_core::conversation_log::file_store::FileCslStore::new(
            session_journal::local_sessions_dir(),
        ),
    );
    let mut mgr = astra_turn_core::conversation_log::manager::CslManager::new(
        store,
        sid.to_string(),
        Default::default(),
    )
    .map_err(|e| {
        let mut error_message = format!("reinitialize CSL manager: {e}");
        append_rollback_error(
            &mut error_message,
            "CSL snapshot",
            restore_optional_file_bytes(&csl_path, csl_backup.clone()),
        );
        error_message
    })?;
    if !messages.is_empty() || state.turn > 0 {
        mgr.load().await.map_err(|e| {
            let mut error_message = format!("reload rewritten CSL state: {e}");
            append_rollback_error(
                &mut error_message,
                "CSL snapshot",
                restore_optional_file_bytes(&csl_path, csl_backup.clone()),
            );
            error_message
        })?;
    }
    state.csl_manager = Some(mgr);
    Ok(())
}

pub(crate) fn csl_log_path_for(session_id: &str) -> std::path::PathBuf {
    session_journal::local_sessions_dir()
        .join(session_id)
        .join("conversation_log.jsonl")
}

fn workspace_path_for(session_id: &str) -> std::path::PathBuf {
    astra_services::session_workspace::workspace_dir_for(session_id).join("workspace.yaml")
}

pub(crate) fn composite_index_path_for(session_id: &str) -> std::path::PathBuf {
    session_journal::local_sessions_dir()
        .join(session_id)
        .join("step_checkpoints")
        .join("composite_snapshots.json")
}

fn workspace_lock_path_for(session_id: &str) -> std::path::PathBuf {
    astra_services::session_workspace::workspace_dir_for(session_id).join(".workspace.lock")
}

pub(crate) struct RecoveryCheckpointRollback {
    step_number: u32,
    composite_index_backup: Option<Vec<u8>>,
}

pub(crate) fn with_workspace_lock<T>(
    session_id: &str,
    op: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    use fs2::FileExt;

    let lock_path = workspace_lock_path_for(session_id);
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create workspace lock directory: {e}"))?;
    }
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| format!("open workspace lock {}: {e}", lock_path.display()))?;
    lock_file
        .lock_exclusive()
        .map_err(|e| format!("lock workspace {}: {e}", lock_path.display()))?;

    let result = op();
    let unlock_result = lock_file
        .unlock()
        .map_err(|e| format!("unlock workspace {}: {e}", lock_path.display()));

    match (result, unlock_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(value), Err(error)) => {
            astra_core::agent_warn!("workspace", "{error}");
            Ok(value)
        }
        (Err(error), Err(unlock_error)) => Err(format!("{error}; {unlock_error}")),
    }
}

pub(crate) fn append_rollback_error(
    message: &mut String,
    label: &str,
    rollback_result: Result<(), String>,
) {
    if let Err(error) = rollback_result {
        message.push_str(&format!("; {label} rollback failed: {error}"));
    }
}

pub(crate) fn sync_parent_dir(path: &std::path::Path) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let dir = std::fs::File::open(parent)
        .map_err(|e| format!("open parent directory {}: {e}", parent.display()))?;
    dir.sync_all()
        .map_err(|e| format!("sync parent directory {}: {e}", parent.display()))
}

pub(crate) fn write_bytes_atomic(
    path: &std::path::Path,
    bytes: &[u8],
    label: &str,
) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{label}: path {} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("{label}: create parent directory {}: {e}", parent.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("{label}: invalid file name {}", path.display()))?;
    let tmp_path = parent.join(format!(".tmp-{file_name}"));
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp_path)
            .map_err(|e| format!("{label}: create temporary file {}: {e}", tmp_path.display()))?;
        file.write_all(bytes)
            .map_err(|e| format!("{label}: write temporary file {}: {e}", tmp_path.display()))?;
        file.sync_all()
            .map_err(|e| format!("{label}: sync temporary file {}: {e}", tmp_path.display()))?;
    }
    std::fs::rename(&tmp_path, path)
        .map_err(|e| format!("{label}: replace {}: {e}", path.display()))?;
    sync_parent_dir(path)
}

pub(crate) fn read_optional_file_bytes(path: &std::path::Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read {}: {error}", path.display())),
    }
}

pub(crate) fn restore_optional_file_bytes(
    path: &std::path::Path,
    bytes: Option<Vec<u8>>,
) -> Result<(), String> {
    match bytes {
        Some(bytes) => write_bytes_atomic(path, &bytes, "restore file bytes"),
        None => match std::fs::remove_file(path) {
            Ok(()) => sync_parent_dir(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("remove restored {}: {error}", path.display())),
        },
    }
}

pub(crate) fn write_full_csl_snapshot_atomic(
    sid: &str,
    turn: u32,
    messages: &[serde_json::Value],
    session_state: &astra_turn_core::conversation_log::SessionStateCompact,
) -> Result<(), String> {
    let path = csl_log_path_for(sid);
    if messages.is_empty() && turn == 0 {
        match std::fs::remove_file(&path) {
            Ok(()) => return sync_parent_dir(&path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("remove stale CSL snapshot: {error}")),
        }
    }

    let snapshot = astra_turn_core::conversation_log::CslEntry::Snapshot {
        seq: 1,
        turn,
        messages: messages.to_vec(),
        session_state: session_state.clone(),
    };
    let mut encoded =
        serde_json::to_string(&snapshot).map_err(|e| format!("serialize CSL snapshot: {e}"))?;
    encoded.push('\n');
    write_bytes_atomic(&path, encoded.as_bytes(), "replace CSL snapshot")
}

fn persist_recovery_workspace_snapshot(state: &SessionState, sid: &str) -> Result<(), String> {
    with_workspace_lock(sid, || {
        let ws = workspace_metadata_from_live_state(state, sid);
        astra_services::session_workspace::write_workspace(&ws)
            .map_err(|e| format!("write workspace: {e}"))
    })
}

pub(crate) fn persist_recovery_checkpoint(
    state: &SessionState,
    sid: &str,
    session_state: &astra_turn_core::conversation_log::SessionStateCompact,
    previous_heavy: Option<&astra_pipeline::step_protocol::HeavyCheckpoint>,
) -> Result<RecoveryCheckpointRollback, String> {
    let next_step = next_step_checkpoint_number(sid)?;
    let composite_index_backup = read_optional_file_bytes(&composite_index_path_for(sid))?;
    let step_cp = build_manual_heavy_step_checkpoint(state, sid, session_state, previous_heavy);
    persist_manual_heavy_and_composite(sid, state.turn, "history-sync", next_step, &step_cp)?;
    Ok(RecoveryCheckpointRollback {
        step_number: next_step,
        composite_index_backup,
    })
}

pub(crate) fn rollback_recovery_checkpoint(
    sid: &str,
    rollback: &RecoveryCheckpointRollback,
) -> Result<(), String> {
    let mut rollback_error = String::new();
    if let Err(error) = restore_optional_file_bytes(
        &composite_index_path_for(sid),
        rollback.composite_index_backup.clone(),
    ) {
        rollback_error = format!("restore composite snapshot index: {error}");
    }
    let delete_result =
        astra_pipeline::step_checkpoint::delete_step_checkpoint(sid, rollback.step_number, "heavy")
            .map_err(|e| {
                format!(
                    "delete heavy step checkpoint {:06}: {e}",
                    rollback.step_number
                )
            });
    if let Err(error) = delete_result {
        if rollback_error.is_empty() {
            rollback_error = error;
        } else {
            rollback_error.push_str(&format!("; {error}"));
        }
    }
    if rollback_error.is_empty() {
        Ok(())
    } else {
        Err(rollback_error)
    }
}

/// Persist the current in-memory conversation state after a manual history mutation
/// (`/undo`, `/redo`, `/compact`, `/session fork`) so the next resume/fork/headless
/// continuation sees the same context the user just saw.
pub(crate) async fn sync_recovery_snapshot_after_history_edit(
    state: &mut SessionState,
) -> Result<(), String> {
    let Some(sid) = state.session_id.clone().filter(|sid| !sid.is_empty()) else {
        return Ok(());
    };

    super::session_projection::rebuild_continuation_anchor_from_live_state(state).await;
    let (previous_heavy, prev_state) = load_previous_recovery_state(state, &sid).await?;
    let session_state = super::session_projection::build_full_session_state_compact(
        state,
        super::session_projection::CslCheckpointFields::default(),
        &prev_state,
    );
    let messages = super::session_projection::history_as_messages(&state.history);
    let workspace_path = workspace_path_for(&sid);
    let workspace_backup = read_optional_file_bytes(&workspace_path)?;

    let checkpoint_rollback =
        persist_recovery_checkpoint(state, &sid, &session_state, previous_heavy.as_ref())?;
    if let Err(error) = persist_recovery_workspace_snapshot(state, &sid) {
        let mut error_message = error;
        append_rollback_error(
            &mut error_message,
            "workspace",
            restore_optional_file_bytes(&workspace_path, workspace_backup.clone()),
        );
        append_rollback_error(
            &mut error_message,
            "recovery checkpoint",
            rollback_recovery_checkpoint(&sid, &checkpoint_rollback),
        );
        return Err(error_message);
    }
    if let Err(error) = rebuild_csl_from_history(state, &sid, &messages, &session_state).await {
        let mut error_message = error;
        append_rollback_error(
            &mut error_message,
            "workspace",
            restore_optional_file_bytes(&workspace_path, workspace_backup),
        );
        append_rollback_error(
            &mut error_message,
            "recovery checkpoint",
            rollback_recovery_checkpoint(&sid, &checkpoint_rollback),
        );
        return Err(error_message);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_pipeline::step_checkpoint::read_composite_snapshot_index;
    use astra_pipeline::step_protocol::StepCheckpoint;

    fn isolated_sessions_dir() -> (tempfile::TempDir, session_journal::JournalDirGuard) {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let guard = session_journal::JournalDirGuard::new(&sessions);
        (tmp, guard)
    }

    fn workspace_backup_path_for(session_id: &str) -> Option<std::path::PathBuf> {
        let workspace_dir = astra_services::session_workspace::workspace_dir_for(session_id);
        std::fs::read_dir(workspace_dir)
            .ok()?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("workspace.yaml.corrupt-"))
            })
    }

    #[test]
    #[serial_test::serial]
    fn workspace_metadata_from_live_state_rebuilds_missing_workspace() {
        let (_tmp, _guard) = isolated_sessions_dir();
        let sid = format!("workspace-live-missing-{}", uuid::Uuid::new_v4());
        let state = SessionState {
            session_id: Some(sid.clone()),
            model: Some("gpt-5".to_string()),
            turn: 3,
            total_prompt_tokens: 111,
            total_completion_tokens: 222,
            total_cache_read_tokens: 33,
            total_cache_creation_tokens: 44,
            ..Default::default()
        };

        let ws = workspace_metadata_from_live_state(&state, &sid);
        assert_eq!(ws.session_id, sid);
        assert_eq!(ws.turn_count, 3);
        assert_eq!(ws.total_tokens_in, 111);
        assert_eq!(ws.total_tokens_out, 222);
        assert_eq!(ws.total_cache_read_tokens, 33);
        assert_eq!(ws.total_cache_creation_tokens, 44);
        assert_eq!(ws.status, "active");
        assert_eq!(ws.model.as_deref(), Some("gpt-5"));
    }

    #[test]
    #[serial_test::serial]
    fn workspace_metadata_from_live_state_recovers_from_corrupt_workspace() {
        let (_tmp, _guard) = isolated_sessions_dir();
        let sid = format!("workspace-live-corrupt-{}", uuid::Uuid::new_v4());
        let mut persisted =
            astra_services::session_workspace::WorkspaceMetadata::new(&sid, "gpt-4");
        persisted.git_root = Some("/repo".to_string());
        astra_services::session_workspace::write_workspace(&persisted).unwrap();
        let workspace_path = astra_services::session_workspace::workspace_file_path(&sid).unwrap();
        let corrupt_bytes = b":\nnot-valid-yaml".to_vec();
        std::fs::write(&workspace_path, &corrupt_bytes).unwrap();

        let state = SessionState {
            session_id: Some(sid.clone()),
            model: Some("gpt-5".to_string()),
            turn: 4,
            total_prompt_tokens: 500,
            total_completion_tokens: 250,
            total_cache_read_tokens: 80,
            total_cache_creation_tokens: 20,
            ..Default::default()
        };

        let ws = workspace_metadata_from_live_state(&state, &sid);
        assert_eq!(ws.session_id, sid);
        assert_eq!(ws.turn_count, 4);
        assert_eq!(ws.total_tokens_in, 500);
        assert_eq!(ws.total_tokens_out, 250);
        assert_eq!(ws.total_cache_read_tokens, 80);
        assert_eq!(ws.total_cache_creation_tokens, 20);
        assert_eq!(ws.status, "active");
        assert_eq!(ws.model.as_deref(), Some("gpt-5"));
        assert!(!ws.cwd.is_empty());
        assert!(!ws.created_at.is_empty());
        let backup =
            workspace_backup_path_for(&sid).expect("corrupt workspace should be backed up");
        assert_eq!(std::fs::read(backup).unwrap(), corrupt_bytes);
    }

    #[test]
    #[serial_test::serial]
    fn workspace_metadata_from_live_state_recovers_checkpoint_turns_from_index() {
        let (_tmp, _guard) = isolated_sessions_dir();
        let sid = format!("workspace-live-checkpoints-{}", uuid::Uuid::new_v4());
        let checkpoint_dir = session_journal::local_sessions_dir()
            .join(&sid)
            .join("checkpoints");
        std::fs::create_dir_all(&checkpoint_dir).unwrap();
        std::fs::write(
            checkpoint_dir.join("index.md"),
            "# Checkpoint Index\n\n  001 - Turn  3 - First\n  002 - Turn  6 - Second\n",
        )
        .unwrap();

        let state = SessionState {
            session_id: Some(sid.clone()),
            model: Some("gpt-5".to_string()),
            turn: 7,
            total_prompt_tokens: 700,
            total_completion_tokens: 300,
            ..Default::default()
        };

        let ws = workspace_metadata_from_live_state(&state, &sid);
        assert_eq!(ws.checkpoints, vec![3, 6]);
    }

    #[test]
    #[serial_test::serial]
    fn workspace_metadata_from_live_state_preserves_monotonic_counters() {
        let (_tmp, _guard) = isolated_sessions_dir();
        let sid = format!("workspace-live-monotonic-{}", uuid::Uuid::new_v4());
        let mut persisted =
            astra_services::session_workspace::WorkspaceMetadata::new(&sid, "gpt-4");
        persisted.turn_count = 5;
        persisted.total_tokens_in = 500;
        persisted.total_tokens_out = 250;
        persisted.total_cache_read_tokens = 80;
        persisted.total_cache_creation_tokens = 20;
        astra_services::session_workspace::write_workspace(&persisted).unwrap();

        let state = SessionState {
            session_id: Some(sid.clone()),
            model: Some("gpt-5".to_string()),
            turn: 3,
            total_prompt_tokens: 100,
            total_completion_tokens: 50,
            total_cache_read_tokens: 10,
            total_cache_creation_tokens: 5,
            ..Default::default()
        };

        let ws = workspace_metadata_from_live_state(&state, &sid);
        assert_eq!(ws.turn_count, 5);
        assert_eq!(ws.total_tokens_in, 500);
        assert_eq!(ws.total_tokens_out, 250);
        assert_eq!(ws.total_cache_read_tokens, 80);
        assert_eq!(ws.total_cache_creation_tokens, 20);
    }

    #[test]
    #[serial_test::serial]
    fn workspace_metadata_from_live_state_recovers_counters_from_journal() {
        let (_tmp, _guard) = isolated_sessions_dir();
        let sid = format!("workspace-live-journal-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                2,
                Some("gpt-5"),
                "continue",
                "done",
                0,
                120,
                45,
                30,
            ))
            .unwrap();

        let state = SessionState {
            session_id: Some(sid.clone()),
            model: Some("gpt-5".to_string()),
            ..Default::default()
        };

        let ws = workspace_metadata_from_live_state(&state, &sid);
        assert_eq!(ws.turn_count, 2);
        assert_eq!(ws.total_tokens_in, 120);
        assert_eq!(ws.total_tokens_out, 45);
    }

    #[test]
    fn sync_plan_fields_copies_repl_into_workspace() {
        let mut state = SessionState::default();
        state.executing_plan_goal = Some("goal-x".to_string());
        state.plan_execution_rounds = 9;
        state.plan_execution_corrections = vec!["note".to_string()];

        let mut ws = astra_services::session_workspace::WorkspaceMetadata::new("sid-plan", "m");
        sync_plan_fields_to_workspace(&state, &mut ws);

        assert_eq!(ws.plan_goal.as_deref(), Some("goal-x"));
        assert_eq!(ws.plan_execution_rounds, 9);
        assert_eq!(ws.plan_corrections, vec!["note".to_string()]);
    }

    #[test]
    fn sync_context_trace_copies_latest_trace_into_workspace() {
        let mut state = SessionState::default();
        let mut obs = astra_runtime::observability::ObservabilitySession::new_simple("sid-trace");
        obs.context_traces
            .push(astra_turn_core::context_assembly_trace::ContextAssemblyTrace {
                turn_id: "turn-3".into(),
                tools: astra_turn_core::context_assembly_trace::ToolSelectionTrace {
                    selection_strategy: "code-intel".into(),
                    selection_confidence: 0.92,
                    tools_selected: vec![astra_turn_core::context_assembly_trace::ToolSelected {
                        tool_name: "lsp".into(),
                        score: 1.0,
                        tokens: 0,
                        selection_factors: Vec::new(),
                    }],
                    ..Default::default()
                },
                memory: astra_turn_core::context_assembly_trace::MemoryRetrievalTrace {
                    query: "resume trace persistence".into(),
                    memories_selected: vec![astra_turn_core::context_assembly_trace::MemorySelection {
                        memory_id: "m1".into(),
                        memory_type: "semantic".into(),
                        content_preview: "trace".into(),
                        relevance_score: 0.8,
                        tokens: 10,
                        source: astra_turn_core::context_assembly_trace::MemorySource::Memoria,
                    }],
                    ..Default::default()
                },
                history: astra_turn_core::context_assembly_trace::HistorySelectionTrace {
                    turns_compressed: vec![astra_turn_core::context_assembly_trace::TurnCompression {
                        turn_index: 1,
                        role: "assistant".into(),
                        original_tokens: 100,
                        compressed_tokens: 50,
                        compression_method:
                            astra_turn_core::context_assembly_trace::CompressionMethod::ReactiveCompact,
                        information_lost: Vec::new(),
                    }],
                    compression_ratio: 0.5,
                    tokens_before: 100,
                    tokens_after: 50,
                    ..Default::default()
                },
                token_budget: astra_turn_core::context_assembly_trace::TokenBudgetTrace {
                    max_tokens: 16_000,
                    total_used: 8_200,
                    budget_pressure: 0.76,
                    ..Default::default()
                },
                explanations: vec![astra_turn_core::context_assembly_trace::DecisionExplanation {
                    decision_type:
                        astra_turn_core::context_assembly_trace::DecisionType::StrategyChoice {
                            strategy: "code-intel".into(),
                        },
                    reasoning: "Need symbol-aware context.".into(),
                    alternatives_considered: Vec::new(),
                    confidence: 0.9,
                }],
                ..Default::default()
            });
        state.observability_session = Some(std::sync::Arc::new(std::sync::RwLock::new(obs)));

        let mut ws = astra_services::session_workspace::WorkspaceMetadata::new("sid-trace", "m");
        sync_context_trace_to_workspace(&state, &mut ws);

        let trace = ws.last_context_trace.expect("missing trace summary");
        assert_eq!(trace.turn_id, "turn-3");
        assert_eq!(
            trace
                .tool_selection
                .as_ref()
                .map(|selection| selection.selected_tools.clone()),
            Some(vec!["lsp".to_string()])
        );
        assert_eq!(
            trace
                .tool_selection
                .as_ref()
                .map(|selection| selection.selection_scope.as_str()),
            Some("latest_round")
        );
        assert_eq!(
            trace
                .memory
                .as_ref()
                .map(|memory| memory.selected_memory_ids.len()),
            Some(1)
        );
        assert_eq!(
            trace.budget.as_ref().map(|budget| budget.total_used),
            Some(8_200)
        );
    }

    #[test]
    fn next_step_checkpoint_number_empty_dir_starts_at_one() {
        let (_tmp, _g) = isolated_sessions_dir();
        assert_eq!(next_step_checkpoint_number("sess-empty").unwrap(), 1);
    }

    #[test]
    fn next_step_checkpoint_number_one_after_max_file() {
        let (tmp, _g) = isolated_sessions_dir();
        let sid = "sess-step";
        let cp_dir = tmp
            .path()
            .join("sessions")
            .join(sid)
            .join("step_checkpoints");
        std::fs::create_dir_all(&cp_dir).unwrap();
        std::fs::write(cp_dir.join("000007-heavy.json"), "{}").unwrap();
        assert_eq!(next_step_checkpoint_number(sid).unwrap(), 8);
    }

    #[test]
    fn manual_heavy_checkpoint_maps_history_to_openai_messages() {
        let mut state = SessionState::default();
        state.history.push(("u1".into(), "a1".into()));
        state.history.push(("u2".into(), "a2".into()));
        state.recent_tools = vec!["bash".to_string()];
        state.turn = 4;
        state.total_prompt_tokens = 11;
        state.total_completion_tokens = 22;
        state.run_id = Some("run-z".to_string());

        let checkpoint = build_manual_heavy_step_checkpoint(
            &state,
            "sess-h",
            &astra_turn_core::conversation_log::SessionStateCompact::default(),
            None,
        );
        let StepCheckpoint::Heavy(heavy) = checkpoint else {
            panic!("expected Heavy checkpoint");
        };
        assert_eq!(heavy.messages.len(), 4);
        assert_eq!(heavy.messages[0]["role"], "user");
        assert_eq!(heavy.messages[0]["content"], "u1");
        assert_eq!(heavy.messages[3]["content"], "a2");
        assert_eq!(heavy.recent_tools, vec!["bash".to_string()]);
        assert_eq!(heavy.light.agent_id, "sess-h");
        assert_eq!(heavy.light.task_id, "run-z");
        assert_eq!(heavy.light.total_tokens, 33);
    }

    #[test]
    fn persist_manual_heavy_and_composite_writes_heavy_and_index() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = "sess-heavy-idx";
        let state = SessionState::default();
        let step_checkpoint = build_manual_heavy_step_checkpoint(
            &state,
            sid,
            &astra_turn_core::conversation_log::SessionStateCompact::default(),
            None,
        );

        let heavy_path =
            persist_manual_heavy_and_composite(sid, 2, "label-z", 1, &step_checkpoint).unwrap();
        assert!(heavy_path.exists());
        assert!(heavy_path.to_string_lossy().ends_with("-heavy.json"));

        let index = read_composite_snapshot_index(sid).unwrap();
        assert_eq!(index.snapshots.len(), 1);
        assert_eq!(index.snapshots[0].label.as_deref(), Some("manual:label-z"));
    }

    #[test]
    fn manual_heavy_checkpoint_preserves_recovery_fields_from_prior_state() {
        let mut state = SessionState::default();
        state.recent_tools = vec!["bash".to_string()];
        state.turn = 3;
        state.total_prompt_tokens = 40;
        state.total_completion_tokens = 12;
        state.config_version_id = Some("cfg-live".to_string());
        state.history.push(("u1".into(), "a1".into()));

        let previous_heavy = astra_pipeline::step_protocol::HeavyCheckpoint {
            light: astra_pipeline::step_protocol::LightCheckpoint {
                protocol_version: astra_pipeline::step_protocol::PROTOCOL_VERSION,
                cursor: Default::default(),
                step_id: "session-turn-3".to_string(),
                task_id: "task-3".to_string(),
                agent_id: "sess-prev".to_string(),
                progress: 1.0,
                total_tokens: 52,
                created_at: astra_pipeline::step_protocol::epoch_ms(),
            },
            messages: Vec::new(),
            budget_remaining_tokens: 321,
            budget_remaining_rounds: 7,
            blocked_tools: vec!["write_file".to_string()],
            recent_tools: vec!["read_file".to_string()],
            memory_context: Some(astra_pipeline::step_protocol::MemoryContext {
                retrieved_memory_ids: vec!["m-1".to_string()],
                domain_hints: vec!["rust".to_string()],
                boost_terms: vec!["resume".to_string()],
                provenance: vec!["memoria".to_string()],
                governance_actions: Vec::new(),
                cluster_insights: Vec::new(),
                snapshot_id: Some("snapshot-1".to_string()),
            }),
            delegation_id: None,
            delegation_pattern: None,
            delegation_sub_run_summaries: Vec::new(),
            interruption: Some(serde_json::json!({"kind": "budget_exhausted"})),
            approval_overrides: Some(serde_json::json!({"tool": "bash"})),
            consecutive_context_window_errors: 2,
            pipeline_state: Some(serde_json::json!({"ema": 0.9})),
            compaction_state: Some(serde_json::json!({"attempt_count": 2})),
            config_version_id: Some("cfg-old".to_string()),
        };

        let session_state = session_state_compact_from_heavy_checkpoint(&previous_heavy);
        let checkpoint = build_manual_heavy_step_checkpoint(
            &state,
            "sess-prev",
            &session_state,
            Some(&previous_heavy),
        );
        let StepCheckpoint::Heavy(heavy) = checkpoint else {
            panic!("expected Heavy checkpoint");
        };
        assert_eq!(heavy.budget_remaining_tokens, 321);
        assert_eq!(heavy.budget_remaining_rounds, 7);
        assert_eq!(heavy.blocked_tools, vec!["write_file".to_string()]);
        let memory_context = heavy.memory_context.expect("memory context");
        assert_eq!(memory_context.retrieved_memory_ids, vec!["m-1".to_string()]);
        assert_eq!(memory_context.domain_hints, vec!["rust".to_string()]);
        assert_eq!(memory_context.boost_terms, vec!["resume".to_string()]);
        assert_eq!(memory_context.provenance, vec!["memoria".to_string()]);
        assert_eq!(memory_context.snapshot_id.as_deref(), Some("snapshot-1"));
        assert_eq!(
            heavy.interruption,
            Some(serde_json::json!({"kind": "budget_exhausted"}))
        );
        assert_eq!(
            heavy.approval_overrides,
            Some(serde_json::json!({"tool": "bash"}))
        );
        assert_eq!(heavy.consecutive_context_window_errors, 2);
        assert_eq!(heavy.pipeline_state, Some(serde_json::json!({"ema": 0.9})));
        assert_eq!(
            heavy.compaction_state,
            Some(serde_json::json!({"attempt_count": 2}))
        );
        assert_eq!(heavy.config_version_id.as_deref(), Some("cfg-live"));
    }

    #[test]
    fn read_optional_file_bytes_returns_missing_and_existing_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("blob.bin");

        assert_eq!(read_optional_file_bytes(&path).unwrap(), None);

        std::fs::write(&path, b"hello").unwrap();
        assert_eq!(
            read_optional_file_bytes(&path).unwrap(),
            Some(b"hello".to_vec())
        );
    }

    #[test]
    fn restore_optional_file_bytes_writes_and_removes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("blob.bin");

        restore_optional_file_bytes(&path, Some(b"restored".to_vec())).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"restored");

        restore_optional_file_bytes(&path, None).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn write_bytes_atomic_replaces_existing_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("atom.txt");
        std::fs::write(&path, b"old").unwrap();

        write_bytes_atomic(&path, b"new", "test atomic write").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert!(
            !tmp.path().join(".tmp-atom.txt").exists(),
            "temporary file should not survive atomic replace"
        );
    }

    #[test]
    fn write_bytes_atomic_surfaces_temporary_path_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("atom.txt");
        std::fs::create_dir(tmp.path().join(".tmp-atom.txt")).unwrap();

        let error = write_bytes_atomic(&path, b"new", "test atomic write")
            .expect_err("directory conflict should fail atomic write");

        assert!(error.contains("create temporary file"), "{error}");
        assert!(!path.exists(), "failed atomic write must not create target");
    }

    #[test]
    fn write_full_csl_snapshot_atomic_persists_snapshot_without_tmp_file() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = format!("write-csl-{}", uuid::Uuid::new_v4());
        let messages = vec![
            serde_json::json!({"role": "user", "content": "hello"}),
            serde_json::json!({"role": "assistant", "content": "world"}),
        ];
        let session_state = astra_turn_core::conversation_log::SessionStateCompact {
            recent_tools: vec!["bash".into()],
            ..Default::default()
        };

        write_full_csl_snapshot_atomic(&sid, 2, &messages, &session_state).unwrap();

        let csl_path = csl_log_path_for(&sid);
        assert!(csl_path.exists());
        assert!(
            !csl_path
                .parent()
                .unwrap()
                .join(".tmp-conversation_log.jsonl")
                .exists(),
            "temporary file should not survive atomic write"
        );

        let line = std::fs::read_to_string(&csl_path)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string();
        let entry: astra_turn_core::conversation_log::CslEntry =
            serde_json::from_str(&line).unwrap();
        match entry {
            astra_turn_core::conversation_log::CslEntry::Snapshot {
                turn,
                messages: restored,
                session_state: restored_state,
                ..
            } => {
                assert_eq!(turn, 2);
                assert_eq!(restored, messages);
                assert_eq!(restored_state.recent_tools, vec!["bash".to_string()]);
            }
            other => panic!("expected snapshot entry, got {other:?}"),
        }
    }

    #[test]
    fn with_workspace_lock_releases_lock_after_panic() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = format!("workspace-lock-{}", uuid::Uuid::new_v4());

        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_workspace_lock(&sid, || -> Result<(), String> {
                panic!("boom");
            })
            .unwrap();
        }));
        assert!(panic_result.is_err(), "panic should propagate to caller");

        let value = with_workspace_lock(&sid, || Ok::<_, String>(42)).unwrap();
        assert_eq!(value, 42, "lock must be released after unwind");
    }

    #[test]
    #[serial_test::serial]
    fn session_workspace_git_root_returns_root_when_workspace_exists() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = format!("git-root-ok-{}", uuid::Uuid::new_v4());
        let mut workspace =
            astra_services::session_workspace::WorkspaceMetadata::new(&sid, "gpt-5");
        workspace.git_root = Some("/repo".to_string());
        astra_services::session_workspace::write_workspace(&workspace).unwrap();

        assert_eq!(
            session_workspace_git_root(Some(&sid)).as_deref(),
            Some("/repo")
        );
    }

    #[test]
    #[serial_test::serial]
    fn session_workspace_git_root_returns_none_for_invalid_workspace() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = format!("git-root-bad-{}", uuid::Uuid::new_v4());
        let mut workspace =
            astra_services::session_workspace::WorkspaceMetadata::new(&sid, "gpt-5");
        workspace.git_root = Some("/repo".to_string());
        astra_services::session_workspace::write_workspace(&workspace).unwrap();
        let workspace_path = astra_services::session_workspace::workspace_file_path(&sid).unwrap();
        std::fs::write(&workspace_path, ":\nnot-valid-yaml").unwrap();

        assert!(session_workspace_git_root(Some(&sid)).is_none());
    }

    #[test]
    fn session_state_compact_from_heavy_checkpoint_extracts_recovery_fields() {
        let heavy = astra_pipeline::step_protocol::HeavyCheckpoint {
            light: astra_pipeline::step_protocol::LightCheckpoint {
                protocol_version: astra_pipeline::step_protocol::PROTOCOL_VERSION,
                cursor: Default::default(),
                step_id: "step-1".to_string(),
                task_id: "task-1".to_string(),
                agent_id: "sess-1".to_string(),
                progress: 1.0,
                total_tokens: 12,
                created_at: astra_pipeline::step_protocol::epoch_ms(),
            },
            messages: Vec::new(),
            budget_remaining_tokens: 321,
            budget_remaining_rounds: 7,
            blocked_tools: vec!["bash".to_string()],
            recent_tools: vec!["read_file".to_string()],
            memory_context: None,
            delegation_id: Some("deleg-1".to_string()),
            delegation_pattern: Some("fan_out".to_string()),
            delegation_sub_run_summaries: vec![
                astra_pipeline::step_protocol::DelegationSubRunSummary {
                    run_id: "sub-1".to_string(),
                    agent_id: "agent-1".to_string(),
                    status: "completed".to_string(),
                    error: None,
                    prompt_tokens: 5,
                    completion_tokens: 3,
                    tool_calls: 1,
                },
            ],
            interruption: Some(serde_json::json!({"kind": "context_overflow"})),
            approval_overrides: Some(serde_json::json!({"bash": "allow"})),
            consecutive_context_window_errors: 2,
            pipeline_state: None,
            compaction_state: Some(serde_json::json!({"attempt_count": 4})),
            config_version_id: None,
        };

        let compact = session_state_compact_from_heavy_checkpoint(&heavy);

        assert_eq!(compact.blocked_tools, vec!["bash".to_string()]);
        assert_eq!(compact.recent_tools, vec!["read_file".to_string()]);
        assert_eq!(
            compact.approval_overrides,
            Some(serde_json::json!({"bash": "allow"}))
        );
        assert_eq!(
            compact.compaction_tracker,
            Some(serde_json::json!({"attempt_count": 4}))
        );
        assert_eq!(compact.budget_remaining_tokens, 321);
        assert_eq!(compact.budget_remaining_rounds, 7);
        assert_eq!(compact.consecutive_ctx_errors, 2);
        assert_eq!(
            compact.interruption,
            Some(serde_json::json!({"kind": "context_overflow"}))
        );
        let delegation = compact.delegation.expect("delegation");
        assert_eq!(delegation.id, "deleg-1");
        assert_eq!(delegation.pattern, "fan_out");
        assert_eq!(delegation.completed_sub_runs.len(), 1);
    }

    #[test]
    fn session_state_compact_from_heavy_checkpoint_drops_partial_delegation() {
        let heavy = astra_pipeline::step_protocol::HeavyCheckpoint {
            light: astra_pipeline::step_protocol::LightCheckpoint {
                protocol_version: astra_pipeline::step_protocol::PROTOCOL_VERSION,
                cursor: Default::default(),
                step_id: "step-1".to_string(),
                task_id: "task-1".to_string(),
                agent_id: "sess-1".to_string(),
                progress: 1.0,
                total_tokens: 12,
                created_at: astra_pipeline::step_protocol::epoch_ms(),
            },
            messages: Vec::new(),
            budget_remaining_tokens: 0,
            budget_remaining_rounds: 0,
            blocked_tools: Vec::new(),
            recent_tools: Vec::new(),
            memory_context: None,
            delegation_id: Some("deleg-1".to_string()),
            delegation_pattern: None,
            delegation_sub_run_summaries: vec![],
            interruption: None,
            approval_overrides: None,
            consecutive_context_window_errors: 0,
            pipeline_state: None,
            compaction_state: None,
            config_version_id: None,
        };

        let compact = session_state_compact_from_heavy_checkpoint(&heavy);

        assert!(
            compact.delegation.is_none(),
            "partial delegation payload should not be reconstructed"
        );
    }

    #[test]
    fn sync_session_state_to_workspace_copies_skills_and_adaptive_state() {
        let mut state = SessionState::default();
        state.session_persistence_error = Some("journal append failed".to_string());
        state.pinned_skills.insert("skill-a".to_string());
        state.discovered_skills.insert("skill-b".to_string());

        let obs = std::sync::Arc::new(std::sync::RwLock::new(
            astra_runtime::observability::ObservabilitySession::new_simple("sid-adaptive"),
        ));
        {
            let mut guard = obs.write().unwrap();
            guard.last_scenario_change_turn = Some(11);
            guard.last_token_budget_direction = -1;
            guard.last_token_budget_change_turn = Some(7);
        }
        state.observability_session = Some(obs);

        let mut ws = astra_services::session_workspace::WorkspaceMetadata::new("sid-adaptive", "m");
        sync_session_state_to_workspace(&state, &mut ws);

        assert_eq!(
            ws.last_persistence_error.as_deref(),
            Some("journal append failed")
        );
        assert_eq!(ws.pinned_skills, vec!["skill-a".to_string()]);
        assert_eq!(ws.discovered_skills, vec!["skill-b".to_string()]);
        assert_eq!(ws.last_scenario_change_turn, Some(11));
        assert_eq!(ws.last_token_budget_direction, -1);
        assert_eq!(ws.last_token_budget_change_turn, Some(7));
        assert!(ws.tuned_config_json.is_some());
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn history_sync_preserves_previous_recovery_state_without_existing_csl() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = format!("history-sync-{}", uuid::Uuid::new_v4());
        let mut state = SessionState {
            session_id: Some(sid.clone()),
            turn: 2,
            history: vec![
                ("question".to_string(), "answer".to_string()),
                ("follow-up".to_string(), "done".to_string()),
            ],
            recent_tools: vec!["bash".to_string()],
            ..Default::default()
        };

        let previous_heavy = astra_pipeline::step_protocol::HeavyCheckpoint {
            light: astra_pipeline::step_protocol::LightCheckpoint {
                protocol_version: astra_pipeline::step_protocol::PROTOCOL_VERSION,
                cursor: Default::default(),
                step_id: "session-turn-2".to_string(),
                task_id: "task-2".to_string(),
                agent_id: sid.clone(),
                progress: 1.0,
                total_tokens: 100,
                created_at: astra_pipeline::step_protocol::epoch_ms(),
            },
            messages: vec![serde_json::json!({"role": "user", "content": "stale"})],
            budget_remaining_tokens: 1234,
            budget_remaining_rounds: 9,
            blocked_tools: vec!["write_file".to_string()],
            recent_tools: vec!["read_file".to_string()],
            memory_context: None,
            delegation_id: None,
            delegation_pattern: None,
            delegation_sub_run_summaries: Vec::new(),
            interruption: Some(serde_json::json!({"kind": "budget_exhausted"})),
            approval_overrides: Some(serde_json::json!({"tool": "bash"})),
            consecutive_context_window_errors: 2,
            pipeline_state: Some(serde_json::json!({"ema": 0.9})),
            compaction_state: Some(serde_json::json!({"attempt_count": 2})),
            config_version_id: None,
        };
        astra_pipeline::step_checkpoint::write_step_checkpoint(
            &sid,
            1,
            &StepCheckpoint::Heavy(Box::new(previous_heavy)),
        )
        .unwrap();

        sync_recovery_snapshot_after_history_edit(&mut state)
            .await
            .expect("history sync should succeed");

        let restored = astra_pipeline::step_restore::restore_session(&sid)
            .unwrap()
            .expect("restored session");
        assert_eq!(restored.messages.len(), 4);
        assert_eq!(restored.blocked_tools, vec!["write_file".to_string()]);
        assert_eq!(restored.budget_remaining_tokens, 1234);
        assert_eq!(restored.budget_remaining_rounds, 9);
        assert_eq!(
            restored.interruption,
            Some(serde_json::json!({"kind": "budget_exhausted"}))
        );
        assert_eq!(
            restored.approval_overrides,
            Some(serde_json::json!({"tool": "bash"}))
        );
        assert_eq!(restored.consecutive_context_window_errors, 2);
        assert_eq!(
            restored.compaction_state,
            Some(serde_json::json!({"attempt_count": 2}))
        );
        assert_eq!(
            restored.pipeline_state,
            Some(serde_json::json!({"ema": 0.9}))
        );

        let store = std::sync::Arc::new(
            astra_turn_core::conversation_log::file_store::FileCslStore::new(
                session_journal::local_sessions_dir(),
            ),
        );
        let mut mgr = astra_turn_core::conversation_log::manager::CslManager::new(
            store,
            sid.clone(),
            Default::default(),
        )
        .unwrap();
        let mat = mgr.load().await.unwrap().expect("csl snapshot");
        assert_eq!(
            mat.session_state.blocked_tools,
            vec!["write_file".to_string()]
        );
        assert_eq!(
            mat.session_state.interruption,
            Some(serde_json::json!({"kind": "budget_exhausted"}))
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn ensure_loaded_csl_state_uses_in_memory_manager_state() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = format!("csl-state-{}", uuid::Uuid::new_v4());
        let store = std::sync::Arc::new(
            astra_turn_core::conversation_log::file_store::FileCslStore::new(
                session_journal::local_sessions_dir(),
            ),
        );
        let mut mgr = astra_turn_core::conversation_log::manager::CslManager::new(
            store,
            sid.clone(),
            Default::default(),
        )
        .unwrap();
        let session_state = astra_turn_core::conversation_log::SessionStateCompact {
            blocked_tools: vec!["write_file".to_string()],
            recent_tools: vec!["bash".to_string()],
            ..Default::default()
        };
        mgr.persist_turn(
            1,
            &[serde_json::json!({"role": "user", "content": "hi"})],
            &session_state,
        )
        .await
        .unwrap();

        let mut state = SessionState {
            csl_manager: Some(mgr),
            ..Default::default()
        };
        let loaded = ensure_loaded_csl_state(&mut state, &sid)
            .await
            .expect("load csl state")
            .expect("in-memory state");
        assert_eq!(loaded.blocked_tools, vec!["write_file".to_string()]);
        assert_eq!(loaded.recent_tools, vec!["bash".to_string()]);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn ensure_loaded_csl_state_returns_none_when_snapshot_missing() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = format!("csl-empty-{}", uuid::Uuid::new_v4());
        let mut state = SessionState::default();

        let loaded = ensure_loaded_csl_state(&mut state, &sid)
            .await
            .expect("load csl state");

        assert!(loaded.is_none());
        assert!(
            state.csl_manager.is_some(),
            "manager should still initialize"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn ensure_loaded_csl_state_returns_err_for_invalid_session_id() {
        let (_tmp, _g) = isolated_sessions_dir();
        let mut state = SessionState::default();

        let error = ensure_loaded_csl_state(&mut state, "../not-a-session")
            .await
            .expect_err("invalid session id should fail");

        assert!(error.contains("initialize CSL state"), "{error}");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn ensure_loaded_csl_state_returns_err_for_corrupt_csl_snapshot() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = format!("csl-corrupt-{}", uuid::Uuid::new_v4());
        let session_dir = session_journal::local_sessions_dir().join(&sid);
        std::fs::create_dir_all(&session_dir).unwrap();
        let csl_path = session_dir.join("conversation_log.jsonl");
        std::fs::write(
            &csl_path,
            "not-json\n{\"kind\":\"snapshot\",\"seq\":1,\"turn\":1,\"messages\":[],\"session_state\":{}}\n",
        )
        .unwrap();

        let mut state = SessionState::default();
        let error = ensure_loaded_csl_state(&mut state, &sid)
            .await
            .expect_err("corrupt csl should fail");

        assert!(error.contains("load CSL state"), "{error}");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn load_previous_recovery_state_returns_err_when_checkpoint_dir_is_invalid() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = format!("checkpoint-bad-{}", uuid::Uuid::new_v4());
        let session_dir = session_journal::local_sessions_dir().join(&sid);
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("step_checkpoints"), "not-a-directory").unwrap();

        let mut state = SessionState::default();
        let error = load_previous_recovery_state(&mut state, &sid)
            .await
            .expect_err("invalid checkpoint directory should fail");

        assert!(error.contains("read latest heavy checkpoint"), "{error}");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn rebuild_csl_from_history_skips_persist_for_empty_turn_zero() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = format!("empty-turn-{}", uuid::Uuid::new_v4());
        let store = std::sync::Arc::new(
            astra_turn_core::conversation_log::file_store::FileCslStore::new(
                session_journal::local_sessions_dir(),
            ),
        );
        let mgr = astra_turn_core::conversation_log::manager::CslManager::new(
            store,
            sid.clone(),
            Default::default(),
        )
        .unwrap();
        let mut state = SessionState {
            csl_manager: Some(mgr),
            ..Default::default()
        };

        rebuild_csl_from_history(
            &mut state,
            &sid,
            &[],
            &astra_turn_core::conversation_log::SessionStateCompact::default(),
        )
        .await
        .expect("empty turn zero should be a no-op");

        let mgr = state.csl_manager.as_ref().expect("manager");
        assert_eq!(mgr.last_seq(), 0);
        assert!(
            !session_journal::local_sessions_dir()
                .join(&sid)
                .join("conversation_log.jsonl")
                .exists(),
            "empty turn zero should not persist a snapshot"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn history_sync_rolls_back_checkpoint_and_index_when_csl_rebuild_fails() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = format!("history-rollback-{}", uuid::Uuid::new_v4());

        let mut existing_index = astra_core::composite_snapshot::CompositeSnapshotIndex::default();
        let mut existing_snapshot =
            astra_core::composite_snapshot::CompositeSnapshotBuilder::new(sid.clone(), 9)
                .label("existing")
                .session_state("000009-heavy.json")
                .workspace_state(sid.clone())
                .build();
        existing_index.append(&mut existing_snapshot).unwrap();
        astra_pipeline::step_checkpoint::write_composite_snapshot_index(&sid, &existing_index)
            .unwrap();

        let mut ws = astra_services::session_workspace::WorkspaceMetadata::new(&sid, "test-model");
        ws.turn_count = 9;
        astra_services::session_workspace::write_workspace(&ws).unwrap();

        let session_dir = session_journal::local_sessions_dir().join(&sid);
        std::fs::create_dir_all(&session_dir).unwrap();
        let csl_path = session_dir.join("conversation_log.jsonl");
        std::fs::write(&csl_path, b"{\"stale\":true}\n").unwrap();
        std::fs::create_dir(session_dir.join(".tmp-conversation_log.jsonl")).unwrap();

        let store = std::sync::Arc::new(
            astra_turn_core::conversation_log::file_store::FileCslStore::new(
                session_journal::local_sessions_dir(),
            ),
        );
        let mgr = astra_turn_core::conversation_log::manager::CslManager::new(
            store,
            sid.clone(),
            Default::default(),
        )
        .unwrap();
        let mut state = SessionState {
            session_id: Some(sid.clone()),
            turn: 1,
            history: vec![("question".into(), "answer".into())],
            csl_manager: Some(mgr),
            ..Default::default()
        };

        let error = sync_recovery_snapshot_after_history_edit(&mut state)
            .await
            .expect_err("temporary CSL path conflict should fail");

        assert!(error.contains("replace CSL snapshot"), "{error}");

        let checkpoints = astra_pipeline::step_checkpoint::list_checkpoints(&sid).unwrap();
        assert!(
            checkpoints.is_empty(),
            "history-sync failure must not leave heavy checkpoint files behind"
        );
        let restored_index = read_composite_snapshot_index(&sid).unwrap();
        assert_eq!(restored_index, existing_index);
        let restored_workspace = astra_services::session_workspace::read_workspace(&sid).unwrap();
        assert_eq!(restored_workspace.turn_count, 9);
        assert_eq!(std::fs::read(&csl_path).unwrap(), b"{\"stale\":true}\n");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn history_sync_rolls_back_when_workspace_yaml_is_corrupt() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = format!("history-workspace-corrupt-{}", uuid::Uuid::new_v4());
        let workspace_dir = astra_services::session_workspace::workspace_dir_for(&sid);
        std::fs::create_dir_all(&workspace_dir).unwrap();
        let workspace_path = workspace_dir.join("workspace.yaml");
        let corrupt_bytes = b":\nnot-valid-yaml".to_vec();
        std::fs::write(&workspace_path, &corrupt_bytes).unwrap();

        let mut state = SessionState {
            session_id: Some(sid.clone()),
            model: Some("test-model".into()),
            turn: 1,
            history: vec![("question".into(), "answer".into())],
            ..Default::default()
        };

        sync_recovery_snapshot_after_history_edit(&mut state)
            .await
            .expect("corrupt workspace should be repaired from live state");

        let checkpoints = astra_pipeline::step_checkpoint::list_checkpoints(&sid).unwrap();
        assert_eq!(checkpoints.len(), 1);
        assert!(
            composite_index_path_for(&sid).exists(),
            "history sync should still persist a composite index"
        );
        let workspace = astra_services::session_workspace::read_workspace(&sid).unwrap();
        assert_eq!(workspace.turn_count, 1);
        assert_eq!(workspace.model.as_deref(), Some("test-model"));
        assert_ne!(std::fs::read(&workspace_path).unwrap(), corrupt_bytes);
        let backup = std::fs::read_dir(&workspace_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("workspace.yaml.corrupt-"))
            })
            .expect("corrupt workspace should be backed up");
        assert_eq!(std::fs::read(backup).unwrap(), corrupt_bytes);
    }

    #[test]
    fn rollback_recovery_checkpoint_restores_index_and_deletes_heavy() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = format!("history-rollback-token-{}", uuid::Uuid::new_v4());

        let mut existing_index = astra_core::composite_snapshot::CompositeSnapshotIndex::default();
        let mut existing_snapshot =
            astra_core::composite_snapshot::CompositeSnapshotBuilder::new(sid.clone(), 4)
                .label("existing")
                .session_state("000004-heavy.json")
                .workspace_state(sid.clone())
                .build();
        existing_index.append(&mut existing_snapshot).unwrap();
        astra_pipeline::step_checkpoint::write_composite_snapshot_index(&sid, &existing_index)
            .unwrap();

        let state = SessionState {
            session_id: Some(sid.clone()),
            turn: 1,
            history: vec![("question".into(), "answer".into())],
            ..Default::default()
        };
        let rollback = persist_recovery_checkpoint(
            &state,
            &sid,
            &astra_turn_core::conversation_log::SessionStateCompact::default(),
            None,
        )
        .unwrap();

        assert_eq!(
            astra_pipeline::step_checkpoint::list_checkpoints(&sid)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            read_composite_snapshot_index(&sid).unwrap().snapshots.len(),
            2
        );

        rollback_recovery_checkpoint(&sid, &rollback).unwrap();

        assert!(
            astra_pipeline::step_checkpoint::list_checkpoints(&sid)
                .unwrap()
                .is_empty(),
            "rollback should delete the just-written heavy checkpoint"
        );
        assert_eq!(read_composite_snapshot_index(&sid).unwrap(), existing_index);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn history_sync_persists_checkpoint_workspace_and_csl_snapshot() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = format!("history-sync-success-{}", uuid::Uuid::new_v4());
        let store = std::sync::Arc::new(
            astra_turn_core::conversation_log::file_store::FileCslStore::new(
                session_journal::local_sessions_dir(),
            ),
        );
        let mgr = astra_turn_core::conversation_log::manager::CslManager::new(
            store,
            sid.clone(),
            Default::default(),
        )
        .unwrap();
        let mut state = SessionState {
            session_id: Some(sid.clone()),
            model: Some("test-model".into()),
            turn: 2,
            history: vec![
                ("question".into(), "answer".into()),
                ("follow-up".into(), "done".into()),
            ],
            recent_tools: vec!["bash".into()],
            csl_manager: Some(mgr),
            ..Default::default()
        };

        sync_recovery_snapshot_after_history_edit(&mut state)
            .await
            .expect("history sync should succeed");

        let checkpoints = astra_pipeline::step_checkpoint::list_checkpoints(&sid).unwrap();
        assert_eq!(checkpoints.len(), 1);
        assert_eq!(checkpoints[0].0, 1);

        let index = read_composite_snapshot_index(&sid).unwrap();
        assert_eq!(index.snapshots.len(), 1);
        assert_eq!(
            index.snapshots[0].session_state(),
            Some("000001-heavy.json")
        );
        assert_eq!(index.snapshots[0].workspace_state(), Some(sid.as_str()));

        let workspace = astra_services::session_workspace::read_workspace(&sid).unwrap();
        assert_eq!(workspace.turn_count, 2);
        assert_eq!(workspace.total_tokens_in, 0);
        assert_eq!(workspace.total_tokens_out, 0);
        assert_eq!(workspace.model.as_deref(), Some("test-model"));

        let csl_path = csl_log_path_for(&sid);
        assert!(
            csl_path.exists(),
            "history sync should persist a CSL snapshot"
        );
        assert_eq!(
            std::fs::read_to_string(&csl_path).unwrap().lines().count(),
            1
        );
        let mgr = state
            .csl_manager
            .as_ref()
            .expect("manager restored after sync");
        assert_eq!(mgr.last_seq(), 1);
        assert_eq!(
            mgr.last_session_state().recent_tools,
            vec!["bash".to_string()]
        );

        let restored = astra_pipeline::step_restore::restore_session(&sid)
            .unwrap()
            .expect("restored session");
        assert_eq!(restored.messages.len(), 4);
    }
}
