//! Unhappy-path integration tests for the server executor layer.
//!
//! Covers: provider lease expiry during execution, workspace partial-creation
//! cleanup chain, cancellation race with tool completion, transport timeout
//! during simulated network partition, provider degradation between
//! resolve and execute, and all-providers-unhealthy baseline.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use astra_runtime::capability_registry::CapabilityRegistry;
use astra_runtime::provider::traits::{CapabilityProvider, ProviderError, ToolRequest, ToolResult};
use astra_runtime::provider::types::{ProviderKind, ToolCapability, ToolCategory};
use astra_runtime_env::IsolationIntent;
use astra_runtime_env::{
    CleanupReason, WorkspaceAuthority, WorkspaceBindingKind, WorkspaceOwnerScope,
    WorkspacePersistence, WorkspaceRecord, WorkspaceSource,
};
use astra_services::{
    InMemoryWorkspaceRecordStore, WorkspaceCleanupDebtEntry, WorkspaceCleanupDebtStore,
    WorkspaceRecordEntry, WorkspaceRecordStore,
};
use async_trait::async_trait;
use serde_json::json;
use tokio::time::timeout;

// ---------------------------------------------------------------------------
// Shared test helpers
// ---------------------------------------------------------------------------

/// A controllable provider: you can set its health status externally and
/// make its execute hang or fail on demand.
struct ControllableProvider {
    isolation: IsolationIntent,
    kind: ProviderKind,
    priority: u8,
    capabilities: Vec<ToolCapability>,
    storage: bool,
    /// If true, health_check returns Ok; otherwise Err.
    healthy: AtomicBool,
    /// If true, execute will block until cancellation.
    hang_on_execute: AtomicBool,
    /// How long execute() takes when not hanging.
    exec_delay_ms: u64,
}

impl ControllableProvider {
    fn new(
        isolation: IsolationIntent,
        kind: ProviderKind,
        priority: u8,
        capabilities: Vec<ToolCapability>,
        storage: bool,
    ) -> Self {
        Self {
            isolation,
            kind,
            priority,
            capabilities,
            storage,
            healthy: AtomicBool::new(true),
            hang_on_execute: AtomicBool::new(false),
            exec_delay_ms: 0,
        }
    }

    fn set_healthy(&self, h: bool) {
        self.healthy.store(h, Ordering::SeqCst);
    }

    fn set_hang(&self, h: bool) {
        self.hang_on_execute.store(h, Ordering::SeqCst);
    }
}

#[async_trait]
impl CapabilityProvider for ControllableProvider {
    fn kind(&self) -> ProviderKind {
        self.kind
    }

    fn isolation_level(&self) -> IsolationIntent {
        self.isolation
    }

    fn priority(&self) -> u8 {
        self.priority
    }

    async fn storage_accessible(&self) -> bool {
        self.storage
    }

    async fn capabilities(&self) -> Vec<ToolCapability> {
        self.capabilities.clone()
    }

    async fn execute(&self, _req: ToolRequest) -> ToolResult {
        if self.hang_on_execute.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            ToolResult::Error {
                message: "hung".into(),
                retryable: true,
            }
        } else if self.exec_delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.exec_delay_ms)).await;
            ToolResult::Success {
                data: json!({"ok": true}),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        } else {
            ToolResult::Success {
                data: json!({"ok": true}),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        }
    }

    async fn health_check(&self) -> Result<(), ProviderError> {
        if self.healthy.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(ProviderError::Internal("unhealthy".into()))
        }
    }
}

