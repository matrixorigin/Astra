//! Transparent filesystem checkpoint system.
//!
//! Before every file-mutating tool call, this module snapshots affected files
//! so users can undo changes via `/undo`. Works without git — pure filesystem.

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Maximum number of snapshots retained before evicting the oldest.
const DEFAULT_MAX_SNAPSHOTS: usize = 100;

/// A single file backup within a snapshot.
#[derive(Debug, Clone)]
pub struct FileBackup {
    /// The original file path that was backed up.
    pub original_path: PathBuf,
    /// Path to the backup copy (inside the backup directory).
    pub backup_path: PathBuf,
    /// Whether the file existed before the mutation. If `false`, undo means delete.
    pub existed: bool,
}

/// A point-in-time snapshot of one or more files.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Monotonically increasing snapshot identifier.
    pub id: usize,
    /// When the snapshot was taken.
    pub timestamp: Instant,
    /// The files captured in this snapshot.
    pub files: Vec<FileBackup>,
}

/// Summary of differences since a snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffStats {
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
}

/// Manages per-session file history with bounded snapshot storage.
#[derive(Debug)]
pub struct FileHistory {
    backup_dir: PathBuf,
    snapshots: VecDeque<Snapshot>,
    max_snapshots: usize,
    next_id: usize,
}

impl FileHistory {
    /// Create a new `FileHistory` backed by `~/.astra/file-history/<session_id>/`.
    pub fn new(session_id: &str) -> Self {
        let base = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".astra")
            .join("file-history")
            .join(session_id);
        Self {
            backup_dir: base,
            snapshots: VecDeque::new(),
            max_snapshots: DEFAULT_MAX_SNAPSHOTS,
            next_id: 0,
        }
    }

    /// Create with a custom backup directory (useful for testing).
    pub fn with_backup_dir(backup_dir: PathBuf, max_snapshots: usize) -> Self {
        Self {
            backup_dir,
            snapshots: VecDeque::new(),
            max_snapshots,
            next_id: 0,
        }
    }

    /// Take a checkpoint of the given file paths before mutation.
    ///
    /// Returns the snapshot ID on success.
    pub fn checkpoint(&mut self, paths: &[&Path]) -> io::Result<usize> {
        let snap_id = self.next_id;
        self.next_id += 1;

        let snap_dir = self.backup_dir.join(format!("snap_{snap_id}"));
        fs::create_dir_all(&snap_dir)?;

        let mut file_backups = Vec::with_capacity(paths.len());

        for &path in paths {
            let existed = path.exists();
            // Derive a safe relative backup name from the absolute path.
            let relative = sanitize_path_for_backup(path);
            let backup_path = snap_dir.join(&relative);

            if existed {
                // Ensure parent directories exist in the backup tree.
                if let Some(parent) = backup_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                // Always copy — hard links share the inode so in-place writes
                // (e.g. fs::write which truncates + writes) would mutate the
                // backup. A copy guarantees snapshot isolation.
                fs::copy(path, &backup_path)?;
            }

            file_backups.push(FileBackup {
                original_path: path.to_path_buf(),
                backup_path,
                existed,
            });
        }

        let snapshot = Snapshot {
            id: snap_id,
            timestamp: Instant::now(),
            files: file_backups,
        };

        self.snapshots.push_back(snapshot);

        // Evict oldest if over capacity.
        while self.snapshots.len() > self.max_snapshots {
            if let Some(old) = self.snapshots.pop_front() {
                let old_dir = self.backup_dir.join(format!("snap_{}", old.id));
                let _ = fs::remove_dir_all(&old_dir);
            }
        }

        Ok(snap_id)
    }

    /// Revert all files in a specific snapshot to their backed-up state.
    pub fn revert_to(&self, snapshot_id: usize) -> io::Result<()> {
        let snapshot = self
            .snapshots
            .iter()
            .find(|s| s.id == snapshot_id)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("snapshot {snapshot_id} not found"),
                )
            })?;

        for backup in &snapshot.files {
            if backup.existed {
                // Restore from backup.
                if let Some(parent) = backup.original_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(&backup.backup_path, &backup.original_path)?;
            } else {
                // File didn't exist before — delete it if it exists now.
                if backup.original_path.exists() {
                    fs::remove_file(&backup.original_path)?;
                }
            }
        }

        Ok(())
    }

    /// Revert the most recent snapshot and remove it from history.
    ///
    /// Returns the snapshot ID that was reverted, or `None` if no snapshots exist.
    pub fn undo_last(&mut self) -> io::Result<Option<usize>> {
        let snapshot = match self.snapshots.pop_back() {
            Some(s) => s,
            None => return Ok(None),
        };

        let snap_id = snapshot.id;

        for backup in &snapshot.files {
            if backup.existed {
                if let Some(parent) = backup.original_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(&backup.backup_path, &backup.original_path)?;
            } else {
                if backup.original_path.exists() {
                    fs::remove_file(&backup.original_path)?;
                }
            }
        }

        // Clean up the snapshot directory.
        let snap_dir = self.backup_dir.join(format!("snap_{snap_id}"));
        let _ = fs::remove_dir_all(&snap_dir);

        Ok(Some(snap_id))
    }

    /// Compute diff statistics between a snapshot and the current file state.
    pub fn diff_since(&self, snapshot_id: usize) -> io::Result<DiffStats> {
        let snapshot = self
            .snapshots
            .iter()
            .find(|s| s.id == snapshot_id)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("snapshot {snapshot_id} not found"),
                )
            })?;

        let mut stats = DiffStats::default();

        for backup in &snapshot.files {
            let current_content = if backup.original_path.exists() {
                fs::read_to_string(&backup.original_path).ok()
            } else {
                None
            };

            let backup_content = if backup.existed {
                fs::read_to_string(&backup.backup_path).ok()
            } else {
                None
            };

            match (&backup_content, &current_content) {
                (Some(old), Some(new)) => {
                    if old != new {
                        stats.files_changed += 1;
                        let (ins, del) = line_diff_counts(old, new);
                        stats.insertions += ins;
                        stats.deletions += del;
                    }
                }
                (None, Some(new)) => {
                    // File was newly created.
                    stats.files_changed += 1;
                    stats.insertions += new.lines().count();
                }
                (Some(old), None) => {
                    // File was deleted.
                    stats.files_changed += 1;
                    stats.deletions += old.lines().count();
                }
                (None, None) => {
                    // Both non-existent — no change.
                }
            }
        }

        Ok(stats)
    }

    /// List all snapshots currently tracked.
    pub fn list_snapshots(&self) -> &VecDeque<Snapshot> {
        &self.snapshots
    }

    /// Return number of snapshots stored.
    pub fn snapshot_count(&self) -> usize {
        self.snapshots.len()
    }
}

