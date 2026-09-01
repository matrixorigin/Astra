//! Step checkpoint persistence — write/read StepCheckpoint JSON to local filesystem.
//!
//! Stores checkpoints at:
//! `~/.astra/sessions/v1/users/b64-<url-safe-user-id>/sessions/<session_id>/step_checkpoints/<number>-<tier>.json`
//!
//! Also provides a file-backed StepEventStore that writes events as JSONL:
//! `~/.astra/sessions/v1/users/b64-<url-safe-user-id>/sessions/<session_id>/step_events.jsonl`
//!
//! Light checkpoints (~1KB) written after each tool completion.
//! Heavy checkpoints (~10-100KB) written after each turn's verdict.
//! On crash recovery, the latest heavy checkpoint restores full session state.

use std::path::{Path, PathBuf};

use astra_services::{OwnerScope, SessionArtifactStore};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::step_protocol::{
    CheckpointTier, HeavyCheckpoint, LightCheckpoint, StepCheckpoint, StepEvent, StepEventStore,
};

/// Directory name within session workspace for step checkpoints.
const STEP_CHECKPOINT_DIR: &str = "step_checkpoints";
pub const STEP_LOCAL_LAYOUT_VERSION: &str = astra_services::LOCAL_SESSION_LAYOUT_VERSION;
pub const STEP_ARTIFACT_SCHEMA_VERSION: u32 = 2;
const STEP_CHECKPOINT_ARTIFACT_KIND: &str = "step_checkpoint";
const STEP_EVENT_ARTIFACT_KIND: &str = "step_event";
const STEP_BREAKPOINT_INDEX_ARTIFACT_KIND: &str = "step_breakpoint_index";
const STEP_COMPOSITE_INDEX_ARTIFACT_KIND: &str = "step_composite_snapshot_index";

fn observed_text_bytes(content: &str) -> u64 {
    content.len().try_into().unwrap_or(u64::MAX)
}

fn record_observed_text(site: astra_core::history_work::HistoryWorkSite, content: &str) {
    if astra_core::history_work::instrumentation_enabled() {
        astra_core::history_work::record_bytes(site, observed_text_bytes(content));
    }
}

fn record_event_journal_read(bytes: usize, rows: usize) {
    if astra_core::history_work::instrumentation_enabled() {
        astra_core::history_work::record_operation(
            astra_core::history_work::HistoryWorkSite::PipelineEventJournalRead,
            bytes.try_into().unwrap_or(u64::MAX),
            rows.try_into().unwrap_or(u64::MAX),
            0,
        );
    }
}

/// Recovery reads a recent tail rather than allowing one long-lived session
/// to force an unbounded journal scan during resume.
pub const STEP_EVENT_RECOVERY_MAX_BYTES: usize = 8 * 1024 * 1024;
pub const STEP_EVENT_RECOVERY_MAX_EVENTS: usize = 4_096;

/// Maximum number of light checkpoints to retain (older ones pruned).
const MAX_LIGHT_CHECKPOINTS: usize = 50;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WriteDurability {
    /// Write and close the file immediately, but let the OS flush dirty pages.
    ///
    /// Used for per-event and per-tool light artifacts on the agent hot path.
    /// Readers can replay the data after this process exits or crashes, but
    /// this deliberately does not pay the multi-second `fsync` cost that some
    /// filesystems impose under load.
    Buffered,
    /// Force file contents and directory metadata to stable storage before returning.
    ///
    /// Reserved for low-frequency recovery anchors such as heavy checkpoints
    /// and indexes where extra latency is acceptable.
    Durable,
}

fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::File::open(parent)?.sync_all()
}

fn sync_dir(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

fn write_atomic_text(
    path: &Path,
    content: &str,
    durability: WriteDurability,
) -> std::io::Result<()> {
    let Some(dir) = path.parent() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "artifact path must have a parent directory",
        ));
    };
    std::fs::create_dir_all(dir)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid file name")
        })?;
    let tmp_path = dir.join(format!(
        ".tmp-{}-{}-{}",
        std::process::id(),
        file_name,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(content.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        if matches!(durability, WriteDurability::Durable) {
            file.sync_all()?;
        }
    }
    std::fs::rename(&tmp_path, path)?;
    if matches!(durability, WriteDurability::Durable) {
        sync_parent_dir(path)?;
    }
    Ok(())
}

fn append_jsonl_line(
    path: &Path,
    content: &str,
    durability: WriteDurability,
) -> std::io::Result<()> {
    let Some(dir) = path.parent() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "JSONL artifact path must have a parent directory",
        ));
    };
    std::fs::create_dir_all(dir)?;
    use fs2::FileExt;
    use std::io::Write;
    let create_attempt = {
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).read(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path)
    };
    let (mut file, created) = match create_attempt {
        Ok(file) => (file, true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (
            std::fs::OpenOptions::new()
                .read(true)
                .append(true)
                .open(path)?,
            false,
        ),
        Err(error) => return Err(error),
    };
    file.lock_exclusive()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }

    // Inspect and append while holding the same cross-process lock. Without
    // this, two writers can both observe a torn/no-newline tail and interleave
    // records, corrupting the recovery authority they are trying to publish.
    let needs_leading_newline = file_needs_trailing_newline_from(&mut file)?;
    if needs_leading_newline {
        file.write_all(b"\n")?;
    }
    writeln!(file, "{content}")?;
    if matches!(durability, WriteDurability::Durable) {
        file.sync_data()?;
    }
    if created && matches!(durability, WriteDurability::Durable) {
        sync_parent_dir(path)?;
    }
    FileExt::unlock(&file)?;
    Ok(())
}

fn file_needs_trailing_newline_from(file: &mut std::fs::File) -> std::io::Result<bool> {
    let metadata = file.metadata()?;
    if metadata.len() == 0 {
        return Ok(false);
    }
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0_u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] != b'\n')
}

pub fn owner_session_dir_for(user_id: &str, session_id: &str) -> std::io::Result<PathBuf> {
    let owner_scope = OwnerScope::user(user_id).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid owner for owner-bound step artifact: {e}"),
        )
    })?;
    astra_services::local_session_artifact_store()
        .session_dir_for_owner(&owner_scope, session_id)
        .map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid session_id for owner-bound step artifact: {e}"),
            )
        })
}

fn checkpoint_dir_for(user_id: &str, session_id: &str) -> std::io::Result<PathBuf> {
    Ok(owner_session_dir_for(user_id, session_id)?.join(STEP_CHECKPOINT_DIR))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VersionedStepArtifact<T> {
    schema_version: u32,
    layout_version: String,
    artifact_kind: String,
    user_id: String,
    session_id: String,
    payload: T,
}

fn encode_versioned_step_artifact<T: Serialize>(
    artifact_kind: &str,
    user_id: &str,
    session_id: &str,
    payload: &T,
) -> std::io::Result<String> {
    let envelope = VersionedStepArtifact {
        schema_version: STEP_ARTIFACT_SCHEMA_VERSION,
        layout_version: STEP_LOCAL_LAYOUT_VERSION.to_string(),
        artifact_kind: artifact_kind.to_string(),
        user_id: user_id.to_string(),
        session_id: session_id.to_string(),
        payload,
    };
    serde_json::to_string(&envelope)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn decode_versioned_step_artifact<T: DeserializeOwned>(
    artifact_kind: &str,
    user_id: &str,
    session_id: &str,
    content: &str,
) -> std::io::Result<T> {
    let envelope: VersionedStepArtifact<T> = serde_json::from_str(content)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    validate_versioned_step_artifact(artifact_kind, user_id, session_id, envelope)
}

fn validate_versioned_step_artifact<T>(
    artifact_kind: &str,
    user_id: &str,
    session_id: &str,
    envelope: VersionedStepArtifact<T>,
) -> std::io::Result<T> {
    if envelope.schema_version != STEP_ARTIFACT_SCHEMA_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "step artifact schema_version mismatch: expected={} found={} artifact_kind={} user_id={} session_id={}",
                STEP_ARTIFACT_SCHEMA_VERSION,
                envelope.schema_version,
                envelope.artifact_kind,
                envelope.user_id,
                envelope.session_id
            ),
        ));
    }
    if envelope.layout_version != STEP_LOCAL_LAYOUT_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "step artifact layout_version mismatch: expected={} found={} artifact_kind={} user_id={} session_id={}",
                STEP_LOCAL_LAYOUT_VERSION,
                envelope.layout_version,
                envelope.artifact_kind,
                envelope.user_id,
                envelope.session_id
            ),
        ));
    }
    if envelope.artifact_kind != artifact_kind {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "step artifact kind mismatch: expected={} found={} user_id={} session_id={}",
                artifact_kind, envelope.artifact_kind, envelope.user_id, envelope.session_id
            ),
        ));
    }
    if envelope.user_id != user_id || envelope.session_id != session_id {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "step artifact owner mismatch: expected_user_id={} expected_session_id={} found_user_id={} found_session_id={}",
                user_id, session_id, envelope.user_id, envelope.session_id
            ),
        ));
    }
    Ok(envelope.payload)
}

fn read_checkpoint_entry(
    user_id: &str,
    session_id: &str,
    entry: &std::fs::DirEntry,
) -> std::io::Result<Option<StepCheckpoint>> {
    let content = match std::fs::read_to_string(entry.path()) {
        Ok(content) => content,
        Err(error) => {
            astra_core::agent_warn!(
                "checkpoint",
                "Skipping unreadable checkpoint {:?}: {}",
                entry.file_name(),
                error
            );
            return Ok(None);
        }
    };
    record_observed_text(
        astra_core::history_work::HistoryWorkSite::PipelineCheckpointRead,
        &content,
    );
    let envelope: VersionedStepArtifact<StepCheckpoint> = match serde_json::from_str(&content) {
        Ok(envelope) => envelope,
        Err(error) => {
            astra_core::agent_warn!(
                "checkpoint",
                "Skipping malformed checkpoint {:?}: {}",
                entry.file_name(),
                error
            );
            return Ok(None);
        }
    };
    validate_versioned_step_artifact(STEP_CHECKPOINT_ARTIFACT_KIND, user_id, session_id, envelope)
        .map(Some)
}

