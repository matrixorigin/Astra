//! Retry orchestration for recoverable turn failures.

use super::turn_auth_retry::prepare_auth_refresh_retry;
use super::turn_session_retry::{
    prepare_session_not_found_retry, should_retry_after_session_not_found,
};
use super::turn_settlement::{
    TurnDispatch, settle_failed_turn, settle_interrupted_turn, settle_successful_turn,
};
use super::turn_stream_runner::TurnAttempt;
use crate::cli::session::session_state::SessionState;

pub(crate) async fn settle_turn_attempt(
    state: &mut SessionState,
    dispatch: &mut TurnDispatch<'_, '_>,
    attempt: TurnAttempt,
    run_chat_turn: impl for<'a> Fn(
        &'a mut SessionState,
        &'a astra_thin_client::ThinClient,
        Option<&'a str>,
        &'a str,
        &'a str,
        Option<&'a str>,
        Option<&'a str>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = TurnAttempt> + 'a>,
    >,
) -> Result<(), String> {
    match attempt {
        TurnAttempt::Interrupted(result) => settle_interrupted_turn(state, dispatch, *result).await,
        TurnAttempt::Completed(result) => match *result {
            Ok(result) => settle_successful_turn(state, dispatch, result).await,
            Err(failure) => {
                if try_retry_after_session_not_found(state, dispatch, &failure, &run_chat_turn)
                    .await
                {
                    return Ok(());
                }

                if try_retry_after_auth_refresh(state, dispatch, &failure, &run_chat_turn).await {
                    return Ok(());
                }

                settle_failed_turn(state, dispatch, &failure);
            }
        },
    }

    Ok(())
}

async fn try_retry_after_session_not_found(
    state: &mut SessionState,
    dispatch: &mut TurnDispatch<'_, '_>,
    failure: &crate::TurnFailure,
    run_chat_turn: &impl for<'a> Fn(
        &'a mut SessionState,
        &'a astra_thin_client::ThinClient,
        Option<&'a str>,
        &'a str,
        &'a str,
        Option<&'a str>,
        Option<&'a str>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = TurnAttempt> + 'a>,
    >,
) -> bool {
    if !should_retry_after_session_not_found(&failure.error, state.session_id.is_some()) {
        return false;
    }

    prepare_session_not_found_retry(state, dispatch.ctx.profile).await;
    dispatch
        .ui
        .show_warning("  Session not found. Creating a new session…");

    let retry = run_chat_turn(
        state,
        dispatch.ctx.api,
        dispatch.ctx.profile,
        dispatch.token,
        dispatch.effective_line,
        None,
        dispatch.semantic_query_override,
    )
    .await;
    settle_retry_attempt(state, dispatch, retry).await;
    true
}

async fn try_retry_after_auth_refresh(
    state: &mut SessionState,
    dispatch: &mut TurnDispatch<'_, '_>,
    failure: &crate::TurnFailure,
    run_chat_turn: &impl for<'a> Fn(
        &'a mut SessionState,
        &'a astra_thin_client::ThinClient,
        Option<&'a str>,
        &'a str,
        &'a str,
        Option<&'a str>,
        Option<&'a str>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = TurnAttempt> + 'a>,
    >,
) -> bool {
    let Some(new_token) =
        prepare_auth_refresh_retry(dispatch.ctx.api, dispatch.ctx.profile, failure, dispatch.ui)
            .await
    else {
        return false;
    };

    let retry = run_chat_turn(
        state,
        dispatch.ctx.api,
        dispatch.ctx.profile,
        &new_token,
        dispatch.effective_line,
        dispatch.session_id,
        dispatch.semantic_query_override,
    )
    .await;
    settle_retry_attempt(state, dispatch, retry).await;
    true
}

async fn settle_retry_attempt(
    state: &mut SessionState,
    dispatch: &mut TurnDispatch<'_, '_>,
    retry: TurnAttempt,
) {
    match retry {
        TurnAttempt::Interrupted(result) => settle_interrupted_turn(state, dispatch, *result).await,
        TurnAttempt::Completed(result) => match *result {
            Ok(result) => settle_successful_turn(state, dispatch, result).await,
            Err(retry_failure) => settle_failed_turn(state, dispatch, &retry_failure),
        },
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
    async fn successful_retry_clears_last_turn_interrupted() {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let ctx = TurnContext {
            api: &api,
            profile: None,
        };
        let mut ui = crate::tests::TestUi::default();
        let mut state = SessionState {
            session_id: Some("sess-stale".into()),
            last_turn_interrupted: true,
            ..SessionState::default()
        };
        let mut dispatch = TurnDispatch {
            ctx: &ctx,
            line: "continue",
            effective_line: "continue",
            token: "token",
            session_id: Some("sess-stale"),
            semantic_query_override: None,
            turn_start: Instant::now(),
            ui: &mut ui,
        };
        let attempt = TurnAttempt::Completed(Box::new(Err(crate::TurnFailure {
            error: "session not found: sess-stale".into(),
            partial: crate::PartialTurnData::default(),
        })));

        settle_turn_attempt(&mut state, &mut dispatch, attempt, |_, _, _, _, _, _, _| {
            Box::pin(async move {
                TurnAttempt::Completed(Box::new(Ok(crate::tests::stub_stream_result("Recovered"))))
            })
        })
        .await
        .unwrap();

        assert!(!state.last_turn_interrupted);
        assert_eq!(
            ui.warnings,
            vec!["  Session not found. Creating a new session…"]
        );
    }
}
