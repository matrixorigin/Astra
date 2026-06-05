//! CSL (Conversation State Log) operations: load, rebuild, snapshot.
use super::io::*;
use crate::cli::*;

pub(crate) async fn ensure_loaded_csl_state(
    state: &mut SessionState,
    sid: &str,
) -> Result<Option<astra_turn_core::conversation_log::SessionStateCompact>, String> {
    let needs_new_manager = state
        .csl_manager
        .as_ref()
        .is_none_or(|mgr| mgr.session_id() != sid);
    if needs_new_manager {
        let store = std::sync::Arc::new(
            astra_turn_core::conversation_log::file_store::FileCslStore::new(
                session_journal::local_sessions_dir(),
            ),
        );
        state.csl_manager = match astra_turn_core::conversation_log::manager::CslManager::new(
            store,
            sid.to_string(),
            Default::default(),
        ) {
            Ok(mgr) => Some(mgr),
            Err(e) => {
                return Err(format!("initialize CSL state for recovery sync: {e}"));
            }
        };
    }

    let Some(mgr) = state.csl_manager.as_mut() else {
        return Ok(None);
    };

    if mgr.last_seq() > 0 {
        return Ok(Some(mgr.last_session_state().clone()));
    }

    match mgr.load().await {
        Ok(Some(mat)) => Ok(Some(mat.session_state)),
        Ok(None) => Ok(None),
        Err(e) => Err(format!("load CSL state for recovery sync: {e}")),
    }
}
pub(crate) async fn rebuild_csl_from_history(
    state: &mut SessionState,
    sid: &str,
    messages: &[serde_json::Value],
    session_state: &astra_turn_core::conversation_log::SessionStateCompact,
) -> Result<(), String> {
    if state.csl_manager.is_none() {
        return Ok(());
    }

    let csl_path = csl_log_path_for(sid);
    let csl_backup = read_optional_file_bytes(&csl_path)?;
    if let Err(error) = write_full_csl_snapshot_atomic(sid, state.turn, messages, session_state) {
        let mut error_message = error;
        append_rollback_error(
            &mut error_message,
            "CSL snapshot",
            restore_optional_file_bytes(&csl_path, csl_backup),
        );
        return Err(error_message);
    }

    let store = std::sync::Arc::new(
        astra_turn_core::conversation_log::file_store::FileCslStore::new(
            session_journal::local_sessions_dir(),
        ),
    );
    let mut mgr = astra_turn_core::conversation_log::manager::CslManager::new(
        store,
        sid.to_string(),
        Default::default(),
    )
    .map_err(|e| {
        let mut error_message = format!("reinitialize CSL manager: {e}");
        append_rollback_error(
            &mut error_message,
            "CSL snapshot",
            restore_optional_file_bytes(&csl_path, csl_backup.clone()),
        );
        error_message
    })?;
    if !messages.is_empty() || state.turn > 0 {
        mgr.load().await.map_err(|e| {
            let mut error_message = format!("reload rewritten CSL state: {e}");
            append_rollback_error(
                &mut error_message,
                "CSL snapshot",
                restore_optional_file_bytes(&csl_path, csl_backup.clone()),
            );
            error_message
        })?;
    }
    state.csl_manager = Some(mgr);
    Ok(())
}

pub(crate) fn csl_log_path_for(session_id: &str) -> std::path::PathBuf {
    session_journal::local_sessions_dir()
        .join(session_id)
        .join("conversation_log.jsonl")
}
pub(crate) fn write_full_csl_snapshot_atomic(
    sid: &str,
    turn: u32,
    messages: &[serde_json::Value],
    session_state: &astra_turn_core::conversation_log::SessionStateCompact,
) -> Result<(), String> {
    let path = csl_log_path_for(sid);
    if messages.is_empty() && turn == 0 {
        match std::fs::remove_file(&path) {
            Ok(()) => return sync_parent_dir(&path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("remove stale CSL snapshot: {error}")),
        }
    }

    let snapshot = astra_turn_core::conversation_log::CslEntry::Snapshot {
        seq: 1,
        turn,
        messages: messages.to_vec(),
        session_state: session_state.clone(),
    };
    let mut encoded =
        serde_json::to_string(&snapshot).map_err(|e| format!("serialize CSL snapshot: {e}"))?;
    encoded.push('\n');
    write_bytes_atomic(&path, encoded.as_bytes(), "replace CSL snapshot")
}
