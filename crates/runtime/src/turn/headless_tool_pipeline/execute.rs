use std::time::Instant;

use super::super::agentic::headless_round::HeadlessStderrStyle;
use super::*;
use crate::turn::agentic_loop::tool_support::edge_tool_status_exit_code;
use astra_turn_core::headless_tool_postprocess::{
    HeadlessExecutionDisposition, HeadlessOutputEnrichCtx, HeadlessOutputEnrichRequest,
    HeadlessOutputEnrichSignal, HeadlessResultQualityRequest,
    append_headless_result_quality_feedback, enrich_headless_tool_output_for_errors_and_limits,
};
use astra_turn_core::headless_tool_stderr_lines::headless_stderr_resource_limit_observed;
use astra_turn_core::hydrate_reflect::hydrate_reflect_placeholder_if_needed;

#[derive(Debug, PartialEq, Eq)]
enum HeadlessInvocationScope<'a> {
    Durable {
        run_id: &'a str,
        turn_chain_id: &'a str,
    },
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
    durable_dispatch_admission: Option<
        crate::server::tool_invocation_runtime::DurableDispatchAdmission,
    >,
    task_resolution_authority: Option<
        &astra_turn_types::task_resolution::TaskResolutionSubmissionAuthority,
    >,
    resolved_provider_policy: Option<
        &astra_turn_core::provider_resolution::ResolvedInvocationPolicy,
    >,
    permission_grant: Option<&crate::server::tool_execution_binding::ToolPermissionGrantSnapshot>,
    session_turn: u32,
    edge_round_present: bool,
) -> crate::server::runtime_tool_executor::RuntimeToolDispatchControl {
    let mut dispatch_control =
        crate::server::runtime_tool_executor::RuntimeToolDispatchControl::Continue;
    if execution.edge_result_missing {
        if let Some(executor) = runtime_tool_executor
            && !selected_runtime_provider_tool_missing_edge_result(execution, edge_round_present)
        {
            executor.set_turn_index(session_turn.max(1));
            let result = match resolve_invocation_scope(current_run_id, current_turn_chain_id) {
                HeadlessInvocationScope::Durable {
                    run_id,
                    turn_chain_id,
                } => {
                    let deferred = executor
                        .execute_invocation_before_governance(
                            run_id,
                            turn_chain_id,
                            &execution.id,
                            &execution.name,
                            &execution.args,
                            resolved_provider_policy,
                            permission_grant,
                            durable_dispatch_admission,
                            task_resolution_authority,
                        )
                        .await;
                    dispatch_control = deferred.dispatch_control;
                    execution.pending_runtime_completion = deferred.pending;
                    execution.confirmed_invocation = deferred.confirmed_invocation;
                    deferred.result
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
            apply_runtime_tool_result(execution, result);
        }
    }
    if execution.edge_result_missing {
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
    dispatch_control
}

fn apply_runtime_tool_result(
    execution: &mut super::HeadlessResolvedExecution,
    result: astra_tools::ToolResult,
) {
    execution.authoritative_is_error = Some(result.is_error);
    execution.edge_result_missing = false;
    let mut fields = result.metadata.unwrap_or_default();
    if result.is_error {
        fields.insert("status".to_string(), Value::String("failed".to_string()));
    }
    execution.tool_result_fields = (!fields.is_empty()).then_some(fields);
    execution.result_str = result.output;
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

#[cfg(test)]
mod runtime_tool_result_tests {
    use super::*;
    use serde_json::json;

    fn execution() -> super::super::HeadlessResolvedExecution {
        super::super::HeadlessResolvedExecution {
            id: "call-1".into(),
            name: "mcp__moi-tools__search_catalog_file_content".into(),
            args: json!({}),
            result_str: String::new(),
            tool_result_fields: None,
            authoritative_is_error: None,
            pending_runtime_completion: None,
            confirmed_invocation: None,
            edge_duration_ms: 0,
            is_edge_tool: false,
            edge_result_missing: false,
            edge_terminal_authority: false,
            early_exit_ms: 0,
        }
    }

    #[test]
    fn runtime_tool_error_flag_reaches_execution_semantics() {
        let mut execution = execution();
        apply_runtime_tool_result(
            &mut execution,
            astra_tools::ToolResult::error("MCP RPC failed".to_string()),
        );

        assert_eq!(
            execution
                .tool_result_fields
                .as_ref()
                .and_then(|fields| fields.get("status"))
                .and_then(Value::as_str),
            Some("failed")
        );
        assert!(execution_result_is_error(
            execution.tool_result_fields.as_ref(),
            execution.authoritative_is_error,
        ));
    }

    #[test]
    fn runtime_tool_success_metadata_is_preserved() {
        let mut execution = execution();
        let mut metadata = Map::new();
        metadata.insert("request_id".to_string(), json!("request-1"));
        apply_runtime_tool_result(
            &mut execution,
            astra_tools::ToolResult {
                output: "Error: this text came from a successful read".to_string(),
                metadata: Some(metadata),
                is_error: false,
                exit_semantics: None,
            },
        );

        assert_eq!(
            execution
                .tool_result_fields
                .as_ref()
                .and_then(|fields| fields.get("request_id")),
            Some(&json!("request-1"))
        );
        assert!(!execution_result_is_error(
            execution.tool_result_fields.as_ref(),
            execution.authoritative_is_error,
        ));
    }

    #[test]
    fn typed_runtime_success_is_not_reclassified_from_domain_status() {
        let mut execution = execution();
        apply_runtime_tool_result(
            &mut execution,
            astra_tools::ToolResult::text(
                serde_json::json!({
                    "status": "recorded",
                    "outcome": "blocked",
                    "blocker_kind": "capability_unavailable"
                })
                .to_string(),
            ),
        );

        assert_eq!(execution.authoritative_is_error, Some(false));
        assert!(!execution_result_is_error(
            execution.tool_result_fields.as_ref(),
            execution.authoritative_is_error,
        ));
    }
}

pub(super) fn execution_result_is_error(
    tool_result_fields: Option<&Map<String, Value>>,
    authoritative_is_error: Option<bool>,
) -> bool {
    let fields = tool_result_fields;

    // Producer-owned executor failures outrank generic status fields. Result
    // prose is never execution authority: it is arbitrary tool/user content.
    if fields.is_some_and(|fields| fields.contains_key("runtime_error"))
        || fields.is_some_and(|fields| fields.get("blocked").and_then(Value::as_bool) == Some(true))
        || fields.is_some_and(crate::turn::agentic_loop::tool_support::has_typed_executor_failure)
    {
        return true;
    }

    // The command executor can publish a more specific process result than
    // the enclosing edge terminal status (for example grep-no-match or a
    // benign bounded-pipeline close). This is still structured evidence.
    if let Some(is_error) = fields.and_then(structured_command_is_error) {
        return is_error;
    }
    if let Some(is_error) = authoritative_is_error {
        return is_error;
    }

    // Without a typed terminal fact the outcome is unknown, not successful.
    fields
        .and_then(|fields| fields.get("status"))
        .and_then(Value::as_str)
        .and_then(edge_tool_status_exit_code)
        .is_none_or(|exit_code| exit_code != 0)
}

fn structured_command_is_error(fields: &Map<String, Value>) -> Option<bool> {
    if let Some(semantics) = fields
        .get("exit_semantics")
        .and_then(Value::as_str)
        .and_then(|tag| {
            serde_json::from_value::<astra_tools::exit_semantics::ExitSemantics>(Value::String(
                tag.to_string(),
            ))
            .ok()
        })
    {
        return Some(semantics.is_tool_error());
    }
    fields
        .get("result_class")
        .and_then(Value::as_str)
        .and_then(|tag| {
            serde_json::from_value::<astra_tools::exit_semantics::CommandResultClass>(
                Value::String(tag.to_string()),
            )
            .ok()
        })
        .map(astra_tools::exit_semantics::CommandResultClass::is_tool_error)
}

pub(super) fn execution_error_kind(
    tool_result_fields: Option<&Map<String, Value>>,
) -> Option<astra_core::ErrorKind> {
    tool_result_fields
        .and_then(|fields| fields.get("error_kind"))
        .and_then(serde_json::Value::as_str)
        .and_then(parse_execution_error_kind_tag)
}

fn execution_recovery_evidence(
    tool_result_fields: Option<&Map<String, Value>>,
) -> Option<astra_core::ToolFailureEvidence> {
    let fields = tool_result_fields?;
    if let Some(evidence) = fields
        .get("recovery_evidence")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
    {
        return Some(evidence);
    }

    // Edge/runtime transports may only have the compact error contract. Turn
    // that typed boundary into the same evidence object used by local tools;
    // do not make the recovery layer classify a human-readable banner and
    // accidentally invent a retryable network error.
    let raw_kind = fields.get("error_kind").and_then(Value::as_str)?;
    let kind = parse_execution_error_kind_tag(raw_kind)?;
    let retryable = fields
        .get("retryable")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| kind.is_retryable());
    let mut evidence = astra_core::ToolFailureEvidence::from_error_kind(kind);
    // A transport can be known to be terminal at this boundary (for example
    // an approval timeout or a side-effect-ambiguous disconnect). Preserve
    // that producer-owned fact instead of deriving retryability from kind.
    evidence.retryable = retryable;
    Some(evidence)
}

fn parse_execution_error_kind_tag(tag: &str) -> Option<astra_core::ErrorKind> {
    astra_core::ErrorKind::parse_tag(tag).or(match tag {
        // These aliases are part of the existing Edge/Server wire contract;
        // normalize them at the boundary instead of scattering string checks
        // through recovery and turn-guard code.
        "capability_denied" | "approval_denied" | "sandbox_denied" => {
            Some(astra_core::ErrorKind::PolicyDenied)
        }
        "approval_timeout" => Some(astra_core::ErrorKind::ToolTimeout),
        "transport_unavailable" => Some(astra_core::ErrorKind::ToolUnavailable),
        _ => None,
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
            HeadlessInvocationScope::Incomplete
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

        assert!(execution_result_is_error(Some(&fields), None,));
    }

    #[test]
    fn missing_terminal_execution_fact_fails_closed() {
        assert!(execution_result_is_error(None, None));
    }

    #[test]
    fn failed_status_without_command_semantics_is_failure() {
        let mut fields = Map::new();
        fields.insert("status".to_string(), Value::String("failed".to_string()));

        assert!(execution_result_is_error(Some(&fields), None));
    }

    #[test]
    fn typed_non_error_exit_semantics_override_stale_authoritative_failure() {
        let fields = Map::from_iter([
            ("status".to_string(), Value::String("failed".to_string())),
            (
                "exit_semantics".to_string(),
                Value::String("empty_result".to_string()),
            ),
            (
                "result_class".to_string(),
                Value::String("empty_result".to_string()),
            ),
        ]);

        assert!(!execution_result_is_error(Some(&fields), Some(true),));
    }

    #[test]
    fn typed_tool_failure_is_not_erased_by_successful_process_exit() {
        let fields = Map::from_iter([
            ("exit_semantics".into(), serde_json::json!("success")),
            ("result_class".into(), serde_json::json!("success")),
            ("exit_code".into(), serde_json::json!(0)),
            ("error_kind".into(), serde_json::json!("tool_unavailable")),
            ("recovery_evidence".into(), serde_json::to_value(
                astra_tools::workspace_observation::explicit_workspace_verification_unavailable_evidence(),
            ).unwrap()),
        ]);
        assert!(execution_result_is_error(Some(&fields), Some(true)));
    }

    #[test]
    fn typed_execution_failure_remains_authoritative() {
        let fields = Map::from_iter([
            ("status".to_string(), Value::String("failed".to_string())),
            (
                "exit_semantics".to_string(),
                Value::String("execution_error".to_string()),
            ),
            (
                "result_class".to_string(),
                Value::String("execution_error".to_string()),
            ),
        ]);

        assert!(execution_result_is_error(Some(&fields), Some(true),));
    }

    #[test]
    fn successful_status_ignores_stale_error_kind_without_runtime_error() {
        let mut fields = Map::new();
        fields.insert("status".to_string(), Value::String("completed".to_string()));
        fields.insert(
            "error_kind".to_string(),
            Value::String("transport_disconnected".to_string()),
        );

        assert!(!execution_result_is_error(Some(&fields), None,));
    }

    #[test]
    fn runtime_error_still_fails_even_with_successful_status() {
        let mut fields = Map::new();
        fields.insert("status".to_string(), Value::String("completed".to_string()));
        fields.insert(
            "runtime_error".to_string(),
            serde_json::json!({"kind": "transport_disconnected"}),
        );

        assert!(execution_result_is_error(Some(&fields), None,));
    }

    #[test]
    fn compact_edge_error_aliases_normalize_to_canonical_kinds() {
        for (wire_kind, canonical) in [
            ("capability_denied", astra_core::ErrorKind::PolicyDenied),
            ("sandbox_denied", astra_core::ErrorKind::PolicyDenied),
            ("approval_timeout", astra_core::ErrorKind::ToolTimeout),
            (
                "transport_unavailable",
                astra_core::ErrorKind::ToolUnavailable,
            ),
        ] {
            let fields = Map::from_iter([(
                "error_kind".to_string(),
                Value::String(wire_kind.to_string()),
            )]);
            assert_eq!(
                execution_error_kind(Some(&fields)),
                Some(canonical),
                "wire alias {wire_kind} must normalize from typed metadata"
            );
        }
    }

    #[test]
    fn compact_edge_error_contract_produces_non_retryable_evidence() {
        let fields = Map::from_iter([
            (
                "error_kind".to_string(),
                Value::String("transport_unavailable".to_string()),
            ),
            ("retryable".to_string(), Value::Bool(false)),
        ]);
        let evidence = execution_recovery_evidence(Some(&fields)).expect("typed evidence");
        assert_eq!(evidence.kind, astra_core::ErrorKind::ToolUnavailable);
        assert_eq!(
            evidence.cause,
            astra_core::ToolFailureCause::CapabilityUnavailable
        );
        assert!(!evidence.retryable);
        assert_eq!(
            evidence.recovery_actions,
            vec![astra_core::ToolRecoveryAction::SelectAvailableCapability]
        );
    }
}

impl<'a, E: EdgeToolRoundRow> HeadlessToolExecutionPipeline<'a, E> {
    pub(super) async fn execute_execution_with_dispatch_control(
        &mut self,
        permitted: PermittedExecution,
    ) -> (
        ExecutedExecution,
        crate::server::runtime_tool_executor::RuntimeToolDispatchControl,
    ) {
        let PermittedExecution {
            mut execution,
            idem_key,
            pre_tool_context,
            resolved_provider_policy,
            permission_grant,
        } = permitted;

        self.begin_execution_trace(&execution, &idem_key);
        let tool_start = Instant::now();
        let dispatch_control = execute_tool_pure(
            &mut execution,
            self.ctx.runtime_tool_executor,
            self.ctx.api,
            self.ctx.token,
            self.ctx.current_session_id,
            self.ctx.current_run_id,
            self.ctx.current_turn_chain_id,
            self.ctx.durable_dispatch_admission,
            self.ctx.task_resolution_authority,
            resolved_provider_policy.as_ref(),
            permission_grant.as_ref(),
            self.ctx.session_turn,
            !self.ctx.edge_tool_round.is_empty(),
        )
        .await;

        let mut executed = self.postprocess_execution(execution, idem_key, tool_start);
        executed.pre_tool_context = pre_tool_context;
        (executed, dispatch_control)
    }

    /// Test helper that asserts the fixture cannot change durable dispatch
    /// control. Production callers always consume the typed transition.
    #[cfg(test)]
    pub(super) async fn execute_execution(
        &mut self,
        permitted: PermittedExecution,
    ) -> ExecutedExecution {
        let (executed, dispatch_control) = self
            .execute_execution_with_dispatch_control(permitted)
            .await;
        debug_assert_eq!(
            dispatch_control,
            crate::server::runtime_tool_executor::RuntimeToolDispatchControl::Continue,
            "unit-test execution helper cannot discard durable dispatch control"
        );
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
        // TurnGuard/ToolHealth compute result fingerprints before the durable
        // record pass.  Feed them the same executor-boundary value that the
        // model, events, and journal receive; otherwise a raw credential can
        // survive as a guessable health oracle even when the displayed text
        // is redacted later.
        let (redacted_result, _) =
            astra_tools::credential_redaction::redact_credentials_for_display(
                &execution.result_str,
            );
        execution.result_str = redacted_result;
        let mut is_err = execution_result_is_error(
            execution.tool_result_fields.as_ref(),
            execution.authoritative_is_error,
        );
        let source_error_kind = execution_error_kind(execution.tool_result_fields.as_ref());
        let source_recovery_evidence =
            execution_recovery_evidence(execution.tool_result_fields.as_ref());
        let execution_disposition = if execution.tool_result_fields.as_ref().is_some_and(|fields| {
            astra_services::session_journal::ToolCallDisposition::from_execution_metadata(
                fields.get("disposition"),
                fields.get("execution_started").and_then(Value::as_bool),
                astra_services::session_journal::ToolCallDisposition::Executed,
            ) == astra_services::session_journal::ToolCallDisposition::Rejected
        }) {
            HeadlessExecutionDisposition::RejectedBeforeExecution
        } else {
            HeadlessExecutionDisposition::Executed
        };
        let tool_already_restricted = self.ctx.restricted_tools.contains(&execution.name);
        let quiet = self.ctx.quiet;
        let term = &mut self.ctx.term;
        let mut advisories = Vec::new();
        let mut enrich_ctx = HeadlessOutputEnrichCtx {
            turn_guard: self.ctx.turn_guard,
            advisories: &mut advisories,
        };
        let resource_limit_recorded = enrich_headless_tool_output_for_errors_and_limits(
            HeadlessOutputEnrichRequest {
                name: &execution.name,
                result_str: &execution.result_str,
                is_err: &mut is_err,
                source_error_kind,
                source_recovery_evidence: source_recovery_evidence.as_ref(),
                tool_already_restricted,
                execution_disposition,
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
                }
            },
        );
        let result_quality = append_headless_result_quality_feedback(
            HeadlessResultQualityRequest {
                name: &execution.name,
                result_str: &execution.result_str,
                source_error_kind,
                execution_failed: is_err,
                execution_disposition,
                resource_limit_recorded,
            },
            self.ctx.turn_guard,
            &mut advisories,
        );
        // Executor metadata is not allowed to author runtime guidance. Replace
        // this reserved field even when the runtime generated no advisory.
        if !advisories.is_empty() || execution.tool_result_fields.is_some() {
            astra_turn_core::tool::result::advisory::set_advisories(
                execution
                    .tool_result_fields
                    .get_or_insert_with(Default::default),
                &advisories,
            );
        }

        let executed_ms = if execution.is_edge_tool && execution.edge_duration_ms > 0 {
            execution.edge_duration_ms
        } else {
            tool_start.elapsed().as_millis() as u64
        };

        // Record outcome under the canonical `(tool, args)` signature so
        // later turns can consult prior attempts before repeating work.
        let outcome_sig = astra_turn_core::tool_result_semantics::tool_health_identity(
            &execution.name,
            &execution.args,
        );
        if execution_disposition == HeadlessExecutionDisposition::Executed {
            self.ctx.turn_guard.record_tool_outcome(
                &outcome_sig,
                result_quality,
                executed_ms,
                &execution.result_str,
                source_error_kind,
            );
        }

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
