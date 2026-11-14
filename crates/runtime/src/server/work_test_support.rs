use astra_core::SharedPool;

/// Delete one test owner's complete canonical Work aggregate in dependency order.
/// Call after owned execution writers have stopped. Slot-first locking matches
/// the background projector; one transaction prevents partially deleted Work
/// from becoming visible to that projector.
pub(crate) async fn cleanup_work_owner(pool: &SharedPool, owner_id: &str) {
    let mut transaction = pool
        .get()
        .begin()
        .await
        .expect("begin Work fixture cleanup");
    for (table, owner_column) in [
        ("work_runtime_event_outbox_slots", "owner_id"),
        ("work_runtime_event_outbox", "owner_id"),
        ("work_recovery_points", "owner_id"),
        ("session_artifact_references", "user_id"),
        ("session_artifacts", "user_id"),
        ("workspace_records", "owner_id"),
        ("work_item_attempts", "owner_id"),
        ("work_establishment_operations", "owner_id"),
        ("work_events", "owner_id"),
        ("work_attention_receipts", "owner_id"),
        ("work_event_sequences", "owner_id"),
        ("work_current_gap_acceptances", "owner_id"),
        ("work_acceptance_decisions", "owner_id"),
        ("work_check_runs", "owner_id"),
        ("work_proposals", "owner_id"),
        ("work_proposal_sequences", "owner_id"),
        ("work_branch_subjects", "owner_id"),
        ("work_branches", "owner_id"),
        ("work_item_revisions", "owner_id"),
        ("work_items", "owner_id"),
        ("work_graph_revisions", "owner_id"),
        ("work_graph_sequences", "owner_id"),
        ("work_criterion_sets", "owner_id"),
        ("work_criterion_revisions", "owner_id"),
        ("work_goal_revisions", "owner_id"),
        ("works", "owner_id"),
        ("agent_sessions", "user_id"),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE {owner_column} = ?"
        )))
        .bind(owner_id)
        .execute(&mut *transaction)
        .await
        .unwrap_or_else(|error| panic!("clean {table}: {error}"));
    }
    transaction
        .commit()
        .await
        .expect("commit Work fixture cleanup");
}

use astra_services::work::{InternalSessionId, WorkOwnerId};

pub(crate) async fn admit_running_test_session(
    pool: &SharedPool,
    owner: &WorkOwnerId,
    session: &InternalSessionId,
    run_id: &str,
    title: &str,
) {
    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, ?, 'active', 0)",
    )
    .bind(session.as_str())
    .bind(owner.as_str())
    .bind(title)
    .execute(pool.get())
    .await
    .expect("canonical session admission fence");
    sqlx::query(
        "INSERT INTO agent_runs
         (run_id, user_id, session_id, root_run_id, ancestor_path, status,
          owner_pod_id, owner_lease_expires_at, run_generation)
         VALUES (?, ?, ?, ?, ?, 'running',
                 'work-lifecycle-test-owner', TIMESTAMPADD(MINUTE, 10, NOW(6)), 0)",
    )
    .bind(run_id)
    .bind(owner.as_str())
    .bind(session.as_str())
    .bind(run_id)
    .bind(run_id)
    .execute(pool.get())
    .await
    .expect("active run authority");
}
