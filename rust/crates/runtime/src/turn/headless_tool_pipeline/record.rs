use std::time::Duration;

use astra_services::SessionArtifactStore;

use super::super::agentic_headless_round::HeadlessStderrStyle;
use super::*;
use astra_turn_core::edge_prompt_context::make_args_preview;
use astra_turn_core::headless_tool_assembly::{
    READ_ONLY_TOOLS, openai_tool_roundtrip_values_with_result_fields,
};
use astra_turn_core::headless_tool_body_preview::emit_headless_tool_body_preview;
use astra_turn_core::headless_tool_journal::journal_record_executed_tool_call;
use astra_turn_core::headless_tool_postprocess::{
    HeadlessCacheableRecordCtx, format_headless_tool_duration,
    record_headless_cacheable_success_and_semantic_hint, try_write_light_headless_step_checkpoint,
};
use astra_turn_core::headless_tool_status_display::{tool_call_detail, tool_result_summary};
use astra_turn_core::headless_tool_stderr_lines::{
    headless_stderr_error_preview_line, headless_stderr_tool_error_detail_line,
    headless_stderr_tool_error_line, headless_stderr_tool_ok_line,
};
use astra_turn_core::tool_result_sanitize::tool_result_content_for_model;

fn emit_tool_display_feedback(
    quiet: bool,
    term: &mut dyn HeadlessRoundTerminal,
    name: &str,
    args: &Value,
    result_str: &str,
    is_err: bool,
    is_edge_tool: bool,
    executed_ms: u64,
) {
    if !quiet && !is_edge_tool {
        let duration_str = format_headless_tool_duration(Duration::from_millis(executed_ms));
        let detail = tool_call_detail(name, args);
        let summary = if !is_err {
            tool_result_summary(name, result_str)
        } else {
            None
        };
        if is_err {
            term.emit_line(
                HeadlessStderrStyle::Red,
                headless_stderr_tool_error_line(name, &duration_str, detail.as_deref()),
            );
            if let Some(first_line) = result_str.lines().next() {
                let preview = headless_stderr_error_preview_line(first_line, 100);
                term.emit_line(
                    HeadlessStderrStyle::Dim,
                    headless_stderr_tool_error_detail_line(&preview),
                );
            }
        } else {
            term.emit_line(
                HeadlessStderrStyle::Green,
                headless_stderr_tool_ok_line(
                    name,
                    &duration_str,
                    detail.as_deref(),
                    summary.as_deref(),
                ),
            );
        }
    }

    if !is_edge_tool {
        emit_headless_tool_body_preview(term, quiet, name, result_str, is_err);
    }
}

fn maybe_persist_model_tool_result(
    current_session_id: Option<&String>,
    id: &str,
    name: &str,
    model_result_str: String,
) -> String {
    if let Some(sid) = current_session_id {
        let session_dir = astra_services::local_session_artifact_store()
            .session_dir(sid)
            .expect("validated session_id must resolve tool-result session dir");
        match astra_turn_core::tool_result_storage::maybe_persist_tool_result(
            &session_dir,
            id,
            name,
            &model_result_str,
        ) {
            Some(replacement) => replacement,
            None => model_result_str,
        }
    } else {
        model_result_str
    }
}

impl<'a, E: EdgeToolRoundRow> HeadlessToolExecutionPipeline<'a, E> {
    pub(super) async fn record_execution(&mut self, executed: ExecutedExecution) {
        let ExecutedExecution {
            mut execution,
            idem_key,
            is_err,
            executed_ms,
        } = executed;
        if !self.ctx.tool_event_hooks.is_empty() && !is_err {
            if let Some(modified) = crate::skills::hooks::evaluate_post_tool_hooks(
                self.ctx.tool_event_hooks,
                &execution.name,
                &execution.args,
                &execution.result_str,
            )
            .await
            {
                execution.result_str = modified;
            }
        }

        let args_json = serde_json::to_string(&execution.args).ok();
        let args_size = args_json.as_ref().map(|s| s.len() as u32).unwrap_or(0);
        let args_preview = make_args_preview(&execution.name, &execution.args);
        let args_full = args_json;
        let file_path = execution
            .args
            .get("path")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        self.ctx
            .tool_call_records
            .push(journal_record_executed_tool_call(
                execution.name.clone(),
                is_err,
                executed_ms,
                args_size,
                execution.result_str.as_str(),
                args_preview.clone(),
                file_path,
                args_full,
            ));
        // Fill observability fields on the just-pushed record.
        if let Some(rec) = self.ctx.tool_call_records.last_mut() {
            if let Some(start) = self.ctx.turn_start {
                rec.start_offset_ms =
                    Some((start.elapsed().as_millis() as u64).saturating_sub(executed_ms));
            }
            rec.round = Some(self.ctx.llm_round);
        }
        self.ctx
            .step_recorder
            .complete_tool_with_result_and_metadata(
                &execution.name,
                &execution.id,
                args_preview.as_deref(),
                is_err,
                executed_ms,
                false,
                &execution.result_str,
            );
        self.executed_this_turn += 1;

        if let Some(sid) = self.ctx.current_session_id {
            try_write_light_headless_step_checkpoint(sid, self.ctx.step_recorder);
        }

        if !is_err
            && crate::turn::tool_side_effects::tool_call_invalidates_read_cache(
                &execution.name,
                Some(&execution.args),
            )
        {
            self.ctx.idempotency_cache.evict_tools(READ_ONLY_TOOLS);
        }

        if !is_err && READ_ONLY_TOOLS.contains(&execution.name.as_str()) {
            record_headless_cacheable_success_and_semantic_hint(
                &execution.name,
                &execution.args,
                &idem_key,
                HeadlessCacheableRecordCtx {
                    result_str: &mut execution.result_str,
                    turn_index: self.ctx.turn_index,
                    idempotency_cache: self.ctx.idempotency_cache,
                    step_recorder: self.ctx.step_recorder,
                    semantic_dedup: self.ctx.semantic_dedup,
                },
            );
        }

        emit_tool_display_feedback(
            self.ctx.quiet,
            self.ctx.term,
            &execution.name,
            &execution.args,
            &execution.result_str,
            is_err,
            execution.is_edge_tool,
            executed_ms,
        );

        let model_result_str =
            tool_result_content_for_model(&execution.name, &execution.result_str);
        let model_result_str = maybe_persist_model_tool_result(
            self.ctx.current_session_id,
            &execution.id,
            &execution.name,
            model_result_str,
        );

        let (mut tool_msg, tr) = openai_tool_roundtrip_values_with_result_fields(
            &execution.id,
            &execution.name,
            &model_result_str,
            execution.tool_result_fields.as_ref(),
        );
        // Add metadata for compression (P6) and folding (P0):
        // - _round_index: Current-round tool results should never be truncated
        //   because the LLM hasn't seen them yet.
        // - _tool_name: Enables proactive folding of old read-only tool results.
        if let Some(obj) = tool_msg.as_object_mut() {
            obj.insert(
                "_round_index".to_string(),
                serde_json::Value::Number(self.ctx.llm_round.into()),
            );
            obj.insert(
                "_tool_name".to_string(),
                serde_json::Value::String(execution.name.clone()),
            );
        }
        self.ctx.messages.push(tool_msg);
        self.ctx.tool_results.push(tr);
    }
}
