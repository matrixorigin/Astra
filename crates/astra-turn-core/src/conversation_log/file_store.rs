//! JSONL-backed [`CslStore`] for edge/CLI deployments.
//!
//! Each session's log is stored as one JSON object per line in
//! `{base_dir}/{session_id}/conversation_log.jsonl`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use async_trait::async_trait;
use dashmap::DashMap;

use super::{
    CslEntry, CslStore, CslStoreError, MaterializedState, materialize, validate_session_id,
};

const TAIL_CHUNK_BYTES: usize = 64 * 1024;
const LOG_FILENAME: &str = "conversation_log.jsonl";
static APPEND_LOCKS: LazyLock<DashMap<PathBuf, Arc<Mutex<()>>>> = LazyLock::new(DashMap::new);

/// File-backed CSL store. Each session gets a JSONL file under `base_dir`.
#[derive(Debug, Clone)]
pub struct FileCslStore {
    base_dir: PathBuf,
}

impl FileCslStore {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    fn log_path(&self, session_id: &str) -> PathBuf {
        self.base_dir.join(session_id).join(LOG_FILENAME)
    }

    /// Load canonical conversation state from the local file store in a
    /// synchronous caller. The blocking contract is explicit so async/UI
    /// callers can move it to `spawn_blocking`; one-shot and app-server
    /// continuation use it during request preparation.
    pub fn load_materialized_blocking(
        &self,
        session_id: &str,
    ) -> Result<Option<MaterializedState>, CslStoreError> {
        validate_session_id(session_id)?;
        let entries = Self::read_latest_snapshot(&self.log_path(session_id))?;
        if entries.is_empty() {
            return Ok(None);
        }
        materialize(&entries).map(Some).map_err(Into::into)
    }

    fn read_latest_snapshot(path: &Path) -> Result<Vec<CslEntry>, CslStoreError> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        read_snapshot_suffix(file, path)
    }

    /// Read all entries from the JSONL file. Returns empty vec if file doesn't exist.
    /// Gracefully skips a corrupted trailing line (e.g. from a crash mid-write).
    fn read_all_entries(path: &Path) -> Result<Vec<CslEntry>, CslStoreError> {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        if astra_core::history_work::instrumentation_enabled() {
            astra_core::history_work::record_bytes(
                astra_core::history_work::HistoryWorkSite::CslFileRead,
                content.len().try_into().unwrap_or(u64::MAX),
            );
        }
        let mut entries = Vec::new();
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        for (idx, line) in lines.into_iter().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<CslEntry>(line) {
                Ok(entry) => entries.push(entry),
                Err(e) if idx == total - 1 => {
                    tracing::warn!(
                        path = %path.display(),
                        "skipping corrupted trailing JSONL line: {e}"
                    );
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(entries)
    }

    /// Append a single entry to the JSONL file with fsync.
    fn append_entry(path: &Path, entry: &CslEntry) -> Result<(), CslStoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let lock = APPEND_LOCKS
            .entry(path.to_path_buf())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .value()
            .clone();
        let _guard = lock
            .lock()
            .map_err(|_| CslStoreError::Other("conversation log append lock poisoned".into()))?;
        let entries = Self::read_all_entries(path)?;
        validate_append_sequence(&entries, entry)?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let mut line = serde_json::to_string(entry)?;
        line.push('\n');
        if astra_core::history_work::instrumentation_enabled() {
            astra_core::history_work::record_bytes(
                astra_core::history_work::HistoryWorkSite::CslFileAppendSerialization,
                line.len().try_into().unwrap_or(u64::MAX),
            );
        }
        file.write_all(line.as_bytes())?;
        file.sync_data()?;
        Ok(())
    }

    /// Rewrite the JSONL file with only the given entries (for truncation/GC).
    fn rewrite(path: &Path, entries: &[CslEntry]) -> Result<u64, CslStoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Atomic: write to temp file, then rename.
        let tmp = path.with_extension("jsonl.tmp");
        let mut serialized_bytes = 0_u64;
        {
            let mut file = std::fs::File::create(&tmp)?;
            // Copy permissions from the original BEFORE writing payload so the
            // healed file never exposes sensitive content with looser permissions.
            if let Ok(meta) = std::fs::metadata(path) {
                let _ = std::fs::set_permissions(&tmp, meta.permissions());
            }
            for entry in entries {
                let mut line = serde_json::to_string(entry)?;
                line.push('\n');
                serialized_bytes =
                    serialized_bytes.saturating_add(line.len().try_into().unwrap_or(u64::MAX));
                file.write_all(line.as_bytes())?;
            }
            file.sync_data()?;
        }
        std::fs::rename(&tmp, path)?;
        if astra_core::history_work::instrumentation_enabled() {
            astra_core::history_work::record_bytes(
                astra_core::history_work::HistoryWorkSite::CslFileRewriteSerialization,
                serialized_bytes,
            );
        }
        Ok(serialized_bytes)
    }
}

