use serde_json::Value;
use uuid::Uuid;

use super::tool_execution_result::approval_timeout_tool_result;

pub(crate) async fn approval_preflight_result(
    session_id: &str,
    gate: Option<&dyn astra_tools::ToolApprovalGate>,
    tool_name: &str,
    args: &Value,
) -> Option<astra_tools::ToolResult> {
    let gate = gate?;
    if !gate.requires_approval_for(tool_name, args) {
        return None;
    }

    let request_id = format!("srv-{session_id}-{}", Uuid::new_v4());
    match gate.request_approval(&request_id, tool_name, args).await {
        astra_tools::ApprovalDecision::Approved => None,
        astra_tools::ApprovalDecision::Denied { reason } => {
            let msg = reason.unwrap_or_else(|| "User denied execution".into());
            Some(astra_tools::ToolResult::error(format!(
                "Tool execution denied: {msg}"
            )))
        }
        astra_tools::ApprovalDecision::Timeout => Some(approval_timeout_tool_result()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct DenyInvocationGate;

    #[async_trait::async_trait]
    impl astra_tools::ToolApprovalGate for DenyInvocationGate {
        async fn request_approval(
            &self,
            _request_id: &str,
            _tool_name: &str,
            _args: &Value,
        ) -> astra_tools::ApprovalDecision {
            astra_tools::ApprovalDecision::Denied {
                reason: Some("blocked by test".to_string()),
            }
        }

        fn requires_approval(&self, _tool_name: &str) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn preflight_uses_invocation_approval_for_git_mutating_action() {
        let result = approval_preflight_result(
            "session-1",
            Some(&DenyInvocationGate),
            "git",
            &json!({"action": "commit", "message": "ship"}),
        )
        .await;

        let result = result.expect("mutating git action must require approval");
        assert!(result.is_error);
        assert!(result.output.contains("blocked by test"));
    }

    #[tokio::test]
    async fn preflight_skips_invocation_approval_for_git_read_only_action() {
        let result = approval_preflight_result(
            "session-1",
            Some(&DenyInvocationGate),
            "git",
            &json!({"action": "diff"}),
        )
        .await;

        assert!(result.is_none());
    }
}
