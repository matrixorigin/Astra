//! CSL (Conversation State Log) operations: load, rebuild, snapshot.
use super::io::{
    csl_log_path_for, read_optional_file_bytes, restore_optional_file_bytes, sync_parent_dir,
    write_bytes_atomic,
};
use crate::cli::session::session_state::SessionState;
use astra_services::session_journal;

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
        return Err(restore_csl_snapshot_after_failure(
            &csl_path, csl_backup, error,
        ));
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
        restore_csl_snapshot_after_failure(
            &csl_path,
            csl_backup.clone(),
            format!("reinitialize CSL manager: {e}"),
        )
    })?;
    if !messages.is_empty() || state.turn > 0 {
        mgr.load().await.map_err(|e| {
            restore_csl_snapshot_after_failure(
                &csl_path,
                csl_backup.clone(),
                format!("reload rewritten CSL state: {e}"),
            )
        })?;
    }
    state.csl_manager = Some(mgr);
    Ok(())
}

pub(super) fn restore_csl_snapshot_after_failure(
    path: &std::path::Path,
    backup: Option<Vec<u8>>,
    mut error_message: String,
) -> String {
    match restore_optional_file_bytes(path, backup) {
        Ok(()) => error_message.push_str("; rolled back CSL snapshot"),
        Err(error) => error_message.push_str(&format!("; CSL snapshot rollback failed: {error}")),
    }
    error_message
}

/// Read the highest `seq` present in the on-disk CSL log, if any.
///
/// Used to compute the seq for a recovery-time full snapshot so the new
/// snapshot strictly dominates anything previously persisted. Lines that fail
/// to parse (corruption, partial writes) are skipped — they cannot constrain
/// the new high-water mark anyway since they are unreadable.
fn read_max_seq_from_log(path: &std::path::Path) -> u64 {
    let Ok(file) = std::fs::File::open(path) else {
        return 0;
    };
    use std::io::BufRead;
    let mut max_seq = 0u64;
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) =
            serde_json::from_str::<astra_turn_core::conversation_log::CslEntry>(&line)
        {
            max_seq = max_seq.max(entry.seq());
        }
    }
    max_seq
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

    // The recovery snapshot replaces the file in its entirety, but the new
    // snapshot's seq must still strictly dominate anything previously written
    // so any out-of-band reader that observed older seqs (e.g. a cached
    // `last_seq` in a still-running CSL manager) does not regress and reject
    // subsequent appends as out-of-order.
    let next_seq = read_max_seq_from_log(&path).saturating_add(1);

    let snapshot = astra_turn_core::conversation_log::CslEntry::Snapshot {
        seq: next_seq,
        turn,
        messages: messages.to_vec(),
        session_state: session_state.clone(),
    };
    let mut encoded =
        serde_json::to_string(&snapshot).map_err(|e| format!("serialize CSL snapshot: {e}"))?;
    encoded.push('\n');
    write_bytes_atomic(&path, encoded.as_bytes(), "replace CSL snapshot")
}
