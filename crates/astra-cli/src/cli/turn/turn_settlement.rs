//! Convert stream outcomes into committed turn state transitions.

use std::time::Instant;

use super::turn_cancellation::apply_user_cancelled_turn;
use super::turn_entry::{TurnContext, TurnUsage};
use super::turn_failure_reporting::{
    reconcile_and_report_turn_failure, report_admission_rejection,
};
use super::turn_success::apply_turn_success_async;
use crate::cli::session::session_state::SessionState;
use crate::cli::stream::streaming_types::StreamResult;

pub(crate) struct TurnDispatch<'a, 'b> {
    pub(crate) ctx: &'a TurnContext<'b>,
    pub(crate) line: &'a str,
    pub(crate) effective_line: &'a str,
    pub(crate) user_intent: &'a str,
    pub(crate) input_runtime_required_texts: &'a [String],
    pub(crate) input_active_system_skills: &'a [String],
    pub(crate) input_runtime_volatile_texts: &'a [String],
    pub(crate) token: &'a str,
    pub(crate) session_id: &'a str,
    pub(crate) semantic_query_override: Option<&'a str>,
    pub(crate) turn_start: Instant,
    pub(crate) ui: &'a mut dyn crate::cli::ui_adapter::ReplUiAdapter,
    pub(crate) turn_usage_sink: Option<&'a std::sync::Arc<std::sync::Mutex<Option<TurnUsage>>>>,
}

fn publish_turn_usage(dispatch: &TurnDispatch<'_, '_>, usage: Option<TurnUsage>) {
    if let Some(sink) = dispatch.turn_usage_sink {
        *sink.lock().unwrap_or_else(|error| error.into_inner()) = usage;
    }
}

