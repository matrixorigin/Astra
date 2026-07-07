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

pub(crate) fn workspace_lock_path_for(session_id: &str) -> std::path::PathBuf {
    astra_services::session_workspace::workspace_dir_for(session_id).join(".workspace.lock")
}

pub(crate) struct RecoveryCheckpointRollback {
    pub(crate) step_number: u32,
    pub(crate) composite_index_backup: Option<Vec<u8>>,
}

/// RAII guard that unlocks an exclusive file lock on drop.
///
/// On explicit `.unlock()` the guard is consumed and the unlock error is
/// reported to the caller. On `Drop` (panic unwind) a best-effort unlock
/// is performed and any error is silently ignored — there is no safe way
/// to surface it during unwinding.
pub(crate) struct WorkspaceLockGuard {
    file: Option<std::fs::File>,
    path: std::path::PathBuf,
}

impl WorkspaceLockGuard {
    fn new(file: std::fs::File, path: std::path::PathBuf) -> Self {
        Self {
            file: Some(file),
            path,
        }
    }

    /// Consume the guard and unlock, returning the result.
    fn unlock(mut self) -> Result<(), String> {
        let file = self.file.take().expect("guard already consumed");
        fs2::FileExt::unlock(&file)
            .map_err(|e| format!("unlock workspace {}: {e}", self.path.display()))
    }
}

impl Drop for WorkspaceLockGuard {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            // Best-effort unlock during unwind — ignore errors.
            let _ = fs2::FileExt::unlock(&file);
        }
    }
}

pub(crate) fn with_workspace_lock<T>(
    session_id: &str,
    op: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    use fs2::FileExt;

    let lock_path = workspace_lock_path_for(session_id);
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create workspace lock directory: {e}"))?;
    }
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| format!("open workspace lock {}: {e}", lock_path.display()))?;
    lock_file
        .lock_exclusive()
        .map_err(|e| format!("lock workspace {}: {e}", lock_path.display()))?;

    let guard = WorkspaceLockGuard::new(lock_file, lock_path.clone());

    let result = op();
    let unlock_result = guard.unlock();

    match (result, unlock_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(value), Err(error)) => {
            astra_core::agent_warn!("workspace", "{error}");
            Ok(value)
        }
        (Err(error), Err(unlock_error)) => Err(format!("{error}; {unlock_error}")),
    }
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
