use std::time::Instant;

use super::super::agentic::headless_round::HeadlessStderrStyle;
use super::*;
use crate::turn::agentic_loop::tool_support::edge_tool_status_exit_code;
use astra_turn_core::headless_tool_postprocess::{
    HeadlessOutputEnrichCtx, HeadlessOutputEnrichRequest, HeadlessOutputEnrichSignal,
    append_headless_result_quality_feedback, enrich_headless_tool_output_for_errors_and_limits,
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
const EDGE_PROTOCOL_ERROR_PREFIX: &str = super::HEADLESS_EDGE_PROTOCOL_ERROR_PREFIX;

#[derive(Debug, PartialEq, Eq)]
enum HeadlessInvocationScope<'a> {
    Durable {
        run_id: &'a str,
        turn_chain_id: &'a str,
    },
    LegacyUnscoped,
    Incomplete,
}

fn resolve_invocation_scope<'a>(
    run_id: Option<&'a str>,
    turn_chain_id: Option<&'a str>,
) -> HeadlessInvocationScope<'a> {
    match (run_id, turn_chain_id) {
        (Some(run_id), Some(turn_chain_id))
            if !run_id.trim().is_empty() && !turn_chain_id.trim().is_empty() =>
        {
            HeadlessInvocationScope::Durable {
                run_id,
                turn_chain_id,
            }
        }
        (None, None) => HeadlessInvocationScope::LegacyUnscoped,
        _ => HeadlessInvocationScope::Incomplete,
    }
}