pub(crate) async fn settle_interrupted_turn(
    state: &mut SessionState,
    dispatch: &mut TurnDispatch<'_, '_>,
    result: Result<StreamResult, crate::TurnFailure>,
) {
    let usage = match &result {
        Ok(result) => TurnUsage::from_stream_result(result),
        Err(failure) => TurnUsage::from_partial(&failure.partial),
    };
    publish_turn_usage(dispatch, usage);
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
    publish_turn_usage(dispatch, TurnUsage::from_stream_result(&result));
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

pub(crate) async fn settle_failed_turn(
    state: &mut SessionState,
    dispatch: &mut TurnDispatch<'_, '_>,
    failure: &mut crate::TurnFailure,
) {
    if failure.partial.admission_rejected {
        report_admission_rejection(state, dispatch.line, failure, dispatch.ui);
        clear_recovery_scoped_turn_restrictions(state);
        return;
    }
    reconcile_and_report_turn_failure(
        state,
        dispatch.ctx.api,
        dispatch.ctx.profile,
        dispatch.line,
        failure,
        dispatch.turn_start,
        dispatch.ui,
    )
    .await;
    publish_turn_usage(dispatch, TurnUsage::from_partial(&failure.partial));
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
            explain_analyze_terminal_degraded: None,
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
            input_active_system_skills: &[],
            input_runtime_volatile_texts: &[],
            token: "token",
            session_id: "session-1",
            semantic_query_override: None,
            turn_start: Instant::now(),
            ui: &mut ui,
            turn_usage_sink: None,
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

    #[tokio::test]
    async fn settle_failed_turn_consumes_resume_restricted_tools() {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let ctx = TurnContext {
            api: &api,
            profile: None,
            post_commit_tx: None,
            explain_analyze_terminal_degraded: None,
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
            input_active_system_skills: &[],
            input_runtime_volatile_texts: &[],
            token: "token",
            session_id: "session-1",
            semantic_query_override: None,
            turn_start: Instant::now(),
            ui: &mut ui,
            turn_usage_sink: None,
        };
        let mut failure = crate::TurnFailure {
            error: "boom".into(),
            partial: crate::PartialTurnData::default(),
        };

        settle_failed_turn(&mut state, &mut dispatch, &mut failure).await;

        assert!(state.resume_restricted_tools.is_empty());
        assert_eq!(
            state.turn, 1,
            "failed settlement must advance the local turn cursor"
        );
    }

    /// The actual SSE-to-settlement boundary for a `session_execution_slot_occupied`
    /// start-rejection: `admission_rejected` here is exactly what
    /// `is_pre_admission_rejection` computes from the server's `error_code`/
    /// `error_metadata`/`run_id` on the wire (see
    /// `crate::cli::chat_stream::sse_loop::server_admission_host`), not a
    /// hand-picked test shortcut. No run was ever admitted, so this must take
    /// the early-return branch in `settle_failed_turn`: the draft goes back to
    /// the user, the local turn cursor does not advance, and — because that
    /// branch returns before `reconcile_and_report_turn_failure` — no
    /// TurnError is journaled and no reconciliation network call is made.
    #[tokio::test]
    async fn settle_failed_turn_restores_draft_without_advancing_cursor_for_session_slot_conflict()
    {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let ctx = TurnContext {
            api: &api,
            profile: None,
            post_commit_tx: None,
            explain_analyze_terminal_degraded: None,
        };
        let mut ui = crate::tests::TestUi::default();
        let mut state = SessionState::default();
        let mut dispatch = TurnDispatch {
            ctx: &ctx,
            line: "hello again",
            effective_line: "hello again",
            user_intent: "hello again",
            input_runtime_required_texts: &[],
            input_active_system_skills: &[],
            input_runtime_volatile_texts: &[],
            token: "token",
            session_id: "session-1",
            semantic_query_override: None,
            turn_start: Instant::now(),
            ui: &mut ui,
            turn_usage_sink: None,
        };
        let error_code = Some("session_execution_slot_occupied".to_string());
        let error_metadata = Some(serde_json::json!({"admission_state": "rejected"}));
        let admission_rejected = crate::cli::chat_stream::is_pre_admission_rejection(
            error_code.as_deref(),
            error_metadata.as_ref(),
            None,
        );
        assert!(
            admission_rejected,
            "server_admission_host's own classifier must agree this is pre-admission"
        );
        let mut failure = crate::TurnFailure {
            error: "session already has an active run".into(),
            partial: crate::PartialTurnData {
                error_code,
                error_metadata,
                admission_rejected,
                ..Default::default()
            },
        };

        settle_failed_turn(&mut state, &mut dispatch, &mut failure).await;

        assert_eq!(
            state.turn, 0,
            "a start-rejection must not advance the local turn cursor"
        );
        assert_eq!(ui.restored_inputs, vec!["hello again"]);
        assert!(
            state.journal.is_none(),
            "a start-rejection must not bootstrap a journal or record a TurnError"
        );
    }

    #[tokio::test]
    async fn admission_rejection_does_not_consume_a_turn_or_poll_a_run() {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let ctx = TurnContext {
            api: &api,
            profile: None,
            post_commit_tx: None,
            explain_analyze_terminal_degraded: None,
        };
        let mut ui = crate::tests::TestUi::default();
        let mut state = SessionState {
            session_id: Some("session-current".into()),
            ..SessionState::default()
        };
        let mut dispatch = TurnDispatch {
            ctx: &ctx,
            line: "hi",
            effective_line: "hi",
            user_intent: "hi",
            input_runtime_required_texts: &[],
            input_active_system_skills: &[],
            input_runtime_volatile_texts: &[],
            token: "token",
            session_id: "session-current",
            semantic_query_override: None,
            turn_start: Instant::now(),
            ui: &mut ui,
            turn_usage_sink: None,
        };
        let mut failure = crate::TurnFailure {
            error: "[invalid_request] This checkout is already attached to another Session".into(),
            partial: crate::PartialTurnData {
                error_code: Some("execution_workspace_claimed".into()),
                error_metadata: Some(serde_json::json!({
                    "admission_state": "rejected",
                    "owner_session_id": "session-owner",
                })),
                admission_rejected: true,
                ..Default::default()
            },
        };

        settle_failed_turn(&mut state, &mut dispatch, &mut failure).await;

        assert_eq!(state.turn, 0);
        assert_eq!(state.session_id.as_deref(), Some("session-current"));
        assert_eq!(state.pending_recovery.as_deref(), Some("session-owner"));
        assert!(state.last_turn_event.is_none());
        assert_eq!(ui.errors.len(), 1);
        assert!(ui.errors[0].contains("Workspace is already in use"));
        assert!(ui.errors[0].contains("astra session show session-owner"));
        assert!(!ui.errors[0].contains("/resume"));
        assert!(ui.errors[0].contains("No model or tool ran"));
        assert_eq!(ui.restored_inputs, vec!["hi"]);
    }
}