// ---------------------------------------------------------------------------
// Test 1: All providers unhealthy — resolve still works, but callers
// should gate on health_check before dispatching.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn all_unhealthy_providers_resolve_works_but_health_fails() {
    let reg = CapabilityRegistry::new();

    let p1 = Arc::new(ControllableProvider::new(
        IsolationIntent::Process,
        ProviderKind::SandboxRuntime,
        5,
        vec![ToolCapability::Category(ToolCategory::Shell)],
        false,
    ));
    let p2 = Arc::new(ControllableProvider::new(
        IsolationIntent::Process,
        ProviderKind::EdgeConnection,
        10,
        vec![ToolCapability::Category(ToolCategory::Shell)],
        false,
    ));
    p1.set_healthy(false);
    p2.set_healthy(false);

    reg.register("provider-1", p1).await.unwrap();
    reg.register("provider-2", p2).await.unwrap();

    let results = reg.health_check_all().await;
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|(_, r)| r.is_err()));

    let request = ToolRequest {
        capability: ToolCapability::Category(ToolCategory::Shell),
        tool_name: "bash".into(),
        tool_call_id: "unhealthy-1".into(),
        parameters: json!(null),
        isolation_required: IsolationIntent::Process,
        storage: None,
        user_id: "test-user".into(),
        run_id: "test-run".into(),
        session_id: "test-session".into(),
    };
    let resolved = reg.resolve(&request).await;
    assert!(resolved.is_ok());
    assert_eq!(resolved.unwrap().priority(), 5);
}

#[tokio::test]
async fn empty_registry_not_capable_vs_all_unhealthy() {
    let reg = CapabilityRegistry::new();
    let request = ToolRequest {
        capability: ToolCapability::Named("bash".into()),
        tool_name: "bash".into(),
        tool_call_id: "empty-1".into(),
        parameters: json!(null),
        isolation_required: IsolationIntent::None,
        storage: None,
        user_id: "test-user".into(),
        run_id: "test-run".into(),
        session_id: "test-session".into(),
    };
    let result = reg.resolve(&request).await;
    assert!(matches!(result, Err(ProviderError::NotCapable { .. })));

    let p = Arc::new(ControllableProvider::new(
        IsolationIntent::None,
        ProviderKind::EdgeConnection,
        1,
        vec![ToolCapability::Named("bash".into())],
        false,
    ));
    p.set_healthy(false);
    reg.register("edge", p).await.unwrap();

    let result2 = reg.resolve(&request).await;
    assert!(
        result2.is_ok(),
        "unhealthy providers still match by capability"
    );
}

// ---------------------------------------------------------------------------
// Test 2: Cancellation race with tool completion
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancellation_race_with_fast_completion_no_deadlock() {
    let cancel = Arc::new(tokio_util::sync::CancellationToken::new());

    let exec_cancel = cancel.clone();
    let exec_future = async move {
        tokio::select! {
            _ = exec_cancel.cancelled() => "cancelled".to_string(),
            _ = tokio::time::sleep(Duration::from_millis(10)) => "completed".to_string(),
        }
    };

    let cancel_future = async move {
        tokio::time::sleep(Duration::from_millis(1)).await;
        cancel.cancel();
    };

    let (result, _) = tokio::join!(exec_future, cancel_future);
    assert!(
        result == "completed" || result == "cancelled",
        "unexpected: {result}"
    );
}

#[tokio::test]
async fn pre_cancelled_token_is_idempotent() {
    let cancel = Arc::new(tokio_util::sync::CancellationToken::new());
    cancel.cancel();

    let exec_cancel = cancel.clone();
    let exec_future = async move {
        tokio::select! {
            _ = exec_cancel.cancelled() => "cancelled",
            _ = tokio::time::sleep(Duration::from_millis(5)) => "completed",
        }
    };

    let result = timeout(Duration::from_secs(1), exec_future).await;
    assert!(result.is_ok(), "pre-cancelled should complete immediately");
    assert_eq!(result.unwrap(), "cancelled");
}

#[tokio::test]
async fn multiple_concurrent_cancellations_no_panic() {
    let cancel = Arc::new(tokio_util::sync::CancellationToken::new());

    let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    for _ in 0..10 {
        let c = cancel.clone();
        handles.push(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1)).await;
            c.cancel();
        }));
    }

    for _ in 0..10 {
        let c = cancel.clone();
        handles.push(tokio::spawn(async move {
            c.cancelled().await;
        }));
    }

    for h in handles {
        let _ = h.await;
    }

    assert!(cancel.is_cancelled());
}

// ---------------------------------------------------------------------------
// Test 3: Provider degrades between resolve and execute
// ---------------------------------------------------------------------------

