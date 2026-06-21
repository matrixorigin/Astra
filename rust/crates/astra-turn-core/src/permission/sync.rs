//! Permission synchronization for parent-child agent communication.
//!
//! Pure type definitions live in `crate::permission::types`.
//! This module re-exports them and adds messaging-dependent extensions.

pub use crate::permission::types::*;

use crate::cloud::approval_policy::{CloudGatedToolKind, cloud_gated_tool_kind_with_args};
use crate::permission::match_target::{AllowMatchTarget, default_match_target};
use astra_messaging::types::{
    AgentAddress, AgentMessage, MessagePayload, MessageTarget, RequestType,
};

/// Extension trait adding messaging capabilities to [`PermissionRequest`].
pub trait PermissionRequestMessaging {
    fn to_message(&self, from: &AgentAddress, to: &AgentAddress) -> AgentMessage;
    fn from_message_payload(data: &serde_json::Value) -> Option<PermissionRequest>;
}

impl PermissionRequestMessaging for PermissionRequest {
    /// Build an AgentMessage to send this permission request to a parent.
    fn to_message(&self, from: &AgentAddress, to: &AgentAddress) -> AgentMessage {
        let data = serde_json::to_value(self).unwrap_or_else(|e| {
            eprintln!("  ⚠ permission: failed to serialize request: {e}");
            serde_json::Value::Null
        });
        AgentMessage::new(
            from.clone(),
            MessageTarget::Direct {
                address: to.clone(),
            },
            MessagePayload::Request {
                request_type: RequestType::ToolPermission,
                data,
            },
        )
    }

    /// Parse a permission request from an incoming message payload.
    fn from_message_payload(data: &serde_json::Value) -> Option<PermissionRequest> {
        serde_json::from_value(data.clone()).ok()
    }
}

/// Extension trait adding messaging capabilities to [`PermissionResponse`].
pub trait PermissionResponseMessaging {
    fn to_message(
        &self,
        from: &AgentAddress,
        to: &AgentAddress,
        correlation_id: &str,
    ) -> AgentMessage;
    fn from_message_payload(data: &serde_json::Value) -> Option<PermissionResponse>;
}

impl PermissionResponseMessaging for PermissionResponse {
    /// Build an AgentMessage to send this permission response to a child.
    fn to_message(
        &self,
        from: &AgentAddress,
        to: &AgentAddress,
        correlation_id: &str,
    ) -> AgentMessage {
        let data = serde_json::to_value(self).unwrap_or_else(|e| {
            eprintln!("  ⚠ permission: failed to serialize response: {e}");
            serde_json::Value::Null
        });
        AgentMessage::new(
            from.clone(),
            MessageTarget::Direct {
                address: to.clone(),
            },
            MessagePayload::Response {
                request_id: correlation_id.to_string(),
                accepted: self.approved,
                data: Some(data),
            },
        )
        .with_correlation(correlation_id)
    }

