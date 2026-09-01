//! Successful turn state application and persistence preparation.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use astra_turn_core::conversation_log::manager::CslManager;

use super::turn_commit::{PrimaryTurnCommit, TurnCommitOutcome, commit_primary_turn};
use super::turn_learning::{analyze_chat_turn_learning, turn_quality_feedback_from_eval};
use super::turn_post_commit::{
    TurnPostCommitJob, account_turn_post_commit_queue, apply_turn_post_commit_completion,
    attach_deferred_sidecars, execute_turn_post_commit_job, extract_csl_fields_from_result,
    prepare_turn_post_commit_job,
};
use super::turn_reporting::{build_history_text, print_turn_status_line};
use crate::cli::cli_config::cli_utils::persist_profile_last_session_or_warn;
use crate::cli::session::session_projection::build_continuation_anchor;
use crate::cli::session::session_startup;
use crate::cli::session::session_state::{ContinuationAnchor, SessionState};
use crate::cli::stream::streaming_types::StreamResult;
use astra_services::session_journal;
use crossterm::style::Stylize;

/// Test-only synchronous settlement executes the derived projection explicitly
/// after the primary event. Production TUI paths hand this work to their
/// managed post-commit queue instead.
#[cfg(test)]
pub(crate) fn apply_turn_success(
    state: &mut SessionState,
    profile: Option<&str>,
    line: &str,
    result: StreamResult,
    turn_start: Instant,
) {
    let primary = apply_turn_success_primary_sync(state, profile, line, result, turn_start);
    if let Some(sidecars) = primary.deferred_sidecars {
        if let Err(error) = sidecars.execute(false) {
            state.session_persistence_error = Some(error.to_string());
        }
    }
}

// Settlement owns several independent commit sinks; keeping them explicit
// makes partial-failure handling auditable at this transaction boundary.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply_turn_success_async(
    state: &mut SessionState,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    line: &str,
    mut result: StreamResult,
    turn_start: Instant,
    ui: &mut dyn crate::cli::ui_adapter::ReplUiAdapter,
    post_commit_tx: Option<&tokio::sync::mpsc::Sender<TurnPostCommitJob>>,
) {
    // A dedicated durable control-tail observer may receive Applied after the
    // primary live SSE fanout closed. Merge that exact fact before snapshotting
    // or committing the turn so local history matches the server transcript.
    let active_run_control =
        astra_core::sync_poison::recover_mutex_lock(&state.active_turn_local_run_control)
            .as_ref()
            .cloned();
    if let Some(run_control) = active_run_control {
        if !run_control
            .wait_for_remote_user_intent_dispositions(std::time::Duration::from_secs(5))
            .await
        {
            ui.show_warning(
                "Turn completed, but durable guidance disposition reconciliation timed out; canonical server history remains authoritative.",
            );
        }
        for intent in run_control.take_remotely_applied_user_intents() {
            if !result
                .applied_user_intents
                .iter()
                .any(|existing| existing.intent_id == intent.intent_id)
            {
                result.applied_user_intents.push(intent);
            }
        }
    }
    // Primary settlement projects the continuation anchor from typed message
    // semantics. Keep the messages on `result` until that synchronous state
    // transition completes; the deferred CSL worker receives its own owned
    // snapshot.
    let final_messages = crate::cli::history_work::clone_json_history(
        astra_core::history_work::HistoryWorkSite::CliPostCommitSnapshot,
        &result.final_messages,
    );
    let csl_checkpoint_fields = extract_csl_fields_from_result(&result);
    let primary_commit_started = Instant::now();
    let primary_commit =
        apply_turn_success_on_blocking_worker(state, profile, line, result, turn_start).await;
    let commit_outcome = &primary_commit.outcome;
    tracing::debug!(
        target: "astra_cli::turn_settlement",
        primary_commit_ms = primary_commit_started.elapsed().as_millis() as u64,
        turn_persisted = commit_outcome.turn_persisted,
        "completed durable primary turn commit"
    );
    if commit_outcome.turn_persisted {
        let post_commit_started = Instant::now();
        let mut post_commit = prepare_turn_post_commit_job(
            state,
            api,
            profile,
            final_messages,
            csl_checkpoint_fields,
            turn_start,
        );
        attach_deferred_sidecars(&mut post_commit, primary_commit.deferred_sidecars);
        if let Some(tx) = post_commit_tx {
            account_turn_post_commit_queue(&mut post_commit);
            match tx.try_send(post_commit) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(post_commit)) => {
                    // Bounded queue pressure is observable but never a drop:
                    // preserving sidecar order is more important than shaving
                    // the rare saturated-turn delay.
                    if tx.send(post_commit).await.is_err() {
                        ui.show_warning(
                            "Turn is durable, but deferred local projections could not be queued.",
                        );
                    }
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(post_commit)) => {
                    let completion = execute_turn_post_commit_job(post_commit).await;
                    for error in apply_turn_post_commit_completion(completion, state) {
                        ui.show_warning(&format!("Deferred turn projection failed: {error}"));
                    }
                }
            }
        } else {
            let completion = execute_turn_post_commit_job(post_commit).await;
            for error in apply_turn_post_commit_completion(completion, state) {
                ui.show_warning(&format!("Deferred turn projection failed: {error}"));
            }
        }
        tracing::debug!(
            target: "astra_cli::turn_settlement",
            post_commit_ms = post_commit_started.elapsed().as_millis() as u64,
            "completed non-primary turn post-commit tasks"
        );
    }
}

