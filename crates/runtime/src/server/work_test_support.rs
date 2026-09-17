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
        ("work_item_edges", "owner_id"),
        ("work_item_revisions", "owner_id"),
        ("work_items", "owner_id"),
        ("work_graph_revisions", "owner_id"),
        ("work_graph_sequences", "owner_id"),
        ("work_criterion_sets", "owner_id"),
        ("work_criterion_revisions", "owner_id"),
        ("work_criteria", "owner_id"),
        ("work_goal_revisions", "owner_id"),
        ("works", "owner_id"),
        ("agent_sessions", "user_id"),
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE {owner_column} = ?"))
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
