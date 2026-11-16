use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use serde_json::{Value, json};
use uuid::Uuid;

use crate::server::tool_execution_binding::WorkspaceBinding;
use crate::server::tool_execution_result::workspace_path_mismatch_tool_result;
use crate::server::tool_plan_gate::{is_plan_mode_blocked_tool, plan_mode_blocked_tool_result};
use crate::server::tool_workspace_path_guard::server_sandbox_tool_path_mismatch;

pub(crate) enum LocalToolPreflight {
    Continue,
    ShortCircuit(astra_tools::ToolResult),
}

pub(crate) struct LocalToolPreflightContext<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) workspace_root: &'a Path,
    pub(crate) workspace_binding: &'a WorkspaceBinding,
    pub(crate) approval_gate: Option<&'a dyn astra_tools::ToolApprovalGate>,
    pub(crate) plan_mode_authoring_active: bool,
}

pub(crate) fn validate_local_tool_arguments(name: &str, args: &Value) -> LocalToolPreflight {
    if let Err(error) = astra_tools::schemas::validate_tool_arguments(name, args) {
        return LocalToolPreflight::ShortCircuit(error.into_tool_result());
    }
    LocalToolPreflight::Continue
}

pub(crate) async fn run_local_tool_policy_preflight(
    context: LocalToolPreflightContext<'_>,
    name: &str,
    args: &Value,
) -> LocalToolPreflight {
    if context.plan_mode_authoring_active && is_plan_mode_blocked_tool(name, args) {
        return LocalToolPreflight::ShortCircuit(plan_mode_blocked_tool_result(name));
    }

    if let Some(reason) = server_sandbox_tool_path_mismatch(
        name,
        args,
        context.workspace_root,
        context.workspace_binding,
    ) {
        return LocalToolPreflight::ShortCircuit(workspace_path_mismatch_tool_result(reason));
    }

    if let Some(result) = super::tool_approval_preflight::approval_preflight_result(
        context.session_id,
        context.approval_gate,
        name,
        args,
    )
    .await
    {
        return LocalToolPreflight::ShortCircuit(result);
    }

    LocalToolPreflight::Continue
}

pub(crate) fn unknown_local_tool_result(name: &str) -> astra_tools::ToolResult {
    let mut result = astra_tools::ToolResult::error(
        json!({
            "status": "failed",
            "error": format!(
                "Tool `{name}` is not available in the current runtime tool surface. The runtime did not advertise this tool for this turn; use a visible tool, select an advertised deferred tool with tool_search, or bind the required capacity provider."
            ),
            "error_kind": astra_core::ErrorKind::ToolNotFound.as_str(),
            "retryable": false,
        })
        .to_string(),
    );
    result.metadata = Some(serde_json::Map::from_iter([
        (
            "error_kind".to_string(),
            json!(astra_core::ErrorKind::ToolNotFound.as_str()),
        ),
        ("disposition".to_string(), json!("rejected")),
        ("execution_started".to_string(), json!(false)),
    ]));
    result
}

pub(crate) fn memory_args_with_context(
    args: &Value,
    session_id: &str,
    user_id: &str,
    turn_index: u32,
    producer_id: &str,
) -> Value {
    astra_tools::memoria::MemoriaToolGateway::args_with_runtime_context(
        args,
        Some(session_id),
        Some(user_id),
        turn_index,
        producer_id,
    )
}

pub(crate) fn normalize_local_tool_result_output(
    name: &str,
    result: &mut astra_tools::ToolResult,
    aggregate_output_bytes: &AtomicUsize,
) {
    result.output = astra_tools::normalize_empty_output(std::mem::take(&mut result.output), name);
    // The headless recording pipeline owns model-facing truncation and,
    // crucially, has the session + tool-call id required to make an oversized
    // result recoverable. Truncating or globally persisting here would discard
    // evidence before that owner can create a session-scoped artifact handle.
    aggregate_output_bytes.fetch_add(result.output.len(), Ordering::Relaxed);
}

pub(crate) struct LocalToolExecutionLifecycle<'a> {
    pub(crate) aggregate_output_bytes: &'a AtomicUsize,
    pub(crate) progress_callback: Option<&'a dyn astra_tools::ToolProgressCallback>,
}

impl<'a> LocalToolExecutionLifecycle<'a> {
    pub(crate) async fn start(&self, name: &str, args: &Value) -> String {
        let call_id = format!("{name}-{}", Uuid::new_v4());
        if let Some(callback) = self.progress_callback {
            callback.tool_started(&call_id, name, args).await;
        }
        call_id
    }