/// Read backward from a fixed EOF, stopping at the latest snapshot. Replay
/// only depends on that snapshot and its suffix. Full-history operations
/// continue to check every row, including records superseded by the snapshot.
fn read_snapshot_suffix(
    mut file: impl Read + Seek,
    path: &Path,
) -> Result<Vec<CslEntry>, CslStoreError> {
    let end = file.seek(SeekFrom::End(0))?;
    let mut offset = end;
    let mut chunk = vec![0; end.min(TAIL_CHUNK_BYTES as u64) as usize];
    let mut reversed_line = Vec::new();
    let mut entries = Vec::new();
    let mut last_line = true;
    let result = (|| {
        while offset > 0 {
            let size = offset.min(TAIL_CHUNK_BYTES as u64) as usize;
            offset -= size as u64;
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut chunk[..size])?;

            let mut remaining = &chunk[..size];
            loop {
                let newline = remaining.iter().rposition(|byte| *byte == b'\n');
                let start = newline.map_or(0, |pos| pos + 1);
                reversed_line.extend(remaining[start..].iter().rev().copied());
                if newline.is_none() && offset != 0 {
                    break;
                }
                // A separator at EOF does not create another physical line.
                if offset + start as u64 != end {
                    reversed_line.reverse();
                    let line = std::str::from_utf8(&reversed_line)
                        .map_err(|error| {
                            std::io::Error::new(std::io::ErrorKind::InvalidData, error)
                        })?
                        .trim();
                    if !line.is_empty() {
                        match serde_json::from_str::<CslEntry>(line) {
                            Ok(entry) => {
                                let is_snapshot = entry.is_snapshot();
                                entries.push(entry);
                                if is_snapshot {
                                    entries.reverse();
                                    return Ok(entries);
                                }
                            }
                            Err(error) if last_line => {
                                tracing::warn!(path = %path.display(), "skipping corrupted trailing JSONL line: {error}");
                            }
                            Err(error) => return Err(error.into()),
                        }
                    }
                    last_line = false;
                }
                reversed_line.clear();
                let Some(pos) = newline else { break };
                remaining = &remaining[..pos];
            }
        }
        // Without a snapshot, there is no reconstructible state.
        Ok(Vec::new())
    })();
    if astra_core::history_work::instrumentation_enabled() {
        astra_core::history_work::record_bytes(
            astra_core::history_work::HistoryWorkSite::CslFileRead,
            end - offset,
        );
    }
    result
}

fn validate_append_sequence(entries: &[CslEntry], entry: &CslEntry) -> Result<(), CslStoreError> {
    let Some(current_seq) = entries.iter().map(CslEntry::seq).max() else {
        if entry.is_snapshot() {
            return Ok(());
        }
        return Err(CslStoreError::Other(
            "conversation log must begin with a snapshot".to_string(),
        ));
    };

    let expected_seq = current_seq.checked_add(1).ok_or_else(|| {
        CslStoreError::Other("conversation log seq overflow at append boundary".to_string())
    })?;
    if entry.seq() != expected_seq {
        return Err(CslStoreError::Other(format!(
            "stale conversation log append: expected seq {expected_seq} after current seq {current_seq}, got {}",
            entry.seq()
        )));
    }
    Ok(())
}

#[async_trait]
impl CslStore for FileCslStore {
    async fn append(&self, session_id: &str, entry: &CslEntry) -> Result<(), CslStoreError> {
        validate_session_id(session_id)?;
        let path = self.log_path(session_id);
        let entry = entry.clone();
        tokio::task::spawn_blocking(move || Self::append_entry(&path, &entry))
            .await
            .map_err(|e| CslStoreError::Other(format!("join error: {e}")))?
    }