/// Returns whether a local heavy checkpoint artifact exists for this owner/session.
pub fn heavy_checkpoint_exists(user_id: &str, session_id: &str) -> std::io::Result<bool> {
    let dir = checkpoint_dir_for(user_id, session_id)?;
    if !dir.exists() {
        return Ok(false);
    }
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().ends_with("-heavy.json") {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Write a step checkpoint to local filesystem.
/// Returns the path where the checkpoint was written.
pub fn write_step_checkpoint(
    user_id: &str,
    session_id: &str,
    number: u32,
    checkpoint: &StepCheckpoint,
) -> std::io::Result<PathBuf> {
    let dir = checkpoint_dir_for(user_id, session_id)?;
    std::fs::create_dir_all(&dir)?;
    with_session_checkpoint_lock(&dir, || {
        write_step_checkpoint_unlocked(user_id, session_id, number, checkpoint, &dir)
    })
}

fn write_step_checkpoint_unlocked(
    user_id: &str,
    session_id: &str,
    number: u32,
    checkpoint: &StepCheckpoint,
    dir: &Path,
) -> std::io::Result<PathBuf> {
    let tier = match checkpoint {
        StepCheckpoint::Light(_) => "light",
        StepCheckpoint::Heavy(_) => "heavy",
    };
    let json = encode_versioned_step_artifact(
        STEP_CHECKPOINT_ARTIFACT_KIND,
        user_id,
        session_id,
        checkpoint,
    )?;
    record_observed_text(
        astra_core::history_work::HistoryWorkSite::PipelineCheckpointSerialization,
        &json,
    );

    let mut allocated_number = number;
    let path = loop {
        let candidate = dir.join(format!("{allocated_number:06}-{tier}.json"));
        if !candidate.exists() {
            break candidate;
        }
        if std::fs::read_to_string(&candidate).is_ok_and(|existing| existing == json) {
            return Ok(candidate);
        }
        allocated_number = list_checkpoints(user_id, session_id)?
            .into_iter()
            .map(|(number, _)| number)
            .max()
            .unwrap_or(allocated_number)
            .checked_add(1)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "session checkpoint sequence exhausted u32",
                )
            })?;
    };

    write_atomic_text(&path, &json, checkpoint_write_durability(checkpoint))?;

    match checkpoint {
        StepCheckpoint::Light(_) => prune_light_checkpoints(dir)?,
        StepCheckpoint::Heavy(_) => {
            // A heavy checkpoint embeds the complete light cursor and is
            // durably on disk at this point. Older light artifacts no longer
            // improve recovery and only amplify writes/listing work. Cleanup
            // is best-effort: failure must not invalidate the durable anchor.
            if let Err(error) = prune_light_checkpoints_superseded_by(dir, allocated_number) {
                astra_core::agent_warn!(
                    "checkpoint",
                    "Failed to prune light checkpoints superseded by heavy checkpoint {}: {}",
                    allocated_number,
                    error
                );
            }
        }
    }

    Ok(path)
}

fn with_session_checkpoint_lock<T>(
    dir: &Path,
    operation: impl FnOnce() -> std::io::Result<T>,
) -> std::io::Result<T> {
    use fs2::FileExt;
    std::fs::create_dir_all(dir)?;
    let lock_path = dir.join(".session-checkpoint.lock");
    let mut options = std::fs::OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(lock_path)?;
    lock.lock_exclusive()?;
    operation()
}

fn checkpoint_write_durability(checkpoint: &StepCheckpoint) -> WriteDurability {
    // Light checkpoints are written after each tool completion (~1KB, up to 500+
    // per session). They are immediately readable after a process crash, but do
    // not provide OS-crash durability; heavy checkpoints remain the durable
    // recovery anchor. Use Buffered to avoid per-tool fsync overhead (5-50ms
    // each on ext4).
    // Heavy checkpoints are written at major phase boundaries and must survive
    // OS crash — keep Durable.
    match checkpoint {
        StepCheckpoint::Light(_) => WriteDurability::Buffered,
        StepCheckpoint::Heavy(_) => WriteDurability::Durable,
    }
}

/// Delete a step checkpoint by number and tier.
pub fn delete_step_checkpoint(
    user_id: &str,
    session_id: &str,
    number: u32,
    tier: &str,
) -> std::io::Result<()> {
    let dir = checkpoint_dir_for(user_id, session_id)?;
    let filename = format!("{:06}-{}.json", number, tier);
    let path = dir.join(&filename);
    match std::fs::remove_file(&path) {
        Ok(()) => sync_parent_dir(&path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

/// Read the latest heavy checkpoint for session recovery.
/// Returns None if no heavy checkpoint exists.
pub fn read_latest_heavy_checkpoint(
    user_id: &str,
    session_id: &str,
) -> std::io::Result<Option<HeavyCheckpoint>> {
    let dir = checkpoint_dir_for(user_id, session_id)?;
    if !dir.exists() {
        return Ok(None);
    }

    let mut heavy_files: Vec<_> = std::fs::read_dir(&dir)?
        .filter_map(|e| match e {
            Ok(entry) => Some(entry),
            Err(err) => {
                astra_core::agent_warn!(
                    "checkpoint",
                    "Failed to read checkpoint dir entry: {}",
                    err
                );
                None
            }
        })
        .filter(|e| e.file_name().to_string_lossy().ends_with("-heavy.json"))
        .collect();

    // Sort by name descending (latest = highest number)
    heavy_files.sort_by_key(|b| std::cmp::Reverse(b.file_name()));

    for entry in &heavy_files {
        let Some(checkpoint) = read_checkpoint_entry(user_id, session_id, entry)? else {
            continue;
        };
        match checkpoint {
            StepCheckpoint::Heavy(boxed) => return Ok(Some(*boxed)),
            _ => continue,
        }
    }
    Ok(None)
}

/// Read the latest light checkpoint (for quick cursor restore).
pub fn read_latest_light_checkpoint(
    user_id: &str,
    session_id: &str,
) -> std::io::Result<Option<LightCheckpoint>> {
    let dir = checkpoint_dir_for(user_id, session_id)?;
    if !dir.exists() {
        return Ok(None);
    }

    // Any step checkpoint contains cursor info; index files in the same
    // directory are different artifact kinds and must not participate.
    let mut all_files: Vec<_> = std::fs::read_dir(&dir)?
        .filter_map(|e| match e {
            Ok(entry) => Some(entry),
            Err(err) => {
                astra_core::agent_warn!(
                    "checkpoint",
                    "Failed to read checkpoint dir entry: {}",
                    err
                );
                None
            }
        })
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.ends_with("-light.json") || name.ends_with("-heavy.json")
        })
        .collect();

    all_files.sort_by_key(|b| std::cmp::Reverse(b.file_name()));

    for entry in &all_files {
        let Some(checkpoint) = read_checkpoint_entry(user_id, session_id, entry)? else {
            continue;
        };
        match checkpoint {
            StepCheckpoint::Light(light) => return Ok(Some(light)),
            StepCheckpoint::Heavy(heavy) => return Ok(Some(heavy.light)),
        }
    }
    Ok(None)
}

/// List all checkpoint numbers and tiers for a session.
pub fn list_checkpoints(
    user_id: &str,
    session_id: &str,
) -> std::io::Result<Vec<(u32, CheckpointTier)>> {
    let dir = checkpoint_dir_for(user_id, session_id)?;
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut result = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                astra_core::agent_warn!(
                    "checkpoint",
                    "Failed to read dir entry during list: {}",
                    err
                );
                continue;
            }
        };
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(rest) = name.strip_suffix(".json")
            && let Some((num_str, tier_str)) = rest.split_once('-')
            && let Ok(num) = num_str.parse::<u32>()
        {
            let tier = match tier_str {
                "light" => CheckpointTier::Light,
                "heavy" => CheckpointTier::Heavy,
                _ => continue,
            };
            result.push((num, tier));
        }
    }
    result.sort_by_key(|(n, _)| *n);
    Ok(result)
}

/// Allocate the next checkpoint number in the session-owned namespace.
///
/// Recorder counters are run-scoped and restart for every run, while checkpoint
/// filenames are session-scoped. Timeline owners must allocate from persisted
/// session state so a later run cannot overwrite an earlier turn's checkpoint.
pub fn next_checkpoint_number(user_id: &str, session_id: &str) -> std::io::Result<u32> {
    list_checkpoints(user_id, session_id)?
        .into_iter()
        .map(|(number, _)| number)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "session checkpoint sequence exhausted u32",
            )
        })
}

/// Remove old light checkpoints, keeping only the most recent MAX_LIGHT_CHECKPOINTS.
fn prune_light_checkpoints(dir: &Path) -> std::io::Result<()> {
    let mut light_files: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| match e {
            Ok(entry) => Some(entry),
            Err(err) => {
                astra_core::agent_warn!(
                    "checkpoint",
                    "Failed to read dir entry during prune: {}",
                    err
                );
                None
            }
        })
        .filter(|e| e.file_name().to_string_lossy().ends_with("-light.json"))
        .collect();

    if light_files.len() <= MAX_LIGHT_CHECKPOINTS {
        return Ok(());
    }

    // Sort ascending by name, remove oldest
    light_files.sort_by_key(|a| a.file_name());
    let to_remove = light_files.len() - MAX_LIGHT_CHECKPOINTS;
    for entry in light_files.into_iter().take(to_remove) {
        if let Err(err) = std::fs::remove_file(entry.path()) {
            astra_core::agent_warn!(
                "checkpoint",
                "Failed to prune light checkpoint {:?}: {}",
                entry.file_name(),
                err
            );
        }
    }

    Ok(())
}

