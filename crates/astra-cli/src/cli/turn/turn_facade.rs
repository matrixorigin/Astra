//! Thin facade for one-shot/headless chat turns.
//!
//! One admitted conversation stays bound to its selected session. A missing
//! session is a failure, not authority to create another and replay its input.
use crate::cli::chat_stream::{
    ApprovalRequestTx, BasicCliChatContext, ChatTurnParams, stream_chat_sse,
};
use crate::cli::permission_manager::PermissionManager;
use crate::cli::stream::streaming_types::{StreamResult, TurnFailure};

const SESSION_BINDING_CANCEL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

#[cfg(test)]
async fn cancel_exact_run_until_settled(
    api: &astra_thin_client::ThinClient,
    token: &str,
    run_id: &str,
) -> Result<(), String> {
    cancel_exact_run_until_settled_with_deadline(
        api,
        token,
        run_id,
        None,
        tokio::time::Instant::now() + SESSION_BINDING_CANCEL_DEADLINE,
    )
    .await
}

/// Resolve the cancellation bearer for one captured owner. `rejected` is
/// `Some` only for the single reactive refresh after a 401 so the checked
/// refresh persistence cannot loop. Every wait uses the same absolute deadline.
/// Owner drift or an unrefreshable source fails closed: never fall back to
/// another binding, profile, or environment bearer.
async fn owner_cancel_bearer(
    api: &astra_thin_client::ThinClient,
    owner: &crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot,
    rejected: Option<&str>,
    deadline: tokio::time::Instant,
) -> Result<String, String> {
    if !owner.is_current() {
        return Err("captured owner is no longer current".to_string());
    }
    let resolved = tokio::time::timeout_at(
        deadline,
        crate::cli::session::session_runtime::owner_access_token(api, owner, rejected),
    )
    .await
    .map_err(|_| "bearer resolution timed out".to_string())?;
    if !owner.is_current() {
        return Err("captured owner changed during bearer resolution".to_string());
    }
    resolved
        .ok_or_else(|| "captured owner credentials are unavailable or unrefreshable".to_string())
}

