//! File edit journal — tracks file mutations for undo support.
//!
//! Records the before-state of every file write so that changes can be reverted
//! at the file level or turn level.
//!
//! # Usage
//!
//! ```text
//! let mut journal = FileEditJournal::new(500);
//!
//! // Before writing, snapshot the current content
//! journal.record_before(&path, "call-001", 3);
//!
//! // After writing, record what was written
//! journal.record_after(&path, "call-001", new_content.as_bytes());
//!
//! // Undo the last edit to a file
//! journal.undo_file(&path)?;
//!
//! // Undo all edits from turn 3
//! journal.undo_turn(3)?;
//! ```

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

// ─── Types ──────────────────────────────────────────────────────────────────

/// Type of file mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EditType {
    /// File was created (no prior content).
    Create,
    /// Existing file was overwritten (write_file).
    Overwrite,
    /// Existing file was patched in-place (str_replace).
    Patch,
    /// Existing file was deleted.
    Delete,
}

/// A single file edit recorded by the journal.
///
/// Serializable so the journal can survive a CLI restart: entries are
/// persisted under `~/.astra/sessions/<sid>/file_checkpoints/<seq>.json`
/// and reloaded on the next session boot. Binary content is serialized
/// as a JSON array of `u8` — compact-ish via serde_json's default, and
/// human-readable for debugging a checkpoint dir by hand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEditEntry {
    /// Monotonic sequence number assigned when the entry is recorded.
    /// `pub` so save_to_dir can filename-key entries by sequence.
    pub sequence: u64,
    /// Absolute path of the edited file.
    pub path: PathBuf,
    /// Agentic loop turn index when the edit occurred.
    pub turn_index: u32,
    /// Wall-clock timestamp.
    pub timestamp: SystemTime,
    /// Content before the edit. `None` if the file didn't exist (Create).
    pub before_content: Option<Vec<u8>>,
    /// Content after the edit. Empty for deletions.
    pub after_content: Vec<u8>,
    /// Tool call ID that triggered this edit.
    pub tool_call_id: String,
    /// What kind of edit this was.
    pub edit_type: EditType,
}

/// Result of an undo operation.
#[derive(Debug)]
pub struct UndoResult {
    /// Files that were successfully reverted.
    pub reverted: Vec<PathBuf>,
    /// Files that failed to revert (path, error message).
    pub failed: Vec<(PathBuf, String)>,
}

// ─── Journal ────────────────────────────────────────────────────────────────

/// Bounded journal of file edits supporting file-level and turn-level undo.
///
/// Entries are stored in chronological order with an LRU eviction policy.
/// Thread-safety note: this struct is NOT `Sync`; wrap in a `Mutex` if shared.
#[derive(Debug)]
pub struct FileEditJournal {
    entries: VecDeque<FileEditEntry>,
    max_entries: usize,
    next_sequence: u64,
    /// When set, new entries are persisted to this directory automatically
    /// and evicted entries are deleted. Enable via [`Self::enable_persistence`].
    /// When `None`, the journal is pure in-memory (original behavior).
    persist_dir: Option<PathBuf>,
}

