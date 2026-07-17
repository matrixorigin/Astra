//! I/O primitives for session recovery: atomic writes, file locks, path helpers.
use astra_services::session_journal;

pub(crate) fn csl_log_path_for(session_id: &str) -> std::path::PathBuf {
    let store = astra_services::local_session_artifact_store();
    astra_services::SessionArtifactStore::session_path(&store, session_id, "conversation_log.jsonl")
        .expect("session id must resolve owner-bound conversation log path")
}

pub(crate) fn csl_store_base_dir() -> std::path::PathBuf {
    session_journal::local_owner_sessions_dir()
}
pub(crate) fn workspace_path_for(session_id: &str) -> std::path::PathBuf {
    astra_services::session_workspace::workspace_dir_for(session_id).join("workspace.yaml")
}

pub(crate) fn composite_index_path_for(
    user_id: &str,
    session_id: &str,
) -> Result<std::path::PathBuf, String> {
    astra_pipeline::step_checkpoint::owner_session_dir_for(user_id, session_id)
        .map(|dir| {
            dir.join("step_checkpoints")
                .join("composite_snapshots.json")
        })
        .map_err(|error| format!("owner-bound composite snapshot index path: {error}"))
}

pub(crate) struct RecoveryCheckpointRollback {
    pub(crate) step_number: u32,
    pub(crate) composite_index_backup: Option<Vec<u8>>,
}

pub(crate) fn append_rollback_error(
    message: &mut String,
    label: &str,
    rollback_result: Result<(), String>,
) {
    if let Err(error) = rollback_result {
        message.push_str(&format!("; {label} rollback failed: {error}"));
    }
}

pub(crate) fn sync_parent_dir(path: &std::path::Path) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let dir = std::fs::File::open(parent)
        .map_err(|e| format!("open parent directory {}: {e}", parent.display()))?;
    dir.sync_all()
        .map_err(|e| format!("sync parent directory {}: {e}", parent.display()))
}

pub(crate) fn write_bytes_atomic(
    path: &std::path::Path,
    bytes: &[u8],
    label: &str,
) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{label}: path {} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("{label}: create parent directory {}: {e}", parent.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("{label}: invalid file name {}", path.display()))?;
    let tmp_path = parent.join(format!(".tmp-{file_name}"));
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp_path)
            .map_err(|e| format!("{label}: create temporary file {}: {e}", tmp_path.display()))?;
        file.write_all(bytes)
            .map_err(|e| format!("{label}: write temporary file {}: {e}", tmp_path.display()))?;
        file.sync_all()
            .map_err(|e| format!("{label}: sync temporary file {}: {e}", tmp_path.display()))?;
    }
    std::fs::rename(&tmp_path, path)
        .map_err(|e| format!("{label}: replace {}: {e}", path.display()))?;
    sync_parent_dir(path)
}

pub(crate) fn read_optional_file_bytes(path: &std::path::Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read {}: {error}", path.display())),
    }
}

pub(crate) fn restore_optional_file_bytes(
    path: &std::path::Path,
    bytes: Option<Vec<u8>>,
) -> Result<(), String> {
    match bytes {
        Some(bytes) => write_bytes_atomic(path, &bytes, "restore file bytes"),
        None => match std::fs::remove_file(path) {
            Ok(()) => sync_parent_dir(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("remove restored {}: {error}", path.display())),
        },
    }
}