/// Remove light cursor artifacts already represented by a durable heavy anchor.
///
/// Checkpoint numbers are session-global and monotonically allocated. A light
/// artifact with a larger number may belong to later work and must be retained.
fn prune_light_checkpoints_superseded_by(dir: &Path, heavy_number: u32) -> std::io::Result<()> {
    let mut removed_any = false;
    for entry in std::fs::read_dir(dir)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                astra_core::agent_warn!(
                    "checkpoint",
                    "Failed to read dir entry during heavy checkpoint prune: {}",
                    error
                );
                continue;
            }
        };
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        let Some(number) = file_name
            .strip_suffix("-light.json")
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        if number > heavy_number {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => removed_any = true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                astra_core::agent_warn!(
                    "checkpoint",
                    "Failed to prune light checkpoint {:?}: {}",
                    entry.file_name(),
                    error
                );
            }
        }
    }
    if removed_any {
        sync_dir(dir)?;
    }
    Ok(())
}

/// Remove heavy recovery artifacts that no composite snapshot can address.
///
/// Post-tool policy may write a defensive heavy anchor before terminal
/// finalization. Once a newer composite index is durable, those unreferenced
/// anchors are neither rollback points nor the latest recovery authority and
/// retaining them makes long tool loops grow storage quadratically.
pub fn prune_unreferenced_heavy_checkpoints(
    user_id: &str,
    session_id: &str,
    index: &astra_core::composite_snapshot::CompositeSnapshotIndex,
) -> std::io::Result<usize> {
    let dir = checkpoint_dir_for(user_id, session_id)?;
    with_session_checkpoint_lock(&dir, || {
        prune_unreferenced_heavy_checkpoints_unlocked(&dir, index)
    })
}

fn prune_unreferenced_heavy_checkpoints_unlocked(
    dir: &Path,
    index: &astra_core::composite_snapshot::CompositeSnapshotIndex,
) -> std::io::Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }
    let referenced = index
        .snapshots
        .iter()
        .filter_map(|snapshot| snapshot.session_state())
        .collect::<std::collections::HashSet<_>>();
    let mut removed = 0usize;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if !file_name.ends_with("-heavy.json") || referenced.contains(file_name.as_ref()) {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => removed = removed.saturating_add(1),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    if removed > 0 {
        sync_dir(dir)?;
    }
    Ok(removed)
}
pub fn read_breakpoint_index(
    user_id: &str,
    session_id: &str,
) -> std::io::Result<crate::step_protocol::BreakpointIndex> {
    let path = checkpoint_dir_for(user_id, session_id)?.join("breakpoints.json");
    if !path.exists() {
        return Ok(crate::step_protocol::BreakpointIndex::default());
    }
    let content = std::fs::read_to_string(&path)?;
    decode_versioned_step_artifact(
        STEP_BREAKPOINT_INDEX_ARTIFACT_KIND,
        user_id,
        session_id,
        &content,
    )
}

// ─── Composite Snapshot I/O ──────────────────────────────────────────────────

/// Persist the composite snapshot index to disk (atomic write).
pub fn write_composite_snapshot_index(
    user_id: &str,
    session_id: &str,
    index: &astra_core::composite_snapshot::CompositeSnapshotIndex,
) -> std::io::Result<()> {
    let dir = checkpoint_dir_for(user_id, session_id)?;
    std::fs::create_dir_all(&dir)?;
    with_session_checkpoint_lock(&dir, || {
        write_composite_snapshot_index_unlocked(user_id, session_id, index, &dir)
    })
}

fn write_composite_snapshot_index_unlocked(
    user_id: &str,
    session_id: &str,
    index: &astra_core::composite_snapshot::CompositeSnapshotIndex,
    dir: &Path,
) -> std::io::Result<()> {
    let path = dir.join("composite_snapshots.json");
    let json = encode_versioned_step_artifact(
        STEP_COMPOSITE_INDEX_ARTIFACT_KIND,
        user_id,
        session_id,
        index,
    )?;
    record_observed_text(
        astra_core::history_work::HistoryWorkSite::PipelineCompositeIndexSerialization,
        &json,
    );
    write_atomic_text(&path, &json, WriteDurability::Durable)
}

/// Atomically allocate and publish a heavy checkpoint plus its composite
/// index entry within one cross-process session lock. The returned index is
/// the exact durable version written by this transaction.
pub fn commit_composite_checkpoint(
    user_id: &str,
    session_id: &str,
    checkpoint: &StepCheckpoint,
    mut snapshot: astra_core::composite_snapshot::CompositeSnapshot,
) -> std::io::Result<(
    u32,
    astra_core::composite_snapshot::CompositeSnapshot,
    astra_core::composite_snapshot::CompositeSnapshotIndex,
)> {
    if !matches!(checkpoint, StepCheckpoint::Heavy(_)) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "composite checkpoint publication requires a heavy checkpoint",
        ));
    }
    let dir = checkpoint_dir_for(user_id, session_id)?;
    with_session_checkpoint_lock(&dir, || {
        let number = list_checkpoints(user_id, session_id)?
            .into_iter()
            .map(|(number, _)| number)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "session checkpoint sequence exhausted u32",
                )
            })?;
        write_step_checkpoint_unlocked(user_id, session_id, number, checkpoint, &dir)?;
        snapshot.refs.retain(|reference| {
            !matches!(
                reference,
                astra_core::composite_snapshot::SnapshotRef::SessionState(_)
            )
        });
        snapshot
            .refs
            .push(astra_core::composite_snapshot::SnapshotRef::SessionState(
                format!("{number:06}-heavy.json"),
            ));
        let mut index = read_composite_snapshot_index(user_id, session_id)?;
        index.append(&mut snapshot).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
        })?;
        write_composite_snapshot_index_unlocked(user_id, session_id, &index, &dir)?;
        prune_unreferenced_heavy_checkpoints_unlocked(&dir, &index)?;
        Ok((number, snapshot, index))
    })
}

/// Read the composite snapshot index from disk.
pub fn read_composite_snapshot_index(
    user_id: &str,
    session_id: &str,
) -> std::io::Result<astra_core::composite_snapshot::CompositeSnapshotIndex> {
    let path = checkpoint_dir_for(user_id, session_id)?.join("composite_snapshots.json");
    if !path.exists() {
        return Ok(astra_core::composite_snapshot::CompositeSnapshotIndex::default());
    }
    let content = std::fs::read_to_string(&path)?;
    record_observed_text(
        astra_core::history_work::HistoryWorkSite::PipelineCompositeIndexRead,
        &content,
    );
    let mut index: astra_core::composite_snapshot::CompositeSnapshotIndex =
        decode_versioned_step_artifact(
            STEP_COMPOSITE_INDEX_ARTIFACT_KIND,
            user_id,
            session_id,
            &content,
        )?;
    index.normalize_versions();
    Ok(index)
}

// ═══════════════════════════════════════════════════════════════════════════════
// File-Backed StepEventStore (JSONL)
// ═══════════════════════════════════════════════════════════════════════════════

/// File path for owner-bound step events JSONL.
pub(crate) fn events_path_for(user_id: &str, session_id: &str) -> std::io::Result<PathBuf> {
    Ok(session_dir_for(user_id, session_id)?.join("step_events.jsonl"))
}

pub(crate) fn session_dir_for(user_id: &str, session_id: &str) -> std::io::Result<PathBuf> {
    owner_session_dir_for(user_id, session_id)
}

/// File-backed event store: in-memory DAG + append-only JSONL on disk.
///
/// Appends are written and closed immediately, but they are not fsynced on the
/// hot path. Heavy checkpoints remain the durable recovery anchors; per-event
/// JSONL gives process-crash replay without adding multi-second filesystem
/// stalls to every tool completion.
pub struct FileBackedEventStore {
    user_id: String,
    session_id: String,
    events: Vec<StepEvent>,
}

#[derive(Clone, Debug, Default)]
pub struct BoundedStepEventWindow {
    pub events: Vec<StepEvent>,
    pub bytes_read: usize,
    pub prefix_truncated: bool,
    pub events_dropped: usize,
    pub trailing_torn_line: bool,
}

impl BoundedStepEventWindow {
    #[must_use]
    pub fn is_complete_since(&self, created_at: u64) -> bool {
        if self.trailing_torn_line {
            return false;
        }
        if !self.prefix_truncated && self.events_dropped == 0 {
            return true;
        }
        self.events
            .first()
            .is_some_and(|event| event.created_at <= created_at)
    }
}

impl FileBackedEventStore {
    /// Create a new store for a session, loading existing events from disk.
    pub fn new(user_id: &str, session_id: &str) -> Self {
        let events = Self::load_events_lenient(user_id, session_id);
        Self {
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            events,
        }
    }

    /// Create empty (for tests or ephemeral sessions).
    pub fn empty(user_id: &str, session_id: &str) -> Self {
        Self {
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            events: Vec::new(),
        }
    }

    fn parse_event_line(
        user_id: &str,
        session_id: &str,
        line: &str,
    ) -> std::io::Result<Option<StepEvent>> {
        if line.trim().is_empty() {
            return Ok(None);
        }
        match serde_json::from_str::<VersionedStepArtifact<StepEvent>>(line) {
            Ok(envelope) => validate_versioned_step_artifact(
                STEP_EVENT_ARTIFACT_KIND,
                user_id,
                session_id,
                envelope,
            )
            .map(Some),
            Err(error)
                if matches!(
                    error.classify(),
                    serde_json::error::Category::Syntax | serde_json::error::Category::Eof
                ) =>
            {
                tracing::warn!(error = %error, "skipping malformed step event JSONL line");
                Ok(None)
            }
            Err(error) => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
        }
    }