/// The durable primary-turn commit performs just the canonical journal fsync.
/// Workspace/checkpoint/transcript projections are fully owned post-commit
/// jobs, so they cannot extend the active turn's lifetime.
///
/// Move the whole mutable session through one blocking worker rather than
/// splitting a transaction across competing owners. The slot restores the
/// state even if the worker panics, so a worker failure surfaces as degraded
/// persistence instead of silently discarding the in-memory session.
async fn apply_turn_success_on_blocking_worker(
    state: &mut SessionState,
    profile: Option<&str>,
    line: &str,
    result: StreamResult,
    turn_start: Instant,
) -> PrimaryTurnCommit {
    let history_queue_reservation = crate::cli::history_work::reserve_pair_history_queue(
        astra_core::history_work::HistoryWorkSite::CliPrimaryCommitWorkerHistoryQueue,
        &state.history,
    );
    let final_messages_queue_reservation = crate::cli::history_work::reserve_json_history_queue(
        astra_core::history_work::HistoryWorkSite::CliPrimaryCommitWorkerFinalMessagesQueue,
        &result.final_messages,
    );
    let state_slot = Arc::new(Mutex::new(Some(std::mem::take(state))));
    let worker_slot = Arc::clone(&state_slot);
    let profile = profile.map(str::to_owned);
    let line = line.to_owned();
    let journal_dir_override = astra_services::session_journal::current_journal_dir_override();

    let worker = tokio::task::spawn_blocking(move || {
        drop(history_queue_reservation);
        drop(final_messages_queue_reservation);
        let _journal_dir_guard = journal_dir_override
            .as_deref()
            .map(astra_services::session_journal::JournalDirGuard::new);
        let mut worker_state = worker_slot
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .expect("turn state is installed before durable commit starts");
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            apply_turn_success_primary_sync(
                &mut worker_state,
                profile.as_deref(),
                &line,
                result,
                turn_start,
            )
        }));
        *worker_slot
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(worker_state);
        outcome
    })
    .await;

    let restored = state_slot
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
        .expect("durable commit worker restores the session state");
    *state = restored;

    match worker {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(_)) => failed_primary_turn_commit("turn commit worker panicked".to_string()),
        Err(error) => failed_primary_turn_commit(format!("turn commit worker failed: {error}")),
    }
}

fn failed_primary_turn_commit(persistence_error: String) -> PrimaryTurnCommit {
    PrimaryTurnCommit {
        outcome: TurnCommitOutcome {
            turn_persisted: false,
            persistence_error: Some(persistence_error),
        },
        deferred_sidecars: None,
    }
}

fn recent_tools_after_successful_turn(previous: &[String], result: &StreamResult) -> Vec<String> {
    if !result.tools_used.is_empty() {
        return result.tools_used.clone();
    }

    if result.tool_calls_count == 0 && result.tool_call_records.is_empty() {
        return previous.to_vec();
    }

    Vec::new()
}

struct TurnSuccessLiveSnapshot {
    session_id: Option<String>,
    run_id: Option<String>,
    had_journal: bool,
    turn: u32,
    total_prompt_tokens: u64,
    total_completion_tokens: u64,
    total_cache_read_tokens: u64,
    total_cache_creation_tokens: u64,
    total_session_cost: f64,
    last_response: Option<String>,
    continuation_anchor: Option<ContinuationAnchor>,
    pending_followup_suggestion: Option<crate::cli::followup_suggestion::FollowupSuggestion>,
    active_conversation: Option<astra_turn_core::active_conversation::ActiveConversation>,
    redo_stack: Vec<(String, String, u32)>,
    history: Vec<(String, String)>,
    recent_tools: Vec<String>,
    activated_deferred_tool_names: Vec<String>,
    resume_restricted_tools: Vec<String>,
    tool_health_entries: Vec<astra_turn_core::tool_health_persistence::ToolHealthEntry>,
    latest_turn_quality_feedback: Option<astra_runtime::self_model::TurnQualityFeedback>,
    latest_context_assembly_trace:
        Option<astra_turn_core::context_assembly_trace::ContextAssemblyTrace>,
    runtime_pipeline_state: Option<serde_json::Value>,
    runtime_compaction_state: Option<serde_json::Value>,
    runtime_consecutive_context_window_errors: u32,
    workspace_observation_quarantine:
        Option<astra_pipeline::step_protocol::WorkspaceObservationQuarantineV1>,
    last_turn_event: Option<session_journal::JournalEvent>,
    observability_session: Option<
        std::sync::Arc<std::sync::RwLock<astra_runtime::observability::ObservabilitySession>>,
    >,
    pending_adaptive_state: Option<crate::cli::session::session_state::PersistedAdaptiveState>,
    last_turn_interrupted: bool,
    session_persistence_error: Option<String>,
}