pub(crate) async fn cancel_exact_run_until_settled_with_deadline(
    api: &astra_thin_client::ThinClient,
    token: &str,
    run_id: &str,
    owner: Option<&crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot>,
    deadline: tokio::time::Instant,
) -> Result<(), String> {
    // The captured owner, when present, supplies every bearer for the exact
    // cancellation request; an inherited transport provider must not replace
    // it with another binding at dispatch. A `None` owner keeps the original
    // explicit token and cannot borrow a profile.
    let api = api.clone().without_bearer_provider();
    let mut last_error = None;
    // At most one reactive refresh/repost after a 401 across the whole loop.
    let mut reactive_refresh_used = false;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            let suffix = last_error
                .map(|error: String| format!("; last cancellation error: {error}"))
                .unwrap_or_default();
            return Err(format!(
                "exact-run cancellation did not settle run {run_id}{suffix}"
            ));
        }
        let attempt_token = match owner {
            Some(owner) => match owner_cancel_bearer(&api, owner, None, deadline).await {
                Ok(resolved) => resolved,
                Err(reason) => {
                    return Err(format!(
                        "exact-run cancellation for run {run_id} failed closed: {reason}"
                    ));
                }
            },
            None => token.to_owned(),
        };
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("cancellation timed out for run {run_id}"));
        }
        match tokio::time::timeout_at(deadline, api.cancel_run(Some(&attempt_token), run_id)).await
        {
            Ok(Ok(response))
                if response
                    .get("execution_settled")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true) =>
            {
                return Ok(());
            }
            Ok(Ok(_)) => {
                // The server accepted the cancellation but execution is not
                // settled yet; keep polling within the same deadline.
                last_error = None;
            }
            Ok(Err(error)) => match &error {
                astra_thin_client::ThinClientError::Api { status, .. }
                    if *status == reqwest::StatusCode::UNAUTHORIZED =>
                {
                    let Some(owner) = owner else {
                        return Err(format!(
                            "exact-run cancellation for run {run_id} was rejected with 401 and remains unsettled; last error: {error}"
                        ));
                    };
                    if reactive_refresh_used {
                        return Err(format!(
                            "exact-run cancellation for run {run_id} was rejected with 401 again after its one refresh and remains unsettled; last error: {error}"
                        ));
                    }
                    reactive_refresh_used = true;
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        return Err(format!(
                            "exact-run cancellation did not settle run {run_id}; last cancellation error: {error}"
                        ));
                    }
                    match owner_cancel_bearer(&api, owner, Some(&attempt_token), deadline).await {
                        // One repost of the same exact cancellation, within
                        // the same deadline and run id; nothing else replays.
                        Ok(_) => {}
                        Err(reason) => {
                            return Err(format!(
                                "exact-run cancellation for run {run_id} failed closed after its one refresh attempt ({reason}); last error: {error}"
                            ));
                        }
                    }
                }
                astra_thin_client::ThinClientError::Api { status, .. }
                    if *status == reqwest::StatusCode::FORBIDDEN =>
                {
                    return Err(format!(
                        "exact-run cancellation for run {run_id} was rejected with 403 and remains unsettled; last error: {error}"
                    ));
                }
                _ => {
                    // DELETE is the idempotent cancellation operation for one
                    // exact durable run. A transport/5xx failure is not proof
                    // that execution is settled, so keep the lease and retry
                    // within the bounded request settlement window.
                    last_error = Some(error.to_string());
                }
            },
            Err(_) => {
                return Err(format!("exact-run cancellation timed out for run {run_id}"));
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
    auth_owner: Option<&crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot>,
    request_lease: Option<
        &crate::cli::session::session_execution_lease::RequestSessionExecutionLease,
    >,
    incremental_state: Option<&astra_turn_core::turn_event_sink::IncrementalTurnState>,
    result: Result<StreamResult, TurnFailure>,
) -> Result<StreamResult, TurnFailure> {
    settle_request_session_binding_failure_with_cancel_deadline(
        api,
        token,
        auth_owner,
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
    auth_owner: Option<&crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot>,
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
            let deadline = tokio::time::Instant::now() + cancel_deadline;
            cancel_exact_run_until_settled_with_deadline(
                api,
                token,
                run_id,
                auth_owner,
                deadline,
            )
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
            turn_failure.partial.failed_run_terminal = None;
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
    pub(crate) executor_observer: Option<crate::cli::chat_stream::CliExecutorObserver>,
    pub(crate) request_session_execution_lease: Option<
        std::sync::Arc<crate::cli::session::session_execution_lease::RequestSessionExecutionLease>,
    >,
    pub(crate) approval_request_tx: Option<ApprovalRequestTx>,
    /// Authoritative 1-based outer-session turn restored before any auxiliary
    /// inference or main bridge request is admitted.
    pub(crate) turn_index: Option<u32>,
}

pub(crate) async fn execute_basic_cli_turn<'a>(
    ctx: &'a BasicCliChatContext<'a>,
    token: &'a str,
    session_id: Option<&'a str>,
    perm_manager: &'a mut PermissionManager,
    mut options: BasicCliTurnOptions,
) -> Result<StreamResult, TurnFailure> {
    let mut params = ChatTurnParams::basic_cli(ctx, token, session_id, perm_manager);
    // Use the single owner snapshot captured when the BasicCliChatContext was
    // built; every retry and cancellation re-entry of this submitted turn
    // shares that one immutable captured owner. Never recapture a later login.
    let auth_owner = ctx.auth_owner.clone();
    params.auth_owner = auth_owner.clone();
    params.pre_loaded_messages = options.pre_loaded_messages;
    params.deferred_tool_activations = Some(&mut options.deferred_tool_activations);
    params.append_system_prompt = options.append_system_prompt;
    params.cancel_token = options.cancel_token;
    params.execution_time_budget = options.execution_time_budget;
    params.incremental_state = options.incremental_state.clone();
    params.executor_observer = options.executor_observer;
    params.request_session_execution_lease = options.request_session_execution_lease.clone();
    params.approval_request_tx = options.approval_request_tx;
    if let Some(turn_index) = options.turn_index {
        params.turn_index = turn_index.max(1);
    }
    settle_request_session_binding_failure(
        ctx.api,
        token,
        auth_owner.as_ref(),
        options.request_session_execution_lease.as_deref(),
        options.incremental_state.as_deref(),
        stream_chat_sse(params).await,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::{
        cancel_exact_run_until_settled, is_settled_stdout_closure,
        settle_request_session_binding_failure,
        settle_request_session_binding_failure_with_cancel_deadline,
    };

    #[serial_test::serial]
    #[tokio::test]
    async fn captured_owner_cancel_refreshes_once_and_requires_execution_settlement() {
        use crate::cli::cli_config::cli_utils::{CredentialsFile, Profile, save_credentials};
        use crate::cli::session::session_runtime::capture_admitted_auth_owner;
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _credentials = crate::tests::isolate_credentials();
        let _identity = crate::cli::cli_config::cli_utils::install_cli_profile_identity_for_test(
            "default",
            Some("cancel-owner"),
        )
        .unwrap();
        for (second_status, settles, refresh_delay, cancel_delay, budget) in [
            (200, true, 0, 0, 1000),
            (401, false, 0, 0, 1000),
            (403, false, 0, 0, 1000),
            // A terminal status alone does not release execution ownership.
            (200, false, 0, 0, 80),
            // Credential resolution and HTTP share one absolute deadline.
            (200, true, 240, 400, 400),
        ] {
            // The slow case must refresh before the first DELETE. A 401
            // refresh followed by a new loop iteration would recompute the
            // budget and miss the stale-duration regression.
            let admitted_token = if refresh_delay > 0 {
                "eyJhbGciOiJub25lIn0.eyJleHAiOjF9.signature"
            } else {
                "old-cancel"
            };
            let mut creds = CredentialsFile::default();
            creds.profiles.insert(
                "default".into(),
                Profile {
                    account_id: Some("cancel-owner".into()),
                    access_token: Some(admitted_token.into()),
                    refresh_token: Some("old-refresh".into()),
                    ..Default::default()
                },
            );
            save_credentials(&creds).unwrap();
            let server = MockServer::start().await;
            Mock::given(method("DELETE"))
                .and(path("/chat/runs/exact-owner-run"))
                .and(header("authorization", format!("Bearer {admitted_token}")))
                .respond_with(ResponseTemplate::new(401))
                .expect(if refresh_delay > 0 { 0 } else { 1 })
                .mount(&server)
                .await;
            Mock::given(method("POST")).and(path("/auth/refresh"))
                .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_millis(refresh_delay))
                    .set_body_json(serde_json::json!({"user_id":"cancel-owner", "access_token":"new-cancel", "refresh_token":"new-refresh"})))
                .expect(1).mount(&server).await;
            Mock::given(method("DELETE"))
                .and(path("/chat/runs/exact-owner-run"))
                .and(header("authorization", "Bearer new-cancel"))
                .respond_with(
                    ResponseTemplate::new(second_status)
                        .set_delay(std::time::Duration::from_millis(cancel_delay))
                        .set_body_json(
                            serde_json::json!({"status":"cancelled", "execution_settled":settles}),
                        ),
                )
                .mount(&server)
                .await;
            let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
            let owner = capture_admitted_auth_owner(&api, None, admitted_token)
                .await
                .unwrap();
            let start = tokio::time::Instant::now();
            let result = super::cancel_exact_run_until_settled_with_deadline(
                &api,
                admitted_token,
                "exact-owner-run",
                Some(&owner),
                start + std::time::Duration::from_millis(budget),
            )
            .await;
            assert_eq!(result.is_ok(), settles && cancel_delay == 0);
            if cancel_delay > 0 {
                assert!(
                    start.elapsed() < std::time::Duration::from_millis(550),
                    "refresh must not restart the cancellation deadline"
                );
            }
            let requests = server.received_requests().await.unwrap();
            assert!(
                requests.iter().all(|r| r.url.path() == "/auth/refresh"
                    || r.url.path() == "/chat/runs/exact-owner-run")
            );
            if second_status == 401 || second_status == 403 {
                assert_eq!(
                    requests
                        .iter()
                        .filter(|r| r.method.as_str() == "DELETE")
                        .count(),
                    2
                );
            }
        }
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn captured_owner_cannot_adopt_an_unknown_same_account_login_for_cancel() {
        use crate::cli::cli_config::cli_utils::{CredentialsFile, Profile, save_credentials};
        let _credentials = crate::tests::isolate_credentials();
        let _identity = crate::cli::cli_config::cli_utils::install_cli_profile_identity_for_test(
            "default",
            Some("cancel-owner"),
        )
        .unwrap();
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".into(),
            Profile {
                account_id: Some("cancel-owner".into()),
                access_token: Some("original".into()),
                refresh_token: Some("original-refresh".into()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();
        let server = wiremock::MockServer::start().await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let owner = crate::cli::session::session_runtime::capture_admitted_auth_owner(
            &api, None, "original",
        )
        .await
        .unwrap();
        creds.profiles.get_mut("default").unwrap().access_token = Some("replacement".into());
        save_credentials(&creds).unwrap();
        let result = super::cancel_exact_run_until_settled_with_deadline(
            &api,
            "original",
            "never-rebind",
            Some(&owner),
            tokio::time::Instant::now() + std::time::Duration::from_secs(1),
        )
        .await;
        assert!(result.is_err());
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn binding_cleanup_failure_preserves_cumulative_stream_telemetry() {
        let (_sessions, _guard) = crate::tests::isolated_sessions_dir();
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
            None,
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
            None,
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

    #[serial_test::serial]
    #[tokio::test]
    async fn binding_failure_cancels_the_exact_run_and_reports_settled_failure() {
        let (_sessions, _guard) = crate::tests::isolated_sessions_dir();
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
            None,
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

    #[serial_test::serial]
    #[tokio::test]
    async fn cancel_transport_failure_preserves_unsettled_remote_owner_and_session_lease() {
        let (_sessions, _guard) = crate::tests::isolated_sessions_dir();
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
            None,
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

    #[serial_test::serial]
    #[tokio::test]
    async fn exact_cancel_retries_transient_failure_until_settled_while_lease_is_held() {
        let (_sessions, _guard) = crate::tests::isolated_sessions_dir();
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

        #[derive(Clone)]
        struct SettleWhenAllowed {
            calls: std::sync::Arc<AtomicUsize>,
            allow_settlement: std::sync::Arc<AtomicBool>,
        }

        impl Respond for SettleWhenAllowed {
            fn respond(&self, _request: &Request) -> ResponseTemplate {
                let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
                if call == 1 {
                    return ResponseTemplate::new(503);
                }
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "run_id": "run-session-binding",
                    "execution_settled": self.allow_settlement.load(Ordering::SeqCst),
                }))
            }
        }

        let server = MockServer::start().await;
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        // Hold the execution-settlement fact until the assertion, rather than
        // relying on a 100ms scheduling window under parallel test load.
        let allow_settlement = std::sync::Arc::new(AtomicBool::new(false));
        Mock::given(method("DELETE"))
            .and(path("/chat/runs/run-session-binding"))
            .and(header("authorization", "Bearer token"))
            .respond_with(SettleWhenAllowed {
                calls: calls.clone(),
                allow_settlement: allow_settlement.clone(),
            })
            .expect(2..)
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
        allow_settlement.store(true, Ordering::SeqCst);
        task.await.unwrap().unwrap();
        astra_services::session_journal::SessionExecutionLease::try_acquire(&session_id)
            .expect("settled request releases its lease");
    }
}
