use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use astra_core::SharedPool;
use astra_services::resource_governor::ResourceGovernor;
use serde_json::{Value, json};
use tracing::Instrument;
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
    astra_tools::ToolResult::error(
        json!({
            "status": "failed",
            "error": format!(
                "Tool `{name}` is not available in the current runtime tool surface. The runtime did not advertise this tool for this turn; use a visible tool, select an advertised deferred tool with tool_search, or bind the required capacity provider."
            ),
            "error_kind": astra_core::ErrorKind::ToolNotFound.as_str(),
            "retryable": false,
        })
        .to_string(),
    )
}

pub(crate) fn spawn_resource_tool_call_recording(
    user_id: &str,
    resource_governor: Option<&Arc<dyn ResourceGovernor>>,
) -> bool {
    let Some(governor) = resource_governor.cloned() else {
        return false;
    };
    let user_id = user_id.to_string();
    tokio::spawn(
        async move {
            governor.record_tool_calls(&user_id, 1).await;
            tracing::debug!(
                target: "astra_runtime::local_tool",
                user_id = %user_id,
                "resource governor tool call recorded"
            );
        }
        .in_current_span(),
    );
    true
}

pub(crate) async fn record_preview_template_missing(
    user_id: &str,
    session_id: &str,
    context_manifest_pool: Option<&SharedPool>,
    tool_name: &str,
) -> bool {
    let Some(pool) = context_manifest_pool else {
        return false;
    };
    let store = astra_services::DatabaseContextManifestStore::new(pool.clone());
    if let Err(error) = store
        .preview_template_budget_or_fallback(user_id, session_id, None, tool_name)
        .await
    {
        tracing::warn!(
            target: "astra_runtime::tool_preview",
            session_id = %session_id,
            tool_name,
            error = %error,
            "failed to persist preview_template_missing event"
        );
    }
    true
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

pub(crate) fn spawn_memory_recall_feedback_after_success(
    session_id: &str,
    producer_id: &str,
    name: &str,
    result: &astra_tools::ToolResult,
    memoria_client: &astra_tools::memoria::MemoriaToolGateway,
) -> bool {
    if name == "memory" || result.is_error {
        return false;
    }

    let snapshots = astra_tools::memoria::MemoriaToolGateway::drain_recalls_for_producer(
        session_id,
        producer_id,
        None,
    );
    if snapshots.is_empty() {
        return false;
    }
    let session_id = session_id.to_string();
    let context = format!("server-tool:{name}");
    let client = astra_tools::memoria::MemoriaToolGateway::new(
        memoria_client.cloud_base.clone(),
        memoria_client.cloud_token.clone(),
    );
    tokio::spawn(
        async move {
            let report = client
                .feedback_recall_snapshots(snapshots, "useful", &context)
                .await;
            if report.attempted > 0 {
                tracing::debug!(
                    session_id = %session_id,
                    context = %context,
                    attempted = report.attempted,
                    succeeded = report.succeeded,
                    failed = report.failed,
                    "closed recall feedback after successful tool"
                );
            }
            if report.failed > 0 {
                tracing::warn!(
                    target: "astra_runtime::local_tool",
                    session_id = %session_id,
                    context = %context,
                    failed = report.failed,
                    "recall feedback had failures"
                );
            }
        }
        .in_current_span(),
    );
    true
}

pub(crate) struct LocalToolExecutionLifecycle<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) producer_id: &'a str,
    pub(crate) aggregate_output_bytes: &'a AtomicUsize,
    pub(crate) memoria_client: &'a astra_tools::memoria::MemoriaToolGateway,
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
            astra_tools::credential_redaction::redact_credentials_for_display(&result.output);
        result.output = redacted_output;
        normalize_local_tool_result_output(name, &mut result, self.aggregate_output_bytes);
        spawn_memory_recall_feedback_after_success(
            self.session_id,
            self.producer_id,
            name,
            &result,
            self.memoria_client,
        );

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
    }

    #[test]
    fn resource_tool_call_recording_skips_missing_governor() {
        assert!(!spawn_resource_tool_call_recording("user-1", None));
    }

    #[tokio::test]
    async fn preview_template_missing_skips_missing_pool() {
        assert!(!record_preview_template_missing("user-1", "session-1", None, "ghost_tool").await);
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

    #[test]
    fn memory_recall_feedback_skips_memory_tool_and_errors() {
        let client = astra_tools::memoria::MemoriaToolGateway::new(None, None);
        let ok_memory = astra_tools::ToolResult::text("ok".to_string());
        let failed = astra_tools::ToolResult::error("Error: denied".to_string());

        assert!(!spawn_memory_recall_feedback_after_success(
            "session-1",
            "run-1",
            "memory",
            &ok_memory,
            &client,
        ));
        assert!(!spawn_memory_recall_feedback_after_success(
            "session-1",
            "run-1",
            "bash",
            &failed,
            &client,
        ));
    }

    #[tokio::test]
    async fn memory_recall_feedback_takes_ledger_ownership_before_returning() {
        let session_id = format!("feedback-ownership-transfer-{}", uuid::Uuid::new_v4());
        astra_tools::memoria::MemoriaToolGateway::reset_session_process_state(&session_id);
        astra_tools::memoria::MemoriaToolGateway::record_recall_for_producer(
            &session_id,
            Some("user-1"),
            "run-1",
            1,
            vec!["memory-1".into()],
        );
        let client = astra_tools::memoria::MemoriaToolGateway::new(
            Some("http://127.0.0.1:1".into()),
            Some("test-token".into()),
        );

        assert!(spawn_memory_recall_feedback_after_success(
            &session_id,
            "run-1",
            "bash",
            &astra_tools::ToolResult::text("ok".into()),
            &client,
        ));
        assert_eq!(
            astra_tools::memoria::MemoriaToolGateway::pending_recall_count(&session_id),
            0,
            "run-boundary cleanup must not race the feedback worker for ledger ownership"
        );
        astra_tools::memoria::MemoriaToolGateway::reset_session_process_state(&session_id);
    }
}