#[tokio::test]
async fn provider_degrades_between_resolve_and_execute() {
    let reg = CapabilityRegistry::new();

    let p = Arc::new(ControllableProvider::new(
        IsolationIntent::Process,
        ProviderKind::SandboxRuntime,
        1,
        vec![ToolCapability::Category(ToolCategory::Shell)],
        false,
    ));
    reg.register("provider", p.clone()).await.unwrap();

    let request = ToolRequest {
        capability: ToolCapability::Category(ToolCategory::Shell),
        tool_name: "bash".into(),
        tool_call_id: "degrade-1".into(),
        parameters: json!({"cmd": "echo hello"}),
        isolation_required: IsolationIntent::Process,
        storage: None,
        user_id: "test-user".into(),
        run_id: "test-run".into(),
        session_id: "test-session".into(),
    };

    let resolved = reg.resolve(&request).await.unwrap();

    p.set_healthy(false);

    let result = resolved.execute(request).await;
    match result {
        ToolResult::Success { .. } => {}
        ToolResult::Error { .. } => {}
    }

    assert!(p.health_check().await.is_err());
}

// ---------------------------------------------------------------------------
// Test 4: Provider lease expiry during execution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lease_expired_provider_still_resolves_but_is_unhealthy() {
    let reg = CapabilityRegistry::new();

    let provider = Arc::new(ControllableProvider::new(
        IsolationIntent::Container,
        ProviderKind::SandboxRuntime,
        1,
        vec![
            ToolCapability::Category(ToolCategory::Shell),
            ToolCapability::Category(ToolCategory::FileSystem),
        ],
        true,
    ));
    reg.register("provider-expired", provider.clone())
        .await
        .unwrap();
    assert!(provider.health_check().await.is_ok());

    provider.set_healthy(false);

    let request = ToolRequest {
        capability: ToolCapability::Category(ToolCategory::Shell),
        tool_name: "bash".into(),
        tool_call_id: "lease-1".into(),
        parameters: json!(null),
        isolation_required: IsolationIntent::Container,
        storage: None,
        user_id: "test-user".into(),
        run_id: "test-run".into(),
        session_id: "test-session".into(),
    };
    let resolved = reg.resolve(&request).await.unwrap();
    assert_eq!(resolved.priority(), 1);

    let results = reg.health_check_all().await;
    assert_eq!(results.len(), 1);
    assert!(results[0].1.is_err());
}

#[tokio::test]
async fn all_providers_lease_expired_resolve_works_health_all_fails() {
    let reg = CapabilityRegistry::new();

    for i in 0..3u8 {
        let r = Arc::new(ControllableProvider::new(
            IsolationIntent::Process,
            ProviderKind::SandboxRuntime,
            i,
            vec![ToolCapability::Category(ToolCategory::Shell)],
            false,
        ));
        r.set_healthy(false);
        reg.register(format!("provider-{i}"), r).await.unwrap();
    }

    let request = ToolRequest {
        capability: ToolCapability::Category(ToolCategory::Shell),
        tool_name: "bash".into(),
        tool_call_id: "all-expired-1".into(),
        parameters: json!(null),
        isolation_required: IsolationIntent::Process,
        storage: None,
        user_id: "test-user".into(),
        run_id: "test-run".into(),
        session_id: "test-session".into(),
    };
    let resolved = reg.resolve(&request).await;
    assert!(resolved.is_ok(), "resolve works despite all leases expired");

    let results = reg.health_check_all().await;
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(|(_, r)| r.is_err()));
}

// ---------------------------------------------------------------------------
// Test 5: Transport timeout during network partition
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hung_transport_times_out_at_caller_level() {
    let reg = CapabilityRegistry::new();

    let hung = Arc::new(ControllableProvider::new(
        IsolationIntent::Process,
        ProviderKind::EdgeConnection,
        5,
        vec![ToolCapability::Category(ToolCategory::Shell)],
        false,
    ));
    hung.set_hang(true);
    reg.register("hung-provider", hung.clone()).await.unwrap();

    let request = ToolRequest {
        capability: ToolCapability::Category(ToolCategory::Shell),
        tool_name: "bash".into(),
        tool_call_id: "hung-1".into(),
        parameters: json!(null),
        isolation_required: IsolationIntent::Process,
        storage: None,
        user_id: "test-user".into(),
        run_id: "test-run".into(),
        session_id: "test-session".into(),
    };
    let resolved = reg.resolve(&request).await.unwrap();

    let exec_result = timeout(Duration::from_millis(200), resolved.execute(request)).await;
    assert!(
        exec_result.is_err(),
        "hung transport call should time out at caller level"
    );
}

