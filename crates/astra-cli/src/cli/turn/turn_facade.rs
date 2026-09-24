//! Thin facade for one-shot/headless chat turns.
//!
//! This centralizes two invariants that used to be duplicated at multiple
//! call sites in `command_router.rs`:
//! 1. the first attempt may include preloaded continuation messages, but a
//!    session-not-found retry must preserve them so local continuation is not lost;
//! 2. a session-not-found retry must clear the persisted "last session"
//!    pointer before retrying without a session id.

use super::turn_session_retry::{
    clear_stale_last_session_pointer, should_retry_after_session_not_found,
};
use crate::cli::chat_stream::{
    ApprovalRequestTx, BasicCliChatContext, ChatTurnParams, stream_chat_sse,
};
use crate::cli::permission_manager::PermissionManager;
use crate::cli::stream::streaming_types::{StreamResult, TurnFailure};

const SESSION_BINDING_CANCEL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

async fn cancel_exact_run_until_settled(
    api: &astra_thin_client::ThinClient,
    token: &str,
    run_id: &str,
) -> Result<(), String> {
    cancel_exact_run_until_settled_with_deadline(
        api,
        token,
        run_id,
        SESSION_BINDING_CANCEL_DEADLINE,
    )
    .await
}

async fn cancel_exact_run_until_settled_with_deadline(
    api: &astra_thin_client::ThinClient,
    token: &str,
    run_id: &str,
    budget: std::time::Duration,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + budget;
    let mut last_error = None;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            let suffix = last_error
                .map(|error: String| format!("; last cancellation error: {error}"))
                .unwrap_or_default();
            return Err(format!(
                "session identity failure cancellation did not settle run {run_id}{suffix}"
            ));
        }
        match tokio::time::timeout(remaining, api.cancel_run(Some(token), run_id)).await {
            Ok(Ok(response))
                if response
                    .get("execution_settled")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true) =>
            {
                return Ok(());
            }
            Ok(Ok(_)) => {
                last_error = None;
            }
            Ok(Err(error)) => {
                // DELETE is the idempotent cancellation operation for one
                // exact durable run. A transport/5xx failure is not proof that
                // execution is settled, so keep the lease and retry within the
                // bounded request settlement window.
                last_error = Some(error.to_string());
            }
            Err(_) => {
                return Err(format!(
                    "session identity failure cancellation timed out for run {run_id}"
                ));
            }
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        tokio::time::sleep(remaining.min(std::time::Duration::from_millis(25))).await;
    }
}

fn exact_run_id_from_turn_result(
    result: &Result<StreamResult, TurnFailure>,
    incremental_state: Option<&astra_turn_core::turn_event_sink::IncrementalTurnState>,
) -> Option<String> {
    let observed = match result {
        Ok(_) => None,
        Err(failure) => failure.partial.remote_cancel_run_id.clone(),
    };
    observed
        .filter(|run_id| !run_id.trim().is_empty())
        .or_else(|| {
            incremental_state
                .and_then(|state| state.snapshot().run_id)
                .filter(|run_id| !run_id.trim().is_empty())
        })
}

async fn settle_request_session_binding_failure(
    api: &astra_thin_client::ThinClient,
    token: &str,
    request_lease: Option<
        &crate::cli::session::session_execution_lease::RequestSessionExecutionLease,
    >,
    incremental_state: Option<&astra_turn_core::turn_event_sink::IncrementalTurnState>,
    result: Result<StreamResult, TurnFailure>,
) -> Result<StreamResult, TurnFailure> {
    settle_request_session_binding_failure_with_cancel_deadline(
        api,
        token,
        request_lease,
        incremental_state,
        result,
        SESSION_BINDING_CANCEL_DEADLINE,
    )
    .await
}

