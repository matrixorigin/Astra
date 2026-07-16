//! Unified CSL manager — encapsulates store + seq tracking + snapshot + GC.
//!
//! Used identically by CLI (FileCslStore) and server (DbCslStore).
//!
//! The manager stores canonical runtime history. Prompt-facing projections are
//! derived by callers at wire/session-display boundaries; they are never used
//! as the persisted source of truth.

use std::sync::Arc;

use sha2::{Digest, Sha256};

use super::{
    AppendMeta, CslEntry, CslStore, CslStoreError, MaterializedState, SessionStateCompact,
    SessionStatePatch, materialize, validate_session_id,
};

#[derive(Debug, Clone)]
pub struct CslManagerConfig {
    pub snapshot_interval: u32,
    pub gc_retain_snapshots: u32,
}

impl Default for CslManagerConfig {
    fn default() -> Self {
        Self {
            snapshot_interval: 5,
            gc_retain_snapshots: 2,
        }
    }
}

pub struct CslManager {
    store: Arc<dyn CslStore>,
    session_id: String,
    config: CslManagerConfig,
    last_seq: u64,
    last_turn: u32,
    last_canonical_message_hashes: Vec<CanonicalMessageHash>,
    trace_id: Option<String>,
    last_session_state: SessionStateCompact,
    /// A manager may be reconstructed while a prior projection is still
    /// completing. Do not assume `last_seq == 0` means the store is empty:
    /// load once before choosing the next append sequence.
    loaded: bool,
}

impl CslManager {
    pub fn new(
        store: Arc<dyn CslStore>,
        session_id: String,
        config: CslManagerConfig,
    ) -> Result<Self, CslStoreError> {
        validate_session_id(&session_id)?;
        Ok(Self {
            store,
            session_id,
            config,
            last_seq: 0,
            last_turn: 0,
            last_canonical_message_hashes: Vec::new(),
            trace_id: None,
            last_session_state: SessionStateCompact::default(),
            loaded: false,
        })
    }

    pub fn set_trace_id(&mut self, id: String) {
        self.trace_id = Some(id);
    }

    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    pub fn last_turn(&self) -> u32 {
        self.last_turn
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The session state from the last `load()` or `persist_turn()` call.
    /// Useful for callers that need to fall back to previous values when
    /// constructing a new `SessionStateCompact` with partial data.
    pub fn last_session_state(&self) -> &SessionStateCompact {
        &self.last_session_state
    }

    /// Load the session's CSL from the store. Returns `None` if no data exists.
    /// Updates internal `last_seq` / `last_turn` from the materialized state.
    pub async fn load(&mut self) -> Result<Option<MaterializedState>, CslStoreError> {
        let entries = self
            .store
            .load_from_latest_snapshot(&self.session_id)
            .await?;
        self.loaded = true;
        if entries.is_empty() {
            return Ok(None);
        }
        let mut mat = materialize(&entries)?;
        mat.session_state = mat.session_state.for_csl_continuity();
        self.last_seq = mat.last_seq;
        self.last_turn = mat.last_turn;
        self.last_canonical_message_hashes = canonical_message_hashes(&mat.messages);
        self.last_session_state = mat.session_state.clone();
        Ok(Some(mat))
    }

    /// Record the message count at the start of the current turn.
    ///
    /// This remains available to callers that track turn boundaries, but
    /// `persist_turn` computes CSL deltas from the last canonical message
    /// sequence instead of trusting a caller-provided count.
    pub fn mark_turn_start(&mut self, _message_count: usize) {}

    /// Persist the outcome of the current turn.
    ///
    /// - First turn (last_seq == 0): writes a `Snapshot` at seq=1.
    /// - Subsequent turns: writes a `TurnDelta`, plus a periodic `Snapshot` + GC.
    pub async fn persist_turn(
        &mut self,
        turn: u32,
        messages: &[serde_json::Value],
        session_state: &SessionStateCompact,
    ) -> Result<(), CslStoreError> {
        // `CslManager::new` intentionally does not perform I/O. A fresh
        // manager can therefore represent either a genuinely empty log or a
        // manager rebuilt after a delayed projection. Reconcile once here so
        // a new snapshot cannot race an existing sequence.
        if !self.loaded {
            self.load().await?;
        }
        let session_state = session_state.for_csl_continuity();
        let canonical_messages = messages.to_vec();
        let canonical_message_count = canonical_messages.len();
        let meta = AppendMeta {
            trace_id: self.trace_id.clone(),
            message_count: Some(canonical_message_count as u32),
        };

        if self.last_seq == 0 {
            let canonical_message_hashes = canonical_message_hashes(&canonical_messages);
            let snapshot = CslEntry::Snapshot {
                seq: 1,
                turn,
                messages: canonical_messages.clone(),
                session_state: session_state.clone(),
            };
            self.store
                .append(&self.session_id, &snapshot, &meta)
                .await?;
            self.last_seq = 1;
            self.last_turn = turn;
            self.last_canonical_message_hashes = canonical_message_hashes;
            self.last_session_state = session_state.clone();
            return Ok(());
        }

        let canonical_message_hashes = canonical_message_hashes(&canonical_messages);
        let common_prefix_len = common_canonical_prefix_len(
            &self.last_canonical_message_hashes,
            &canonical_message_hashes,
        );
        if common_prefix_len < self.last_canonical_message_hashes.len() {
            let next_seq = self.last_seq + 1;
            let snapshot = CslEntry::Snapshot {
                seq: next_seq,
                turn,
                messages: canonical_messages.clone(),
                session_state: session_state.clone(),
            };
            self.store
                .append(&self.session_id, &snapshot, &meta)
                .await?;
            self.last_seq = next_seq;
            self.last_turn = turn;
            self.last_canonical_message_hashes = canonical_message_hashes;
            self.last_session_state = session_state.clone();
            self.gc().await?;
            return Ok(());
        }

        let appended = canonical_messages[common_prefix_len..].to_vec();

        let next_seq = self.last_seq + 1;
        let delta = CslEntry::TurnDelta {
            seq: next_seq,
            turn,
            appended,
            state_patch: Some(SessionStatePatch::from_full(&session_state)),
        };
        self.store.append(&self.session_id, &delta, &meta).await?;
        self.last_seq = next_seq;
        self.last_turn = turn;
        self.last_canonical_message_hashes = canonical_message_hashes.clone();
        self.last_session_state = session_state.clone();

        // Periodic snapshot + GC.
        if turn > 0
            && self.config.snapshot_interval > 0
            && turn.is_multiple_of(self.config.snapshot_interval)
        {
            let snap_seq = self.last_seq + 1;
            let snapshot = CslEntry::Snapshot {
                seq: snap_seq,
                turn,
                messages: canonical_messages.clone(),
                session_state: session_state.clone(),
            };
            self.store
                .append(&self.session_id, &snapshot, &meta)
                .await?;
            self.last_seq = snap_seq;
            self.last_canonical_message_hashes = canonical_message_hashes;

            self.gc().await?;
        }

        Ok(())
    }

    /// Fork this session at `fork_after_turn`, creating a new session.
    /// Returns a fresh `CslManager` for the child and the materialized state
    /// (if non-empty), so the caller doesn't need to call `load()` again.
    pub async fn fork(
        &self,
        new_session_id: &str,
        fork_after_turn: u32,
    ) -> Result<(CslManager, Option<MaterializedState>), CslStoreError> {
        validate_session_id(new_session_id)?;
        self.store
            .fork(&self.session_id, new_session_id, fork_after_turn)
            .await?;

        let mut child = CslManager::new(
            Arc::clone(&self.store),
            new_session_id.to_string(),
            self.config.clone(),
        )?;
        child.trace_id = self.trace_id.clone();
        let mat = child.load().await?;
        Ok((child, mat))
    }

    /// Discard the session's CSL and reset internal state so the next
    /// `persist_turn` writes a fresh Snapshot at seq=1.
    /// Used after `/undo` or similar operations that discard the CSL.
    pub async fn reset(&mut self) -> Result<(), CslStoreError> {
        self.store
            .truncate_before(&self.session_id, i64::MAX as u64)
            .await?;
        self.last_seq = 0;
        self.last_turn = 0;
        self.last_canonical_message_hashes.clear();
        self.last_session_state = SessionStateCompact::default();
        self.loaded = true;
        Ok(())
    }

    async fn gc(&self) -> Result<(), CslStoreError> {
        if self.config.gc_retain_snapshots == 0 {
            return Ok(());
        }

        let snapshot_seqs = self.store.snapshot_seqs(&self.session_id).await?;

        if snapshot_seqs.len() as u32 <= self.config.gc_retain_snapshots {
            return Ok(());
        }

        // Keep the last `gc_retain_snapshots` snapshots (and everything after them).
        let keep_idx = snapshot_seqs.len() - self.config.gc_retain_snapshots as usize;
        let gc_before_seq = snapshot_seqs[keep_idx];
        self.store
            .truncate_before(&self.session_id, gc_before_seq)
            .await?;
        Ok(())
    }
}

type CanonicalMessageHash = [u8; 32];

fn canonical_message_hashes(messages: &[serde_json::Value]) -> Vec<CanonicalMessageHash> {
    messages.iter().map(canonical_message_hash).collect()
}

fn canonical_message_hash(message: &serde_json::Value) -> CanonicalMessageHash {
    // serde_json::Value object order can come from insertion order when the
    // preserve_order feature is enabled, so sort recursively before hashing.
    let canonical = sort_json_object_keys(message);
    let bytes = serde_json::to_vec(&canonical).unwrap_or_else(|_| {
        // Serializing a serde_json::Value should be infallible; keep a stable
        // fallback so a representation error cannot crash CSL diffing.
        canonical.to_string().into_bytes()
    });
    Sha256::digest(bytes).into()
}

/// Recursively sort all object keys in a JSON value to guarantee
/// deterministic serialization regardless of insertion order.
fn sort_json_object_keys(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut sorted: Vec<_> = map
                .iter()
                .map(|(k, v)| (k.clone(), sort_json_object_keys(v)))
                .collect();
            sorted.sort_by(|(a, _), (b, _)| a.cmp(b));
            serde_json::Value::Object(sorted.into_iter().collect())
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(sort_json_object_keys).collect())
        }
        other => other.clone(),
    }
}

