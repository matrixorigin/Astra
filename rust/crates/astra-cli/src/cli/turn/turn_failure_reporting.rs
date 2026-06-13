//! User-facing and stateful failure reporting for failed turns.

use std::time::Instant;

use crate::cli::auth_flow::{is_auth_error, is_llm_provider_auth_error};
use crate::cli::cli_config::cli_utils::persist_profile_last_session_or_warn;
use crate::cli::session::session_side_effects::enqueue_ingestion_pub;
use crate::cli::session::session_startup;
use crate::cli::session::session_state::SessionState;
use astra_services::session_journal;

pub(crate) fn report_turn_failure(
    state: &mut SessionState,
    profile: Option<&str>,
    line: &str,
    failure: &crate::TurnFailure,
    turn_start: Instant,
    ui: &mut dyn crate::cli::ui_adapter::ReplUiAdapter,
) {
    if is_llm_provider_auth_error(&failure.error) {
        ui.show_error("  LLM provider credentials invalid — check model API key configuration.");
    } else if is_auth_error(&failure.error) {
        ui.show_error("  Session expired. Run /login to refresh.");
    } else {
        ui.show_error(&format!(
            "  {} {}",
            crate::cli::theme::icon_err(),
            &failure.error
        ));
    }

    if state.journal.is_none()
        && let Some(sid) = failure
            .partial
            .session_id
            .as_deref()
            .filter(|session_id| !session_id.is_empty())
    {
        session_startup::initialize_journal_pub(state, sid);
        persist_profile_last_session_or_warn(
            profile,
            sid,
            "turn_failure_reporting:report_turn_failure",
        );
        state.set_session_id(sid.to_string());
    }

    super::turn_runtime_state::update_from_turn_failure(state, failure);

    if let Some(journal) = state.journal.as_ref() {
        let mut err_event = session_journal::JournalEvent::turn_error(
            state.session_id.as_deref(),
            state.turn + 1,
            astra_core::model_override::normalize_model_override(state.model.as_deref()),
            line,
            &failure.error,
            turn_start.elapsed().as_millis() as u64,
        );

        crate::cli::stream::streaming_types::apply_partial_turn_data_to_error_event(
            &mut err_event,
            &failure.partial,
        );
        {
            let error_kind = astra_core::ClassifiedError::from(failure.error.clone()).kind;
            err_event.metadata = Some(serde_json::json!({
                "error_kind": error_kind.as_str(),
                "retryable": error_kind.is_retryable(),
                "guidance": error_kind.guidance(),
                "stall_count": failure.partial.stall_events.len(),
                "verdict_count": failure.partial.verdict_events.len(),
                "has_checkpoint": failure.partial.last_heavy_checkpoint.is_some(),
                "partial_tokens_in": failure.partial.prompt_tokens,
                "partial_tokens_out": failure.partial.completion_tokens,
                "partial_cache_read_tokens": failure.partial.cache_read_tokens,
                "partial_cache_creation_tokens": failure.partial.cache_creation_tokens,
                "partial_tool_calls": failure.partial.tool_calls_count,
            }));
        }
        err_event = err_event.with_run_id(failure.partial.run_id.as_deref());

        crate::cli::cli_config::cli_utils::append_journal_event_or_warn(
            journal,
            state.session_id.as_deref(),
            &err_event,
            "turn_failure_reporting:report_turn_failure",
        );
        enqueue_ingestion_pub(state, &err_event);
        state.last_turn_event = Some(err_event);
    }

    if !failure.partial.partial_text.is_empty() {
        let partial_with_note = format!(
            "[Interrupted: {}]\n\n{}",
            failure.error, failure.partial.partial_text
        );
        state.history.push((line.to_string(), partial_with_note));
        state.last_response = Some(failure.partial.partial_text.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::report_turn_failure;
    use crate::cli::session::session_state::SessionState;
    use astra_pipeline::step_protocol::{ExecutionCursor, StepCheckpoint};
    use astra_services::session_journal;
    use std::time::Instant;

    fn tool_call_record(
        name: &str,
        ok: bool,
        result_preview: Option<&str>,
    ) -> session_journal::ToolCallRecord {
        session_journal::ToolCallRecord {
            name: name.into(),
            ok,
            ms: 0,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
            result_preview: result_preview.map(str::to_string),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        }
    }

    fn heavy_checkpoint_with_runtime_state(
        pipeline_state: serde_json::Value,
        compaction_state: serde_json::Value,
        consecutive_context_window_errors: u32,
    ) -> StepCheckpoint {
        let mut heavy = match StepCheckpoint::heavy(
            "session-turn-1".to_string(),
            "task-1".to_string(),
            "agent-1".to_string(),
            ExecutionCursor::default(),
        ) {
            StepCheckpoint::Heavy(heavy) => *heavy,
            StepCheckpoint::Light(_) => unreachable!("heavy checkpoint constructor returned light"),
        };
        heavy.pipeline_state = Some(pipeline_state);
        heavy.compaction_state = Some(compaction_state);
        heavy.consecutive_context_window_errors = consecutive_context_window_errors;
        StepCheckpoint::Heavy(Box::new(heavy))
    }

    #[test]
    fn report_turn_failure_persists_filtered_partial_metrics() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-turn-failure-{}", uuid::Uuid::new_v4());
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            ..Default::default()
        };
        let failure = crate::TurnFailure {
            error: "model overloaded".into(),
            partial: crate::PartialTurnData {
                tool_call_records: vec![
                    tool_call_record(
                        "bash",
                        false,
                        Some("Skipped: the skill already completed this work."),
                    ),
                    tool_call_record("read_file", true, Some("contents")),
                ],
                tools_used: vec!["read_file".into()],
                run_id: Some("run-failure-1".into()),
                prompt_tokens: 13,
                completion_tokens: 7,
                tool_calls_count: 1,
                partial_text: "Partial analysis".into(),
                ..Default::default()
            },
        };

        report_turn_failure(
            &mut state,
            None,
            "show session metrics",
            &failure,
            Instant::now(),
            &mut crate::cli::ui_adapter::LineUiAdapter,
        );

        let event = state.last_turn_event.as_ref().expect("turn_error event");
        assert_eq!(event.tool_count, Some(1));
        assert_eq!(event.tools_used, Some(vec!["read_file".into()]));
        assert_eq!(event.tokens_in, Some(13));
        assert_eq!(event.tokens_out, Some(7));
        assert_eq!(event.tool_calls.as_ref().map(Vec::len), Some(2));
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.last_response.as_deref(), Some("Partial analysis"));

        let events = session_journal::read_journal(&sid).unwrap();
        let persisted = events.last().expect("persisted turn_error");
        assert_eq!(persisted.tool_count, Some(1));
        assert_eq!(persisted.tools_used, Some(vec!["read_file".into()]));
        assert_eq!(persisted.tool_calls.as_ref().map(Vec::len), Some(2));
        assert_eq!(
            persisted.metadata.as_ref().unwrap()["run_id"],
            "run-failure-1"
        );
    }

    #[test]
    fn report_turn_failure_updates_runtime_recovery_state_from_partial_checkpoint() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-turn-failure-runtime-{}", uuid::Uuid::new_v4());
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid),
            runtime_pipeline_state: Some(serde_json::json!({"old": true})),
            runtime_compaction_state: Some(serde_json::json!({"old": true})),
            runtime_consecutive_context_window_errors: 9,
            ..Default::default()
        };
        let failure = crate::TurnFailure {
            error: "context window exceeded".into(),
            partial: crate::PartialTurnData {
                last_heavy_checkpoint: Some(heavy_checkpoint_with_runtime_state(
                    serde_json::json!({"stats": {"ema": 0.91}}),
                    serde_json::json!({
                        "attempt_count": 5,
                        "cumulative_tokens_freed": 21000,
                        "last_tokens_freed": 1000,
                        "last_was_insufficient": true,
                    }),
                    3,
                )),
                ..Default::default()
            },
        };

        report_turn_failure(
            &mut state,
            None,
            "continue",
            &failure,
            Instant::now(),
            &mut crate::cli::ui_adapter::LineUiAdapter,
        );

        assert_eq!(
            state.runtime_pipeline_state,
            Some(serde_json::json!({"stats": {"ema": 0.91}}))
        );
        assert_eq!(
            state.runtime_compaction_state,
            Some(serde_json::json!({
                "attempt_count": 5,
                "cumulative_tokens_freed": 21000,
                "last_tokens_freed": 1000,
                "last_was_insufficient": true,
            }))
        );
        assert_eq!(state.runtime_consecutive_context_window_errors, 3);
    }

    #[test]
    fn report_turn_failure_preserves_runtime_recovery_state_without_partial_checkpoint() {
        let idem_key = astra_pipeline::step_protocol::IdempotencyKey::semantic(
            "read_file",
            &serde_json::json!({"path": "src/lib.rs"}),
        );
        let mut idempotency_cache = astra_pipeline::step_protocol::InMemoryIdempotencyCache::new();
        idempotency_cache.record(
            &idem_key,
            astra_pipeline::step_protocol::CachedToolResult {
                tool_name: "read_file".into(),
                output: "cached contents".into(),
                is_error: false,
                cached_at: 1,
                context_signature: None,
            },
        );
        let mut state = SessionState {
            runtime_pipeline_state: Some(serde_json::json!({"previous": true})),
            runtime_compaction_state: Some(serde_json::json!({"attempt_count": 2})),
            runtime_consecutive_context_window_errors: 2,
            runtime_idempotency_cache: Some(idempotency_cache),
            ..Default::default()
        };
        let failure = crate::TurnFailure {
            error: "network reset before checkpoint".into(),
            partial: crate::PartialTurnData::default(),
        };

        report_turn_failure(
            &mut state,
            None,
            "continue",
            &failure,
            Instant::now(),
            &mut crate::cli::ui_adapter::LineUiAdapter,
        );

        assert_eq!(
            state.runtime_pipeline_state,
            Some(serde_json::json!({"previous": true}))
        );
        assert_eq!(
            state.runtime_compaction_state,
            Some(serde_json::json!({"attempt_count": 2}))
        );
        assert_eq!(state.runtime_consecutive_context_window_errors, 2);
        assert_eq!(
            state
                .runtime_idempotency_cache
                .as_ref()
                .and_then(|cache| cache.check(&idem_key))
                .expect("restored cache should survive no-checkpoint failure")
                .output,
            "cached contents"
        );
    }

    #[test]
    #[serial_test::serial]
    fn report_turn_failure_bootstraps_missing_journal_from_partial_session_id() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("bootstrap-turn-failure-{}", uuid::Uuid::new_v4());
        let mut state = SessionState {
            model: Some("gpt-5".into()),
            ..Default::default()
        };
        let failure = crate::TurnFailure {
            error: "transport reset".into(),
            partial: crate::PartialTurnData {
                session_id: Some(sid.clone()),
                partial_text: "partial".into(),
                ..Default::default()
            },
        };

        report_turn_failure(
            &mut state,
            None,
            "retry",
            &failure,
            Instant::now(),
            &mut crate::cli::ui_adapter::LineUiAdapter,
        );

        assert!(
            state.journal.is_some(),
            "journal should bootstrap from partial sid"
        );
        assert_eq!(state.session_id.as_deref(), Some(sid.as_str()));
        let events = session_journal::read_journal(&sid).unwrap();
        assert!(
            events
                .iter()
                .any(|event| event.event_type == session_journal::JournalEventType::TurnError),
            "turn error should be persisted after journal bootstrap"
        );
    }
}
