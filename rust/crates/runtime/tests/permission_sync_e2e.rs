//! End-to-end integration tests for permission synchronization.
//!
//! Tests the full flow: parent agent → spawn child → permission request → response,
//! covering inherited permissions, mailbox communication, and permission handler logic.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;

/// Timeout for permission request calls in tests.
/// Increase if CI environments are slow.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Timeout for receiving messages from mailbox.
const RECV_TIMEOUT: Duration = Duration::from_secs(2);
/// Short timeout for testing timeout behavior itself.
const SHORT_TIMEOUT: Duration = Duration::from_millis(100);

use astra_runtime::messaging::in_process::InProcessTransport;
use astra_runtime::messaging::router::AgentMailboxRouter;
use astra_runtime::messaging::types::AgentAddress;
use astra_runtime::orchestration::{
    InheritedPermissions, PermissionDecision, PermissionMode, PermissionRequest,
    PermissionRequestHandler, PermissionRule, PermissionSyncContext, PermissionUpdate,
};
use astra_runtime::server::delegation_engine::{DelegationTracker, SubRunRecord, SubRunState};

// ─── Helpers ────────────────────────────────────────────────────────────────

fn addr(run: &str, agent: &str) -> AgentAddress {
    AgentAddress::new(run, agent)
}

fn tracker() -> Arc<DelegationTracker> {
    Arc::new(DelegationTracker::new())
}

async fn setup_router(dt: Arc<DelegationTracker>) -> Arc<AgentMailboxRouter> {
    let transport = Arc::new(InProcessTransport::new());
    Arc::new(AgentMailboxRouter::new(transport, dt))
}

// ─── E2E: Parent-Child Permission Flow ──────────────────────────────────────

/// Full end-to-end test: child sends permission request, parent processes and responds
#[tokio::test]
async fn e2e_child_requests_permission_parent_approves() {
    let dt = tracker();
    let router = setup_router(dt.clone()).await;

    // Setup parent and child addresses
    let parent_addr = addr("parent-run", "orchestrator");
    let child_addr = addr("child-run", "explorer");
    let child_addr_for_response = child_addr.clone();

    // Register parent's mailbox
    let parent_mailbox = router.register(parent_addr.clone(), None).await.unwrap();

    // Setup delegation relationship
    dt.record_sub_run(SubRunRecord {
        run_id: "child-run".into(),
        parent_run_id: "parent-run".into(),
        delegation_id: "del-1".into(),
        agent_id: "explorer".into(),
        depth: 1,
        state: SubRunState::Created,
        retry_of: None,
    })
    .await;

    // Parent's permission context with auto mode
    let parent_ctx = PermissionSyncContext::root(PermissionMode::Auto);
    let handler = PermissionRequestHandler::new(Arc::new(RwLock::new(parent_ctx)));

    // Child spawns and sends a permission request
    let child_router = router.clone();
    let child_handle = tokio::spawn(async move {
        // Register child mailbox
        let mut child_mailbox = child_router
            .register(child_addr.clone(), None)
            .await
            .unwrap();

        // Create permission request
        let request = PermissionRequest::new("bash", serde_json::json!({"command": "git status"}))
            .with_hint("git status")
            .with_suggested_rule("bash(git:*)");

        // Send and wait for response
        child_mailbox
            .request_permission(request, REQUEST_TIMEOUT)
            .await
    });

    // Parent waits for message using recv()
    let msg = tokio::time::timeout(RECV_TIMEOUT, parent_mailbox.recv())
        .await
        .expect("should receive within timeout")
        .expect("should have a message");

    // Process through handler
    let result = handler.process_message(&msg).await;
    assert!(result.is_some(), "should recognize permission request");

    let (correlation_id, response) = result.unwrap();
    assert!(response.approved);
    assert_eq!(response.updates.len(), 1); // suggested rule applied

    // Send response back
    let response_msg = response.to_message(&parent_addr, &child_addr_for_response, &correlation_id);
    router.send(response_msg).await.unwrap();

    // Child should receive approval
    let child_result = child_handle.await.unwrap();
    assert!(child_result.is_ok());
    let resp = child_result.unwrap();
    assert!(resp.approved);
    assert_eq!(resp.updates.len(), 1);
    assert_eq!(resp.updates[0].rule.tool, "bash");
}

