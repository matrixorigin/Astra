//! Durable turn commit: journal, workspace, checkpoint, and sidecar persistence.

use std::time::Instant;

use super::turn_learning::TurnLearningSnapshot;
use crate::cli::cli_config::cli_utils;
use crate::cli::session::session_lessons;
use crate::cli::session::session_recovery;
use crate::cli::session::session_side_effects::{
    build_bridge_pipeline_journal_events, enqueue_ingestion_pub,
};
use crate::cli::session::session_state::SessionState;
use crate::cli::stream::streaming_types::StreamResult;
use astra_services::session_journal;

fn cache_pending_context_assembly_trace(state: &mut SessionState, trace_json: &serde_json::Value) {
    match serde_json::from_value::<astra_turn_core::context_assembly_trace::ContextAssemblyTrace>(
        trace_json.clone(),
    ) {
        Ok(trace) => state.latest_context_assembly_trace = Some(trace),
        Err(err) => {
            astra_core::agent_warn!("context_trace", "failed to cache context trace: {err}")
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnStallType<'a> {
    SignalStall,
    SkillLockout { skill: Option<&'a str> },
    Other,
}

impl<'a> TurnStallType<'a> {
    fn parse(raw: &'a str) -> Self {
        match raw {
            "sig_stall" => Self::SignalStall,
            "skill_lockout" => Self::SkillLockout { skill: None },
            value => value
                .strip_prefix("skill_lockout:")
                .map(|skill| Self::SkillLockout { skill: Some(skill) })
                .unwrap_or(Self::Other),
        }
    }

    fn confidence(self) -> f64 {
        match self {
            Self::SignalStall | Self::SkillLockout { .. } => 1.0,
            Self::Other => 0.0,
        }
    }
}

pub(crate) fn stall_type_confidence(stall_type: &str) -> f64 {
    TurnStallType::parse(stall_type).confidence()
}

fn rewrite_workspace_persistence_error(
    state: &SessionState,
    session_id: &str,
    persistence_error: Option<String>,
) -> std::io::Result<()> {
    let mut workspace = session_recovery::workspace_metadata_from_live_state(state, session_id);
    workspace.last_persistence_error = persistence_error;
    astra_services::session_workspace::write_workspace(&workspace)
}

#[derive(Default)]
struct TurnCommitIssues {
    messages: Vec<String>,
}

impl TurnCommitIssues {
    fn summary(&self) -> Option<String> {
        (!self.messages.is_empty()).then(|| self.messages.join("; "))
    }

    fn record_error(&mut self, action: &str, error: impl std::fmt::Display) {
        let message = format!("failed to {action}: {error}");
        astra_core::agent_warn!("turn_commit", "{message}");
        self.messages.push(message);
    }

    fn into_summary(self) -> Option<String> {
        self.summary()
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TurnCommitOutcome {
    /// True when the primary turn event is durable (or no durable session exists).
    ///
    /// Sidecar/workspace failures may still set `persistence_error` while this
    /// remains true; callers should rollback live turn state only when this is
    /// false.
    pub(crate) turn_persisted: bool,
    /// Summary of durable degradation encountered during commit.
    pub(crate) persistence_error: Option<String>,
}

fn persist_workspace_and_checkpoint(
    state: &SessionState,
    session_id: &str,
    result: &StreamResult,
    issues: &mut TurnCommitIssues,
    sidecar_events: &mut Vec<session_journal::JournalEvent>,
) -> bool {
    let mut workspace = session_recovery::workspace_metadata_from_live_state(state, session_id);
    let mut checkpoint_event = None;
    let mut checkpoint_to_cleanup = None;

    if astra_services::session_checkpoint::should_checkpoint(
        workspace.turn_count,
        astra_services::session_checkpoint::CHECKPOINT_INTERVAL,
    ) {
        let checkpoint_number = workspace.checkpoints.len() as u32 + 1;
        let checkpoint = astra_services::session_checkpoint::Checkpoint {
            number: checkpoint_number,
            turn: workspace.turn_count,
            title: format!("Turn {} checkpoint", workspace.turn_count),
            summary: format!(
                "Accumulated {} tokens ({} in, {} out). Tools: {}",
                workspace.total_tokens_in + workspace.total_tokens_out,
                workspace.total_tokens_in,
                workspace.total_tokens_out,
                result.tools_used.join(", "),
            ),
            tools_used: result.tools_used.clone(),
            total_tokens: workspace.total_tokens_in + workspace.total_tokens_out,
            had_stalls: false,
            error_count: 0,
            contract_state_json: state
                .durable_task_state
                .as_ref()
                .and_then(|durable| serde_json::to_string(&durable.contract).ok()),
        };

        match astra_services::session_checkpoint::write_checkpoint(session_id, &checkpoint) {
            Ok(_path) => {
                workspace.record_checkpoint();
                checkpoint_event = Some(session_journal::JournalEvent::checkpoint(
                    Some(session_id),
                    workspace.turn_count,
                    &checkpoint.summary,
                    checkpoint.total_tokens,
                    checkpoint.tools_used.len(),
                ));
                checkpoint_to_cleanup = Some(checkpoint);
            }
            Err(error) => {
                issues.record_error("write session checkpoint", error);
            }
        }
    }

    workspace.last_persistence_error = issues.summary();
    if let Err(error) = astra_services::session_workspace::write_workspace(&workspace) {
        issues.record_error("write workspace metadata", error);
        if let Some(checkpoint) = checkpoint_to_cleanup.as_ref()
            && let Err(cleanup_error) =
                astra_services::session_checkpoint::remove_checkpoint(session_id, checkpoint)
        {
            issues.record_error("remove unreferenced session checkpoint", cleanup_error);
        }
        return false;
    }

    if let Some(event) = checkpoint_event {
        sidecar_events.push(event);
    }
    true
}

fn extend_runtime_sidecar_events(
    sidecar_events: &mut Vec<session_journal::JournalEvent>,
    state: &SessionState,
    line: &str,
    result: &StreamResult,
    learning_snap: &TurnLearningSnapshot,
) {
    for (stall_type, _) in &result.stall_events {
        let confidence = stall_type_confidence(stall_type);
        if confidence == 0.0 {
            continue;
        }
        let stall_event = session_journal::JournalEvent::stall_detected(
            state.session_id.as_deref(),
            state.turn,
            stall_type,
            0,
            confidence,
            &[],
        );
        sidecar_events.push(stall_event);
    }

    for verdict in &result.verdict_events {
        let verdict_event = session_journal::JournalEvent::turn_guard_verdict(
            state.session_id.as_deref(),
            state.turn,
            &verdict.severity,
            &verdict.injections,
            &verdict.avoid_tools,
            &verdict.deprioritized_tools,
            verdict.force_stop,
            verdict.nudge_count,
            verdict.total_errors,
            verdict.deprioritized_count,
            verdict.total_timeouts,
            &verdict.timeout_dominant_tools,
            verdict.total_cache_hits,
            verdict.flaky_count,
        );
        sidecar_events.push(verdict_event);
    }

    let turn_eval_event = astra_runtime::pipeline::evaluation::build_turn_evaluation_journal_event(
        state.session_id.as_deref(),
        Some(state.turn),
        "cli_repl",
        line,
        &state.recent_tools,
        &result.tool_call_records,
        result.stall_events.len(),
        result.verdict_events.iter().any(|event| {
            event.severity.eq_ignore_ascii_case("warning")
                || event.severity.eq_ignore_ascii_case("critical")
        }),
        result.budget_pressure,
        &learning_snap.eval,
    );
    sidecar_events.push(turn_eval_event);
}

fn build_primary_turn_event(
    state: &SessionState,
    line: &str,
    result: &mut StreamResult,
    turn_start: Instant,
    issues: &mut TurnCommitIssues,
) -> (
    session_journal::JournalEvent,
    Vec<session_journal::JournalEvent>,
) {
    let mut turn_observability_events = std::mem::take(&mut result.turn_observability_events);
    let bridge_pipeline_events = match build_bridge_pipeline_journal_events(
        state.session_id.as_deref(),
        state.turn,
        astra_core::model_override::normalize_model_override(state.model.as_deref())
            .unwrap_or("unknown"),
        &turn_observability_events,
    ) {
        Ok(events) => events,
        Err(error) => {
            issues.record_error("build bridge pipeline journal events", error);
            Vec::new()
        }
    };
    turn_observability_events.extend(bridge_pipeline_events);

    let mut turn_event = session_journal::JournalEvent::turn(
        state.session_id.as_deref(),
        state.turn,
        astra_core::model_override::normalize_model_override(state.model.as_deref()),
        line,
        &result.full_text,
        result.tool_calls_count,
        result.prompt_tokens,
        result.completion_tokens,
        turn_start.elapsed().as_millis() as u64,
    )
    .with_tool_surface(
        std::mem::take(&mut result.visible_tools),
        std::mem::take(&mut result.selected_skills),
        result.tools_used.clone(),
        result.budget_used,
    )
    .with_run_id(result.run_id.as_deref())
    .with_tool_calls(result.tool_call_records.clone())
    .with_budget_pressure(result.budget_pressure)
    .with_plan_subtask(state.current_plan_subtask_id.as_deref())
    .with_ttft(result.ttft_ms)
    .with_context_time(result.context_ms)
    .with_routing_telemetry(
        result.routing_domain_hint.take(),
        result.entity_learn_skipped_no_domain,
    )
    .with_memoria_time(result.memoria_ms)
    .with_cache_tokens(result.cache_read_tokens, result.cache_creation_tokens);

    let git_root = session_recovery::session_workspace_git_root(state.session_id.as_deref());
    let (git_head, git_branch) = cli_utils::git_snapshot(git_root.as_deref());
    turn_event = turn_event.with_git_snapshot(git_head, git_branch);

    turn_event.llm_rounds = result.llm_rounds;
    let tool_ms: u64 = result
        .tool_call_records
        .iter()
        .filter(|record| !record.is_synthetic_placeholder())
        .map(|record| record.ms)
        .sum();
    turn_event.total_tool_ms = Some(tool_ms);
    if let Some(duration) = turn_event.duration_ms {
        turn_event.total_llm_ms = Some(duration.saturating_sub(tool_ms));
    }
    if let Some(interruption) = result.interruption.as_ref() {
        turn_event.metadata = Some(merge_interruption_metadata(
            turn_event.metadata.take(),
            interruption,
        ));
    }

    (turn_event, turn_observability_events)
}

pub(crate) fn commit_turn_journal_workspace_and_sidecars(
    state: &mut SessionState,
    line: &str,
    result: &mut StreamResult,
    learning_snap: &TurnLearningSnapshot,
    turn_start: Instant,
) -> TurnCommitOutcome {
    let has_stalls = !result.stall_events.is_empty();
    let mut issues = TurnCommitIssues::default();
    let mut turn_persisted = state.session_id.is_none();
    if let Some((_internal_turn, trace_json)) = &result.pending_context_assembly_trace {
        cache_pending_context_assembly_trace(state, trace_json);
    }

    if let Some(journal) = state.journal.as_ref() {
        let mut workspace_written_for_turn = false;
        // Phase 1: build and append the primary turn event. The turn event is
        // the commit gate; later sidecars may fail without rolling back history.
        let (turn_event, turn_observability_events) =
            build_primary_turn_event(state, line, result, turn_start, &mut issues);

        turn_persisted = match journal.append(&turn_event) {
            Ok(()) => {
                state.last_turn_event = Some(turn_event.clone());
                enqueue_ingestion_pub(state, &turn_event);
                true
            }
            Err(error) => {
                issues.record_error("append turn event", error);
                false
            }
        };

        let mut sidecar_events = Vec::new();
        // Phase 2: stage sidecar events. They are batched later to avoid
        // per-event fsyncs, but not allowed to hide durability degradation.
        if turn_persisted {
            sidecar_events.extend(turn_observability_events);

            if let Some((_internal_turn, trace_json)) = &result.pending_context_assembly_trace {
                let assembly_event = session_journal::JournalEvent::context_assembly_recorded(
                    state.session_id.as_deref(),
                    state.turn,
                    trace_json.clone(),
                );
                sidecar_events.push(assembly_event);
            }
        }

        // Phase 3: write workspace/checkpoint state. Checkpoint journal events
        // are published only after workspace metadata can reference them.
        if turn_persisted && let Some(session_id) = state.session_id.as_deref() {
            workspace_written_for_turn = persist_workspace_and_checkpoint(
                state,
                session_id,
                result,
                &mut issues,
                &mut sidecar_events,
            );
        }

        // Phase 4: publish remaining sidecars in one append. If this fails, the
        // primary turn remains committed but the session is marked degraded.
        if turn_persisted {
            extend_runtime_sidecar_events(&mut sidecar_events, state, line, result, learning_snap);

            if !sidecar_events.is_empty()
                && let Err(error) = journal.append_bulk(&sidecar_events)
            {
                issues.record_error("append turn sidecar events", error);
                if workspace_written_for_turn
                    && let Some(session_id) = state.session_id.as_deref()
                    && let Err(workspace_error) =
                        rewrite_workspace_persistence_error(state, session_id, issues.summary())
                {
                    issues.record_error(
                        "write workspace metadata after sidecar failure",
                        workspace_error,
                    );
                }
            } else {
                for event in &sidecar_events {
                    enqueue_ingestion_pub(state, event);
                }
            }
        }
    } else if state.session_id.is_some() {
        issues.record_error("commit durable turn state", "session journal missing");
    }

    let persistence_error = issues.into_summary();
    state.session_persistence_error = persistence_error.clone();

    if has_stalls {
        session_lessons::checkpoint_lessons_from_runtime(state);
    }

    TurnCommitOutcome {
        turn_persisted,
        persistence_error,
    }
}

fn merge_interruption_metadata(
    existing: Option<serde_json::Value>,
    interruption: &serde_json::Value,
) -> serde_json::Value {
    let mut metadata = match existing {
        Some(serde_json::Value::Object(map)) => map,
        Some(value) => {
            let mut map = serde_json::Map::new();
            map.insert("previous_metadata".into(), value);
            map
        }
        None => serde_json::Map::new(),
    };
    metadata.insert("partial".into(), serde_json::json!(true));
    metadata.insert("interrupted".into(), serde_json::json!(true));
    if let Some(kind) = interruption.get("kind").and_then(|value| value.as_str()) {
        metadata.insert("interruption_kind".into(), serde_json::json!(kind));
    }
    metadata.insert("interruption".into(), interruption.clone());
    serde_json::Value::Object(metadata)
}
#[cfg(test)]
mod tests {
    use super::{
        commit_turn_journal_workspace_and_sidecars, merge_interruption_metadata,
        rewrite_workspace_persistence_error, stall_type_confidence,
    };
    use crate::cli::session::session_state::SessionState;
    use crate::cli::turn::turn_learning::analyze_chat_turn_learning;
    use astra_services::session_journal;
    use std::time::Instant;

    #[test]
    fn commit_turn_persists_turn_evaluation_event() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-turn-eval-{}", uuid::Uuid::new_v4());
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            turn: 1,
            recent_tools: vec!["git".into()],
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result("Workspace is clean.");
        result.tools_used = vec!["git".into()];
        result.tool_calls_count = 1;
        result.tool_call_records = vec![session_journal::ToolCallRecord {
            name: "git".into(),
            ok: true,
            ms: 12,
            error: None,
            input_bytes: Some(16),
            output_bytes: Some(240),
            args_preview: None,
            result_preview: Some("clean".into()),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        }];

        let learning =
            analyze_chat_turn_learning("git status", state.turn, &state.recent_tools, &result);
        commit_turn_journal_workspace_and_sidecars(
            &mut state,
            "git status",
            &mut result,
            &learning,
            Instant::now(),
        );

        let events = session_journal::read_journal(&sid).unwrap();
        let event = events
            .iter()
            .find(|event| event.event_type == session_journal::JournalEventType::TurnEvaluation)
            .expect("turn evaluation event");
        assert_eq!(event.turn, Some(1));
        let metadata = event.metadata.as_ref().expect("turn evaluation metadata");
        assert_eq!(metadata["source"], "cli_repl");
        assert_eq!(metadata["live_query"], true);
        assert_eq!(metadata["success"], true);
        assert_eq!(metadata["tool_call_count"], 1);
        assert_eq!(metadata["signal_count"], 2);
        assert_eq!(metadata["signals"][0]["kind"], "tool_error_rate");
        assert_eq!(metadata["signals"][1]["kind"], "all_tools_healthy");
    }

    #[test]
    fn interrupted_success_turn_is_marked_partial_in_journal() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-turn-partial-{}", uuid::Uuid::new_v4());
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            turn: 7,
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result(
            "[budget_exhausted] 3 tool call(s) completed. You can continue in the next message.",
        );
        result.run_id = Some("run-budget-1".into());
        result.interruption = Some(serde_json::json!({
            "kind": "budget_exhausted",
            "resumable": true,
            "tool_calls_completed": 3,
            "user_message": "[budget_exhausted] 3 tool call(s) completed. You can continue in the next message."
        }));
        result.tool_calls_count = 3;

        let learning = analyze_chat_turn_learning("continue", state.turn, &[], &result);
        commit_turn_journal_workspace_and_sidecars(
            &mut state,
            "continue",
            &mut result,
            &learning,
            Instant::now(),
        );

        let event = state.last_turn_event.as_ref().expect("turn event");
        let metadata = event.metadata.as_ref().expect("partial metadata");
        assert_eq!(metadata["partial"], true);
        assert_eq!(metadata["interrupted"], true);
        assert_eq!(metadata["interruption_kind"], "budget_exhausted");
        assert_eq!(metadata["interruption"]["resumable"], true);
        assert_eq!(metadata["run_id"], "run-budget-1");
    }

    #[test]
    fn interruption_metadata_preserves_non_object_previous_metadata() {
        let interruption = serde_json::json!({
            "kind": "budget_exhausted",
            "resumable": true,
        });
        let metadata =
            merge_interruption_metadata(Some(serde_json::json!("legacy-metadata")), &interruption);
        assert_eq!(metadata["previous_metadata"], "legacy-metadata");
        assert_eq!(metadata["partial"], true);
        assert_eq!(metadata["interruption_kind"], "budget_exhausted");
        assert_eq!(metadata["interruption"]["resumable"], true);
    }

    #[test]
    fn interrupted_turn_replay_persists_observability_and_context_trace() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-turn-replay-{}", uuid::Uuid::new_v4());
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            turn: 4,
            ..Default::default()
        };
        let partial_text =
            "[budget_exhausted] 2 tool call(s) completed. You can continue in the next message.";
        let mut result = crate::tests::stub_stream_result(partial_text);
        result.prompt_tokens = 12_345;
        result.completion_tokens = 234;
        result.llm_rounds = Some(2);
        result.tool_calls_count = 2;
        result.interruption = Some(serde_json::json!({
            "kind": "budget_exhausted",
            "resumable": true,
            "tool_calls_completed": 2,
            "user_message": partial_text,
        }));
        let mut llm_round = session_journal::JournalEvent::base_public(
            session_journal::JournalEventType::LlmRound,
            Some(&sid),
        );
        llm_round.turn = Some(4);
        llm_round.round = Some(1);
        llm_round.tokens_in = Some(12_345);
        llm_round.tokens_out = Some(234);
        llm_round.metadata = Some(serde_json::json!({
            "source": "agentic_loop",
            "finish_reason": "tool_calls",
        }));
        result.turn_observability_events = vec![llm_round];
        let mut trace = astra_turn_core::context_assembly_trace::ContextAssemblyTrace {
            turn_id: "turn-99".into(),
            session_id: sid.clone(),
            ..Default::default()
        };
        trace.tools.visible_tools = vec![
            astra_turn_core::context_assembly_trace::VisibleTool {
                tool_name: "git".into(),
                tokens: 0,
            },
            astra_turn_core::context_assembly_trace::VisibleTool {
                tool_name: "read_file".into(),
                tokens: 0,
            },
        ];
        trace.token_budget.total_used = 12_345;
        result.pending_context_assembly_trace = Some((99, trace.to_json_value()));

        let learning = analyze_chat_turn_learning("continue", state.turn, &[], &result);
        commit_turn_journal_workspace_and_sidecars(
            &mut state,
            "continue",
            &mut result,
            &learning,
            Instant::now(),
        );

        let events = session_journal::read_journal(&sid).unwrap();
        let turn_event = events
            .iter()
            .find(|event| event.event_type == session_journal::JournalEventType::Turn)
            .expect("persisted turn event");
        let metadata = turn_event.metadata.as_ref().expect("turn metadata");
        assert_eq!(metadata["partial"], true);
        assert_eq!(metadata["interruption_kind"], "budget_exhausted");
        let cached_trace = state
            .latest_context_assembly_trace
            .as_ref()
            .expect("cached context trace");
        assert_eq!(cached_trace.turn_id, "turn-99");
        assert_eq!(cached_trace.token_budget.total_used, 12_345);
    }

    #[test]
    fn commit_turn_records_persistence_error_when_journal_append_fails() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-turn-commit-journal-fail-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        std::fs::create_dir(writer.path()).unwrap();
        let mut state = SessionState {
            journal: Some(writer),
            session_id: Some(sid),
            model: Some("gpt-5".into()),
            turn: 1,
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result("hello");
        let learning = analyze_chat_turn_learning("hello", state.turn, &[], &result);

        commit_turn_journal_workspace_and_sidecars(
            &mut state,
            "hello",
            &mut result,
            &learning,
            Instant::now(),
        );

        let error = state
            .session_persistence_error
            .as_deref()
            .expect("journal append failure should degrade persistence state");
        assert!(error.contains("append turn event"), "{error}");
        assert!(
            astra_services::session_workspace::read_workspace_optional(
                state.session_id.as_deref().unwrap()
            )
            .expect("workspace lookup should not fail")
            .is_none(),
            "workspace metadata must not advance when the turn event was not journaled"
        );
    }

    #[test]
    fn commit_turn_does_not_record_missing_checkpoint_in_workspace() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-turn-commit-checkpoint-fail-{}", uuid::Uuid::new_v4());
        let workspace_dir = astra_services::session_workspace::workspace_dir_for(&sid);
        std::fs::create_dir_all(&workspace_dir).unwrap();
        std::fs::write(workspace_dir.join("checkpoints"), b"not-a-directory").unwrap();
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            turn: astra_services::session_checkpoint::CHECKPOINT_INTERVAL,
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result("hello");
        let learning = analyze_chat_turn_learning("hello", state.turn, &[], &result);

        commit_turn_journal_workspace_and_sidecars(
            &mut state,
            "hello",
            &mut result,
            &learning,
            Instant::now(),
        );

        let error = state
            .session_persistence_error
            .as_deref()
            .expect("checkpoint write failure should degrade persistence state");
        assert!(error.contains("write session checkpoint"), "{error}");
        let persisted = astra_services::session_workspace::read_workspace(&sid)
            .expect("workspace should still be written after checkpoint failure");
        assert!(
            persisted.checkpoints.is_empty(),
            "workspace must not reference a checkpoint file that was never written"
        );
        assert!(
            persisted
                .last_persistence_error
                .as_deref()
                .expect("checkpoint failure should be persisted in workspace")
                .contains("write session checkpoint")
        );
    }

    #[test]
    fn commit_turn_records_persistence_error_when_workspace_write_fails() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-turn-commit-workspace-fail-{}", uuid::Uuid::new_v4());
        let workspace_dir = astra_services::session_workspace::workspace_dir_for(&sid);
        std::fs::create_dir_all(workspace_dir.parent().unwrap()).unwrap();
        std::fs::write(&workspace_dir, b"not-a-directory").unwrap();
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid),
            model: Some("gpt-5".into()),
            turn: 1,
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result("hello");
        let learning = analyze_chat_turn_learning("hello", state.turn, &[], &result);

        commit_turn_journal_workspace_and_sidecars(
            &mut state,
            "hello",
            &mut result,
            &learning,
            Instant::now(),
        );

        let error = state
            .session_persistence_error
            .as_deref()
            .expect("workspace write failure should degrade persistence state");
        assert!(error.contains("write workspace metadata"), "{error}");
    }

    #[test]
    fn commit_turn_records_bridge_pipeline_build_error_without_rolling_back_turn() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-turn-commit-bridge-fail-{}", uuid::Uuid::new_v4());
        let journal_path = session_journal::journal_file_path(&sid);
        std::fs::create_dir_all(journal_path.parent().unwrap()).unwrap();
        std::fs::write(&journal_path, [0xff]).unwrap();
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            turn: 1,
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result("hello");
        let learning = analyze_chat_turn_learning("hello", state.turn, &[], &result);

        let outcome = commit_turn_journal_workspace_and_sidecars(
            &mut state,
            "hello",
            &mut result,
            &learning,
            Instant::now(),
        );

        assert!(
            outcome.turn_persisted,
            "journal append should still succeed after bridge event construction failed"
        );
        let error = state
            .session_persistence_error
            .as_deref()
            .expect("bridge pipeline failure should degrade persistence state");
        assert!(
            error.contains("build bridge pipeline journal events"),
            "{error}"
        );
        let persisted = astra_services::session_workspace::read_workspace(&sid)
            .expect("workspace should preserve bridge failure");
        assert!(
            persisted
                .last_persistence_error
                .as_deref()
                .unwrap_or_default()
                .contains("build bridge pipeline journal events")
        );
    }

    #[test]
    fn rewrite_workspace_persistence_error_updates_workspace_metadata() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!(
            "test-turn-commit-sidecar-workspace-error-{}",
            uuid::Uuid::new_v4()
        );
        let state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            turn: 1,
            total_prompt_tokens: 11,
            total_completion_tokens: 13,
            ..Default::default()
        };

        rewrite_workspace_persistence_error(
            &state,
            &sid,
            Some("failed to append turn sidecar events: disk full".to_string()),
        )
        .expect("workspace error rewrite should succeed");

        let persisted = astra_services::session_workspace::read_workspace(&sid)
            .expect("workspace should be written");
        assert_eq!(
            persisted.last_persistence_error.as_deref(),
            Some("failed to append turn sidecar events: disk full")
        );
        assert_eq!(persisted.turn_count, 1);
        assert_eq!(persisted.total_tokens_in, 11);
        assert_eq!(persisted.total_tokens_out, 13);
    }

    #[cfg(unix)]
    #[test]
    fn commit_turn_removes_checkpoint_artifacts_when_workspace_write_fails() {
        use std::os::unix::fs::PermissionsExt;

        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!(
            "test-turn-commit-checkpoint-rollback-{}",
            uuid::Uuid::new_v4()
        );
        let workspace_dir = astra_services::session_workspace::workspace_dir_for(&sid);
        let checkpoint_dir = workspace_dir.join("checkpoints");
        std::fs::create_dir_all(&checkpoint_dir).unwrap();
        std::fs::set_permissions(&workspace_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let checkpoint_path = workspace_dir
            .join("checkpoints")
            .join("001-turn-5-checkpoint.md");
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            turn: astra_services::session_checkpoint::CHECKPOINT_INTERVAL,
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result("hello");
        let learning = analyze_chat_turn_learning("hello", state.turn, &[], &result);

        commit_turn_journal_workspace_and_sidecars(
            &mut state,
            "hello",
            &mut result,
            &learning,
            Instant::now(),
        );
        std::fs::set_permissions(&workspace_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        let error = state
            .session_persistence_error
            .as_deref()
            .expect("workspace write failure should degrade persistence state");
        assert!(error.contains("write workspace metadata"), "{error}");
        assert!(
            !checkpoint_path.exists(),
            "checkpoint file must be removed when workspace cannot reference it"
        );
        assert!(
            astra_services::session_checkpoint::read_checkpoint_index(&sid)
                .unwrap()
                .is_empty(),
            "checkpoint index must not reference rolled-back checkpoint"
        );
        let events = session_journal::read_journal(&sid).expect("journal should remain readable");
        assert!(
            !events.iter().any(|event| matches!(
                event.event_type,
                session_journal::JournalEventType::Checkpoint
            )),
            "rolled-back checkpoint must not be published as a journal sidecar"
        );
    }

    #[test]
    fn clean_commit_clears_stale_persistence_error() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-turn-commit-clear-{}", uuid::Uuid::new_v4());
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            turn: 1,
            session_persistence_error: Some("stale".into()),
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result("hello");
        let learning = analyze_chat_turn_learning("hello", state.turn, &[], &result);

        commit_turn_journal_workspace_and_sidecars(
            &mut state,
            "hello",
            &mut result,
            &learning,
            Instant::now(),
        );

        assert!(
            state.session_persistence_error.is_none(),
            "clean commit should clear stale persistence errors"
        );
        let persisted = astra_services::session_workspace::read_workspace(&sid)
            .expect("clean commit should refresh workspace metadata");
        assert!(
            persisted.last_persistence_error.is_none(),
            "successful commit should clear persisted degradation state"
        );
    }

    #[test]
    fn stall_type_confidence_maps_known_signals() {
        assert_eq!(stall_type_confidence("sig_stall"), 1.0);
        assert_eq!(stall_type_confidence("skill_lockout"), 1.0);
        assert_eq!(stall_type_confidence("skill_lockout:review-changes"), 1.0);
        assert_eq!(stall_type_confidence("repetition_stall"), 0.0);
        assert_eq!(stall_type_confidence("unknown_type"), 0.0);
    }
}
