use std::time::Duration;

use astra_services::SessionArtifactStore;

use super::super::agentic::headless_round::HeadlessStderrStyle;
use super::execute::execution_error_kind;
use super::*;
use astra_turn_core::edge_prompt_context::make_args_preview;
use astra_turn_core::headless_tool_assembly::{
    READ_ONLY_TOOLS, openai_tool_roundtrip_values_with_result_fields,
};
use astra_turn_core::headless_tool_body_preview::emit_headless_tool_body_preview;
use astra_turn_core::headless_tool_journal::journal_record_executed_tool_call;
use astra_turn_core::headless_tool_postprocess::{
    HeadlessCacheableRecordCtx, format_headless_tool_duration,
    record_headless_cacheable_success_and_semantic_hint_if_ok,
    try_write_light_headless_step_checkpoint,
};
use astra_turn_core::headless_tool_status_display::{
    tool_call_detail, tool_error_summary, tool_result_summary,
};
use astra_turn_core::headless_tool_stderr_lines::{
    headless_stderr_error_preview_line, headless_stderr_tool_error_detail_line,
    headless_stderr_tool_error_line, headless_stderr_tool_ok_line,
};
use astra_turn_core::tool_result_sanitize::{
    tool_result_content_for_model_unbounded, truncate_tool_result_for_model,
};

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
            let summary = tool_error_summary(name, result_str);
            let preview = headless_stderr_error_preview_line(&summary, 100);
            term.emit_line(
                HeadlessStderrStyle::Dim,
                headless_stderr_tool_error_detail_line(&preview),
            );
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
    full_model_result_str: &str,
    inline_model_result_str: String,
) -> String {
    if let Some(sid) = current_session_id {
        let session_dir = astra_services::local_session_artifact_store()
            .session_dir(sid)
            .expect("validated session_id must resolve tool-result session dir");
        match astra_turn_core::tool_result_storage::maybe_persist_tool_result(
            &session_dir,
            id,
            name,
            full_model_result_str,
        ) {
            Some(replacement) => replacement,
            None => inline_model_result_str,
        }
    } else {
        inline_model_result_str
    }
}

fn truncate_tool_error(result_str: &str) -> String {
    // Take the first non-empty line as the error summary, truncated to 200 chars.
    result_str
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("(no output)")
        .chars()
        .take(200)
        .collect()
}

