use std::sync::Arc;
use std::time::{Duration, Instant};

use super::turn_failure_reporting::report_turn_failure;
use super::turn_success::apply_turn_success_async;
use super::*;
use crate::StreamResult;

#[allow(clippy::result_large_err)]
fn fabricate_user_cancel_failure(
    reason: &str,
    incremental_state: Arc<astra_turn_core::turn_event_sink::IncrementalTurnState>,
) -> Result<StreamResult, crate::TurnFailure> {
    let snap = incremental_state.snapshot();
    Err(crate::TurnFailure {
        error: reason.to_string(),
        partial: crate::PartialTurnData {
            prompt_tokens: snap.prompt_tokens,
            completion_tokens: snap.completion_tokens,
            cache_read_tokens: snap.cache_read_tokens,
            cache_creation_tokens: snap.cache_creation_tokens,
            tool_calls_count: snap.tool_call_records.len() as u32,
            tool_call_records: snap.tool_call_records,
            tools_used: snap.tools_used,
            partial_text: snap.partial_text,
            session_id: snap.session_id,
            run_id: snap.run_id,
            ..Default::default()
        },
    })
}

#[allow(clippy::result_large_err)]
pub(crate) async fn drain_after_cancel<F>(
    stream_fut: &mut std::pin::Pin<&mut F>,
    timeout: Duration,
    source: &'static str,
    incremental_state: Arc<astra_turn_core::turn_event_sink::IncrementalTurnState>,
) -> Result<StreamResult, crate::TurnFailure>
where
    F: std::future::Future<Output = Result<StreamResult, crate::TurnFailure>>,
{
    tokio::select! {
        biased;
        r = stream_fut => r,
        _ = tokio::signal::ctrl_c() => {
            tracing::warn!(
                target: "astra::cli::cancel",
                source,
                "user force-exited via second Ctrl+C; recovering partial data from incremental state"
            );
            eprintln!("{}", "  Force-exiting (partial data recovered).".dim());
            fabricate_user_cancel_failure(
                &format!("user_interrupted ({source}, force-exit)"),
                incremental_state,
            )
        }
        _ = tokio::time::sleep(timeout) => {
            tracing::warn!(
                target: "astra::cli::cancel",
                source,
                drain_secs = timeout.as_secs(),
                "agentic loop drain timed out after cancel; recovering partial data from incremental state"
            );
            fabricate_user_cancel_failure(
                &format!(
                    "user_interrupted ({source}, drain timed out after {}s)",
                    timeout.as_secs()
                ),
                incremental_state,
            )
        }
    }
}

pub(crate) async fn apply_user_cancelled_turn(
    state: &mut SessionState,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    line: &str,
    result: Result<StreamResult, crate::TurnFailure>,
    turn_start: Instant,
    _ui: &mut dyn crate::cli::ui_adapter::ReplUiAdapter,
) {
    state.last_turn_interrupted = true;
    match result {
        Ok(stream_result) => {
            apply_turn_success_async(
                state,
                api,
                profile,
                line,
                stream_result,
                turn_start,
                &mut SilentUi,
            )
            .await;
            state.last_turn_interrupted = false;
        }
        Err(mut failure) => {
            failure.error = "[cancelled] user_interrupted (Ctrl+C)".to_string();
            report_turn_failure(state, profile, line, &failure, turn_start, &mut SilentUi);
            if failure.partial.partial_text.is_empty() {
                state.history.push((
                    line.to_string(),
                    "[Interrupted by user before any response was produced]".to_string(),
                ));
            }
        }
    }
}

struct SilentUi;

impl crate::cli::ui_adapter::ReplUiAdapter for SilentUi {
    fn show_error(&mut self, _msg: &str) {}
    fn show_warning(&mut self, _msg: &str) {}
    fn show_info(&mut self, _msg: &str) {}
    fn show_status(&mut self, _msg: &str) {}
    fn blank_line(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[serial_test::serial]
    #[tokio::test]
    async fn user_cancelled_turn_with_partial_text_preserves_user_line_in_history() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = format!("test-user-cancel-{}", uuid::Uuid::new_v4());
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            ..Default::default()
        };

        let failure = crate::TurnFailure {
            error: "stream cancelled mid-flight".into(),
            partial: crate::PartialTurnData {
                partial_text: "The first half of the answer".into(),
                ..Default::default()
            },
        };
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();

        apply_user_cancelled_turn(
            &mut state,
            &api,
            None,
            "explain LoopDispatcher",
            Err(failure),
            Instant::now(),
            &mut crate::cli::ui_adapter::LineUiAdapter,
        )
        .await;

        assert_eq!(state.history.len(), 1, "user line must be in history");
        assert_eq!(state.history[0].0, "explain LoopDispatcher");
        assert!(state.history[0].1.contains("user_interrupted"));
        assert!(state.history[0].1.contains("The first half of the answer"));
        assert!(state.last_turn_interrupted);

