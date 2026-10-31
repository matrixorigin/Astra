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
    /// Canonical session identity established before interactive turn admission.
    ///
    /// Unlike headless requests, an interactive turn owns durable local state
    /// from its first boundary and must never enter the agentic loop with a
    /// provisional or display-only identity.
    pub(crate) session_id: &'a str,
    pub(crate) admitted_server_turn: Option<&'a super::turn_entry::AdmittedServerTurn>,
    pub(crate) semantic_query_override: Option<&'a str>,
    pub(crate) explain_analyze_terminal_degraded: Option<&'a std::sync::atomic::AtomicBool>,
}

pub(crate) struct TurnExecutionRequest<'a> {
    pub(crate) state: &'a mut SessionState,
    pub(crate) input: TurnExecutionInput<'a>,
}

pub(crate) enum TurnAttempt {
    /// Local preflight rejected the input before execution began.
    NotStarted(Box<crate::TurnFailure>),
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
    // Capture the admitted owner after admission/validation but before the
    // stream is built; only this snapshot may drive in-flight refresh.
    let auth_owner = match input.admitted_server_turn {
        Some(turn) => turn.auth_owner.clone(),
        None => {
            crate::cli::session::session_runtime::capture_admitted_auth_owner(
                input.api,
                input.profile,
                input.token,
            )
            .await
        }
    };
    if let Some(server_turn) = input.admitted_server_turn
        && (!server_turn.attachment_owner.is_current()
            || server_turn
                .auth_owner
                .as_ref()
                .is_some_and(|owner| !owner.is_current())
            || state
                .server_resume_attachment
                .as_ref()
                .is_none_or(|(cursor, epoch)| {
                    cursor.session_id != input.session_id
                        || cursor.owner_id != crate::cli::cli_config::cli_utils::cli_user_id()
                        || *epoch != state.session_attachment_epoch
                }))
    {
        return TurnAttempt::NotStarted(Box::new(crate::TurnFailure {
            error:
                "Server resume authentication changed before execution; restore the session again"
                    .into(),
            partial: crate::PartialTurnData {
                session_id: Some(input.session_id.to_string()),
                ..Default::default()
            },
        }));
    }
    let prepared = prepare_turn_stream_state(state).await;
    *astra_core::sync_poison::recover_mutex_lock(&state.active_turn_local_run_control) =
        Some(prepared.run_control.clone());
    let mut params = build_turn_stream_params(state, input, &prepared);
    params.auth_owner = auth_owner;
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
        match result {
            Err(failure) if failure.partial.server_admission_not_dispatched => {
                TurnAttempt::NotStarted(Box::new(failure))
            }
            result => TurnAttempt::Completed(Box::new(result)),
        }
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
    if state.active_conversation.is_none()
        && let Some(session_id) = state
            .session_id
            .as_deref()
            .or(Some(input.session_id))
            .map(str::to_owned)
    {
        match crate::cli::session::session_continuation::recover_or_initialize_active_conversation(
            &session_id,
        ) {
            Ok(active) => state.active_conversation = Some(active),
            Err(error) => tracing::warn!(
                session_id = %session_id,
                error = %error,
                "canonical conversation could not be recovered at the live turn boundary"
            ),
        }
    }
    ChatTurnParams {
        api: input
            .admitted_server_turn
            .map_or(input.api, |turn| &turn.api),
        token: input.token,
        auth_owner: None,
        message: input.message,
        user_intent: input.user_intent,
        input_runtime_required_texts: input.input_runtime_required_texts,
        input_runtime_volatile_texts: input.input_runtime_volatile_texts,
        input_work_unit_observations: &prepared.input_work_unit_observations,
        semantic_query_override: input.semantic_query_override,
        session_id: Some(input.session_id),
        explain_analyze_terminal_degraded: input.explain_analyze_terminal_degraded,
        offering_id: state
            .model
            .as_ref()
            .and_then(|model| model.offering_id())
            .map(str::to_string),
        model: astra_core::model_override::normalize_model_override(state.model.as_deref()),
        provider: None,
        explain: state.explain,
        runtime_config: std::sync::Arc::new(state.runtime_config.clone()),
        render_md: true,
        history: &state.history,
        perm_manager: &mut state.perm_manager,
        verbose_mode: state.verbose_mode,
        render_policy: state
            .tui_render_policy
            .unwrap_or(crate::cli::stream::stream_render::RenderPolicy::Stream),
        cli_context: Some(&state.cli_context),
        recent_tools: &state.recent_tools,
        deferred_tool_activations: Some(&mut state.deferred_tool_activations),
        tool_health_entries: &state.tool_health_entries,
        resume_restricted_tools: &state.resume_restricted_tools,

        latest_turn_quality_feedback: state.latest_turn_quality_feedback.as_ref(),
        unified_skill_registry: &state.unified_skill_registry,
        is_plan_subtask: false,
        plan_subtask_id: None,
        cancel_token: Some(prepared.cancel_token.clone()),
        execution_time_budget: None,
        run_control: Some(prepared.run_control.clone()),
        incremental_state: Some(prepared.incremental_state.clone()),
        executor_observer: None,
        request_session_execution_lease: None,
        plan_assemble_line_release: None,
        stream_event_tx: state.tui_stream_event_tx.clone(),
        stream_json_emitter: None,
        agent_live_event_sink: state.tui_agent_live_event_sink.clone(),
        approval_request_tx: state.tui_approval_request_tx.clone(),
        ask_user_request_tx: state.tui_ask_user_request_tx.clone(),
        plan_review_request_tx: state.tui_plan_review_request_tx.clone(),
        mcp_manager: Some(state.mcp_manager.clone()),

        agent_spawner: state.agent_spawner.clone(),
        root_agent_id: Some("main"),
        observability_hub: prepared.observability_hub.clone(),
        observability_session: prepared.observability_session.clone(),
        file_journal: Some(state.file_journal.clone()),
        file_state: Some(state.file_state.clone()),
        database_snapshot_journal: Some(state.database_snapshot_journal.clone()),

        git_worktree_journal: Some(state.git_worktree_journal.clone()),
        session_state_journal: Some(state.session_state_journal.clone()),
        bg_task_commands: Some(state.bg_task_commands.clone()),
        bg_task_list_cache: Some(state.bg_task_list_cache.clone()),
        bash_detach_slot: Some(state.bash_detach_slot.clone()),
        // This clock scopes every local tool mutation, checkpoint and journal
        // event. Server admission allocates its own turn independently.
        turn_index: state.turn.saturating_add(1),
        pipeline_state: state.runtime_pipeline_state.clone(),
        compaction_state: state.runtime_compaction_state.clone(),
        consecutive_context_window_errors: state.runtime_consecutive_context_window_errors,
        workspace_observation_quarantine: state.workspace_observation_quarantine.clone(),
        // Headless read observations share a lifecycle with the turn-local
        // workspace epoch. They must not be imported from session state.
        idempotency_cache: None,
        pre_loaded_messages: input
            .admitted_server_turn
            .map(|turn| turn.continuation.messages.clone())
            .or_else(|| {
                state
                    .active_conversation
                    .as_ref()
                    .map(|conversation| conversation.materialize())
            }),
        append_system_prompt: prepared.append_system_prompt.clone(),
        #[cfg(feature = "harness")]
        harness_sink: Some(state.harness_sink.clone()),

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
    let auth_owner = params.auth_owner.clone();
    let stream_fut = stream_chat_sse(params);
    tokio::pin!(stream_fut);

    let drain_timeout = crate::cli::stream::streaming_types::INTERACTIVE_CANCEL_TIMEOUT;
    tokio::select! {
        biased;
        mut result = &mut stream_fut => {
            if let Err(failure) = &mut result
                && failure.partial.remote_cancel_required
            {
                apply_stream_failure_local_cancellation(
                    failure,
                    &prepared.run_control,
                    &cancel_token_for_signal,
                );
                if failure.partial.callback_client_detached {
                    note_interactive_callback_detach(failure);
                }
                let fallback_run_id = incremental_state.snapshot().run_id;
                report_server_run_detach_after_internal_failure(
                    failure
                        .partial
                        .remote_cancel_run_id
                        .as_deref()
                        .or(failure.partial.run_id.as_deref()),
                    fallback_run_id.as_deref(),
                );
            }
            (result, false)
        },
        _ = tokio::signal::ctrl_c() => {
            prepared.run_control.request_cancel_for_user();
            cancel_token_for_signal.cancel();
            if tui_cancel_token.is_none() {
                eprintln!("\n{}", format!(
                    "  Interrupted. (press Ctrl+C again to force-exit, or wait up to {}s for graceful drain)",
                    drain_timeout.as_secs(),
                ).dim());
            }
            let drained = settle_interactive_user_cancel(
                &mut stream_fut,
                api, bearer, auth_owner.as_ref(), &incremental_state,
                "Ctrl+C", drain_timeout,
            ).await;
            (drained, true)
        }
        _ = async {
            match tui_cancel_token.as_ref() {
                Some(token) => token.cancelled().await,
                None => std::future::pending().await,
            }
        } => {
            prepared.run_control.request_cancel_for_user();
            cancel_token_for_signal.cancel();
            let drained = settle_interactive_user_cancel(
                &mut stream_fut,
                api, bearer, auth_owner.as_ref(), &incremental_state,
                "TUI cancel", drain_timeout,
            ).await;
            (drained, true)
        }
    }
}

fn apply_stream_failure_local_cancellation(
    failure: &crate::TurnFailure,
    run_control: &LocalRunControl,
    cancel_token: &tokio_util::sync::CancellationToken,
) {
    if !failure.is_clean_internal_stream_detach() {
        run_control.request_cancel_for_runtime();
        cancel_token.cancel();
    }
}

pub(crate) fn note_interactive_callback_detach(failure: &mut crate::TurnFailure) {
    if !failure.partial.callback_client_detached {
        return;
    }
    let recovery = crate::cli::stream::streaming_types::interactive_detach_recovery(
        failure.partial.session_id.as_deref(),
    );
    if !failure.error.is_empty() {
        failure.error.push(' ');
    }
    failure.error.push_str(&recovery);
}

pub(crate) fn report_server_run_detach_after_internal_failure(
    failure_run_id: Option<&str>,
    fallback_run_id: Option<&str>,
) {
    let run_id = failure_run_id
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .or_else(|| {
            fallback_run_id
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
        });
    let Some(run_id) = run_id else {
        tracing::error!(
            target: "astra::cli::cancel",
            source = "internal_stream_failure",
            "durable run cancellation required but no run_id was captured"
        );
        return;
    };
    // DELETE /chat/runs/{run_id} is the public, run-level User cancellation
    // command. A parser, callback, or response-transport failure is not user
    // authority and must never be rewritten as that durable fact. Dropping
    // this response detaches the client; the server-owned execution and its
    // durable recovery path remain authoritative until an attached-stream
    // capability can express exact-generation Runtime cancellation.
    tracing::warn!(
        target: "astra::cli::stream",
        run_id = %run_id,
        source = "internal_stream_failure",
        "client detached from an unsettled durable run; server recovery retains lifecycle authority"
    );
}

/// Retain the same physical response while explicit user cancellation settles.
/// Admission can precede observation of its run header. The SSE owner drains
/// that identity without executing tools; never discover a target by Session.
/// Keep settlement state on the heap: its branches otherwise enlarge the
/// ordinary turn's poll stack even when the user never cancels.
pub(crate) fn settle_interactive_user_cancel<F>(
    stream_fut: &mut std::pin::Pin<&mut F>,
    api: &astra_thin_client::ThinClient,
    bearer: &str,
    auth_owner: Option<&crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot>,
    incremental_state: &Arc<astra_turn_core::turn_event_sink::IncrementalTurnState>,
    source: &'static str,
    timeout: std::time::Duration,
) -> impl std::future::Future<Output = Result<StreamResult, crate::TurnFailure>>
where
    F: std::future::Future<Output = Result<StreamResult, crate::TurnFailure>>,
{
    Box::pin(settle_interactive_user_cancel_inner(
        stream_fut,
        api,
        bearer,
        auth_owner,
        incremental_state,
        source,
        timeout,
    ))
}

async fn settle_interactive_user_cancel_inner<F>(
    stream_fut: &mut std::pin::Pin<&mut F>,
    api: &astra_thin_client::ThinClient,
    bearer: &str,
    auth_owner: Option<&crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot>,
    incremental_state: &Arc<astra_turn_core::turn_event_sink::IncrementalTurnState>,
    source: &'static str,
    timeout: std::time::Duration,
) -> Result<StreamResult, crate::TurnFailure>
where
    F: std::future::Future<Output = Result<StreamResult, crate::TurnFailure>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    let settle = async {
        if let Some(run_id) = incremental_state
            .snapshot()
            .run_id
            .filter(|id| !id.trim().is_empty())
        {
            // An in-flight local callback may need Server cancellation to
            // unblock. Do not spend the entire budget draining it first.
            return tokio::join!(
                drain_after_cancel(stream_fut, timeout, source, incremental_state.clone()),
                super::turn_facade::cancel_exact_run_until_settled_with_deadline(
                    api, bearer, &run_id, auth_owner, deadline,
                ),
            );
        }
        let result =
            drain_after_cancel(stream_fut, timeout, source, incremental_state.clone()).await;
        let run_id = incremental_state
            .snapshot()
            .run_id
            .filter(|id| !id.trim().is_empty());
        let settled = match run_id {
            Some(run_id) => {
                super::turn_facade::cancel_exact_run_until_settled_with_deadline(
                    api, bearer, &run_id, auth_owner, deadline,
                )
                .await
            }
            None if result.as_ref().is_err_and(|failure| {
                failure.partial.server_admission_not_dispatched
                    || failure.partial.admission_rejected
            }) =>
            {
                Ok(())
            }
            None => Err(
                "Server admission identity was not observed before cancellation completed".into(),
            ),
        };
        (result, settled)
    };
    let (result, settled) = tokio::select! {
        biased;
        _ = tokio::signal::ctrl_c() => {
            eprintln!("{}", "  Force-exiting (partial data recovered).".dim());
            (
                super::turn_cancellation::fabricate_user_cancel_failure(
                    "user_interrupted (force-exit)", incremental_state.clone(),
                ),
                Err("forced exit before cancellation was confirmed".to_string()),
            )
        },
        pair = settle => pair,
    };
    let snapshot = incremental_state.snapshot();
    match settled {
        Ok(()) => result.map_err(|mut failure| {
            failure.partial.run_id = failure.partial.run_id.or(snapshot.run_id);
            failure.partial.remote_cancel_required = false;
            failure.partial.remote_cancel_run_id = None;
            failure
        }),
        Err(error) => {
            let error = astra_logging::redact_known_secret_patterns(&error);
            tracing::warn!(
                target: "astra::cli::cancel",
                source, run_id = ?snapshot.run_id, error = %error,
                "explicit user cancellation remains unconfirmed"
            );
            let mut failure = match result {
                Err(failure) => failure,
                Ok(_) => super::turn_cancellation::fabricate_user_cancel_failure(
                    "user_interrupted (remote cancellation unconfirmed)",
                    incremental_state.clone(),
                )
                .expect_err("cancellation failure retains the incremental prefix"),
            };
            failure.partial.run_id = failure.partial.run_id.or(snapshot.run_id);
            failure
                .partial
                .remote_cancel_run_id
                .clone_from(&failure.partial.run_id);
            failure.partial.remote_cancel_required = true;
            Err(failure)
        }
    }
}

