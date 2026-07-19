//! Run a turn stream and collect the raw stream result.

use super::turn_cancellation::drain_after_cancel;
use crate::cli::chat_stream::{ChatTurnParams, stream_chat_sse};
use crate::cli::session::session_state::SessionState;
use crate::cli::stream::streaming_types::StreamResult;
use crate::cli::turn::local_run_control::LocalRunControl;
use crossterm::style::Stylize;
use std::sync::Arc;

struct PreparedTurnStreamState {
    cancel_token: Arc<tokio_util::sync::CancellationToken>,
    incremental_state: Arc<astra_turn_core::turn_event_sink::IncrementalTurnState>,
    run_control: Arc<LocalRunControl>,
    tui_cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    observability_hub: Option<std::sync::Arc<astra_runtime::observability::ObservabilityHub>>,
    observability_session: Option<
        std::sync::Arc<std::sync::RwLock<astra_runtime::observability::ObservabilitySession>>,
    >,
    append_system_prompt: Option<String>,
    input_work_unit_observations: Vec<astra_core::work_unit::WorkUnitObservation>,
}

#[derive(Clone, Copy)]
pub(crate) struct TurnExecutionInput<'a> {
    pub(crate) api: &'a astra_thin_client::ThinClient,
    pub(crate) profile: Option<&'a str>,
    pub(crate) token: &'a str,
    pub(crate) message: &'a str,
    pub(crate) user_intent: &'a str,
    pub(crate) input_runtime_required_texts: &'a [String],
    pub(crate) input_runtime_volatile_texts: &'a [String],
    pub(crate) session_id: Option<&'a str>,
    pub(crate) semantic_query_override: Option<&'a str>,
}

pub(crate) struct TurnExecutionRequest<'a> {
    pub(crate) state: &'a mut SessionState,
    pub(crate) input: TurnExecutionInput<'a>,
}

pub(crate) enum TurnAttempt {
    Completed(Box<Result<StreamResult, crate::TurnFailure>>),
    /// User-cancelled (Ctrl+C / TUI cancel) but the stream was awaited to
    /// completion so partial text + tool records reach the same persistence
    /// paths that successful turns use. Carries the same payload as
    /// `Completed`; the distinction is purely policy: skip auth/session
    /// retries (the user said stop) and skip auto-invoke (latency).
    Interrupted(Box<Result<StreamResult, crate::TurnFailure>>),
}

pub(crate) async fn execute_stream_turn(request: TurnExecutionRequest<'_>) -> TurnAttempt {
    let TurnExecutionRequest { state, input } = request;
    let prepared = prepare_turn_stream_state(state).await;
    *astra_core::sync_poison::recover_mutex_lock(&state.active_turn_local_run_control) =
        Some(prepared.run_control.clone());
    let params = build_turn_stream_params(state, input, &prepared);
    let (result, was_user_cancel) =
        await_stream_with_interrupts(params, &prepared, input.api, input.token).await;
    // Release the TUI's sender at transport close, before post-stream
    // persistence. A completed answer must not keep its live typing marker
    // while journal/CSL/workspace work is still settling.
    state.tui_stream_event_tx = None;
    *astra_core::sync_poison::recover_mutex_lock(&state.active_turn_local_run_control) = None;

    if was_user_cancel {
        TurnAttempt::Interrupted(Box::new(result))
    } else {
        TurnAttempt::Completed(Box::new(result))
    }
}

async fn prepare_turn_stream_state(state: &SessionState) -> PreparedTurnStreamState {
    let append_system_prompt = crate::cli::execution_state_summary::format_for_session_state(state);

    // Capture producer truth at the actual model boundary. This refreshes on
    // retries and wake turns and avoids feeding the TUI's rendered XML cache
    // back into lifecycle decisions.
    let input_work_unit_observations = state.active_work_registry.active_work_observations();

    let run_control =
        astra_core::sync_poison::recover_mutex_lock(&state.active_turn_local_run_control)
            .clone()
            .unwrap_or_else(LocalRunControl::shared);

    PreparedTurnStreamState {
        cancel_token: Arc::new(tokio_util::sync::CancellationToken::new()),
        incremental_state: Arc::new(
            astra_turn_core::turn_event_sink::IncrementalTurnState::default(),
        ),
        run_control,
        tui_cancel_token: state.tui_cancel_token.clone(),
        observability_hub: state.observability_hub.clone(),
        observability_session: state.observability_session.clone(),
        append_system_prompt,
        input_work_unit_observations,
    }
}