#[tokio::test]
async fn network_partition_all_providers_hung() {
    let reg = CapabilityRegistry::new();

    for i in 0..3u8 {
        let r = Arc::new(ControllableProvider::new(
            IsolationIntent::Process,
            ProviderKind::SandboxRuntime,
            i,
            vec![ToolCapability::Category(ToolCategory::Shell)],
            false,
        ));
        r.set_hang(true);
        reg.register(format!("provider-{i}"), r).await.unwrap();
    }

    let request = ToolRequest {
        capability: ToolCapability::Category(ToolCategory::Shell),
        tool_name: "bash".into(),
        tool_call_id: "partition-1".into(),
        parameters: json!(null),
        isolation_required: IsolationIntent::Process,
        storage: None,
        user_id: "test-user".into(),
        run_id: "test-run".into(),
        session_id: "test-session".into(),
    };
    let resolved = reg.resolve(&request).await.unwrap();

    let exec_result = timeout(Duration::from_millis(200), resolved.execute(request)).await;
    assert!(
        exec_result.is_err(),
        "all providers hung -> timeout at caller level"
    );

    let health_results = reg.health_check_all().await;
    assert_eq!(health_results.len(), 3);
}

// ===========================================================================
// Test 6: Workspace partial-creation cleanup chain
// ===========================================================================

fn test_workspace_record(workspace_id: &str) -> WorkspaceRecord {
    WorkspaceRecord {
        workspace_id: workspace_id.into(),
        owner_scope: WorkspaceOwnerScope::User,
        kind: WorkspaceBindingKind::LocalFilesystem,
        authority: WorkspaceAuthority::ReadWrite,
        root_or_volume_ref: "/tmp/test-workspace".into(),
        source: WorkspaceSource::Scratch,
        persistence: WorkspacePersistence::Session,
        revision: "v0".into(),
        display_name: "test-ws".into(),
    }
}

#[tokio::test]
async fn partial_workspace_creation_records_cleanup_debts() {
    let store = InMemoryWorkspaceRecordStore::new();
    let owner_id = "owner-1";
    let ws_id = "ws-partial-fail";

    let record = test_workspace_record(ws_id);
    let entry = WorkspaceRecordEntry::new(owner_id, Some("session-1".into()), None, record);
    store
        .upsert_workspace_record(entry)
        .await
        .expect("upsert should succeed");

    let loaded = store
        .load_workspace_record(owner_id, ws_id)
        .await
        .unwrap()
        .expect("workspace must exist");
    assert_eq!(loaded.workspace_id(), ws_id);
    assert_eq!(loaded.owner_id, owner_id);

    let debt = WorkspaceCleanupDebtEntry::new(
        owner_id,
        Some("session-1".into()),
        None,
        test_workspace_record(ws_id),
        CleanupReason::Failed,
        "clone-failed-file-system-left-dirty",
    );
    store
        .record_cleanup_debt(debt)
        .await
        .expect("cleanup debt recording must succeed");

    let debts = store.list_cleanup_debts(owner_id, 100).await.unwrap();
    assert_eq!(debts.len(), 1);
    assert_eq!(debts[0].workspace_id, ws_id);
    assert!(
        debts[0].message.contains("clone-failed"),
        "debt reason should describe the partial failure"
    );

    let other_debts = store.list_cleanup_debts("other-owner", 100).await.unwrap();
    assert!(other_debts.is_empty(), "other owners should not see debts");
}

