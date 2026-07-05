use std::time::Instant;

use super::super::agentic::headless_round::HeadlessStderrStyle;
use super::*;
use crate::turn::agentic_loop::tool_support::edge_tool_status_exit_code;
use astra_turn_core::headless_tool_postprocess::{
    HeadlessOutputEnrichCtx, HeadlessOutputEnrichSignal, append_headless_result_quality_feedback,
    enrich_headless_tool_output_for_errors_and_limits,
};
use astra_turn_core::headless_tool_stderr_lines::{
    headless_stderr_resource_limit_in_output, headless_stderr_resource_limit_observed,
};
use astra_turn_core::hydrate_reflect::hydrate_reflect_placeholder_if_needed;
use astra_turn_core::tool_result_semantics::{
    ToolErrorSeverity, classify_tool_error, tool_output_has_explicit_success_signal,
};

/// The sentinel error prefix emitted by `take_edge_output_for_tool_call_with_duration`
/// when no edge agent matched the tool call.
const EDGE_PROTOCOL_ERROR_PREFIX: &str = "Error: headless edge protocol";

/// Pure execution: server-side tool execution + hydration.
/// No &mut pipeline needed — only shared refs.
pub(crate) async fn execute_tool_pure(
    execution: &mut super::HeadlessResolvedExecution,
    runtime_tool_executor: Option<&crate::server::runtime_tool_executor::RuntimeToolExecutor>,
    api: &ThinClient,
    token: &str,
    current_session_id: Option<&String>,
    session_turn: u32,
) {
    if !execution.is_edge_tool && execution.result_str.starts_with(EDGE_PROTOCOL_ERROR_PREFIX) {
        if let Some(executor) = runtime_tool_executor {
            executor.set_turn_index(session_turn.max(1));
            let mut server_args = execution.args.clone();
            if let Some(obj) = server_args.as_object_mut() {
                obj.insert(
                    "_tool_call_id".to_string(),
                    serde_json::Value::String(execution.id.clone()),
                );
            }
            let result = executor
                .execute_with_metadata(&execution.name, &server_args)
                .await;
            execution.tool_result_fields = result.metadata;
            execution.result_str = result.output;
        }
    }
    if execution.result_str.starts_with(EDGE_PROTOCOL_ERROR_PREFIX) {
        let fields = execution.tool_result_fields.get_or_insert_with(Map::new);
        fields.insert("status".to_string(), Value::String("failed".to_string()));
        fields.insert(
            "error_kind".to_string(),
            Value::String(astra_core::ErrorKind::ToolBinding.as_str().to_string()),
        );
        fields.insert(
            "finish_reason".to_string(),
            Value::String("tool_binding".to_string()),
        );
    }

    execution.result_str = hydrate_reflect_placeholder_if_needed(
        api,
        token,
        current_session_id,
        &execution.name,
        &execution.args,
        std::mem::take(&mut execution.result_str),
    )
    .await;
}

pub(super) fn execution_result_is_error(
    name: &str,
    result_str: &str,
    tool_result_fields: Option<&Map<String, Value>>,
) -> bool {
    let metadata_failed = tool_result_fields
        .and_then(|fields| fields.get("status"))
        .and_then(serde_json::Value::as_str)
        .and_then(edge_tool_status_exit_code)
        .is_some_and(|exit_code| exit_code != 0);

    match classify_tool_error(name, result_str) {
        ToolErrorSeverity::HardError => true,
        ToolErrorSeverity::InfrastructureError => true,
        ToolErrorSeverity::SoftError => false,
        // Success arm — body-wins reconciliation contract:
        //
        // When edge metadata says the call failed (non-zero exit status) but
        // the visible result body says it succeeded, the body MUST win. This
        // prevents a real mutation (e.g. a successful `str_replace`) from
        // being recorded as a failed tool call when transport metadata is
        // stale or inconsistent.
        //
        // The signal we trust is `tool_output_has_explicit_success_signal`,
        // which keys on the stable `TOOL_SUCCESS_SENTINEL` emitted by
        // file-mutation tools. A mutation emitter that does NOT emit the
        // sentinel will fall back to legacy prose matching, and if neither
        // matches, a stale failed status will be recorded. Therefore any new
        // mutation emitter MUST append the sentinel on success.
        ToolErrorSeverity::Success => {
            metadata_failed && !tool_output_has_explicit_success_signal(result_str)
        }
    }
}