#[cfg(test)]
pub(crate) async fn admitted_interactive_turn_inputs_for_test(
    state: &mut SessionState,
    api: &astra_thin_client::ThinClient,
    settle: bool,
) -> Result<(u32, Vec<serde_json::Value>, u32), String> {
    admitted_interactive_turn_inputs_with_token_for_test(
        state,
        api,
        settle,
        "admitted-token",
        false,
    )
    .await
}

#[cfg(test)]
pub(crate) async fn admitted_interactive_turn_inputs_with_token_for_test(
    state: &mut SessionState,
    api: &astra_thin_client::ThinClient,
    settle: bool,
    input_token: &str,
    fail_primary_append: bool,
) -> Result<(u32, Vec<serde_json::Value>, u32), String> {
    let session_id = state.session_id.clone().expect("restored session");
    let admission =
        super::turn_entry::acquire_interactive_turn_admission(state, api, None, input_token)
            .await?;
    assert!(
        matches!(
            astra_services::session_journal::SessionExecutionLease::try_acquire(&session_id),
            Err(astra_services::session_journal::SessionExecutionLeaseError::Conflict { .. }),
        ),
        "the admitted local snapshot remains protected through settlement"
    );
    let token = admission
        .as_ref()
        .and_then(|admission| admission.server_turn.as_ref())
        .map_or(input_token, |turn| turn.bearer.as_str());
    let prepared = prepare_turn_stream_state(state).await;
    let params = build_turn_stream_params(
        state,
        TurnExecutionInput {
            api,
            profile: None,
            token,
            message: "continue again",
            user_intent: "continue again",
            input_runtime_required_texts: &[],
            input_runtime_volatile_texts: &[],
            session_id: &session_id,
            admitted_server_turn: admission
                .as_ref()
                .and_then(|admission| admission.server_turn.as_ref()),
            semantic_query_override: None,
            explain_analyze_terminal_degraded: None,
        },
        &prepared,
    );
    if let Some(turn) = admission
        .as_ref()
        .and_then(|admission| admission.server_turn.as_ref())
    {
        assert_eq!(
            params.token, turn.bearer,
            "resume and runtime use the same resolved bearer"
        );
        if crate::cli::session::session_runtime::uses_environment_access_token(params.token) {
            assert!(
                turn.auth_owner.is_none(),
                "environment bearers confer no profile refresh authority"
            );
        }
    }
    if settle {
        params
            .api
            .post_bearer_path_empty_json::<serde_json::Value>(
                params.token,
                "/admission-token-probe",
            )
            .await
            .map_err(|error| error.to_string())?;
    }
    let turn_index = params.turn_index;
    let messages = params
        .pre_loaded_messages
        .expect("canonical prompt projection");
    if settle {
        // Use the real primary commit and sidecars, with deterministic runtime
        // output, after the same local turn allocation as successful settlement.
        let previous = state.active_conversation.as_ref().unwrap().cursor().clone();
        let mut result = crate::tests::stub_stream_result("continued");
        result.final_messages = messages.clone();
        result.final_messages.extend([
            serde_json::json!({"role":"user", "content":"continue again"}),
            serde_json::json!({"role":"assistant", "content":"continued"}),
        ]);
        result.session_id = Some(session_id.clone());
        let trace_id = format!("admitted-local-turn-{}", state.turn.saturating_add(1));
        let trace = astra_turn_core::context_assembly_trace::ContextAssemblyTrace {
            turn_id: trace_id.clone(),
            session_id: session_id.clone(),
            ..Default::default()
        };
        result.pending_context_assembly_trace = Some((turn_index, trace.to_json_value()));
        let expected_messages = result.final_messages.clone();
        let previous_turn = state.turn;
        let failed_append_paths = if fail_primary_append {
            let journal_path = state.journal.as_ref().unwrap().path().to_path_buf();
            let backup = journal_path.with_extension("before-failed-append");
            std::fs::rename(&journal_path, &backup).unwrap();
            std::fs::create_dir(&journal_path).unwrap();
            Some((journal_path, backup))
        } else {
            None
        };
        super::turn_success::apply_turn_success(
            state,
            None,
            "continue again",
            result,
            std::time::Instant::now(),
        );
        if let Some((journal_path, backup)) = failed_append_paths {
            std::fs::remove_dir(&journal_path).unwrap();
            std::fs::rename(&backup, &journal_path).unwrap();
            assert!(
                state
                    .session_persistence_error
                    .as_ref()
                    .is_some_and(|error| error.contains("append turn event"))
            );
            assert_eq!(
                state.turn, previous_turn,
                "failed primary settlement rolls back local allocation"
            );
            assert_eq!(
                state.active_conversation.as_ref().unwrap().cursor(),
                &previous
            );
            return Ok((turn_index, messages, state.turn));
        }
        assert!(
            state.session_persistence_error.is_none(),
            "{:?}",
            state.session_persistence_error
        );
        let events = astra_services::session_journal::JournalWriter::new(&session_id)
            .unwrap()
            .complete_append_order_snapshot()
            .unwrap();
        let commit = events
            .iter()
            .rev()
            .find_map(|event| event.conversation_commit.as_ref())
            .unwrap();
        assert_eq!(commit.base_root_hash, previous.canonical_root_hash);
        assert_eq!(commit.cursor.completed_turn, state.turn);
        assert_eq!(
            commit.cursor.conversation_seq,
            previous.conversation_seq + 1
        );
        let replay = crate::cli::session::session_continuation::recover_or_initialize_active_conversation_from_append_order_events(
            &session_id, &events,
        ).unwrap();
        assert_eq!(replay.messages(), expected_messages);
        let workspace = astra_services::session_workspace::read_workspace(&session_id).unwrap();
        assert_eq!(workspace.turn_count, state.turn);
        assert_eq!(
            workspace
                .last_context_trace
                .as_ref()
                .map(|trace| trace.turn_id.as_str()),
            Some(trace_id.as_str()),
            "a Server restore must not make a current local sidecar look stale",
        );
    }
    Ok((turn_index, messages, state.turn))
}

