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

/// Internal, non-model metadata carrying the lossless Work-board projection.
///
/// Work receipts intentionally use a compact model-facing projection so a
/// long Work run does not replay every task's objective/expected result on
/// every round.  The live board, however, is a deterministic protocol
/// boundary and must be built from the complete typed update.  Keep that
/// boundary in the private tool-result lane rather than making the model
/// projection authoritative.
pub(crate) const CANONICAL_WORK_TASK_BOARD_UPDATE_FIELD: &str =
    "_astra_canonical_work_task_board_update";

fn tool_call_disposition_from_result_fields(
    fields: &serde_json::Map<String, Value>,
    fallback: astra_services::session_journal::ToolCallDisposition,
) -> astra_services::session_journal::ToolCallDisposition {
    fields
        .get("disposition")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .or_else(|| {
            (fields.get("execution_started").and_then(Value::as_bool) == Some(false))
                .then_some(astra_services::session_journal::ToolCallDisposition::Rejected)
        })
        .unwrap_or(fallback)
}

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

/// Persist the complete sanitized result whenever the inline presentation is
/// lossy. This is the durable-record boundary and must not be optimized for
/// the next model prompt.
fn persist_tool_result_for_record(
    current_user_id: Option<&str>,
    current_session_id: Option<&String>,
    id: &str,
    name: &str,
    full_model_result_str: &str,
    inline_model_result_str: String,
) -> String {
    // An artifact handle in the record is the lossless fallback for every
    // tool-specific presentation bound, including read_file/introspect.
    // Keeping this separate from the model boundary prevents an optimization
    // for the next prompt from silently deleting audit/recovery evidence.
    if let Some(sid) = current_session_id {
        let session_dir = model_tool_result_session_dir(current_user_id, sid)
            .expect("validated session_id must resolve tool-result session dir");
        let replacement = if full_model_result_str != inline_model_result_str {
            // A tool-specific model bound has already omitted evidence. Keep
            // that evidence recoverable even when the full result is below
            // the general large-result threshold.
            astra_turn_core::tool_result_storage::persist_tool_result_with_replacement(
                &session_dir,
                id,
                name,
                full_model_result_str,
            )
        } else {
            astra_turn_core::tool_result_storage::maybe_persist_tool_result(
                &session_dir,
                id,
                name,
                full_model_result_str,
            )
        };
        match replacement {
            Some(replacement) => replacement,
            None => inline_model_result_str,
        }
    } else {
        inline_model_result_str
    }
}

/// Select the representation appended to the next model boundary.
///
/// `read_file` and `introspect` have typed, deterministic recovery APIs. A
/// bounded source/snapshot result therefore stays inline instead of becoming
/// an opaque artifact prompt that encourages recursive pagination. The full
/// result has already gone through [`persist_tool_result_for_record`] and is
/// still available to the journal/artifact resolver.
fn model_tool_result_for_followup(
    current_user_id: Option<&str>,
    current_session_id: Option<&String>,
    id: &str,
    name: &str,
    full_model_result_str: &str,
    inline_model_result_str: String,
) -> String {
    if matches!(name, "read_file" | "introspect")
        && full_model_result_str != inline_model_result_str
    {
        return inline_model_result_str;
    }

    persist_tool_result_for_record(
        current_user_id,
        current_session_id,
        id,
        name,
        full_model_result_str,
        inline_model_result_str,
    )
}

/// Extract and validate the lossless Work-board update before the model
/// projection removes redundant task prose.  This is deliberately structural:
/// no display text or tool-result wording participates in the decision.
pub(crate) fn canonical_work_task_board_update_for_record(
    tool_name: &str,
    content: &str,
) -> Option<Value> {
    if !matches!(
        tool_name,
        "start_work" | "run_next_work_item" | "settle_work_item"
    ) {
        return None;
    }
    let value = serde_json::from_str::<Value>(content).ok()?;
    let update = value.get("task_board_update")?.clone();
    let typed: astra_server_types::WorkTaskBoardUpdateV1 =
        serde_json::from_value(update.clone()).ok()?;
    (typed.schema_version == astra_server_types::WORK_TASK_BOARD_UPDATE_SCHEMA_VERSION)
        .then_some(update)
}

