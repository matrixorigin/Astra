//! Retry orchestration for recoverable turn failures.

use super::turn_auth_retry::prepare_auth_refresh_retry;
use super::turn_settlement::{
    TurnDispatch, settle_failed_turn, settle_interrupted_turn, settle_successful_turn,
};
use super::turn_stream_runner::{TurnAttempt, TurnExecutionInput, TurnExecutionRequest};
use crate::cli::session::session_state::SessionState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnSettlementOutcome {
    NotStarted,
    Succeeded,
    Interrupted,
    Failed,
}

pub(crate) async fn settle_turn_attempt(
    state: &mut SessionState,
    dispatch: &mut TurnDispatch<'_, '_>,
    attempt: TurnAttempt,
    run_chat_turn: impl for<'a> Fn(
        TurnExecutionRequest<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = TurnAttempt> + 'a>,
    >,
) -> Result<TurnSettlementOutcome, String> {
    match attempt {
        TurnAttempt::NotStarted(failure) => {
            if let Some(outcome) =
                try_retry_after_auth_refresh(state, dispatch, &failure, &run_chat_turn).await
            {
                return Ok(outcome);
            }
            settle_not_started_turn(state, dispatch, &failure);
            Ok(TurnSettlementOutcome::NotStarted)
        }
        TurnAttempt::Interrupted(result) => {
            settle_interrupted_turn(state, dispatch, *result).await;
            Ok(TurnSettlementOutcome::Interrupted)
        }
        TurnAttempt::Completed(result) => match *result {
            Ok(result) => {
                settle_successful_turn(state, dispatch, result).await;
                Ok(TurnSettlementOutcome::Succeeded)
            }
            Err(mut failure) => {
                // Once execution was attempted, retain and settle its facts.
                // Only producer-owned local preflight may authorize a retry.
                settle_failed_turn(state, dispatch, &mut failure).await;
                Ok(TurnSettlementOutcome::Failed)
            }
        },
    }
}

async fn try_retry_after_auth_refresh(
    state: &mut SessionState,
    dispatch: &mut TurnDispatch<'_, '_>,
    failure: &crate::TurnFailure,
    run_chat_turn: &impl for<'a> Fn(
        TurnExecutionRequest<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = TurnAttempt> + 'a>,
    >,
) -> Option<TurnSettlementOutcome> {
    if !dispatch.ctx.permits_admission(state) {
        return None;
    }
    let new_token = if let Some(admission) = &dispatch.ctx.admission {
        // A bound submission cannot borrow the ambient profile or infer
        // refresh authority from a rendered error sentence.
        if !super::turn_auth_retry::should_retry_after_auth_refresh(failure) {
            return None;
        }
        crate::cli::session::session_runtime::owner_access_token(
            dispatch.ctx.api,
            admission.owner,
            Some(dispatch.token),
        )
        .await?
    } else {
        prepare_auth_refresh_retry(dispatch.ctx.api, dispatch.ctx.profile, failure, dispatch.ui)
            .await?
    };
    if !dispatch.ctx.permits_admission(state) {
        return None;
    }

    let retry = run_chat_turn(TurnExecutionRequest {
        state,
        input: TurnExecutionInput {
            api: dispatch.ctx.api,
            profile: dispatch.ctx.profile,
            token: &new_token,
            message: dispatch.effective_line,
            user_intent: dispatch.user_intent,
            input_runtime_required_texts: dispatch.input_runtime_required_texts,
            input_runtime_volatile_texts: dispatch.input_runtime_volatile_texts,
            session_id: dispatch.session_id,
            semantic_query_override: dispatch.semantic_query_override,
            explain_analyze_terminal_degraded: dispatch.ctx.explain_analyze_terminal_degraded,
        },
    })
    .await;
    Some(settle_retry_attempt(state, dispatch, retry).await)
}

async fn settle_retry_attempt(
    state: &mut SessionState,
    dispatch: &mut TurnDispatch<'_, '_>,
    retry: TurnAttempt,
) -> TurnSettlementOutcome {
    match retry {
        TurnAttempt::NotStarted(failure) => {
            settle_not_started_turn(state, dispatch, &failure);
            TurnSettlementOutcome::NotStarted
        }
        TurnAttempt::Interrupted(result) => {
            settle_interrupted_turn(state, dispatch, *result).await;
            TurnSettlementOutcome::Interrupted
        }
        TurnAttempt::Completed(result) => match *result {
            Ok(result) => {
                settle_successful_turn(state, dispatch, result).await;
                TurnSettlementOutcome::Succeeded
            }
            Err(mut retry_failure) => {
                settle_failed_turn(state, dispatch, &mut retry_failure).await;
                TurnSettlementOutcome::Failed
            }
        },
    }
}

fn settle_not_started_turn(
    state: &SessionState,
    dispatch: &mut TurnDispatch<'_, '_>,
    failure: &crate::TurnFailure,
) {
    dispatch.ui.show_error(&failure.error);
    if !dispatch.line.trim().is_empty() {
        let _ = dispatch
            .ui
            .restore_input(dispatch.line, state.session_id.as_deref());
    }
}

#[cfg(test)]
mod tests {
    use super::{TurnAttempt, settle_turn_attempt};
    use crate::cli::session::session_state::SessionState;
    use crate::cli::turn::turn_entry::TurnContext;
    use crate::cli::turn::turn_settlement::TurnDispatch;
    use std::time::Instant;