impl FileEditJournal {
    /// Create a new journal with the given capacity.
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(max_entries.min(1024)),
            max_entries,
            next_sequence: 0,
            persist_dir: None,
        }
    }

    /// Turn on auto-persistence to `dir`. Each subsequent `push` writes
    /// the new entry atomically to `<dir>/<seq:06>.json`; each eviction
    /// removes the stale file.
    ///
    /// **Initial sync**: existing in-memory entries are flushed to `dir`
    /// right now via [`Self::save_to_dir`]. Because `save_to_dir` also
    /// prunes on-disk entries whose sequences are not in-memory, calling
    /// `enable_persistence` on a dir that another process has written to
    /// will destructively reconcile it to match our in-memory state.
    /// Callers relying on crash-recovery should load from `dir` FIRST
    /// (via [`Self::load_from_dir`]) and then `enable_persistence` on the
    /// loaded journal — the in-memory state then matches disk and the
    /// flush is a no-op write of identical bytes.
    ///
    /// Errors are logged at `warn` and do not propagate — the in-memory
    /// journal keeps working even if disk I/O fails.
    pub fn enable_persistence(&mut self, dir: PathBuf) {
        self.persist_dir = Some(dir);
        // Flush current entries so the on-disk state matches in-memory.
        if let Some(d) = self.persist_dir.clone()
            && let Err(e) = self.save_to_dir(&d)
        {
            astra_core::agent_warn!(
                "file_edit_journal",
                "initial flush to {} failed: {e}",
                d.display()
            );
        }
    }

    /// Current auto-persistence directory, if enabled. Exposed so a caller
    /// wiring a shared journal into a session-scoped executor can verify
    /// the journal is already configured (and avoid double-binding).
    pub fn persist_dir(&self) -> Option<&Path> {
        self.persist_dir.as_deref()
    }

    /// Record the before-state of a file that is about to be written.
    ///
    /// Call this BEFORE `fs::write` / `str_replace`. If the file does not
    /// exist yet, `before_content` is stored as `None` (Create).
    pub fn record_before(&mut self, path: &Path, tool_call_id: &str, turn_index: u32) {
        let before_content = std::fs::read(path).ok();
        let edit_type = if before_content.is_none() {
            EditType::Create
        } else {
            EditType::Overwrite
        };

        self.push(FileEditEntry {
            sequence: 0,
            path: path.to_owned(),
            turn_index,
            timestamp: SystemTime::now(),
            before_content,
            after_content: Vec::new(), // filled by record_after
            tool_call_id: tool_call_id.to_string(),
            edit_type,
        });
    }

    /// Update the most recent entry for `path` with the after-state content
    /// and optionally refine the edit type (e.g., to `Patch` for str_replace).
    pub fn record_after(&mut self, path: &Path, tool_call_id: &str, content: &[u8]) {
        // Walk backwards to find the matching entry
        let mut updated_seq: Option<u64> = None;
        for entry in self.entries.iter_mut().rev() {
            if entry.path == path && entry.tool_call_id == tool_call_id {
                entry.after_content = content.to_vec();
                updated_seq = Some(entry.sequence);
                break;
            }
        }
        match updated_seq {
            Some(seq) => self.persist_entry_by_sequence(seq),
            None => astra_core::agent_warn!(
                "file_edit",
                "record_after: no matching entry for path={} tool_call_id={}",
                path.display(),
                tool_call_id
            ),
        }
    }

    /// Convenience: record before-state with `Patch` edit type (for str_replace).
    pub fn record_before_patch(&mut self, path: &Path, tool_call_id: &str, turn_index: u32) {
        let before_content = std::fs::read(path).ok();
        self.push(FileEditEntry {
            sequence: 0,
            path: path.to_owned(),
            turn_index,
            timestamp: SystemTime::now(),
            before_content,
            after_content: Vec::new(),
            tool_call_id: tool_call_id.to_string(),
            edit_type: EditType::Patch,
        });
    }

    /// Record a successful file deletion with the deleted file's prior content.
    pub fn record_delete(
        &mut self,
        path: &Path,
        tool_call_id: &str,
        turn_index: u32,
        before_content: Vec<u8>,
    ) {
        self.push(FileEditEntry {
            sequence: 0,
            path: path.to_owned(),
            turn_index,
            timestamp: SystemTime::now(),
            before_content: Some(before_content),
            after_content: Vec::new(),
            tool_call_id: tool_call_id.to_string(),
            edit_type: EditType::Delete,
        });
    }

    /// Revert the most recent edit to a specific file.
    ///
    /// If the file was created by the agent, it is deleted. If it was
    /// overwritten or patched, the original content is restored.
    pub fn undo_file(&self, path: &Path) -> io::Result<Option<EditType>> {
        let entry = match self.entries.iter().rev().find(|e| e.path == path) {
            Some(e) => e,
            None => return Ok(None),
        };
        Self::apply_revert(entry)?;
        Ok(Some(entry.edit_type))
    }

    /// Revert ALL file edits from a specific turn (in reverse chronological order).
    pub fn undo_turn(&self, turn_index: u32) -> UndoResult {
        self.undo_turn_since(turn_index, 0)
    }

    /// Revert file edits from a specific turn recorded at or after a checkpoint.
    pub fn undo_turn_since(&self, turn_index: u32, checkpoint: u64) -> UndoResult {
        let mut result = UndoResult {
            reverted: Vec::new(),
            failed: Vec::new(),
        };
        // Collect entries for this turn in reverse order
        let turn_entries: Vec<&FileEditEntry> = self
            .entries
            .iter()
            .rev()
            .filter(|e| e.turn_index == turn_index && e.sequence >= checkpoint)
            .collect();

        for entry in turn_entries {
            match Self::apply_revert(entry) {
                Ok(()) => result.reverted.push(entry.path.clone()),
                Err(e) => result.failed.push((entry.path.clone(), e.to_string())),
            }
        }
        result
    }

    /// Return a checkpoint token for future turn-scoped rollback filtering.
    pub fn checkpoint(&self) -> u64 {
        self.next_sequence
    }

    /// Number of entries currently in the journal.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the journal is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Absorb another journal's entries as PRECEDING this journal's.
    ///
    /// Sequence numbers are reassigned contiguously starting from 0 —
    /// older entries first, then self's entries. `next_sequence` is
    /// updated so subsequent `push` calls can't collide with any
    /// existing entry's sequence.
    ///
    /// Ring-buffer semantics apply: if `older.len() + self.len() > max_entries`,
    /// the oldest entries (starting from `older`) are evicted. Self's
    /// `max_entries` wins — the older journal's cap is ignored.
    ///
    /// **No on-disk side effects**. This is purely an in-memory merge —
    /// callers that want to sync the merged state to disk should call
    /// [`Self::save_to_dir`] afterward.
    ///
    /// **Invariant loss**: any [`Self::checkpoint`] token captured
    /// before this call is invalidated. The re-sequencing means the
    /// token no longer corresponds to a real boundary in the journal,
    /// and passing it to [`Self::undo_turn_since`] will filter against
    /// the new sequence space incorrectly. Capture fresh checkpoints
    /// AFTER a merge.
    ///
    /// **Turn-index preservation**: older entries retain their original
    /// `turn_index` values. Callers that filter by turn (`undo_turn`,
    /// `files_in_turn`) will see merged-in entries appear under their
    /// original turn numbers — which may bucket them with unrelated
    /// self-originated entries if turn counters happen to overlap.
    ///
    /// Use case: a `ToolExecutor` is wiring a shared journal that
    /// already holds pre-session entries. The session's on-disk dir may
    /// also hold entries from a prior run. Without this operation the
    /// caller has to choose which side to keep; with it, both are
    /// preserved and re-sequenced cleanly.
    pub fn merge_older_entries(&mut self, older: FileEditJournal) {
        if older.entries.is_empty() {
            return;
        }
        // Build the combined entry list: older first, self's after.
        // Drop each entry's original sequence by overwriting below.
        let mut combined: Vec<FileEditEntry> = older
            .entries
            .into_iter()
            .chain(std::mem::take(&mut self.entries))
            .collect();

        // Apply ring-buffer cap: drop oldest if over.
        if combined.len() > self.max_entries {
            let drop_n = combined.len() - self.max_entries;
            combined.drain(..drop_n);
        }

        // Reassign contiguous sequences 0..combined.len().
        for (i, entry) in combined.iter_mut().enumerate() {
            entry.sequence = i as u64;
        }
        self.next_sequence = combined.len() as u64;
        self.entries = combined.into_iter().collect();
    }

    /// List files edited in a specific turn.
    pub fn files_in_turn(&self, turn_index: u32) -> Vec<&Path> {
        self.entries
            .iter()
            .filter(|e| e.turn_index == turn_index)
            .map(|e| e.path.as_path())
            .collect()
    }

    /// Summary of all edits (for display).
    pub fn summary(&self) -> Vec<(PathBuf, u32, EditType)> {
        self.entries
            .iter()
            .map(|e| (e.path.clone(), e.turn_index, e.edit_type))
            .collect()
    }

    // ── Persistence (F1–F5) ──────────────────────────────────────────────────

    /// Persist all current entries to `dir`, one `<seq:06>.json` file per
    /// entry. The directory is created if missing. Writes are atomic
    /// (tmp + rename) to survive partial crashes.
    ///
    /// **Destructive pruning**: any `<seq:06>.json` on disk whose
    /// sequence number is NOT in the current in-memory ring is deleted.
    /// This keeps the disk footprint bounded by `max_entries` without a
    /// separate GC pass, but it also means `save_to_dir` is NOT a safe
    /// operation on a dir shared with another process — it will delete
    /// the other process's entries. Callers must ensure exclusive
    /// ownership of `dir` for the lifetime of this journal.
    pub fn save_to_dir(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)?;

        // Collect current sequences so we can prune stale on-disk entries.
        let live: std::collections::HashSet<u64> =
            self.entries.iter().map(|e| e.sequence).collect();

        // Prune stale files (entries evicted from the ring buffer).
        if let Ok(read) = std::fs::read_dir(dir) {
            for entry in read.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let seq = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.parse::<u64>().ok());
                if let Some(seq) = seq
                    && !live.contains(&seq)
                {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }

        // Write each live entry atomically.
        for entry in &self.entries {
            let dest = dir.join(format!("{:06}.json", entry.sequence));
            let tmp = dir.join(format!(".{:06}.tmp", entry.sequence));
            let json = serde_json::to_vec(entry)
                .map_err(|e| io::Error::other(format!("entry serialize: {e}")))?;
            std::fs::write(&tmp, &json)?;
            std::fs::rename(&tmp, &dest)?;
        }
        // fsync the directory to ensure renames are durable on crash.
        // Linux guarantees directory fsync semantics; macOS/Windows treat it
        // as best-effort (kernel may no-op). Log at debug so failures are
        // observable without being noisy in the common no-op case.
        #[cfg(target_os = "linux")]
        {
            match std::fs::File::open(dir).and_then(|d| d.sync_all()) {
                Ok(()) => {}
                Err(e) => tracing::debug!(
                    dir = %dir.display(),
                    error = %e,
                    "journal: directory fsync failed (durability best-effort)"
                ),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            // Directory fsync is not reliably defined on non-Linux; rely on
            // rename(2) atomicity at the inode level. This is best-effort.
            let _ = dir;
        }
        Ok(())
    }

    /// Rebuild a journal from entries previously saved via [`Self::save_to_dir`].
    ///
    /// - Missing directory → empty journal (first-run case; not an error).
    /// - Malformed / unparseable files → skipped with a warning.
    /// - More entries on disk than `max_entries` → keep the newest
    ///   `max_entries` by sequence number; older files stay on disk until
    ///   the next `save_to_dir` prunes them.
    pub fn load_from_dir(dir: &Path, max_entries: usize) -> io::Result<Self> {
        let mut journal = Self::new(max_entries);
        if !dir.exists() {
            return Ok(journal);
        }

        let mut entries: Vec<FileEditEntry> = Vec::new();
        for dir_entry in std::fs::read_dir(dir)? {
            let dir_entry = match dir_entry {
                Ok(de) => de,
                Err(e) => {
                    astra_core::agent_warn!(
                        "file_edit_journal",
                        "skipping unreadable dir entry: {e}"
                    );
                    continue;
                }
            };
            let path = dir_entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // Skip tmp files (in-flight writes from a crashed session).
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'))
            {
                continue;
            }
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    astra_core::agent_warn!(
                        "file_edit_journal",
                        "skipping unreadable checkpoint file {}: {e}",
                        path.display()
                    );
                    continue;
                }
            };
            match serde_json::from_slice::<FileEditEntry>(&bytes) {
                Ok(entry) => entries.push(entry),
                Err(e) => {
                    astra_core::agent_warn!(
                        "file_edit_journal",
                        "skipping malformed checkpoint file {}: {e}",
                        path.display()
                    );
                }
            }
        }

        // Sort by sequence (chronological order) and enforce the cap
        // by keeping the newest N.
        entries.sort_by_key(|e| e.sequence);
        if entries.len() > max_entries {
            let drop_n = entries.len() - max_entries;
            entries.drain(..drop_n);
        }

        // Seed next_sequence past the highest loaded sequence so newly
        // recorded entries don't collide with restored ones.
        if let Some(max_seq) = entries.iter().map(|e| e.sequence).max() {
            journal.next_sequence = max_seq.saturating_add(1);
        }
        journal.entries = entries.into_iter().collect();

        Ok(journal)
    }

    /// Read-only iterator over all journaled entries in insertion order.
    ///
    /// Exposed for diagnostics and test assertions. Callers must not
    /// rely on this for mutation — modifying the ring buffer outside the
    /// `record_*` / `undo_*` / `enable_persistence` methods would break
    /// the FIFO + persistence invariants.
    pub fn entries(&self) -> impl Iterator<Item = &FileEditEntry> {
        self.entries.iter()
    }

    /// Test-only convenience: collect all entries into a Vec for
    /// assertion helpers. Prefer [`Self::entries`] in production code.
    #[cfg(test)]
    pub fn entries_for_test(&self) -> Vec<FileEditEntry> {
        self.entries.iter().cloned().collect()
    }

    // ── Internals ────────────────────────────────────────────────────────────

    fn push(&mut self, mut entry: FileEditEntry) {
        entry.sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        let evicted_sequence = if self.entries.len() >= self.max_entries {
            self.entries.pop_front().map(|e| e.sequence)
        } else {
            None
        };
        self.entries.push_back(entry);
        self.persist_last_and_evict(evicted_sequence);
    }

    /// If persistence is enabled, write the newest entry atomically and
    /// delete the file of any just-evicted entry. Errors are logged at
    /// warn and swallowed; persistence is best-effort and never blocks
    /// the in-memory journal.
    fn persist_last_and_evict(&self, evicted_sequence: Option<u64>) {
        let Some(dir) = self.persist_dir.as_ref() else {
            return;
        };
        let Some(entry) = self.entries.back() else {
            return;
        };
        if let Err(e) = std::fs::create_dir_all(dir) {
            astra_core::agent_warn!(
                "file_edit_journal",
                "cannot create persist dir {}: {e}",
                dir.display()
            );
            return;
        }
        let dest = dir.join(format!("{:06}.json", entry.sequence));
        let tmp = dir.join(format!(".{:06}.tmp", entry.sequence));
        match serde_json::to_vec(entry) {
            Ok(bytes) => {
                if let Err(e) =
                    std::fs::write(&tmp, &bytes).and_then(|_| std::fs::rename(&tmp, &dest))
                {
                    astra_core::agent_warn!(
                        "file_edit_journal",
                        "persist entry seq={} failed: {e}",
                        entry.sequence
                    );
                }
            }
            Err(e) => {
                astra_core::agent_warn!(
                    "file_edit_journal",
                    "serialize entry seq={} failed: {e}",
                    entry.sequence
                );
            }
        }
        // Clean up the evicted entry's file, if any.
        if let Some(seq) = evicted_sequence {
            let stale = dir.join(format!("{:06}.json", seq));
            let _ = std::fs::remove_file(stale);
        }
    }

    /// Re-persist a specific entry (identified by its monotonic sequence)
    /// without touching any other on-disk entry. Used by `record_after`
    /// to overwrite the pre-state entry with its completed after-state.
    fn persist_entry_by_sequence(&self, seq: u64) {
        let Some(dir) = self.persist_dir.as_ref() else {
            return;
        };
        let Some(entry) = self.entries.iter().find(|e| e.sequence == seq) else {
            return;
        };
        let dest = dir.join(format!("{:06}.json", seq));
        let tmp = dir.join(format!(".{:06}.tmp", seq));
        match serde_json::to_vec(entry) {
            Ok(bytes) => {
                if let Err(e) =
                    std::fs::write(&tmp, &bytes).and_then(|_| std::fs::rename(&tmp, &dest))
                {
                    astra_core::agent_warn!(
                        "file_edit_journal",
                        "re-persist entry seq={seq} failed: {e}"
                    );
                }
            }
            Err(e) => astra_core::agent_warn!(
                "file_edit_journal",
                "serialize entry seq={seq} failed: {e}"
            ),
        }
    }

    fn apply_revert(entry: &FileEditEntry) -> io::Result<()> {
        match &entry.before_content {
            Some(content) => {
                // Restore original content
                std::fs::write(&entry.path, content)?;
            }
            None => {
                // File was created by the agent — remove it
                if entry.path.exists() {
                    std::fs::remove_file(&entry.path)?;
                }
            }
        }
        Ok(())
    }
}

