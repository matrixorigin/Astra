//! Convert stream outcomes into committed turn state transitions.

use std::time::Instant;

use super::turn_cancellation::apply_user_cancelled_turn;
use super::turn_entry::TurnContext;
use super::turn_failure_reporting::report_turn_failure;
use super::turn_success::apply_turn_success_async;
use crate::cli::session::session_state::SessionState;
use crate::cli::stream::streaming_types::StreamResult;

pub(crate) struct TurnDispatch<'a, 'b> {
    pub(crate) ctx: &'a TurnContext<'b>,
    pub(crate) line: &'a str,
    pub(crate) effective_line: &'a str,
    pub(crate) user_intent: &'a str,
    pub(crate) input_runtime_required_texts: &'a [String],
    pub(crate) input_runtime_volatile_texts: &'a [String],
    pub(crate) token: &'a str,
    pub(crate) session_id: Option<&'a str>,
    pub(crate) semantic_query_override: Option<&'a str>,
    pub(crate) turn_start: Instant,
    pub(crate) ui: &'a mut dyn crate::cli::ui_adapter::ReplUiAdapter,
}

pub(crate) async fn settle_interrupted_turn(
    state: &mut SessionState,
    dispatch: &mut TurnDispatch<'_, '_>,
    result: Result<StreamResult, crate::TurnFailure>,
) {
    apply_user_cancelled_turn(
        state,
        dispatch.ctx.api,
        dispatch.ctx.profile,
        dispatch.line,
        result,
        dispatch.turn_start,
        dispatch.ui,
        dispatch.ctx.post_commit_tx.as_ref(),
    )
    .await;
    clear_recovery_scoped_turn_restrictions(state);
}

pub(crate) async fn settle_successful_turn(
    state: &mut SessionState,
    dispatch: &mut TurnDispatch<'_, '_>,
    result: StreamResult,
) {
    apply_turn_success_async(
        state,
        dispatch.ctx.api,
        dispatch.ctx.profile,
        dispatch.line,
        result,
        dispatch.turn_start,
        dispatch.ui,
        dispatch.ctx.post_commit_tx.as_ref(),
    )
    .await;
    clear_recovery_scoped_turn_restrictions(state);
}

pub(crate) fn settle_failed_turn(
    state: &mut SessionState,
    dispatch: &mut TurnDispatch<'_, '_>,
    failure: &crate::TurnFailure,
) {
    report_turn_failure(
        state,
        dispatch.ctx.profile,
        dispatch.line,
        failure,
        dispatch.turn_start,
        dispatch.ui,
    );
    clear_recovery_scoped_turn_restrictions(state);
}

fn clear_recovery_scoped_turn_restrictions(state: &mut SessionState) {
    state.resume_restricted_tools.clear();
}

#[cfg(test)]
mod tests {
    use super::TurnContext;
    use super::{TurnDispatch, settle_failed_turn, settle_successful_turn};
    use crate::cli::session::session_state::SessionState;
    use std::time::Instant;

    #[tokio::test]
    async fn settle_successful_turn_clears_last_turn_interrupted() {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let ctx = TurnContext {
            api: &api,
            profile: None,
            post_commit_tx: None,
        };
        let mut ui = crate::tests::TestUi::default();
        let mut state = SessionState {
            last_turn_interrupted: true,
            resume_restricted_tools: vec!["bash".into()],
            ..SessionState::default()
        };
        let mut dispatch = TurnDispatch {
            ctx: &ctx,
            line: "continue",
            effective_line: "continue",
            user_intent: "continue",
            input_runtime_required_texts: &[],
            input_runtime_volatile_texts: &[],
            token: "token",
            session_id: None,
            semantic_query_override: None,
            turn_start: Instant::now(),
            ui: &mut ui,
        };

        settle_successful_turn(
            &mut state,
            &mut dispatch,
            crate::tests::stub_stream_result("done"),
        )
        .await;

        assert!(!state.last_turn_interrupted);
        assert!(state.resume_restricted_tools.is_empty());
        assert_eq!(state.history.len(), 1);
    }

    #[test]
    fn settle_failed_turn_consumes_resume_restricted_tools() {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let ctx = TurnContext {
            api: &api,
            profile: None,
            post_commit_tx: None,
        };
        let mut ui = crate::tests::TestUi::default();
        let mut state = SessionState {
            resume_restricted_tools: vec!["bash".into(), "write_file".into()],
            ..SessionState::default()
        };
        let mut dispatch = TurnDispatch {
            ctx: &ctx,
            line: "continue",
            effective_line: "continue",
            user_intent: "continue",
            input_runtime_required_texts: &[],
            input_runtime_volatile_texts: &[],
            token: "token",
            session_id: None,
            semantic_query_override: None,
            turn_start: Instant::now(),
            ui: &mut ui,
        };
        let failure = crate::TurnFailure {
            error: "boom".into(),
            partial: crate::PartialTurnData::default(),
        };

        settle_failed_turn(&mut state, &mut dispatch, &failure);

        assert!(state.resume_restricted_tools.is_empty());
        assert_eq!(
            state.turn, 1,
            "failed settlement must advance the local turn cursor"
        );
    }
}