    #[tokio::test]
    async fn unstarted_initial_and_retry_preserve_input_and_recovery_state() {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap();
        let ctx = TurnContext {
            api: &api,
            profile: None,
            post_commit_tx: None,
            admission: None,
            explain_analyze_terminal_degraded: None,
        };
        for retry in [false, true] {
            let mut state = SessionState {
                session_id: Some("same-session".into()),
                turn: 7,
                resume_guidance: Some("unfinished evidence".into()),
                diagnostics_context: Some("diagnostic".into()),
                pending_bg_notifications: vec!["child complete".into()],
                resume_restricted_tools: vec!["bash".into()],
                ..SessionState::default()
            };
            let mut ui = crate::tests::TestUi::default();
            let usage = std::sync::Arc::new(std::sync::Mutex::new(None));
            let mut dispatch = TurnDispatch {
                ctx: &ctx,
                line: "/model literal task",
                effective_line: "/model literal task",
                user_intent: "/model literal task",
                input_runtime_required_texts: &[],
                input_runtime_volatile_texts: &[],
                token: "token",
                session_id: "same-session",
                semantic_query_override: None,
                turn_start: Instant::now(),
                ui: &mut ui,
                turn_usage_sink: Some(&usage),
            };
            let attempt = TurnAttempt::NotStarted(Box::new(crate::TurnFailure {
                error: "model selection rejected".into(),
                partial: Default::default(),
            }));
            let outcome = if retry {
                super::settle_retry_attempt(&mut state, &mut dispatch, attempt).await
            } else {
                settle_turn_attempt(&mut state, &mut dispatch, attempt, |_| {
                    Box::pin(async { panic!("preflight rejection cannot execute a turn") })
                })
                .await
                .unwrap()
            };
            assert_eq!(outcome, super::TurnSettlementOutcome::NotStarted);
            assert_eq!(state.turn, 7);
            assert!(state.history.is_empty());
            assert_eq!(state.session_id.as_deref(), Some("same-session"));
            assert_eq!(
                state.resume_guidance.as_deref(),
                Some("unfinished evidence")
            );
            assert_eq!(state.diagnostics_context.as_deref(), Some("diagnostic"));
            assert_eq!(state.pending_bg_notifications, ["child complete"]);
            assert_eq!(state.resume_restricted_tools, ["bash"]);
            assert_eq!(ui.restored_inputs, ["/model literal task"]);
            assert_eq!(ui.errors, ["model selection rejected"]);
            assert!(usage.lock().unwrap().is_none());
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn executed_failure_preserves_partial_delivery_without_auth_or_session_replay() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        for error in [
            "session not found: sess-stale",
            "API Error (401): Could not validate credentials",
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/sessions"))
                .respond_with(ResponseTemplate::new(500))
                .expect(0)
                .mount(&server)
                .await;
            let (_sessions, _sessions_guard) = crate::tests::isolated_sessions_dir();
            let _credentials_guard = crate::tests::isolate_credentials();
            use crate::cli::cli_config::cli_utils::{CredentialsFile, Profile, save_credentials};
            let mut credentials = CredentialsFile::default();
            credentials.profiles.insert(
                "default".into(),
                Profile {
                    account_id: Some("user-id-1".into()),
                    access_token: Some("token".into()),
                    refresh_token: Some("refresh-token".into()),
                    ..Default::default()
                },
            );
            save_credentials(&credentials).unwrap();
            Mock::given(method("POST"))
                .and(path("/auth/refresh"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "user_id": "user-id-1",
                    "access_token": "new-token", "refresh_token": "new-refresh-token"
                })))
                .expect(0)
                .mount(&server)
                .await;
            let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
            let ctx = TurnContext {
                api: &api,
                profile: None,
                post_commit_tx: None,
                admission: None,
                explain_analyze_terminal_degraded: None,
            };
            let mut ui = crate::tests::TestUi::default();
            let mut state = SessionState {
                session_id: Some("sess-stale".into()),
                last_turn_interrupted: true,
                ..SessionState::default()
            };
            let usage = std::sync::Arc::new(std::sync::Mutex::new(None));
            let mut dispatch = TurnDispatch {
                ctx: &ctx,
                line: "continue",
                effective_line: "continue",
                user_intent: "continue",
                input_runtime_required_texts: &[],
                input_runtime_volatile_texts: &[],
                token: "token",
                session_id: "sess-stale",
                semantic_query_override: None,
                turn_start: Instant::now(),
                ui: &mut ui,
                turn_usage_sink: Some(&usage),
            };
            let attempt = TurnAttempt::Completed(Box::new(Err(crate::TurnFailure {
                error: error.into(),
                partial: crate::PartialTurnData {
                    error_code: Some(astra_core::ErrorKind::Auth.as_str().into()),
                    error_metadata: Some(
                        serde_json::json!({"source": "model_access", "http_status": 401}),
                    ),
                    partial_text: "Already observed output".into(),
                    prompt_tokens: 17,
                    completion_tokens: 3,
                    ..Default::default()
                },
            })));

            let retry_calls = std::sync::atomic::AtomicUsize::new(0);
            let outcome = settle_turn_attempt(&mut state, &mut dispatch, attempt, |_| {
                retry_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async { panic!("executed failure must not authorize a replay") })
            })
            .await
            .unwrap();

            assert_eq!(outcome, super::TurnSettlementOutcome::Failed);
            assert_eq!(retry_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert_eq!(state.session_id.as_deref(), Some("sess-stale"));
            assert_eq!(
                state.last_response.as_deref(),
                Some("Already observed output")
            );
            {
                let usage = usage.lock().unwrap();
                let usage = usage.as_ref().expect("partial usage retained");
                assert_eq!(usage.prompt_tokens, 17);
                assert_eq!(usage.completion_tokens, 3);
            }
            server.verify().await;
        }
    }
}
