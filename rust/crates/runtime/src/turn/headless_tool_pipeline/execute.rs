use std::time::Instant;

use super::super::agentic_headless_round::HeadlessStderrStyle;
use super::super::headless_tool_assembly::CACHEABLE_TOOLS;
use super::super::headless_tool_postprocess::{
    HeadlessOutputEnrichSignal, append_headless_result_quality_feedback,
    enrich_headless_tool_output_for_errors_and_limits,
};
use super::super::headless_tool_stderr_lines::{
    headless_stderr_resource_limit_blocked, headless_stderr_resource_limit_in_output,
};
use super::super::hydrate_reflect::hydrate_reflect_placeholder_if_needed;
use super::*;
use crate::turn::tool_result_semantics::is_tool_error;

/// The sentinel error prefix emitted by `take_edge_output_for_tool_call_with_duration`
/// when no edge agent matched the tool call.
const EDGE_PROTOCOL_ERROR_PREFIX: &str = "Error: headless edge protocol";

impl<'a, E: EdgeToolRoundRow> HeadlessToolExecutionPipeline<'a, E> {
    pub(super) async fn execute_execution(
        &mut self,
        permitted: PermittedExecution,
    ) -> ExecutedExecution {
        let PermittedExecution {
            mut execution,
            idem_key,
        } = permitted;

        // ── Server-side tool execution fallback ────────────────────────
        // When no edge agent is connected (web-only mode), the edge tool
        // round is empty and `resolve_headless_tool_execution` returns the
        // edge protocol error.  If a server tool executor is available,
        // execute the tool directly on the server instead.
        if !execution.is_edge_tool && execution.result_str.starts_with(EDGE_PROTOCOL_ERROR_PREFIX) {
            if let Some(executor) = self.ctx.server_tool_executor {
                executor.set_turn_index(self.ctx.turn_index.min(u32::MAX as usize) as u32);
                let result = executor
                    .execute_with_metadata(&execution.name, &execution.args)
                    .await;
                execution.tool_result_fields = result.metadata;
                execution.result_str = result.output;
            }
        }

        execution.result_str = hydrate_reflect_placeholder_if_needed(
            self.ctx.api,
            self.ctx.token,
            self.ctx.current_session_id,
            &execution.name,
            &execution.args,
            execution.result_str,
        )
        .await;

        let tool_start = Instant::now();
        let tool_idem_key = if CACHEABLE_TOOLS.contains(&execution.name.as_str()) {
            Some(idem_key.cache_key())
        } else {
            None
        };
        self.ctx.step_recorder.begin_tool_with_key(
            &execution.name,
            &execution.id,
            tool_idem_key.as_deref(),
        );

        if let Some(emitter) = self.ctx.progress_emitter {
            emitter.tool_executing(&execution.name, self.ctx.turn_index as u32);
        }

        let mut is_err = is_tool_error(&execution.result_str);
        let tool_already_restricted = self.ctx.restricted_tools.contains(&execution.name);
        let quiet = self.ctx.quiet;
        let term = &mut self.ctx.term;
        let resource_limit_recorded = enrich_headless_tool_output_for_errors_and_limits(
            &execution.name,
            &mut execution.result_str,
            &mut is_err,
            tool_already_restricted,
            self.ctx.turn_guard,
            self.ctx.restricted_tools,
            |sig| {
                if quiet {
                    return;
                }
                match sig {
                    HeadlessOutputEnrichSignal::ResourceLimitBlocked { tool } => {
                        term.emit_line(
                            HeadlessStderrStyle::Yellow,
                            headless_stderr_resource_limit_blocked(&tool),
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
        let _result_quality = append_headless_result_quality_feedback(
            &execution.name,
            &mut execution.result_str,
            resource_limit_recorded,
            self.ctx.turn_guard,
        );

        let executed_ms = if execution.is_edge_tool && execution.edge_duration_ms > 0 {
            execution.edge_duration_ms
        } else {
            tool_start.elapsed().as_millis() as u64
        };

        ExecutedExecution {
            execution,
            idem_key,
            is_err,
            executed_ms,
        }
    }
}