fn tool_result_fields_for_model_roundtrip(
    tool_name: &str,
    full_model_result: &str,
    existing_fields: Option<&serde_json::Map<String, Value>>,
) -> Option<serde_json::Map<String, Value>> {
    let mut fields = existing_fields.cloned().unwrap_or_default();
    if let Some(update) = canonical_work_task_board_update_for_record(tool_name, full_model_result)
    {
        fields.insert(CANONICAL_WORK_TASK_BOARD_UPDATE_FIELD.to_string(), update);
    }
    (!fields.is_empty()).then_some(fields)
}

fn source_preimage_recovery_for_record(
    tool_name: &str,
    fields: &serde_json::Map<String, Value>,
) -> Option<Value> {
    (tool_name == "bash")
        .then(|| astra_tools::source_preimage::inferred_recovery_fact(fields))
        .flatten()
}

/// Build the durable argument projection without retaining credential-shaped
/// values in the journal.  The executor has already consumed the original
/// `Value`; lifecycle code that needs exact arguments must do so before this
/// record is published.  The record itself is a persistence/audit boundary,
/// so both its full JSON and short preview use the same display-safe view.
pub(crate) fn safe_tool_arguments_for_record(
    tool_name: &str,
    args: &Value,
) -> (Option<String>, Option<String>, Option<String>) {
    let mut safe_args = args.clone();
    astra_tools::credential_redaction::redact_credentials_in_json(&mut safe_args);
    let args_full = serde_json::to_string(&safe_args).ok();
    let args_preview = make_args_preview(tool_name, &safe_args).map(|preview| {
        astra_tools::credential_redaction::redact_credentials_for_display(&preview).0
    });
    let file_path = safe_args
        .get("path")
        .or_else(|| safe_args.get("file_path"))
        .and_then(Value::as_str)
        .map(ToString::to_string);
    (args_full, args_preview, file_path)
}

/// Safe short argument view for step/event projections.  The model's raw
/// arguments remain available to the in-memory executor, but every durable or
/// user-visible preview must pass this same boundary as `ToolCallRecord`.
pub(crate) fn safe_args_preview(tool_name: &str, args: &Value) -> Option<String> {
    safe_tool_arguments_for_record(tool_name, args).1
}