    /// Parse a permission response from an incoming message payload.
    fn from_message_payload(data: &serde_json::Value) -> Option<PermissionResponse> {
        serde_json::from_value(data.clone()).ok()
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_permission_rule_parse() {
        let rule = PermissionRule::parse("bash");
        assert_eq!(rule.tool, "bash");
        assert!(rule.pattern.is_none());

        let rule = PermissionRule::parse("Bash(git commit:*)");
        assert_eq!(rule.tool, "bash");
        assert_eq!(rule.pattern, Some("git commit".to_string()));
    }

    #[test]
    fn test_permission_rule_matches() {
        let rule = PermissionRule::parse("bash(git commit:*)");

        assert!(rule.matches("bash", Some("git commit -m 'fix'")));
        assert!(rule.matches("Bash", Some("git commit --amend")));
        assert!(!rule.matches("bash", Some("git commitizen")));
        assert!(!rule.matches("bash", Some("git push")));
        assert!(!rule.matches("edit", Some("git commit")));
    }

    #[test]
    fn test_inherited_permissions() {
        let mut inherited = InheritedPermissions::default();
        inherited.add_allow(PermissionRule::parse("bash(git:*)"));
        inherited.add_deny(PermissionRule::parse("bash(rm -rf:*)"));

        assert!(inherited.is_allowed("bash", Some("git status")));
        assert!(!inherited.is_allowed("bash", Some("npm install")));
        assert!(inherited.is_denied("bash", Some("rm -rf /")));
        assert!(!inherited.is_denied("bash", Some("rm file.txt")));
    }

    #[test]
    fn test_permission_sync_context() {
        let inherited = InheritedPermissions {
            mode: PermissionMode::Prompt,
            allow_rules: vec![PermissionRule::parse("bash(git:*)")],
            deny_rules: vec![],
            ..Default::default()
        };

        let mut ctx = PermissionSyncContext::new(inherited);

        // Inherited rule works
        assert!(ctx.is_allowed("bash", Some("git status")));

        // Apply session override
        let update = PermissionUpdate::allow(PermissionRule::parse("bash(npm:*)"));
        ctx.apply_update(&update);
        assert!(ctx.is_allowed("bash", Some("npm install")));

        // Export for child
        let child_perms = ctx.for_child(true);
        assert!(child_perms.is_background);
        assert!(child_perms.is_allowed("bash", Some("git status")));
        assert!(child_perms.is_allowed("bash", Some("npm install")));
    }

    #[test]
    fn test_permission_response() {
        let response = PermissionResponse::approve()
            .with_update(PermissionUpdate::allow(PermissionRule::tool("edit")).persistent());

        assert!(response.approved);
        assert_eq!(response.updates.len(), 1);
        assert!(response.updates[0].persist);
    }

    #[test]
    fn test_tool_allowlist() {
        let inherited = InheritedPermissions {
            allowed_tools: Some(
                ["view", "grep", "glob"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ),
            ..Default::default()
        };

        assert!(inherited.is_tool_allowed_by_allowlist("view"));
        assert!(inherited.is_tool_allowed_by_allowlist("grep"));
        assert!(!inherited.is_tool_allowed_by_allowlist("bash"));
        assert!(!inherited.is_tool_allowed_by_allowlist("edit"));
    }

    #[test]
    fn test_permission_request_to_message() {
        let request = PermissionRequest::new("bash", serde_json::json!({"command": "git status"}));
        let from = AgentAddress::new("child-run", "child-agent");
        let to = AgentAddress::new("parent-run", "parent-agent");

        let msg = request.to_message(&from, &to);

        assert_eq!(&msg.from, &from);
        if let MessageTarget::Direct { address } = &msg.to {
            assert_eq!(address, &to);
        } else {
            panic!("Expected Direct target");
        }

        if let MessagePayload::Request { request_type, data } = &msg.payload {
            assert!(matches!(request_type, RequestType::ToolPermission));
            let parsed = PermissionRequest::from_message_payload(data).unwrap();
            assert_eq!(parsed.tool_name, "bash");
        } else {
            panic!("Expected Request payload");
        }
    }

    #[test]
    fn test_permission_response_to_message() {
        let response = PermissionResponse::approve().with_update(PermissionUpdate::allow(
            PermissionRule::parse("bash(git:*)"),
        ));
        let from = AgentAddress::new("parent-run", "parent-agent");
        let to = AgentAddress::new("child-run", "child-agent");

        let msg = response.to_message(&from, &to, "req-123");

        assert_eq!(&msg.from, &from);
        assert_eq!(msg.correlation_id.as_deref(), Some("req-123"));

        if let MessagePayload::Response {
            request_id,
            accepted,
            data,
        } = &msg.payload
        {
            assert_eq!(request_id, "req-123");
            assert!(accepted);
            let data_ref = data.as_ref().expect("Should have data");
            let parsed = PermissionResponse::from_message_payload(data_ref).unwrap();
            assert!(parsed.approved);
            assert_eq!(parsed.updates.len(), 1);
        } else {
            panic!("Expected Response payload");
        }
    }
}

// ─── Permission Request Handler ─────────────────────────────────────────────

use std::sync::Arc;
use tokio::sync::RwLock;

fn accept_edits_auto_allows_request(request: &PermissionRequest) -> bool {
    matches!(
        (
            cloud_gated_tool_kind_with_args(&request.tool_name, Some(&request.args)),
            default_match_target(&request.tool_name, &request.args),
        ),
        (Some(CloudGatedToolKind::Write), AllowMatchTarget::Prefix(_))
    )
}

/// Handler for incoming permission requests from child agents.
///
/// The parent agent creates a handler and registers a callback to make
/// permission decisions. The handler processes incoming requests and
/// sends responses back to the child.
pub struct PermissionRequestHandler {
    /// The parent's permission context.
    sync_context: Arc<RwLock<PermissionSyncContext>>,
    /// Callback for making permission decisions.
    callback: Option<PermissionCallback>,
}

impl PermissionRequestHandler {
    /// Create a new handler with the given sync context.
    pub fn new(sync_context: Arc<RwLock<PermissionSyncContext>>) -> Self {
        Self {
            sync_context,
            callback: None,
        }
    }

    /// Set the permission decision callback.
    pub fn with_callback(mut self, callback: PermissionCallback) -> Self {
        self.callback = Some(callback);
        self
    }

    /// Handle an incoming permission request.
    ///
    /// Returns the response to send back to the child agent.
    pub async fn handle_request(&self, request: &PermissionRequest) -> PermissionResponse {
        let ctx = self.sync_context.read().await;

        // First check if already allowed by inherited or session rules
        let command = request.hint.as_deref();
        if ctx.is_allowed(&request.tool_name, command) {
            return PermissionResponse::approve();
        }

        // Check if denied
        if ctx.is_denied(&request.tool_name, command) {
            return PermissionResponse::deny("denied by permission rules");
        }

        // Check mode
        match ctx.mode() {
            PermissionMode::Auto => {
                // Auto approves at this layer. Bypass-immune safety guards
                // (catastrophic-command circuit breaker, sensitive-path
                // checks) live earlier in the pipeline; by the time we reach
                // permission_sync those have already had their say.
                let response = if let Some(ref rule_str) = request.suggested_rule {
                    PermissionResponse::approve()
                        .with_update(PermissionUpdate::allow(PermissionRule::parse(rule_str)))
                } else {
                    PermissionResponse::approve()
                };

                // Apply updates to our context
                drop(ctx);
                if !response.updates.is_empty() {
                    let mut ctx_mut = self.sync_context.write().await;
                    ctx_mut.apply_response(&response);
                }

                response
            }
            PermissionMode::AcceptEdits if accept_edits_auto_allows_request(request) => {
                let response = if let Some(ref rule_str) = request.suggested_rule {
                    PermissionResponse::approve()
                        .with_update(PermissionUpdate::allow(PermissionRule::parse(rule_str)))
                } else {
                    PermissionResponse::approve()
                };

                drop(ctx);
                if !response.updates.is_empty() {
                    let mut ctx_mut = self.sync_context.write().await;
                    ctx_mut.apply_response(&response);
                }

                response
            }
            PermissionMode::Deny => {
                // Deny mode: reject without escalation
                PermissionResponse::deny("permission mode is deny")
            }
            PermissionMode::Plan => {
                if crate::tool::schema::prune::PLAN_MODE_REQUIRED_TOOLS
                    .contains(&request.tool_name.as_str())
                {
                    PermissionResponse::approve()
                } else {
                    PermissionResponse::deny(crate::permission::engine::plan_mode_denial_reason(
                        &request.tool_name,
                        &request.args,
                    ))
                }
            }
            PermissionMode::AcceptEdits | PermissionMode::Prompt => {
                // Prompt mode: use callback or default logic
                drop(ctx);

                if let Some(ref callback) = self.callback {
                    let ctx = self.sync_context.read().await;
                    match callback(request, &ctx) {
                        PermissionDecision::Approve { updates } => {
                            let response = PermissionResponse {
                                approved: true,
                                reason: None,
                                updates: updates.clone(),
                            };

                            // Apply updates
                            drop(ctx);
                            if !updates.is_empty() {
                                let mut ctx_mut = self.sync_context.write().await;
                                ctx_mut.apply_response(&response);
                            }

                            response
                        }
                        PermissionDecision::Deny { reason } => PermissionResponse::deny(reason),
                        PermissionDecision::Escalate => {
                            // For now, treat escalate as deny for background agents
                            let ctx = self.sync_context.read().await;
                            if ctx.inherited.is_background {
                                PermissionResponse::deny("cannot escalate in background mode")
                            } else {
                                // Mark as pending for interactive resolution
                                // In real implementation, this would wait for user input
                                PermissionResponse::deny("interactive permission pending")
                            }
                        }
                    }
                } else {
                    PermissionResponse::deny("no permission handler configured")
                }
            }
        }
    }

    /// Process a message and return a response if it's a permission request.
    ///
    /// Returns `Some((correlation_id, response))` if the message was a permission request,
    /// or `None` if it wasn't.
    pub async fn process_message(
        &self,
        msg: &AgentMessage,
    ) -> Option<(String, PermissionResponse)> {
        if let MessagePayload::Request {
            request_type: RequestType::ToolPermission,
            data,
        } = &msg.payload
        {
            if let Some(request) = PermissionRequest::from_message_payload(data) {
                let correlation_id = msg
                    .correlation_id
                    .clone()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let response = self.handle_request(&request).await;
                return Some((correlation_id, response));
            }
        }
        None
    }

    /// Get the sync context.
    pub fn sync_context(&self) -> Arc<RwLock<PermissionSyncContext>> {
        Arc::clone(&self.sync_context)
    }
}

// ─── Tests for Handler ─────────────────────────────────────────────────────

#[cfg(test)]
mod handler_tests {
    use super::*;
    use std::sync::Arc as StdArc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn handler_auto_mode_approves() {
        let ctx = PermissionSyncContext::root(PermissionMode::Auto);
        let handler = PermissionRequestHandler::new(Arc::new(RwLock::new(ctx)));

        let request = PermissionRequest::new("bash", serde_json::json!({"command": "echo hello"}));
        let response = handler.handle_request(&request).await;

        assert!(response.approved);
    }

    #[tokio::test]
    async fn handler_deny_mode_denies() {
        let ctx = PermissionSyncContext::root(PermissionMode::Deny);
        let handler = PermissionRequestHandler::new(Arc::new(RwLock::new(ctx)));

        let request = PermissionRequest::new("bash", serde_json::json!({"command": "rm -rf /"}));
        let response = handler.handle_request(&request).await;

        assert!(!response.approved);
        assert!(response.reason.as_ref().unwrap().contains("deny"));
    }

    #[tokio::test]
    async fn handler_plan_mode_denies() {
        let ctx = PermissionSyncContext::root(PermissionMode::Plan);
        let handler = PermissionRequestHandler::new(Arc::new(RwLock::new(ctx)));

        let request = PermissionRequest::new("write_file", serde_json::json!({"path": "plan.txt"}));
        let response = handler.handle_request(&request).await;

        assert!(!response.approved);
        assert!(response.reason.as_ref().unwrap().contains("plan"));
    }

    #[tokio::test]
    async fn handler_plan_mode_allows_plan_control_tools() {
        let ctx = PermissionSyncContext::root(PermissionMode::Plan);
        let handler = PermissionRequestHandler::new(Arc::new(RwLock::new(ctx)));

        let request =
            PermissionRequest::new("exit_plan_mode", serde_json::json!({"plan": "# plan"}));
        let response = handler.handle_request(&request).await;

        assert!(response.approved);
    }

    #[tokio::test]
    async fn handler_plan_mode_guides_legacy_aliases_to_exit_plan_mode() {
        let ctx = PermissionSyncContext::root(PermissionMode::Plan);
        let handler = PermissionRequestHandler::new(Arc::new(RwLock::new(ctx)));

        let request =
            PermissionRequest::new("session", serde_json::json!({"action": "exit_plan_mode"}));
        let response = handler.handle_request(&request).await;

        assert!(!response.approved);
        assert!(
            response
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("Use `exit_plan_mode` directly"))
        );
    }

    #[tokio::test]
    async fn handler_accept_edits_auto_approves_workspace_write_without_callback() {
        let ctx = PermissionSyncContext::root(PermissionMode::AcceptEdits);
        let callback_calls = StdArc::new(AtomicUsize::new(0));
        let seen = StdArc::clone(&callback_calls);
        let handler = PermissionRequestHandler::new(Arc::new(RwLock::new(ctx))).with_callback(
            Box::new(move |_req, _ctx| {
                seen.fetch_add(1, Ordering::SeqCst);
                PermissionDecision::deny("callback should not run for workspace edits")
            }),
        );

        let request = PermissionRequest::new(
            "write_file",
            serde_json::json!({"path": "src/lib.rs", "content": "pub fn shipped() {}\n"}),
        );
        let response = handler.handle_request(&request).await;

        assert!(response.approved);
        assert_eq!(callback_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn handler_accept_edits_background_denies_external_write_escalation() {
        let mut inherited = InheritedPermissions::new(PermissionMode::AcceptEdits);
        inherited.is_background = true;
        let ctx = PermissionSyncContext::new(inherited);
        let callback_calls = StdArc::new(AtomicUsize::new(0));
        let seen = StdArc::clone(&callback_calls);
        let handler = PermissionRequestHandler::new(Arc::new(RwLock::new(ctx))).with_callback(
            Box::new(move |_req, _ctx| {
                seen.fetch_add(1, Ordering::SeqCst);
                PermissionDecision::Escalate
            }),
        );

        let request = PermissionRequest::new(
            "write_file",
            serde_json::json!({"path": "/tmp/outside.rs", "content": "pub fn nope() {}\n"}),
        );
        let response = handler.handle_request(&request).await;

        assert!(!response.approved);
        assert_eq!(callback_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            response.reason.as_deref(),
            Some("cannot escalate in background mode")
        );
    }

    #[tokio::test]
    async fn handler_accept_edits_still_asks_callback_for_bash() {
        let ctx = PermissionSyncContext::root(PermissionMode::AcceptEdits);
        let callback_calls = StdArc::new(AtomicUsize::new(0));
        let seen = StdArc::clone(&callback_calls);
        let handler = PermissionRequestHandler::new(Arc::new(RwLock::new(ctx))).with_callback(
            Box::new(move |_req, _ctx| {
                seen.fetch_add(1, Ordering::SeqCst);
                PermissionDecision::deny("bash still needs approval")
            }),
        );

        let request = PermissionRequest::new("bash", serde_json::json!({"command": "cargo test"}));
        let response = handler.handle_request(&request).await;

        assert!(!response.approved);
        assert_eq!(callback_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            response.reason.as_deref(),
            Some("bash still needs approval")
        );
    }

    #[tokio::test]
    async fn handler_prompt_without_callback_fails_closed_for_foreground_agent() {
        let ctx = PermissionSyncContext::root(PermissionMode::Prompt);
        let handler = PermissionRequestHandler::new(Arc::new(RwLock::new(ctx)));

        let request = PermissionRequest::new("write_file", serde_json::json!({"path": "out.txt"}));
        let response = handler.handle_request(&request).await;

        assert!(!response.approved);
        assert!(
            response
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("no permission handler configured")),
            "missing callback should fail closed with an actionable reason, got {response:?}"
        );
    }

    #[tokio::test]
    async fn handler_accept_edits_without_callback_fails_closed_for_mutation() {
        let ctx = PermissionSyncContext::root(PermissionMode::AcceptEdits);
        let handler = PermissionRequestHandler::new(Arc::new(RwLock::new(ctx)));

        let request = PermissionRequest::new("bash", serde_json::json!({"command": "npm test"}));
        let response = handler.handle_request(&request).await;

        assert!(!response.approved);
        assert!(
            response
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("no permission handler configured")),
            "missing callback should fail closed with an actionable reason, got {response:?}"
        );
    }

    #[tokio::test]
    async fn handler_respects_inherited_allow() {
        let mut inherited = InheritedPermissions::new(PermissionMode::Prompt);
        inherited.add_allow(PermissionRule::parse("bash(git:*)"));
        let ctx = PermissionSyncContext::new(inherited);
        let handler = PermissionRequestHandler::new(Arc::new(RwLock::new(ctx)));

        let request = PermissionRequest::new("bash", serde_json::json!({"command": "git status"}))
            .with_hint("git status");
        let response = handler.handle_request(&request).await;

        assert!(response.approved);
    }

    #[tokio::test]
    async fn handler_respects_inherited_deny() {
        let mut inherited = InheritedPermissions::new(PermissionMode::Auto);
        inherited.add_deny(PermissionRule::parse("bash(rm -rf:*)"));
        let ctx = PermissionSyncContext::new(inherited);
        let handler = PermissionRequestHandler::new(Arc::new(RwLock::new(ctx)));

        let request = PermissionRequest::new("bash", serde_json::json!({"command": "rm -rf /"}))
            .with_hint("rm -rf /");
        let response = handler.handle_request(&request).await;

        assert!(!response.approved);
    }

    #[tokio::test]
    async fn handler_uses_callback() {
        let ctx = PermissionSyncContext::root(PermissionMode::Prompt);
        let handler = PermissionRequestHandler::new(Arc::new(RwLock::new(ctx))).with_callback(
            Box::new(|req, _ctx| {
                if req.tool_name == "bash" {
                    PermissionDecision::approve_with_rule(PermissionRule::parse("bash(git:*)"))
                } else {
                    PermissionDecision::deny("unknown tool")
                }
            }),
        );

        let request = PermissionRequest::new("bash", serde_json::json!({"command": "git status"}));
        let response = handler.handle_request(&request).await;

        assert!(response.approved);
        assert_eq!(response.updates.len(), 1);
        assert_eq!(response.updates[0].rule.tool, "bash");
    }

    #[tokio::test]
    async fn handler_applies_suggested_rule_in_auto() {
        let ctx = PermissionSyncContext::root(PermissionMode::Auto);
        let handler = PermissionRequestHandler::new(Arc::new(RwLock::new(ctx)));

        let request = PermissionRequest::new("bash", serde_json::json!({"command": "git status"}))
            .with_suggested_rule("bash(git:*)");
        let response = handler.handle_request(&request).await;

        assert!(response.approved);
        assert_eq!(response.updates.len(), 1);

        // Verify rule was applied to context
        let sync_ctx = handler.sync_context();
        let ctx = sync_ctx.read().await;
        assert!(ctx.is_allowed("bash", Some("git push")));
    }

    #[tokio::test]
    async fn handler_process_message() {
        let ctx = PermissionSyncContext::root(PermissionMode::Auto);
        let handler = PermissionRequestHandler::new(Arc::new(RwLock::new(ctx)));

        let request = PermissionRequest::new("bash", serde_json::json!({"command": "ls"}));
        let from = AgentAddress::new("child-run", "child");
        let to = AgentAddress::new("parent-run", "parent");
        let msg = request.to_message(&from, &to).with_correlation("req-456");

        let result = handler.process_message(&msg).await;
        assert!(result.is_some());

        let (correlation_id, response) = result.unwrap();
        assert_eq!(correlation_id, "req-456");
        assert!(response.approved);
    }
}