/// Compute a simple line-based diff: (insertions, deletions).
fn line_diff_counts(old: &str, new: &str) -> (usize, usize) {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    // Simple diff approximation: count lines present only in new (insertions)
    // and lines present only in old (deletions).
    let mut insertions = 0;
    let mut deletions = 0;

    // Use a basic LCS-approximation via matching.
    let mut old_used = vec![false; old_lines.len()];
    let mut new_used = vec![false; new_lines.len()];

    // First pass: mark exact matches in order (greedy).
    let mut oi = 0;
    let mut ni = 0;
    while oi < old_lines.len() && ni < new_lines.len() {
        if old_lines[oi] == new_lines[ni] {
            old_used[oi] = true;
            new_used[ni] = true;
            oi += 1;
            ni += 1;
        } else {
            // Try to find old_lines[oi] in new_lines ahead.
            let found_in_new = new_lines[ni..]
                .iter()
                .position(|l| *l == old_lines[oi])
                .map(|p| p + ni);
            let found_in_old = old_lines[oi..]
                .iter()
                .position(|l| *l == new_lines[ni])
                .map(|p| p + oi);

            match (found_in_new, found_in_old) {
                (Some(npos), Some(opos)) => {
                    // Pick the closer match.
                    if (npos - ni) <= (opos - oi) {
                        // Lines ni..npos are insertions.
                        ni = npos;
                    } else {
                        // Lines oi..opos are deletions.
                        oi = opos;
                    }
                }
                (Some(npos), None) => {
                    ni = npos;
                }
                (None, Some(opos)) => {
                    oi = opos;
                }
                (None, None) => {
                    oi += 1;
                    ni += 1;
                }
            }
        }
    }

    for (i, used) in old_used.iter().enumerate() {
        if !*used {
            // Check if this line appears in any unused new line (unmatched).
            let _ = i; // suppress unused warning
            deletions += 1;
        }
    }
    for used in &new_used {
        if !*used {
            insertions += 1;
        }
    }

    (insertions, deletions)
}