fn build_turn_stream_params<'a>(
    state: &'a mut SessionState,
    input: TurnExecutionInput<'a>,
    prepared: &'a PreparedTurnStreamState,
) -> ChatTurnParams<'a> {
    ChatTurnParams {
        api: input.api,
        token: input.token,
        auth_profile: input.profile,
        message: input.message,
        user_intent: input.user_intent,
        input_runtime_required_texts: input.input_runtime_required_texts,
        input_runtime_volatile_texts: input.input_runtime_volatile_texts,
        input_work_unit_observations: &prepared.input_work_unit_observations,
        semantic_query_override: input.semantic_query_override,
        session_id: input.session_id,
        model_id: crate::cli::slash::slash_config::active_model_id_for_request(),
        model: astra_core::model_override::normalize_model_override(state.model.as_deref()),
        provider: None,
        explain: state.explain,
        render_md: true,
        history: &state.history,
        perm_manager: &mut state.perm_manager,
        verbose_mode: state.verbose_mode,
        render_policy: state
            .tui_render_policy
            .unwrap_or(crate::cli::stream::stream_render::RenderPolicy::Stream),
        cli_context: Some(&state.cli_context),
        recent_tools: &state.recent_tools,
        activated_deferred_tool_names: Some(&mut state.activated_deferred_tool_names),
        tool_health_entries: &state.tool_health_entries,
        resume_restricted_tools: &state.resume_restricted_tools,
        session_lessons: &state.session_lessons,
        latest_skill_diagnosis: state.latest_skill_diagnosis.as_ref(),
        latest_turn_quality_feedback: state.latest_turn_quality_feedback.as_ref(),
        unified_skill_registry: &state.unified_skill_registry,
        is_plan_subtask: state.current_plan_subtask_id.is_some(),
        plan_subtask_id: state.current_plan_subtask_id.as_deref(),
        delegation_engine: state.delegation_engine.clone(),
        cancel_token: Some(prepared.cancel_token.clone()),
        run_control: Some(prepared.run_control.clone()),
        incremental_state: Some(prepared.incremental_state.clone()),
        plan_assemble_line_release: None,
        stream_event_tx: state.tui_stream_event_tx.clone(),
        agent_live_event_sink: state.tui_agent_live_event_sink.clone(),
        approval_request_tx: state.tui_approval_request_tx.clone(),
        ask_user_request_tx: state.tui_ask_user_request_tx.clone(),
        plan_review_request_tx: state.tui_plan_review_request_tx.clone(),
        mcp_manager: Some(state.mcp_manager.clone()),
        skill_quality_tracker: &mut state.skill_quality_tracker,
        discovered_skills: Some(&mut state.discovered_skills),
        messaging_metrics: state.messaging_metrics.clone(),
        agent_spawner: state.agent_spawner.clone(),
        root_agent_id: Some("main"),
        root_mailbox_slot: Some(&mut state.root_mailbox),
        observability_hub: prepared.observability_hub.clone(),
        observability_session: prepared.observability_session.clone(),
        file_journal: Some(state.file_journal.clone()),
        file_state: Some(state.file_state.clone()),
        database_snapshot_journal: Some(state.database_snapshot_journal.clone()),
        git_stash_journal: Some(state.git_stash_journal.clone()),
        git_commit_journal: Some(state.git_commit_journal.clone()),
        git_worktree_journal: Some(state.git_worktree_journal.clone()),
        session_state_journal: Some(state.session_state_journal.clone()),
        task_manager: Some(state.task_manager.clone()),
        task_notify_tx: state.task_notify_tx.clone(),
        bg_task_commands: Some(state.bg_task_commands.clone()),
        bg_task_list_cache: Some(state.bg_task_list_cache.clone()),
        bash_detach_slot: Some(state.bash_detach_slot.clone()),
        turn_index: state.turn.saturating_add(1),
        pipeline_state: state.runtime_pipeline_state.clone(),
        compaction_state: state.runtime_compaction_state.clone(),
        consecutive_context_window_errors: state.runtime_consecutive_context_window_errors,
        // Headless read observations share a lifecycle with the turn-local
        // workspace epoch. They must not be imported from session state.
        idempotency_cache: None,
        pre_loaded_messages: None,
        append_system_prompt: prepared.append_system_prompt.clone(),
        session_memory_extractor: state.session_memory_extractor.clone(),
        #[cfg(feature = "harness")]
        harness_sink: Some(state.harness_sink.clone()),
        #[cfg(feature = "harness")]
        harness_trace: Some(state.harness_trace.clone()),
        #[cfg(feature = "harness")]
        benchmark_profile: None,
    }
}

