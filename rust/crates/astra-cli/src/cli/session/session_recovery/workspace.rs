//! Workspace metadata: build, sync plan/session/context-trace fields, snapshot.
use super::io::with_workspace_lock;
use crate::cli::session::session_state::SessionState;

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

pub(crate) fn context_trace_signal_from_trace(
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

pub(crate) fn latest_context_trace_signal(
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

pub(crate) fn fresh_workspace_metadata(
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
    let journal_state = match super::super::session_runtime::session_state_from_journal(sid) {
        Ok(state) => state,
        Err(error) => {
            astra_core::agent_warn!(
                "workspace",
                "rebuilding workspace metadata from live state without journal state: {error}"
            );
            super::super::session_runtime::RestoredSessionState::default()
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

pub(crate) fn persist_recovery_workspace_snapshot(
    state: &SessionState,
    sid: &str,
) -> Result<(), String> {
    with_workspace_lock(sid, || {
        let ws = workspace_metadata_from_live_state(state, sid);
        astra_services::session_workspace::write_workspace(&ws)
            .map_err(|e| format!("write workspace: {e}"))
    })
}
