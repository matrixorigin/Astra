use std::time::Instant;

use crate::cli::session::session_state::SessionState;
use super::turn_cancellation::apply_user_cancelled_turn;
use super::turn_entry::TurnContext;
use super::turn_failure_reporting::report_turn_failure;
use super::turn_success::apply_turn_success_async;
use super::*;

pub(crate) struct TurnDispatch<'a, 'b> {
    pub(crate) ctx: &'a TurnContext<'b>,
    pub(crate) line: &'a str,
    pub(crate) effective_line: &'a str,
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
    )
    .await;
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
    )
    .await;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct CollectingUi;

    impl crate::cli::ui_adapter::ReplUiAdapter for CollectingUi {
        fn show_error(&mut self, _msg: &str) {}
        fn show_warning(&mut self, _msg: &str) {}
        fn show_info(&mut self, _msg: &str) {}
        fn show_status(&mut self, _msg: &str) {}
        fn blank_line(&mut self) {}
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

    #[tokio::test]
    async fn settle_successful_turn_clears_last_turn_interrupted() {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let ctx = TurnContext {
            api: &api,
            profile: None,
        };
        let mut ui = CollectingUi;
        let mut state = SessionState {
            last_turn_interrupted: true,
            ..SessionState::default()
        };
        let mut dispatch = TurnDispatch {
            ctx: &ctx,
            line: "continue",
            effective_line: "continue",
            token: "token",
            session_id: None,
            semantic_query_override: None,
            turn_start: Instant::now(),
            ui: &mut ui,
        };

        settle_successful_turn(&mut state, &mut dispatch, stub_stream_result("done")).await;

        assert!(!state.last_turn_interrupted);
        assert_eq!(state.history.len(), 1);
    }
}
