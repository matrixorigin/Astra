//! Unified CSL manager — encapsulates store + seq tracking + snapshot + GC.
//!
//! Used identically by CLI (FileCslStore) and server (DbCslStore).

use std::sync::Arc;

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
    turn_start_message_count: usize,
    trace_id: Option<String>,
    last_session_state: SessionStateCompact,
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
            turn_start_message_count: 0,
            trace_id: None,
            last_session_state: SessionStateCompact::default(),
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
        if entries.is_empty() {
            return Ok(None);
        }
        let mat = materialize(&entries)?;
        self.last_seq = mat.last_seq;
        self.last_turn = mat.last_turn;
        self.turn_start_message_count = mat.messages.len();
        self.last_session_state = mat.session_state.clone();
        Ok(Some(mat))
    }

    /// Record the message count at the start of the current turn.
    /// `persist_turn` uses this to compute appended messages.
    pub fn mark_turn_start(&mut self, message_count: usize) {
        self.turn_start_message_count = message_count;
    }

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
        let meta = AppendMeta {
            trace_id: self.trace_id.clone(),
            message_count: Some(messages.len() as u32),
        };

        if self.last_seq == 0 {
            let snapshot = CslEntry::Snapshot {
                seq: 1,
                turn,
                messages: messages.to_vec(),
                session_state: session_state.clone(),
            };
            self.store
                .append(&self.session_id, &snapshot, &meta)
                .await?;
            self.last_seq = 1;
            self.last_turn = turn;
            self.turn_start_message_count = messages.len();
            self.last_session_state = session_state.clone();
            return Ok(());
        }

        // Compute appended messages from the turn_start marker.
        let appended = if self.turn_start_message_count < messages.len() {
            messages[self.turn_start_message_count..].to_vec()
        } else {
            Vec::new()
        };

        let next_seq = self.last_seq + 1;
        let delta = CslEntry::TurnDelta {
            seq: next_seq,
            turn,
            appended,
            state_patch: Some(SessionStatePatch::from_full(session_state)),
        };
        self.store.append(&self.session_id, &delta, &meta).await?;
        self.last_seq = next_seq;
        self.last_turn = turn;
        self.turn_start_message_count = messages.len();
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
                messages: messages.to_vec(),
                session_state: session_state.clone(),
            };
            self.store
                .append(&self.session_id, &snapshot, &meta)
                .await?;
            self.last_seq = snap_seq;

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
        self.turn_start_message_count = 0;
        self.last_session_state = SessionStateCompact::default();
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