    fn load_events_lenient(user_id: &str, session_id: &str) -> Vec<StepEvent> {
        let path = match events_path_for(user_id, session_id) {
            Ok(path) => path,
            Err(error) => {
                tracing::warn!(
                    user_id,
                    session_id,
                    error = %error,
                    "invalid owner-bound path while loading step events leniently"
                );
                return Vec::new();
            }
        };
        let mut events = Vec::new();
        if !path.exists() {
            return events;
        }

        use std::io::BufRead;
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(error) => {
                tracing::warn!(
                        user_id,
                        session_id,
                        error = %error,
                        "failed to open step events for lenient replay"
                );
                return events;
            }
        };
        let observed_bytes = astra_core::history_work::instrumentation_enabled()
            .then(|| {
                file.metadata()
                    .ok()
                    .and_then(|metadata| metadata.len().try_into().ok())
            })
            .flatten();
        let reader = std::io::BufReader::new(file);
        let mut completed_read = true;
        for line in reader.lines() {
            let line = match line {
                Ok(line) => line,
                Err(error) => {
                    tracing::warn!(
                        user_id,
                        session_id,
                        error = %error,
                        "failed to read step event line during lenient replay"
                    );
                    completed_read = false;
                    break;
                }
            };
            match Self::parse_event_line(user_id, session_id, &line) {
                Ok(Some(event)) => events.push(event),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        user_id,
                        session_id,
                        error = %error,
                        "skipping invalid step event during lenient replay"
                    );
                }
            }
        }
        if completed_read && let Some(bytes) = observed_bytes {
            record_event_journal_read(bytes, events.len());
        }
        events
    }

    /// Stream persisted events without materializing the whole journal.
    pub fn for_each_event(
        user_id: &str,
        session_id: &str,
        mut visit: impl FnMut(&StepEvent),
    ) -> std::io::Result<()> {
        let path = events_path_for(user_id, session_id)?;
        if !path.exists() {
            return Ok(());
        }
        use std::io::BufRead;
        let file = std::fs::File::open(&path)?;
        let observed_bytes = astra_core::history_work::instrumentation_enabled()
            .then(|| {
                file.metadata()
                    .ok()
                    .and_then(|metadata| metadata.len().try_into().ok())
            })
            .flatten();
        let reader = std::io::BufReader::new(file);
        let mut observed_rows = 0_usize;
        for line in reader.lines() {
            let line = line?;
            if let Some(event) = Self::parse_event_line(user_id, session_id, &line)? {
                visit(&event);
                observed_rows = observed_rows.saturating_add(1);
            }
        }
        if let Some(bytes) = observed_bytes {
            record_event_journal_read(bytes, observed_rows);
        }
        Ok(())
    }

    /// Stream only events written at or after a checkpoint timestamp. Recovery
    /// uses this instead of materializing the entire long-session journal.
    pub fn load_events_created_at_or_after(
        user_id: &str,
        session_id: &str,
        checkpoint_created_at: u64,
    ) -> std::io::Result<Vec<StepEvent>> {
        let window = Self::load_recent_events_bounded(
            user_id,
            session_id,
            STEP_EVENT_RECOVERY_MAX_BYTES,
            STEP_EVENT_RECOVERY_MAX_EVENTS,
        )?;
        if !window.is_complete_since(checkpoint_created_at) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "step event recovery window is incomplete: bytes_read={}, prefix_truncated={}, events_dropped={}, checkpoint_created_at={checkpoint_created_at}",
                    window.bytes_read, window.prefix_truncated, window.events_dropped
                ),
            ));
        }
        Ok(window
            .events
            .into_iter()
            .filter(|event| event.created_at >= checkpoint_created_at)
            .collect())
    }

    /// Read a byte- and event-bounded tail of the owner-scoped JSONL journal.
    /// A torn final append is reported explicitly; corruption in any complete
    /// line fails rather than yielding a silently partial recovery view.
    pub fn load_recent_events_bounded(
        user_id: &str,
        session_id: &str,
        max_bytes: usize,
        max_events: usize,
    ) -> std::io::Result<BoundedStepEventWindow> {
        if max_bytes == 0 || max_events == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "step event recovery bounds must be positive",
            ));
        }
        let path = events_path_for(user_id, session_id)?;
        if !path.exists() {
            return Ok(BoundedStepEventWindow::default());
        }

        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::open(path)?;
        let file_bytes = file.metadata()?.len();
        let max_bytes_u64 = u64::try_from(max_bytes).unwrap_or(u64::MAX);
        let start = file_bytes.saturating_sub(max_bytes_u64);
        let starts_at_line_boundary = if start == 0 {
            true
        } else {
            file.seek(SeekFrom::Start(start - 1))?;
            let mut previous = [0_u8; 1];
            file.read_exact(&mut previous)?;
            previous[0] == b'\n'
        };
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::with_capacity(
            usize::try_from(file_bytes.saturating_sub(start)).unwrap_or(max_bytes),
        );
        file.read_to_end(&mut bytes)?;
        let bytes_read = bytes.len();
        let mut prefix_truncated = start > 0;
        if prefix_truncated && !starts_at_line_boundary {
            match bytes.iter().position(|byte| *byte == b'\n') {
                Some(end) => {
                    bytes.drain(..=end);
                }
                None => {
                    bytes.clear();
                }
            }
        }
        let text = String::from_utf8(bytes).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("step event journal is not UTF-8: {error}"),
            )
        })?;
        let ends_with_newline = text.ends_with('\n');
        let mut events = Vec::new();
        let mut trailing_torn_line = false;
        let line_count = text.split('\n').count();
        for (index, line) in text.split('\n').enumerate() {
            let line = line.trim_end_matches('\r');
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<VersionedStepArtifact<StepEvent>>(line) {
                Ok(envelope) => events.push(validate_versioned_step_artifact(
                    STEP_EVENT_ARTIFACT_KIND,
                    user_id,
                    session_id,
                    envelope,
                )?),
                Err(_) if index + 1 == line_count && !ends_with_newline => {
                    trailing_torn_line = true;
                }
                Err(error) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("invalid complete step event JSONL line: {error}"),
                    ));
                }
            }
        }
        let events_dropped = events.len().saturating_sub(max_events);
        if events_dropped > 0 {
            events.drain(..events_dropped);
            prefix_truncated = true;
        }
        record_event_journal_read(bytes_read, events.len());
        Ok(BoundedStepEventWindow {
            events,
            bytes_read,
            prefix_truncated,
            events_dropped,
            trailing_torn_line,
        })
    }

    /// Append a single event to the JSONL file.
    fn persist_event(&self, event: &StepEvent) -> std::io::Result<()> {
        let dir = session_dir_for(&self.user_id, &self.session_id)?;
        std::fs::create_dir_all(&dir)?;
        let path = events_path_for(&self.user_id, &self.session_id)?;
        let json = encode_versioned_step_artifact(
            STEP_EVENT_ARTIFACT_KIND,
            &self.user_id,
            &self.session_id,
            event,
        )?;
        append_jsonl_line(&path, &json, step_event_write_durability(event))
    }

    /// Get all events (for audit/replay).
    pub fn all_events(&self) -> &[StepEvent] {
        &self.events
    }

    /// Event count.
    pub fn event_count(&self) -> usize {
        self.events.len()
    }
}

fn step_event_write_durability(event: &StepEvent) -> WriteDurability {
    use crate::step_protocol::StepEventType;

    // A non-replay-safe tool's start and terminal receipt are the minimal
    // crash-consistency boundary around an external side effect. Persisting
    // the start prevents a reboot from treating an uncertain invocation as
    // never attempted; persisting the terminal receipt proves completion.
    // Pure reads, idempotent writes, and other trace events stay buffered so
    // ordinary observation does not pay two fsyncs per tool. A skip receipt
    // follows the same safety class as its start: otherwise a durable start
    // plus a lost skip would look like an in-flight side effect after reboot.
    match &event.event_type {
        StepEventType::ToolCallStarted
        | StepEventType::ToolCallCompleted
        | StepEventType::ToolCallFailed
        | StepEventType::ToolCallSkipped => {
            let replay_safe = event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("tool_name"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|tool_name| {
                    !matches!(
                        astra_turn_types::classify_tool_idempotency(tool_name, None),
                        astra_turn_types::ToolIdempotency::NonIdempotent
                    )
                });
            if replay_safe {
                WriteDurability::Buffered
            } else {
                WriteDurability::Durable
            }
        }
        _ => WriteDurability::Buffered,
    }
}

impl StepEventStore for FileBackedEventStore {
    fn append(&mut self, event: StepEvent) -> std::io::Result<()> {
        self.persist_event(&event)?;
        self.events.push(event);
        Ok(())
    }

    fn events_for_step(&self, step_id: &str) -> Vec<&StepEvent> {
        self.events
            .iter()
            .filter(|e| e.step_id == step_id)
            .collect()
    }

    fn ancestors(&self, event_id: &str) -> Vec<&StepEvent> {
        let mut result = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        let mut visited = std::collections::HashSet::new();
        queue.push_back(event_id.to_string());
        visited.insert(event_id.to_string());

        while let Some(current) = queue.pop_front() {
            if let Some(ev) = self.events.iter().find(|e| e.event_id == current) {
                if ev.event_id != event_id {
                    result.push(ev);
                }
                for parent in &ev.caused_by {
                    if visited.insert(parent.clone()) {
                        queue.push_back(parent.clone());
                    }
                }
            }
        }
        result
    }

    fn descendants(&self, event_id: &str) -> Vec<&StepEvent> {
        let mut result = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        let mut visited = std::collections::HashSet::new();
        queue.push_back(event_id.to_string());
        visited.insert(event_id.to_string());

        while let Some(current) = queue.pop_front() {
            for ev in &self.events {
                if ev.caused_by.contains(&current) && visited.insert(ev.event_id.clone()) {
                    result.push(ev);
                    queue.push_back(ev.event_id.clone());
                }
            }
        }
        result
    }

    fn leaves(&self) -> Vec<&StepEvent> {
        let parent_ids: std::collections::HashSet<&str> = self
            .events
            .iter()
            .flat_map(|e| e.caused_by.iter().map(|s| s.as_str()))
            .collect();
        self.events
            .iter()
            .filter(|e| !parent_ids.contains(e.event_id.as_str()))
            .collect()
    }