fn common_canonical_prefix_len(
    left: &[CanonicalMessageHash],
    right: &[CanonicalMessageHash],
) -> usize {
    left.iter()
        .zip(right.iter())
        .take_while(|(left, right)| left == right)
        .count()
}

impl SessionStatePatch {
    /// Build a full patch from a complete `SessionStateCompact`.
    /// Every continuity field is explicitly set — no diffing. Legacy runtime
    /// budget fields are intentionally omitted.
    pub fn from_full(state: &SessionStateCompact) -> Self {
        Self {
            blocked_tools: Some(state.blocked_tools.clone()),
            recent_tools: Some(state.recent_tools.clone()),
            activated_deferred_tool_names: Some(state.activated_deferred_tool_names.clone()),
            approval_overrides: Some(state.approval_overrides.clone()),
            interruption: Some(state.interruption.clone()),
            budget_remaining_tokens: None,
            budget_remaining_rounds: None,
            consecutive_ctx_errors: Some(state.consecutive_ctx_errors),
            delegation: Some(state.delegation.clone()),
            compaction_tracker: Some(state.compaction_tracker.clone()),
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_log::file_store::FileCslStore;
    use serde_json::json;
    use tempfile::TempDir;

    fn user_msg(content: &str) -> serde_json::Value {
        json!({"role": "user", "content": content})
    }

    fn assistant_msg(content: &str) -> serde_json::Value {
        json!({"role": "assistant", "content": content})
    }

    fn default_state() -> SessionStateCompact {
        SessionStateCompact::default()
    }

    fn state_with_tools(recent: &[&str], blocked: &[&str]) -> SessionStateCompact {
        SessionStateCompact {
            recent_tools: recent.iter().map(|s| s.to_string()).collect(),
            blocked_tools: blocked.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn make_store(tmp: &tempfile::TempDir) -> Arc<dyn CslStore> {
        Arc::new(FileCslStore::new(tmp.path())) as Arc<dyn CslStore>
    }

    fn test_manager(tmp: &tempfile::TempDir) -> CslManager {
        CslManager::new(
            make_store(tmp),
            "test-session".into(),
            CslManagerConfig::default(),
        )
        .unwrap()
    }

    fn test_manager_with_config(
        tmp: &tempfile::TempDir,
        session_id: &str,
        config: CslManagerConfig,
    ) -> CslManager {
        CslManager::new(make_store(tmp), session_id.into(), config).unwrap()
    }

    #[tokio::test]
    async fn first_turn_writes_snapshot_at_seq_1() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = test_manager(&tmp);

        let msgs = vec![user_msg("hello"), assistant_msg("hi")];
        mgr.persist_turn(1, &msgs, &default_state()).await.unwrap();

        assert_eq!(mgr.last_seq(), 1);
        assert_eq!(mgr.last_turn(), 1);

        // Reload and verify it's a Snapshot.
        let mut mgr2 = test_manager(&tmp);
        let mat = mgr2.load().await.unwrap().unwrap();
        assert_eq!(mat.messages.len(), 2);
        assert_eq!(mat.last_seq, 1);
        assert_eq!(mat.messages[0]["content"], "hello");
    }

    #[tokio::test]
    async fn fresh_manager_reconciles_existing_sequence_before_persisting() {
        let tmp = TempDir::new().unwrap();
        let mut first = test_manager(&tmp);
        first
            .persist_turn(
                1,
                &[user_msg("first"), assistant_msg("one")],
                &default_state(),
            )
            .await
            .unwrap();

        // A worker can be rebuilt after a delayed write without an explicit
        // caller-side `load()`. Its next persist must append a delta rather
        // than collide with seq=1 by writing another snapshot.
        let mut rebuilt = test_manager(&tmp);
        rebuilt
            .persist_turn(
                2,
                &[
                    user_msg("first"),
                    assistant_msg("one"),
                    user_msg("second"),
                    assistant_msg("two"),
                ],
                &default_state(),
            )
            .await
            .unwrap();

        assert_eq!(rebuilt.last_seq(), 2);
        let mut loader = test_manager(&tmp);
        let materialized = loader.load().await.unwrap().unwrap();
        assert_eq!(materialized.messages.len(), 4);
        assert_eq!(materialized.messages[3]["content"], "two");
    }

    #[tokio::test]
    async fn subsequent_turn_writes_delta() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = test_manager(&tmp);

        let msgs1 = vec![user_msg("t1"), assistant_msg("r1")];
        mgr.persist_turn(1, &msgs1, &default_state()).await.unwrap();

        let msgs2 = vec![
            user_msg("t1"),
            assistant_msg("r1"),
            user_msg("t2"),
            assistant_msg("r2"),
        ];
        mgr.mark_turn_start(2);
        mgr.persist_turn(2, &msgs2, &default_state()).await.unwrap();

        assert_eq!(mgr.last_seq(), 2);
        assert_eq!(mgr.last_turn(), 2);

        let mut mgr2 = test_manager(&tmp);
        let mat = mgr2.load().await.unwrap().unwrap();
        assert_eq!(mat.messages.len(), 4);
        assert_eq!(mat.messages[3]["content"], "r2");
    }

    #[tokio::test]
    async fn periodic_snapshot_every_n_turns() {
        let tmp = TempDir::new().unwrap();
        let config = CslManagerConfig {
            snapshot_interval: 3,
            gc_retain_snapshots: 10,
        };
        let mut mgr = test_manager_with_config(&tmp, "test-snap", config);

        // Turn 1: Snapshot at seq=1
        mgr.persist_turn(1, &[user_msg("t1"), assistant_msg("r1")], &default_state())
            .await
            .unwrap();
        assert_eq!(mgr.last_seq(), 1);

        // Turn 2: Delta at seq=2
        mgr.mark_turn_start(2);
        let msgs2 = vec![
            user_msg("t1"),
            assistant_msg("r1"),
            user_msg("t2"),
            assistant_msg("r2"),
        ];
        mgr.persist_turn(2, &msgs2, &default_state()).await.unwrap();
        assert_eq!(mgr.last_seq(), 2);

        // Turn 3: Delta at seq=3, then Snapshot at seq=4 (because 3 % 3 == 0)
        mgr.mark_turn_start(4);
        let msgs3 = vec![
            user_msg("t1"),
            assistant_msg("r1"),
            user_msg("t2"),
            assistant_msg("r2"),
            user_msg("t3"),
            assistant_msg("r3"),
        ];
        mgr.persist_turn(3, &msgs3, &default_state()).await.unwrap();
        assert_eq!(mgr.last_seq(), 4); // seq=3 delta + seq=4 snapshot
    }

    #[tokio::test]
    async fn gc_after_snapshot_retains_n_snapshots() {
        let tmp = TempDir::new().unwrap();
        let config = CslManagerConfig {
            snapshot_interval: 2,
            gc_retain_snapshots: 1,
        };
        let store = make_store(&tmp);
        let mut mgr = CslManager::new(Arc::clone(&store), "test-gc".into(), config).unwrap();

        // Turn 1: Snapshot(seq=1)
        mgr.persist_turn(1, &[user_msg("t1")], &default_state())
            .await
            .unwrap();

        // Turn 2: Delta(seq=2) + Snapshot(seq=3) + GC
        mgr.mark_turn_start(1);
        let msgs2 = vec![user_msg("t1"), user_msg("t2")];
        mgr.persist_turn(2, &msgs2, &default_state()).await.unwrap();
        assert_eq!(mgr.last_seq(), 3);

        // After GC with retain=1, only the latest snapshot (seq=3) should remain.
        // The Snapshot at seq=1 and Delta at seq=2 should be truncated.
        let entries = store.load_after("test-gc", 0).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_snapshot());
        assert_eq!(entries[0].seq(), 3);
    }

    #[tokio::test]
    async fn load_empty_session_returns_none() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = test_manager(&tmp);
        let result = mgr.load().await.unwrap();
        assert!(result.is_none());
        assert_eq!(mgr.last_seq(), 0);
    }