impl Default for FileEditJournal {
    fn default() -> Self {
        Self::new(500)
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── Persistence (F1 + F2) ─────────────────────────────────────────────

    /// FileEditEntry survives a serde_json roundtrip byte-identical.
    /// We persist with raw bytes serialized via serde-bytes-compatible
    /// `Vec<u8>` serialization (base64'd by serde_json as number arrays).
    /// Test covers all four edit types and the None-before-content case.
    // R9.1: merge_older_entries lets a journal that already holds
    // in-memory entries absorb an older (prior-run) set from disk
    // without either side being lost. After merge, the older side's
    // entries precede self's, and self's entries keep relative order
    // but are re-sequenced past the older set's max so future saves
    // don't collide on disk.
    #[test]
    fn merge_older_entries_preserves_both_sides_and_re_sequences() {
        let tmp = TempDir::new().unwrap();
        let file_a = tmp.path().join("a");
        let file_b = tmp.path().join("b");
        let file_x = tmp.path().join("x");

        // Older journal: two entries with sequences 0 and 1.
        let mut older = FileEditJournal::new(100);
        std::fs::write(&file_a, b"a0").unwrap();
        older.record_before(&file_a, "A", 0);
        older.record_after(&file_a, "A", b"a1");
        std::fs::write(&file_b, b"b0").unwrap();
        older.record_before(&file_b, "B", 0);
        older.record_after(&file_b, "B", b"b1");
        assert_eq!(older.len(), 2);

        // Self: one entry at sequence 0.
        let mut j = FileEditJournal::new(100);
        std::fs::write(&file_x, b"x0").unwrap();
        j.record_before(&file_x, "X", 1);
        j.record_after(&file_x, "X", b"x1");
        assert_eq!(j.len(), 1);

        j.merge_older_entries(older);

        // All 3 entries present.
        assert_eq!(j.len(), 3);
        let entries = j.entries_for_test();

        // Older entries come first (chronologically earlier).
        assert_eq!(entries[0].tool_call_id, "A");
        assert_eq!(entries[1].tool_call_id, "B");
        assert_eq!(entries[2].tool_call_id, "X");

        // All sequences unique and monotonic so save_to_dir doesn't overwrite.
        let seqs: Vec<u64> = entries.iter().map(|e| e.sequence).collect();
        let mut sorted = seqs.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 3, "sequences must be unique after merge");
        assert!(seqs.windows(2).all(|w| w[0] < w[1]), "sequences monotonic");

        // Subsequent record gets a sequence past all three.
        let max_existing = *seqs.iter().max().unwrap();
        std::fs::write(&file_x, b"x1").unwrap();
        j.record_before(&file_x, "Y", 2);
        let new_seq = j.entries_for_test().last().unwrap().sequence;
        assert!(
            new_seq > max_existing,
            "next_sequence must advance past max"
        );
    }