async fn await_stream_with_interrupts<'a>(
    params: ChatTurnParams<'a>,
    prepared: &PreparedTurnStreamState,
    api: &astra_thin_client::ThinClient,
    bearer: &str,
) -> (Result<StreamResult, crate::TurnFailure>, bool) {
    let cancel_token_for_signal = prepared.cancel_token.clone();
    let incremental_state = prepared.incremental_state.clone();
    let tui_cancel_token = prepared.tui_cancel_token.clone();
    let stream_fut = stream_chat_sse(params);
    tokio::pin!(stream_fut);

    let drain_timeout = std::time::Duration::from_secs(10);
    tokio::select! {
        biased;
        result = &mut stream_fut => (result, false),
        _ = tokio::signal::ctrl_c() => {
            prepared.run_control.request_cancel();
            cancel_token_for_signal.cancel();
            notify_server_to_cancel_run(api, bearer, &incremental_state, "Ctrl+C");
            if tui_cancel_token.is_none() {
                eprintln!("\n{}", "  Interrupted. (press Ctrl+C again to force-exit, or wait up to 10s for graceful drain)".dim());
            }
            let drained = drain_after_cancel(
                &mut stream_fut,
                drain_timeout,
                "Ctrl+C",
                incremental_state.clone(),
            ).await;
            (drained, true)
        }
        _ = async {
            match tui_cancel_token.as_ref() {
                Some(token) => token.cancelled().await,
                None => std::future::pending().await,
            }
        } => {
            prepared.run_control.request_cancel();
            cancel_token_for_signal.cancel();
            notify_server_to_cancel_run(api, bearer, &incremental_state, "TUI cancel");
            let drained = drain_after_cancel(
                &mut stream_fut,
                drain_timeout,
                "TUI cancel",
                incremental_state.clone(),
            ).await;
            (drained, true)
        }
    }
}