    #[tokio::test]
    async fn load_after_persist_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = test_manager(&tmp);

        let state = state_with_tools(&["read_file"], &["bash"]);
        let msgs = vec![user_msg("hello"), assistant_msg("world")];
        mgr.persist_turn(1, &msgs, &state).await.unwrap();

        let mut mgr2 = test_manager(&tmp);
        let mat = mgr2.load().await.unwrap().unwrap();
        assert_eq!(mat.messages.len(), 2);
        assert_eq!(mat.session_state.recent_tools, vec!["read_file"]);
        assert_eq!(mat.session_state.blocked_tools, vec!["bash"]);
        assert_eq!(mgr2.last_seq(), 1);
        assert_eq!(mgr2.last_turn(), 1);
    }

    #[tokio::test]
    async fn fork_returns_new_manager_at_seq_1() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = test_manager(&tmp);

        let msgs = vec![user_msg("t1"), assistant_msg("r1")];
        mgr.persist_turn(1, &msgs, &default_state()).await.unwrap();

        mgr.mark_turn_start(2);
        let msgs2 = vec![
            user_msg("t1"),
            assistant_msg("r1"),
            user_msg("t2"),
            assistant_msg("r2"),
        ];
        mgr.persist_turn(2, &msgs2, &default_state()).await.unwrap();

        let (child, child_mat) = mgr.fork("child-session", 1).await.unwrap();
        assert_eq!(child.last_seq(), 1);
        assert_eq!(child.last_turn(), 1);
        assert_eq!(child.session_id(), "child-session");
        assert!(child_mat.is_some());

        // Load child and verify only turn 1 messages.
        let mut child2 =
            test_manager_with_config(&tmp, "child-session", CslManagerConfig::default());
        let mat = child2.load().await.unwrap().unwrap();
        assert_eq!(mat.messages.len(), 2);
        assert_eq!(mat.messages[0]["content"], "t1");
        assert_eq!(mat.messages[1]["content"], "r1");
    }

    #[tokio::test]
    async fn reset_forces_fresh_snapshot_on_next_persist() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = test_manager(&tmp);

        let msgs = vec![user_msg("t1"), assistant_msg("r1")];
        mgr.persist_turn(1, &msgs, &default_state()).await.unwrap();
        assert_eq!(mgr.last_seq(), 1);

        mgr.reset().await.unwrap();
        assert_eq!(mgr.last_seq(), 0);

        // Next persist should write a fresh Snapshot at seq=1 again.
        let msgs2 = vec![user_msg("fresh"), assistant_msg("start")];
        mgr.persist_turn(1, &msgs2, &default_state()).await.unwrap();
        assert_eq!(mgr.last_seq(), 1);

        let mut mgr2 = test_manager(&tmp);
        let mat = mgr2.load().await.unwrap().unwrap();
        assert_eq!(mat.messages[0]["content"], "fresh");
    }

    #[tokio::test]
    async fn persist_turn_includes_full_state_patch() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let mut mgr = CslManager::new(
            Arc::clone(&store),
            "test-patch".into(),
            CslManagerConfig::default(),
        )
        .unwrap();

        let state = SessionStateCompact {
            recent_tools: vec!["read".into()],
            blocked_tools: vec!["bash".into()],
            budget_remaining_tokens: 50_000,
            budget_remaining_rounds: 7,
            consecutive_ctx_errors: 2,
            approval_overrides: Some(json!({"tool": "bash"})),
            ..Default::default()
        };

        // Turn 1: Snapshot
        mgr.persist_turn(1, &[user_msg("t1")], &state)
            .await
            .unwrap();

        // Turn 2: Delta with full state patch
        let state2 = SessionStateCompact {
            recent_tools: vec!["write".into()],
            blocked_tools: vec![],
            budget_remaining_tokens: 40_000,
            budget_remaining_rounds: 6,
            consecutive_ctx_errors: 0,
            ..Default::default()
        };
        mgr.mark_turn_start(1);
        mgr.persist_turn(2, &[user_msg("t1"), user_msg("t2")], &state2)
            .await
            .unwrap();

        // Reload and verify state2 is fully applied.
        let entries = store.load_from_latest_snapshot("test-patch").await.unwrap();
        let mat = materialize(&entries).unwrap();
        assert_eq!(mat.session_state.recent_tools, vec!["write"]);
        assert!(mat.session_state.blocked_tools.is_empty());
        assert_eq!(mat.session_state.budget_remaining_tokens, 0);
        assert_eq!(mat.session_state.budget_remaining_rounds, 0);
        assert_eq!(mat.session_state.consecutive_ctx_errors, 0);
        assert!(mat.session_state.approval_overrides.is_none());
    }

    #[tokio::test]
    async fn trace_id_propagated_via_append_meta() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = test_manager(&tmp);
        mgr.set_trace_id("trace-123".into());

        let msgs = vec![user_msg("hello")];
        mgr.persist_turn(1, &msgs, &default_state()).await.unwrap();

        // The trace_id is written to AppendMeta which FileCslStore ignores.
        // We verify the manager accepted it (no error) and state is correct.
        assert_eq!(mgr.last_seq(), 1);
    }

    #[tokio::test]
    async fn session_state_patch_from_full_covers_all_fields() {
        let state = SessionStateCompact {
            blocked_tools: vec!["bash".into()],
            recent_tools: vec!["read".into()],
            activated_deferred_tool_names: vec!["write_file".into()],
            approval_overrides: Some(json!({"x": 1})),
            compaction_tracker: Some(json!({"v": 2})),
            budget_remaining_tokens: 42,
            budget_remaining_rounds: 7,
            consecutive_ctx_errors: 3,
            delegation: Some(super::super::DelegationCompact {
                id: "d1".into(),
                pattern: "p1".into(),
                completed_sub_runs: vec![],
            }),
            interruption: Some(json!({"k": "v"})),
        };

        let patch = SessionStatePatch::from_full(&state);
        assert_eq!(patch.blocked_tools, Some(vec!["bash".into()]));
        assert_eq!(patch.recent_tools, Some(vec!["read".into()]));
        assert_eq!(
            patch.activated_deferred_tool_names,
            Some(vec!["write_file".into()])
        );
        assert_eq!(patch.approval_overrides, Some(Some(json!({"x": 1}))));
        assert_eq!(patch.compaction_tracker, Some(Some(json!({"v": 2}))));
        assert_eq!(patch.budget_remaining_tokens, None);
        assert_eq!(patch.budget_remaining_rounds, None);
        assert_eq!(patch.consecutive_ctx_errors, Some(3));
        assert!(patch.delegation.is_some());
        assert_eq!(patch.interruption, Some(Some(json!({"k": "v"}))));
    }

    #[tokio::test]
    async fn multi_turn_lifecycle() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = test_manager(&tmp);

        // Turn 1
        let msgs1 = vec![user_msg("t1"), assistant_msg("r1")];
        let state1 = state_with_tools(&["read"], &[]);
        mgr.persist_turn(1, &msgs1, &state1).await.unwrap();

        // Turn 2
        mgr.mark_turn_start(2);
        let msgs2 = vec![
            user_msg("t1"),
            assistant_msg("r1"),
            user_msg("t2"),
            assistant_msg("r2"),
        ];
        let state2 = state_with_tools(&["write"], &["bash"]);
        mgr.persist_turn(2, &msgs2, &state2).await.unwrap();

        // Turn 3
        mgr.mark_turn_start(4);
        let msgs3 = vec![
            user_msg("t1"),
            assistant_msg("r1"),
            user_msg("t2"),
            assistant_msg("r2"),
            user_msg("t3"),
            assistant_msg("r3"),
        ];
        let state3 = SessionStateCompact {
            recent_tools: vec!["exec".into()],
            blocked_tools: vec!["bash".into()],
            budget_remaining_tokens: 80_000,
            consecutive_ctx_errors: 1,
            ..Default::default()
        };
        mgr.persist_turn(3, &msgs3, &state3).await.unwrap();

        assert_eq!(mgr.last_seq(), 3);
        assert_eq!(mgr.last_turn(), 3);

        // Reload and verify
        let mut mgr2 = test_manager(&tmp);
        let mat = mgr2.load().await.unwrap().unwrap();
        assert_eq!(mat.messages.len(), 6);
        assert_eq!(mat.session_state.recent_tools, vec!["exec"]);
        assert_eq!(mat.session_state.blocked_tools, vec!["bash"]);
        assert_eq!(mat.session_state.budget_remaining_tokens, 0);
        assert_eq!(mat.session_state.consecutive_ctx_errors, 1);
    }

    // ── Bug #1: reset() must not produce duplicate seq on next persist ──

    #[tokio::test]
    async fn reset_then_persist_does_not_produce_duplicate_seqs() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let mut mgr = CslManager::new(
            Arc::clone(&store),
            "test-reset-dup".into(),
            CslManagerConfig::default(),
        )
        .unwrap();

        // Turn 1 writes Snapshot(seq=1)
        mgr.persist_turn(1, &[user_msg("t1"), assistant_msg("r1")], &default_state())
            .await
            .unwrap();
        assert_eq!(mgr.last_seq(), 1);

        mgr.reset().await.unwrap();

        // After reset, persist again. Must not collide with existing seq=1.
        mgr.persist_turn(1, &[user_msg("fresh")], &default_state())
            .await
            .unwrap();

        // All entries in the store must have unique seqs.
        let entries = store.load_after("test-reset-dup", 0).await.unwrap();
        let seqs: Vec<u64> = entries.iter().map(|e| e.seq()).collect();
        let unique: std::collections::HashSet<u64> = seqs.iter().copied().collect();
        assert_eq!(seqs.len(), unique.len(), "duplicate seqs found: {seqs:?}");

        // Load must return only the fresh data.
        let mut mgr2 = CslManager::new(
            Arc::clone(&store),
            "test-reset-dup".into(),
            CslManagerConfig::default(),
        )
        .unwrap();
        let mat = mgr2.load().await.unwrap().unwrap();
        assert_eq!(mat.messages[0]["content"], "fresh");
    }

    // ── Review fix: persist_turn auto-advances canonical state ───

    #[tokio::test]
    async fn persist_turn_auto_advances_turn_start_so_deltas_are_incremental() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let mut mgr = CslManager::new(
            Arc::clone(&store),
            "test-auto-advance".into(),
            CslManagerConfig::default(),
        )
        .unwrap();

        // Turn 1: 2 messages.
        let t1 = vec![user_msg("q1"), assistant_msg("a1")];
        mgr.persist_turn(1, &t1, &default_state()).await.unwrap();

        // Turn 2: 4 messages total. WITHOUT calling mark_turn_start manually,
        // the delta should contain only the 2 new messages (not all 4).
        let t2 = vec![
            user_msg("q1"),
            assistant_msg("a1"),
            user_msg("q2"),
            assistant_msg("a2"),
        ];
        mgr.persist_turn(2, &t2, &default_state()).await.unwrap();

        let entries = store.load_after("test-auto-advance", 0).await.unwrap();
        // Entry 0: Snapshot(seq=1, turn=1, 2 messages)
        // Entry 1: TurnDelta(seq=2, turn=2, appended=2 new messages)
        assert_eq!(entries.len(), 2);
        if let CslEntry::TurnDelta { appended, .. } = &entries[1] {
            assert_eq!(
                appended.len(),
                2,
                "delta should contain only appended messages, not full history; got {appended:?}"
            );
            assert_eq!(appended[0]["content"], "q2");
        } else {
            panic!("entry[1] should be TurnDelta");
        }
    }

    #[tokio::test]
    async fn concurrent_stale_managers_allow_only_one_canonical_append() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let session_id = "test-concurrent-stale";
        let mut seed = CslManager::new(
            Arc::clone(&store),
            session_id.into(),
            CslManagerConfig::default(),
        )
        .unwrap();
        let t1 = vec![user_msg("q1"), assistant_msg("a1")];
        seed.persist_turn(1, &t1, &default_state()).await.unwrap();

        let mut left = CslManager::new(
            Arc::clone(&store),
            session_id.into(),
            CslManagerConfig::default(),
        )
        .unwrap();
        let mut right = CslManager::new(
            Arc::clone(&store),
            session_id.into(),
            CslManagerConfig::default(),
        )
        .unwrap();
        left.load().await.unwrap().unwrap();
        right.load().await.unwrap().unwrap();

        let left_messages = vec![
            user_msg("q1"),
            assistant_msg("a1"),
            user_msg("left"),
            assistant_msg("left done"),
        ];
        let right_messages = vec![
            user_msg("q1"),
            assistant_msg("a1"),
            user_msg("right"),
            assistant_msg("right done"),
        ];
        let left_state = default_state();
        let right_state = default_state();

        let (left_result, right_result) = tokio::join!(
            left.persist_turn(2, &left_messages, &left_state),
            right.persist_turn(2, &right_messages, &right_state)
        );

        let success_count = usize::from(left_result.is_ok()) + usize::from(right_result.is_ok());
        assert_eq!(
            success_count, 1,
            "stale concurrent managers must not both append turn 2: left={left_result:?}, right={right_result:?}"
        );
        let stale_error = left_result
            .as_ref()
            .err()
            .or_else(|| right_result.as_ref().err())
            .expect("one writer should lose the stale append race");
        assert!(
            stale_error
                .to_string()
                .contains("stale conversation log append"),
            "unexpected stale append error: {stale_error}"
        );

        let entries = store.load_after(session_id, 0).await.unwrap();
        assert_eq!(entries.len(), 2);
        let materialized = materialize(&entries).unwrap();
        assert_eq!(materialized.messages.len(), 4);
        let winner = materialized.messages[2]["content"].as_str().unwrap();
        assert!(
            winner == "left" || winner == "right",
            "unexpected winning canonical delta: {materialized:?}"
        );
    }

    #[tokio::test]
    async fn persist_turn_preserves_raw_canonical_history_at_manager_boundary() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let mut mgr = CslManager::new(
            Arc::clone(&store),
            "manager-canonical".into(),
            CslManagerConfig::default(),
        )
        .unwrap();

        let t1 = vec![
            user_msg(
                "review changes\n\n<system-reminder>\n[session-resume:v1]\nresume hint\n</system-reminder>",
            ),
            json!({"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "function": {"name": "skill", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "<skill-loaded name=\"review-changes\"/>"}),
            assistant_msg("reviewed"),
        ];
        mgr.persist_turn(1, &t1, &default_state()).await.unwrap();

        let t2 = vec![
            t1[0].clone(),
            t1[1].clone(),
            t1[2].clone(),
            t1[3].clone(),
            user_msg("next step\n\n<system-reminder>\nbackground update\n</system-reminder>"),
            json!({"role": "assistant", "content": null, "tool_calls": [{"id": "c2", "function": {"name": "bash", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": "c2", "content": "raw command output"}),
            assistant_msg("next done"),
        ];
        mgr.persist_turn(2, &t2, &default_state()).await.unwrap();

        let entries = store.load_after("manager-canonical", 0).await.unwrap();
        assert_eq!(entries.len(), 2);
        if let CslEntry::TurnDelta { appended, .. } = &entries[1] {
            assert_eq!(appended.len(), 4);
            assert_eq!(appended[0]["role"], "user");
            assert!(
                appended[0]["content"]
                    .as_str()
                    .unwrap()
                    .contains("<system-reminder>")
            );
            assert!(appended[1].get("tool_calls").is_some());
            assert_eq!(appended[2]["role"], "tool");
            assert_eq!(appended[2]["content"], "raw command output");
            assert_eq!(appended[3]["content"], "next done");
        } else {
            panic!("entry[1] should be TurnDelta");
        }

        let mut loader = CslManager::new(
            Arc::clone(&store),
            "manager-canonical".into(),
            CslManagerConfig::default(),
        )
        .unwrap();
        let mat = loader.load().await.unwrap().unwrap();
        assert_eq!(mat.messages.len(), 8);
        let joined = mat
            .messages
            .iter()
            .filter_map(|msg| msg["content"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("<system-reminder>"));
        assert!(joined.contains("[session-resume:v1]"));
        assert!(joined.contains("<skill-loaded"));
        assert!(mat.messages.iter().any(|msg| msg["role"] == "tool"));
        assert!(
            mat.messages
                .iter()
                .any(|msg| msg.get("tool_calls").is_some())
        );

        let prompt_messages =
            crate::prompt_facing::sanitize_prompt_facing_messages(mat.messages.clone());
        assert_eq!(
            prompt_messages,
            vec![
                user_msg("review changes"),
                assistant_msg("reviewed"),
                user_msg("next step"),
                assistant_msg("next done"),
            ],
            "prompt projection must derive clean user/assistant history from canonical CSL"
        );
    }

    #[tokio::test]
    async fn canonical_history_divergence_writes_snapshot_instead_of_count_based_delta() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let mut mgr = CslManager::new(
            Arc::clone(&store),
            "canonical-divergence".into(),
            CslManagerConfig::default(),
        )
        .unwrap();

        let t1 = vec![
            user_msg("1+1"),
            user_msg("1+2"),
            assistant_msg("1+1 = 2\n1+2 = 3"),
        ];
        mgr.persist_turn(1, &t1, &default_state()).await.unwrap();

        let t2 = vec![
            user_msg("1+1\n\n1+2"),
            assistant_msg("1+1 = 2\n1+2 = 3"),
            user_msg("1+4"),
            assistant_msg("1+4 = 5"),
        ];
        mgr.mark_turn_start(t1.len());
        mgr.persist_turn(2, &t2, &default_state()).await.unwrap();

        let entries = store.load_after("canonical-divergence", 0).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_snapshot());
        assert!(
            entries[1].is_snapshot(),
            "non-prefix canonical history must replace state with a snapshot"
        );

        let mut loader = CslManager::new(
            Arc::clone(&store),
            "canonical-divergence".into(),
            CslManagerConfig::default(),
        )
        .unwrap();
        let mat = loader.load().await.unwrap().unwrap();
        assert_eq!(mat.messages, t2);
    }

    // ── Review fix: reset with large before_seq must actually truncate ──

    #[tokio::test]
    async fn reset_truncate_uses_safe_max_value() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let mut mgr = CslManager::new(
            Arc::clone(&store),
            "test-reset-safe".into(),
            CslManagerConfig::default(),
        )
        .unwrap();

        mgr.persist_turn(1, &[user_msg("t1")], &default_state())
            .await
            .unwrap();
        assert_eq!(mgr.last_seq(), 1);

        // After reset, NO entries should remain in the store.
        mgr.reset().await.unwrap();

        let remaining = store.load_after("test-reset-safe", 0).await.unwrap();
        assert!(
            remaining.is_empty(),
            "store should be empty after reset, but has {} entries",
            remaining.len()
        );
    }

    // ── Bug #3: from_full must express "clear delegation" ──

    #[tokio::test]
    async fn from_full_clears_delegation_when_compact_has_none() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let mut mgr = CslManager::new(
            Arc::clone(&store),
            "test-clear-deleg".into(),
            CslManagerConfig::default(),
        )
        .unwrap();

        // Turn 1: state WITH delegation.
        let state1 = SessionStateCompact {
            delegation: Some(super::super::DelegationCompact {
                id: "d1".into(),
                pattern: "review".into(),
                completed_sub_runs: vec![],
            }),
            ..Default::default()
        };
        mgr.persist_turn(1, &[user_msg("t1")], &state1)
            .await
            .unwrap();

        // Turn 2: delegation cleared.
        let state2 = SessionStateCompact {
            delegation: None,
            ..Default::default()
        };
        mgr.mark_turn_start(1);
        mgr.persist_turn(2, &[user_msg("t1"), user_msg("t2")], &state2)
            .await
            .unwrap();

        // Reload: delegation must be None.
        let mut mgr2 = CslManager::new(
            Arc::clone(&store),
            "test-clear-deleg".into(),
            CslManagerConfig::default(),
        )
        .unwrap();
        let mat = mgr2.load().await.unwrap().unwrap();
        assert!(
            mat.session_state.delegation.is_none(),
            "delegation should be cleared but was: {:?}",
            mat.session_state.delegation
        );
    }

    // ── C1: Fork child first persist must not duplicate messages ─────────

    #[tokio::test]
    async fn fork_child_first_persist_does_not_duplicate_messages() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);

        // Parent: 2 turns, 4 messages.
        let mut parent =
            CslManager::new(Arc::clone(&store), "p".into(), CslManagerConfig::default()).unwrap();
        let t1 = vec![user_msg("q1"), assistant_msg("a1")];
        parent.persist_turn(1, &t1, &default_state()).await.unwrap();
        let t2 = vec![
            user_msg("q1"),
            assistant_msg("a1"),
            user_msg("q2"),
            assistant_msg("a2"),
        ];
        parent.persist_turn(2, &t2, &default_state()).await.unwrap();

        // Fork at turn 2 → child gets Snapshot with 4 messages.
        let (mut child, _) = parent.fork("c", 2).await.unwrap();

        // Child's first turn adds 2 new messages (6 total).
        let t3 = vec![
            user_msg("q1"),
            assistant_msg("a1"),
            user_msg("q2"),
            assistant_msg("a2"),
            user_msg("q3"),
            assistant_msg("a3"),
        ];
        child.persist_turn(3, &t3, &default_state()).await.unwrap();

        // Reload child and verify: should have exactly 6 messages, not 10
        // (would be 10 if delta duplicated all 6 messages onto the 4-message snapshot).
        let mut child2 =
            CslManager::new(Arc::clone(&store), "c".into(), CslManagerConfig::default()).unwrap();
        let mat = child2.load().await.unwrap().unwrap();
        assert_eq!(
            mat.messages.len(),
            6,
            "child should have 6 messages (4 from fork + 2 new), got {}",
            mat.messages.len()
        );
        assert_eq!(mat.messages[4]["content"], "q3");
        assert_eq!(mat.messages[5]["content"], "a3");
    }

    // ── R1: GC failure must not leave canonical state stale ──────

    /// Store wrapper that delegates to an inner store but fails `truncate_before`.
    struct FailingGcStore {
        inner: Arc<dyn CslStore>,
    }

    #[async_trait::async_trait]
    impl CslStore for FailingGcStore {
        async fn append(
            &self,
            session_id: &str,
            entry: &CslEntry,
            meta: &super::AppendMeta,
        ) -> Result<(), CslStoreError> {
            self.inner.append(session_id, entry, meta).await
        }

        async fn load_from_latest_snapshot(
            &self,
            session_id: &str,
        ) -> Result<Vec<CslEntry>, CslStoreError> {
            self.inner.load_from_latest_snapshot(session_id).await
        }

        async fn load_after(
            &self,
            session_id: &str,
            after_seq: u64,
        ) -> Result<Vec<CslEntry>, CslStoreError> {
            self.inner.load_after(session_id, after_seq).await
        }

        async fn truncate_before(
            &self,
            _session_id: &str,
            _before_seq: u64,
        ) -> Result<u64, CslStoreError> {
            Err(CslStoreError::Other("simulated GC failure".into()))
        }

        async fn fork(
            &self,
            parent_session_id: &str,
            new_session_id: &str,
            fork_after_turn: u32,
        ) -> Result<u64, CslStoreError> {
            self.inner
                .fork(parent_session_id, new_session_id, fork_after_turn)
                .await
        }
    }

    #[tokio::test]
    async fn gc_failure_still_updates_canonical_prompt_state() {
        let tmp = TempDir::new().unwrap();
        let real_store = make_store(&tmp);
        let failing_store: Arc<dyn CslStore> = Arc::new(FailingGcStore {
            inner: Arc::clone(&real_store),
        });

        let config = CslManagerConfig {
            snapshot_interval: 2,
            gc_retain_snapshots: 1,
        };
        let mut mgr = CslManager::new(failing_store, "gc-fail".into(), config).unwrap();

        // Turn 1: Snapshot(seq=1)
        let t1 = vec![user_msg("q1"), assistant_msg("a1")];
        mgr.persist_turn(1, &t1, &default_state()).await.unwrap();

        // Turn 2 triggers snapshot+GC. GC will fail — but persist_turn should
        // still update the canonical state so the NEXT turn's delta is correct.
        let t2 = vec![
            user_msg("q1"),
            assistant_msg("a1"),
            user_msg("q2"),
            assistant_msg("a2"),
        ];
        // GC failure → persist_turn returns Err. The caller ignores errors (warn log).
        let gc_result = mgr.persist_turn(2, &t2, &default_state()).await;
        // The GC error propagates — that's expected.
        assert!(gc_result.is_err(), "GC failure should propagate");

        // KEY ASSERTION: even though GC failed, the manager must retain the
        // canonical 4-message state from turn 2, so the next delta is incremental.
        let t3 = vec![
            user_msg("q1"),
            assistant_msg("a1"),
            user_msg("q2"),
            assistant_msg("a2"),
            user_msg("q3"),
            assistant_msg("a3"),
        ];
        mgr.persist_turn(3, &t3, &default_state()).await.unwrap();

        // Reload from the real store and verify we have 6 messages, not 8.
        // (If turn_start was stale at 2, delta for turn 3 would append msgs[2..6]=4 items
        //  onto the 4-message snapshot, giving 8.)
        let mut mgr2 =
            CslManager::new(real_store, "gc-fail".into(), CslManagerConfig::default()).unwrap();
        let mat = mgr2.load().await.unwrap().unwrap();
        assert_eq!(
            mat.messages.len(),
            6,
            "GC failure should not cause message duplication; expected 6, got {}",
            mat.messages.len()
        );
    }

    // ── I1: load() should set canonical state ────────────────────

    #[tokio::test]
    async fn load_sets_canonical_prompt_state() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);

        // Write 2 turns.
        let mut mgr =
            CslManager::new(Arc::clone(&store), "s".into(), CslManagerConfig::default()).unwrap();
        let t1 = vec![user_msg("q1"), assistant_msg("a1")];
        mgr.persist_turn(1, &t1, &default_state()).await.unwrap();
        let t2 = vec![
            user_msg("q1"),
            assistant_msg("a1"),
            user_msg("q2"),
            assistant_msg("a2"),
        ];
        mgr.persist_turn(2, &t2, &default_state()).await.unwrap();

        // Fresh manager loads the session (4 messages).
        let mut mgr2 =
            CslManager::new(Arc::clone(&store), "s".into(), CslManagerConfig::default()).unwrap();
        let mat = mgr2.load().await.unwrap().unwrap();
        assert_eq!(mat.messages.len(), 4);

        // Persist turn 3 WITHOUT calling mark_turn_start — load() should have
        // restored the canonical 4-message state so the delta is only the 2 new messages.
        let t3 = vec![
            user_msg("q1"),
            assistant_msg("a1"),
            user_msg("q2"),
            assistant_msg("a2"),
            user_msg("q3"),
            assistant_msg("a3"),
        ];
        mgr2.persist_turn(3, &t3, &default_state()).await.unwrap();

        // Reload and verify: should have 6 messages, not 10.
        let mut mgr3 =
            CslManager::new(Arc::clone(&store), "s".into(), CslManagerConfig::default()).unwrap();
        let mat = mgr3.load().await.unwrap().unwrap();
        assert_eq!(
            mat.messages.len(),
            6,
            "after load+persist without mark_turn_start, should have 6 messages, got {}",
            mat.messages.len()
        );
    }

    // ── Test gap: persist_turn with non-prefix canonical history ────────

    #[tokio::test]
    async fn persist_turn_non_prefix_history_replaces_state_with_snapshot() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let mut mgr = CslManager::new(
            Arc::clone(&store),
            "fewer-msgs".into(),
            CslManagerConfig::default(),
        )
        .unwrap();

        // Turn 1: 4 messages.
        let t1 = vec![
            user_msg("q1"),
            assistant_msg("a1"),
            user_msg("q2"),
            assistant_msg("a2"),
        ];
        mgr.persist_turn(1, &t1, &default_state()).await.unwrap();

        // Turn 2: only 2 messages remain. A TurnDelta cannot express removals,
        // so the manager must write a new canonical snapshot.
        let t2 = vec![user_msg("compacted"), assistant_msg("summary")];
        mgr.persist_turn(2, &t2, &default_state()).await.unwrap();

        let entries = store.load_after("fewer-msgs", 0).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_snapshot());
        assert!(entries[1].is_snapshot());

        let mut mgr2 = CslManager::new(
            Arc::clone(&store),
            "fewer-msgs".into(),
            CslManagerConfig::default(),
        )
        .unwrap();
        let mat = mgr2.load().await.unwrap().unwrap();
        assert_eq!(mat.messages, t2);
    }

    #[tokio::test]
    async fn manager_keeps_only_canonical_message_hashes_between_persists() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = test_manager(&tmp);
        let large_assistant_text = "x".repeat(256 * 1024);
        let messages = vec![
            user_msg("q1"),
            assistant_msg(&large_assistant_text),
            assistant_msg("a1"),
        ];

        mgr.persist_turn(1, &messages, &default_state())
            .await
            .unwrap();

        assert_eq!(
            mgr.last_canonical_message_hashes.len(),
            messages.len(),
            "manager should retain one fixed-size hash per canonical message"
        );
        assert_eq!(
            std::mem::size_of_val(mgr.last_canonical_message_hashes.as_slice()),
            messages.len() * std::mem::size_of::<CanonicalMessageHash>(),
            "manager state must scale with message count, not message payload bytes"
        );
    }

    #[test]
    fn canonical_message_hash_is_independent_of_object_insertion_order() {
        fn object(fields: Vec<(&str, serde_json::Value)>) -> serde_json::Value {
            let mut map = serde_json::Map::new();
            for (key, value) in fields {
                map.insert(key.to_string(), value);
            }
            serde_json::Value::Object(map)
        }

        let left = object(vec![
            ("role", json!("assistant")),
            (
                "content",
                json!([
                    {"b": 2, "a": {"z": 0, "y": 1}},
                    {"tool": {"name": "read_file", "args": {"path": "README.md", "start_line": 1, "end_line": 20}}}
                ]),
            ),
            (
                "metadata",
                object(vec![("beta", json!(2)), ("alpha", json!(1))]),
            ),
        ]);
        let right = object(vec![
            (
                "metadata",
                object(vec![("alpha", json!(1)), ("beta", json!(2))]),
            ),
            (
                "content",
                json!([
                    {"a": {"y": 1, "z": 0}, "b": 2},
                    {"tool": {"args": {"end_line": 20, "path": "README.md", "start_line": 1}, "name": "read_file"}}
                ]),
            ),
            ("role", json!("assistant")),
        ]);
        let changed = object(vec![
            (
                "metadata",
                object(vec![("alpha", json!(1)), ("beta", json!(3))]),
            ),
            (
                "content",
                json!([
                    {"a": {"y": 1, "z": 0}, "b": 2},
                    {"tool": {"args": {"end_line": 20, "path": "README.md", "start_line": 1}, "name": "read_file"}}
                ]),
            ),
            ("role", json!("assistant")),
        ]);

        assert_eq!(
            canonical_message_hash(&left),
            canonical_message_hash(&right)
        );
        assert_ne!(
            canonical_message_hash(&left),
            canonical_message_hash(&changed)
        );
    }

    // ── Test gap: fork at turn 0 ───────────────────────────────────────

    #[tokio::test]
    async fn fork_at_turn_0_produces_empty_child() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let mut parent = CslManager::new(
            Arc::clone(&store),
            "parent-t0".into(),
            CslManagerConfig::default(),
        )
        .unwrap();

        let t1 = vec![user_msg("q1"), assistant_msg("a1")];
        parent.persist_turn(1, &t1, &default_state()).await.unwrap();

        // Fork at turn 0 — no entries qualify (all entries are turn >= 1).
        let (child, child_mat) = parent.fork("child-t0", 0).await.unwrap();
        assert_eq!(child.last_seq(), 0, "child should have no CSL data");
        assert_eq!(child.last_turn(), 0);
        assert!(child_mat.is_none());

        // Child's first persist should write a fresh snapshot.
        let mut child = child;
        let ct1 = vec![user_msg("child-q1"), assistant_msg("child-a1")];
        child.persist_turn(1, &ct1, &default_state()).await.unwrap();
        assert_eq!(child.last_seq(), 1);

        let mut loader = CslManager::new(
            Arc::clone(&store),
            "child-t0".into(),
            CslManagerConfig::default(),
        )
        .unwrap();
        let mat = loader.load().await.unwrap().unwrap();
        assert_eq!(mat.messages.len(), 2);
        assert_eq!(mat.messages[0]["content"], "child-q1");
    }

    // ── Test gap: reset followed by load ───────────────────────────────

    #[tokio::test]
    async fn reset_then_load_returns_none() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let mut mgr = CslManager::new(
            Arc::clone(&store),
            "reset-load".into(),
            CslManagerConfig::default(),
        )
        .unwrap();

        let t1 = vec![user_msg("q1"), assistant_msg("a1")];
        mgr.persist_turn(1, &t1, &default_state()).await.unwrap();
        assert_eq!(mgr.last_seq(), 1);

        mgr.reset().await.unwrap();

        // Load after reset should return None (all data truncated).
        let result = mgr.load().await.unwrap();
        assert!(result.is_none(), "load after reset should return None");
        assert_eq!(mgr.last_seq(), 0);
        assert_eq!(mgr.last_turn(), 0);
    }

    // ── Test gap: last_session_state preserved across persists ─────────

    #[tokio::test]
    async fn last_session_state_reflects_latest_persist() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = test_manager(&tmp);

        let state1 = SessionStateCompact {
            budget_remaining_tokens: 100_000,
            budget_remaining_rounds: 10,
            ..Default::default()
        };
        mgr.persist_turn(1, &[user_msg("t1")], &state1)
            .await
            .unwrap();
        assert_eq!(mgr.last_session_state().budget_remaining_tokens, 0);

        let state2 = SessionStateCompact {
            budget_remaining_tokens: 80_000,
            budget_remaining_rounds: 8,
            ..Default::default()
        };
        mgr.persist_turn(2, &[user_msg("t1"), user_msg("t2")], &state2)
            .await
            .unwrap();
        assert_eq!(mgr.last_session_state().budget_remaining_tokens, 0);
        assert_eq!(mgr.last_session_state().budget_remaining_rounds, 0);
    }

    #[tokio::test]
    async fn last_session_state_set_by_load() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);

        let state = SessionStateCompact {
            budget_remaining_tokens: 50_000,
            recent_tools: vec!["read".into()],
            ..Default::default()
        };
        let mut mgr = CslManager::new(
            Arc::clone(&store),
            "state-load".into(),
            CslManagerConfig::default(),
        )
        .unwrap();
        mgr.persist_turn(1, &[user_msg("t1")], &state)
            .await
            .unwrap();

        // Fresh manager loads — should have the state.
        let mut mgr2 = CslManager::new(
            Arc::clone(&store),
            "state-load".into(),
            CslManagerConfig::default(),
        )
        .unwrap();
        assert_eq!(mgr2.last_session_state().budget_remaining_tokens, 0);
        mgr2.load().await.unwrap();
        assert_eq!(mgr2.last_session_state().budget_remaining_tokens, 0);
        assert_eq!(mgr2.last_session_state().recent_tools, vec!["read"]);
    }

    // ── Session ID validation at CslManager level ─────────────────────

    #[test]
    fn rejects_invalid_session_ids_at_construction() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);

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
            let result = CslManager::new(
                Arc::clone(&store),
                bad_id.to_string(),
                CslManagerConfig::default(),
            );
            match result {
                Err(CslStoreError::InvalidSessionId(_)) => {}
                Err(other) => panic!("'{bad_id}': expected InvalidSessionId, got {other}"),
                Ok(_) => panic!("session_id '{bad_id}' should be rejected"),
            }
        }
    }

    #[tokio::test]
    async fn fork_rejects_invalid_child_session_id() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = test_manager(&tmp);
        mgr.persist_turn(1, &[user_msg("t1")], &default_state())
            .await
            .unwrap();

        let result = mgr.fork("../malicious", 1).await;
        assert!(result.is_err(), "fork with invalid child ID should fail");
    }
}
