//! Permission gate for tool execution.
//!
//! This module provides permission checking before tool execution.
//! When a tool is blocked by permissions, it returns an error result
//! instead of executing, or requests permission from the parent agent.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

use crate::messaging::router::AgentMailbox;
use crate::orchestration::permission_sync::{
    PermissionMode, PermissionRequest, PermissionSyncContext, PermissionUpdate,
};
use crate::turn::tool_argument_hints::{
    normalize_llm_function_arguments, permission_prompt_primary_detail,
};

/// Result of a permission check.
#[derive(Debug, Clone)]
pub enum PermissionCheckResult {
    /// Permission granted — proceed with tool execution.
    Allowed,
    /// Permission denied — return this error message instead of executing.
    Denied { reason: String },
    /// Permission granted after requesting from parent.
    AllowedViaRequest {
        /// New rules received from parent (for logging/debug).
        new_rules: Vec<PermissionUpdate>,
    },
}

/// Check permission for a tool call.
///
/// Flow:
/// 1. If no permission_context, always allow (legacy mode)
/// 2. Check PermissionSyncContext.is_allowed()
/// 3. If denied and have mailbox, request permission from the parent
/// 4. Return result
pub async fn check_tool_permission(
    tool_name: &str,
    args: Option<&str>,
    permission_context: Option<&Arc<RwLock<PermissionSyncContext>>>,
    mailbox: Option<&mut AgentMailbox>,
    timeout: Duration,
) -> PermissionCheckResult {
    // No permission context = legacy mode, always allow
    let Some(ctx) = permission_context else {
        return PermissionCheckResult::Allowed;
    };

    let normalized_args = args
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .map(|parsed| normalize_llm_function_arguments(&parsed))
        .unwrap_or_else(|| serde_json::json!({}));
    let permission_hint = permission_prompt_primary_detail(tool_name, &normalized_args);
    let rule_match_hint = permission_hint.as_deref();

    // Check local permission rules
    {
        let ctx_guard = ctx.read().await;
        if ctx_guard.is_allowed(tool_name, rule_match_hint) {
            return PermissionCheckResult::Allowed;
        }

        if ctx_guard.is_denied(tool_name, rule_match_hint) {
            drop(ctx_guard);
            ctx.write().await.record_blocked_tool(tool_name);
            return PermissionCheckResult::Denied {
                reason: format!("Tool '{}' denied by permission rules", tool_name),
            };
        }

        // If mode is Deny, don't even try to request
        if ctx_guard.mode() == PermissionMode::Deny {
            drop(ctx_guard);
            ctx.write().await.record_blocked_tool(tool_name);
            return PermissionCheckResult::Denied {
                reason: format!("Tool '{}' denied by permission mode", tool_name),
            };
        }
    }

    // Try to request permission from parent
    let Some(mailbox) = mailbox else {
        ctx.write().await.record_blocked_tool(tool_name);
        return PermissionCheckResult::Denied {
            reason: format!(
                "Tool '{}' requires permission but no parent available",
                tool_name
            ),
        };
    };

    // Build permission request
    let mut request = PermissionRequest::new(tool_name, normalized_args)
        .with_reason(format!("Requesting permission to use tool: {}", tool_name));
    if let Some(ref hint) = permission_hint {
        request = request.with_hint(hint.clone());
    }

    // Send request and wait for response
    ctx.write().await.record_permission_request();
    match mailbox.request_permission(request, timeout).await {
        Ok(response) => {
            if response.approved {
                // Apply any new rules to our context
                let new_rules = response.updates.clone();
                {
                    let mut ctx_guard = ctx.write().await;
                    ctx_guard.record_permission_approved();
                    for update in &new_rules {
                        ctx_guard.apply_update(update);
                    }
                }
                PermissionCheckResult::AllowedViaRequest { new_rules }
            } else {
                ctx.write().await.record_blocked_tool(tool_name);
                PermissionCheckResult::Denied {
                    reason: response
                        .reason
                        .unwrap_or_else(|| "Permission denied by parent".to_string()),
                }
            }
        }
        Err(e) => {
            ctx.write().await.record_blocked_tool(tool_name);
            PermissionCheckResult::Denied {
                reason: format!("Permission request failed: {}", e),
            }
        }
    }
}