impl SessionStatePatch {
    /// Build a full patch from a complete `SessionStateCompact`.
    /// Every field is explicitly set — no diffing.
    pub fn from_full(state: &SessionStateCompact) -> Self {
        Self {
            continuity: Some(state.continuity.clone()),
            blocked_tools: Some(state.blocked_tools.clone()),
            recent_tools: Some(state.recent_tools.clone()),
            approval_overrides: Some(state.approval_overrides.clone()),
            interruption: Some(state.interruption.clone()),
            budget_remaining_tokens: Some(state.budget_remaining_tokens),
            budget_remaining_rounds: Some(state.budget_remaining_rounds),
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
        assert_eq!(mat.session_state.budget_remaining_tokens, 40_000);
        assert_eq!(mat.session_state.budget_remaining_rounds, 6);
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
            continuity: None,
            blocked_tools: vec!["bash".into()],
            recent_tools: vec!["read".into()],
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
        // continuity: None in compact → Some(None) in patch = "clear it"
        assert_eq!(patch.continuity, Some(None));
        assert_eq!(patch.blocked_tools, Some(vec!["bash".into()]));
        assert_eq!(patch.recent_tools, Some(vec!["read".into()]));
        assert_eq!(patch.approval_overrides, Some(Some(json!({"x": 1}))));
        assert_eq!(patch.compaction_tracker, Some(Some(json!({"v": 2}))));
        assert_eq!(patch.budget_remaining_tokens, Some(42));
        assert_eq!(patch.budget_remaining_rounds, Some(7));
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
        assert_eq!(mat.session_state.budget_remaining_tokens, 80_000);
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

    // ── Review fix: persist_turn auto-advances turn_start_message_count ──

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

    // ── Bug #2: from_full must express "clear continuity" ──

    #[tokio::test]
    async fn from_full_clears_continuity_when_compact_has_none() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let mut mgr = CslManager::new(
            Arc::clone(&store),
            "test-clear-cont".into(),
            CslManagerConfig::default(),
        )
        .unwrap();

        // Turn 1: state WITH continuity set.
        let continuity = astra_turn_types::continuity::ContinuityState {
            goal: Default::default(),
            todos: Default::default(),
            facts: Default::default(),
            user_corrections: vec![],
            verification: Default::default(),
        };
        let state1 = SessionStateCompact {
            continuity: Some(continuity),
            ..Default::default()
        };
        mgr.persist_turn(1, &[user_msg("t1")], &state1)
            .await
            .unwrap();

        // Turn 2: state WITHOUT continuity (cleared).
        let state2 = SessionStateCompact {
            continuity: None,
            ..Default::default()
        };
        mgr.mark_turn_start(1);
        mgr.persist_turn(2, &[user_msg("t1"), user_msg("t2")], &state2)
            .await
            .unwrap();

        // Reload: continuity must be None (cleared), not carried over from turn 1.
        let mut mgr2 = CslManager::new(
            Arc::clone(&store),
            "test-clear-cont".into(),
            CslManagerConfig::default(),
        )
        .unwrap();
        let mat = mgr2.load().await.unwrap().unwrap();
        assert!(
            mat.session_state.continuity.is_none(),
            "continuity should be cleared but was: {:?}",
            mat.session_state.continuity
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

    // ── R1: GC failure must not leave turn_start_message_count stale ────

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
    async fn gc_failure_still_updates_turn_start_message_count() {
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
        // still update turn_start_message_count so the NEXT turn's delta is correct.
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

        // KEY ASSERTION: even though GC failed, turn_start_message_count
        // must reflect the 4 messages from turn 2, so the next delta is incremental.
        // If the bug is present, turn_start_message_count stays at 2 (from turn 1).
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

    // ── I1: load() should set turn_start_message_count ──────────────────

    #[tokio::test]
    async fn load_sets_turn_start_message_count() {
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
        // set turn_start_message_count to 4 so the delta is only the 2 new messages.
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

    // ── Test gap: persist_turn with fewer messages than turn_start ──────

    #[tokio::test]
    async fn persist_turn_fewer_messages_than_start_produces_empty_delta() {
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

        // Turn 2: only 2 messages (compaction removed older ones).
        // turn_start_message_count is 4, but messages.len() is 2.
        let t2 = vec![user_msg("compacted"), assistant_msg("summary")];
        mgr.persist_turn(2, &t2, &default_state()).await.unwrap();

        // The delta should have empty appended (no panic from slice).
        let entries = store.load_after("fewer-msgs", 0).await.unwrap();
        assert_eq!(entries.len(), 2); // snapshot + delta
        if let CslEntry::TurnDelta { appended, .. } = &entries[1] {
            assert!(
                appended.is_empty(),
                "appended should be empty after compaction"
            );
        } else {
            panic!("expected TurnDelta");
        }

        // Reload: only the snapshot messages + empty delta = 4 msgs (from snapshot).
        // Actually, the delta appends nothing, so messages stay as snapshot (4).
        // But the *persisted* messages for turn 2 were only 2 — this is a
        // limitation since the delta can't remove messages. The next snapshot
        // will capture the compacted state.
        let mut mgr2 = CslManager::new(
            Arc::clone(&store),
            "fewer-msgs".into(),
            CslManagerConfig::default(),
        )
        .unwrap();
        let mat = mgr2.load().await.unwrap().unwrap();
        // Messages from snapshot (turn 1) + empty delta = still 4.
        assert_eq!(mat.messages.len(), 4);
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
        assert_eq!(mgr.last_session_state().budget_remaining_tokens, 100_000);

        let state2 = SessionStateCompact {
            budget_remaining_tokens: 80_000,
            budget_remaining_rounds: 8,
            ..Default::default()
        };
        mgr.persist_turn(2, &[user_msg("t1"), user_msg("t2")], &state2)
            .await
            .unwrap();
        assert_eq!(mgr.last_session_state().budget_remaining_tokens, 80_000);
        assert_eq!(mgr.last_session_state().budget_remaining_rounds, 8);
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
        assert_eq!(mgr2.last_session_state().budget_remaining_tokens, 50_000);
        assert_eq!(mgr2.last_session_state().recent_tools, vec!["read"]);
    }

    // ── Session ID validation at CslManager level ─────────────────────

    #[test]
    fn rejects_invalid_session_ids_at_construction() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);

        for bad_id in [
            "../etc/passwd",
            "foo/bar",
            "a\\b",
            "..",
            "",
            "   ",
            "has\0nul",
            "has\nnewline",
            "has\ttab",
            "has\x7Fdel",
        ] {
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