/// Convert an absolute path to a sanitized relative path for the backup tree.
fn sanitize_path_for_backup(path: &Path) -> PathBuf {
    // Strip the root prefix and join with underscores or preserve structure.
    let components: Vec<&str> = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    PathBuf::from(components.join("/"))
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_history(tmp: &TempDir) -> FileHistory {
        FileHistory::with_backup_dir(tmp.path().join("backups"), DEFAULT_MAX_SNAPSHOTS)
    }

    #[test]
    fn test_checkpoint_creates_backup_for_existing_file() {
        let tmp = TempDir::new().unwrap();
        let mut history = make_history(&tmp);

        // Create a file to checkpoint.
        let file_path = tmp.path().join("hello.txt");
        fs::write(&file_path, "original content").unwrap();

        let snap_id = history.checkpoint(&[file_path.as_path()]).unwrap();
        assert_eq!(snap_id, 0);

        // Verify backup exists and has correct content.
        let snapshots = history.list_snapshots();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].files.len(), 1);
        assert!(snapshots[0].files[0].existed);
        let backup_content = fs::read_to_string(&snapshots[0].files[0].backup_path).unwrap();
        assert_eq!(backup_content, "original content");
    }

    #[test]
    fn test_checkpoint_for_new_file_records_non_existence() {
        let tmp = TempDir::new().unwrap();
        let mut history = make_history(&tmp);

        // Path that does NOT exist.
        let file_path = tmp.path().join("nonexistent.txt");
        let snap_id = history.checkpoint(&[file_path.as_path()]).unwrap();
        assert_eq!(snap_id, 0);

        let snapshots = history.list_snapshots();
        assert_eq!(snapshots.len(), 1);
        assert!(!snapshots[0].files[0].existed);
        // Backup file should NOT exist on disk.
        assert!(!snapshots[0].files[0].backup_path.exists());
    }

    #[test]
    fn test_revert_restores_file_content() {
        let tmp = TempDir::new().unwrap();
        let mut history = make_history(&tmp);

        let file_path = tmp.path().join("data.txt");
        fs::write(&file_path, "version 1").unwrap();

        let snap_id = history.checkpoint(&[file_path.as_path()]).unwrap();

        // Mutate the file.
        fs::write(&file_path, "version 2").unwrap();
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "version 2");

        // Revert.
        history.revert_to(snap_id).unwrap();
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "version 1");
    }

    #[test]
    fn test_revert_of_new_file_deletes_it() {
        let tmp = TempDir::new().unwrap();
        let mut history = make_history(&tmp);

        let file_path = tmp.path().join("new_file.txt");
        // File doesn't exist — take a checkpoint.
        let snap_id = history.checkpoint(&[file_path.as_path()]).unwrap();

        // Now "create" the file (simulating the tool creating it).
        fs::write(&file_path, "new content").unwrap();
        assert!(file_path.exists());

        // Revert should delete it.
        history.revert_to(snap_id).unwrap();
        assert!(!file_path.exists());
    }

    #[test]
    fn test_multiple_snapshots_tracked_correctly() {
        let tmp = TempDir::new().unwrap();
        let mut history = make_history(&tmp);

        let file_path = tmp.path().join("multi.txt");
        fs::write(&file_path, "v1").unwrap();
        let id0 = history.checkpoint(&[file_path.as_path()]).unwrap();

        fs::write(&file_path, "v2").unwrap();
        let id1 = history.checkpoint(&[file_path.as_path()]).unwrap();

        fs::write(&file_path, "v3").unwrap();
        let id2 = history.checkpoint(&[file_path.as_path()]).unwrap();

        assert_eq!(id0, 0);
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(history.snapshot_count(), 3);

        // Revert to v1 (snapshot 0).
        history.revert_to(id0).unwrap();
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "v1");

        // Revert to v2 (snapshot 1).
        history.revert_to(id1).unwrap();
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "v2");

        // Revert to v3 (snapshot 2).
        history.revert_to(id2).unwrap();
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "v3");
    }

    #[test]
    fn test_max_snapshot_eviction() {
        let tmp = TempDir::new().unwrap();
        let max = 5;
        let mut history = FileHistory::with_backup_dir(tmp.path().join("backups"), max);

        let file_path = tmp.path().join("evict.txt");
        fs::write(&file_path, "content").unwrap();

        // Create max + 3 snapshots.
        for _ in 0..(max + 3) {
            history.checkpoint(&[file_path.as_path()]).unwrap();
        }

        // Should only have `max` snapshots retained.
        assert_eq!(history.snapshot_count(), max);

        // The oldest snapshot IDs (0, 1, 2) should have been evicted.
        let remaining_ids: Vec<usize> = history.list_snapshots().iter().map(|s| s.id).collect();
        assert_eq!(remaining_ids, vec![3, 4, 5, 6, 7]);

        // Evicted snapshot directories should be cleaned up.
        assert!(!tmp.path().join("backups/snap_0").exists());
        assert!(!tmp.path().join("backups/snap_1").exists());
        assert!(!tmp.path().join("backups/snap_2").exists());
    }

    #[test]
    fn test_undo_last_returns_most_recent_and_removes() {
        let tmp = TempDir::new().unwrap();
        let mut history = make_history(&tmp);

        let file_path = tmp.path().join("undo.txt");
        fs::write(&file_path, "original").unwrap();
        history.checkpoint(&[file_path.as_path()]).unwrap();

        fs::write(&file_path, "modified").unwrap();
        history.checkpoint(&[file_path.as_path()]).unwrap();

        fs::write(&file_path, "latest").unwrap();

        // Undo last should revert to "modified" state.
        let undone = history.undo_last().unwrap();
        assert_eq!(undone, Some(1));
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "modified");
        assert_eq!(history.snapshot_count(), 1);

        // Undo again should revert to "original".
        let undone = history.undo_last().unwrap();
        assert_eq!(undone, Some(0));
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "original");
        assert_eq!(history.snapshot_count(), 0);

        // Undo with nothing left should return None.
        let undone = history.undo_last().unwrap();
        assert_eq!(undone, None);
    }

    #[test]
    fn test_diff_stats_computation() {
        let tmp = TempDir::new().unwrap();
        let mut history = make_history(&tmp);

        let file_path = tmp.path().join("diff.txt");
        fs::write(&file_path, "line1\nline2\nline3\n").unwrap();

        let snap_id = history.checkpoint(&[file_path.as_path()]).unwrap();

        // Modify the file.
        fs::write(&file_path, "line1\nmodified\nline3\nnew_line\n").unwrap();

        let stats = history.diff_since(snap_id).unwrap();
        assert_eq!(stats.files_changed, 1);
        // "line2" deleted, "modified" and "new_line" inserted.
        assert!(stats.insertions > 0);
        assert!(stats.deletions > 0);
    }

    #[test]
    fn test_diff_stats_new_file_created() {
        let tmp = TempDir::new().unwrap();
        let mut history = make_history(&tmp);

        let file_path = tmp.path().join("brand_new.txt");
        let snap_id = history.checkpoint(&[file_path.as_path()]).unwrap();

        // Create the file after checkpoint.
        fs::write(&file_path, "hello\nworld\n").unwrap();

        let stats = history.diff_since(snap_id).unwrap();
        assert_eq!(stats.files_changed, 1);
        assert_eq!(stats.insertions, 2);
        assert_eq!(stats.deletions, 0);
    }

    #[test]
    fn test_diff_stats_file_deleted() {
        let tmp = TempDir::new().unwrap();
        let mut history = make_history(&tmp);

        let file_path = tmp.path().join("to_delete.txt");
        fs::write(&file_path, "a\nb\nc\n").unwrap();

        let snap_id = history.checkpoint(&[file_path.as_path()]).unwrap();

        // Delete the file.
        fs::remove_file(&file_path).unwrap();

        let stats = history.diff_since(snap_id).unwrap();
        assert_eq!(stats.files_changed, 1);
        assert_eq!(stats.insertions, 0);
        assert_eq!(stats.deletions, 3);
    }

    #[test]
    fn test_diff_stats_no_change() {
        let tmp = TempDir::new().unwrap();
        let mut history = make_history(&tmp);

        let file_path = tmp.path().join("stable.txt");
        fs::write(&file_path, "unchanged").unwrap();

        let snap_id = history.checkpoint(&[file_path.as_path()]).unwrap();

        let stats = history.diff_since(snap_id).unwrap();
        assert_eq!(stats.files_changed, 0);
        assert_eq!(stats.insertions, 0);
        assert_eq!(stats.deletions, 0);
    }

    #[test]
    fn test_revert_nonexistent_snapshot_returns_error() {
        let tmp = TempDir::new().unwrap();
        let history = make_history(&tmp);

        let result = history.revert_to(999);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn test_diff_nonexistent_snapshot_returns_error() {
        let tmp = TempDir::new().unwrap();
        let history = make_history(&tmp);

        let result = history.diff_since(42);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn test_checkpoint_missing_file_gracefully_handled() {
        let tmp = TempDir::new().unwrap();
        let mut history = make_history(&tmp);

        // The path doesn't exist — should record existed=false without error.
        let missing = tmp.path().join("ghost.txt");
        let snap_id = history.checkpoint(&[missing.as_path()]).unwrap();
        assert_eq!(snap_id, 0);

        let snapshots = history.list_snapshots();
        assert!(!snapshots[0].files[0].existed);
    }

    #[test]
    fn test_multiple_files_in_single_checkpoint() {
        let tmp = TempDir::new().unwrap();
        let mut history = make_history(&tmp);

        let file_a = tmp.path().join("a.txt");
        let file_b = tmp.path().join("b.txt");
        fs::write(&file_a, "aaa").unwrap();
        fs::write(&file_b, "bbb").unwrap();

        let snap_id = history.checkpoint(&[file_a.as_path(), file_b.as_path()]).unwrap();

        // Modify both.
        fs::write(&file_a, "AAA").unwrap();
        fs::write(&file_b, "BBB").unwrap();

        // Revert should restore both.
        history.revert_to(snap_id).unwrap();
        assert_eq!(fs::read_to_string(&file_a).unwrap(), "aaa");
        assert_eq!(fs::read_to_string(&file_b).unwrap(), "bbb");
    }

    #[test]
    fn test_undo_last_for_new_file_deletes_it() {
        let tmp = TempDir::new().unwrap();
        let mut history = make_history(&tmp);

        let file_path = tmp.path().join("created_by_tool.txt");
        history.checkpoint(&[file_path.as_path()]).unwrap();

        // Simulate tool creating the file.
        fs::write(&file_path, "tool output").unwrap();
        assert!(file_path.exists());

        // Undo should delete the file.
        history.undo_last().unwrap();
        assert!(!file_path.exists());
    }

    #[test]
    fn test_snapshot_ids_monotonically_increase() {
        let tmp = TempDir::new().unwrap();
        let mut history = make_history(&tmp);

        let file_path = tmp.path().join("seq.txt");
        fs::write(&file_path, "x").unwrap();

        let id0 = history.checkpoint(&[file_path.as_path()]).unwrap();
        let id1 = history.checkpoint(&[file_path.as_path()]).unwrap();
        let id2 = history.checkpoint(&[file_path.as_path()]).unwrap();

        assert_eq!(id0, 0);
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
    }

    #[test]
    fn test_sanitize_path_for_backup() {
        let path = Path::new("/home/user/project/src/main.rs");
        let sanitized = sanitize_path_for_backup(path);
        assert_eq!(sanitized, PathBuf::from("home/user/project/src/main.rs"));
    }
}