/// Test: parent denies based on inherited deny rules
#[tokio::test]
async fn e2e_parent_denies_based_on_rules() {
    let dt = tracker();
    let router = setup_router(dt.clone()).await;

    let parent_addr = addr("parent-run", "orchestrator");
    let child_addr = addr("child-run", "worker");
    let child_addr_for_response = child_addr.clone();

    let parent_mailbox = router.register(parent_addr.clone(), None).await.unwrap();

    dt.record_sub_run(SubRunRecord {
        run_id: "child-run".into(),
        parent_run_id: "parent-run".into(),
        delegation_id: "del-2".into(),
        agent_id: "worker".into(),
        depth: 1,
        state: SubRunState::Created,
        retry_of: None,
    })
    .await;

    // Parent context with deny rule for dangerous commands
    let mut inherited = InheritedPermissions::new(PermissionMode::Auto);
    inherited.add_deny(PermissionRule::parse("bash(rm -rf:*)"));
    let parent_ctx = PermissionSyncContext::new(inherited);
    let handler = PermissionRequestHandler::new(Arc::new(RwLock::new(parent_ctx)));

    // Child sends dangerous request
    let child_router = router.clone();
    let child_handle = tokio::spawn(async move {
        let mut child_mailbox = child_router
            .register(child_addr.clone(), None)
            .await
            .unwrap();

        let request =
            PermissionRequest::new("bash", serde_json::json!({"command": "rm -rf /important"}))
                .with_hint("rm -rf /important");

        child_mailbox
            .request_permission(request, REQUEST_TIMEOUT)
            .await
    });

    // Parent processes
    let msg = tokio::time::timeout(RECV_TIMEOUT, parent_mailbox.recv())
        .await
        .expect("should receive request")
        .expect("should have message");

    let (correlation_id, response) = handler.process_message(&msg).await.unwrap();
    assert!(!response.approved);
    assert!(response.reason.as_ref().unwrap().contains("denied"));

    // Send response
    let response_msg = response.to_message(&parent_addr, &child_addr_for_response, &correlation_id);
    router.send(response_msg).await.unwrap();

    // Child should receive denial
    let child_result = child_handle.await.unwrap();
    assert!(child_result.is_ok());
    let resp = child_result.unwrap();
    assert!(!resp.approved);
}

/// Test: permission callback controls decisions
#[tokio::test]
async fn e2e_callback_controls_permission() {
    let dt = tracker();
    let router = setup_router(dt.clone()).await;

    let parent_addr = addr("parent-run", "orchestrator");
    let child_addr = addr("child-run", "analyzer");
    let child_addr_for_response = child_addr.clone();

    let parent_mailbox = router.register(parent_addr.clone(), None).await.unwrap();

    dt.record_sub_run(SubRunRecord {
        run_id: "child-run".into(),
        parent_run_id: "parent-run".into(),
        delegation_id: "del-3".into(),
        agent_id: "analyzer".into(),
        depth: 1,
        state: SubRunState::Created,
        retry_of: None,
    })
    .await;

    // Handler with custom callback
    let parent_ctx = PermissionSyncContext::root(PermissionMode::Prompt);
    let handler = PermissionRequestHandler::new(Arc::new(RwLock::new(parent_ctx))).with_callback(
        Box::new(|req, _ctx| {
            // Only allow view and grep, deny everything else
            if req.tool_name == "view" || req.tool_name == "grep" {
                PermissionDecision::approve()
            } else {
                PermissionDecision::deny(format!("tool {} not allowed by policy", req.tool_name))
            }
        }),
    );

    // Test: allowed tool (view)
    let child_router = router.clone();
    let child_handle = tokio::spawn(async move {
        let mut child_mailbox = child_router
            .register(child_addr.clone(), None)
            .await
            .unwrap();

        let request = PermissionRequest::new("view", serde_json::json!({"path": "/etc/passwd"}));

        child_mailbox
            .request_permission(request, REQUEST_TIMEOUT)
            .await
    });

    let msg = tokio::time::timeout(RECV_TIMEOUT, parent_mailbox.recv())
        .await
        .unwrap()
        .unwrap();

    let (cid, response) = handler.process_message(&msg).await.unwrap();
    assert!(response.approved);

    let response_msg = response.to_message(&parent_addr, &child_addr_for_response, &cid);
    router.send(response_msg).await.unwrap();

    let result = child_handle.await.unwrap().unwrap();
    assert!(result.approved);
}

/// Test: inherited permissions propagate to child context
#[tokio::test]
async fn e2e_inherited_permissions_propagate() {
    // Parent creates inherited permissions for child
    let mut parent_ctx = PermissionSyncContext::root(PermissionMode::Prompt);

    // Add session allow rule
    let update = PermissionUpdate::allow(PermissionRule::parse("bash(git:*)"));
    parent_ctx.apply_update(&update);

    // Create child's inherited permissions
    let child_inherited = parent_ctx.for_child(true); // background child

    assert!(child_inherited.is_background);
    assert!(child_inherited.is_allowed("bash", Some("git status")));
    assert!(child_inherited.is_allowed("bash", Some("git push")));
    assert!(!child_inherited.is_allowed("bash", Some("npm install")));

    // Child's context should use these
    let child_ctx = PermissionSyncContext::new(child_inherited);
    assert_eq!(child_ctx.mode(), PermissionMode::Prompt);
    assert!(child_ctx.is_allowed("bash", Some("git commit -m 'test'")));
}

