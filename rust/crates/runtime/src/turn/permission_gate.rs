//! Permission gate for tool execution.
//!
//! This module provides permission checking before tool execution.
//! When a tool is blocked by permissions, it returns an error result
//! instead of executing, or requests permission from the parent agent.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

use crate::orchestration::permission_sync::{
    PermissionRequest, PermissionResponse, PermissionSyncContext, PermissionUpdate,
};
use astra_messaging::router::AgentMailbox;
use astra_turn_core::permission_engine::{DecisionSource, HardDecision, evaluate_permission};
use astra_turn_core::tool_argument_hints::normalize_llm_function_arguments;

/// Result of a permission check.
#[derive(Debug, Clone)]
pub enum PermissionCheckResult {
    /// Permission granted — proceed with tool execution.
    Allowed,
    /// Permission granted locally by policy; callers should surface this
    /// because no interactive approval UI was shown.
    AllowedImplicit { reason: String },
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
    let prompt = {
        let ctx_guard = ctx.read().await;
        let envelope = evaluate_permission(tool_name, &normalized_args, &ctx_guard);
        astra_turn_core::permission_audit::record_evaluated_envelope(
            tool_name,
            &normalized_args,
            &envelope,
            "runtime",
            None,
        );
        match envelope.decision {
            HardDecision::Allow => {
                let implicit_reason = match &envelope.source {
                    DecisionSource::Mode { mode } if mode == "auto" => Some("auto permission mode"),
                    DecisionSource::Mode { mode } if mode == "agent policy allowlist" => {
                        Some("agent policy allowlist")
                    }
                    _ => None,
                };
                return implicit_reason.map_or(PermissionCheckResult::Allowed, |reason| {
                    PermissionCheckResult::AllowedImplicit {
                        reason: reason.to_string(),
                    }
                });
            }
            HardDecision::Deny { reason } => {
                drop(ctx_guard);
                ctx.write()
                    .await
                    .record_blocked_tool_with_reason(tool_name, Some(&reason));
                return PermissionCheckResult::Denied { reason };
            }
            HardDecision::NeedExternal { prompt } => prompt,
        }
    };

    // Try to request permission from parent
    let Some(mailbox) = mailbox else {
        let reason = format!("Tool '{tool_name}' requires permission but no parent available");
        ctx.write()
            .await
            .record_blocked_tool_with_reason(tool_name, Some(&reason));
        return PermissionCheckResult::Denied { reason };
    };