async fn settle_request_session_binding_failure_with_cancel_deadline(
    api: &astra_thin_client::ThinClient,
    token: &str,
    request_lease: Option<
        &crate::cli::session::session_execution_lease::RequestSessionExecutionLease,
    >,
    incremental_state: Option<&astra_turn_core::turn_event_sink::IncrementalTurnState>,
    result: Result<StreamResult, TurnFailure>,
    cancel_deadline: std::time::Duration,
) -> Result<StreamResult, TurnFailure> {
    let binding_failure = request_lease.and_then(|lease| lease.failure());
    let remote_cancel_required = matches!(
        &result,
        Err(failure) if failure.partial.remote_cancel_required
    );
    if binding_failure.is_none() && !remote_cancel_required {
        return result;
    }
    // A lost response stream is a client-observation failure, not user
    // authority. DELETE /chat/runs/{id} persists a User cancellation marker;
    // using it here turns a resumable transport failure into the false
    // `cancelled_by_user` child state seen by the fanout harness. Leave the
    // server-owned run under durable recovery. The request-scoped local lease
    // remains held through canonical partial commit, then drops normally:
    // server session-slot admission owns any still-active remote run.
    if binding_failure.is_none()
        && matches!(&result, Err(failure) if failure.is_clean_internal_stream_detach())
    {
        return result;
    }
    let exact_run_id = exact_run_id_from_turn_result(&result, incremental_state);
    let cancellation_error = match exact_run_id.as_deref() {
        Some(run_id) => {
            cancel_exact_run_until_settled_with_deadline(api, token, run_id, cancel_deadline)
                .await
                .err()
        }
        None => Some(
            "remote owner cleanup could not cancel the exact run because no authoritative run id was observed"
                .to_string(),
        ),
    };
    if let Some(request_lease) = request_lease {
        if cancellation_error.is_some() {
            request_lease.retain_unsettled_owner_until_process_exit();
        } else {
            request_lease.mark_remote_execution_settled();
        }
    }
    match result {
        Err(mut turn_failure) => {
            let callback_detach_without_binding =
                binding_failure.is_none() && turn_failure.partial.callback_client_detached;
            let mut error = if let Some(failure) = binding_failure {
                let mut error = failure.message;
                if turn_failure.error != error && !turn_failure.error.is_empty() {
                    error.push_str("; stream failure: ");
                    error.push_str(&turn_failure.error);
                }
                error
            } else {
                turn_failure.error.clone()
            };
            if let Some(cancellation_error) = cancellation_error.as_deref() {
                if !error.is_empty() {
                    error.push_str("; ");
                }
                error.push_str(cancellation_error);
            }
            if callback_detach_without_binding {
                note_headless_callback_cancel_outcome(
                    &mut error,
                    exact_run_id.as_deref(),
                    turn_failure.partial.session_id.as_deref(),
                    cancellation_error.as_deref(),
                );
            }
            turn_failure.error = error;
            if cancellation_error.is_none() {
                turn_failure.partial.remote_cancel_required = false;
                turn_failure.partial.remote_cancel_run_id = None;
            } else {
                turn_failure.partial.remote_cancel_required = true;
                if turn_failure.partial.remote_cancel_run_id.is_none() {
                    turn_failure.partial.remote_cancel_run_id = exact_run_id;
                }
            }
            Err(turn_failure)
        }
        Ok(result) => Err(TurnFailure {
            error: {
                let mut error = binding_failure
                    .map(|failure| failure.message)
                    .unwrap_or_else(|| "remote owner cleanup required".to_string());
                if let Some(cancellation_error) = cancellation_error.as_deref() {
                    error.push_str("; ");
                    error.push_str(cancellation_error);
                }
                error
            },
            partial: crate::PartialTurnData {
                session_id: result.session_id,
                run_id: result.run_id,
                partial_text: result.full_text,
                tools_used: result.tools_used,
                tool_call_records: result.tool_call_records,
                tool_calls_count: result.tool_calls_count,
                prompt_tokens: result.prompt_tokens,
                completion_tokens: result.completion_tokens,
                cache_read_tokens: result.cache_read_tokens,
                cache_creation_tokens: result.cache_creation_tokens,
                llm_rounds: result.llm_rounds,
                token_usage_coverage: result.token_usage_coverage,
                interruption: result.interruption,
                remote_cancel_required: cancellation_error.is_some(),
                remote_cancel_run_id: cancellation_error
                    .is_some()
                    .then_some(exact_run_id)
                    .flatten(),
                ..Default::default()
            },
        }),
    }
}