impl<'a, E: EdgeToolRoundRow> HeadlessToolExecutionPipeline<'a, E> {
    pub(super) async fn record_execution(&mut self, executed: ExecutedExecution) {
        let ExecutedExecution {
            mut execution,
            idem_key,
            pre_tool_context,
            mut is_err,
            error_kind: source_error_kind,
            executed_ms,
        } = executed;
        // Reusable observations exclude invocation-specific PostTool
        // presentation, but they are never allowed to retain raw credentials
        // or prompt-injection payloads.
        let cache_observation =
            astra_turn_core::safety_middleware::sanitize_tool_output_for_llm(&execution.result_str)
                .content;
        if let Some(context) = pre_tool_context {
            execution
                .result_str
                .push_str(&format!("\n\n[Hook context]: {context}"));
        }
        let mut post_tool_modified = false;
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
                post_tool_modified = true;
            }
        }
        let exit_semantics = execution
            .tool_result_fields
            .as_ref()
            .and_then(|metadata| metadata.get("exit_semantics"))
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok());
        let governed = crate::server::runtime_tool_executor::govern_runtime_tool_result(
            astra_tools::ToolResult {
                output: execution.result_str,
                metadata: execution.tool_result_fields,
                is_error: is_err,
                exit_semantics,
            },
            post_tool_modified,
        );
        let finalized = if let Some(pending) = execution.pending_runtime_completion.take() {
            let executor = self
                .ctx
                .runtime_tool_executor
                .expect("only a runtime executor can create a pending tool completion");
            executor
                .finish_governed_tool_result(governed, Some(pending))
                .await
        } else {
            governed.into_inner()
        };
        is_err = finalized.is_error;
        let error_kind = execution_error_kind(&finalized.output, finalized.metadata.as_ref())
            .or(source_error_kind);
        execution.result_str = finalized.output;
        execution.tool_result_fields = finalized.metadata;

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
            rec.tool_call_id = Some(execution.id.clone());
            rec.error_kind = error_kind;
            if let Some(fields) = execution.tool_result_fields.as_ref() {
                rec.disposition = fields
                    .get("disposition")
                    .cloned()
                    .and_then(|value| serde_json::from_value(value).ok())
                    .or(rec.disposition);
                rec.exit_semantics = fields
                    .get("exit_semantics")
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string);
                rec.result_class = fields
                    .get("result_class")
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string);
            }
            if let Some(start) = self.ctx.turn_start {
                rec.start_offset_ms =
                    Some((start.elapsed().as_millis() as u64).saturating_sub(executed_ms));
            }
            rec.round = Some(self.ctx.llm_round);
        }

        // Emit a ToolCallError journal event when a tool fails.
        // This closes the gap where non-zero bash exits weren't surfaced
        // to introspect/reflect because they weren't promoted to error events.
        if is_err {
            if let Some(sid) = self.ctx.current_session_id {
                if let Some(rec) = self.ctx.tool_call_records.last() {
                    let error_msg = format!(
                        "tool '{}' failed: {}",
                        execution.name,
                        truncate_tool_error(&execution.result_str)
                    );
                    let event = astra_services::session_journal::JournalEvent::tool_call_error(
                        Some(sid),
                        self.ctx.session_turn,
                        &execution.name,
                        &error_msg,
                        rec.clone(),
                    );
                    match astra_services::session_journal::JournalWriter::new(sid) {
                        Ok(journal) => {
                            let _ = journal.append(&event);
                        }
                        Err(err) => {
                            tracing::warn!(
                                target: "astra_runtime::headless_tool_pipeline",
                                session_id = %sid,
                                err = %err,
                                "failed to open journal for ToolCallError event"
                            );
                        }
                    }
                }
            }
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

        if let (Some(user_id), Some(sid)) = (self.ctx.current_user_id, self.ctx.current_session_id)
        {
            try_write_light_headless_step_checkpoint(user_id, sid, self.ctx.step_recorder);
        }

        if !is_err
            && crate::turn::tool_side_effects::tool_call_invalidates_read_cache(
                &execution.name,
                Some(&execution.args),
            )
        {
            self.ctx.turn_guard.record_workspace_mutation();
            self.ctx.idempotency_cache.evict_tools(&READ_ONLY_TOOLS);
            self.ctx.semantic_dedup.clear_observation_cache();
            self.ctx.call_counts.clear();
        }

        if READ_ONLY_TOOLS.contains(&execution.name.as_str()) {
            // Cache and compare the provider observation, not presentation
            // transforms from the current Pre/PostTool hook set. Reuse applies
            // the then-current hooks again after authorization.
            record_headless_cacheable_success_and_semantic_hint_if_ok(
                &execution.name,
                &execution.args,
                &idem_key,
                HeadlessCacheableRecordCtx {
                    observation: &cache_observation,
                    result_str: &mut execution.result_str,
                    call_id: Some(&execution.id),
                    turn_index: self.ctx.turn_index,
                    semantic_context_generation: self.ctx.turn_guard.workspace_epoch(),
                    idempotency_cache: self.ctx.idempotency_cache,
                    step_recorder: self.ctx.step_recorder,
                    semantic_dedup: self.ctx.semantic_dedup,
                },
                is_err,
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

        let full_model_result_str =
            tool_result_content_for_model_unbounded(&execution.name, &execution.result_str);
        let model_result_str =
            truncate_tool_result_for_model(&execution.name, &full_model_result_str);
        let model_result_str = maybe_persist_model_tool_result(
            self.ctx.current_session_id,
            &execution.id,
            &execution.name,
            &full_model_result_str,
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
