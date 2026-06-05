use std::sync::Arc;
use std::time::Instant;

use astra_turn_core::chat_turn_heuristics::looks_like_live_query_with_context;
use astra_turn_core::conversation_log::manager::CslManager;

#[cfg(test)]
use super::session_improvement;
use super::session_projection::build_continuation_anchor;
use super::session_startup;
use super::turn_commit::TurnCommitOutcome;
use super::turn_commit::commit_turn_journal_workspace_and_sidecars;
use super::turn_learning::{analyze_chat_turn_learning, turn_quality_feedback_from_eval};
use super::turn_post_commit::{extract_csl_fields_from_result, run_turn_post_commit_tasks};
use super::turn_reporting::{build_history_text, print_turn_status_line};
use super::*;
use crate::StreamResult;

/// Test-only sync variant of `apply_turn_success`. Production code paths must
/// use [`apply_turn_success_async`] so the LLM-driven skill-improvement path
/// can await its network call. This wrapper keeps the existing synchronous
/// test fixtures working without pulling a tokio runtime into every assertion.
#[cfg(test)]
pub(crate) fn apply_turn_success(
    state: &mut SessionState,
    profile: Option<&str>,
    line: &str,
    result: StreamResult,
    turn_start: Instant,
) {
    let _ = apply_turn_success_sync(state, profile, line, result, turn_start);
    session_improvement::check_skill_improvement_sync(state);
}