    // Build permission request
    let mut request = PermissionRequest::new(tool_name, normalized_args).with_reason(prompt.reason);
    if let Some(ref hint) = prompt.detail {
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

/// Tools that are unconditionally read-only — safe to call in plan mode.
///
/// This list mirrors the tools that claudecode's plan-mode workflow
/// instructions explicitly allow ("Thoroughly explore the codebase
/// using Glob, Grep, and Read tools"). The `enter_plan_mode` /
/// `exit_plan_mode` tools themselves must also pass — otherwise plan
/// mode is a trap (model enters but can't exit).
///
/// Tools NOT on this list are treated as potentially-mutating in plan
/// mode and denied. The list is conservative: it's safer to deny a
/// surprising read-only tool than to allow a surprising write tool.
fn is_read_only_in_plan_mode(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "read_file"
            | "grep"
            | "glob"
            | "list_dir"
            | "symbols"
            | "introspect"
            | "lsp"
            | "web_fetch"
            | "web_search"
            | "memory"
            | "session"
            | "task"
            | "tool_search"
            | "ask_user"
            | "notify"
            | "view_image"
            | "exit_plan_mode" // Read-only sub-actions of `git` tool. The full git tool also
                               // has stash/commit/revert which mutate, but the model picks
                               // those by `action` — we'd need argument-aware filtering to
                               // be safe, so deny the whole git tool in plan mode for now
                               // and let the model use individual git read tools through
                               // bash if it really must (which is itself denied — the
                               // intent is "read the code, don't mutate state").
    )
}

/// Plan-mode-aware wrapper around [`check_tool_permission`]. When
/// `plan_mode_active` is true, mutating tools are denied at the gate
/// with a redirect to `exit_plan_mode`; read-only tools fall through
/// to the normal permission flow.
///
/// The two callsites — the test suite and `headless_tool_pipeline` —
/// pass `plan_mode_active` from the session's active_plan_id flag.
/// When no flag is wired (legacy code paths, tests that don't care
/// about plan mode), use [`check_tool_permission`] directly.
pub async fn check_tool_permission_in_plan_mode(
    tool_name: &str,
    args: Option<&str>,
    permission_context: Option<&Arc<RwLock<PermissionSyncContext>>>,
    mailbox: Option<&mut AgentMailbox>,
    timeout: Duration,
    plan_mode_active: bool,
) -> PermissionCheckResult {
    if plan_mode_active && !is_read_only_in_plan_mode(tool_name) {
        return PermissionCheckResult::Denied {
            reason: format!(
                "tool '{tool_name}' is blocked while plan mode is active. \
                 Plan mode is a read-only authoring phase — explore the codebase \
                 with read tools (read_file, grep, glob, list_dir, symbols), then \
                 call `exit_plan_mode(plan='...', approved=true)` to exit and \
                 unlock writes."
            ),
        };
    }
    check_tool_permission(tool_name, args, permission_context, mailbox, timeout).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestration::permission_sync::{
        InheritedPermissions, PermissionMode, PermissionResponseMessaging, PermissionRule,
    };
    use astra_turn_core::permission_types::RuleMatchContext;
    use std::collections::HashSet;

    fn is_allowed(result: &PermissionCheckResult) -> bool {
        matches!(
            result,
            PermissionCheckResult::Allowed | PermissionCheckResult::AllowedImplicit { .. }
        )
    }

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
        assert!(is_allowed(&result));
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
            ..Default::default()
        };
        let ctx = Arc::new(RwLock::new(PermissionSyncContext::new(inherited)));