    /// When combined length exceeds max_entries, oldest entries are evicted
    /// (ring-buffer semantics preserved across merges).
    #[test]
    fn merge_older_entries_respects_max_entries_cap() {
        let tmp = TempDir::new().unwrap();
        let mk_entry = |j: &mut FileEditJournal, tag: &str| {
            let f = tmp.path().join(tag);
            std::fs::write(&f, b"x").unwrap();
            j.record_before(&f, tag, 0);
            j.record_after(&f, tag, b"y");
        };

        // Self with cap=3 and 2 entries.
        let mut j = FileEditJournal::new(3);
        mk_entry(&mut j, "self-0");
        mk_entry(&mut j, "self-1");

        // Older with 3 entries.
        let mut older = FileEditJournal::new(10);
        mk_entry(&mut older, "old-0");
        mk_entry(&mut older, "old-1");
        mk_entry(&mut older, "old-2");

        j.merge_older_entries(older);

        // 2 + 3 = 5 total, cap=3 → keep newest 3.
        assert_eq!(j.len(), 3);
        // Oldest (old-0, old-1) evicted; old-2 + both self-* survive.
        let tags: Vec<String> = j
            .entries_for_test()
            .iter()
            .map(|e| e.tool_call_id.clone())
            .collect();
        assert_eq!(tags, vec!["old-2", "self-0", "self-1"]);
    }