pub(crate) async fn apply_turn_success_async(
    state: &mut SessionState,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    line: &str,
    mut result: StreamResult,
    turn_start: Instant,
    ui: &mut dyn crate::cli::ui_adapter::ReplUiAdapter,
) {
    let final_messages = std::mem::take(&mut result.final_messages);
    let csl_checkpoint_fields = extract_csl_fields_from_result(&result);
    let commit_outcome = apply_turn_success_sync(state, profile, line, result, turn_start);
    if commit_outcome.turn_persisted {
        run_turn_post_commit_tasks(
            state,
            api,
            profile,
            final_messages,
            csl_checkpoint_fields,
            turn_start,
            ui,
        )
        .await;
    }
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
    redo_stack: Vec<(String, String, u32)>,
    history: Vec<(String, String)>,
    recent_tools: Vec<String>,
    resume_restricted_tools: Vec<String>,
    tool_health_entries: Vec<astra_turn_core::tool_health_persistence::ToolHealthEntry>,
    latest_turn_quality_feedback: Option<astra_runtime::self_model::TurnQualityFeedback>,
    latest_context_assembly_trace:
        Option<astra_turn_core::context_assembly_trace::ContextAssemblyTrace>,
    last_turn_event: Option<session_journal::JournalEvent>,
    observability_session: Option<
        std::sync::Arc<std::sync::RwLock<astra_runtime::observability::ObservabilitySession>>,
    >,
    pending_adaptive_state: Option<crate::cli::session_state::PersistedAdaptiveState>,
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
            redo_stack: state.redo_stack.clone(),
            history: state.history.clone(),
            recent_tools: state.recent_tools.clone(),
            resume_restricted_tools: state.resume_restricted_tools.clone(),
            tool_health_entries: state.tool_health_entries.clone(),
            latest_turn_quality_feedback: state.latest_turn_quality_feedback.clone(),
            latest_context_assembly_trace: state.latest_context_assembly_trace.clone(),
            last_turn_event: state.last_turn_event.clone(),
            observability_session: state.observability_session.clone(),
            pending_adaptive_state: state.pending_adaptive_state.clone(),
            last_turn_interrupted: state.last_turn_interrupted,
            session_persistence_error: state.session_persistence_error.clone(),
        }
    }

    fn restore(self, state: &mut SessionState) {
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
        state.redo_stack = self.redo_stack;
        state.history = self.history;
        state.recent_tools = self.recent_tools;
        state.resume_restricted_tools = self.resume_restricted_tools;
        state.tool_health_entries = self.tool_health_entries;
        state.latest_turn_quality_feedback = self.latest_turn_quality_feedback;
        state.latest_context_assembly_trace = self.latest_context_assembly_trace;
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
            astra_services::session_journal::local_sessions_dir(),
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

fn apply_turn_success_sync(
    state: &mut SessionState,
    profile: Option<&str>,
    line: &str,
    result: StreamResult,
    turn_start: Instant,
) -> TurnCommitOutcome {
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

    let turn_cost = crate::cli::slash_stats::cost_for_tokens(
        result.prompt_tokens,
        result.completion_tokens,
        result.cache_read_tokens,
        result.cache_creation_tokens,
        &state.cached_pricing,
    );
    state.total_session_cost += turn_cost;
    state.last_response = Some(result.full_text.clone());
    state.continuation_anchor = build_continuation_anchor(state, line, &result);
    state.pending_followup_suggestion =
        crate::cli::followup_suggestion::suggest_followup(line, state, &result);

    state.redo_stack.clear();
    state.history.push((
        line.to_string(),
        build_history_text(&result.full_text, &result.tool_call_records),
    ));
    state.recent_tools = result.tools_used.clone();
    state.resume_restricted_tools = result
        .interruption
        .as_ref()
        .map(astra_turn_core::interruption::resume_restricted_tools_from_interruption_json)
        .unwrap_or_default();

    if !result.tool_health_export.is_empty() {
        state.tool_health_entries = result.tool_health_export.clone();
    }

    let learning_snap = analyze_chat_turn_learning(line, state.turn, &state.recent_tools, &result);
    state.latest_turn_quality_feedback =
        turn_quality_feedback_from_eval(state.turn, &learning_snap.eval);
    let mut result = result;
    let routing_domain = learning_snap
        .routing
        .domain_hint
        .map(|domain| astra_turn_core::routing_engine::domain_hint_to_label(domain).to_string());
    let entity_skipped = learning_snap.eval.success
        && !result.tools_used.is_empty()
        && learning_snap.routing.domain_hint.is_none();
    result.set_repl_learning_journal_fields(routing_domain, entity_skipped);

    let commit_outcome = commit_turn_journal_workspace_and_sidecars(
        state,
        line,
        &result,
        &learning_snap,
        turn_start,
    );
    if !commit_outcome.turn_persisted {
        let persistence_error = commit_outcome.persistence_error.clone();
        live_snapshot.restore(state);
        state.session_persistence_error = persistence_error;
        return commit_outcome;
    }

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

    state.last_turn_interrupted = false;
    print_turn_status_line(state, &result, turn_start);
    if state.tui_render_policy.is_none() {
        if let Some(suggestion) = state.pending_followup_suggestion.as_ref() {
            eprintln!(
                "{}",
                format!("  💡 Next prompt: {}  (Tab to accept)", suggestion.text).dim()
            );
        }

        if result.tool_calls_count == 0
            && looks_like_live_query_with_context(line, &state.recent_tools)
        {
            eprintln!(
                "{}",
                "  ⚠ Warning: This answer was generated without tool calls. Data may be hallucinated."
                    .yellow()
            );
        }
    }

    commit_outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct CollectingUi;

    impl crate::cli::ui_adapter::ReplUiAdapter for CollectingUi {
        fn show_error(&mut self, _msg: &str) {}
        fn show_warning(&mut self, _msg: &str) {}
        fn show_info(&mut self, _msg: &str) {}
        fn show_status(&mut self, _msg: &str) {}
        fn blank_line(&mut self) {}
    }

    fn isolated_sessions_dir() -> (tempfile::TempDir, session_journal::JournalDirGuard) {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let guard = session_journal::JournalDirGuard::new(&sessions);
        (tmp, guard)
    }

    fn stub_stream_result(full_text: &str) -> StreamResult {
        StreamResult {
            session_id: None,
            run_id: None,
            session_persistence_error: None,
            full_text: full_text.to_string(),
            prompt_tokens: 0,
            completion_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_calls_count: 0,
            tools_selected: Vec::new(),
            selected_skills: Vec::new(),
            tools_used: Vec::new(),
            tool_call_records: Vec::new(),
            budget_used: 0,
            budget_pressure: 0.0,
            stall_events: Vec::new(),
            verdict_events: Vec::new(),
            step_recorder_summary: None,
            tool_health_export: Vec::new(),
            last_heavy_checkpoint: None,
            ttft_ms: None,
            context_ms: None,
            memoria_ms: None,
            routing_domain_hint: None,
            entity_learn_skipped_no_domain: false,
            pending_context_assembly_trace: None,
            turn_observability_events: Vec::new(),
            llm_rounds: None,
            interruption: None,
            final_state: "completed".into(),
            interruption_kind: None,
            final_messages: Vec::new(),
            background_agent_results: Vec::new(),
        }
    }

    #[test]
    #[serial_test::serial]
    fn apply_turn_success_sets_prompt_hint_for_followup() {
        let (_tmp, _g) = isolated_sessions_dir();

        let mut state = SessionState::default();
        let mut result = stub_stream_result("Updated the code.");
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
    fn apply_turn_success_clears_stale_prompt_hint_when_suppressed() {
        let (_tmp, _g) = isolated_sessions_dir();

        let mut state = SessionState {
            cloud_plan_mirror: Some(astra_runtime::plan::PlanModeState::new("goal".to_string())),
            ..Default::default()
        };
        state
            .perm_manager
            .set_mode(crate::cli::permission_manager::PermissionMode::Plan);
        let result = stub_stream_result("Plan updated.");

        apply_turn_success(&mut state, None, "continue", result, Instant::now());

        assert!(state.pending_followup_suggestion.is_none());
    }

    #[test]
    #[serial_test::serial]
    fn apply_turn_success_rolls_back_live_state_when_turn_journal_fails() {
        let (_tmp, _g) = isolated_sessions_dir();
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
        let mut result = stub_stream_result("new response");
        result.prompt_tokens = 11;
        result.completion_tokens = 13;
        result.cache_read_tokens = 17;
        result.cache_creation_tokens = 19;
        result.tools_used = vec!["bash".into()];

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
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn apply_turn_success_async_rolls_back_first_turn_without_post_commit_side_effects() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = format!("turn-success-first-turn-fail-{}", uuid::Uuid::new_v4());
        let journal_path = session_journal::journal_file_path(&sid);
        std::fs::create_dir_all(&journal_path).unwrap();

        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let mut ui = CollectingUi;
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
        let mut result = stub_stream_result("new response");
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
        )
        .await;

        assert!(state.session_id.is_none());
        assert!(state.run_id.is_none());
        assert!(state.journal.is_none());
        assert!(state.csl_manager.is_none());
        assert!(state.observability_session.is_none());
        assert!(state.last_turn_interrupted);
        assert_eq!(state.task_manager.session_id(), "");
        assert!(state.pending_adaptive_state.is_some());
        assert!(
            !crate::cli::session_recovery::csl::csl_log_path_for(&sid).exists(),
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

    #[test]
    #[serial_test::serial]
    fn apply_turn_success_rebinds_observability_session_through_hub() {
        let (_tmp, _g) = isolated_sessions_dir();
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
        let mut result = stub_stream_result("done");
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
