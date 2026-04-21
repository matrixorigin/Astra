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
    PermissionMode, PermissionRequest, PermissionResponse, PermissionSyncContext, PermissionUpdate,
};
use crate::turn::action_compensation::explicit_approval_reason;
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
///
/// # Parameters
///
/// * `tool_name` — Name of the tool being invoked (e.g. `"bash"`, `"edit_file"`).
/// * `args` — Optional JSON string of tool arguments, used for rule matching.
/// * `permission_context` — Shared permission rules for this agent. `None` means
///   legacy/unrestricted mode (all tools allowed).
/// * `mailbox` — Agent's mailbox for requesting permission from the parent.
///   `None` when no parent is available (root agent or standalone mode).
/// * `timeout` — Maximum time to wait for a parent permission response.
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
    let explicit_approval = explicit_approval_reason(tool_name, &normalized_args);

    // Check local permission rules
    {
        let ctx_guard = ctx.read().await;
        if ctx_guard.is_denied(tool_name, rule_match_hint) {
            drop(ctx_guard);
            let reason = format!("Tool '{}' denied by permission rules", tool_name);
            ctx.write()
                .await
                .record_blocked_tool_with_reason(tool_name, Some(&reason));
            return PermissionCheckResult::Denied { reason };
        }

        if explicit_approval.is_none() && ctx_guard.is_allowed(tool_name, rule_match_hint) {
            return PermissionCheckResult::Allowed;
        }

        // Auto mode: approve locally without mailbox round-trip.
        // This avoids the 30s permission-request timeout that would otherwise
        // block child agents whose parent happens to be mid-LLM-call.
        // Still respects the allowed_tools allowlist — tools not on the list
        // are denied even in Auto mode.
        if ctx_guard.mode() == PermissionMode::Auto {
            if !ctx_guard.inherited.is_tool_allowed_by_allowlist(tool_name) {
                drop(ctx_guard);
                let reason = format!("Tool '{}' not in allowed tools list", tool_name);
                ctx.write()
                    .await
                    .record_blocked_tool_with_reason(tool_name, Some(&reason));
                return PermissionCheckResult::Denied { reason };
            }
            if explicit_approval.is_none() {
                return PermissionCheckResult::Allowed;
            }
        }

        // If mode is Deny, don't even try to request
        if ctx_guard.mode() == PermissionMode::Deny {
            drop(ctx_guard);
            let reason = explicit_approval
                .clone()
                .unwrap_or_else(|| format!("Tool '{}' denied by permission mode", tool_name));
            ctx.write()
                .await
                .record_blocked_tool_with_reason(tool_name, Some(&reason));
            return PermissionCheckResult::Denied { reason };
        }
    }

    // Try to request permission from parent
    let Some(mailbox) = mailbox else {
        let reason = explicit_approval.clone().map_or_else(
            || {
                format!(
                    "Tool '{}' requires permission but no parent available",
                    tool_name
                )
            },
            |reason| format!("{reason} No parent is available to approve this tool call."),
        );
        ctx.write()
            .await
            .record_blocked_tool_with_reason(tool_name, Some(&reason));
        return PermissionCheckResult::Denied { reason };
    };

    // Build permission request
    let mut request = PermissionRequest::new(tool_name, normalized_args).with_reason(
        explicit_approval
            .clone()
            .unwrap_or_else(|| format!("Requesting permission to use tool: {tool_name}")),
    );
    if let Some(ref hint) = permission_hint {
        request = request.with_hint(hint.clone());
    }

    // Send request and wait for response
    ctx.write().await.record_permission_request();
    match mailbox.request_permission(request, timeout).await {
        Ok(outcome) => {
            // Deserialize generic PermissionOutcome into PermissionResponse
            let response: PermissionResponse = outcome
                .data
                .as_ref()
                .and_then(|d| serde_json::from_value(d.clone()).ok())
                .unwrap_or_else(|| {
                    if outcome.accepted {
                        PermissionResponse::approve()
                    } else {
                        PermissionResponse::deny("Permission denied by parent")
                    }
                });
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
                let reason = response
                    .reason
                    .unwrap_or_else(|| "Permission denied by parent".to_string());
                ctx.write()
                    .await
                    .record_blocked_tool_with_reason(tool_name, Some(&reason));
                PermissionCheckResult::Denied { reason }
            }
        }
        Err(e) => {
            let reason = format!("Permission request failed: {}", e);
            ctx.write()
                .await
                .record_blocked_tool_with_reason(tool_name, Some(&reason));
            PermissionCheckResult::Denied { reason }
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
    use crate::orchestration::permission_sync::{
        InheritedPermissions, PermissionResponseMessaging, PermissionRule,
    };
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

    /// Auto mode should approve locally without mailbox round-trip,
    /// but still respect the allowed_tools allowlist.
    #[tokio::test]
    async fn auto_mode_approves_locally() {
        let inherited = InheritedPermissions {
            mode: PermissionMode::Auto,
            allow_rules: vec![],
            deny_rules: vec![],
            ask_rules: vec![],
            allowed_tools: None, // no allowlist = all tools allowed
            is_background: false,
        };
        let ctx = Arc::new(RwLock::new(PermissionSyncContext::new(inherited)));

        // Should approve without needing a mailbox
        let result = check_tool_permission(
            "bash",
            Some(r#"{"command":"echo hi"}"#),
            Some(&ctx),
            None,
            Duration::from_secs(1),
        )
        .await;
        assert!(
            matches!(result, PermissionCheckResult::Allowed),
            "Auto mode should approve tools locally"
        );

        let telemetry = ctx.read().await.telemetry();
        assert_eq!(
            telemetry.permission_requests, 0,
            "Auto mode should not send mailbox requests"
        );
    }

    /// Auto mode should deny tools not in the allowed_tools allowlist.
    #[tokio::test]
    async fn auto_mode_respects_allowlist() {
        let inherited = InheritedPermissions {
            mode: PermissionMode::Auto,
            allow_rules: vec![],
            deny_rules: vec![],
            ask_rules: vec![],
            allowed_tools: Some(HashSet::from(["view".to_string(), "grep".to_string()])),
            is_background: false,
        };
        let ctx = Arc::new(RwLock::new(PermissionSyncContext::new(inherited)));

        // "view" is in the allowlist — should approve
        let result =
            check_tool_permission("view", None, Some(&ctx), None, Duration::from_secs(1)).await;
        assert!(matches!(result, PermissionCheckResult::Allowed));

        // "bash" is NOT in the allowlist — should deny
        let result =
            check_tool_permission("bash", None, Some(&ctx), None, Duration::from_secs(1)).await;
        assert!(
            matches!(result, PermissionCheckResult::Denied { .. }),
            "Auto mode should deny tools not in allowlist"
        );

        let telemetry = ctx.read().await.telemetry();
        assert_eq!(
            telemetry.permission_requests, 0,
            "Auto mode should never send mailbox requests"
        );
        assert_eq!(telemetry.tools_blocked, 1);
    }

    #[tokio::test]
    async fn explicit_actions_are_denied_without_parent_even_in_auto_mode() {
        let inherited = InheritedPermissions {
            mode: PermissionMode::Auto,
            allow_rules: vec![PermissionRule::parse("git_commit")],
            deny_rules: vec![],
            ask_rules: vec![],
            allowed_tools: None,
            is_background: false,
        };
        let ctx = Arc::new(RwLock::new(PermissionSyncContext::new(inherited)));

        let result = check_tool_permission(
            "git_commit",
            Some(r#"{"message":"ship it"}"#),
            Some(&ctx),
            None,
            Duration::from_secs(1),
        )
        .await;
        match result {
            PermissionCheckResult::Denied { reason } => {
                assert!(reason.contains("Explicit approval required"));
            }
            other => panic!("expected denied explicit approval, got {other:?}"),
        }

        let telemetry = ctx.read().await.telemetry();
        assert_eq!(telemetry.permission_requests, 0);
        assert_eq!(telemetry.tools_blocked, 1);
    }

    #[tokio::test]
    async fn explicit_actions_request_parent_even_when_tool_is_allowed() {
        use crate::messaging::in_process::InProcessTransport;
        use crate::messaging::router::AgentMailboxRouter;
        use crate::messaging::types::AgentAddress;
        use crate::server::delegation_engine::{DelegationTracker, SubRunRecord, SubRunState};

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
                delegation_id: "del-explicit".into(),
                agent_id: "worker".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;

        let inherited = InheritedPermissions {
            mode: PermissionMode::Auto,
            allow_rules: vec![PermissionRule::parse("git_commit")],
            deny_rules: vec![],
            ask_rules: vec![],
            allowed_tools: None,
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
                        crate::orchestration::permission_sync::PermissionResponse::approve();
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
            "git_commit",
            Some(r#"{"message":"ship it"}"#),
            Some(&ctx),
            Some(&mut child_mailbox),
            Duration::from_secs(1),
        )
        .await;
        responder.await.unwrap();

        assert!(matches!(
            result,
            PermissionCheckResult::AllowedViaRequest { .. }
        ));
        let telemetry = ctx.read().await.telemetry();
        assert_eq!(telemetry.permission_requests, 1);
        assert_eq!(telemetry.permission_requests_approved, 1);
    }

    #[tokio::test]
    async fn requests_parent_and_applies_updates() {
        use crate::messaging::in_process::InProcessTransport;
        use crate::messaging::router::AgentMailboxRouter;
        use crate::messaging::types::AgentAddress;
        use crate::server::delegation_engine::{DelegationTracker, SubRunRecord, SubRunState};

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
                state: SubRunState::Created,
                retry_of: None,
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
        use crate::server::delegation_engine::{DelegationTracker, SubRunRecord, SubRunState};

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
                state: SubRunState::Created,
                retry_of: None,
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