/// Project server-owned Work receipts for the next model boundary.
///
/// The durable journal and the task-board event keep the complete typed
/// receipt.  Repeating the same board snapshot inside every model-facing tool
/// result is redundant, however: the model already has the invocation
/// arguments and only needs the lifecycle fields that determine its next
/// action.  Keeping this projection structural (JSON keys, never prose
/// matching) preserves the board/UI contract while reducing the volatile
/// prompt suffix for prefix-cache providers.
fn work_receipt_for_model(tool_name: &str, content: &str) -> Option<String> {
    if !matches!(
        tool_name,
        "start_work" | "run_next_work_item" | "settle_work_item"
    ) {
        return None;
    }
    let value = serde_json::from_str::<Value>(content).ok()?;
    let object = value.as_object()?;
    if !object.contains_key("task_board_update") {
        return None;
    }

    fn copy_fields(source: &serde_json::Map<String, Value>, names: &[&str]) -> Value {
        let mut projected = serde_json::Map::new();
        for name in names {
            if let Some(value) = source.get(*name) {
                projected.insert((*name).to_string(), value.clone());
            }
        }
        Value::Object(projected)
    }

    fn project_task(value: &Value) -> Option<Value> {
        let task = value.as_object()?;
        Some(copy_fields(
            task,
            &[
                "item_id",
                "item_revision",
                "attempt_id",
                "execution_status",
                "declaration_state",
                "delivery_status",
                "blocker_kind",
                "status",
                "next_action",
                "outcome",
                "authority",
                "summary_authority",
            ],
        ))
    }

    fn project_board(value: &Value) -> Option<Value> {
        let board = value.as_object()?;
        let mut projected = serde_json::Map::new();
        for name in [
            "schema_version",
            "work_id",
            "branch_id",
            "graph_revision",
            "kind",
            "criteria_member_count",
        ] {
            if let Some(value) = board.get(name) {
                projected.insert(name.to_string(), value.clone());
            }
        }
        if let Some(tasks) = board.get("tasks").and_then(Value::as_array) {
            projected.insert(
                "tasks".to_string(),
                Value::Array(tasks.iter().map(project_task).collect::<Option<Vec<_>>>()?),
            );
        }
        Some(Value::Object(projected))
    }

    let mut projected = serde_json::Map::new();
    for name in [
        "activation",
        "work_id",
        "branch_id",
        "graph_revision",
        "initial_item_count",
        "status",
        "next_action",
        "outcome",
        "execution_status",
        "item_id",
        "item_revision",
        "attempt_id",
        "blocker_kind",
        "dispatch_error",
        "status_scope",
    ] {
        if let Some(value) = object.get(name) {
            projected.insert(name.to_string(), value.clone());
        }
    }
    for name in ["initial_task", "next_task"] {
        if let Some(value) = object.get(name)
            && let Some(task_object) = value.as_object()
        {
            // The assigned task is the only task whose objective and
            // expected-result text is needed at this boundary.  The original
            // declaration is already in the conversation; the board
            // snapshot only needs the live status of every item. Keeping this
            // distinction structural prevents a long multi-item Work run
            // from replaying duplicate prose on every settle.
            let mut task = project_task(value)?;
            for name in ["objective", "expected_result"] {
                if let Some(value) = task_object.get(name) {
                    task[name] = value.clone();
                }
            }
            projected.insert(name.to_string(), task);
        }
    }
    if let Some(transition) = object.get("settlement_transition").and_then(project_task) {
        projected.insert("settlement_transition".to_string(), transition);
    }
    let task_board_update = project_board(object.get("task_board_update")?)?;
    projected.insert("task_board_update".to_string(), task_board_update);

    serde_json::to_string(&Value::Object(projected)).ok()
}

