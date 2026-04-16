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

// ─── Types ──────────────────────────────────────────────────────────────────

/// Type of file mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone)]
pub struct FileEditEntry {
    /// Monotonic sequence number assigned when the entry is recorded.
    sequence: u64,
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
}

impl FileEditJournal {
    /// Create a new journal with the given capacity.
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(max_entries.min(1024)),
            max_entries,
            next_sequence: 0,
        }
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
        for entry in self.entries.iter_mut().rev() {
            if entry.path == path && entry.tool_call_id == tool_call_id {
                entry.after_content = content.to_vec();
                return;
            }
        }
        astra_core::agent_warn!(
            "file_edit",
            "record_after: no matching entry for path={} tool_call_id={}",
            path.display(),
            tool_call_id
        );
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

    // ── Internals ────────────────────────────────────────────────────────────

    fn push(&mut self, mut entry: FileEditEntry) {
        entry.sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        if self.entries.len() >= self.max_entries {
            self.entries.pop_front(); // evict oldest
        }
        self.entries.push_back(entry);
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
}
