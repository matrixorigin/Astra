mod common;

use astra_runtime_env::{
    WorkspaceAuthority, WorkspaceBindingKind, WorkspaceOwnerScope, WorkspacePersistence,
    WorkspaceRecord, WorkspaceSource,
};
use astra_services::workspace_records::{
    DatabaseWorkspaceRecordStore, WorkspaceCleanupDebtStore, WorkspaceCleanupDebtStoreError,
    WorkspaceRecordStore, WorkspaceRecordStoreError,
};
use uuid::Uuid;

fn workspace_record(workspace_id: &str) -> WorkspaceRecord {
    WorkspaceRecord {
        workspace_id: workspace_id.to_string(),
        owner_scope: WorkspaceOwnerScope::Tenant,
        kind: WorkspaceBindingKind::CloudWorkspace,
        authority: WorkspaceAuthority::ReadWrite,
        root_or_volume_ref: format!("/tmp/{workspace_id}"),
        source: WorkspaceSource::Scratch,
        persistence: WorkspacePersistence::Session,
        revision: "rev-1".to_string(),
        display_name: "Corrupt row probe".to_string(),
    }
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn database_workspace_records_reject_corrupt_rows() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let store = DatabaseWorkspaceRecordStore::new(shared.clone());

    let owner_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let workspace_id = format!("workspace-{}", Uuid::new_v4());
    let debt_workspace_id = format!("workspace-{}", Uuid::new_v4());
    let debt_id = format!("debt-{}", Uuid::new_v4());
    let valid_record_json =
        serde_json::to_string(&workspace_record(&debt_workspace_id)).expect("record json");

    let _ = sqlx::query("DELETE FROM workspace_records WHERE workspace_id IN (?, ?)")
        .bind(&workspace_id)
        .bind(&debt_workspace_id)
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM workspace_cleanup_debts WHERE debt_id = ?")
        .bind(&debt_id)
        .execute(&pool)
        .await;

    sqlx::query(
        "INSERT INTO workspace_records \
         (workspace_id, owner_id, session_id, run_id, kind, authority, persistence, \
          root_or_volume_ref, source_json, revision, display_name, source_key, record_json) \
         VALUES (?, ?, ?, NULL, 'cloud_workspace', 'read_write', 'session', ?, '{}', 'rev-1', \
                 'corrupt workspace', NULL, 'not-json')",
    )
    .bind(&workspace_id)
    .bind(&owner_id)
    .bind(&session_id)
    .bind(format!("/tmp/{workspace_id}"))
    .execute(&pool)
    .await
    .expect("insert corrupt workspace record");

    let err = store
        .load_workspace_record(&owner_id, &workspace_id)
        .await
        .expect_err("corrupt workspace record_json must fail loudly");
    assert!(
        matches!(err, WorkspaceRecordStoreError::Json(_)),
        "unexpected workspace record error: {err:?}"
    );

    sqlx::query(
        "INSERT INTO workspace_cleanup_debts \
         (debt_id, owner_id, session_id, run_id, workspace_id, reason, message, attempts, record_json) \
         VALUES (?, ?, ?, NULL, ?, 'failed', 'negative attempts fixture', -1, ?)",
    )
    .bind(&debt_id)
    .bind(&owner_id)
    .bind(&session_id)
    .bind(&debt_workspace_id)
    .bind(&valid_record_json)
    .execute(&pool)
    .await
    .expect("insert corrupt workspace cleanup debt");

    let err = store
        .list_cleanup_debts(&owner_id, 10)
        .await
        .expect_err("negative cleanup debt attempts must fail loudly");
    assert!(
        matches!(err, WorkspaceCleanupDebtStoreError::Database(_)),
        "unexpected cleanup debt error: {err:?}"
    );
    assert!(
        err.to_string().contains("attempts"),
        "cleanup debt error should identify attempts corruption: {err}"
    );

    let _ = sqlx::query("DELETE FROM workspace_cleanup_debts WHERE debt_id = ?")
        .bind(&debt_id)
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM workspace_records WHERE workspace_id = ?")
        .bind(&workspace_id)
        .execute(&pool)
        .await;
}