    /// T69: when self's cap is tighter than the older journal's size,
    /// and self is empty, merge keeps the newest N of older (not all of
    /// them). Pins "self's max_entries wins" — important because
    /// `load_from_dir` reads up to its own cap (typically 500), which
    /// may not match the bound journal's cap.
    #[test]
    fn merge_older_entries_self_cap_wins_when_tighter_than_older() {
        let tmp = TempDir::new().unwrap();
        let mk = |j: &mut FileEditJournal, tag: &str| {
            let f = tmp.path().join(tag);
            std::fs::write(&f, b"x").unwrap();
            j.record_before(&f, tag, 0);
            j.record_after(&f, tag, b"y");
        };

        // Self has cap=3, empty. Older has 5 entries (its own cap=10).
        let mut j = FileEditJournal::new(3);
        let mut older = FileEditJournal::new(10);
        mk(&mut older, "old-0");
        mk(&mut older, "old-1");
        mk(&mut older, "old-2");
        mk(&mut older, "old-3");
        mk(&mut older, "old-4");

        j.merge_older_entries(older);

        // Self.max_entries=3 wins: keep newest 3 of older.
        assert_eq!(j.len(), 3);
        let tags: Vec<String> = j
            .entries_for_test()
            .iter()
            .map(|e| e.tool_call_id.clone())
            .collect();
        assert_eq!(tags, vec!["old-2", "old-3", "old-4"]);
    }