fn note_headless_callback_cancel_outcome(
    error: &mut String,
    run_id: Option<&str>,
    session_id: Option<&str>,
    cancellation_error: Option<&str>,
) {
    if !error.is_empty() {
        error.push(' ');
    }
    match cancellation_error {
        None => {
            let run_id = run_id
                .map(str::trim)
                .filter(|run_id| !run_id.is_empty())
                .unwrap_or("the exact run");
            error.push_str(&format!("Exact server run {run_id} cancellation settled."));
        }
        Some(_) => {
            error.push_str(
                &crate::cli::stream::streaming_types::headless_unconfirmed_cancel_notice(
                    session_id,
                ),
            );
        }
    }
}

/// A closed public stdout pipe is a conventional pipeline terminal only after
/// the facade has positively settled the exact durable owner. Other output
/// failures and incomplete cleanup remain hard errors.
pub(crate) fn is_settled_stdout_closure(failure: &TurnFailure) -> bool {
    failure.partial.output_transport_failure
        == Some(crate::cli::stream::streaming_types::OutputTransportFailure::Closed)
        && !failure.partial.remote_cancel_required
}

#[derive(Clone, Default)]
pub(crate) struct BasicCliTurnOptions {
    pub(crate) pre_loaded_messages: Option<Vec<serde_json::Value>>,
    pub(crate) deferred_tool_activations: Vec<astra_turn_types::DeferredToolActivation>,
    pub(crate) append_system_prompt: Option<String>,
    pub(crate) cancel_token: Option<std::sync::Arc<tokio_util::sync::CancellationToken>>,
    pub(crate) execution_time_budget: Option<crate::cli::chat_stream::ExecutionTimeBudgetClock>,
    pub(crate) incremental_state:
        Option<std::sync::Arc<astra_turn_core::turn_event_sink::IncrementalTurnState>>,
    pub(crate) request_session_execution_lease: Option<
        std::sync::Arc<crate::cli::session::session_execution_lease::RequestSessionExecutionLease>,
    >,
    pub(crate) approval_request_tx: Option<ApprovalRequestTx>,
    pub(crate) disable_session_not_found_retry: bool,
    /// Authoritative 1-based outer-session turn restored before any auxiliary
    /// inference or main bridge request is admitted.
    pub(crate) turn_index: Option<u32>,
}

struct BasicCliTurnAttempt<'a> {
    pre_loaded_messages: Option<Vec<serde_json::Value>>,
    deferred_tool_activations: &'a mut Vec<astra_turn_types::DeferredToolActivation>,
}

fn build_basic_cli_turn_params<'a>(
    ctx: &'a BasicCliChatContext<'a>,
    token: &'a str,
    session_id: Option<&'a str>,
    perm_manager: &'a mut PermissionManager,
    skill_quality_tracker: &'a mut astra_skills::quality::SkillQualityTracker,
    options: &BasicCliTurnOptions,
    attempt: BasicCliTurnAttempt<'a>,
) -> ChatTurnParams<'a> {
    let mut params =
        ChatTurnParams::basic_cli(ctx, token, session_id, perm_manager, skill_quality_tracker);
    params.pre_loaded_messages = attempt.pre_loaded_messages;
    params.deferred_tool_activations = Some(attempt.deferred_tool_activations);
    params.append_system_prompt = options.append_system_prompt.clone();
    params.cancel_token = options.cancel_token.clone();
    params.execution_time_budget = options.execution_time_budget.clone();
    params.incremental_state = options.incremental_state.clone();
    params.request_session_execution_lease = options.request_session_execution_lease.clone();
    params.approval_request_tx = options.approval_request_tx.clone();
    if let Some(turn_index) = options.turn_index {
        params.turn_index = turn_index.max(1);
    }
    params
}

fn should_retry_without_session(
    error: &str,
    session_id: Option<&str>,
    retry_disabled: bool,
    request_lease_present: bool,
) -> bool {
    !retry_disabled
        && !request_lease_present
        && should_retry_after_session_not_found(error, session_id.is_some())
}

fn retry_pre_loaded_messages(
    pre_loaded_messages: &Option<Vec<serde_json::Value>>,
) -> Option<Vec<serde_json::Value>> {
    pre_loaded_messages.as_deref().map(|messages| {
        crate::cli::history_work::clone_json_history(
            astra_core::history_work::HistoryWorkSite::CliTurnRetryHistoryClone,
            messages,
        )
    })
}