/// Test: permission updates are applied and persist in context
#[tokio::test]
async fn e2e_permission_updates_persist() {
    let parent_ctx = Arc::new(RwLock::new(PermissionSyncContext::root(
        PermissionMode::Auto,
    )));
    let handler = PermissionRequestHandler::new(parent_ctx.clone());

    // Request with suggested rule for bash git commands
    let request = PermissionRequest::new("bash", serde_json::json!({"command": "git status"}))
        .with_suggested_rule("bash(git:*)");

    let response = handler.handle_request(&request).await;
    assert!(response.approved);
    assert_eq!(response.updates.len(), 1);

    // Check rule was applied to context
    let ctx = parent_ctx.read().await;
    assert!(ctx.is_allowed("bash", Some("git push")));
    assert!(ctx.is_allowed("bash", Some("git commit -m 'test'")));
    // Doesn't match other commands
    assert!(!ctx.is_allowed("bash", Some("npm install")));
}

/// Test: deny mode rejects all requests
#[tokio::test]
async fn e2e_deny_mode_rejects_all() {
    let parent_ctx = PermissionSyncContext::root(PermissionMode::Deny);
    let handler = PermissionRequestHandler::new(Arc::new(RwLock::new(parent_ctx)));

    let requests = vec![
        PermissionRequest::new("bash", serde_json::json!({"command": "ls"})),
        PermissionRequest::new("edit", serde_json::json!({"path": "/tmp/file"})),
        PermissionRequest::new("view", serde_json::json!({"path": "/etc/passwd"})),
    ];

    for req in requests {
        let response = handler.handle_request(&req).await;
        assert!(!response.approved);
        assert!(response.reason.as_ref().unwrap().contains("deny"));
    }
}

/// Test: tool allowlist in inherited permissions
#[tokio::test]
async fn e2e_tool_allowlist() {
    let inherited = InheritedPermissions {
        mode: PermissionMode::Prompt,
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

/// Test: multi-level delegation (grandchild)
#[tokio::test]
async fn e2e_multi_level_delegation() {
    // Root → Child → Grandchild
    let root_ctx = PermissionSyncContext::root(PermissionMode::Prompt);

    // Add allow rule at root level
    let mut root_ctx = root_ctx;
    root_ctx.apply_update(&PermissionUpdate::allow(PermissionRule::parse(
        "bash(git:*)",
    )));

    // Create child inherited
    let child_inherited = root_ctx.for_child(false);
    let mut child_ctx = PermissionSyncContext::new(child_inherited);

    // Child adds another rule
    child_ctx.apply_update(&PermissionUpdate::allow(PermissionRule::parse(
        "bash(npm:*)",
    )));

    // Create grandchild inherited
    let grandchild_inherited = child_ctx.for_child(true); // background

    // Grandchild should have both rules
    assert!(grandchild_inherited.is_background);
    assert!(grandchild_inherited.is_allowed("bash", Some("git push")));
    assert!(grandchild_inherited.is_allowed("bash", Some("npm install")));
    assert!(!grandchild_inherited.is_allowed("bash", Some("rm -rf /")));
}

/// Test: timeout when parent doesn't respond
#[tokio::test]
async fn e2e_timeout_no_response() {
    let dt = tracker();
    let router = setup_router(dt.clone()).await;

    let parent_addr = addr("parent-run", "orchestrator");
    let child_addr = addr("child-run", "worker");

    // Register both but don't process parent's messages
    let _parent_mailbox = router.register(parent_addr.clone(), None).await.unwrap();
    let mut child_mailbox = router.register(child_addr.clone(), None).await.unwrap();

    dt.record_sub_run(SubRunRecord {
        run_id: "child-run".into(),
        parent_run_id: "parent-run".into(),
        delegation_id: "del-timeout".into(),
        agent_id: "worker".into(),
        depth: 1,
        state: SubRunState::Created,
        retry_of: None,
    })
    .await;

    // Child sends request with short timeout
    let request = PermissionRequest::new("bash", serde_json::json!({"command": "test"}));
    let result = child_mailbox
        .request_permission(request, SHORT_TIMEOUT)
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(format!("{}", err).contains("timeout") || format!("{:?}", err).contains("Timeout"));
}