impl TurnSuccessLiveSnapshot {
    fn capture(state: &SessionState) -> Self {
        Self {
            session_id: state.session_id.clone(),
            run_id: state.run_id.clone(),
            had_journal: state.journal.is_some(),
            turn: state.turn,
            total_prompt_tokens: state.total_prompt_tokens,
            total_completion_tokens: state.total_completion_tokens,
            total_cache_read_tokens: state.total_cache_read_tokens,
            total_cache_creation_tokens: state.total_cache_creation_tokens,
            total_session_cost: state.total_session_cost,
            last_response: state.last_response.clone(),
            continuation_anchor: state.continuation_anchor.clone(),
            pending_followup_suggestion: state.pending_followup_suggestion.clone(),
            active_conversation: state.active_conversation.clone(),
            redo_stack: state.redo_stack.clone(),
            history: crate::cli::history_work::clone_pair_history(
                astra_core::history_work::HistoryWorkSite::CliSettlementRollbackSnapshot,
                &state.history,
            ),
            recent_tools: state.recent_tools.clone(),
            activated_deferred_tool_names: state.activated_deferred_tool_names.clone(),
            resume_restricted_tools: state.resume_restricted_tools.clone(),
            tool_health_entries: state.tool_health_entries.clone(),
            latest_turn_quality_feedback: state.latest_turn_quality_feedback.clone(),
            latest_context_assembly_trace: state.latest_context_assembly_trace.clone(),
            runtime_pipeline_state: state.runtime_pipeline_state.clone(),
            runtime_compaction_state: state.runtime_compaction_state.clone(),
            runtime_consecutive_context_window_errors: state
                .runtime_consecutive_context_window_errors,
            workspace_observation_quarantine: state.workspace_observation_quarantine.clone(),
            last_turn_event: state.last_turn_event.clone(),
            observability_session: state.observability_session.clone(),
            pending_adaptive_state: state.pending_adaptive_state.clone(),
            last_turn_interrupted: state.last_turn_interrupted,
            session_persistence_error: state.session_persistence_error.clone(),
        }
    }

    fn restore(self, state: &mut SessionState) {
        // A quarantine is sticky safety state.  If this turn discovered one
        // and the journal commit later failed, rolling the rest of the live
        // projection back must not erase it.  Only merge it when the failed
        // commit stayed on the same session; a session rebind must restore the
        // old snapshot without importing state from the new session.
        let same_session = self.session_id == state.session_id;
        let quarantine_after_rollback = if same_session {
            state
                .workspace_observation_quarantine
                .clone()
                .or_else(|| self.workspace_observation_quarantine.clone())
        } else {
            self.workspace_observation_quarantine.clone()
        };
        state.journal = None;
        match self.session_id.as_deref() {
            Some(session_id) => {
                state.set_session_id(session_id.to_string());
                if self.had_journal {
                    session_startup::attach_session_journal_pub(state, session_id);
                }
            }
            None => state.clear_session_id(),
        }
        state.run_id = self.run_id;
        state.turn = self.turn;
        state.total_prompt_tokens = self.total_prompt_tokens;
        state.total_completion_tokens = self.total_completion_tokens;
        state.total_cache_read_tokens = self.total_cache_read_tokens;
        state.total_cache_creation_tokens = self.total_cache_creation_tokens;
        state.total_session_cost = self.total_session_cost;
        state.last_response = self.last_response;
        state.continuation_anchor = self.continuation_anchor;
        state.pending_followup_suggestion = self.pending_followup_suggestion;
        state.active_conversation = self.active_conversation;
        state.redo_stack = self.redo_stack;
        state.history = self.history;
        state.recent_tools = self.recent_tools;
        state.activated_deferred_tool_names = self.activated_deferred_tool_names;
        state.resume_restricted_tools = self.resume_restricted_tools;
        state.tool_health_entries = self.tool_health_entries;
        state.latest_turn_quality_feedback = self.latest_turn_quality_feedback;
        state.latest_context_assembly_trace = self.latest_context_assembly_trace;
        state.runtime_pipeline_state = self.runtime_pipeline_state;
        state.runtime_compaction_state = self.runtime_compaction_state;
        state.runtime_consecutive_context_window_errors =
            self.runtime_consecutive_context_window_errors;
        state.workspace_observation_quarantine = quarantine_after_rollback;
        state.last_turn_event = self.last_turn_event;
        state.observability_session = self.observability_session;
        state.pending_adaptive_state = self.pending_adaptive_state;
        state.last_turn_interrupted = self.last_turn_interrupted;
        state.session_persistence_error = self.session_persistence_error;
    }
}

fn build_csl_manager(session_id: &str) -> Option<CslManager> {
    let store = Arc::new(
        astra_turn_core::conversation_log::file_store::FileCslStore::new(
            astra_services::session_journal::local_owner_sessions_dir(),
        ),
    );
    match CslManager::new(store, session_id.to_string(), Default::default()) {
        Ok(manager) => Some(manager),
        Err(error) => {
            astra_core::agent_warn!("csl", "manager init failed: {error}");
            None
        }
    }
}

fn initialize_post_commit_session_state(
    state: &mut SessionState,
    session_id: &str,
    session_rebound: bool,
) {
    if session_rebound || state.csl_manager.is_none() {
        state.csl_manager = build_csl_manager(session_id);
    }
}

fn clear_rebound_observability_session(state: &mut SessionState) {
    if let Some(session_id) = state.observability_session.as_ref().map(|session| {
        session
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .session_id
            .clone()
    }) && let Some(hub) = &state.observability_hub
    {
        let _ = hub.end_session(&session_id);
    }
    state.observability_session = None;
}