    /// Merging an empty older journal is a no-op.
    #[test]
    fn merge_older_entries_empty_older_is_noop() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("a");
        std::fs::write(&file, b"a").unwrap();

        let mut j = FileEditJournal::new(100);
        j.record_before(&file, "A", 0);
        j.record_after(&file, "A", b"b");

        let before_len = j.len();
        let before_seq = j.checkpoint();
        j.merge_older_entries(FileEditJournal::new(100));
        assert_eq!(j.len(), before_len);
        assert_eq!(j.checkpoint(), before_seq);
    }

    #[test]
    fn entry_serde_roundtrip_preserves_fields() {
        let original = FileEditEntry {
            sequence: 42,
            path: PathBuf::from("/tmp/a.txt"),
            turn_index: 7,
            timestamp: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
            before_content: Some(vec![0x00, 0x01, 0xff, b'h', b'i']),
            after_content: vec![b'n', b'e', b'w'],
            tool_call_id: "call-xyz".into(),
            edit_type: EditType::Patch,
        };

        let json = serde_json::to_string(&original).expect("serialize");
        let back: FileEditEntry = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(back.sequence, original.sequence);
        assert_eq!(back.path, original.path);
        assert_eq!(back.turn_index, original.turn_index);
        assert_eq!(back.timestamp, original.timestamp);
        assert_eq!(back.before_content, original.before_content);
        assert_eq!(back.after_content, original.after_content);
        assert_eq!(back.tool_call_id, original.tool_call_id);
        assert_eq!(back.edit_type, original.edit_type);
    }

    /// Create edit (no prior content) roundtrips None correctly.
    #[test]
    fn entry_serde_roundtrip_none_before_content() {
        let original = FileEditEntry {
            sequence: 1,
            path: PathBuf::from("/tmp/new.txt"),
            turn_index: 0,
            timestamp: SystemTime::now(),
            before_content: None,
            after_content: b"hello".to_vec(),
            tool_call_id: "c".into(),
            edit_type: EditType::Create,
        };
        let json = serde_json::to_string(&original).unwrap();
        let back: FileEditEntry = serde_json::from_str(&json).unwrap();
        assert!(back.before_content.is_none());
        assert_eq!(back.edit_type, EditType::Create);
    }

    /// save_to_dir then load_from_dir reproduces the journal's entries
    /// in the same chronological order.
    #[test]
    fn journal_save_load_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let file_a = tmp.path().join("a.txt");
        let file_b = tmp.path().join("b.txt");
        std::fs::write(&file_a, "A0").unwrap();
        std::fs::write(&file_b, "B0").unwrap();

        let mut j = FileEditJournal::new(100);
        j.record_before(&file_a, "c-a", 0);
        j.record_after(&file_a, "c-a", b"A1");
        j.record_before(&file_b, "c-b", 0);
        j.record_after(&file_b, "c-b", b"B1");

        let persist_dir = tmp.path().join("persist");
        j.save_to_dir(&persist_dir).expect("save");

        let loaded = FileEditJournal::load_from_dir(&persist_dir, 100).expect("load");
        assert_eq!(loaded.len(), 2);
        // Entries preserve order.
        let entries = loaded.entries_for_test();
        assert_eq!(entries[0].path, file_a);
        assert_eq!(entries[0].after_content, b"A1");
        assert_eq!(entries[1].path, file_b);
        assert_eq!(entries[1].after_content, b"B1");
    }

    /// Loading from a non-existent dir returns an empty journal — this is
    /// the "first run" case, not an error.
    #[test]
    fn journal_load_missing_dir_is_empty() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let loaded = FileEditJournal::load_from_dir(&missing, 100).expect("load");
        assert_eq!(loaded.len(), 0);
    }

    /// Malformed entries on disk are skipped, not fatal. A partial write
    /// from a prior crashed process must not block subsequent sessions.
    #[test]
    fn journal_load_skips_malformed_files() {
        let tmp = TempDir::new().unwrap();
        let persist = tmp.path().join("persist");
        std::fs::create_dir_all(&persist).unwrap();
        std::fs::write(persist.join("000001.json"), "{this is not json}").unwrap();
        std::fs::write(persist.join("000002.json"), "[]").unwrap();

        let loaded = FileEditJournal::load_from_dir(&persist, 100).expect("load");
        assert_eq!(loaded.len(), 0, "malformed files must be skipped");
    }

    // F4: simulate a CLI restart. Record edits via a journal with
    // auto-persistence; drop it; spin up a fresh journal loading from
    // the same dir; verify undo works across the restart.
    #[test]
    fn journal_persistence_survives_restart_undo_works() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("project.txt");
        std::fs::write(&file, b"original").unwrap();
        let persist = tmp.path().join("checkpoints");

        // Session 1: write + record.
        {
            let mut j1 = FileEditJournal::new(100);
            j1.enable_persistence(persist.clone());
            j1.record_before(&file, "call-1", 0);
            std::fs::write(&file, b"modified").unwrap();
            j1.record_after(&file, "call-1", b"modified");
        } // j1 dropped — simulates CLI exit.

        // Verify the on-disk state has a complete entry.
        assert_eq!(std::fs::read(&file).unwrap(), b"modified");

        // Session 2: reload journal and undo.
        let j2 = FileEditJournal::load_from_dir(&persist, 100).unwrap();
        assert_eq!(j2.len(), 1, "one persisted entry must survive restart");

        let result = j2.undo_file(&file).unwrap();
        assert_eq!(result, Some(EditType::Overwrite));
        assert_eq!(
            std::fs::read(&file).unwrap(),
            b"original",
            "undo across restart must restore the pre-edit content"
        );
    }

    // Auto-persistence: eviction cleans up stale on-disk entries.
    #[test]
    fn auto_persistence_evicts_stale_files_on_ring_buffer_pop() {
        let tmp = TempDir::new().unwrap();
        let persist = tmp.path().join("cp");

        let mut j = FileEditJournal::new(2); // tiny ring
        j.enable_persistence(persist.clone());
        for i in 0..5 {
            let p = tmp.path().join(format!("f{i}.txt"));
            std::fs::write(&p, b"x").unwrap();
            j.record_before(&p, &format!("c{i}"), 0);
            j.record_after(&p, &format!("c{i}"), b"y");
        }

        // Only 2 JSON files should remain (seqs 8 and 9: each record_before
        // takes one seq, record_after re-persists the same seq).
        let files: Vec<_> = std::fs::read_dir(&persist)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
            .collect();
        assert_eq!(files.len(), 2, "evicted entries must be removed from disk");
    }

    /// F5: load respects max_entries cap — if 200 files are on disk with
    /// cap=50, we keep the newest 50 by sequence number.
    #[test]
    fn journal_load_respects_max_entries_cap() {
        let tmp = TempDir::new().unwrap();
        let persist = tmp.path().join("persist");
        std::fs::create_dir_all(&persist).unwrap();

        // Write 20 entries with monotonic sequences.
        for i in 0..20u64 {
            let entry = FileEditEntry {
                sequence: i,
                path: PathBuf::from(format!("/tmp/{i}.txt")),
                turn_index: 0,
                timestamp: SystemTime::UNIX_EPOCH,
                before_content: None,
                after_content: Vec::new(),
                tool_call_id: format!("c{i}"),
                edit_type: EditType::Create,
            };
            let json = serde_json::to_string(&entry).unwrap();
            std::fs::write(persist.join(format!("{i:06}.json")), json).unwrap();
        }

        let loaded = FileEditJournal::load_from_dir(&persist, 5).expect("load");
        assert_eq!(loaded.len(), 5);
        // Newest 5 are sequences 15..=19.
        let entries = loaded.entries_for_test();
        assert_eq!(entries.first().unwrap().sequence, 15);
        assert_eq!(entries.last().unwrap().sequence, 19);
    }

    #[test]
    fn record_and_undo_overwrite() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("test.txt");
        std::fs::write(&file, "original").unwrap();

        let mut journal = FileEditJournal::new(100);
        journal.record_before(&file, "call-1", 0);

        // Simulate write
        std::fs::write(&file, "modified").unwrap();
        journal.record_after(&file, "call-1", b"modified");

        assert_eq!(std::fs::read_to_string(&file).unwrap(), "modified");

        // Undo
        let result = journal.undo_file(&file).unwrap();
        assert_eq!(result, Some(EditType::Overwrite));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "original");
    }

    #[test]
    fn record_and_undo_create() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("new.txt");

        let mut journal = FileEditJournal::new(100);
        journal.record_before(&file, "call-1", 0);

        // Simulate create
        std::fs::write(&file, "new content").unwrap();
        journal.record_after(&file, "call-1", b"new content");

        assert!(file.exists());

        // Undo should delete the file
        let result = journal.undo_file(&file).unwrap();
        assert_eq!(result, Some(EditType::Create));
        assert!(!file.exists());
    }

    #[test]
    fn undo_turn_reverts_multiple_files() {
        let tmp = TempDir::new().unwrap();
        let file_a = tmp.path().join("a.txt");
        let file_b = tmp.path().join("b.txt");
        std::fs::write(&file_a, "A original").unwrap();
        std::fs::write(&file_b, "B original").unwrap();

        let mut journal = FileEditJournal::new(100);

        // Turn 5: edit both files
        journal.record_before(&file_a, "call-1", 5);
        std::fs::write(&file_a, "A modified").unwrap();
        journal.record_after(&file_a, "call-1", b"A modified");

        journal.record_before(&file_b, "call-2", 5);
        std::fs::write(&file_b, "B modified").unwrap();
        journal.record_after(&file_b, "call-2", b"B modified");

        let result = journal.undo_turn(5);
        assert_eq!(result.reverted.len(), 2);
        assert!(result.failed.is_empty());
        assert_eq!(std::fs::read_to_string(&file_a).unwrap(), "A original");
        assert_eq!(std::fs::read_to_string(&file_b).unwrap(), "B original");
    }

    #[test]
    fn undo_nonexistent_file_returns_none() {
        let journal = FileEditJournal::new(100);
        let result = journal.undo_file(Path::new("/nonexistent")).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn lru_eviction() {
        let mut journal = FileEditJournal::new(3);
        for i in 0..5 {
            let path = PathBuf::from(format!("/tmp/test_{i}.txt"));
            journal.push(FileEditEntry {
                sequence: 0,
                path,
                turn_index: i,
                timestamp: SystemTime::now(),
                before_content: None,
                after_content: Vec::new(),
                tool_call_id: format!("call-{i}"),
                edit_type: EditType::Create,
            });
        }
        assert_eq!(journal.len(), 3);
        // Oldest (0, 1) should be evicted; entries 2, 3, 4 remain
        assert_eq!(journal.entries.front().unwrap().turn_index, 2);
    }

    #[test]
    fn files_in_turn() {
        let mut journal = FileEditJournal::new(100);
        let push = |j: &mut FileEditJournal, path: &str, turn: u32| {
            j.push(FileEditEntry {
                sequence: 0,
                path: PathBuf::from(path),
                turn_index: turn,
                timestamp: SystemTime::now(),
                before_content: None,
                after_content: Vec::new(),
                tool_call_id: "x".to_string(),
                edit_type: EditType::Create,
            });
        };
        push(&mut journal, "/a", 1);
        push(&mut journal, "/b", 2);
        push(&mut journal, "/c", 1);

        let turn_1_files = journal.files_in_turn(1);
        assert_eq!(turn_1_files.len(), 2);
    }

    #[test]
    fn record_before_patch() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("patched.txt");
        std::fs::write(&file, "line1\nline2\nline3\n").unwrap();

        let mut journal = FileEditJournal::new(100);
        journal.record_before_patch(&file, "call-p", 0);

        assert_eq!(journal.entries.back().unwrap().edit_type, EditType::Patch);
    }

    #[test]
    fn record_and_undo_delete() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("gone.txt");
        std::fs::write(&file, "restore me").unwrap();

        let mut journal = FileEditJournal::new(100);
        let before = std::fs::read(&file).unwrap();
        std::fs::remove_file(&file).unwrap();
        journal.record_delete(&file, "call-d", 2, before);

        assert!(!file.exists());

        let result = journal.undo_file(&file).unwrap();
        assert_eq!(result, Some(EditType::Delete));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "restore me");
    }

    #[test]
    fn undo_turn_since_only_reverts_entries_after_checkpoint() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("test.txt");
        std::fs::write(&file, "original").unwrap();

        let mut journal = FileEditJournal::new(100);
        journal.record_before(&file, "call-1", 4);
        std::fs::write(&file, "first").unwrap();
        journal.record_after(&file, "call-1", b"first");

        let checkpoint = journal.checkpoint();

        journal.record_before(&file, "call-2", 4);
        std::fs::write(&file, "second").unwrap();
        journal.record_after(&file, "call-2", b"second");

        let result = journal.undo_turn_since(4, checkpoint);
        assert_eq!(result.reverted.len(), 1);
        assert!(result.failed.is_empty());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "first");
    }

    /// Merge with cap eviction followed by save_to_dir must leave exactly
    /// cap files on disk, and the evicted older entries must NOT be present.
    #[test]
    fn merge_cap_eviction_then_save_to_dir_disk_consistent() {
        let tmp = TempDir::new().unwrap();
        let persist_dir = tmp.path().join("journal");

        let mk = |j: &mut FileEditJournal, tag: &str| {
            let f = tmp.path().join(tag);
            std::fs::write(&f, b"x").unwrap();
            j.record_before(&f, tag, 0);
            j.record_after(&f, tag, b"y");
        };

        // Simulate a prior run with 4 entries on disk.
        let mut prior = FileEditJournal::new(10);
        mk(&mut prior, "prior-0");
        mk(&mut prior, "prior-1");
        mk(&mut prior, "prior-2");
        mk(&mut prior, "prior-3");
        prior.save_to_dir(&persist_dir).unwrap();
        let disk_before: Vec<_> = std::fs::read_dir(&persist_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        assert_eq!(disk_before.len(), 4);

        // Current session: cap=3, 1 pre-session entry.
        let mut current = FileEditJournal::new(3);
        mk(&mut current, "new-0");

        // Load disk into a temp journal and merge as "older".
        let older = FileEditJournal::load_from_dir(&persist_dir, 500).unwrap();
        assert_eq!(older.len(), 4);
        current.merge_older_entries(older);

        // 4 + 1 = 5 total, cap=3 → keep newest 3: prior-2, prior-3, new-0
        assert_eq!(current.len(), 3);

        // Save merged state back to disk.
        current.save_to_dir(&persist_dir).unwrap();

        // Disk must have exactly 3 files (evicted prior-0, prior-1 removed).
        let disk_after: Vec<_> = std::fs::read_dir(&persist_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        assert_eq!(disk_after.len(), 3, "disk must match cap after merge+save");

        // Reload and verify content consistency.
        let reloaded = FileEditJournal::load_from_dir(&persist_dir, 10).unwrap();
        assert_eq!(reloaded.len(), 3);
        let tags: Vec<String> = reloaded
            .entries_for_test()
            .iter()
            .map(|e| e.tool_call_id.clone())
            .collect();
        assert_eq!(tags, vec!["prior-2", "prior-3", "new-0"]);
    }
}