fn model_tool_result_session_dir(
    current_user_id: Option<&str>,
    session_id: &str,
) -> Result<std::path::PathBuf, String> {
    let store = astra_services::local_session_artifact_store();
    match current_user_id {
        Some(user_id) => {
            let owner = astra_services::OwnerScope::user(user_id)?;
            store.session_dir_for_owner(&owner, session_id)
        }
        None => store.session_dir(session_id),
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
        // The executor may briefly hold a raw result, but no downstream
        // ledger, hook, event, journal, step recorder, or model message may.
        // Redact before any persistence or presentation so a failed edit or
        // failed tool cannot leak the very credential the model could not see.
        let initial_sanitized =
            astra_turn_core::safety_middleware::sanitize_tool_output_for_llm(&execution.result_str);
        execution.result_str = initial_sanitized.content;
        if let Some(metadata) = execution.tool_result_fields.take() {
            let sanitized =
                astra_turn_core::safety_middleware::sanitize_tool_metadata_for_persistence(
                    metadata,
                );
            execution.tool_result_fields = Some(sanitized.metadata);
        }
        // Reusable observations exclude invocation-specific PostTool
        // presentation, but they are never allowed to retain raw credentials
        // or prompt-injection payloads.
        let cache_observation = execution.result_str.clone();
        if let Some(context) = pre_tool_context {
            let context =
                astra_turn_core::safety_middleware::sanitize_tool_output_for_llm(&context).content;
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
        // Hooks are untrusted producers too; govern their output before it
        // reaches runtime reconciliation or a journal.
        execution.result_str =
            astra_turn_core::safety_middleware::sanitize_tool_output_for_llm(&execution.result_str)
                .content;
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
        execution.result_str =
            astra_turn_core::safety_middleware::sanitize_tool_output_for_llm(&finalized.output)
                .content;
        execution.tool_result_fields = finalized.metadata.map(|metadata| {
            astra_turn_core::safety_middleware::sanitize_tool_metadata_for_persistence(metadata)
                .metadata
        });
        let error_kind =
            execution_error_kind(&execution.result_str, execution.tool_result_fields.as_ref())
                .or(source_error_kind);

        let journal_result_source =
            tool_result_content_for_model_unbounded(&execution.name, &execution.result_str);
        let journal_result_inline =
            truncate_tool_result_for_model(&execution.name, &journal_result_source);
        let journal_result = persist_tool_result_for_record(
            self.ctx.current_user_id,
            self.ctx.current_session_id,
            &execution.id,
            &execution.name,
            &journal_result_source,
            journal_result_inline,
        );

        let raw_args_full = serde_json::to_string(&execution.args).ok();
        let args_size = raw_args_full
            .as_ref()
            .map(|value| u32::try_from(value.len()).unwrap_or(u32::MAX))
            .unwrap_or(0);
        let (args_full, args_preview, file_path) =
            safe_tool_arguments_for_record(&execution.name, &execution.args);
        self.ctx
            .tool_call_records
            .push(journal_record_executed_tool_call(
                execution.name.clone(),
                is_err,
                executed_ms,
                args_size,
                journal_result.as_str(),
                args_preview.clone(),
                file_path,
                args_full,
            ));
        // Fill observability fields on the just-pushed record.
        if let Some(rec) = self.ctx.tool_call_records.last_mut() {
            rec.runtime_args_full = raw_args_full;
            rec.tool_call_id = Some(execution.id.clone());
            rec.error_kind = error_kind;
            if let Some(fields) = execution.tool_result_fields.as_ref() {
                rec.disposition = Some(tool_call_disposition_from_result_fields(
                    fields,
                    rec.effective_disposition(),
                ));
                rec.exit_semantics = fields
                    .get("exit_semantics")
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string);
                rec.result_class = fields
                    .get("result_class")
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string);
                rec.workspace_mutation_observed = fields
                    .get(astra_tools::workspace_observation::OBSERVED_FIELD)
                    .and_then(serde_json::Value::as_bool);
                rec.workspace_mutation_scope = fields
                    .get(astra_tools::workspace_observation::SCOPE_FIELD)
                    .or_else(|| {
                        fields.get(astra_tools::workspace_observation::OBSERVATION_SCOPE_FIELD)
                    })
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string);
                rec.workspace_mutation_receipt = fields
                    .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                    .or_else(|| {
                        fields.get(astra_tools::workspace_observation::OBSERVATION_RECEIPT_FIELD)
                    })
                    .cloned();
                rec.external_effect_observed = fields
                    .get(astra_tools::workspace_observation::EXTERNAL_EFFECT_OBSERVED_FIELD)
                    .and_then(serde_json::Value::as_bool);
                rec.external_effect_scope = fields
                    .get(astra_tools::workspace_observation::EXTERNAL_EFFECT_SCOPE_FIELD)
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string);
                rec.external_effect_receipt = fields
                    .get(astra_tools::workspace_observation::EXTERNAL_EFFECT_RECEIPT_FIELD)
                    .cloned();
                rec.workspace_mutation_partial = fields
                    .get("workspace_mutation_partial")
                    .and_then(serde_json::Value::as_bool);
                rec.workspace_mutation_partial_paths = fields
                    .get("workspace_mutation_partial_paths")
                    .and_then(serde_json::Value::as_array)
                    .map(|paths| {
                        paths
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(ToString::to_string)
                            .collect()
                    });
                rec.source_preimage_recovery =
                    source_preimage_recovery_for_record(&execution.name, fields);
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
                    let writer = match self.ctx.current_user_id {
                        Some(user_id) => {
                            astra_services::session_journal::JournalWriter::for_user(user_id, sid)
                        }
                        None => astra_services::session_journal::JournalWriter::new(sid),
                    };
                    match writer {
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

        let observed_workspace_mutation = self.ctx.tool_call_records.last().is_some_and(|record| {
            record.name == "bash"
                && record.workspace_mutation_observed == Some(true)
                && record.workspace_mutation_scope.as_deref()
                    == Some(astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE)
                && record
                    .workspace_mutation_receipt
                    .as_ref()
                    .is_some_and(astra_tools::workspace_observation::is_changed_receipt)
        });
        if observed_workspace_mutation
            || (!is_err
                && crate::turn::tool_side_effects::tool_call_invalidates_read_cache(
                    &execution.name,
                    Some(&execution.args),
                ))
        {
            self.ctx.turn_guard.record_workspace_mutation();
            self.ctx.idempotency_cache.evict_tools(&READ_ONLY_TOOLS);
            self.ctx.semantic_dedup.clear_observation_cache();
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
        let structural_model_projection =
            work_receipt_for_model(&execution.name, &full_model_result_str);
        let model_result_str = structural_model_projection.clone().unwrap_or_else(|| {
            truncate_tool_result_for_model(&execution.name, &full_model_result_str)
        });
        // Work receipts are already durably retained in the canonical journal
        // above.  Keep the compact typed projection inline so the model can
        // act on item IDs/status without replacing it with an opaque artifact
        // handle.  Ordinary large/lossy results retain the existing artifact
        // replacement behavior.
        let model_result_fields = tool_result_fields_for_model_roundtrip(
            &execution.name,
            &full_model_result_str,
            execution.tool_result_fields.as_ref(),
        );
        let model_result_str = if structural_model_projection.is_some() {
            model_result_str
        } else {
            model_tool_result_for_followup(
                self.ctx.current_user_id,
                self.ctx.current_session_id,
                &execution.id,
                &execution.name,
                &full_model_result_str,
                model_result_str,
            )
        };

        let (mut tool_msg, tr) = openai_tool_roundtrip_values_with_result_fields(
            &execution.id,
            &execution.name,
            &model_result_str,
            model_result_fields.as_ref(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::session_journal::JournalDirGuard;
    use astra_services::session_journal::ToolCallDisposition;
    use serde_json::json;

    #[test]
    fn model_tool_result_directory_uses_authenticated_owner_scope() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = format!("owner-tool-result-{}", uuid::Uuid::new_v4());

        let actual = model_tool_result_session_dir(Some("user-a"), &session_id)
            .expect("owner-scoped session directory");
        let owner = astra_services::OwnerScope::user("user-a").expect("owner scope");
        let expected = astra_services::local_session_artifact_store()
            .session_dir_for_owner(&owner, &session_id)
            .expect("expected owner path");
        let local = astra_services::local_session_artifact_store()
            .session_dir(&session_id)
            .expect("legacy local path");

        assert_eq!(actual, expected);
        assert_ne!(actual, local);
    }

    #[test]
    fn model_result_persistence_binds_large_evidence_to_its_owner_and_call_id() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = format!("persisted-tool-result-{}", uuid::Uuid::new_v4());
        let content = "review evidence 😀\n".repeat(4_000);
        let inline = "inline preview should be replaced".to_string();

        let model_result = persist_tool_result_for_record(
            Some("reviewer-a"),
            Some(&session_id),
            "call-evidence-1",
            "git",
            &content,
            inline,
        );

        assert!(model_result.contains(
            &astra_turn_core::tool_result_storage::session_tool_result_artifact_uri(
                "call-evidence-1"
            )
        ));
        assert!(model_result.contains("introspect(artifact="));
        let dir = model_tool_result_session_dir(Some("reviewer-a"), &session_id).unwrap();
        assert_eq!(
            astra_turn_core::tool_result_storage::read_persisted_result(&dir, "call-evidence-1"),
            Some(content),
            "the model handle and durable evidence must share the same owner/session/call identity"
        );
    }

    #[test]
    fn durable_tool_argument_projection_redacts_nested_and_command_credentials() {
        let args = json!({
            "command": "python3 -c 'os.environ[\"AWS_SECRET_ACCESS_KEY\"] = \"D4w8z9wKN1aVeT3BpQj6kIuN7wH8X0M9KfV5OqzF\"' && tool --token hf_abcdefghijklmnopqrstuvwxyz123456",
            "api_key": "hf_abcdefghijklmnopqrstuvwxyz123456",
            "path": "safe.txt",
        });
        let (full, preview, file_path) = safe_tool_arguments_for_record("bash", &args);
        let full = full.expect("safe arguments must serialize");
        let preview = preview.expect("bash preview should be present");
        for secret in [
            "D4w8z9wKN1aVeT3BpQj6kIuN7wH8X0M9KfV5OqzF",
            "hf_abcdefghijklmnopqrstuvwxyz123456",
        ] {
            assert!(!full.contains(secret), "durable args leaked {secret}");
            assert!(!preview.contains(secret), "args preview leaked {secret}");
        }
        assert_eq!(file_path.as_deref(), Some("safe.txt"));
        assert!(full.contains("[REDACTED:AWS_SECRET_KEY]"));
        assert!(full.contains("[REDACTED:TOKEN_ARGUMENT]"));
        assert!(full.contains("[REDACTED:SECRET_FIELD]"));
    }

    #[test]
    fn source_recovery_projection_requires_a_validated_bash_executor_fact() {
        let fields = serde_json::Map::from_iter([(
            "source_preimage".into(),
            json!({
                "schema_version": 1,
                "source": "astra_source_preimage_store",
                "receipt_id": "00000000-0000-4000-8000-000000000001",
                "mode": "inferred_advisory",
                "guarantee": false,
                "status": "changed",
                "entries": [{"path": "evidence.bin", "status": "deleted"}],
                "restore_available": true,
            }),
        )]);
        let fact = source_preimage_recovery_for_record("bash", &fields)
            .expect("trusted Bash metadata should project");
        assert_eq!(fact["changed_paths"][0], "evidence.bin");
        assert!(source_preimage_recovery_for_record("external_tool", &fields).is_none());

        let mut forged = fields;
        forged["source_preimage"]["source"] = json!("external_tool");
        assert!(source_preimage_recovery_for_record("bash", &forged).is_none());
    }

    #[test]
    fn work_receipt_projection_is_structural_and_keeps_durable_board_shape() {
        let full = serde_json::json!({
            "activation": "start",
            "work_id": "work-1",
            "goal": "long user goal that is already in the conversation",
            "initial_item_count": 2,
            "initial_task": {
                "item_id": "task-1",
                "objective": "Inspect the source",
                "expected_result": "A cited finding",
                "task_board_update": {"tasks": [{"item_id": "task-1"}]}
            },
            "settlement_transition": {
                "authority": "canonical_work_state",
                "item_id": "task-1",
                "item_revision": 1,
                "declaration_state": "active",
                "execution_status": "completed",
                "delivery_status": "delivered",
                "summary_authority": "non_authoritative_progress_note",
                "summary": "Arbitrary contradictory progress prose"
            },
            "task_board_update": {
                "schema_version": 1,
                "work_id": "work-1",
                "branch_id": "branch-1",
                "graph_revision": 2,
                "goal": "duplicated goal",
                "tasks": [{
                    "item_id": "task-1",
                    "objective": "Inspect the source",
                    "expected_result": "A cited finding",
                    "execution_status": "running"
                }]
            },
            "opaque_internal_field": {"large": "payload"}
        })
        .to_string();

        let projected = work_receipt_for_model("start_work", &full)
            .expect("typed Work receipt should have a model projection");
        let projected: Value = serde_json::from_str(&projected).expect("valid projected JSON");
        assert_eq!(projected["initial_item_count"], 2);
        assert_eq!(projected["initial_task"]["item_id"], "task-1");
        assert_eq!(projected["initial_task"]["objective"], "Inspect the source");
        assert_eq!(
            projected["initial_task"]["expected_result"],
            "A cited finding"
        );
        assert!(projected["initial_task"]["task_board_update"].is_null());
        assert!(projected.get("goal").is_none());
        assert!(projected.get("opaque_internal_field").is_none());
        assert!(projected.get("declared_tasks").is_none());
        assert!(projected.get("runnable_items").is_none());
        assert_eq!(
            projected["settlement_transition"]["authority"],
            "canonical_work_state"
        );
        assert_eq!(
            projected["settlement_transition"]["declaration_state"],
            "active"
        );
        assert_eq!(
            projected["settlement_transition"]["delivery_status"],
            "delivered"
        );
        assert!(projected["settlement_transition"].get("summary").is_none());
        assert_eq!(projected["task_board_update"]["schema_version"], 1);
        assert_eq!(
            projected["task_board_update"]["tasks"][0]["item_id"],
            "task-1"
        );
        assert_eq!(
            projected["task_board_update"]["tasks"][0]["execution_status"],
            "running"
        );
        assert!(
            projected["task_board_update"]["tasks"][0]
                .get("objective")
                .is_none()
        );
        assert!(
            projected["task_board_update"]["tasks"][0]
                .get("expected_result")
                .is_none()
        );
    }

    #[test]
    fn canonical_work_board_update_is_lossless_and_structurally_validated() {
        let full_update = json!({
            "schema_version": 1,
            "work_id": "work-1",
            "branch_id": "branch-1",
            "kind": "snapshot",
            "goal": "Deliver the change",
            "graph_revision": 2,
            "criteria_member_count": 0,
            "tasks": [{
                "item_id": "task-1",
                "item_revision": 1,
                "objective": "Inspect the source",
                "expected_result": "A cited finding",
                "declaration_state": "active",
                "execution_status": "running",
                "delivery_status": "unreported",
                "delivery_summary": null,
                "blocker_kind": null,
                "unavailable_capabilities": []
            }]
        });
        let result = json!({
            "status": "started",
            "task_board_update": full_update,
        })
        .to_string();

        let update = canonical_work_task_board_update_for_record("start_work", &result)
            .expect("full typed update must stay in the internal result lane");
        assert_eq!(update["tasks"][0]["objective"], "Inspect the source");
        assert_eq!(update["tasks"][0]["expected_result"], "A cited finding");

        let existing = serde_json::Map::from_iter([("disposition".to_string(), json!("executed"))]);
        let fields = tool_result_fields_for_model_roundtrip("start_work", &result, Some(&existing))
            .expect("the internal roundtrip metadata must be retained");
        assert_eq!(fields["disposition"], "executed");
        assert_eq!(
            fields[CANONICAL_WORK_TASK_BOARD_UPDATE_FIELD]["tasks"][0]["expected_result"],
            "A cited finding"
        );
        let compact_model_result = work_receipt_for_model("start_work", &result)
            .expect("the provider-facing Work receipt remains compact");
        let (model_message, internal_result) = openai_tool_roundtrip_values_with_result_fields(
            "call-work",
            "start_work",
            &compact_model_result,
            Some(&fields),
        );
        assert!(
            !model_message["content"]
                .to_string()
                .contains("Inspect the source"),
            "task prose must not be duplicated into the cache-sensitive model message"
        );
        assert_eq!(
            internal_result[CANONICAL_WORK_TASK_BOARD_UPDATE_FIELD]["tasks"][0]["objective"],
            "Inspect the source"
        );

        assert!(canonical_work_task_board_update_for_record("bash", &result).is_none());
        let compact_result = json!({
            "task_board_update": {
                "schema_version": 1,
                "work_id": "work-1",
                "branch_id": "branch-1",
                "kind": "snapshot",
                "goal": "Deliver the change",
                "graph_revision": 2,
                "criteria_member_count": 0,
                "tasks": [{"item_id": "task-1"}]
            }
        })
        .to_string();
        assert!(
            canonical_work_task_board_update_for_record("start_work", &compact_result).is_none(),
            "a model-facing compact receipt is not a canonical board event"
        );
    }

    #[test]
    fn ordinary_tool_results_do_not_use_work_projection() {
        assert!(work_receipt_for_model("read_file", r#"{"task_board_update":{}}"#).is_none());
        assert!(work_receipt_for_model("settle_work_item", "not json").is_none());
        assert!(
            work_receipt_for_model(
                "settle_work_item",
                r#"{"task_board_update":{"tasks":["malformed"]}}"#
            )
            .is_none()
        );
    }

    #[test]
    fn lossy_model_bound_persists_evidence_below_large_result_threshold() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = format!("bounded-tool-result-{}", uuid::Uuid::new_v4());
        let content = "bounded evidence\n".repeat(1_200);
        assert!(
            content.chars().count() < astra_turn_core::tool_result_storage::PERSIST_THRESHOLD_CHARS,
            "test must exercise the lossy presentation boundary, not the size threshold"
        );

        let model_result = persist_tool_result_for_record(
            Some("reviewer-a"),
            Some(&session_id),
            "call-bounded-1",
            "bash",
            &content,
            "bounded preview".to_string(),
        );

        assert!(model_result.contains("artifact://session/tool-result/"));
        assert!(model_result.contains("introspect(artifact="));
        let dir = model_tool_result_session_dir(Some("reviewer-a"), &session_id).unwrap();
        assert_eq!(
            astra_turn_core::tool_result_storage::read_persisted_result(&dir, "call-bounded-1"),
            Some(content)
        );
    }

    #[test]
    fn bounded_read_file_stays_inline_and_uses_line_range_recovery() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = format!("inline-read-file-{}", uuid::Uuid::new_v4());
        let content = "line\n".repeat(1_200);
        let inline = astra_turn_core::tool_result_sanitize::truncate_tool_result_for_model(
            "read_file",
            &content,
        );
        assert_ne!(
            content, inline,
            "fixture must cross the read_file model cap"
        );

        let model_result = model_tool_result_for_followup(
            Some("reviewer-a"),
            Some(&session_id),
            "call-read-file-1",
            "read_file",
            &content,
            inline.clone(),
        );

        assert_eq!(model_result, inline);
        assert!(
            !model_result.contains("introspect(artifact="),
            "read_file should advertise its native start_line/end_line recovery"
        );

        let record_result = persist_tool_result_for_record(
            Some("reviewer-a"),
            Some(&session_id),
            "call-read-file-record-1",
            "read_file",
            &content,
            inline,
        );
        assert!(record_result.contains("artifact://session/tool-result/"));
        let dir = model_tool_result_session_dir(Some("reviewer-a"), &session_id).unwrap();
        assert_eq!(
            astra_turn_core::tool_result_storage::read_persisted_result(
                &dir,
                "call-read-file-record-1"
            ),
            Some(content)
        );
    }

    #[test]
    fn bounded_introspect_stays_inline_and_uses_facets_for_recovery() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = format!("inline-introspect-{}", uuid::Uuid::new_v4());
        let content = "snapshot\n".repeat(1_200);
        let inline = astra_turn_core::tool_result_sanitize::truncate_tool_result_for_model(
            "introspect",
            &content,
        );
        assert_ne!(content, inline, "fixture must cross the generic model cap");

        let model_result = model_tool_result_for_followup(
            Some("reviewer-a"),
            Some(&session_id),
            "call-introspect-1",
            "introspect",
            &content,
            inline.clone(),
        );

        assert_eq!(model_result, inline);
        assert!(
            !model_result.contains("introspect(artifact="),
            "introspect should use typed facet requests rather than recursively paging its own snapshot"
        );

        let record_result = persist_tool_result_for_record(
            Some("reviewer-a"),
            Some(&session_id),
            "call-introspect-record-1",
            "introspect",
            &content,
            inline,
        );
        assert!(record_result.contains("artifact://session/tool-result/"));
        let dir = model_tool_result_session_dir(Some("reviewer-a"), &session_id).unwrap();
        assert_eq!(
            astra_turn_core::tool_result_storage::read_persisted_result(
                &dir,
                "call-introspect-record-1"
            ),
            Some(content)
        );
    }

    #[test]
    fn result_that_never_started_is_rejected_not_executed() {
        let fields = json!({
            "execution_started": false,
            "error_kind": "transport_unavailable",
            "failure_scope": "executor_transport",
        })
        .as_object()
        .unwrap()
        .clone();

        assert_eq!(
            tool_call_disposition_from_result_fields(&fields, ToolCallDisposition::Executed),
            ToolCallDisposition::Rejected
        );
    }

    #[test]
    fn explicit_disposition_remains_authoritative() {
        let fields = json!({
            "execution_started": false,
            "disposition": "deferred",
        })
        .as_object()
        .unwrap()
        .clone();

        assert_eq!(
            tool_call_disposition_from_result_fields(&fields, ToolCallDisposition::Executed),
            ToolCallDisposition::Deferred
        );
    }
}
