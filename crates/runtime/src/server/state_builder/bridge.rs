use super::*;
use std::sync::Arc;

pub(super) fn attach_chat_turn_bridge(
    state: AppState,
    settings: &AppSettings,
    shared_pool: &SharedPool,
    bridge_encryptor: &Arc<FernetTokenEncryptor>,
    matrix_rt: &Arc<crate::matrix_cloud_runtime::MatrixCloudRuntime>,
) -> AppState {
    let edge_callback_ledger = state.edge_callback_ledger.clone();
    let session_context_coordinator = state
        .session_context_coordinator
        .clone()
        .expect("core state must install the canonical session coordinator before the bridge");
    state
        .with_chat_turn_bridge(Arc::new(
            turn::bridge::inprocess::InProcessChatTurnBridge::new(
                settings.matrixone.clone(),
                Arc::clone(bridge_encryptor),
            )
            .with_pool(shared_pool.clone())
            .with_session_context_coordinator(session_context_coordinator)
            .with_edge_callback_ledger(edge_callback_ledger)
            .with_persist_tracker(
                Arc::clone(matrix_rt) as Arc<dyn crate::matrix_cloud_runtime::BridgePersistTracker>
            ),
        ))
        .with_chat_turn_bridge_secret(settings.bridge_secret.clone())
}
