use super::*;

pub(super) fn attach_chat_turn_bridge(
    state: AppState,
    settings: &AppSettings,
    shared_pool: &SharedPool,
    bridge_encryptor: &Arc<FernetTokenEncryptor>,
    matrix_rt: &Arc<crate::matrix_cloud_runtime::MatrixCloudRuntime>,
) -> AppState {
    let edge_callback_ledger = state.edge_callback_ledger.clone();
    state
        .with_chat_turn_bridge(Arc::new(
            turn::bridge_inprocess::InProcessChatTurnBridge::new(
                settings.matrixone.clone(),
                Arc::clone(bridge_encryptor),
            )
            .with_pool(shared_pool.clone())
            .with_edge_callback_ledger(edge_callback_ledger)
            .with_persist_tracker(
                Arc::clone(matrix_rt) as Arc<dyn crate::matrix_cloud_runtime::BridgePersistTracker>
            ),
        ))
        .with_chat_turn_bridge_secret(settings.bridge_secret.clone())
}

pub(super) fn spawn_runtime_sweepers(shared_pool: SharedPool) {
    super::super::device_lease_sweeper::spawn_device_lease_expiry_sweeper(shared_pool.clone());
    super::super::artifact_retention_sweeper::spawn_artifact_retention_sweeper(shared_pool.clone());
    // U-16/U-17/U-18: lifecycle sweepers for `session_todos`.
    // Stale `in_progress` rows auto-pause after 24h; stale
    // `completed` rows auto-archive weekly; long-retained
    // `archived` rows GC after 90 days. All three are idle when
    // the table has no qualifying rows, so cost stays near-zero.
    super::super::session_todo_sweeper::spawn_session_todo_stale_sweeper(shared_pool.clone());
    super::super::session_todo_sweeper::spawn_session_todo_archive_sweeper(shared_pool);
}
