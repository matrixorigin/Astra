//! User-facing and stateful failure reporting for failed turns.

use std::time::Instant;

use crate::cli::auth_flow::{is_auth_error, is_llm_provider_auth_error};
use crate::cli::cli_config::cli_utils::persist_profile_last_session_or_warn;
use crate::cli::session::session_side_effects::enqueue_ingestion_pub;
use crate::cli::session::session_startup;
use crate::cli::session::session_state::SessionState;
use astra_services::session_journal;

pub(crate) async fn reconcile_and_report_turn_failure(
    state: &mut SessionState,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    line: &str,
    failure: &mut crate::TurnFailure,
    turn_start: Instant,
    ui: &mut dyn crate::cli::ui_adapter::ReplUiAdapter,
) {
    // A closed primary SSE fanout is not permission to lose a server-applied
    // user intent. The dedicated control-tail observer owns that fact until
    // either success or failure settlement consumes it.
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
                "Turn failed before durable guidance disposition reconciliation completed; canonical server history remains authoritative.",
            );
        }
        for intent in run_control.take_remotely_applied_user_intents() {
            if !failure
                .partial
                .applied_user_intents
                .iter()
                .any(|existing| existing.intent_id == intent.intent_id)
            {
                failure.partial.applied_user_intents.push(intent);
            }
        }
    }

    reconcile_failure_accounting(api, profile, &mut failure.partial).await;
    report_turn_failure(state, profile, line, failure, turn_start, ui);
}

async fn reconcile_failure_accounting(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    partial: &mut crate::PartialTurnData,
) {
    let Some(run_id) = partial
        .run_id
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(str::to_string)
    else {
        return;
    };
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let Some(token) = await_failure_reconciliation_before_deadline(
        deadline,
        crate::cli::session::session_runtime::fresh_access_token(api, profile),
    )
    .await
    .flatten() else {
        tracing::warn!(
            run_id,
            "durable failure accounting could not acquire authentication before the bounded reconciliation deadline"
        );
        return;
    };
    let mut backoff = std::time::Duration::from_millis(50);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            tracing::warn!(
                run_id,
                "durable failure accounting did not settle before the bounded reconciliation deadline"
            );
            return;
        }
        let request_timeout = remaining.min(std::time::Duration::from_secs(1));
        if let Ok(Ok(run)) =
            tokio::time::timeout(request_timeout, api.get_run(Some(&token), &run_id)).await
            && run
                .get("accounting")
                .is_some_and(|accounting| apply_durable_run_accounting(partial, accounting))
        {
            return;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            continue;
        }
        tokio::time::sleep(backoff.min(remaining)).await;
        backoff = (backoff * 2).min(std::time::Duration::from_millis(500));
    }
}

async fn await_failure_reconciliation_before_deadline<F>(
    deadline: tokio::time::Instant,
    future: F,
) -> Option<F::Output>
where
    F: std::future::Future,
{
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        return None;
    }
    tokio::time::timeout(remaining, future).await.ok()
}

