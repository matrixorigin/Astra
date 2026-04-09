//! Permission gate for tool execution.
//!
//! This module provides permission checking before tool execution.
//! When a tool is blocked by permissions, it returns an error result
//! instead of executing, or requests permission from the parent agent.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

use crate::messaging::router::AgentMailbox;
use crate::messaging::types::AgentAddress;
use crate::orchestration::permission_sync::{PermissionMode, PermissionRequest, PermissionSyncContext, PermissionUpdate};

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
/// 3. If denied and have mailbox + parent_address, request permission
/// 4. Return result
pub async fn check_tool_permission(
    tool_name: &str,
    args: Option<&str>,
    permission_context: Option<&Arc<RwLock<PermissionSyncContext>>>,
    mailbox: Option<&mut AgentMailbox>,
    parent_address: Option<&AgentAddress>,
    timeout: Duration,
) -> PermissionCheckResult {
    // No permission context = legacy mode, always allow
    let Some(ctx) = permission_context else {
        return PermissionCheckResult::Allowed;
    };

    // Check local permission rules
    {
        let ctx_guard = ctx.read().await;
        if ctx_guard.is_allowed(tool_name, args) {
            return PermissionCheckResult::Allowed;
        }

        // If mode is Deny, don't even try to request
        if ctx_guard.mode() == PermissionMode::Deny {
            return PermissionCheckResult::Denied {
                reason: format!("Tool '{}' denied by permission mode", tool_name),
            };
        }
    }

    // Try to request permission from parent
    let (Some(mailbox), Some(_parent_addr)) = (mailbox, parent_address) else {
        return PermissionCheckResult::Denied {
            reason: format!(
                "Tool '{}' requires permission but no parent available",
                tool_name
            ),
        };
    };

    // Build permission request
    let args_json = args
        .map(|a| serde_json::from_str(a).unwrap_or_else(|_| serde_json::json!({"raw": a})))
        .unwrap_or(serde_json::json!({}));

    let request = PermissionRequest::new(tool_name, args_json)
        .with_reason(format!("Requesting permission to use tool: {}", tool_name));

    // Send request and wait for response
    match mailbox.request_permission(request, timeout).await {
        Ok(response) => {
            if response.approved {
                // Apply any new rules to our context
                let new_rules = response.updates.clone();
                if !new_rules.is_empty() {
                    let mut ctx_guard = ctx.write().await;
                    for update in &new_rules {
                        ctx_guard.apply_update(update);
                    }
                }
                PermissionCheckResult::AllowedViaRequest { new_rules }
            } else {
                PermissionCheckResult::Denied {
                    reason: response
                        .reason
                        .unwrap_or_else(|| "Permission denied by parent".to_string()),
                }
            }
        }
        Err(e) => PermissionCheckResult::Denied {
            reason: format!("Permission request failed: {}", e),
        },
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
        let result = check_tool_permission("edit", Some("src/main.rs"), None, None, None, Duration::from_secs(5)).await;
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

        let result = check_tool_permission("edit", None, Some(&ctx), None, None, Duration::from_secs(5)).await;
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

        let result = check_tool_permission("edit", None, Some(&ctx), None, None, Duration::from_secs(5)).await;
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

        let result = check_tool_permission("edit", None, Some(&ctx), None, None, Duration::from_secs(5)).await;
        assert!(matches!(result, PermissionCheckResult::Denied { .. }));
    }
}