    async fn load_from_latest_snapshot(
        &self,
        session_id: &str,
    ) -> Result<Vec<CslEntry>, CslStoreError> {
        validate_session_id(session_id)?;
        let path = self.log_path(session_id);
        tokio::task::spawn_blocking(move || Self::read_latest_snapshot(&path))
            .await
            .map_err(|e| CslStoreError::Other(format!("join error: {e}")))?
    }

    async fn load_after(
        &self,
        session_id: &str,
        after_seq: u64,
    ) -> Result<Vec<CslEntry>, CslStoreError> {
        validate_session_id(session_id)?;
        let path = self.log_path(session_id);
        let entries = tokio::task::spawn_blocking(move || Self::read_all_entries(&path))
            .await
            .map_err(|e| CslStoreError::Other(format!("join error: {e}")))??;
        Ok(entries
            .into_iter()
            .filter(|e| e.seq() > after_seq)
            .collect())
    }

    async fn truncate_before(
        &self,
        session_id: &str,
        before_seq: u64,
    ) -> Result<u64, CslStoreError> {
        validate_session_id(session_id)?;
        let path = self.log_path(session_id);
        tokio::task::spawn_blocking(move || {
            let entries = Self::read_all_entries(&path)?;
            let kept: Vec<_> = entries
                .iter()
                .filter(|e| e.seq() >= before_seq)
                .cloned()
                .collect();
            let removed = entries.len() as u64 - kept.len() as u64;
            if removed > 0 {
                let _ = Self::rewrite(&path, &kept)?;
            }
            Ok::<u64, CslStoreError>(removed)
        })
        .await
        .map_err(|e| CslStoreError::Other(format!("join error: {e}")))?
    }

    async fn fork(
        &self,
        parent_session_id: &str,
        new_session_id: &str,
        fork_after_turn: u32,
    ) -> Result<u64, CslStoreError> {
        validate_session_id(parent_session_id)?;
        validate_session_id(new_session_id)?;
        let parent_path = self.log_path(parent_session_id);
        let new_path = self.log_path(new_session_id);

        tokio::task::spawn_blocking(move || {
            let entries = Self::read_all_entries(&parent_path)?;
            if entries.is_empty() {
                return Ok(0);
            }

            // Collect entries up to fork_after_turn.
            let relevant: Vec<_> = entries
                .iter()
                .filter(|e| e.turn() <= fork_after_turn)
                .cloned()
                .collect();

            if relevant.is_empty() {
                return Ok(0);
            }

            // Materialize state at fork point, then write as a single Snapshot.
            let mat = materialize(&relevant)?;
            let prefix_rows = mat.messages.len().try_into().unwrap_or(u64::MAX);
            let fork_snapshot = CslEntry::Snapshot {
                seq: 1,
                turn: mat.last_turn,
                messages: mat.messages,
                session_state: mat.session_state,
            };

            let serialized_bytes = Self::rewrite(&new_path, &[fork_snapshot])?;
            if astra_core::history_work::instrumentation_enabled() {
                astra_core::history_work::record_operation(
                    astra_core::history_work::HistoryWorkSite::ForkPrefixMaterialization,
                    serialized_bytes,
                    prefix_rows,
                    0,
                );
            }
            Ok::<u64, CslStoreError>(1)
        })
        .await
        .map_err(|e| CslStoreError::Other(format!("join error: {e}")))?
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_log::{SessionStateCompact, materialize};
    use serde_json::json;
    use tempfile::TempDir;

    fn user_msg(content: &str) -> serde_json::Value {
        json!({"role": "user", "content": content})
    }

    fn assistant_msg(content: &str) -> serde_json::Value {
        json!({"role": "assistant", "content": content})
    }

    fn tool_result_msg(id: &str, content: &str) -> serde_json::Value {
        json!({"role": "tool", "tool_call_id": id, "content": content})
    }

    fn make_snapshot(seq: u64, turn: u32, msgs: Vec<serde_json::Value>) -> CslEntry {
        CslEntry::Snapshot {
            seq,
            turn,
            messages: msgs,
            session_state: SessionStateCompact::default(),
        }
    }

    fn make_delta(seq: u64, turn: u32, appended: Vec<serde_json::Value>) -> CslEntry {
        CslEntry::TurnDelta {
            seq,
            turn,
            appended,
            state_patch: None,
        }
    }

    #[tokio::test]
    async fn append_and_load_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let store = FileCslStore::new(tmp.path());
        let sid = "sess-1";

        let snap = make_snapshot(0, 1, vec![user_msg("hello")]);
        store.append(sid, &snap).await.unwrap();

        let delta = make_delta(1, 2, vec![user_msg("turn2"), assistant_msg("resp2")]);
        store.append(sid, &delta).await.unwrap();

        let entries = store.load_from_latest_snapshot(sid).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_snapshot());
        assert!(!entries[1].is_snapshot());