    fn len(&self) -> usize {
        self.events.len()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::step_protocol::{ExecutionCursor, PROTOCOL_VERSION};
    use serde_json::json;

    const TEST_USER_ID: &str = "test-user";

    #[test]
    fn observed_text_bytes_matches_exact_utf8_artifact_buffer() {
        let content = r#"{"role":"user","content":"历史🙂"}"#;
        assert_eq!(
            observed_text_bytes(content),
            u64::try_from(content.len()).expect("test buffer length fits u64")
        );
        assert!(
            content.len() > content.chars().count(),
            "instrumentation must count persisted bytes rather than Unicode scalar values"
        );
        assert_eq!(observed_text_bytes(""), 0);
    }

    fn make_light(step_id: &str, progress: f64) -> LightCheckpoint {
        LightCheckpoint {
            protocol_version: PROTOCOL_VERSION,
            cursor: ExecutionCursor::for_act(1),
            step_id: step_id.to_string(),
            task_id: "task-1".to_string(),
            agent_id: "agent-1".to_string(),
            progress,
            total_tokens: 1000,
            created_at: 1234567890,
        }
    }

    fn make_heavy(step_id: &str, messages: Vec<serde_json::Value>) -> HeavyCheckpoint {
        HeavyCheckpoint {
            light: make_light(step_id, 0.5),
            conversation_cursor: None,
            messages,
            budget_remaining_tokens: 50000,
            budget_remaining_rounds: 8,
            blocked_tools: vec!["bash".to_string()],
            recent_tools: vec!["grep".to_string(), "read_file".to_string()],
            activated_deferred_tool_names: Vec::new(),
            memory_context: None,
            delegation_id: None,
            delegation_pattern: None,
            delegation_sub_run_summaries: Vec::new(),
            interruption: None,
            approval_overrides: None,
            consecutive_context_window_errors: 0,
            compaction_state: None,
            pipeline_state: None,
            config_version_id: None,
            workspace_observation_quarantine: None,
        }
    }

    fn unique_session_id(prefix: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{prefix}-{}-{nanos}", std::process::id())
    }

    #[test]
    fn light_checkpoints_are_buffered_and_heavy_checkpoints_are_durable() {
        let light = StepCheckpoint::Light(make_light("light-fast-path", 0.25));
        let heavy = StepCheckpoint::Heavy(Box::new(make_heavy(
            "heavy-anchor",
            vec![json!({"role": "assistant", "content": "done"})],
        )));

        assert_eq!(
            checkpoint_write_durability(&light),
            WriteDurability::Buffered,
            "light checkpoints stay on the hot path and rely on heavy checkpoints for OS-crash anchors"
        );
        assert_eq!(
            checkpoint_write_durability(&heavy),
            WriteDurability::Durable,
            "heavy checkpoints are the low-frequency durable recovery anchor"
        );
    }

    #[test]
    fn tool_receipts_are_durable_while_non_execution_trace_stays_buffered() {
        for event_type in [
            StepEventType::ToolCallStarted,
            StepEventType::ToolCallCompleted,
            StepEventType::ToolCallFailed,
        ] {
            let event = make_event("receipt", "step", event_type);
            assert_eq!(
                step_event_write_durability(&event),
                WriteDurability::Durable
            );
        }
        let mut replay_safe = make_event("read-receipt", "step", StepEventType::ToolCallCompleted);
        replay_safe.payload = Some(json!({"tool_name": "read_file"}));
        assert_eq!(
            step_event_write_durability(&replay_safe),
            WriteDurability::Buffered
        );
        let mut skipped = make_event("skipped", "step", StepEventType::ToolCallSkipped);
        skipped.payload = Some(json!({"tool_name": "read_file"}));
        assert_eq!(
            step_event_write_durability(&skipped),
            WriteDurability::Buffered
        );
        let trace = make_event("trace", "step", StepEventType::LlmRoundCompleted);
        assert_eq!(
            step_event_write_durability(&trace),
            WriteDurability::Buffered
        );
    }

    #[test]
    fn durable_tool_receipt_append_is_immediately_replayable() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let session_id = unique_session_id("buffered-event");
        let mut store = FileBackedEventStore::empty(TEST_USER_ID, &session_id);
        let event = StepEvent {
            event_id: "evt-buffered".to_string(),
            run_id: "test-run".into(),
            canonical_event_id: None,
            step_id: "step-buffered".to_string(),
            event_type: crate::step_protocol::StepEventType::ToolCallCompleted,
            agent_id: None,
            caused_by: Vec::new(),
            payload: Some(json!({"tool_name": "bash"})),
            created_at: 123,
        };

        store.append(event).expect("append durable tool receipt");
        let replayed = FileBackedEventStore::new(TEST_USER_ID, &session_id);

        assert_eq!(replayed.event_count(), 1);
        assert_eq!(replayed.all_events()[0].event_id, "evt-buffered");
    }

    #[test]
    fn concurrent_event_writers_preserve_complete_jsonl_records() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut writers = Vec::new();
        for writer in 0..8 {
            let path = path.clone();
            let barrier = barrier.clone();
            writers.push(std::thread::spawn(move || {
                barrier.wait();
                for index in 0..5 {
                    let record = json!({"writer": writer, "index": index}).to_string();
                    append_jsonl_line(&path, &record, WriteDurability::Durable).unwrap();
                }
            }));
        }
        for writer in writers {
            writer.join().unwrap();
        }