        let event = state
            .last_turn_event
            .as_ref()
            .expect("turn_error event written");
        let error_text = event.error.as_deref().unwrap_or_default();
        assert!(error_text.contains("user_interrupted"));
        assert!(!error_text.contains("stream cancelled mid-flight"));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn user_cancelled_turn_with_ok_outcome_persists_history_and_clears_interrupted() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = format!("test-user-cancel-ok-{}", uuid::Uuid::new_v4());
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            ..Default::default()
        };

        let mut stream_result = stub_stream_result("The first half of the answer");
        stream_result.interruption = Some(serde_json::json!({
            "kind": "UserCancelled",
            "reason": null
        }));

        let initial_turn = state.turn;
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();

        apply_user_cancelled_turn(
            &mut state,
            &api,
            None,
            "explain LoopDispatcher",
            Ok(stream_result),
            Instant::now(),
            &mut crate::cli::ui_adapter::LineUiAdapter,
        )
        .await;

        assert_eq!(state.history.len(), 1);
        assert_eq!(state.history[0].0, "explain LoopDispatcher");
        assert!(state.history[0].1.contains("The first half of the answer"));
        assert!(!state.last_turn_interrupted);
        assert_eq!(state.turn, initial_turn + 1);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn user_cancelled_turn_without_partial_text_still_pushes_user_line_to_history() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = format!("test-user-cancel-empty-{}", uuid::Uuid::new_v4());
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            ..Default::default()
        };

        let failure = crate::TurnFailure {
            error: "stream cancelled before first token".into(),
            partial: crate::PartialTurnData::default(),
        };
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();

        apply_user_cancelled_turn(
            &mut state,
            &api,
            None,
            "what's in run_lifecycle.rs",
            Err(failure),
            Instant::now(),
            &mut crate::cli::ui_adapter::LineUiAdapter,
        )
        .await;

        assert_eq!(
            state.history.len(),
            1,
            "user line must be pushed to history"
        );
        assert_eq!(state.history[0].0, "what's in run_lifecycle.rs");
        assert!(
            state.history[0]
                .1
                .contains("Interrupted by user before any response"),
        );
        assert!(state.last_turn_interrupted);

        let events = session_journal::read_journal(&sid).unwrap();
        let event = events
            .iter()
            .find(|event| {
                event.event_type == session_journal::JournalEventType::TurnError
                    && event.user_input.as_deref() == Some("what's in run_lifecycle.rs")
            })
            .expect("user-cancelled turn must reach journal");
        let error_text = event.error.as_deref().unwrap_or_default();
        assert!(error_text.contains("user_interrupted"));
    }

    #[tokio::test]
    async fn drain_after_cancel_returns_completed_future_result() {
        let incremental_state =
            Arc::new(astra_turn_core::turn_event_sink::IncrementalTurnState::default());
        let mut stream_fut = std::pin::pin!(async { Ok(stub_stream_result("drained")) });

        let result = drain_after_cancel(
            &mut stream_fut,
            Duration::from_secs(1),
            "unit-test",
            incremental_state,
        )
        .await
        .expect("ready future should win");

        assert_eq!(result.full_text, "drained");
    }

    #[tokio::test]
    async fn drain_after_cancel_timeout_recovers_incremental_state() {
        let incremental_state =
            Arc::new(astra_turn_core::turn_event_sink::IncrementalTurnState::default());
        incremental_state.add_prompt_tokens(9);
        incremental_state.add_completion_tokens(4);
        incremental_state.append_text("partial");

        let mut stream_fut = std::pin::pin!(async {
            std::future::pending::<Result<StreamResult, crate::TurnFailure>>().await
        });

        let failure = drain_after_cancel(
            &mut stream_fut,
            Duration::from_millis(1),
            "unit-test",
            incremental_state,
        )
        .await
        .expect_err("timeout branch should synthesize a failure");

        assert!(failure.error.contains("drain timed out"));
        assert_eq!(failure.partial.prompt_tokens, 9);
        assert_eq!(failure.partial.completion_tokens, 4);
        assert_eq!(failure.partial.partial_text, "partial");
    }

    #[test]
    fn incremental_state_survives_force_exit() {
        let inc = Arc::new(astra_turn_core::turn_event_sink::IncrementalTurnState::default());
        inc.add_prompt_tokens(500);
        inc.add_completion_tokens(200);
        inc.add_cache_read_tokens(50);
        inc.add_cache_creation_tokens(10);
        inc.append_text("Here is the partial response before the user");
        inc.set_session_id("sess-test-001".into());
        inc.set_run_id("run-test-abc".into());
        inc.push_tool_record(astra_services::session_journal::ToolCallRecord {
            name: "read_file".into(),
            ok: true,
            ms: 42,
            error: None,
            ..Default::default()
        });
        inc.add_tool_used("read_file");

        let result = fabricate_user_cancel_failure("user_interrupted (Ctrl+C, force-exit)", inc);

        match result {
            Ok(_) => panic!("expected TurnFailure, got Ok"),
            Err(failure) => {
                assert_eq!(failure.error, "user_interrupted (Ctrl+C, force-exit)");
                assert_eq!(failure.partial.prompt_tokens, 500);
                assert_eq!(failure.partial.completion_tokens, 200);
                assert_eq!(failure.partial.cache_read_tokens, 50);
                assert_eq!(failure.partial.cache_creation_tokens, 10);
                assert_eq!(
                    failure.partial.partial_text,
                    "Here is the partial response before the user"
                );
                assert_eq!(failure.partial.session_id.as_deref(), Some("sess-test-001"));
                assert_eq!(failure.partial.run_id.as_deref(), Some("run-test-abc"));
                assert_eq!(failure.partial.tool_calls_count, 1);
            }
        }
    }

    #[test]
    fn empty_incremental_state_produces_zero_tokens() {
        let inc = Arc::new(astra_turn_core::turn_event_sink::IncrementalTurnState::default());
        let result = fabricate_user_cancel_failure("user_interrupted (timeout)", inc);

        match result {
            Ok(_) => panic!("expected TurnFailure, got Ok"),
            Err(failure) => {
                assert_eq!(failure.partial.prompt_tokens, 0);
                assert_eq!(failure.partial.completion_tokens, 0);
                assert_eq!(failure.partial.cache_read_tokens, 0);
                assert_eq!(failure.partial.cache_creation_tokens, 0);
                assert!(failure.partial.partial_text.is_empty());
                assert_eq!(failure.partial.tool_calls_count, 0);
            }
        }
    }
}