fn execution_error_kind(
    tool_result_fields: Option<&Map<String, Value>>,
) -> Option<astra_core::ErrorKind> {
    tool_result_fields
        .and_then(|fields| fields.get("error_kind"))
        .and_then(serde_json::Value::as_str)
        .and_then(astra_core::ErrorKind::parse_tag)
}

impl<'a, E: EdgeToolRoundRow> HeadlessToolExecutionPipeline<'a, E> {
    pub(super) async fn execute_execution(
        &mut self,
        permitted: PermittedExecution,
    ) -> ExecutedExecution {
        let PermittedExecution {
            mut execution,
            idem_key,
        } = permitted;

        self.begin_execution_trace(&execution, &idem_key);
        let tool_start = Instant::now();
        execute_tool_pure(
            &mut execution,
            self.ctx.runtime_tool_executor,
            self.ctx.api,
            self.ctx.token,
            self.ctx.current_session_id,
            self.ctx.session_turn,
        )
        .await;

        // P1 (tool-design-gaps plan): use `classify_tool_error` so that
        // soft errors (read_file ENOENT, str_replace not-unique, grep
        // no-match) are NOT counted as ToolCallFailed. Only HardError
        // (permission denied, disk full, sandbox violation) is a real
        // failure. Before this fix, any result starting with "Error:"
        // was marked as failed via `is_tool_error`, which inflated
        // ToolHealthTracker failure rates and caused CLI exit code 1
        // even on expected-negative tool outcomes.
        let mut is_err = execution_result_is_error(
            &execution.name,
            &execution.result_str,
            execution.tool_result_fields.as_ref(),
        );
        let source_error_kind = execution_error_kind(execution.tool_result_fields.as_ref());
        let tool_already_restricted = self.ctx.restricted_tools.contains(&execution.name);
        let quiet = self.ctx.quiet;
        let term = &mut self.ctx.term;
        let mut enrich_ctx = HeadlessOutputEnrichCtx {
            turn_guard: self.ctx.turn_guard,
        };
        let resource_limit_recorded = enrich_headless_tool_output_for_errors_and_limits(
            &execution.name,
            &mut execution.result_str,
            &mut is_err,
            source_error_kind,
            tool_already_restricted,
            &mut enrich_ctx,
            |sig| {
                if quiet {
                    return;
                }
                match sig {
                    HeadlessOutputEnrichSignal::ResourceLimitObserved { tool } => {
                        term.emit_line(
                            HeadlessStderrStyle::Dim,
                            headless_stderr_resource_limit_observed(&tool),
                        );
                    }
                    HeadlessOutputEnrichSignal::ResourceLimitDetectedInOutput { tool } => {
                        term.emit_line(
                            HeadlessStderrStyle::Dim,
                            headless_stderr_resource_limit_in_output(&tool),
                        );
                    }
                }
            },
        );
        let result_quality = append_headless_result_quality_feedback(
            &execution.name,
            &mut execution.result_str,
            source_error_kind,
            resource_limit_recorded,
            self.ctx.turn_guard,
        );

        let executed_ms = if execution.is_edge_tool && execution.edge_duration_ms > 0 {
            execution.edge_duration_ms
        } else {
            tool_start.elapsed().as_millis() as u64
        };

        // Record outcome under the canonical `(tool, args)` signature so
        // later turns can consult prior attempts before repeating work.
        let outcome_sig = astra_turn_core::tool_result_semantics::tool_dedup_signature(
            &execution.name,
            &execution.args,
        );
        self.ctx.turn_guard.record_tool_outcome(
            &outcome_sig,
            result_quality,
            executed_ms,
            &execution.result_str,
        );

        ExecutedExecution {
            execution,
            idem_key,
            is_err,
            executed_ms,
        }
    }
}