        let state = materialize(&entries).unwrap();
        assert_eq!(state.messages.len(), 3);
        assert_eq!(state.messages[0]["content"], "hello");
        assert_eq!(state.messages[2]["content"], "resp2");
    }

    #[test]
    fn blocking_materialized_load_returns_latest_canonical_state() {
        let tmp = TempDir::new().unwrap();
        let store = FileCslStore::new(tmp.path());
        let sid = "blocking-load";
        let path = store.log_path(sid);
        FileCslStore::append_entry(&path, &make_snapshot(1, 1, vec![user_msg("first")])).unwrap();
        FileCslStore::append_entry(&path, &make_delta(2, 2, vec![assistant_msg("second")]))
            .unwrap();

        let materialized = store
            .load_materialized_blocking(sid)
            .unwrap()
            .expect("canonical state");

        assert_eq!(materialized.last_seq, 2);
        assert_eq!(materialized.last_turn, 2);
        assert_eq!(materialized.messages.len(), 2);
        assert_eq!(materialized.messages[0]["content"], "first");
        assert_eq!(materialized.messages[1]["content"], "second");
    }

    #[test]
    fn rewrite_returns_the_exact_bytes_already_written() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("rewrite-bytes.jsonl");
        let entries = [
            make_snapshot(1, 1, vec![user_msg("first")]),
            make_delta(2, 2, vec![assistant_msg("second")]),
        ];

        let serialized_bytes = FileCslStore::rewrite(&path, &entries).unwrap();

        assert_eq!(
            serialized_bytes,
            std::fs::metadata(path).unwrap().len(),
            "fork instrumentation must reuse rewrite's existing byte count"
        );
    }

    #[tokio::test]
    async fn load_from_latest_snapshot_skips_old() {
        let tmp = TempDir::new().unwrap();
        let store = FileCslStore::new(tmp.path());
        let sid = "sess-2";

        // Two snapshots with deltas between.
        store
            .append(sid, &make_snapshot(0, 1, vec![user_msg("old")]))
            .await
            .unwrap();
        store
            .append(sid, &make_delta(1, 2, vec![user_msg("delta_old")]))
            .await
            .unwrap();
        store
            .append(sid, &make_snapshot(2, 3, vec![user_msg("compacted")]))
            .await
            .unwrap();
        store
            .append(sid, &make_delta(3, 4, vec![user_msg("new")]))
            .await
            .unwrap();

        let entries = store.load_from_latest_snapshot(sid).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq(), 2);
        assert_eq!(entries[1].seq(), 3);

        let state = materialize(&entries).unwrap();
        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.messages[0]["content"], "compacted");
        assert_eq!(state.messages[1]["content"], "new");
    }

    #[tokio::test]
    async fn load_nonexistent_session_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let store = FileCslStore::new(tmp.path());
        let entries = store.load_from_latest_snapshot("nope").await.unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn load_after_filters_by_seq() {
        let tmp = TempDir::new().unwrap();
        let store = FileCslStore::new(tmp.path());
        let sid = "sess-3";

        store
            .append(sid, &make_snapshot(0, 1, vec![]))
            .await
            .unwrap();
        store
            .append(sid, &make_delta(1, 2, vec![user_msg("a")]))
            .await
            .unwrap();
        store
            .append(sid, &make_delta(2, 3, vec![user_msg("b")]))
            .await
            .unwrap();

        let after = store.load_after(sid, 1).await.unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].seq(), 2);
    }

    #[tokio::test]
    async fn truncate_before_removes_old_entries() {
        let tmp = TempDir::new().unwrap();
        let store = FileCslStore::new(tmp.path());
        let sid = "sess-4";

        store
            .append(sid, &make_snapshot(0, 1, vec![user_msg("old")]))
            .await
            .unwrap();
        store
            .append(sid, &make_delta(1, 2, vec![user_msg("mid")]))
            .await
            .unwrap();
        store
            .append(sid, &make_snapshot(2, 3, vec![user_msg("new_snap")]))
            .await
            .unwrap();
        store
            .append(sid, &make_delta(3, 4, vec![user_msg("latest")]))
            .await
            .unwrap();

        let removed = store.truncate_before(sid, 2).await.unwrap();
        assert_eq!(removed, 2);

        let entries = store.load_from_latest_snapshot(sid).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq(), 2);
        assert_eq!(entries[1].seq(), 3);
    }

    #[tokio::test]
    async fn fork_creates_materialized_snapshot() {
        let tmp = TempDir::new().unwrap();
        let store = FileCslStore::new(tmp.path());
        let parent = "parent-sess";

        store
            .append(
                parent,
                &make_snapshot(0, 1, vec![user_msg("t1"), assistant_msg("r1")]),
            )
            .await
            .unwrap();
        store
            .append(
                parent,
                &make_delta(1, 2, vec![user_msg("t2"), assistant_msg("r2")]),
            )
            .await
            .unwrap();
        store
            .append(
                parent,
                &make_delta(2, 3, vec![user_msg("t3"), assistant_msg("r3")]),
            )
            .await
            .unwrap();

        // Fork at turn 2 — should include turn 1 and 2 but not 3.
        let count = store.fork(parent, "child-sess", 2).await.unwrap();
        assert_eq!(count, 1);

        let entries = store.load_from_latest_snapshot("child-sess").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_snapshot());

        let state = materialize(&entries).unwrap();
        assert_eq!(state.messages.len(), 4); // t1, r1, t2, r2
        assert_eq!(state.messages[0]["content"], "t1");
        assert_eq!(state.messages[3]["content"], "r2");
    }

    #[tokio::test]
    async fn fork_empty_parent_returns_zero() {
        let tmp = TempDir::new().unwrap();
        let store = FileCslStore::new(tmp.path());
        let count = store.fork("empty", "child", 5).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn fork_preserves_tool_results() {
        let tmp = TempDir::new().unwrap();
        let store = FileCslStore::new(tmp.path());
        let parent = "tool-parent";

        store
            .append(parent, &make_snapshot(0, 1, vec![user_msg("read file")]))
            .await
            .unwrap();
        store
            .append(
                parent,
                &make_delta(
                    1,
                    1,
                    vec![tool_result_msg("c1", "fn main() {}"), assistant_msg("done")],
                ),
            )
            .await
            .unwrap();

        store.fork(parent, "tool-child", 1).await.unwrap();

        let entries = store.load_from_latest_snapshot("tool-child").await.unwrap();
        let state = materialize(&entries).unwrap();
        assert_eq!(state.messages.len(), 3);
        assert_eq!(state.messages[1]["role"], "tool");
        assert_eq!(state.messages[1]["content"], "fn main() {}");
    }

    #[tokio::test]
    async fn multiple_appends_accumulate() {
        let tmp = TempDir::new().unwrap();
        let store = FileCslStore::new(tmp.path());
        let sid = "accum";

        store
            .append(sid, &make_snapshot(0, 0, vec![]))
            .await
            .unwrap();
        for i in 1..=10u64 {
            store
                .append(
                    sid,
                    &make_delta(i, i as u32, vec![user_msg(&format!("t{i}"))]),
                )
                .await
                .unwrap();
        }

        let entries = store.load_from_latest_snapshot(sid).await.unwrap();
        assert_eq!(entries.len(), 11);
        let state = materialize(&entries).unwrap();
        assert_eq!(state.messages.len(), 10);
    }

    #[tokio::test]
    async fn append_rejects_delta_before_first_snapshot() {
        let tmp = TempDir::new().unwrap();
        let store = FileCslStore::new(tmp.path());
        let sid = "deltas-only";

        let err = store
            .append(sid, &make_delta(0, 1, vec![user_msg("orphan1")]))
            .await
            .expect_err("orphan deltas must be rejected at append time");

        assert!(
            err.to_string().contains("must begin with a snapshot"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn fork_preserves_state_patch() {
        use crate::conversation_log::SessionStatePatch;

        let tmp = TempDir::new().unwrap();
        let store = FileCslStore::new(tmp.path());
        let parent = "patch-parent";

        store
            .append(parent, &make_snapshot(0, 1, vec![user_msg("hi")]))
            .await
            .unwrap();
        store
            .append(
                parent,
                &CslEntry::TurnDelta {
                    seq: 1,
                    turn: 2,
                    appended: vec![user_msg("turn2")],
                    state_patch: Some(SessionStatePatch {
                        blocked_tools: Some(vec!["bash".into(), "write".into()]),
                        recent_tools: Some(vec!["read_file".into()]),
                        approval_overrides: Some(Some(json!({"tool": "bash", "approved": true}))),
                        ..Default::default()
                    }),
                },
            )
            .await
            .unwrap();

        store.fork(parent, "patch-child", 2).await.unwrap();

        let entries = store
            .load_from_latest_snapshot("patch-child")
            .await
            .unwrap();
        let state = materialize(&entries).unwrap();
        assert_eq!(state.session_state.blocked_tools, vec!["bash", "write"]);
        assert_eq!(state.session_state.recent_tools, vec!["read_file"]);
        assert_eq!(
            state.session_state.approval_overrides,
            Some(json!({"tool": "bash", "approved": true}))
        );
    }

    /// Simulates the runtime CSL lifecycle:
    ///   Turn 1: persist snapshot + delta with state_patch
    ///   Turn 2: load → materialize → verify history + state restored
    ///   Turn 2: persist delta
    ///   Turn 3: load → verify both turns visible
    #[tokio::test]
    async fn csl_multi_turn_persist_load_roundtrip() {
        use crate::conversation_log::{SessionStateCompact, SessionStatePatch};

        let tmp = TempDir::new().unwrap();
        let store = FileCslStore::new(tmp.path());
        let sid = "lifecycle";

        // ── Turn 1: initial snapshot (session start) ──
        let snap = CslEntry::Snapshot {
            seq: 0,
            turn: 1,
            messages: vec![user_msg("hello"), assistant_msg("hi there")],
            session_state: SessionStateCompact {
                budget_remaining_tokens: 100_000,
                budget_remaining_rounds: 10,
                ..Default::default()
            },
        };
        store.append(sid, &snap).await.unwrap();

        // ── Turn 2: load CSL → materialize → simulate loop → persist delta ──
        let entries = store.load_from_latest_snapshot(sid).await.unwrap();
        let mat = materialize(&entries).unwrap();
        assert_eq!(mat.messages.len(), 2);
        assert_eq!(mat.session_state.budget_remaining_tokens, 0);
        assert_eq!(mat.last_seq, 0);

        // Simulate: loop ran, produced new messages and changed state
        let delta = CslEntry::TurnDelta {
            seq: mat.last_seq + 1,
            turn: 2,
            appended: vec![user_msg("turn2"), assistant_msg("resp2")],
            state_patch: Some(SessionStatePatch {
                blocked_tools: Some(vec!["bash".into()]),
                budget_remaining_tokens: Some(80_000),
                budget_remaining_rounds: Some(9),
                ..Default::default()
            }),
        };
        store.append(sid, &delta).await.unwrap();

        // ── Turn 3: load → verify turn 2 is visible ──
        let entries = store.load_from_latest_snapshot(sid).await.unwrap();
        let mat = materialize(&entries).unwrap();
        assert_eq!(mat.messages.len(), 4); // 2 from snap + 2 from delta
        assert_eq!(mat.messages[0]["content"], "hello");
        assert_eq!(mat.messages[3]["content"], "resp2");
        assert_eq!(mat.session_state.blocked_tools, vec!["bash"]);
        assert_eq!(mat.session_state.budget_remaining_tokens, 0);
        assert_eq!(mat.session_state.budget_remaining_rounds, 0);
        assert_eq!(mat.last_seq, 1);

        // Persist turn 3 delta
        let delta2 = CslEntry::TurnDelta {
            seq: mat.last_seq + 1,
            turn: 3,
            appended: vec![
                user_msg("turn3"),
                tool_result_msg("c1", "file contents"),
                assistant_msg("resp3"),
            ],
            state_patch: Some(SessionStatePatch {
                recent_tools: Some(vec!["read_file".into()]),
                consecutive_ctx_errors: Some(1),
                ..Default::default()
            }),
        };
        store.append(sid, &delta2).await.unwrap();

        // ── Final verify ──
        let entries = store.load_from_latest_snapshot(sid).await.unwrap();
        let mat = materialize(&entries).unwrap();
        assert_eq!(mat.messages.len(), 7);
        assert_eq!(mat.last_seq, 2);
        assert_eq!(mat.last_turn, 3);
        // State accumulated across turns
        assert_eq!(mat.session_state.blocked_tools, vec!["bash"]);
        assert_eq!(mat.session_state.recent_tools, vec!["read_file"]);
        assert_eq!(mat.session_state.budget_remaining_tokens, 0);
        assert_eq!(mat.session_state.consecutive_ctx_errors, 1);
        // Tool results preserved
        assert_eq!(mat.messages[4]["role"], "user");
        assert_eq!(mat.messages[5]["role"], "tool");
        assert_eq!(mat.messages[5]["content"], "file contents");
    }

    /// Verify that snapshot at turn N captures full state, and subsequent
    /// load only returns from that snapshot onward.
    #[tokio::test]
    async fn snapshot_compaction_resets_load_window() {
        use crate::conversation_log::SessionStateCompact;

        let tmp = TempDir::new().unwrap();
        let store = FileCslStore::new(tmp.path());
        let sid = "compaction";

        // 5 turns of deltas, then a compaction snapshot
        store
            .append(
                sid,
                &CslEntry::Snapshot {
                    seq: 0,
                    turn: 1,
                    messages: vec![user_msg("t1")],
                    session_state: SessionStateCompact::default(),
                },
            )
            .await
            .unwrap();
        for i in 1..=4u64 {
            store
                .append(
                    sid,
                    &make_delta(i, (i + 1) as u32, vec![user_msg(&format!("t{}", i + 1))]),
                )
                .await
                .unwrap();
        }
        // Compaction snapshot at turn 5
        store
            .append(
                sid,
                &CslEntry::Snapshot {
                    seq: 5,
                    turn: 5,
                    messages: vec![user_msg("compacted_summary")],
                    session_state: SessionStateCompact {
                        budget_remaining_tokens: 70_000,
                        budget_remaining_rounds: 6,
                        blocked_tools: vec!["bash".into()],
                        ..Default::default()
                    },
                },
            )
            .await
            .unwrap();
        // One more delta after compaction
        store
            .append(sid, &make_delta(6, 6, vec![user_msg("t6")]))
            .await
            .unwrap();

        let entries = store.load_from_latest_snapshot(sid).await.unwrap();
        // Should only return snapshot at seq=5 + delta at seq=6
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq(), 5);

        let mat = materialize(&entries).unwrap();
        assert_eq!(mat.messages.len(), 2);
        assert_eq!(mat.messages[0]["content"], "compacted_summary");
        assert_eq!(mat.session_state.budget_remaining_tokens, 0);
        assert_eq!(mat.session_state.blocked_tools, vec!["bash"]);
    }

    // ── Path traversal protection ──────────────────────────────────────

    #[tokio::test]
    async fn rejects_path_traversal_session_ids() {
        let tmp = TempDir::new().unwrap();
        let store = FileCslStore::new(tmp.path());
        let snap = make_snapshot(0, 1, vec![user_msg("hi")]);

        let long_id = "a".repeat(201);
        let cases: Vec<&str> = vec![
            "../etc/passwd",
            "foo/bar",
            "a\\b",
            "..",
            "",
            "   ",
            ".",
            "has\0nul",
            "has\nnewline",
            "has\ttab",
            "has\x7Fdel",
            "café",
            "abc\u{200B}def",
            &long_id,
        ];
        for bad_id in cases {
            let result = store.append(bad_id, &snap).await;
            match result {
                Err(CslStoreError::InvalidSessionId(_)) => {}
                Err(other) => panic!("'{bad_id}': expected InvalidSessionId, got {other}"),
                Ok(_) => panic!("session_id '{bad_id}' should be rejected"),
            }
        }
    }

    #[tokio::test]
    async fn accepts_valid_session_ids() {
        let tmp = TempDir::new().unwrap();
        let store = FileCslStore::new(tmp.path());
        let snap = make_snapshot(0, 1, vec![user_msg("hi")]);

        for good_id in [
            "abc123",
            "550e8400-e29b-41d4-a716-446655440000",
            "session_with-dashes.and.dots",
        ] {
            let result = store.append(good_id, &snap).await;
            assert!(
                result.is_ok(),
                "session_id '{good_id}' should be accepted: {:?}",
                result.err()
            );
        }
    }

    #[tokio::test]
    async fn snapshot_reads_stop_at_latest_snapshot_across_chunks() {
        let tmp = TempDir::new().unwrap();
        let store = FileCslStore::new(tmp.path());
        let sid = "long-history";
        let path = store.log_path(sid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let snapshot = make_snapshot(100, 100, vec![user_msg(&"界".repeat(TAIL_CHUNK_BYTES))]);
        let delta = make_delta(101, 101, vec![assistant_msg("recent")]);
        let mut file = File::create(&path).unwrap();
        // A full read would reach malformed historical data. The valid recent
        // snapshot is sufficient to reconstruct every fact in the suffix.
        writeln!(file, "{{obsolete damaged record").unwrap();
        for entry in [&snapshot, &delta] {
            writeln!(file, "{}", serde_json::to_string(entry).unwrap()).unwrap();
        }
        drop(file);
        assert_eq!(
            store.load_from_latest_snapshot(sid).await.unwrap(),
            vec![snapshot, delta]
        );
        assert!(store.load_after(sid, 0).await.is_err());
        assert_eq!(
            store
                .load_materialized_blocking(sid)
                .unwrap()
                .unwrap()
                .last_seq,
            101
        );
    }

    #[test]
    fn latest_snapshot_reads_one_chunk_with_a_long_superseded_prefix() {
        struct CountingReader {
            file: File,
            bytes_read: usize,
        }

        impl Read for CountingReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let count = self.file.read(buf)?;
                self.bytes_read += count;
                Ok(count)
            }
        }

        impl Seek for CountingReader {
            fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
                self.file.seek(pos)
            }
        }

        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(LOG_FILENAME);
        for count in [100, 10_000, 100_000] {
            let mut file = std::io::BufWriter::new(File::create(&path).unwrap());
            let old_message = user_msg(&"a".repeat(if count == 100 { 1024 } else { 32 }));
            writeln!(
                file,
                "{}",
                serde_json::to_string(&make_snapshot(0, 0, vec![])).unwrap()
            )
            .unwrap();
            for seq in 1..=count {
                let delta = make_delta(seq, 1, vec![old_message.clone()]);
                writeln!(file, "{}", serde_json::to_string(&delta).unwrap()).unwrap();
            }
            let snapshot = make_snapshot(count + 1, 2, vec![user_msg("compacted")]);
            writeln!(file, "{}", serde_json::to_string(&snapshot).unwrap()).unwrap();
            file.flush().unwrap();
            drop(file);

            let mut reader = CountingReader {
                file: File::open(&path).unwrap(),
                bytes_read: 0,
            };
            assert_eq!(
                read_snapshot_suffix(&mut reader, &path).unwrap(),
                vec![snapshot]
            );
            assert_eq!(
                reader.bytes_read, TAIL_CHUNK_BYTES,
                "history length {count}"
            );
        }
    }

    // ── Corrupted trailing JSONL line ──────────────────────────────────

    #[tokio::test]
    async fn corrupted_trailing_line_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let store = FileCslStore::new(tmp.path());
        let sid = "corrupt-trailing";

        store
            .append(sid, &make_snapshot(0, 1, vec![user_msg("good")]))
            .await
            .unwrap();

        // Manually append a corrupted line to the JSONL file.
        let path = tmp.path().join(sid).join("conversation_log.jsonl");
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, "{{truncated garbage").unwrap();

        let entries = store.load_from_latest_snapshot(sid).await.unwrap();
        assert_eq!(entries.len(), 1, "should skip corrupted trailing line");
        assert!(entries[0].is_snapshot());
    }

    #[tokio::test]
    async fn corrupted_middle_line_is_error() {
        let tmp = TempDir::new().unwrap();
        let store = FileCslStore::new(tmp.path());
        let sid = "corrupt-middle";

        store
            .append(sid, &make_snapshot(0, 1, vec![user_msg("good")]))
            .await
            .unwrap();

        // Insert a corrupted line in the middle, then add a valid one after.
        let path = tmp.path().join(sid).join("conversation_log.jsonl");
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, "{{garbage").unwrap();
        let delta = make_delta(1, 2, vec![user_msg("after")]);
        let line = serde_json::to_string(&delta).unwrap();
        writeln!(f, "{line}").unwrap();

        let result = store.load_from_latest_snapshot(sid).await;
        assert!(
            result.is_err(),
            "corrupted non-trailing line should cause error"
        );
    }
}
