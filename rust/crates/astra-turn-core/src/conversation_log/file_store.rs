//! JSONL-backed [`CslStore`] for edge/CLI deployments.
//!
//! Each session's log is stored as one JSON object per line in
//! `{base_dir}/{session_id}/conversation_log.jsonl`.

use std::io::Write;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use super::{CslEntry, CslStore, CslStoreError, materialize, validate_session_id};

const LOG_FILENAME: &str = "conversation_log.jsonl";

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

    /// Read all entries from the JSONL file. Returns empty vec if file doesn't exist.
    /// Gracefully skips a corrupted trailing line (e.g. from a crash mid-write).
    fn read_all_entries(path: &Path) -> Result<Vec<CslEntry>, CslStoreError> {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
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
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let mut line = serde_json::to_string(entry)?;
        line.push('\n');
        file.write_all(line.as_bytes())?;
        file.sync_data()?;
        Ok(())
    }

    /// Rewrite the JSONL file with only the given entries (for truncation/GC).
    fn rewrite(path: &Path, entries: &[CslEntry]) -> Result<(), CslStoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Atomic: write to temp file, then rename.
        let tmp = path.with_extension("jsonl.tmp");
        {
            let mut file = std::fs::File::create(&tmp)?;
            for entry in entries {
                let mut line = serde_json::to_string(entry)?;
                line.push('\n');
                file.write_all(line.as_bytes())?;
            }
            file.sync_data()?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[async_trait]
impl CslStore for FileCslStore {
    async fn append(
        &self,
        session_id: &str,
        entry: &CslEntry,
        _meta: &super::AppendMeta,
    ) -> Result<(), CslStoreError> {
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
        let entries = tokio::task::spawn_blocking(move || Self::read_all_entries(&path))
            .await
            .map_err(|e| CslStoreError::Other(format!("join error: {e}")))??;

        if entries.is_empty() {
            return Ok(Vec::new());
        }

        // Find last snapshot index and return from there.
        // If no snapshot exists, return empty — materialize() requires a Snapshot
        // as the first entry, so returning orphan TurnDeltas would fail.
        let snapshot_idx = entries.iter().rposition(CslEntry::is_snapshot);
        match snapshot_idx {
            Some(idx) => Ok(entries[idx..].to_vec()),
            None => Ok(Vec::new()),
        }
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
                Self::rewrite(&path, &kept)?;
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
            let fork_snapshot = CslEntry::Snapshot {
                seq: 1,
                turn: mat.last_turn,
                messages: mat.messages,
                session_state: mat.session_state,
            };

            Self::rewrite(&new_path, &[fork_snapshot])?;
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
    use crate::conversation_log::{AppendMeta, SessionStateCompact, materialize};
    use serde_json::json;
    use tempfile::TempDir;

    fn meta() -> AppendMeta {
        AppendMeta::default()
    }

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
        store.append(sid, &snap, &meta()).await.unwrap();

        let delta = make_delta(1, 2, vec![user_msg("turn2"), assistant_msg("resp2")]);
        store.append(sid, &delta, &meta()).await.unwrap();

        let entries = store.load_from_latest_snapshot(sid).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_snapshot());
        assert!(!entries[1].is_snapshot());

        let state = materialize(&entries).unwrap();
        assert_eq!(state.messages.len(), 3);
        assert_eq!(state.messages[0]["content"], "hello");
        assert_eq!(state.messages[2]["content"], "resp2");
    }

    #[tokio::test]
    async fn load_from_latest_snapshot_skips_old() {
        let tmp = TempDir::new().unwrap();
        let store = FileCslStore::new(tmp.path());
        let sid = "sess-2";

        // Two snapshots with deltas between.
        store
            .append(sid, &make_snapshot(0, 1, vec![user_msg("old")]), &meta())
            .await
            .unwrap();
        store
            .append(sid, &make_delta(1, 2, vec![user_msg("delta_old")]), &meta())
            .await
            .unwrap();
        store
            .append(
                sid,
                &make_snapshot(2, 3, vec![user_msg("compacted")]),
                &meta(),
            )
            .await
            .unwrap();
        store
            .append(sid, &make_delta(3, 4, vec![user_msg("new")]), &meta())
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
            .append(sid, &make_snapshot(0, 1, vec![]), &meta())
            .await
            .unwrap();
        store
            .append(sid, &make_delta(1, 2, vec![user_msg("a")]), &meta())
            .await
            .unwrap();
        store
            .append(sid, &make_delta(2, 3, vec![user_msg("b")]), &meta())
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
            .append(sid, &make_snapshot(0, 1, vec![user_msg("old")]), &meta())
            .await
            .unwrap();
        store
            .append(sid, &make_delta(1, 2, vec![user_msg("mid")]), &meta())
            .await
            .unwrap();
        store
            .append(
                sid,
                &make_snapshot(2, 3, vec![user_msg("new_snap")]),
                &meta(),
            )
            .await
            .unwrap();
        store
            .append(sid, &make_delta(3, 4, vec![user_msg("latest")]), &meta())
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
                &meta(),
            )
            .await
            .unwrap();
        store
            .append(
                parent,
                &make_delta(1, 2, vec![user_msg("t2"), assistant_msg("r2")]),
                &meta(),
            )
            .await
            .unwrap();
        store
            .append(
                parent,
                &make_delta(2, 3, vec![user_msg("t3"), assistant_msg("r3")]),
                &meta(),
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
            .append(
                parent,
                &make_snapshot(0, 1, vec![user_msg("read file")]),
                &meta(),
            )
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
                &meta(),
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
            .append(sid, &make_snapshot(0, 0, vec![]), &meta())
            .await
            .unwrap();
        for i in 1..=10u64 {
            store
                .append(
                    sid,
                    &make_delta(i, i as u32, vec![user_msg(&format!("t{i}"))]),
                    &meta(),
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
    async fn load_only_deltas_without_snapshot_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let store = FileCslStore::new(tmp.path());
        let sid = "deltas-only";

        store
            .append(sid, &make_delta(0, 1, vec![user_msg("orphan1")]), &meta())
            .await
            .unwrap();
        store
            .append(sid, &make_delta(1, 2, vec![user_msg("orphan2")]), &meta())
            .await
            .unwrap();

        let entries = store.load_from_latest_snapshot(sid).await.unwrap();
        assert!(
            entries.is_empty(),
            "should return empty when no Snapshot exists"
        );
    }

    #[tokio::test]
    async fn fork_preserves_state_patch() {
        use crate::conversation_log::SessionStatePatch;

        let tmp = TempDir::new().unwrap();
        let store = FileCslStore::new(tmp.path());
        let parent = "patch-parent";

        store
            .append(parent, &make_snapshot(0, 1, vec![user_msg("hi")]), &meta())
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
                &meta(),
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
        store.append(sid, &snap, &meta()).await.unwrap();

        // ── Turn 2: load CSL → materialize → simulate loop → persist delta ──
        let entries = store.load_from_latest_snapshot(sid).await.unwrap();
        let mat = materialize(&entries).unwrap();
        assert_eq!(mat.messages.len(), 2);
        assert_eq!(mat.session_state.budget_remaining_tokens, 100_000);
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
        store.append(sid, &delta, &meta()).await.unwrap();

        // ── Turn 3: load → verify turn 2 is visible ──
        let entries = store.load_from_latest_snapshot(sid).await.unwrap();
        let mat = materialize(&entries).unwrap();
        assert_eq!(mat.messages.len(), 4); // 2 from snap + 2 from delta
        assert_eq!(mat.messages[0]["content"], "hello");
        assert_eq!(mat.messages[3]["content"], "resp2");
        assert_eq!(mat.session_state.blocked_tools, vec!["bash"]);
        assert_eq!(mat.session_state.budget_remaining_tokens, 80_000);
        assert_eq!(mat.session_state.budget_remaining_rounds, 9);
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
        store.append(sid, &delta2, &meta()).await.unwrap();

        // ── Final verify ──
        let entries = store.load_from_latest_snapshot(sid).await.unwrap();
        let mat = materialize(&entries).unwrap();
        assert_eq!(mat.messages.len(), 7);
        assert_eq!(mat.last_seq, 2);
        assert_eq!(mat.last_turn, 3);
        // State accumulated across turns
        assert_eq!(mat.session_state.blocked_tools, vec!["bash"]);
        assert_eq!(mat.session_state.recent_tools, vec!["read_file"]);
        assert_eq!(mat.session_state.budget_remaining_tokens, 80_000);
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
                &meta(),
            )
            .await
            .unwrap();
        for i in 1..=4u64 {
            store
                .append(
                    sid,
                    &make_delta(i, (i + 1) as u32, vec![user_msg(&format!("t{}", i + 1))]),
                    &meta(),
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
                &meta(),
            )
            .await
            .unwrap();
        // One more delta after compaction
        store
            .append(sid, &make_delta(6, 6, vec![user_msg("t6")]), &meta())
            .await
            .unwrap();

        let entries = store.load_from_latest_snapshot(sid).await.unwrap();
        // Should only return snapshot at seq=5 + delta at seq=6
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq(), 5);

        let mat = materialize(&entries).unwrap();
        assert_eq!(mat.messages.len(), 2);
        assert_eq!(mat.messages[0]["content"], "compacted_summary");
        assert_eq!(mat.session_state.budget_remaining_tokens, 70_000);
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
            let result = store.append(bad_id, &snap, &meta()).await;
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
            let result = store.append(good_id, &snap, &meta()).await;
            assert!(
                result.is_ok(),
                "session_id '{good_id}' should be accepted: {:?}",
                result.err()
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
            .append(sid, &make_snapshot(0, 1, vec![user_msg("good")]), &meta())
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
            .append(sid, &make_snapshot(0, 1, vec![user_msg("good")]), &meta())
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