#[cfg(test)]
pub(crate) async fn probe_admitted_retry_request_for_test(
    request: TurnExecutionRequest<'_>,
) -> (String, u32, Vec<serde_json::Value>) {
    assert!(request.input.admitted_server_turn.is_some());
    let prepared = prepare_turn_stream_state(request.state).await;
    let params = build_turn_stream_params(request.state, request.input, &prepared);
    params
        .api
        .post_bearer_path_empty_json::<serde_json::Value>(params.token, "/admission-token-probe")
        .await
        .unwrap();
    (
        params.token.to_string(),
        params.turn_index,
        params.pre_loaded_messages.unwrap(),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        PreparedTurnStreamState, TurnExecutionInput, apply_stream_failure_local_cancellation,
        build_turn_stream_params, note_interactive_callback_detach, prepare_turn_stream_state,
        report_server_run_detach_after_internal_failure,
    };
    use crate::cli::session::session_state::SessionState;
    use crate::cli::turn::local_run_control::LocalRunControl;
    use std::sync::Arc;

    #[tokio::test]
    async fn interactive_cancel_future_is_heap_pinned() {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap();
        let incremental =
            Arc::new(astra_turn_core::turn_event_sink::IncrementalTurnState::default());
        let mut stream = std::pin::pin!(std::future::pending::<
            Result<super::StreamResult, crate::TurnFailure>,
        >());
        let settlement = super::settle_interactive_user_cancel(
            &mut stream,
            &api,
            "synthetic-token",
            None,
            &incremental,
            "test",
            std::time::Duration::from_secs(10),
        );

        assert_eq!(
            std::mem::size_of_val(&settlement),
            std::mem::size_of::<usize>(),
            "cancellation state must not enlarge the ordinary turn's embedded future"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn interactive_cancel_without_admission_identity_remains_unconfirmed() {
        let server = wiremock::MockServer::start().await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let incremental =
            Arc::new(astra_turn_core::turn_event_sink::IncrementalTurnState::default());
        let mut stream = std::pin::pin!(async {
            super::super::turn_cancellation::fabricate_user_cancel_failure(
                "cancelled",
                incremental.clone(),
            )
        });
        let started = tokio::time::Instant::now();
        let result = super::settle_interactive_user_cancel(
            &mut stream,
            &api,
            "test-token",
            None,
            &incremental,
            "test",
            std::time::Duration::from_millis(20),
        )
        .await
        .unwrap_err();

        assert!(result.partial.remote_cancel_required);
        assert!(result.partial.run_id.is_none());
        assert_eq!(started.elapsed(), std::time::Duration::ZERO);
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "never select an unrelated run by session"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn interactive_cancel_before_dispatch_or_after_rejection_is_immediate() {
        // Both are authoritative no-admission outcomes; neither permits a
        // Session lookup, cancel, or warning about somebody else's run.
        for not_dispatched in [true, false] {
            let server = wiremock::MockServer::start().await;
            let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
            let incremental =
                Arc::new(astra_turn_core::turn_event_sink::IncrementalTurnState::default());
            let mut stream = std::pin::pin!(async {
                Err(crate::TurnFailure {
                    error: "synthetic cancellation or admission rejection".into(),
                    partial: crate::PartialTurnData {
                        server_admission_not_dispatched: not_dispatched,
                        admission_rejected: !not_dispatched,
                        ..Default::default()
                    },
                })
            });
            let started = tokio::time::Instant::now();
            let failure = super::settle_interactive_user_cancel(
                &mut stream,
                &api,
                "synthetic-token",
                None,
                &incremental,
                "test",
                std::time::Duration::from_secs(10),
            )
            .await
            .unwrap_err();
            assert!(!failure.partial.remote_cancel_required);
            assert!(failure.partial.remote_cancel_run_id.is_none());
            assert_eq!(started.elapsed(), std::time::Duration::ZERO);
            assert!(server.received_requests().await.unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn interactive_cancel_failure_is_not_treated_as_settled() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/chat/runs/run-owned"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let incremental =
            Arc::new(astra_turn_core::turn_event_sink::IncrementalTurnState::default());
        incremental.set_run_id("run-owned".into());
        let mut stream = std::pin::pin!(async {
            super::super::turn_cancellation::fabricate_user_cancel_failure(
                "cancelled",
                incremental.clone(),
            )
        });
        let failure = super::settle_interactive_user_cancel(
            &mut stream,
            &api,
            "test-token",
            None,
            &incremental,
            "test",
            std::time::Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(failure.partial.remote_cancel_required);
        assert_eq!(
            failure.partial.remote_cancel_run_id.as_deref(),
            Some("run-owned")
        );
        assert!(!failure.is_clean_local_cancel());
    }

    #[tokio::test]
    async fn interactive_cancel_known_run_does_not_wait_for_a_blocked_local_callback() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let acknowledgement = Arc::new(tokio::sync::Notify::new());
        Mock::given(method("DELETE"))
            .and(path("/chat/runs/run-owned"))
            .respond_with({
                let acknowledgement = acknowledgement.clone();
                move |_: &wiremock::Request| {
                    acknowledgement.notify_one();
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"execution_settled": true}))
                }
            })
            .expect(1)
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let incremental =
            Arc::new(astra_turn_core::turn_event_sink::IncrementalTurnState::default());
        incremental.set_run_id("run-owned".into());
        let mut stream = std::pin::pin!(async {
            acknowledgement.notified().await;
            super::super::turn_cancellation::fabricate_user_cancel_failure(
                "cancelled",
                incremental.clone(),
            )
        });
        let failure = super::settle_interactive_user_cancel(
            &mut stream,
            &api,
            "synthetic-token",
            None,
            &incremental,
            "test",
            std::time::Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(!failure.partial.remote_cancel_required);
    }

    #[tokio::test]
    async fn interactive_cancel_pending_ack_does_not_outlive_the_settlement_deadline() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/chat/runs/run-owned"))
            .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({
                "execution_settled": false
            })))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let incremental =
            Arc::new(astra_turn_core::turn_event_sink::IncrementalTurnState::default());
        incremental.set_run_id("run-owned".into());
        let mut stream = std::pin::pin!(async {
            super::super::turn_cancellation::fabricate_user_cancel_failure(
                "cancelled",
                incremental.clone(),
            )
        });
        let failure = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            super::settle_interactive_user_cancel(
                &mut stream,
                &api,
                "test-token",
                None,
                &incremental,
                "test",
                std::time::Duration::from_millis(40),
            ),
        )
        .await
        .expect("bounded pending acknowledgement")
        .unwrap_err();
        assert!(failure.partial.remote_cancel_required);
    }

    #[tokio::test]
    async fn clean_stream_detach_does_not_cancel_shared_local_work() {
        use astra_runtime::turn::run_control::{RunControlStatus, RunStatusProvider};

        let control = LocalRunControl::default();
        let token = tokio_util::sync::CancellationToken::new();
        let mut failure = crate::TurnFailure {
            error: "stream closed".into(),
            partial: crate::PartialTurnData {
                remote_cancel_required: true,
                interruption: Some(serde_json::json!({"kind": "stream_transport"})),
                ..Default::default()
            },
        };
        apply_stream_failure_local_cancellation(&failure, &control, &token);
        assert!(!token.is_cancelled());
        assert_eq!(control.control_status("user", "run").await.unwrap(), None);

        failure.partial.callback_client_detached = true;
        apply_stream_failure_local_cancellation(&failure, &control, &token);
        assert!(token.is_cancelled());
        assert_eq!(
            control.control_status("user", "run").await.unwrap(),
            Some(RunControlStatus::Cancelled)
        );
    }

    #[test]
    fn interactive_callback_detach_tells_the_user_the_server_run_remains() {
        let mut failure = crate::TurnFailure {
            error: crate::cli::stream::stream_render::edge_callback_detach_message(
                "approval",
                "after bounded transport retries",
                &"HTTP error: error sending request",
            ),
            partial: crate::PartialTurnData {
                session_id: Some("sess-active".into()),
                callback_client_detached: true,
                remote_cancel_required: true,
                ..Default::default()
            },
        };

        note_interactive_callback_detach(&mut failure);

        assert!(failure.error.contains("This client detached"));
        assert!(
            failure
                .error
                .contains("not a cancellation of the server run")
        );
        let cancel_at = failure
            .error
            .find("astra session cancel sess-active")
            .expect("cancel command");
        let resume_at = failure
            .error
            .find("astra --resume sess-active")
            .expect("resume after session cancel");
        assert!(cancel_at < resume_at);
        assert!(failure.error.contains("The server run was not cancelled."));
        assert!(!failure.error.contains("cancellation settled"));
        assert!(!failure.error.contains("will be cancelled"));
    }

    #[test]
    fn internal_callback_failure_does_not_fabricate_user_cancellation() {
        report_server_run_detach_after_internal_failure(
            Some("run-callback-failed"),
            Some("run-consuming-stream"),
        );
    }

    #[test]
    fn internal_callback_failure_can_report_legacy_stream_identity_without_mutation() {
        report_server_run_detach_after_internal_failure(None, Some("run-legacy-stream"));
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
    #[serial_test::serial]
    async fn failed_first_turn_recovers_explicit_empty_canonical_state_not_display_pairs() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("failed-first-turn-{}", uuid::Uuid::new_v4());
        let writer = astra_services::session_journal::JournalWriter::new(&session_id).unwrap();
        writer
            .append(&astra_services::session_journal::JournalEvent::turn_error(
                Some(&session_id),
                1,
                Some("mock-model"),
                "launch work",
                "foreground ownership moved to the background",
                0,
            ))
            .unwrap();
        let mut state = SessionState {
            session_id: Some(session_id.clone()),
            journal: Some(writer),
            history: vec![(
                "launch work".into(),
                "display-only interrupted result".into(),
            )],
            ..SessionState::default()
        };
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let prepared = prepare_turn_stream_state(&state).await;

        let params = build_turn_stream_params(
            &mut state,
            TurnExecutionInput {
                api: &api,
                profile: None,
                token: "token",
                message: "what work is running?",
                user_intent: "what work is running?",
                input_runtime_required_texts: &[],
                input_runtime_volatile_texts: &[],
                session_id: &session_id,
                admitted_server_turn: None,
                semantic_query_override: None,
                explain_analyze_terminal_degraded: None,
            },
            &prepared,
        );

        assert_eq!(params.pre_loaded_messages.as_deref(), Some([].as_slice()));
        assert!(
            state
                .active_conversation
                .as_ref()
                .is_some_and(|active| active.messages().is_empty())
        );
    }

    #[tokio::test]
    async fn prepare_turn_stream_state_captures_canonical_active_fanout_truth() {
        use astra_core::work_unit::{
            WorkUnitObservation, WorkUnitObservationMode, WorkUnitStatus, WorkUnitWakePolicy,
        };
        let active_work_registry = Arc::new(astra_core::work_unit::ActiveWorkRegistry::default());
        active_work_registry.observe(
            &WorkUnitObservation::new(
                "review-group",
                "agent_fanout",
                WorkUnitStatus::Pending,
                1,
                WorkUnitObservationMode::Transition,
            )
            .unwrap()
            .with_wake_policy(WorkUnitWakePolicy::OnAttentionOrTerminal),
        );
        let state = SessionState {
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
    fn build_turn_stream_params_respects_render_policy_without_legacy_plan_subtask() {
        let canonical_messages = vec![
            serde_json::json!({"role": "user", "content": "canonical question"}),
            serde_json::json!({"role": "assistant", "content": "canonical answer"}),
        ];
        let mut state = SessionState {
            tui_render_policy: Some(crate::cli::stream::stream_render::RenderPolicy::Silent),
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
            history: vec![("display-only question".into(), "display-only answer".into())],
            active_conversation: Some(
                astra_turn_core::active_conversation::ActiveConversation::from_projection(
                    astra_services::local_owner_scope().id(),
                    "sess-1",
                    canonical_messages.clone(),
                    1,
                    astra_turn_core::active_conversation::ActiveConversationSource::Journal,
                )
                .unwrap(),
            ),
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
                session_id: "sess-1",
                admitted_server_turn: None,
                semantic_query_override: None,
                explain_analyze_terminal_degraded: None,
            },
            &prepared,
        );

        assert_eq!(
            params.render_policy,
            crate::cli::stream::stream_render::RenderPolicy::Silent
        );
        assert!(!params.is_plan_subtask);
        assert_eq!(params.plan_subtask_id, None);
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
        assert_eq!(
            params.pre_loaded_messages,
            Some(canonical_messages),
            "the model boundary must consume canonical typed history, never the display pairs"
        );
    }
}