/// Pure execution: server-side tool execution + hydration.
/// No &mut pipeline needed — only shared refs.
pub(crate) async fn execute_tool_pure(
    execution: &mut super::HeadlessResolvedExecution,
    runtime_tool_executor: Option<&crate::server::runtime_tool_executor::RuntimeToolExecutor>,
    api: &ThinClient,
    token: &str,
    current_session_id: Option<&String>,
    current_run_id: Option<&str>,
    current_turn_chain_id: Option<&str>,
    resolved_provider_policy: Option<
        &astra_turn_core::provider_resolution::ResolvedInvocationPolicy,
    >,
    permission_grant: Option<&crate::server::tool_execution_binding::ToolPermissionGrantSnapshot>,
    session_turn: u32,
    edge_round_present: bool,
) {
    if !execution.is_edge_tool && execution.result_str.starts_with(EDGE_PROTOCOL_ERROR_PREFIX) {
        if let Some(executor) = runtime_tool_executor
            && !selected_runtime_provider_tool_missing_edge_result(execution, edge_round_present)
        {
            executor.set_turn_index(session_turn.max(1));
            let result = match resolve_invocation_scope(current_run_id, current_turn_chain_id) {
                HeadlessInvocationScope::Durable {
                    run_id,
                    turn_chain_id,
                } => {
                    executor
                        .execute_invocation_with_metadata(
                            run_id,
                            turn_chain_id,
                            &execution.id,
                            &execution.name,
                            &execution.args,
                            resolved_provider_policy,
                            permission_grant,
                        )
                        .await
                }
                HeadlessInvocationScope::LegacyUnscoped => {
                    executor
                        .execute_with_metadata(&execution.name, &execution.args)
                        .await
                }
                HeadlessInvocationScope::Incomplete => astra_tools::ToolResult::error(
                    serde_json::json!({
                        "status": "failed",
                        "error": "incomplete runtime tool invocation identity",
                        "error_kind": astra_core::ErrorKind::ToolBinding.as_str(),
                        "retryable": false,
                    })
                    .to_string(),
                ),
            };
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

fn selected_runtime_provider_tool_missing_edge_result(
    execution: &super::HeadlessResolvedExecution,
    edge_round_present: bool,
) -> bool {
    if !edge_round_present || execution.is_edge_tool {
        return false;
    }
    let registry = astra_runtime_env::ToolRegistry::builtins();
    registry.get(&execution.name).is_some_and(|spec| {
        matches!(
            spec.required.executor,
            astra_runtime_env::RequiredExecutor::RuntimeExecutor
                | astra_runtime_env::RequiredExecutor::ServiceOrRuntimeExecutor
        )
    })
}

pub(super) fn execution_result_is_error(
    name: &str,
    result_str: &str,
    tool_result_fields: Option<&Map<String, Value>>,
) -> bool {
    let metadata_failed = tool_result_fields.is_some_and(|fields| {
        let status_exit_code = fields
            .get("status")
            .and_then(serde_json::Value::as_str)
            .and_then(edge_tool_status_exit_code);
        let failed_status = status_exit_code.is_some_and(|exit_code| exit_code != 0);
        let successful_status = status_exit_code == Some(0);
        let runtime_error = fields.get("runtime_error").is_some();
        let blocked = fields
            .get("blocked")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let explicit_error_kind = fields.get("error_kind").is_some();

        failed_status || runtime_error || blocked || (explicit_error_kind && !successful_status)
    });

    match classify_tool_error(name, result_str) {
        ToolErrorSeverity::HardError => true,
        ToolErrorSeverity::InfrastructureError => true,
        ToolErrorSeverity::SoftError => metadata_failed,
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
    result_str: &str,
    tool_result_fields: Option<&Map<String, Value>>,
) -> Option<astra_core::ErrorKind> {
    tool_result_fields
        .and_then(|fields| fields.get("error_kind"))
        .and_then(serde_json::Value::as_str)
        .and_then(astra_core::ErrorKind::parse_tag)
        .or_else(|| structured_output_error_kind(result_str))
}

fn execution_recovery_evidence(
    tool_result_fields: Option<&Map<String, Value>>,
) -> Option<astra_core::ToolFailureEvidence> {
    tool_result_fields
        .and_then(|fields| fields.get("recovery_evidence"))
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

fn structured_output_error_kind(result_str: &str) -> Option<astra_core::ErrorKind> {
    serde_json::from_str::<Value>(result_str)
        .ok()
        .and_then(|value| {
            value
                .get("error_kind")
                .and_then(Value::as_str)
                .and_then(astra_core::ErrorKind::parse_tag)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::{Map, Value};

    #[test]
    fn invocation_scope_rejects_partial_or_blank_identity() {
        assert_eq!(
            resolve_invocation_scope(Some("run-1"), Some("turn-1")),
            HeadlessInvocationScope::Durable {
                run_id: "run-1",
                turn_chain_id: "turn-1",
            }
        );
        assert_eq!(
            resolve_invocation_scope(None, None),
            HeadlessInvocationScope::LegacyUnscoped
        );
        assert_eq!(
            resolve_invocation_scope(Some("run-1"), None),
            HeadlessInvocationScope::Incomplete
        );
        assert_eq!(
            resolve_invocation_scope(Some("  "), Some("turn-1")),
            HeadlessInvocationScope::Incomplete
        );
    }

    #[test]
    fn transport_failure_metadata_marks_read_only_tool_as_error() {
        let mut fields = Map::new();
        fields.insert("status".to_string(), Value::String("failed".to_string()));
        fields.insert(
            "error_kind".to_string(),
            Value::String("transport_disconnected".to_string()),
        );
        fields.insert("blocked".to_string(), Value::Bool(true));

        assert!(execution_result_is_error(
            "list_dir",
            "Error: transport 'edge_ws' disconnected or timed out while executing tool 'list_dir'",
            Some(&fields),
        ));
    }

    #[test]
    fn plain_read_only_timeout_without_runtime_metadata_stays_soft() {
        assert!(!execution_result_is_error(
            "grep",
            "Error: command timed out after 30s",
            None,
        ));
    }

    #[test]
    fn explicit_success_body_can_override_stale_failed_status() {
        let mut fields = Map::new();
        fields.insert("status".to_string(), Value::String("failed".to_string()));

        assert!(!execution_result_is_error(
            "str_replace",
            "Replaced 1 occurrence\n<<<ASTRA_TOOL_OK>>>",
            Some(&fields),
        ));
    }

    #[test]
    fn successful_status_ignores_stale_error_kind_without_runtime_error() {
        let mut fields = Map::new();
        fields.insert("status".to_string(), Value::String("completed".to_string()));
        fields.insert(
            "error_kind".to_string(),
            Value::String("transport_disconnected".to_string()),
        );

        assert!(!execution_result_is_error("list_dir", "ok", Some(&fields)));
    }

    #[test]
    fn runtime_error_still_fails_even_with_successful_status() {
        let mut fields = Map::new();
        fields.insert("status".to_string(), Value::String("completed".to_string()));
        fields.insert(
            "runtime_error".to_string(),
            serde_json::json!({"kind": "transport_disconnected"}),
        );

        assert!(execution_result_is_error("list_dir", "ok", Some(&fields)));
    }
}

impl<'a, E: EdgeToolRoundRow> HeadlessToolExecutionPipeline<'a, E> {
    pub(super) async fn execute_execution(
        &mut self,
        permitted: PermittedExecution,
    ) -> ExecutedExecution {
        let PermittedExecution {
            mut execution,
            idem_key,
            pre_tool_context,
            resolved_provider_policy,
            permission_grant,
        } = permitted;

        self.begin_execution_trace(&execution, &idem_key);
        let tool_start = Instant::now();
        execute_tool_pure(
            &mut execution,
            self.ctx.runtime_tool_executor,
            self.ctx.api,
            self.ctx.token,
            self.ctx.current_session_id,
            self.ctx.current_run_id,
            self.ctx.current_turn_chain_id,
            resolved_provider_policy.as_ref(),
            permission_grant.as_ref(),
            self.ctx.session_turn,
            !self.ctx.edge_tool_round.is_empty(),
        )
        .await;

        let mut executed = self.postprocess_execution(execution, idem_key, tool_start);
        executed.pre_tool_context = pre_tool_context;
        executed
    }

    /// Apply the canonical execution-outcome semantics after a provider has
    /// returned. Serial and concurrent tool paths both call this boundary so
    /// error attribution, TurnGuard evidence, and journal disposition cannot
    /// drift based on scheduling mode.
    pub(super) fn postprocess_execution(
        &mut self,
        mut execution: super::HeadlessResolvedExecution,
        idem_key: IdempotencyKey,
        tool_start: Instant,
    ) -> ExecutedExecution {
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
        let source_error_kind =
            execution_error_kind(&execution.result_str, execution.tool_result_fields.as_ref());
        let source_recovery_evidence =
            execution_recovery_evidence(execution.tool_result_fields.as_ref());
        let tool_already_restricted = self.ctx.restricted_tools.contains(&execution.name);
        let quiet = self.ctx.quiet;
        let term = &mut self.ctx.term;
        let mut enrich_ctx = HeadlessOutputEnrichCtx {
            turn_guard: self.ctx.turn_guard,
        };
        let resource_limit_recorded = enrich_headless_tool_output_for_errors_and_limits(
            HeadlessOutputEnrichRequest {
                name: &execution.name,
                result_str: &mut execution.result_str,
                is_err: &mut is_err,
                source_error_kind,
                source_recovery_evidence: source_recovery_evidence.as_ref(),
                tool_already_restricted,
            },
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
            is_err,
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
            source_error_kind,
        );

        ExecutedExecution {
            execution,
            idem_key,
            pre_tool_context: None,
            is_err,
            error_kind: source_error_kind,
            executed_ms,
        }
    }
}