        let result =
            check_tool_permission("edit", None, Some(&ctx), None, Duration::from_secs(5)).await;
        assert!(is_allowed(&result));
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            is_allowed(&result),
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
            ..Default::default()
        };
        let ctx = Arc::new(RwLock::new(PermissionSyncContext::new(inherited)));

        // "view" is in the allowlist — should approve
        let result =
            check_tool_permission("view", None, Some(&ctx), None, Duration::from_secs(1)).await;
        assert!(is_allowed(&result));

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

    /// REGRESSION: a sub-agent in `Prompt` mode whose tool IS in the
    /// agent_type's `allowed_tools` allowlist must be auto-approved
    /// without trying to ask the parent. The user already authorized
    /// the agent.spawn that created this sub-agent, knowing its
    /// agent_type's tool surface (e.g. `code-review` ⇒ bash, grep,
    /// glob, view). Asking them again per-tool-call is friction
    /// without consent value.
    ///
    /// The pre-fix bug (session 2a98814b): sub-agents inherited
    /// Prompt mode, fell through to the "request parent" branch, but
    /// the orchestrator mailbox was never registered in
    /// `initialize_multi_agent_runtime` — so `request_permission`
    /// returned `MailboxError::AgentNotFound("orchestrator@run-...")`
    /// and the tool call was denied. 4 review agents spawned, all
    /// returned 0 tool calls, useless output.
    ///
    /// Fix: extend the local-approve fast path so it also fires when
    /// `allowed_tools.is_some()` AND the tool is in the list,
    /// regardless of mode. The allowlist IS the consent.
    #[tokio::test]
    async fn allowlisted_tool_auto_approves_in_prompt_mode_without_mailbox() {
        let inherited = InheritedPermissions {
            mode: PermissionMode::Prompt,
            allow_rules: vec![],
            deny_rules: vec![],
            ask_rules: vec![],
            // Mirror the current `code-review` agent type: bash, grep,
            // glob, list_dir, read_file.
            allowed_tools: Some(HashSet::from([
                "bash".to_string(),
                "grep".to_string(),
                "glob".to_string(),
                "list_dir".to_string(),
                "read_file".to_string(),
            ])),
            is_background: false,
            ..Default::default()
        };
        let ctx = Arc::new(RwLock::new(PermissionSyncContext::new(inherited)));

        // No mailbox — orchestrator is not registered (the actual
        // production state on a fresh REPL).
        let result = check_tool_permission(
            "bash",
            Some(r#"{"command":"git show HEAD"}"#),
            Some(&ctx),
            None,
            Duration::from_secs(1),
        )
        .await;
        assert!(
            is_allowed(&result),
            "tool in agent_type allowlist must auto-approve in Prompt mode; \
             got {result:?}. Pre-fix this would have hit the 'mailbox = None' \
             branch and denied the call."
        );

        let telemetry = ctx.read().await.telemetry();
        assert_eq!(
            telemetry.permission_requests, 0,
            "must NOT send a mailbox request — the allowlist is the consent"
        );
        assert_eq!(
            telemetry.tools_blocked, 0,
            "must NOT block the call — allowlisted tools bypass the prompt path"
        );
    }

    /// Regression for session 2ee7f992: code-review children were
    /// prompted to inspect `/tmp/astra_review_diff.txt`, chose
    /// `read_file`, but the built-in allowlist still used the legacy
    /// `view` tool name. That pushed `read_file` into the parent
    /// permission-request path, which then failed with
    /// `AgentNotFound(\"orchestrator@run-...\")` because the root
    /// orchestrator mailbox is not registered.
    #[tokio::test]
    async fn code_review_allowlist_auto_approves_read_file_without_mailbox() {
        let code_review = astra_turn_core::orchestration_builtin_agents::get_builtin_agent_types()
            .into_iter()
            .find(|def| def.agent_type == "code-review")
            .expect("builtins must include code-review");
        let inherited = InheritedPermissions {
            mode: PermissionMode::Prompt,
            allow_rules: vec![],
            deny_rules: vec![],
            ask_rules: vec![],
            allowed_tools: Some(code_review.allowed_tools),
            is_background: false,
            ..Default::default()
        };
        let ctx = Arc::new(RwLock::new(PermissionSyncContext::new(inherited)));

        let result = check_tool_permission(
            "read_file",
            Some(r#"{"path":"/tmp/astra_review_diff.txt"}"#),
            Some(&ctx),
            None,
            Duration::from_secs(1),
        )
        .await;
        assert!(
            is_allowed(&result),
            "code-review allowlist must admit read_file locally; got {result:?}"
        );

        let telemetry = ctx.read().await.telemetry();
        assert_eq!(telemetry.permission_requests, 0);
        assert_eq!(telemetry.tools_blocked, 0);
    }

    /// Companion test: a tool OUTSIDE the allowlist still gets blocked
    /// in Prompt mode without a mailbox. The fast-path doesn't widen
    /// the agent's surface beyond what `agent_type` declared.
    #[tokio::test]
    async fn non_allowlisted_tool_still_denied_in_prompt_without_mailbox() {
        let inherited = InheritedPermissions {
            mode: PermissionMode::Prompt,
            allow_rules: vec![],
            deny_rules: vec![],
            ask_rules: vec![],
            allowed_tools: Some(HashSet::from(["bash".to_string(), "grep".to_string()])),
            is_background: false,
            ..Default::default()
        };
        let ctx = Arc::new(RwLock::new(PermissionSyncContext::new(inherited)));

        // `edit` is NOT in the code-review-style allowlist.
        let result = check_tool_permission(
            "edit",
            Some(r#"{"path":"src/main.rs"}"#),
            Some(&ctx),
            None,
            Duration::from_secs(1),
        )
        .await;
        assert!(
            matches!(result, PermissionCheckResult::Denied { .. }),
            "tool outside the allowlist must still be denied; got {result:?}"
        );
    }

    #[tokio::test]
    async fn deny_mode_overrides_agent_type_allowlist() {
        let inherited = InheritedPermissions {
            mode: PermissionMode::Deny,
            allow_rules: vec![],
            deny_rules: vec![],
            ask_rules: vec![],
            allowed_tools: Some(HashSet::from(["bash".to_string()])),
            is_background: false,
            ..Default::default()
        };
        let ctx = Arc::new(RwLock::new(PermissionSyncContext::new(inherited)));

        let result = check_tool_permission(
            "bash",
            Some(r#"{"command":"git status"}"#),
            Some(&ctx),
            None,
            Duration::from_secs(1),
        )
        .await;
        assert!(
            matches!(result, PermissionCheckResult::Denied { .. }),
            "Deny mode must not be bypassed by an agent_type allowlist; got {result:?}"
        );
    }

    #[tokio::test]
    async fn explicit_actions_follow_auto_mode_without_parent() {
        let inherited = InheritedPermissions {
            mode: PermissionMode::Auto,
            allow_rules: vec![PermissionRule::parse("git_commit")],
            deny_rules: vec![],
            ask_rules: vec![],
            allowed_tools: None,
            is_background: false,
            ..Default::default()
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
        assert!(matches!(result, PermissionCheckResult::Allowed));

        let telemetry = ctx.read().await.telemetry();
        assert_eq!(telemetry.permission_requests, 0);
        assert_eq!(telemetry.tools_blocked, 0);
    }

    #[tokio::test]
    async fn explicit_actions_request_parent_even_when_tool_is_allowed() {
        use crate::server::delegation_engine::{DelegationTracker, SubRunRecord, SubRunState};
        use astra_messaging::in_process::InProcessTransport;
        use astra_messaging::router::AgentMailboxRouter;
        use astra_messaging::types::AgentAddress;

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
            mode: PermissionMode::Prompt,
            allow_rules: vec![PermissionRule::parse("git_commit")],
            deny_rules: vec![],
            ask_rules: vec![],
            allowed_tools: None,
            is_background: false,
            ..Default::default()
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
        use crate::server::delegation_engine::{DelegationTracker, SubRunRecord, SubRunState};
        use astra_messaging::in_process::InProcessTransport;
        use astra_messaging::router::AgentMailboxRouter;
        use astra_messaging::types::AgentAddress;

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

        // No allowlist set — Prompt mode falls through to the
        // request-parent flow that this test is exercising. (When an
        // allowlist IS set, allowlisted tools auto-approve locally;
        // see `allowlisted_tool_auto_approves_in_prompt_mode_without_mailbox`.)
        let inherited = InheritedPermissions {
            mode: PermissionMode::Prompt,
            allow_rules: vec![],
            deny_rules: vec![],
            ask_rules: vec![],
            allowed_tools: None,
            is_background: false,
            ..Default::default()
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
                            .with_update(PermissionUpdate::allow(PermissionRule::parse(
                                "Bash(touch:*)",
                            )));
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
            Some(r#"{"command":"touch astra-permission-gate-test"}"#),
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
        let rule_ctx = RuleMatchContext::from_tool_args(
            "bash",
            &serde_json::json!({"command":"touch astra-permission-gate-test"}),
        );
        assert!(ctx_guard.is_allowed_with_context("bash", &rule_ctx));
        let telemetry = ctx_guard.telemetry();
        assert_eq!(telemetry.permission_requests, 1);
        assert_eq!(telemetry.permission_requests_approved, 1);
        assert_eq!(telemetry.tools_blocked, 0);
    }

    // ── Phase 2: plan-mode write-tool gate ──────────────────────────────
    //
    // While the session is in plan mode (active_plan_id != None), all
    // mutating tools (str_replace, write_file, bash, git commit, etc.)
    // must be denied at the gate with a redirect to `exit_plan_mode`.
    // Read-only tools (read_file, grep, glob, list_dir) stay allowed so
    // the model can still explore the codebase to write a good plan.
    //
    // claudecode references: plan-mode write block enforces "DO NOT
    // write or edit any files yet. This is a read-only exploration and
    // planning phase." Without this gate, the model can call write
    // tools while in plan mode and silently bypass the workflow.

    #[tokio::test]
    async fn plan_mode_blocks_write_tool_calls() {
        // Auto mode + no allowlist = would normally allow `bash`. Plan
        // mode flag must override and deny.
        let inherited = InheritedPermissions {
            mode: PermissionMode::Auto,
            allow_rules: vec![],
            deny_rules: vec![],
            ask_rules: vec![],
            allowed_tools: None,
            is_background: false,
            ..Default::default()
        };
        let ctx = Arc::new(RwLock::new(PermissionSyncContext::new(inherited)));

        // Pretend the session is mid-plan with active plan id present.
        let result = check_tool_permission_in_plan_mode(
            "bash",
            Some(r#"{"command":"echo hi"}"#),
            Some(&ctx),
            None,
            Duration::from_secs(1),
            true, // plan_mode_active
        )
        .await;
        match result {
            PermissionCheckResult::Denied { reason } => {
                assert!(
                    reason.to_lowercase().contains("plan mode")
                        || reason.contains("exit_plan_mode"),
                    "plan-mode denial must point the model at exit_plan_mode \
                     so it can recover. Got: {reason}"
                );
            }
            other => panic!(
                "bash in plan mode must be denied (it's a write/exec tool). \
                 Got {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn plan_mode_blocks_str_replace_and_write_file() {
        let inherited = InheritedPermissions {
            mode: PermissionMode::Auto,
            ..Default::default()
        };
        let ctx = Arc::new(RwLock::new(PermissionSyncContext::new(inherited)));

        for tool in &["str_replace", "write_file", "multi_edit"] {
            let result = check_tool_permission_in_plan_mode(
                tool,
                None,
                Some(&ctx),
                None,
                Duration::from_secs(1),
                true,
            )
            .await;
            assert!(
                matches!(result, PermissionCheckResult::Denied { .. }),
                "`{tool}` is a write tool — plan mode must deny it. Got: {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn plan_mode_allows_read_only_tools() {
        let inherited = InheritedPermissions {
            mode: PermissionMode::Auto,
            ..Default::default()
        };
        let ctx = Arc::new(RwLock::new(PermissionSyncContext::new(inherited)));

        for tool in &["read_file", "grep", "glob", "list_dir", "symbols"] {
            let result = check_tool_permission_in_plan_mode(
                tool,
                None,
                Some(&ctx),
                None,
                Duration::from_secs(1),
                true,
            )
            .await;
            assert!(
                is_allowed(&result),
                "`{tool}` is read-only — plan mode must allow it so the model \
                 can explore the codebase before writing the plan. Got: {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn plan_mode_inactive_does_not_block_writes() {
        // When plan_mode is NOT active, the existing permission rules
        // apply unchanged — Auto mode + no allowlist would normally allow
        // bash, and the new gate must not regress that path.
        let inherited = InheritedPermissions {
            mode: PermissionMode::Auto,
            ..Default::default()
        };
        let ctx = Arc::new(RwLock::new(PermissionSyncContext::new(inherited)));

        let result = check_tool_permission_in_plan_mode(
            "bash",
            Some(r#"{"command":"echo hi"}"#),
            Some(&ctx),
            None,
            Duration::from_secs(1),
            false, // plan_mode_active
        )
        .await;
        assert!(
            is_allowed(&result),
            "bash must not be blocked when plan_mode is inactive. Got: {result:?}"
        );
    }

    /// `exit_plan_mode` itself must always be allowed in plan mode —
    /// otherwise it's a trap (model enters plan mode but can't exit
    /// because the gate denies the only escape tool).
    #[tokio::test]
    async fn plan_mode_always_allows_exit_plan_mode() {
        let inherited = InheritedPermissions {
            mode: PermissionMode::Auto,
            ..Default::default()
        };
        let ctx = Arc::new(RwLock::new(PermissionSyncContext::new(inherited)));

        let result = check_tool_permission_in_plan_mode(
            "exit_plan_mode",
            Some(r#"{"plan":"1. step","approved":true}"#),
            Some(&ctx),
            None,
            Duration::from_secs(1),
            true,
        )
        .await;
        assert!(
            is_allowed(&result),
            "exit_plan_mode must always be allowed in plan mode — \
             without this the model is trapped. Got: {result:?}"
        );
    }

    #[tokio::test]
    async fn denied_rules_do_not_request_parent() {
        use crate::server::delegation_engine::{DelegationTracker, SubRunRecord, SubRunState};
        use astra_messaging::in_process::InProcessTransport;
        use astra_messaging::router::AgentMailboxRouter;
        use astra_messaging::types::AgentAddress;

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
            ..Default::default()
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