/// Fire-and-forget: tell the server to cancel the durable run that is
/// currently streaming. Without this call, the only effect of a user
/// cancellation is closing the local SSE stream — the server-side run
/// keeps executing tool calls and burning tokens until it finishes
/// naturally. We snapshot the run_id from the incremental state (the SSE
/// loop populates it as soon as the server emits the run header), then
/// dispatch a DELETE in the background so cancellation does not block
/// the drain path. Errors are logged at warn level only — the user has
/// already asked to stop, and surfacing a cancel-API failure on top of
/// that adds noise without giving them an action to take.
fn notify_server_to_cancel_run(
    api: &astra_thin_client::ThinClient,
    bearer: &str,
    incremental_state: &Arc<astra_turn_core::turn_event_sink::IncrementalTurnState>,
    source: &'static str,
) {
    let snap = incremental_state.snapshot();
    let Some(run_id) = snap.run_id else {
        tracing::debug!(
            target: "astra::cli::cancel",
            source,
            "no run_id captured yet; server-side cancel skipped"
        );
        return;
    };
    let api = api.clone();
    let bearer = bearer.to_string();
    tokio::spawn(async move {
        match api.cancel_run(Some(&bearer), &run_id).await {
            Ok(_) => tracing::info!(
                target: "astra::cli::cancel",
                run_id = %run_id,
                source,
                "server-side run cancellation requested"
            ),
            Err(e) => tracing::warn!(
                target: "astra::cli::cancel",
                run_id = %run_id,
                source,
                error = %e,
                "server-side run cancellation failed"
            ),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{
        PreparedTurnStreamState, TurnExecutionInput, build_turn_stream_params,
        prepare_turn_stream_state,
    };
    use crate::cli::session::session_state::SessionState;
    use crate::cli::turn::local_run_control::LocalRunControl;
    use std::sync::Arc;

    #[tokio::test]
    async fn prepare_turn_stream_state_does_not_inject_task_board_prompt() {
        let state = SessionState::default();
        state.task_manager.rebind("sess-stream");
        let create = state
            .task_manager
            .create(&serde_json::json!({"title": "Validate stream projection"}))
            .await;
        assert!(!create.starts_with("Error:"), "{create}");

        let prepared = prepare_turn_stream_state(&state).await;
        let prompt = prepared.append_system_prompt.as_deref().unwrap_or_default();
        assert!(
            !prompt.contains("Validate stream projection")
                && !prompt.contains("Active task board")
                && !prompt.contains("Active Task Board"),
            "task board must flow through plan context, not append_system_prompt: {prompt}"
        );
    }

    #[tokio::test]
    async fn prepare_turn_stream_state_reuses_preinstalled_tui_run_control() {
        let state = SessionState::default();
        let preinstalled = LocalRunControl::shared();
        *astra_core::sync_poison::recover_mutex_lock(&state.active_turn_local_run_control) =
            Some(preinstalled.clone());

        let prepared = prepare_turn_stream_state(&state).await;

        assert!(
            Arc::ptr_eq(&prepared.run_control, &preinstalled),
            "pre-turn queued TUI input must flow into the same provider used by the stream"
        );
    }

    #[tokio::test]
    async fn prepare_turn_stream_state_captures_canonical_active_fanout_truth() {
        let transport = Arc::new(astra_messaging::InProcessTransport::new());
        let tracker = Arc::new(astra_runtime::server::delegation::engine::DelegationTracker::new());
        let router = Arc::new(astra_messaging::AgentMailboxRouter::new(transport, tracker));
        let active_work_registry = Arc::new(astra_core::work_unit::ActiveWorkRegistry::default());
        let spawner = Arc::new(
            astra_runtime::orchestration::DynamicAgentSpawner::new(router)
                .with_active_work_registry(active_work_registry.clone()),
        );
        spawner
            .declare_fanout_group(
                "review-group",
                "Three-angle review",
                3,
                Some("tool-fanout"),
                "prior-root-run",
            )
            .await
            .unwrap();
        let state = SessionState {
            agent_spawner: Some(spawner),
            active_work_registry,
            ..SessionState::default()
        };

        let prepared = prepare_turn_stream_state(&state).await;

        assert_eq!(prepared.input_work_unit_observations.len(), 1);
        let observation = &prepared.input_work_unit_observations[0];
        assert_eq!(observation.id, "review-group");
        assert_eq!(observation.kind, "agent_fanout");
        assert_eq!(
            observation.status,
            astra_core::work_unit::WorkUnitStatus::Pending
        );
        assert_eq!(
            observation.wake_policy,
            astra_core::work_unit::WorkUnitWakePolicy::OnAttentionOrTerminal
        );
    }

    #[test]
    fn build_turn_stream_params_respects_render_policy_and_plan_subtask() {
        let mut state = SessionState {
            tui_render_policy: Some(crate::cli::stream::stream_render::RenderPolicy::Silent),
            current_plan_subtask_id: Some("subtask-1".into()),
            resume_restricted_tools: vec!["read_file".into()],
            runtime_pipeline_state: Some(serde_json::json!({"stats": {"ema": 0.7}})),
            runtime_compaction_state: Some(serde_json::json!({
                "attempt_count": 2,
                "cumulative_tokens_freed": 12000,
                "last_tokens_freed": 3000,
                "last_was_insufficient": true,
                "consecutive_futile_attempts": 1,
            })),
            runtime_consecutive_context_window_errors: 2,
            ..SessionState::default()
        };
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let prepared = PreparedTurnStreamState {
            cancel_token: Arc::new(tokio_util::sync::CancellationToken::new()),
            incremental_state: Arc::new(
                astra_turn_core::turn_event_sink::IncrementalTurnState::default(),
            ),
            run_control: LocalRunControl::shared(),
            tui_cancel_token: None,
            observability_hub: None,
            observability_session: None,
            append_system_prompt: Some("task board".into()),
            input_work_unit_observations: Vec::new(),
        };

        let params = build_turn_stream_params(
            &mut state,
            TurnExecutionInput {
                api: &api,
                profile: Some("profile"),
                token: "token",
                message: "continue",
                user_intent: "continue",
                input_runtime_required_texts: &[],
                input_runtime_volatile_texts: &[],
                session_id: Some("sess-1"),
                semantic_query_override: None,
            },
            &prepared,
        );

        assert_eq!(
            params.render_policy,
            crate::cli::stream::stream_render::RenderPolicy::Silent
        );
        assert!(params.is_plan_subtask);
        assert_eq!(params.plan_subtask_id, Some("subtask-1"));
        assert_eq!(params.append_system_prompt.as_deref(), Some("task board"));
        assert_eq!(params.resume_restricted_tools, &["read_file".to_string()]);
        assert_eq!(
            params.pipeline_state,
            Some(serde_json::json!({"stats": {"ema": 0.7}}))
        );
        assert_eq!(
            params.compaction_state,
            Some(serde_json::json!({
                "attempt_count": 2,
                "cumulative_tokens_freed": 12000,
                "last_tokens_freed": 3000,
                "last_was_insufficient": true,
                "consecutive_futile_attempts": 1,
            }))
        );
        assert_eq!(params.consecutive_context_window_errors, 2);
        assert!(
            params.idempotency_cache.is_none(),
            "turn-local observation cache must start cold"
        );
        assert!(params.cancel_token.is_some());
        assert!(params.incremental_state.is_some());
    }
}
