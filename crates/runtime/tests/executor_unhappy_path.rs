//! Workspace cleanup-debt and cross-owner source-claim contract tests.
use astra_runtime_env::{
    CleanupReason, WorkspaceAuthority, WorkspaceBindingKind, WorkspaceOwnerScope,
    WorkspacePersistence, WorkspaceRecord, WorkspaceSource,
};
use astra_services::{
    InMemoryWorkspaceRecordStore, WorkspaceCleanupDebtEntry, WorkspaceCleanupDebtStore,
    WorkspaceRecordEntry, WorkspaceRecordStore,
};

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
    let owner_id = "00000000-0000-0000-0000-000000000001";
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
    let owner_id = "00000000-0000-0000-0000-000000000002";
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
    let owner_a = "00000000-0000-0000-0000-00000000000a";
    let owner_b = "00000000-0000-0000-0000-00000000000b";
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