fn apply_turn_success_primary_sync(
    state: &mut SessionState,
    profile: Option<&str>,
    line: &str,
    mut result: StreamResult,
    turn_start: Instant,
) -> PrimaryTurnCommit {
    // Capture before session/turn mutation. If the primary turn event cannot
    // be persisted, this snapshot restores the live user-visible turn state.
    let live_snapshot = TurnSuccessLiveSnapshot::capture(state);
    let result_session_id = result.session_id.clone();
    let session_rebound = result_session_id.as_deref() != live_snapshot.session_id.as_deref();

    if let Some(session_id) = result_session_id.as_deref() {
        session_startup::attach_session_journal_pub(state, session_id);
        state.set_session_id(session_id.to_string());
        state.run_id = result.run_id.clone();
    }

    state.turn += 1;
    state.total_prompt_tokens += result.prompt_tokens;
    state.total_completion_tokens += result.completion_tokens;
    state.total_cache_read_tokens += result.cache_read_tokens;
    state.total_cache_creation_tokens += result.cache_creation_tokens;

    let turn_cost = crate::cli::slash::slash_stats::cost_for_tokens(
        result.prompt_tokens,
        result.completion_tokens,
        result.cache_read_tokens,
        result.cache_creation_tokens,
        &state.cached_pricing,
    );
    state.total_session_cost += turn_cost;
    let effective_user_input = result.effective_user_input(line);
    let latest_user_input = result.latest_user_input(line);
    if !latest_user_input.trim().is_empty() {
        state.continuation_anchor = build_continuation_anchor(state, &latest_user_input, &result);
        state.pending_followup_suggestion =
            crate::cli::followup_suggestion::suggest_followup(&latest_user_input, state, &result);
    }

    state.redo_stack.clear();
    let recent_tools = recent_tools_after_successful_turn(&state.recent_tools, &result);
    let learning_snap =
        analyze_chat_turn_learning(&latest_user_input, state.turn, &recent_tools, &result);
    state.last_response = Some(result.full_text.clone());
    state.history.push((
        effective_user_input.clone(),
        build_history_text(&result.full_text, &result.tool_call_records),
    ));
    state.recent_tools = recent_tools;
    state.resume_restricted_tools = result
        .interruption
        .as_ref()
        .map(astra_turn_core::interruption::resume_restricted_tools_from_interruption_json)
        .unwrap_or_default();
    super::turn_runtime_state::update_from_stream_result(state, &result);

    if !result.tool_health_export.is_empty() {
        state.tool_health_entries = result.tool_health_export.clone();
    }

    state.latest_turn_quality_feedback =
        turn_quality_feedback_from_eval(state.turn, &learning_snap.eval);
    let entity_skipped = learning_snap.eval.success
        && !result.tools_used.is_empty()
        && result.routing_domain_hint.is_none();
    result.entity_learn_skipped_no_domain = entity_skipped;

    let primary_commit = commit_primary_turn(state, line, &mut result, &learning_snap, turn_start);
    if !primary_commit.outcome.turn_persisted {
        let persistence_error = primary_commit.outcome.persistence_error.clone();
        live_snapshot.restore(state);
        state.session_persistence_error = persistence_error;
        return primary_commit;
    }

    persist_tool_health_after_durable_turn(state, profile);

    if let Some(session_id) = result_session_id.as_deref() {
        persist_profile_last_session_or_warn(
            profile,
            session_id,
            "turn_success:apply_turn_success_sync",
        );
        if session_rebound {
            clear_rebound_observability_session(state);
        }
        session_startup::initialize_journal_pub(state, session_id);
        initialize_post_commit_session_state(state, session_id, session_rebound);
    }

    state.last_turn_interrupted = result.interruption.is_some()
        || result.interruption_kind.is_some()
        || result.final_state == "interrupted";
    print_turn_status_line(state, &result, Some(&learning_snap.eval), turn_start);
    if state.tui_render_policy.is_none() {
        if let Some(suggestion) = state.pending_followup_suggestion.as_ref() {
            eprintln!(
                "{}",
                format!("  💡 Next prompt: {}  (Tab to accept)", suggestion.text).dim()
            );
        }
    }

    primary_commit
}

/// Cross-session learning is a derived snapshot: it must never survive a
/// failed primary turn, and a snapshot write failure must never roll back a
/// turn that is already durable in its journal.
fn persist_tool_health_after_durable_turn(state: &mut SessionState, profile: Option<&str>) {
    let profile_name = profile.unwrap_or("default");
    if let Err(error) = astra_turn_core::tool_health_persistence::save_learning_state(
        profile_name,
        &state.tool_health_entries,
    ) {
        let message = format!("persist tool-health snapshot for profile `{profile_name}`: {error}");
        tracing::warn!(
            target: "astra_cli::turn_settlement",
            profile = profile_name,
            error = %error,
            "turn is durable but cross-session tool-health persistence failed"
        );
        state.session_persistence_error = Some(message);
    }
}