fn apply_durable_run_accounting(
    partial: &mut crate::PartialTurnData,
    accounting: &serde_json::Value,
) -> bool {
    let read = |key: &str| accounting.get(key).and_then(serde_json::Value::as_u64);
    let (Some(prompt), Some(completion), Some(cache_read), Some(cache_creation)) = (
        read("prompt_tokens"),
        read("completion_tokens"),
        read("cache_read_tokens"),
        read("cache_creation_tokens"),
    ) else {
        return false;
    };
    partial.prompt_tokens = prompt;
    partial.completion_tokens = completion;
    partial.cache_read_tokens = cache_read;
    partial.cache_creation_tokens = cache_creation;
    if let Some(count) = read("tool_call_count").and_then(|value| u32::try_from(value).ok()) {
        partial.tool_calls_count = count;
    }
    partial.tool_outcomes = accounting
        .get("tool_outcomes")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok());
    true
}

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
            failure.error
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
            "tool_outcomes": failure.partial.tool_outcomes.clone(),
        }));
    }
    err_event = err_event.with_applied_user_intents(
        failure.partial.applied_user_intents.iter().map(|intent| {
            (
                intent.intent_id.as_str(),
                intent.delivery,
                intent.status,
                intent.event_index,
                intent.content.as_str(),
            )
        }),
    );
    err_event = err_event.with_run_id(failure.partial.run_id.as_deref());

    // The live continuation and restart hydration must be byte-for-byte the
    // same projection of the same durable TurnError fact. Partial assistant
    // output remains diagnostic evidence, never canonical conversation input.
    state
        .history
        .extend(crate::cli::session::session_runtime::turn_error_history_pairs(&err_event));

    if let Some(journal) = state.journal.as_ref() {
        crate::cli::cli_config::cli_utils::append_journal_event_or_warn(
            journal,
            state.session_id.as_deref(),
            &err_event,
            "turn_failure_reporting:report_turn_failure",
        );
        enqueue_ingestion_pub(state, &err_event);
        state.last_turn_event = Some(err_event.clone());

        let transcript_events = crate::cli::stream::streaming_types::root_run_transcript_events(
            state.session_id.as_deref(),
            failure.partial.run_id.as_deref(),
            &failure.partial.run_transcript_messages,
        );
        if !transcript_events.is_empty() {
            if let Err(error) = journal.append_bulk(&transcript_events) {
                let message = format!("failed to persist interrupted run transcript: {error}");
                tracing::warn!(%error, "{message}");
                ui.show_error(&format!("  {} {message}", crate::cli::theme::icon_err()));
                state.session_persistence_error =
                    Some(match state.session_persistence_error.take() {
                        Some(existing) => format!("{existing}; {message}"),
                        None => message,
                    });
            } else {
                for event in &transcript_events {
                    enqueue_ingestion_pub(state, event);
                }
            }
        }
    }

    // A final failed settlement still consumed a user-visible turn. Keep the
    // local turn cursor aligned with the turn_error written above so the next
    // bridge request cannot reuse a stale explicit session_turn.
    state.turn += 1;

    if !failure.partial.partial_text.is_empty() {
        state.last_response = Some(failure.partial.partial_text.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_durable_run_accounting, await_failure_reconciliation_before_deadline,
        reconcile_and_report_turn_failure, report_turn_failure,
    };
    use crate::cli::session::session_state::SessionState;
    use crate::tests::heavy_checkpoint_with_runtime_state;
    use astra_services::session_journal;
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn failure_reconciliation_bounds_a_never_returning_token_refresh() {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(25);
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            await_failure_reconciliation_before_deadline(
                deadline,
                std::future::pending::<Option<String>>(),
            ),
        )
        .await
        .expect("the shared failure deadline must bound token refresh");
        assert_eq!(result, None);
    }

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

    #[tokio::test]
    #[serial_test::serial]
    async fn failed_settlement_drains_closed_fanout_guidance_before_turn_error_commit() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("failed-guidance-tail-{}", uuid::Uuid::new_v4());
        let control = crate::cli::turn::local_run_control::LocalRunControl::shared();
        control.expect_remote_user_intent_disposition("intent-failed-tail", 1);
        control.record_remotely_applied_user_intent(
            crate::cli::stream::streaming_types::AppliedStreamUserIntent {
                intent_id: "intent-failed-tail".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::Applied,
                event_index: 23,
                content: "stop old work".into(),
            },
        );
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid),
            ..Default::default()
        };
        *astra_core::sync_poison::recover_mutex_lock(&state.active_turn_local_run_control) =
            Some(control);
        let mut failure = crate::TurnFailure {
            error: "callback failed".into(),
            partial: crate::PartialTurnData::default(),
        };
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let mut ui = crate::tests::TestUi::default();

        reconcile_and_report_turn_failure(
            &mut state,
            &api,
            None,
            "original",
            &mut failure,
            Instant::now(),
            &mut ui,
        )
        .await;

        let intents = state
            .last_turn_event
            .as_ref()
            .and_then(|event| event.metadata.as_ref())
            .and_then(|metadata| metadata.get("user_intents"))
            .and_then(serde_json::Value::as_array)
            .expect("durable applied guidance");
        assert_eq!(intents[0]["intent_id"], "intent-failed-tail");
        assert_eq!(intents[0]["content"], "stop old work");
        assert_eq!(
            state.history,
            vec![
                (
                    "original".to_string(),
                    "[Previous turn failed: callback failed]".to_string()
                ),
                (
                    "stop old work".to_string(),
                    "[Previous turn failed: callback failed]".to_string()
                ),
            ],
            "the online continuation must retain the same original+guidance facts as restart"
        );
    }

    #[test]
    fn durable_run_accounting_replaces_latest_frame_lower_bound() {
        let mut partial = crate::PartialTurnData {
            prompt_tokens: 7_603,
            completion_tokens: 315,
            cache_read_tokens: 34_688,
            tool_calls_count: 3,
            ..Default::default()
        };
        let accounting = serde_json::json!({
            "prompt_tokens": 47_955,
            "cache_read_tokens": 35_000,
            "cache_creation_tokens": 0,
            "completion_tokens": 650,
            "tool_call_count": 4,
            "tool_outcomes": {
                "requested": 4,
                "executed": 3,
                "succeeded": 2,
                "failed": 1,
                "rejected": 1,
                "reused": 0,
                "suppressed": 0,
                "deferred": 0
            }
        });
        assert!(apply_durable_run_accounting(&mut partial, &accounting));
        assert_eq!(partial.prompt_tokens, 47_955);
        assert_eq!(partial.cache_read_tokens, 35_000);
        assert_eq!(partial.completion_tokens, 650);
        assert_eq!(partial.tool_calls_count, 4);
        let outcomes = partial.tool_outcomes.expect("typed tool outcomes");
        assert_eq!(outcomes.requested, 4);
        assert_eq!(outcomes.executed, 3);
        assert_eq!(outcomes.rejected, 1);
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
        assert_eq!(event.turn, Some(1));
        assert_eq!(event.tools_used, Some(vec!["read_file".into()]));
        assert_eq!(event.tokens_in, Some(13));
        assert_eq!(event.tokens_out, Some(7));
        assert_eq!(event.tool_calls.as_ref().map(Vec::len), Some(2));
        assert_eq!(
            state.turn, 1,
            "failed turn settlement must advance the local turn cursor"
        );
        assert_eq!(state.history.len(), 1);
        assert_eq!(
            state.history[0],
            (
                "show session metrics".to_string(),
                "[Previous turn failed: model overloaded]".to_string()
            ),
            "live continuation must use the same canonical TurnError projection as restart"
        );
        assert_eq!(
            state.history,
            crate::cli::session::session_runtime::turn_error_history_pairs(event),
            "online and restart projections must remain byte-for-byte identical"
        );
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
    fn report_turn_failure_persists_captured_root_run_items() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-turn-failure-transcript-{}", uuid::Uuid::new_v4());
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            ..Default::default()
        };
        let failure = crate::TurnFailure {
            error: "provider stream ended".into(),
            partial: crate::PartialTurnData {
                run_id: Some("run-root-failure-1".into()),
                partial_text: "partial answer".into(),
                run_transcript_messages: vec![
                    serde_json::json!({"role": "user", "content": "review this change"}),
                    serde_json::json!({"role": "assistant", "tool_calls": [{"id": "call-1"}]}),
                    serde_json::json!({"role": "tool", "tool_call_id": "call-1", "content": "partial result"}),
                ],
                ..Default::default()
            },
        };

        report_turn_failure(
            &mut state,
            None,
            "review this change",
            &failure,
            Instant::now(),
            &mut crate::cli::ui_adapter::LineUiAdapter,
        );

        let items = session_journal::read_journal(&sid)
            .expect("journal should be readable")
            .into_iter()
            .filter_map(|event| event.transcript_item)
            .collect::<Vec<_>>();
        assert_eq!(items.len(), 3);
        assert!(items.iter().all(|item| item.run_id == "run-root-failure-1"));
        assert_eq!(
            items.iter().map(|item| item.item_seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(items[2].message["content"], "partial result");
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
        let mut state = SessionState {
            runtime_pipeline_state: Some(serde_json::json!({"previous": true})),
            runtime_compaction_state: Some(serde_json::json!({"attempt_count": 2})),
            runtime_consecutive_context_window_errors: 2,
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
            state.turn, 1,
            "transport failures without journal persistence still consume a local turn"
        );
        assert_eq!(
            state.runtime_compaction_state,
            Some(serde_json::json!({"attempt_count": 2}))
        );
        assert_eq!(state.runtime_consecutive_context_window_errors, 2);
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
        assert_eq!(
            state.turn, 1,
            "bootstrapped turn_error must advance the local turn cursor"
        );
        let events = session_journal::read_journal(&sid).unwrap();
        assert!(
            events
                .iter()
                .any(|event| event.event_type == session_journal::JournalEventType::TurnError),
            "turn error should be persisted after journal bootstrap"
        );
    }
}