/// Format a permission-denied error for tool result.
pub fn permission_denied_error_result(tool_name: &str, reason: &str) -> String {
    format!(
        "Error: Permission denied for tool '{}'. Reason: {}",
        tool_name, reason
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestration::permission_sync::{InheritedPermissions, PermissionRule};
    use std::collections::HashSet;

    #[tokio::test]
    async fn no_context_always_allowed() {
        let result = check_tool_permission(
            "edit",
            Some("src/main.rs"),
            None,
            None,
            Duration::from_secs(5),
        )
        .await;
        assert!(matches!(result, PermissionCheckResult::Allowed));
    }

    #[tokio::test]
    async fn allowed_by_rules() {
        let inherited = InheritedPermissions {
            mode: PermissionMode::Auto,
            allow_rules: vec![PermissionRule::parse("edit")],
            deny_rules: vec![],
            ask_rules: vec![],
            allowed_tools: None,
            is_background: false,
        };
        let ctx = Arc::new(RwLock::new(PermissionSyncContext::new(inherited)));

        let result =
            check_tool_permission("edit", None, Some(&ctx), None, Duration::from_secs(5)).await;
        assert!(matches!(result, PermissionCheckResult::Allowed));
    }

    #[tokio::test]
    async fn denied_by_deny_mode() {
        let inherited = InheritedPermissions {
            mode: PermissionMode::Deny,
            allow_rules: vec![],
            deny_rules: vec![],
            ask_rules: vec![],
            allowed_tools: None,
            is_background: true,
        };
        let ctx = Arc::new(RwLock::new(PermissionSyncContext::new(inherited)));

        let result =
            check_tool_permission("edit", None, Some(&ctx), None, Duration::from_secs(5)).await;
        assert!(matches!(result, PermissionCheckResult::Denied { .. }));
    }

    #[tokio::test]
    async fn denied_without_parent() {
        let inherited = InheritedPermissions {
            mode: PermissionMode::Auto,
            allow_rules: vec![],
            deny_rules: vec![],
            ask_rules: vec![],
            allowed_tools: Some(HashSet::from(["view".to_string()])), // edit not allowed
            is_background: false,
        };
        let ctx = Arc::new(RwLock::new(PermissionSyncContext::new(inherited)));

        let result =
            check_tool_permission("edit", None, Some(&ctx), None, Duration::from_secs(5)).await;
        assert!(matches!(result, PermissionCheckResult::Denied { .. }));

        let ctx_guard = ctx.read().await;
        let telemetry = ctx_guard.telemetry();
        assert_eq!(telemetry.permission_requests, 0);
        assert_eq!(telemetry.permission_requests_approved, 0);
        assert_eq!(telemetry.tools_blocked, 1);
        assert_eq!(telemetry.recent_denials, vec!["edit".to_string()]);
    }

    #[tokio::test]
    async fn requests_parent_and_applies_updates() {
        use crate::messaging::in_process::InProcessTransport;
        use crate::messaging::router::AgentMailboxRouter;
        use crate::messaging::types::AgentAddress;
        use crate::server::delegation_engine::{DelegationTracker, SubRunRecord};

        let transport = Arc::new(InProcessTransport::new());
        let tracker = Arc::new(DelegationTracker::new());
        let router = Arc::new(AgentMailboxRouter::new(transport, tracker.clone()));

        let parent_addr = AgentAddress::new("run-parent", "orchestrator");
        let child_addr = AgentAddress::new("run-child", "worker");
        let mut parent_mailbox = router.register(parent_addr.clone(), None).await.unwrap();
        let mut child_mailbox = router.register(child_addr.clone(), None).await.unwrap();

        tracker
            .record_sub_run(SubRunRecord {
                run_id: "run-child".into(),
                parent_run_id: "run-parent".into(),
                delegation_id: "del-perm".into(),
                agent_id: "worker".into(),
                depth: 1,
            })
            .await;

        let inherited = InheritedPermissions {
            mode: PermissionMode::Prompt,
            allow_rules: vec![],
            deny_rules: vec![],
            ask_rules: vec![],
            allowed_tools: Some(HashSet::from(["view".to_string()])),
            is_background: false,
        };
        let ctx = Arc::new(RwLock::new(PermissionSyncContext::new(inherited)));

        let router_clone = router.clone();
        let parent_addr_clone = parent_addr.clone();
        let child_addr_clone = child_addr.clone();
        let responder = tokio::spawn(async move {
            loop {
                if let Some(msg) = parent_mailbox.try_recv() {
                    let correlation_id = msg.correlation_id.clone().unwrap();
                    let response =
                        crate::orchestration::permission_sync::PermissionResponse::approve()
                            .with_update(PermissionUpdate::allow(PermissionRule::parse("bash")));
                    router_clone
                        .send(response.to_message(
                            &parent_addr_clone,
                            &child_addr_clone,
                            &correlation_id,
                        ))
                        .await
                        .unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });

        let result = check_tool_permission(
            "bash",
            Some(r#"{"command":"echo hi"}"#),
            Some(&ctx),
            Some(&mut child_mailbox),
            Duration::from_secs(1),
        )
        .await;
        responder.await.unwrap();

        match result {
            PermissionCheckResult::AllowedViaRequest { new_rules } => {
                assert_eq!(new_rules.len(), 1);
            }
            other => panic!("expected AllowedViaRequest, got {other:?}"),
        }

        let ctx_guard = ctx.read().await;
        assert!(ctx_guard.is_allowed("bash", Some(r#"{"command":"echo hi"}"#)));
        let telemetry = ctx_guard.telemetry();
        assert_eq!(telemetry.permission_requests, 1);
        assert_eq!(telemetry.permission_requests_approved, 1);
        assert_eq!(telemetry.tools_blocked, 0);
    }

    #[tokio::test]
    async fn denied_rules_do_not_request_parent() {
        use crate::messaging::in_process::InProcessTransport;
        use crate::messaging::router::AgentMailboxRouter;
        use crate::messaging::types::AgentAddress;
        use crate::server::delegation_engine::{DelegationTracker, SubRunRecord};

        let transport = Arc::new(InProcessTransport::new());
        let tracker = Arc::new(DelegationTracker::new());
        let router = Arc::new(AgentMailboxRouter::new(transport, tracker.clone()));

        let parent_addr = AgentAddress::new("run-parent", "orchestrator");
        let child_addr = AgentAddress::new("run-child", "worker");
        let mut parent_mailbox = router.register(parent_addr.clone(), None).await.unwrap();
        let mut child_mailbox = router.register(child_addr.clone(), None).await.unwrap();

        tracker
            .record_sub_run(SubRunRecord {
                run_id: "run-child".into(),
                parent_run_id: "run-parent".into(),
                delegation_id: "del-deny".into(),
                agent_id: "worker".into(),
                depth: 1,
            })
            .await;

        let inherited = InheritedPermissions {
            mode: PermissionMode::Prompt,
            allow_rules: vec![],
            deny_rules: vec![PermissionRule::parse("bash(rm -rf:*)")],
            ask_rules: vec![],
            allowed_tools: None,
            is_background: false,
        };
        let ctx = Arc::new(RwLock::new(PermissionSyncContext::new(inherited)));

        let result = check_tool_permission(
            "bash",
            Some(r#"{"command":"rm -rf /tmp/nope"}"#),
            Some(&ctx),
            Some(&mut child_mailbox),
            Duration::from_secs(1),
        )
        .await;

        assert!(matches!(result, PermissionCheckResult::Denied { .. }));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            parent_mailbox.try_recv().is_none(),
            "explicit deny should not send a permission request to the parent"
        );

        let ctx_guard = ctx.read().await;
        let telemetry = ctx_guard.telemetry();
        assert_eq!(telemetry.permission_requests, 0);
        assert_eq!(telemetry.permission_requests_approved, 0);
        assert_eq!(telemetry.tools_blocked, 1);
        assert_eq!(telemetry.recent_denials, vec!["bash".to_string()]);
    }
}