    pub(crate) async fn finish(
        &self,
        name: &str,
        call_id: &str,
        mut result: astra_tools::ToolResult,
    ) -> astra_tools::ToolResult {
        // Progress callbacks are an earlier server event lane than runtime
        // governance and durable recording. This lifecycle owns server-local
        // tool execution, so it is the correct place to issue an edit-capable
        // reference (the later central pass is only a display-only fallback).
        let (redacted_output, _) =
            astra_text_utils::credential_redaction::redact_credentials_for_display(&result.output);
        result.output = redacted_output;
        normalize_local_tool_result_output(name, &mut result, self.aggregate_output_bytes);
        if let Some(callback) = self.progress_callback {
            callback
                .tool_completed(call_id, &result.output, !result.is_error)
                .await;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_local_tool_result_is_fail_closed_and_user_readable() {
        let result = unknown_local_tool_result("delegate");

        assert!(result.is_error);
        let parsed: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(parsed["status"], "failed");
        assert_eq!(
            parsed["error_kind"],
            astra_core::ErrorKind::ToolNotFound.as_str()
        );
        assert_eq!(parsed["retryable"], false);
        let error = parsed["error"].as_str().unwrap();
        assert!(error.contains("Tool `delegate` is not available"));
        assert!(!result.output.contains("Available:"));
        assert!(!result.output.contains("mo_query"));
        assert!(!result.output.contains("powershell"));
        assert!(error.contains("current runtime tool surface"));
        let metadata = result.metadata.expect("typed rejection metadata");
        assert_eq!(
            metadata["error_kind"],
            astra_core::ErrorKind::ToolNotFound.as_str()
        );
        assert_eq!(metadata["disposition"], "rejected");
        assert_eq!(metadata["execution_started"], false);
    }

    #[test]
    fn server_preflight_rejects_invalid_arguments_before_policy_and_execution() {
        let result = validate_local_tool_arguments(
            "memory",
            &json!({"action": "forget", "memory_id": "m1"}),
        );

        let LocalToolPreflight::ShortCircuit(result) = result else {
            panic!("invalid built-in arguments must short-circuit server execution");
        };
        assert_eq!(
            result
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("error_kind"))
                .and_then(Value::as_str),
            Some(astra_core::ErrorKind::ToolInvalidArgs.as_str())
        );
    }

    #[test]
    fn memory_args_include_session_user_and_turn_without_action() {
        let args = memory_args_with_context(
            &json!({
                "action": "recall",
                "query": "closed loop",
            }),
            "session-1",
            "user-1",
            12,
            "run-1",
        );

        assert!(args.get("action").is_none());
        assert_eq!(args["session_id"].as_str(), Some("session-1"));
        assert_eq!(args["user_id"].as_str(), Some("user-1"));
        assert_eq!(args["turn"].as_u64(), Some(12));
        assert_eq!(args["_attribution_id"].as_str(), Some("run-1"));
    }

    #[test]
    fn normalize_local_tool_result_output_fills_empty_and_counts_bytes() {
        let aggregate = AtomicUsize::new(0);
        let mut result = astra_tools::ToolResult::text("   \n".to_string());

        normalize_local_tool_result_output("bash", &mut result, &aggregate);

        assert_eq!(result.output, "(bash completed with no output)");
        assert_eq!(aggregate.load(Ordering::Relaxed), result.output.len());
    }

    #[test]
    fn normalize_large_output_keeps_full_evidence_for_session_recording() {
        let aggregate = AtomicUsize::new(0);
        let output = "review evidence\n".repeat(6_000);
        let mut result = astra_tools::ToolResult::text(output.clone());

        normalize_local_tool_result_output("git", &mut result, &aggregate);

        assert_eq!(result.output, output);
        assert_eq!(aggregate.load(Ordering::Relaxed), output.len());
    }

    #[tokio::test]
    async fn tool_success_does_not_claim_an_unattributed_memory_was_useful() {
        let session_id = format!("unattributed-memory-{}", uuid::Uuid::new_v4());
        astra_tools::memoria::MemoriaToolGateway::reset_session_process_state(&session_id);
        astra_tools::memoria::MemoriaToolGateway::record_recall_for_producer(
            &session_id,
            Some("user-1"),
            "run-1",
            1,
            vec!["memory-1".into()],
        );
        let aggregate = AtomicUsize::new(0);
        let lifecycle = LocalToolExecutionLifecycle {
            aggregate_output_bytes: &aggregate,
            progress_callback: None,
        };
        let result = lifecycle
            .finish("bash", "call-1", astra_tools::ToolResult::text("ok".into()))
            .await;
        assert!(!result.is_error);
        assert_eq!(
            astra_tools::memoria::MemoriaToolGateway::latest_recall(&session_id)
                .unwrap()
                .memory_ids,
            vec!["memory-1"]
        );
        astra_tools::memoria::MemoriaToolGateway::reset_session_process_state(&session_id);
    }
}