#[cfg(test)]
fn apply_turn_success_sync(
    state: &mut SessionState,
    profile: Option<&str>,
    line: &str,
    result: StreamResult,
    turn_start: Instant,
) -> TurnCommitOutcome {
    let primary = apply_turn_success_primary_sync(state, profile, line, result, turn_start);
    let outcome = primary.outcome.clone();
    if let Some(sidecars) = primary.deferred_sidecars {
        if let Err(error) = sidecars.execute(false) {
            state.session_persistence_error = Some(error.to_string());
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::{apply_turn_success, apply_turn_success_async, apply_turn_success_sync};
    use crate::cli::session::session_state::PersistedAdaptiveState;
    use crate::cli::session::session_state::SessionState;
    use crate::cli::turn::turn_post_commit::{
        apply_turn_post_commit_completion, execute_turn_post_commit_job,
    };
    use crate::tests::{HomeGuard, heavy_checkpoint_with_runtime_state};
    use astra_services::session_journal;
    use astra_turn_core::tool_health_persistence::{ToolHealthEntry, load_tool_health};
    use std::time::Instant;

    #[test]
    #[serial_test::serial]
    fn apply_turn_success_sets_prompt_hint_for_followup() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();

        let mut state = SessionState::default();
        let mut result = crate::tests::stub_stream_result("Updated the code.");
        result.tools_used = vec!["str_replace".to_string()];

        apply_turn_success(&mut state, None, "fix the bug", result, Instant::now());

        assert_eq!(
            state
                .pending_followup_suggestion
                .as_ref()
                .map(|suggestion| suggestion.text.as_str()),
            Some("run the tests")
        );
    }

    #[test]
    #[serial_test::serial]
    fn turn_success_does_not_infer_domain_from_keyword_rich_text() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("typed-domain-none-{}", uuid::Uuid::new_v4());
        let mut state = SessionState::default();
        let mut result = crate::tests::stub_stream_result_with_records(
            "Done.",
            vec![session_journal::ToolCallRecord {
                name: "git".to_string(),
                ok: true,
                ..Default::default()
            }],
        );
        result.session_id = Some(session_id.clone());

        apply_turn_success(
            &mut state,
            None,
            "show github database memory and fix code",
            result,
            Instant::now(),
        );

        let event = session_journal::read_journal(&session_id)
            .expect("journal")
            .into_iter()
            .find(|event| event.event_type == session_journal::JournalEventType::Turn)
            .expect("turn event");
        assert_eq!(event.routing_domain_hint, None);
        assert!(
            event.entity_learn_skipped_no_domain,
            "missing typed domain must remain observably unknown"
        );
    }

    #[test]
    #[serial_test::serial]
    fn turn_success_preserves_typed_llm_domain_without_reclassification() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("typed-domain-value-{}", uuid::Uuid::new_v4());
        let mut state = SessionState::default();
        let mut result = crate::tests::stub_stream_result_with_records(
            "Done.",
            vec![session_journal::ToolCallRecord {
                name: "bash".to_string(),
                ok: true,
                ..Default::default()
            }],
        );
        result.session_id = Some(session_id.clone());
        result.routing_domain_hint = Some("database".to_string());

        apply_turn_success(&mut state, None, "opaque input", result, Instant::now());

        let event = session_journal::read_journal(&session_id)
            .expect("journal")
            .into_iter()
            .find(|event| event.event_type == session_journal::JournalEventType::Turn)
            .expect("turn event");
        assert_eq!(event.routing_domain_hint.as_deref(), Some("database"));
        assert!(!event.entity_learn_skipped_no_domain);
    }

    #[test]
    #[serial_test::serial]
    fn apply_turn_success_preserves_recent_tools_after_toolless_turn() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let mut state = SessionState {
            recent_tools: vec!["git".into(), "bash".into()],
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result(
            "我将逐个修复所有发现的问题。先确认每个文件的当前状态，再依次修复。",
        );
        result.visible_tools = vec![
            "bash".into(),
            "git".into(),
            "read_file".into(),
            "tool_search".into(),
        ];

        apply_turn_success(&mut state, None, "修复这些问题", result, Instant::now());

        assert_eq!(
            state.recent_tools,
            vec!["git".to_string(), "bash".to_string()],
            "a text-only turn must not erase the previous tool context"
        );
    }

    #[test]
    #[serial_test::serial]
    fn apply_turn_success_replaces_recent_tools_when_tools_run() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let mut state = SessionState {
            recent_tools: vec!["git".into(), "bash".into()],
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result("Updated one file.");
        result.tools_used = vec!["str_replace".into()];
        result.tool_calls_count = 1;

        apply_turn_success(&mut state, None, "apply the fix", result, Instant::now());

        assert_eq!(state.recent_tools, vec!["str_replace".to_string()]);
    }

    #[test]
    #[serial_test::serial]
    fn apply_turn_success_persists_tool_health_after_the_primary_turn_is_durable() {
        let (_sessions, _sessions_guard) = crate::tests::isolated_sessions_dir();
        let _home = HomeGuard::temp();
        let profile = format!("turn-success-health-{}", uuid::Uuid::new_v4());
        let sid = format!("turn-success-health-durable-{}", uuid::Uuid::new_v4());
        let mut state = SessionState::default();
        let mut result = crate::tests::stub_stream_result("Done.");
        result.session_id = Some(sid.clone());
        result.tool_health_export = vec![ToolHealthEntry {
            name: "git".to_string(),
            total_calls: 7,
            total_failures: 2,
            input_validation_failures: 3,
            failure_rate: 2.0 / 7.0,
            last_updated_epoch: 42,
            recent_outcomes: Vec::new(),
        }];

        apply_turn_success(
            &mut state,
            Some(&profile),
            "review the change",
            result,
            Instant::now(),
        );

        assert_eq!(state.tool_health_entries.len(), 1);
        assert!(state.session_persistence_error.is_none());
        assert!(
            session_journal::journal_file_path(&sid).is_file(),
            "the primary turn must be durable before its health snapshot is saved"
        );
        let restored = load_tool_health(&profile);
        assert_eq!(restored, state.tool_health_entries);
    }

    #[test]
    #[serial_test::serial]
    fn apply_turn_success_keeps_transient_evaluation_out_of_canonical_history() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let mut state = SessionState::default();
        let mut result = crate::tests::stub_stream_result_with_records(
            "Done.",
            vec![session_journal::ToolCallRecord {
                name: "read_file".to_string(),
                ok: false,
                error: Some("PATH_RESOLUTION_FAILED: requested path does not exist".to_string()),
                file_path: Some("obsolete/workspace/src/lib.rs".to_string()),
                ..Default::default()
            }],
        );
        result.llm_rounds = Some(40);
        result.prompt_tokens = 94_900;

        apply_turn_success(
            &mut state,
            None,
            "fix the broken build",
            result,
            Instant::now(),
        );

        let last_response = state.last_response.as_deref().unwrap_or_default();
        assert_eq!(last_response, "Done.");

        let history_text = state
            .history
            .last()
            .map(|(_, assistant)| assistant.as_str())
            .unwrap_or_default();
        assert_eq!(
            history_text, "Done.\n\n[Turn context: failed: read_file | tool_calls: 1]",
            "canonical history should contain only the assistant response and the structured tool projection"
        );
        let feedback = state
            .latest_turn_quality_feedback
            .as_ref()
            .expect("a failed tool outcome should guide the next turn");
        assert!(
            feedback
                .findings
                .iter()
                .any(|finding| finding.contains("tool outcome failure")),
            "the transient runtime feedback should retain the actionable tool failure"
        );
        assert!(
            !history_text.contains(&feedback.recommended_action),
            "next-turn runtime feedback must not become canonical conversation history"
        );
    }

    #[test]
    #[serial_test::serial]
    fn apply_turn_success_preserves_interrupted_state_for_partial_turn() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let mut state = SessionState::default();
        let mut result = crate::tests::stub_stream_result(
            "[budget_exhausted] 46 tool call(s) completed. A checkpoint was saved. You can continue in the next message.",
        );
        result.final_state = "interrupted".into();
        result.interruption_kind = Some("budget_exhausted".into());
        result.interruption = Some(serde_json::json!({
            "kind": "budget_exhausted",
            "resumable": true,
            "resume_restricted_tools": ["bash"],
            "user_message": "[budget_exhausted] 46 tool call(s) completed. A checkpoint was saved. You can continue in the next message."
        }));

        apply_turn_success(&mut state, None, "continue", result, Instant::now());

        assert!(state.last_turn_interrupted);
        assert_eq!(state.resume_restricted_tools, vec!["bash".to_string()]);
        assert!(
            state
                .last_response
                .as_deref()
                .unwrap_or_default()
                .contains("46 tool call(s) completed")
        );
    }

    #[test]
    #[serial_test::serial]
    fn apply_turn_success_clears_stale_prompt_hint_when_suppressed() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();

        let mut state = SessionState {
            cloud_plan_mirror: Some(astra_runtime::plan::PlanModeState::new("goal".to_string())),
            ..Default::default()
        };
        state
            .perm_manager
            .set_mode(crate::cli::permission_manager::PermissionMode::Plan);
        let result = crate::tests::stub_stream_result("Plan updated.");

        apply_turn_success(&mut state, None, "continue", result, Instant::now());

        assert!(state.pending_followup_suggestion.is_none());
    }

    #[test]
    #[serial_test::serial]
    fn apply_turn_success_updates_runtime_recovery_state_from_heavy_checkpoint() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("turn-runtime-state-{}", uuid::Uuid::new_v4());
        let mut state = SessionState {
            runtime_pipeline_state: Some(serde_json::json!({"old": true})),
            runtime_compaction_state: Some(serde_json::json!({"old": true})),
            runtime_consecutive_context_window_errors: 9,
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result("done");
        result.session_id = Some(sid);
        result.last_heavy_checkpoint = Some(heavy_checkpoint_with_runtime_state(
            serde_json::json!({"stats": {"ema": 0.8}}),
            serde_json::json!({
                "attempt_count": 3,
                "cumulative_tokens_freed": 15000,
                "last_tokens_freed": 4000,
                "last_was_insufficient": true,
            }),
            2,
        ));

        apply_turn_success(&mut state, None, "continue", result, Instant::now());

        assert_eq!(
            state.runtime_pipeline_state,
            Some(serde_json::json!({"stats": {"ema": 0.8}}))
        );
        assert_eq!(
            state.runtime_compaction_state,
            Some(serde_json::json!({
                "attempt_count": 3,
                "cumulative_tokens_freed": 15000,
                "last_tokens_freed": 4000,
                "last_was_insufficient": true,
            }))
        );
        assert_eq!(state.runtime_consecutive_context_window_errors, 2);
    }

    #[test]
    #[serial_test::serial]
    fn apply_turn_success_clears_runtime_recovery_state_without_checkpoint() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("turn-runtime-state-clear-{}", uuid::Uuid::new_v4());
        let mut state = SessionState {
            runtime_pipeline_state: Some(serde_json::json!({"old": true})),
            runtime_compaction_state: Some(serde_json::json!({"old": true})),
            runtime_consecutive_context_window_errors: 9,
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result("done");
        result.session_id = Some(sid);

        apply_turn_success(&mut state, None, "continue", result, Instant::now());

        assert!(state.runtime_pipeline_state.is_none());
        assert!(state.runtime_compaction_state.is_none());
        assert_eq!(state.runtime_consecutive_context_window_errors, 0);
    }

    #[test]
    #[serial_test::serial]
    fn apply_turn_success_rolls_back_live_state_when_turn_journal_fails() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("turn-success-journal-fail-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        std::fs::create_dir(writer.path()).unwrap();
        let mut state = SessionState {
            journal: Some(writer),
            session_id: Some(sid),
            model: Some("gpt-5".into()),
            turn: 2,
            total_prompt_tokens: 100,
            total_completion_tokens: 50,
            total_cache_read_tokens: 7,
            total_cache_creation_tokens: 3,
            total_session_cost: 1.25,
            last_response: Some("old response".into()),
            history: vec![("old question".into(), "old answer".into())],
            recent_tools: vec!["read_file".into()],
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result("new response");
        result.prompt_tokens = 11;
        result.completion_tokens = 13;
        result.cache_read_tokens = 17;
        result.cache_creation_tokens = 19;
        result.tools_used = vec!["bash".into()];
        result.last_heavy_checkpoint = Some(heavy_checkpoint_with_runtime_state(
            serde_json::json!({"stats": {"ema": 0.4}}),
            serde_json::json!({"attempt_count": 1}),
            1,
        ));
        if let Some(astra_pipeline::step_protocol::StepCheckpoint::Heavy(heavy)) =
            result.last_heavy_checkpoint.as_mut()
        {
            heavy.workspace_observation_quarantine = Some(
                astra_pipeline::step_protocol::WorkspaceObservationQuarantineV1::weak_process_ownership(
                    Some("failed-commit-call".into()),
                ),
            );
        }

        let outcome =
            apply_turn_success_sync(&mut state, None, "new question", result, Instant::now());
        assert!(!outcome.turn_persisted);

        assert_eq!(state.turn, 2);
        assert_eq!(state.total_prompt_tokens, 100);
        assert_eq!(state.total_completion_tokens, 50);
        assert_eq!(state.total_cache_read_tokens, 7);
        assert_eq!(state.total_cache_creation_tokens, 3);
        assert_eq!(state.total_session_cost, 1.25);
        assert_eq!(state.last_response.as_deref(), Some("old response"));
        assert_eq!(
            state.history,
            vec![("old question".to_string(), "old answer".to_string())]
        );
        assert_eq!(state.recent_tools, vec!["read_file".to_string()]);
        assert!(
            state
                .session_persistence_error
                .as_deref()
                .unwrap_or_default()
                .contains("append turn event")
        );
        assert_eq!(
            state
                .workspace_observation_quarantine
                .as_ref()
                .and_then(|q| q.source_tool_call_id.as_deref()),
            Some("failed-commit-call")
        );
    }

    #[test]
    #[serial_test::serial]
    fn failed_primary_turn_does_not_persist_tool_health_snapshot() {
        let (_sessions, _sessions_guard) = crate::tests::isolated_sessions_dir();
        let _home = HomeGuard::temp();
        let profile = format!("turn-success-health-failed-{}", uuid::Uuid::new_v4());
        let sid = format!("turn-success-health-journal-fail-{}", uuid::Uuid::new_v4());
        std::fs::create_dir_all(session_journal::journal_file_path(&sid)).unwrap();

        let mut state = SessionState::default();
        let mut result = crate::tests::stub_stream_result("new response");
        result.session_id = Some(sid);
        result.tool_health_export = vec![ToolHealthEntry {
            name: "git".to_string(),
            total_calls: 1,
            total_failures: 1,
            input_validation_failures: 0,
            failure_rate: 1.0,
            last_updated_epoch: 7,
            recent_outcomes: Vec::new(),
        }];

        let outcome = apply_turn_success_sync(
            &mut state,
            Some(&profile),
            "new question",
            result,
            Instant::now(),
        );

        assert!(!outcome.turn_persisted);
        assert!(state.tool_health_entries.is_empty());
        assert!(load_tool_health(&profile).is_empty());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn apply_turn_success_async_rolls_back_first_turn_without_post_commit_side_effects() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("turn-success-first-turn-fail-{}", uuid::Uuid::new_v4());
        let journal_path = session_journal::journal_file_path(&sid);
        std::fs::create_dir_all(&journal_path).unwrap();

        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let mut ui = crate::tests::TestUi::default();
        let mut state = SessionState {
            last_turn_interrupted: true,
            observability_hub: Some(std::sync::Arc::new(
                astra_runtime::observability::ObservabilityHub::new(),
            )),
            pending_adaptive_state: Some(PersistedAdaptiveState {
                active_experiment_id: Some("exp-1".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result("new response");
        result.session_id = Some(sid.clone());
        result.run_id = Some("run-1".into());
        result.final_messages = vec![serde_json::json!({"role": "assistant", "content": "done"})];

        apply_turn_success_async(
            &mut state,
            &api,
            None,
            "new question",
            result,
            Instant::now(),
            &mut ui,
            None,
        )
        .await;

        assert!(state.session_id.is_none());
        assert!(state.run_id.is_none());
        assert!(state.journal.is_none());
        assert!(state.csl_manager.is_none());
        assert!(state.observability_session.is_none());
        assert!(state.last_turn_interrupted);
        assert!(state.pending_adaptive_state.is_some());
        assert!(
            !crate::cli::session::session_recovery::io::csl_log_path_for(&sid).exists(),
            "post-commit CSL persistence must be skipped when the primary turn commit fails"
        );
        assert!(
            state
                .session_persistence_error
                .as_deref()
                .unwrap_or_default()
                .contains("append turn event")
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn tui_post_commit_queue_releases_turn_after_primary_journal_event() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("queued-turn-post-commit-{}", uuid::Uuid::new_v4());
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let mut state = SessionState {
            session_id: Some(sid.clone()),
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            model: Some("gpt-5".into()),
            ..Default::default()
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let mut ui = crate::tests::TestUi::default();
        let mut result = crate::tests::stub_stream_result("The canonical event is durable.");
        result.final_messages = vec![
            serde_json::json!({"role": "user", "content": "inspect persistence"}),
            serde_json::json!({"role": "assistant", "content": "The canonical event is durable."}),
        ];
        astra_turn_types::mark_user_turn_semantics(
            &mut result.final_messages[0],
            astra_turn_types::UserTurnSemantics::new(
                astra_turn_types::ObjectiveRelation::Replace,
                None,
            ),
        );

        apply_turn_success_async(
            &mut state,
            &api,
            None,
            "inspect persistence",
            result,
            Instant::now(),
            &mut ui,
            Some(&tx),
        )
        .await;

        let primary_events = session_journal::read_journal(&sid).unwrap();
        assert!(
            primary_events
                .iter()
                .any(|event| { event.event_type == session_journal::JournalEventType::Turn })
        );
        let anchor = state
            .continuation_anchor
            .as_ref()
            .expect("typed objective should reach foreground continuation projection");
        assert_eq!(
            anchor.objective_context,
            vec!["objective: inspect persistence"]
        );
        assert!(
            !primary_events.iter().any(|event| {
                event.event_type == session_journal::JournalEventType::TurnEvaluation
            }),
            "derived sidecars must not extend the foreground turn"
        );

        let job = rx.recv().await.expect("post-commit job should be queued");
        let completion = execute_turn_post_commit_job(job).await;
        assert!(completion.errors.is_empty(), "{:?}", completion.errors);
        let errors = apply_turn_post_commit_completion(completion, &mut state);
        assert!(errors.is_empty(), "{errors:?}");
        let projected_events = session_journal::read_journal(&sid).unwrap();
        assert!(projected_events.iter().any(|event| {
            event.event_type == session_journal::JournalEventType::TurnEvaluation
        }));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn durable_tail_guidance_is_included_when_primary_live_fanout_closed() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("remote-guidance-commit-{}", uuid::Uuid::new_v4());
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let control = crate::cli::turn::local_run_control::LocalRunControl::shared();
        control.expect_remote_user_intent_disposition("intent-tail", 1);
        control.record_remotely_applied_user_intent(
            crate::cli::stream::streaming_types::AppliedStreamUserIntent {
                intent_id: "intent-tail".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::Applied,
                event_index: 17,
                content: "wait".into(),
            },
        );
        let mut state = SessionState {
            session_id: Some(sid.clone()),
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            model: Some("test-model".into()),
            ..Default::default()
        };
        *astra_core::sync_poison::recover_mutex_lock(&state.active_turn_local_run_control) =
            Some(control);
        let mut result = crate::tests::stub_stream_result("done");
        result.final_messages = vec![
            serde_json::json!({"role":"user","content":"original"}),
            serde_json::json!({"role":"assistant","content":"done"}),
        ];
        let mut ui = crate::tests::TestUi::default();

        apply_turn_success_async(
            &mut state,
            &api,
            None,
            "original",
            result,
            Instant::now(),
            &mut ui,
            None,
        )
        .await;

        let event = state.last_turn_event.as_ref().expect("turn event");
        let applied = event
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("user_intents"))
            .and_then(serde_json::Value::as_array)
            .expect("applied guidance metadata");
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0]["intent_id"], "intent-tail");
        assert_eq!(applied[0]["content"], "wait");
    }

    #[test]
    #[serial_test::serial]
    fn apply_turn_success_rebinds_observability_session_through_hub() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let hub = std::sync::Arc::new(astra_runtime::observability::ObservabilityHub::new());
        let pending = hub.start_session("user-1", "pending");
        let mut state = SessionState {
            ingestion_user_id: Some("user-1".into()),
            observability_hub: Some(hub.clone()),
            observability_session: Some(pending),
            pending_adaptive_state: Some(PersistedAdaptiveState {
                active_experiment_id: Some("exp-1".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result("done");
        result.session_id = Some("sess-live".into());
        result.run_id = Some("run-1".into());

        let outcome = apply_turn_success_sync(&mut state, None, "continue", result, Instant::now());

        assert!(outcome.turn_persisted);
        assert_eq!(state.session_id.as_deref(), Some("sess-live"));
        assert_eq!(
            state
                .observability_session
                .as_ref()
                .map(|session| {
                    session
                        .read()
                        .unwrap_or_else(|error| error.into_inner())
                        .session_id
                        .clone()
                })
                .as_deref(),
            Some("sess-live")
        );
        assert!(hub.get_session("pending").is_none());
        assert!(hub.get_session("sess-live").is_some());
        assert!(state.pending_adaptive_state.is_none());
    }
}