#[tokio::test]
async fn compound_workspace_failure_multiple_cleanup_debts() {
    let store = InMemoryWorkspaceRecordStore::new();
    let owner_id = "owner-2";
    let ws_id = "ws-compound-fail";

    let record = test_workspace_record(ws_id);
    let entry = WorkspaceRecordEntry::new(owner_id, Some("session-1".into()), None, record);
    store.upsert_workspace_record(entry).await.unwrap();

    let debt_reasons = [
        ("mount-resources-left", CleanupReason::Failed),
        ("clone-orphaned-refs", CleanupReason::Failed),
        ("health-check-artifacts", CleanupReason::Cancelled),
    ];

    let mut debt_ids = Vec::new();
    for (reason, cleanup_reason) in &debt_reasons {
        let debt = WorkspaceCleanupDebtEntry::new(
            owner_id,
            Some("session-1".into()),
            None,
            test_workspace_record(ws_id),
            *cleanup_reason,
            *reason,
        );
        let debt_id = debt.debt_id.clone();
        store.record_cleanup_debt(debt).await.unwrap();
        debt_ids.push(debt_id);
    }

    let all_debts = store.list_cleanup_debts(owner_id, 100).await.unwrap();
    assert_eq!(all_debts.len(), 3);

    for debt_id in &debt_ids {
        let resolved = store.resolve_cleanup_debt(owner_id, debt_id).await;
        assert!(
            resolved.is_ok(),
            "each debt should be individually resolvable"
        );
        assert!(resolved.unwrap(), "debt should be found and removed");
    }

    let remaining = store.list_cleanup_debts(owner_id, 100).await.unwrap();
    assert!(
        remaining.is_empty(),
        "all debts should be resolved, got {} remaining",
        remaining.len()
    );
}

#[tokio::test]
async fn cleanup_debt_store_validation_rejects_invalid_input() {
    let store = InMemoryWorkspaceRecordStore::new();
    let owner_id = "owner-3";

    let result = store.list_cleanup_debts(owner_id, 100).await;
    assert!(result.is_ok());
    assert!(result.unwrap().is_empty());

    let bad_record = WorkspaceRecord {
        workspace_id: String::new(),
        owner_scope: WorkspaceOwnerScope::User,
        kind: WorkspaceBindingKind::None,
        authority: WorkspaceAuthority::None,
        root_or_volume_ref: String::new(),
        source: WorkspaceSource::None,
        persistence: WorkspacePersistence::None,
        revision: String::new(),
        display_name: String::new(),
    };
    let bad_debt = WorkspaceCleanupDebtEntry::new(
        owner_id,
        None,
        None,
        bad_record,
        CleanupReason::Failed,
        "bad-debt",
    );
    let result = store.record_cleanup_debt(bad_debt).await;
    assert!(result.is_err(), "empty workspace_id should be rejected");
}

#[tokio::test]
async fn workspace_source_cannot_be_claimed_by_two_owners() {
    let store = InMemoryWorkspaceRecordStore::new();
    let owner_a = "owner-a";
    let owner_b = "owner-b";
    let snapshot_id = "snap-123";

    let record_a = WorkspaceRecord {
        workspace_id: "ws-a".into(),
        owner_scope: WorkspaceOwnerScope::User,
        kind: WorkspaceBindingKind::ServerSandbox,
        authority: WorkspaceAuthority::ReadWrite,
        root_or_volume_ref: "/tmp/ws-a".into(),
        source: WorkspaceSource::UploadedSnapshot {
            artifact_id: snapshot_id.into(),
        },
        persistence: WorkspacePersistence::Session,
        revision: "v0".into(),
        display_name: "ws-a".into(),
    };
    let entry_a = WorkspaceRecordEntry::new(owner_a, Some("session-a".into()), None, record_a);
    store
        .upsert_workspace_record(entry_a)
        .await
        .expect("owner A should claim snapshot");

    let record_b = WorkspaceRecord {
        workspace_id: "ws-b".into(),
        owner_scope: WorkspaceOwnerScope::User,
        kind: WorkspaceBindingKind::ServerSandbox,
        authority: WorkspaceAuthority::ReadWrite,
        root_or_volume_ref: "/tmp/ws-b".into(),
        source: WorkspaceSource::UploadedSnapshot {
            artifact_id: snapshot_id.into(),
        },
        persistence: WorkspacePersistence::Session,
        revision: "v0".into(),
        display_name: "ws-b".into(),
    };
    let entry_b = WorkspaceRecordEntry::new(owner_b, Some("session-b".into()), None, record_b);
    let result = store.upsert_workspace_record(entry_b).await;
    assert!(result.is_err(), "cross-owner source claim must be rejected");
}