        let text = std::fs::read_to_string(&path).unwrap();
        let records = text
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 40);
        assert_eq!(
            records
                .iter()
                .map(|record| (
                    record["writer"].as_u64().unwrap(),
                    record["index"].as_u64().unwrap()
                ))
                .collect::<std::collections::HashSet<_>>()
                .len(),
            40
        );
    }

    #[cfg(unix)]
    #[test]
    fn append_repairs_legacy_event_journal_permissions_while_locked() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        std::fs::write(&path, "{}\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        append_jsonl_line(&path, "{}", WriteDurability::Buffered).unwrap();

        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn durable_heavy_checkpoint_supersedes_all_older_light_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let session_id = unique_session_id("prune-list");

        let light_total = MAX_LIGHT_CHECKPOINTS + 10;
        for i in 0..light_total {
            let checkpoint = StepCheckpoint::Light(make_light(&format!("light-{i}"), 0.5));
            write_step_checkpoint(TEST_USER_ID, &session_id, i as u32, &checkpoint).unwrap();
        }
        for i in 0..5 {
            let number = (light_total + i) as u32;
            let checkpoint = StepCheckpoint::Heavy(Box::new(make_heavy(
                &format!("heavy-{i}"),
                vec![json!({"role": "assistant", "content": format!("heavy-{i}")})],
            )));
            write_step_checkpoint(TEST_USER_ID, &session_id, number, &checkpoint).unwrap();
        }

        let listed = list_checkpoints(TEST_USER_ID, &session_id).unwrap();
        let light_numbers: Vec<u32> = listed
            .iter()
            .filter_map(|(number, tier)| matches!(tier, &CheckpointTier::Light).then_some(*number))
            .collect();
        let heavy_numbers: Vec<u32> = listed
            .iter()
            .filter_map(|(number, tier)| matches!(tier, &CheckpointTier::Heavy).then_some(*number))
            .collect();

        assert!(
            light_numbers.is_empty(),
            "the first durable heavy checkpoint embeds and supersedes every older light cursor"
        );
        assert_eq!(
            heavy_numbers,
            ((light_total as u32)..(light_total as u32 + 5)).collect::<Vec<_>>(),
            "heavy checkpoints must not be pruned when light checkpoints exceed the limit"
        );
    }

    #[test]
    fn durable_heavy_checkpoint_supersedes_older_light_cursor_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let session_id = unique_session_id("heavy-supersedes-light");
        let light = make_light("same-step", 1.0);
        let heavy = make_heavy(
            "same-step",
            vec![json!({"role": "assistant", "content": "recoverable"})],
        );

        write_step_checkpoint(TEST_USER_ID, &session_id, 1, &StepCheckpoint::Light(light)).unwrap();
        write_step_checkpoint(
            TEST_USER_ID,
            &session_id,
            2,
            &StepCheckpoint::Heavy(Box::new(heavy)),
        )
        .unwrap();

        assert_eq!(
            list_checkpoints(TEST_USER_ID, &session_id).unwrap(),
            vec![(2, CheckpointTier::Heavy)],
            "the durable heavy checkpoint already contains the latest cursor and full recovery state"
        );
        assert_eq!(
            read_latest_light_checkpoint(TEST_USER_ID, &session_id)
                .unwrap()
                .expect("heavy checkpoint exposes its embedded light cursor")
                .step_id,
            "same-step"
        );
        assert_eq!(
            read_latest_heavy_checkpoint(TEST_USER_ID, &session_id)
                .unwrap()
                .expect("durable recovery anchor")
                .messages,
            vec![json!({"role": "assistant", "content": "recoverable"})]
        );
    }

    #[test]
    fn composite_index_prunes_only_unreferenced_heavy_recovery_anchors() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let session_id = unique_session_id("prune-unreferenced-heavy");
        for number in 1..=3 {
            write_step_checkpoint(
                TEST_USER_ID,
                &session_id,
                number,
                &StepCheckpoint::Heavy(Box::new(make_heavy(
                    &format!("step-{number}"),
                    vec![json!({"role": "assistant", "content": number})],
                ))),
            )
            .unwrap();
        }

        let mut index = astra_core::composite_snapshot::CompositeSnapshotIndex::default();
        for number in [1, 3] {
            let mut snapshot = astra_core::composite_snapshot::CompositeSnapshotBuilder::new(
                session_id.clone(),
                number,
            )
            .session_state(format!("{number:06}-heavy.json"))
            .build();
            index.append(&mut snapshot).unwrap();
        }

        assert_eq!(
            prune_unreferenced_heavy_checkpoints(TEST_USER_ID, &session_id, &index).unwrap(),
            1
        );
        assert_eq!(
            list_checkpoints(TEST_USER_ID, &session_id).unwrap(),
            vec![(1, CheckpointTier::Heavy), (3, CheckpointTier::Heavy)]
        );
    }

    #[test]
    fn heavy_checkpoint_never_prunes_a_later_light_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let session_id = unique_session_id("heavy-preserves-later-light");

        write_step_checkpoint(
            TEST_USER_ID,
            &session_id,
            3,
            &StepCheckpoint::Light(make_light("later-step", 0.75)),
        )
        .unwrap();
        write_step_checkpoint(
            TEST_USER_ID,
            &session_id,
            2,
            &StepCheckpoint::Heavy(Box::new(make_heavy(
                "earlier-step",
                vec![json!({"role": "assistant", "content": "earlier"})],
            ))),
        )
        .unwrap();

        assert_eq!(
            list_checkpoints(TEST_USER_ID, &session_id).unwrap(),
            vec![(2, CheckpointTier::Heavy), (3, CheckpointTier::Light)],
            "cleanup must be ordered by the durable recovery frontier, not by file type alone"
        );
        assert_eq!(
            read_latest_light_checkpoint(TEST_USER_ID, &session_id)
                .unwrap()
                .expect("later cursor remains recoverable")
                .step_id,
            "later-step"
        );
    }

    #[test]
    fn next_checkpoint_number_uses_the_persisted_session_sequence() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let session_id = unique_session_id("next-session-checkpoint");

        assert_eq!(
            next_checkpoint_number(TEST_USER_ID, &session_id).unwrap(),
            1
        );
        write_step_checkpoint(
            TEST_USER_ID,
            &session_id,
            7,
            &StepCheckpoint::Light(make_light("run-local-seven", 0.5)),
        )
        .unwrap();
        write_step_checkpoint(
            TEST_USER_ID,
            &session_id,
            12,
            &StepCheckpoint::Heavy(Box::new(make_heavy(
                "later-run",
                vec![json!({"role": "assistant", "content": "durable"})],
            ))),
        )
        .unwrap();

        assert_eq!(
            next_checkpoint_number(TEST_USER_ID, &session_id).unwrap(),
            13,
            "a new run must continue the session sequence instead of reusing its local counter"
        );
    }

    #[test]
    fn concurrent_composite_publishers_keep_both_index_entries_and_anchors() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let sessions_root = tmp.path().to_path_buf();
        let session_id = unique_session_id("concurrent-composite");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut threads = Vec::new();
        for turn in [1_u32, 2_u32] {
            let session_id = session_id.clone();
            let barrier = barrier.clone();
            let sessions_root = sessions_root.clone();
            threads.push(std::thread::spawn(move || {
                let _guard = astra_services::session_journal::JournalDirGuard::new(sessions_root);
                let checkpoint = StepCheckpoint::Heavy(Box::new(make_heavy(
                    &format!("step-{turn}"),
                    vec![json!({"role": "assistant", "content": format!("turn-{turn}")})],
                )));
                let snapshot = astra_core::composite_snapshot::CompositeSnapshotBuilder::new(
                    session_id.clone(),
                    turn,
                )
                .workspace_state(session_id.clone())
                .build();
                barrier.wait();
                commit_composite_checkpoint(TEST_USER_ID, &session_id, &checkpoint, snapshot)
                    .unwrap()
            }));
        }
        barrier.wait();
        let committed = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        let index = read_composite_snapshot_index(TEST_USER_ID, &session_id).unwrap();
        assert_eq!(index.snapshots.len(), 2);
        let refs = index
            .snapshots
            .iter()
            .filter_map(|snapshot| snapshot.session_state())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(refs.len(), 2);
        let dir = checkpoint_dir_for(TEST_USER_ID, &session_id).unwrap();
        assert!(refs.iter().all(|reference| dir.join(reference).exists()));
        assert!(
            committed
                .iter()
                .all(|(_, snapshot, _)| snapshot.version > 0)
        );
    }

    #[test]
    fn write_step_checkpoint_creates_dir_and_file() {
        // Use a unique session ID with tempdir-like suffix to avoid collision
        let session_id = format!("test-step-cp-{}", std::process::id());
        let dir = checkpoint_dir_for(TEST_USER_ID, &session_id).unwrap();

        // Clean up from any previous run
        let _ = std::fs::remove_dir_all(&dir);

        let light = make_light("step-write-test", 1.0);
        let cp = StepCheckpoint::Light(light);
        let result = write_step_checkpoint(TEST_USER_ID, &session_id, 1, &cp);
        assert!(result.is_ok());
        let path = result.unwrap();
        assert!(path.exists());

        // Read back the versioned owner-bound JSON artifact.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.trim_start().starts_with('{'));
        let envelope: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            envelope["schema_version"],
            serde_json::json!(STEP_ARTIFACT_SCHEMA_VERSION)
        );
        assert_eq!(envelope["layout_version"], STEP_LOCAL_LAYOUT_VERSION);
        assert_eq!(envelope["artifact_kind"], STEP_CHECKPOINT_ARTIFACT_KIND);
        assert_eq!(envelope["user_id"], TEST_USER_ID);
        assert_eq!(envelope["session_id"], session_id);
        assert_eq!(envelope["payload"]["Light"]["step_id"], "step-write-test");

        let restored = read_latest_light_checkpoint(TEST_USER_ID, &session_id)
            .unwrap()
            .expect("written light checkpoint must be readable");
        assert_eq!(restored.step_id, "step-write-test");

        // Clean up
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn owner_directory_key_is_versioned_url_safe_base64() {
        let user_id = "owner+matrixone@example.com";
        let session_id = "owner-layout";

        let path = owner_session_dir_for(user_id, session_id).unwrap();
        let rendered = path.to_string_lossy();
        assert!(rendered.contains(&format!(
            "/{STEP_LOCAL_LAYOUT_VERSION}/users/b64-b3duZXIrbWF0cml4b25lQGV4YW1wbGUuY29t/sessions/owner-layout"
        )));
        assert!(!rendered.contains("sha256-"));
        assert!(!rendered.contains('='));
    }

    #[test]
    fn delete_step_checkpoint_removes_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let session_id = "delete-existing";
        let checkpoint = StepCheckpoint::Light(make_light("step-delete", 1.0));
        let path = write_step_checkpoint(TEST_USER_ID, session_id, 7, &checkpoint).unwrap();
        assert!(path.exists());

        delete_step_checkpoint(TEST_USER_ID, session_id, 7, "light").unwrap();

        assert!(!path.exists());
    }

    #[test]
    fn delete_step_checkpoint_ignores_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());

        delete_step_checkpoint(TEST_USER_ID, "delete-missing", 99, "heavy").unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn delete_step_checkpoint_surfaces_permission_denied() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let session_id = "delete-perms";
        let checkpoint = StepCheckpoint::Light(make_light("step-delete-perms", 1.0));
        let path = write_step_checkpoint(TEST_USER_ID, session_id, 3, &checkpoint).unwrap();
        let dir = path.parent().expect("checkpoint dir").to_path_buf();

        let original_permissions = std::fs::metadata(&dir).unwrap().permissions();
        let mut readonly_permissions = original_permissions.clone();
        readonly_permissions.set_mode(0o555);
        std::fs::set_permissions(&dir, readonly_permissions).unwrap();

        let result = delete_step_checkpoint(TEST_USER_ID, session_id, 3, "light");

        std::fs::set_permissions(&dir, original_permissions).unwrap();

        let error = result.expect_err("readonly checkpoint dir should deny deletion");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            path.exists(),
            "failed delete must leave checkpoint untouched"
        );
    }

    #[test]
    fn composite_snapshot_index_is_versioned_owner_bound_json() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let session_id = "test-composite-snapshot-index";

        let index = astra_core::composite_snapshot::CompositeSnapshotIndex::default();
        write_composite_snapshot_index(TEST_USER_ID, session_id, &index).unwrap();

        let raw = std::fs::read_to_string(
            checkpoint_dir_for(TEST_USER_ID, session_id)
                .unwrap()
                .join("composite_snapshots.json"),
        )
        .unwrap();
        let envelope: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            envelope["schema_version"],
            serde_json::json!(STEP_ARTIFACT_SCHEMA_VERSION)
        );
        assert_eq!(envelope["layout_version"], STEP_LOCAL_LAYOUT_VERSION);
        assert_eq!(
            envelope["artifact_kind"],
            STEP_COMPOSITE_INDEX_ARTIFACT_KIND
        );
        assert_eq!(envelope["user_id"], TEST_USER_ID);
        assert_eq!(envelope["session_id"], session_id);
        assert!(
            envelope["payload"]["snapshots"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        let restored = read_composite_snapshot_index(TEST_USER_ID, session_id).unwrap();
        assert!(restored.snapshots.is_empty());
    }

    #[test]
    fn read_latest_heavy_on_empty_returns_none() {
        let result = read_latest_heavy_checkpoint(TEST_USER_ID, "nonexistent-session-xyz-42");
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    // ── FileBackedEventStore tests ──────────────────────────────────────────

    use crate::step_protocol::StepEventType;

    fn make_event(id: &str, step_id: &str, event_type: StepEventType) -> StepEvent {
        StepEvent {
            event_id: id.to_string(),
            run_id: "test-run".into(),
            step_id: step_id.to_string(),
            event_type,
            agent_id: None,
            caused_by: vec![],
            payload: None,
            canonical_event_id: None,
            created_at: 1000,
        }
    }

    fn checkpoint_json_for_test(session_id: &str, checkpoint: &StepCheckpoint) -> String {
        encode_versioned_step_artifact(
            STEP_CHECKPOINT_ARTIFACT_KIND,
            TEST_USER_ID,
            session_id,
            checkpoint,
        )
        .unwrap()
    }

    fn event_json_for_test(session_id: &str, event: &StepEvent) -> String {
        encode_versioned_step_artifact(STEP_EVENT_ARTIFACT_KIND, TEST_USER_ID, session_id, event)
            .unwrap()
    }

    #[test]
    fn file_event_store_does_not_advance_memory_when_append_fails() {
        let mut store = FileBackedEventStore::empty(TEST_USER_ID, "../invalid-session-id");
        let result = store.append(make_event("e1", "step-1", StepEventType::StepCreated));

        assert!(result.is_err());
        assert_eq!(
            store.event_count(),
            0,
            "memory view must not advance ahead of durable journal"
        );
    }

    #[test]
    fn file_event_store_persist_and_reload() {
        let session_id = format!("test-persist-events-{}", std::process::id());
        let path = events_path_for(TEST_USER_ID, &session_id).unwrap();

        // Clean up from previous runs
        let _ = std::fs::remove_file(&path);

        {
            let mut store = FileBackedEventStore::empty(TEST_USER_ID, &session_id);
            let _ = store.append(make_event("e1", "s1", StepEventType::StepCreated));
            let _ = store.append(make_event("e2", "s1", StepEventType::ToolCallCompleted));
        }

        // Reload from disk
        let store2 = FileBackedEventStore::new(TEST_USER_ID, &session_id);
        assert_eq!(store2.event_count(), 2);
        assert_eq!(store2.all_events()[0].event_id, "e1");
        assert_eq!(store2.all_events()[1].event_id, "e2");

        // Clean up
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(session_dir_for(TEST_USER_ID, &session_id).unwrap());
    }

    #[test]
    fn lenient_event_view_keeps_valid_prefix_but_recovery_rejects_torn_tail() {
        let session_id = unique_session_id("test-torn-jsonl-tail");
        let path = events_path_for(TEST_USER_ID, &session_id).unwrap();
        let _ = std::fs::remove_dir_all(session_dir_for(TEST_USER_ID, &session_id).unwrap());

        {
            let mut store = FileBackedEventStore::empty(TEST_USER_ID, &session_id);
            let _ = store.append(make_event("e1", "s1", StepEventType::StepCreated));
            let _ = store.append(make_event("e2", "s1", StepEventType::ToolCallCompleted));
        }

        let torn_json = event_json_for_test(
            &session_id,
            &make_event("torn", "s1", StepEventType::ToolCallCompleted),
        );
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(&torn_json.as_bytes()[..17]).unwrap();
        }

        let store = FileBackedEventStore::new(TEST_USER_ID, &session_id);
        let ids: Vec<_> = store
            .all_events()
            .iter()
            .map(|event| event.event_id.as_str())
            .collect();
        assert_eq!(ids, vec!["e1", "e2"]);

        let error =
            FileBackedEventStore::load_events_created_at_or_after(TEST_USER_ID, &session_id, 0)
                .expect_err("crash recovery must not hide a torn terminal event");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

        let _ = std::fs::remove_dir_all(session_dir_for(TEST_USER_ID, &session_id).unwrap());
    }

    #[test]
    fn append_after_torn_jsonl_tail_keeps_new_events_readable() {
        let session_id = unique_session_id("test-append-after-torn-jsonl-tail");
        let path = events_path_for(TEST_USER_ID, &session_id).unwrap();
        let _ = std::fs::remove_dir_all(session_dir_for(TEST_USER_ID, &session_id).unwrap());

        {
            let mut store = FileBackedEventStore::empty(TEST_USER_ID, &session_id);
            let _ = store.append(make_event("e1", "s1", StepEventType::StepCreated));
        }

        let torn_json = event_json_for_test(
            &session_id,
            &make_event("torn", "s1", StepEventType::ToolCallCompleted),
        );
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(&torn_json.as_bytes()[..17]).unwrap();
        }

        {
            let mut store = FileBackedEventStore::empty(TEST_USER_ID, &session_id);
            let _ = store.append(make_event("e2", "s1", StepEventType::ToolCallCompleted));
        }

        let store = FileBackedEventStore::new(TEST_USER_ID, &session_id);
        let ids: Vec<_> = store
            .all_events()
            .iter()
            .map(|event| event.event_id.as_str())
            .collect();
        assert_eq!(ids, vec!["e1", "e2"]);

        let _ = std::fs::remove_dir_all(session_dir_for(TEST_USER_ID, &session_id).unwrap());
    }

    #[test]
    fn file_event_store_loads_recovery_window_without_full_store_materialization() {
        let session_id = format!("test-recovery-window-{}", std::process::id());
        let path = events_path_for(TEST_USER_ID, &session_id).unwrap();
        let _ = std::fs::remove_file(&path);

        {
            let mut store = FileBackedEventStore::empty(TEST_USER_ID, &session_id);
            for idx in 0..10 {
                let mut event =
                    make_event(&format!("e{idx}"), "s1", StepEventType::ToolCallCompleted);
                event.created_at = idx * 100;
                let _ = store.append(event);
            }
        }

        let events =
            FileBackedEventStore::load_events_created_at_or_after(TEST_USER_ID, &session_id, 500)
                .expect("load recovery window");
        let ids: Vec<_> = events.iter().map(|event| event.event_id.as_str()).collect();
        assert_eq!(ids, vec!["e5", "e6", "e7", "e8", "e9"]);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(session_dir_for(TEST_USER_ID, &session_id).unwrap());
    }

    #[test]
    fn bounded_event_tail_reports_omitted_history_and_proves_only_covered_checkpoints() {
        let session_id = unique_session_id("test-bounded-event-tail");
        let _ = std::fs::remove_dir_all(session_dir_for(TEST_USER_ID, &session_id).unwrap());
        {
            let mut store = FileBackedEventStore::empty(TEST_USER_ID, &session_id);
            for idx in 0..10 {
                let mut event =
                    make_event(&format!("e{idx}"), "s1", StepEventType::ToolCallCompleted);
                event.created_at = idx * 100;
                store.append(event).unwrap();
            }
        }

        let window = FileBackedEventStore::load_recent_events_bounded(
            TEST_USER_ID,
            &session_id,
            STEP_EVENT_RECOVERY_MAX_BYTES,
            3,
        )
        .unwrap();
        let ids = window
            .events
            .iter()
            .map(|event| event.event_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["e7", "e8", "e9"]);
        assert!(window.prefix_truncated);
        assert_eq!(window.events_dropped, 7);
        assert!(window.is_complete_since(700));
        assert!(!window.is_complete_since(699));

        let _ = std::fs::remove_dir_all(session_dir_for(TEST_USER_ID, &session_id).unwrap());
    }

    #[test]
    fn bounded_event_tail_surfaces_torn_tail_without_accepting_midstream_corruption() {
        let session_id = unique_session_id("test-bounded-event-corruption");
        let path = events_path_for(TEST_USER_ID, &session_id).unwrap();
        let _ = std::fs::remove_dir_all(session_dir_for(TEST_USER_ID, &session_id).unwrap());
        std::fs::create_dir_all(session_dir_for(TEST_USER_ID, &session_id).unwrap()).unwrap();
        let event = event_json_for_test(
            &session_id,
            &make_event("e1", "s1", StepEventType::ToolCallCompleted),
        );
        std::fs::write(&path, format!("{event}\n{{\"torn\":")).unwrap();
        let window = FileBackedEventStore::load_recent_events_bounded(
            TEST_USER_ID,
            &session_id,
            STEP_EVENT_RECOVERY_MAX_BYTES,
            STEP_EVENT_RECOVERY_MAX_EVENTS,
        )
        .unwrap();
        assert_eq!(window.events.len(), 1);
        assert!(window.trailing_torn_line);

        std::fs::write(&path, format!("{event}\n{{not-json}}\n{event}\n")).unwrap();
        let error = FileBackedEventStore::load_recent_events_bounded(
            TEST_USER_ID,
            &session_id,
            STEP_EVENT_RECOVERY_MAX_BYTES,
            STEP_EVENT_RECOVERY_MAX_EVENTS,
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

        let _ = std::fs::remove_dir_all(session_dir_for(TEST_USER_ID, &session_id).unwrap());
    }

    #[test]
    fn file_event_store_fails_on_corrupt_event_json() {
        let session_id = format!("test-corrupt-event-json-{}", std::process::id());
        let path = events_path_for(TEST_USER_ID, &session_id).unwrap();
        let _ = std::fs::remove_file(&path);
        std::fs::create_dir_all(session_dir_for(TEST_USER_ID, &session_id).unwrap())
            .expect("session dir");
        append_jsonl_line(
            &path,
            r#"{"not":"a step event"}"#,
            WriteDurability::Buffered,
        )
        .expect("append corrupt event");

        let error =
            FileBackedEventStore::load_events_created_at_or_after(TEST_USER_ID, &session_id, 0)
                .expect_err("corrupt event json should fail recovery");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(session_dir_for(TEST_USER_ID, &session_id).unwrap());
    }

    #[test]
    fn file_event_store_new_skips_invalid_event_without_emptying_history() {
        let session_id = unique_session_id("test-lenient-invalid-event");
        let path = events_path_for(TEST_USER_ID, &session_id).unwrap();
        let _ = std::fs::remove_dir_all(session_dir_for(TEST_USER_ID, &session_id).unwrap());
        std::fs::create_dir_all(session_dir_for(TEST_USER_ID, &session_id).unwrap())
            .expect("session dir");

        let e1 = event_json_for_test(
            &session_id,
            &make_event("e1", "s1", StepEventType::StepCreated),
        );
        let e2 = event_json_for_test(
            &session_id,
            &make_event("e2", "s1", StepEventType::ToolCallCompleted),
        );
        let content = format!("{e1}\n{{\"not\":\"a step event\"}}\n{e2}\n");
        std::fs::write(&path, content).expect("write mixed event log");

        let store = FileBackedEventStore::new(TEST_USER_ID, &session_id);
        let ids: Vec<_> = store
            .all_events()
            .iter()
            .map(|event| event.event_id.as_str())
            .collect();
        assert_eq!(ids, vec!["e1", "e2"]);

        let _ = std::fs::remove_dir_all(session_dir_for(TEST_USER_ID, &session_id).unwrap());
    }

    #[test]
    fn file_event_store_handles_empty_session() {
        let store = FileBackedEventStore::new(TEST_USER_ID, "nonexistent-event-session-xyz");
        assert_eq!(store.event_count(), 0);
        assert!(store.all_events().is_empty());
    }

    // ── Corruption robustness tests (regression for silent IO fix) ──────

    #[test]
    fn read_heavy_skips_corrupted_json_files() {
        let session_id = format!("test-corrupt-heavy-{}", std::process::id());
        let dir = checkpoint_dir_for(TEST_USER_ID, &session_id).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Write a valid heavy checkpoint.
        let heavy = make_heavy("step-ok", vec![json!({"role": "user", "content": "hello"})]);
        let cp = StepCheckpoint::Heavy(Box::new(heavy));
        let json_str = checkpoint_json_for_test(&session_id, &cp);
        std::fs::write(dir.join("000002-heavy.json"), &json_str).unwrap();

        // Write a corrupted heavy checkpoint with a higher number
        std::fs::write(dir.join("000003-heavy.json"), "NOT VALID JSON{{{").unwrap();

        // read_latest_heavy should skip 000003 (corrupted) and fall back to 000002 (valid).
        let result = read_latest_heavy_checkpoint(TEST_USER_ID, &session_id);
        assert!(
            result.is_ok(),
            "Corrupted checkpoint must not propagate error: {:?}",
            result.err()
        );
        let cp = result.unwrap();
        assert!(cp.is_some(), "must fall back to valid checkpoint");
        assert_eq!(cp.unwrap().light.step_id, "step-ok");

        // Clean up
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn read_light_skips_corrupted_json_files() {
        let session_id = format!("test-corrupt-light-{}", std::process::id());
        let dir = checkpoint_dir_for(TEST_USER_ID, &session_id).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Write a valid light checkpoint.
        let light = make_light("step-ok", 0.5);
        let cp = StepCheckpoint::Light(light);
        let json_str = checkpoint_json_for_test(&session_id, &cp);
        std::fs::write(dir.join("000001-light.json"), &json_str).unwrap();

        // Write a corrupted light checkpoint with higher number
        std::fs::write(dir.join("000002-light.json"), "GARBAGE").unwrap();

        // read_latest_light tries 000002 first → corrupted → falls back to 000001
        let result = read_latest_light_checkpoint(TEST_USER_ID, &session_id);
        assert!(
            result.is_ok(),
            "Corrupted light checkpoint must not propagate error: {:?}",
            result.err()
        );
        let cp = result.unwrap();
        assert!(cp.is_some(), "must fall back to valid checkpoint");
        assert_eq!(cp.unwrap().step_id, "step-ok");

        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn file_event_store_skips_malformed_jsonl_lines() {
        let session_id = format!("test-malformed-jsonl-{}", std::process::id());
        let dir = session_dir_for(TEST_USER_ID, &session_id).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Write a JSONL file with some valid and some malformed lines
        let valid_event = make_event("e1", "s1", StepEventType::StepCreated);
        let valid_json = event_json_for_test(&session_id, &valid_event);

        let content = format!("{valid_json}\nNOT VALID JSON\n{{\n{valid_json}\n");
        std::fs::write(
            events_path_for(TEST_USER_ID, &session_id).unwrap(),
            &content,
        )
        .unwrap();

        // Load should skip malformed lines, keep valid ones
        let store = FileBackedEventStore::new(TEST_USER_ID, &session_id);
        assert_eq!(
            store.event_count(),
            2,
            "Should load 2 valid events, skip 2 malformed lines"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1-E: write_step_checkpoint uses fsync before rename.
    /// Simulates power loss by truncating the temp file to 0 bytes after write
    /// but before rename. The read path must fall back to the previous checkpoint.
    /// P1-E: Orphaned temp files (from interrupted writes) must be ignored
    /// by the checkpoint reader, falling back to the previous valid checkpoint.
    #[test]
    fn orphaned_temp_file_ignored_by_reader() {
        let session_id = format!("test-fsync-{}", std::process::id());
        let dir = checkpoint_dir_for(TEST_USER_ID, &session_id).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Write a valid checkpoint first.
        let heavy = make_heavy("step-valid", vec![]);
        let cp = StepCheckpoint::Heavy(Box::new(heavy));
        let json_str = checkpoint_json_for_test(&session_id, &cp);
        std::fs::write(dir.join("000001-heavy.json"), &json_str).unwrap();

        // Simulate power loss: a corrupted temp file left behind (never renamed)
        // This represents a crash after write but before rename.
        std::fs::write(dir.join(".tmp-000002-heavy.json"), b"").unwrap();

        // The read path must return the valid checkpoint, ignoring the temp file
        let result = read_latest_heavy_checkpoint(TEST_USER_ID, &session_id);
        let cp = result
            .expect("must succeed")
            .expect("must find valid checkpoint");
        assert_eq!(
            cp.light.step_id, "step-valid",
            "must return valid checkpoint, not be confused by orphaned temp file"
        );

        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    // =======================================================================
    // Regression: checkpoint_dir_for must NOT panic on invalid session_id (C2)
    // =======================================================================

    #[test]
    fn checkpoint_dir_for_returns_err_on_traversal_session_id() {
        // Regression: checkpoint_dir_for used .expect() which would panic
        // on invalid session_id (e.g. path traversal). Must return Err.
        let result = checkpoint_dir_for(TEST_USER_ID, "../../etc/passwd");
        assert!(
            result.is_err(),
            "checkpoint_dir_for must return Err for path-traversal session_id, \
             got Ok({:?})",
            result
        );
    }

    #[test]
    fn checkpoint_dir_for_returns_err_on_empty_session_id() {
        let result = checkpoint_dir_for(TEST_USER_ID, "");
        assert!(
            result.is_err(),
            "checkpoint_dir_for must return Err for empty session_id, got Ok({:?})",
            result
        );
    }

    #[test]
    fn checkpoint_dir_for_returns_err_on_empty_user_id() {
        let result = checkpoint_dir_for("", "session-without-owner");
        assert!(
            result.is_err(),
            "checkpoint_dir_for must return Err for empty user_id, got Ok({:?})",
            result
        );
    }

    #[test]
    fn write_step_checkpoint_returns_err_on_invalid_session_id() {
        let light = make_light("step-invalid-id", 1.0);
        let cp = StepCheckpoint::Light(light);
        let result = write_step_checkpoint(TEST_USER_ID, "../../etc/passwd", 1, &cp);
        assert!(
            result.is_err(),
            "write_step_checkpoint must return Err for invalid session_id, got Ok"
        );
    }

    #[test]
    fn read_latest_heavy_checkpoint_returns_err_on_invalid_session_id() {
        let result = read_latest_heavy_checkpoint(TEST_USER_ID, "../../etc/passwd");
        assert!(
            result.is_err(),
            "read_latest_heavy_checkpoint must return Err for invalid session_id, got Ok"
        );
    }

    #[test]
    fn read_latest_heavy_checkpoint_ignores_unversioned_raw_payload() {
        let session_id = format!("test-unversioned-heavy-{}", std::process::id());
        let dir = checkpoint_dir_for(TEST_USER_ID, &session_id).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let checkpoint = StepCheckpoint::Heavy(Box::new(make_heavy(
            "step-plaintext",
            vec![json!({"role":"user","content":"hi"})],
        )));
        let path = dir.join("000099-heavy.json");
        std::fs::write(&path, serde_json::to_string(&checkpoint).unwrap()).unwrap();

        let result = read_latest_heavy_checkpoint(TEST_USER_ID, &session_id).unwrap();
        assert!(
            result.is_none(),
            "raw payload without an owner/version envelope must not be treated as a checkpoint"
        );

        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn read_latest_light_checkpoint_ignores_unversioned_raw_payload() {
        let session_id = format!("test-unversioned-light-{}", std::process::id());
        let dir = checkpoint_dir_for(TEST_USER_ID, &session_id).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let checkpoint = StepCheckpoint::Light(make_light("step-plaintext", 0.25));
        let path = dir.join("000099-light.json");
        std::fs::write(&path, serde_json::to_string(&checkpoint).unwrap()).unwrap();

        let result = read_latest_light_checkpoint(TEST_USER_ID, &session_id).unwrap();
        assert!(
            result.is_none(),
            "raw payload without an owner/version envelope must not be treated as a checkpoint"
        );

        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn read_composite_snapshot_index_rejects_unversioned_raw_payload() {
        let session_id = format!("test-unversioned-composite-{}", std::process::id());
        let dir = checkpoint_dir_for(TEST_USER_ID, &session_id).unwrap();
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join("composite_snapshots.json");
        std::fs::write(&path, r#"{"snapshots":[]}"#).unwrap();

        let error = read_composite_snapshot_index(TEST_USER_ID, &session_id)
            .expect_err("raw index without an owner/version envelope must not be accepted");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn read_latest_heavy_checkpoint_rejects_owner_mismatch_in_file_body() {
        let session_id = format!("test-owner-mismatch-{}", std::process::id());
        let dir = checkpoint_dir_for(TEST_USER_ID, &session_id).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let checkpoint = StepCheckpoint::Heavy(Box::new(make_heavy("step-owner-mismatch", vec![])));
        let json = encode_versioned_step_artifact(
            STEP_CHECKPOINT_ARTIFACT_KIND,
            "other-user",
            &session_id,
            &checkpoint,
        )
        .unwrap();
        std::fs::write(dir.join("000001-heavy.json"), json).unwrap();

        let error = read_latest_heavy_checkpoint(TEST_USER_ID, &session_id)
            .expect_err("owner mismatch in file body must fail closed");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);

        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }
}