pub(crate) async fn execute_basic_cli_turn<'a>(
    ctx: &'a BasicCliChatContext<'a>,
    token: &'a str,
    session_id: Option<&'a str>,
    profile: Option<&str>,
    perm_manager: &'a mut PermissionManager,
    skill_quality_tracker: &'a mut astra_skills::quality::SkillQualityTracker,
    mut options: BasicCliTurnOptions,
) -> Result<StreamResult, TurnFailure> {
    let pre_loaded_messages = options.pre_loaded_messages.take();
    let retry_messages = retry_pre_loaded_messages(&pre_loaded_messages);
    let mut deferred_tool_activations = std::mem::take(&mut options.deferred_tool_activations);
    let params = build_basic_cli_turn_params(
        ctx,
        token,
        session_id,
        perm_manager,
        skill_quality_tracker,
        &options,
        BasicCliTurnAttempt {
            pre_loaded_messages,
            deferred_tool_activations: &mut deferred_tool_activations,
        },
    );
    let first = settle_request_session_binding_failure(
        ctx.api,
        token,
        options.request_session_execution_lease.as_deref(),
        options.incremental_state.as_deref(),
        stream_chat_sse(params).await,
    )
    .await;
    match first {
        Err(err)
            if should_retry_without_session(
                &err.error,
                session_id,
                options.disable_session_not_found_retry,
                options.request_session_execution_lease.is_some(),
            ) =>
        {
            if let Some(stale_session_id) = session_id
                && let Err(clear_error) =
                    clear_stale_last_session_pointer(profile, stale_session_id)
            {
                tracing::warn!(
                    error = %clear_error,
                    session_id = ?stale_session_id,
                    "failed to clear stale last-session pointer before retrying without session id"
                );
            }
            let retry = stream_chat_sse(build_basic_cli_turn_params(
                ctx,
                token,
                None,
                perm_manager,
                skill_quality_tracker,
                &options,
                BasicCliTurnAttempt {
                    pre_loaded_messages: retry_messages,
                    deferred_tool_activations: &mut deferred_tool_activations,
                },
            ))
            .await;
            settle_request_session_binding_failure(
                ctx.api,
                token,
                options.request_session_execution_lease.as_deref(),
                options.incremental_state.as_deref(),
                retry,
            )
            .await
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BasicCliTurnOptions, cancel_exact_run_until_settled, is_settled_stdout_closure,
        retry_pre_loaded_messages, settle_request_session_binding_failure,
        settle_request_session_binding_failure_with_cancel_deadline, should_retry_without_session,
    };

    #[test]
    fn first_attempt_consumes_preloaded_messages_once() {
        let mut options = BasicCliTurnOptions {
            pre_loaded_messages: Some(vec![serde_json::json!({"role": "user", "content": "hi"})]),
            ..Default::default()
        };

        let first = options.pre_loaded_messages.take();
        let second = options.pre_loaded_messages.take();

        assert_eq!(first.as_ref().map(Vec::len), Some(1));
        assert!(
            second.is_none(),
            "preloaded messages should only be sent once"
        );
    }

    #[test]
    fn retry_without_session_requires_not_found_error_and_session_id() {
        assert!(should_retry_without_session(
            "session not found: 1234",
            Some("1234"),
            false,
            false,
        ));
        assert!(!should_retry_without_session(
            "session not found: 1234",
            None,
            false,
            false,
        ));
        assert!(!should_retry_without_session(
            "session not found: 1234",
            Some("1234"),
            true,
            false,
        ));
        assert!(!should_retry_without_session(
            "rate limited",
            Some("1234"),
            false,
            false,
        ));
        assert!(!should_retry_without_session(
            "session not found: 1234",
            Some("1234"),
            false,
            true,
        ));
    }

    #[test]
    fn session_not_found_retry_replays_preloaded_messages() {
        let original = Some(vec![
            serde_json::json!({"role": "assistant", "content": "previous answer"}),
        ]);

        let retry = retry_pre_loaded_messages(&original);

        assert_eq!(retry, original);
    }

    #[tokio::test]
    async fn binding_cleanup_failure_preserves_cumulative_stream_telemetry() {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap();
        let first = format!("telemetry-bind-first-{}", uuid::Uuid::new_v4());
        let second = format!("telemetry-bind-second-{}", uuid::Uuid::new_v4());
        let holder =
            crate::cli::session::session_execution_lease::RequestSessionExecutionLease::new(Some(
                &first,
            ))
            .unwrap();
        holder.bind(&second).unwrap_err();
        let result = crate::cli::stream::streaming_types::StreamResult {
            llm_rounds: Some(7),
            tool_calls_count: 11,
            token_usage_coverage: astra_turn_core::chat_turn_sse_dispatch::TokenUsageCoverage {
                attempts: 7,
                provider_reported: 6,
                unavailable: 1,
            },
            ..Default::default()
        };

        let failure = settle_request_session_binding_failure(
            &api,
            "token",
            Some(holder.as_ref()),
            None,
            Ok(result),
        )
        .await
        .expect_err("cleanup failure must become a TurnFailure");

        assert_eq!(failure.partial.llm_rounds, Some(7));
        assert_eq!(failure.partial.tool_calls_count, 11);
        assert_eq!(failure.partial.token_usage_coverage.attempts, 7);
        assert_eq!(failure.partial.token_usage_coverage.provider_reported, 6);
        assert_eq!(failure.partial.token_usage_coverage.unavailable, 1);
    }

    #[tokio::test]
    async fn callback_detach_headless_cancel_success_reports_settled_run() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/chat/runs/run-callback"))
            .and(header("authorization", "Bearer token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "run_id": "run-callback",
                "status": "cancelled",
                "execution_settled": true,
            })))
            .expect(1)
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let settled = settle_request_session_binding_failure(
            &api,
            "token",
            None,
            None,
            Err(crate::cli::stream::streaming_types::TurnFailure {
                error: crate::cli::stream::stream_render::edge_callback_detach_message(
                    "approval",
                    "after bounded transport retries",
                    &"HTTP error: error sending request",
                ),
                partial: crate::PartialTurnData {
                    session_id: Some("sess-active".into()),
                    callback_client_detached: true,
                    remote_cancel_required: true,
                    remote_cancel_run_id: Some("run-callback".into()),
                    ..Default::default()
                },
            }),
        )
        .await
        .unwrap_err();

        assert!(settled.error.contains("This client detached"));
        assert!(
            settled
                .error
                .contains("Exact server run run-callback cancellation settled.")
        );
        assert!(!settled.error.contains("astra session cancel"));
        assert!(!settled.error.contains("not confirmed"));
        assert!(!settled.error.contains("will be cancelled"));
        assert!(!settled.partial.remote_cancel_required);
    }

    #[tokio::test]
    async fn callback_detach_headless_cancel_failure_keeps_explicit_recovery() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/chat/runs/run-callback"))
            .and(header("authorization", "Bearer token"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let unsettled = settle_request_session_binding_failure_with_cancel_deadline(
            &api,
            "token",
            None,
            None,
            Err(crate::cli::stream::streaming_types::TurnFailure {
                error: crate::cli::stream::stream_render::edge_callback_detach_message(
                    "approval",
                    "after bounded transport retries",
                    &"HTTP error: error sending request",
                ),
                partial: crate::PartialTurnData {
                    session_id: Some("sess-active".into()),
                    callback_client_detached: true,
                    remote_cancel_required: true,
                    remote_cancel_run_id: Some("run-callback".into()),
                    ..Default::default()
                },
            }),
            std::time::Duration::from_millis(75),
        )
        .await
        .unwrap_err();

        assert!(unsettled.error.contains("This client detached"));
        assert!(unsettled.error.contains(
            "Exact server run cancellation could not be confirmed; the run may still be active."
        ));
        assert!(
            !unsettled
                .error
                .contains("The server run was not cancelled.")
        );
        let cancel_at = unsettled
            .error
            .find("astra session cancel sess-active")
            .expect("cancel command");
        let resume_at = unsettled
            .error
            .find("astra --resume sess-active")
            .expect("resume after session cancel");
        assert!(cancel_at < resume_at);
        assert!(!unsettled.error.contains("cancellation settled."));
        assert!(unsettled.partial.remote_cancel_required);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn internal_stream_failure_does_not_persist_user_cancellation() {
        use wiremock::MockServer;

        let (_sessions, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("stream-failure-{}", uuid::Uuid::new_v4());
        let holder =
            crate::cli::session::session_execution_lease::RequestSessionExecutionLease::new(Some(
                &session_id,
            ))
            .unwrap();
        let server = MockServer::start().await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let failure = settle_request_session_binding_failure(
            &api,
            "token",
            Some(holder.as_ref()),
            None,
            Err(crate::cli::stream::streaming_types::TurnFailure {
                error: "The model response connection failed.".to_string(),
                partial: crate::PartialTurnData {
                    session_id: Some(session_id.clone()),
                    interruption: Some(serde_json::json!({"kind": "stream_transport"})),
                    remote_cancel_required: true,
                    remote_cancel_run_id: Some("run-stream-failure".to_string()),
                    ..Default::default()
                },
            }),
        )
        .await
        .expect_err("the original stream failure remains a hard error");

        assert_eq!(failure.error, "The model response connection failed.");
        assert!(failure.partial.remote_cancel_required);
        assert!(
            astra_services::session_journal::SessionExecutionLease::try_acquire(&session_id)
                .is_err(),
            "the caller still owns the local lease through canonical partial commit"
        );
        drop(holder);
        assert!(
            astra_services::session_journal::SessionExecutionLease::try_acquire(&session_id)
                .is_ok(),
            "an internal stream failure must not quarantine the local lock for process lifetime"
        );
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "internal stream cleanup must not call the user cancellation endpoint"
        );
    }

    #[tokio::test]
    async fn stdout_closure_cancels_exact_run_once_before_becoming_pipeline_terminal() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/chat/runs/run-output-closed"))
            .and(header("authorization", "Bearer token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "run_id": "run-output-closed",
                "status": "cancelled",
                "execution_settled": true,
            })))
            .expect(1)
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let settled = settle_request_session_binding_failure(
            &api,
            "token",
            None,
            None,
            Err(crate::cli::stream::streaming_types::TurnFailure {
                error: "stdout output transport closed by its consumer".to_string(),
                partial: crate::PartialTurnData {
                    remote_cancel_required: true,
                    remote_cancel_run_id: Some("run-output-closed".to_string()),
                    output_transport_failure: Some(
                        crate::cli::stream::streaming_types::OutputTransportFailure::Closed,
                    ),
                    ..Default::default()
                },
            }),
        )
        .await
        .unwrap_err();

        assert!(is_settled_stdout_closure(&settled));
        assert!(!settled.partial.remote_cancel_required);
        assert_eq!(settled.partial.remote_cancel_run_id, None);
    }

    #[test]
    fn output_failure_or_unsettled_owner_remains_a_hard_error() {
        use crate::cli::stream::streaming_types::{
            OutputTransportFailure, PartialTurnData, TurnFailure,
        };

        let failed = TurnFailure {
            error: "stdout output transport failed".into(),
            partial: PartialTurnData {
                output_transport_failure: Some(OutputTransportFailure::Failed),
                ..Default::default()
            },
        };
        assert!(!is_settled_stdout_closure(&failed));

        let unsettled = TurnFailure {
            error: "stdout closed; cancellation failed".into(),
            partial: PartialTurnData {
                output_transport_failure: Some(OutputTransportFailure::Closed),
                remote_cancel_required: true,
                remote_cancel_run_id: Some("run-unsettled".into()),
                ..Default::default()
            },
        };
        assert!(!is_settled_stdout_closure(&unsettled));

        // Facade exits must never infer output ownership from a process-global
        // closed sink. A simultaneous hard runtime error has no typed output
        // cause and therefore remains the command's non-141 failure.
        let concurrent_hard_error = TurnFailure {
            error: "provider authentication failed while stdout was closed".into(),
            partial: PartialTurnData::default(),
        };
        assert!(!is_settled_stdout_closure(&concurrent_hard_error));
    }

    #[tokio::test]
    async fn binding_failure_cancels_the_exact_run_and_reports_settled_failure() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/chat/runs/run-binding-failure"))
            .and(header("authorization", "Bearer token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "run_id": "run-binding-failure",
                "execution_settled": true,
            })))
            .expect(1)
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let first = format!("bind-failure-first-{}", uuid::Uuid::new_v4());
        let second = format!("bind-failure-second-{}", uuid::Uuid::new_v4());
        let holder =
            crate::cli::session::session_execution_lease::RequestSessionExecutionLease::new(Some(
                &first,
            ))
            .unwrap();
        holder.bind(&second).unwrap_err();

        let settled = settle_request_session_binding_failure(
            &api,
            "token",
            Some(holder.as_ref()),
            None,
            Err(crate::cli::stream::streaming_types::TurnFailure {
                error: "stream cancelled after identity failure".to_string(),
                partial: crate::PartialTurnData {
                    interruption: Some(serde_json::json!({"kind": "stream_transport"})),
                    remote_cancel_required: true,
                    remote_cancel_run_id: Some("run-binding-failure".to_string()),
                    ..Default::default()
                },
            }),
        )
        .await
        .unwrap_err();

        assert!(settled.error.contains("changed within one request"));
        assert!(!settled.partial.remote_cancel_required);
        assert_eq!(settled.partial.remote_cancel_run_id, None);
    }

    #[tokio::test]
    async fn cancel_transport_failure_preserves_unsettled_remote_owner_and_session_lease() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/chat/runs/run-unsettled"))
            .and(header("authorization", "Bearer token"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let first = format!("bind-unsettled-first-{}", uuid::Uuid::new_v4());
        let second = format!("bind-unsettled-second-{}", uuid::Uuid::new_v4());
        let holder =
            crate::cli::session::session_execution_lease::RequestSessionExecutionLease::new(Some(
                &first,
            ))
            .unwrap();
        holder.bind(&second).unwrap_err();

        let unsettled = settle_request_session_binding_failure_with_cancel_deadline(
            &api,
            "token",
            Some(holder.as_ref()),
            None,
            Err(crate::cli::stream::streaming_types::TurnFailure {
                error: "stream cancelled after identity failure".to_string(),
                partial: crate::PartialTurnData {
                    remote_cancel_required: true,
                    remote_cancel_run_id: Some("run-unsettled".to_string()),
                    ..Default::default()
                },
            }),
            std::time::Duration::from_millis(75),
        )
        .await
        .unwrap_err();

        assert!(unsettled.partial.remote_cancel_required);
        assert_eq!(
            unsettled.partial.remote_cancel_run_id.as_deref(),
            Some("run-unsettled")
        );
        drop(holder);
        assert!(
            astra_services::session_journal::SessionExecutionLease::try_acquire(&first).is_err(),
            "an unsettled remote owner must retain its lease after the request returns"
        );
    }

    #[tokio::test]
    async fn exact_cancel_retries_transient_failure_until_settled_while_lease_is_held() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

        #[derive(Clone)]
        struct SettleOnSecondCall(std::sync::Arc<AtomicUsize>);

        impl Respond for SettleOnSecondCall {
            fn respond(&self, _request: &Request) -> ResponseTemplate {
                let call = self.0.fetch_add(1, Ordering::SeqCst) + 1;
                if call == 1 {
                    return ResponseTemplate::new(503);
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "run_id": "run-session-binding",
                    "execution_settled": true,
                }))
            }
        }

        let server = MockServer::start().await;
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        Mock::given(method("DELETE"))
            .and(path("/chat/runs/run-session-binding"))
            .and(header("authorization", "Bearer token"))
            .respond_with(SettleOnSecondCall(calls.clone()))
            .expect(2)
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let session_id = format!("cancel-lease-lifetime-{}", uuid::Uuid::new_v4());
        let holder =
            crate::cli::session::session_execution_lease::RequestSessionExecutionLease::new(Some(
                &session_id,
            ))
            .unwrap();

        let task = tokio::spawn(async move {
            let _holder = holder;
            cancel_exact_run_until_settled(&api, "token", "run-session-binding").await
        });
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        assert!(
            astra_services::session_journal::SessionExecutionLease::try_acquire(&session_id)
                .is_err(),
            "the session lease must remain held throughout cancel-and-settle"
        );
        task.await.unwrap().unwrap();
        astra_services::session_journal::SessionExecutionLease::try_acquire(&session_id)
            .expect("settled request releases its lease");
    }
}
