//! File state tracking for staleness detection and read deduplication.
//!
//! Tracks mtime after each read/write/edit to prevent overwriting user edits
//! and skip re-reading unchanged files. Inspired by Claude Code's readFileState.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use super::{ToolExecutor, passive_cargo_check, passive_tsc_check};

/// Maximum number of entries in the file state cache. When exceeded, the
/// entry with the oldest timestamp is evicted.
const MAX_FILE_STATE_ENTRIES: usize = 200;

/// Shape of the last `read_file` call, for consecutive-request dedup (same idea as
/// Claude Code `FileReadTool`: same offset+limit + unchanged mtime → stub before I/O).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReadDedupKey {
    Full,
    Outline,
    /// Raw `start_line` / `end_line` JSON (absent key = `None`), like Claude's offset/limit.
    Range {
        start_line: Option<u64>,
        end_line: Option<u64>,
    },
}

/// Tracks the last-read state of a file for staleness detection and dedup.
/// Inspired by Claude Code's readFileState mechanism.
pub(super) struct FileState {
    /// mtime (milliseconds) at the time of last read/write.
    pub(super) timestamp_ms: u128,
    /// True if the last operation was a read (not a write/edit).
    /// Dedup only fires when the previous op was a read.
    pub(super) from_read: bool,
    /// True if the last read was a partial view (outline, line range).
    pub(super) is_partial: bool,
    /// How many times this file has been fully read.
    /// Used for escalating warnings when the model loops on the same file.
    pub(super) read_count: u32,
    /// How many times this file has been read with different ranges.
    /// Used to nudge the model toward grep for large files.
    pub(super) ranged_read_count: u32,
    /// Last read_file request shape (updated on every successful read).
    pub(super) last_dedup_key: ReadDedupKey,
}

impl ToolExecutor {
    // ─── File state helpers ──────────────────────────────────────────────────

    /// Get the mtime of a file in milliseconds. Returns 0 on error.
    pub(super) fn file_mtime_ms(path: &Path) -> u128 {
        fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis())
            .unwrap_or(0)
    }

    /// Record file state after a read.
    pub(super) fn record_read(&self, path: &Path, is_partial: bool, last_dedup_key: ReadDedupKey) {
        let ts = Self::file_mtime_ms(path);
        if let Ok(mut state) = self.file_state.lock() {
            let prev = state.get(path);
            let prev_count = prev.map(|fs| fs.read_count).unwrap_or(0);
            let prev_ranged = prev.map(|fs| fs.ranged_read_count).unwrap_or(0);
            // Only increment read_count for full (non-partial) reads.
            // Ranged reads of different sections are expected behavior
            // (guided by the size gate), not wasteful repetition.
            let new_count = if is_partial {
                prev_count
            } else {
                prev_count + 1
            };
            let new_ranged = if is_partial {
                prev_ranged + 1
            } else {
                prev_ranged
            };
            state.insert(
                path.to_path_buf(),
                FileState {
                    timestamp_ms: ts,
                    from_read: true,
                    is_partial,
                    read_count: new_count,
                    ranged_read_count: new_ranged,
                    last_dedup_key,
                },
            );
            // LRU eviction: keep at most MAX_FILE_STATE_ENTRIES
            if state.len() > MAX_FILE_STATE_ENTRIES
                && let Some(oldest_key) = state
                    .iter()
                    .min_by_key(|(_, fs)| fs.timestamp_ms)
                    .map(|(k, _)| k.clone())
            {
                state.remove(&oldest_key);
            }
        }
    }

    fn record_write_impl(&self, path: &Path, content: Option<&str>) {
        if passive_cargo_check::should_schedule_passive_cargo(&self.project_root, path) {
            self.passive_cargo_pending.store(true, Ordering::SeqCst);
        }
        if passive_tsc_check::should_schedule_passive_tsc(&self.project_root, path) {
            self.passive_tsc_pending.store(true, Ordering::SeqCst);
        }
        match content {
            Some(text) => {
                self.passive_lsp
                    .sync_after_write_with_content(&self.project_root, path, text)
            }
            None => self.passive_lsp.sync_after_write(&self.project_root, path),
        }
        let ts = Self::file_mtime_ms(path);
        if let Ok(mut state) = self.file_state.lock() {
            state.insert(
                path.to_path_buf(),
                FileState {
                    timestamp_ms: ts,
                    from_read: false,
                    is_partial: false,
                    read_count: 0,
                    ranged_read_count: 0,
                    last_dedup_key: ReadDedupKey::Full,
                },
            );
            // LRU eviction: keep at most MAX_FILE_STATE_ENTRIES
            if state.len() > MAX_FILE_STATE_ENTRIES
                && let Some(oldest_key) = state
                    .iter()
                    .min_by_key(|(_, fs)| fs.timestamp_ms)
                    .map(|(k, _)| k.clone())
            {
                state.remove(&oldest_key);
            }
        }
    }

    /// Record file state after a write/edit when only the path is known.
    /// Uses from_read=false to distinguish from reads — dedup won't fire after writes.
    pub(super) fn record_write(&self, path: &Path) {
        self.record_write_impl(path, None);
    }

    /// Record file state after a write/edit when the new content is already known.
    pub(super) fn record_write_with_content(&self, path: &Path, content: &str) {
        self.record_write_impl(path, Some(content));
    }

    /// Check if a file has been modified since we last read/wrote it.
    /// Returns Err(message) if stale, Ok(()) if fresh or unknown.
    ///
    /// The error message includes the concrete file path and the exact tool call
    /// the model should make next, so the LLM can act without extra reasoning.
    pub(super) fn check_staleness(&self, path: &Path) -> Result<(), String> {
        let current_ts = Self::file_mtime_ms(path);
        if current_ts == 0 {
            return Ok(()); // file doesn't exist yet — ok for write_file
        }
        let rel = path
            .strip_prefix(&self.project_root)
            .unwrap_or(path)
            .to_string_lossy();
        if let Ok(state) = self.file_state.lock() {
            if let Some(fs) = state.get(path) {
                if current_ts > fs.timestamp_ms {
                    return Err(format!(
                        "File has been modified since last read (by user or linter). \
                         Read it again before editing.\n\
                         → Action required: call read_file(\"{rel}\") first, then retry."
                    ));
                }
            } else {
                // Never read — require read first for existing files
                return Err(format!(
                    "File exists but has not been read yet. \
                     Read it first before writing/editing.\n\
                     → Action required: call read_file(\"{rel}\") first, then retry."
                ));
            }
        }
        Ok(())
    }

    /// Register a file as "read" from an external source (e.g. skill execution
    /// that loaded and returned the file content). This prevents the
    /// read-before-write guard from rejecting subsequent edits to the file.
    pub fn register_external_read(&self, path: &Path) {
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.project_root.join(path)
        };
        self.record_read(&abs, false, ReadDedupKey::Full);
    }

    /// Check if a file was read as a full view (not partial/outline).
    pub(super) fn was_fully_read(&self, path: &Path) -> bool {
        self.file_state
            .lock()
            .ok()
            .and_then(|s| s.get(path).map(|fs| !fs.is_partial))
            .unwrap_or(false)
    }

    /// Consecutive identical partial read (outline or same raw line range) with unchanged
    /// mtime — stub **before** disk read, like Claude Code `tengu_file_read_dedup` for
    /// the same offset/limit as the immediately previous read.
    pub(super) fn can_dedup_identical_partial_read(
        &self,
        path: &Path,
        requested: &ReadDedupKey,
    ) -> bool {
        if std::env::var("MO_DEDUP_DISABLED").is_ok_and(|v| v == "1" || v == "true") {
            return false;
        }
        if matches!(requested, ReadDedupKey::Full) {
            return false;
        }
        let current_ts = Self::file_mtime_ms(path);
        if current_ts == 0 {
            return false;
        }
        self.file_state
            .lock()
            .ok()
            .and_then(|s| {
                s.get(path).and_then(|fs| {
                    (fs.from_read
                        && fs.timestamp_ms == current_ts
                        && fs.last_dedup_key == *requested)
                        .then_some(())
                })
            })
            .is_some()
    }

    /// Check if we can dedup a read (previous op was a full read, unchanged mtime).
    /// Respects `MO_DEDUP_DISABLED=1` env var killswitch (inspired by Claude Code's
    /// `tengu_read_dedup_killswitch` feature flag).
    pub(super) fn can_dedup_read(&self, path: &Path) -> bool {
        if std::env::var("MO_DEDUP_DISABLED").is_ok_and(|v| v == "1" || v == "true") {
            return false;
        }
        let current_ts = Self::file_mtime_ms(path);
        if current_ts == 0 {
            return false;
        }
        self.file_state
            .lock()
            .ok()
            .and_then(|s| {
                s.get(path)
                    .map(|fs| fs.from_read && !fs.is_partial && fs.timestamp_ms == current_ts)
            })
            .unwrap_or(false)
    }

    /// How many times this file has been read in the current session.
    pub(super) fn file_read_count(&self, path: &Path) -> u32 {
        self.file_state
            .lock()
            .ok()
            .and_then(|s| s.get(path).map(|fs| fs.read_count))
            .unwrap_or(0)
    }

    /// How many times this file has been read with different ranges.
    pub(super) fn file_ranged_read_count(&self, path: &Path) -> u32 {
        self.file_state
            .lock()
            .ok()
            .and_then(|s| s.get(path).map(|fs| fs.ranged_read_count))
            .unwrap_or(0)
    }

    /// Check if a file was previously partially read (outline or line range) and
    /// hasn't been modified since. Used to auto-expand subsequent ranged reads
    /// to the full file, eliminating fragmented multi-range read patterns.
    pub(super) fn was_partially_read_unchanged(&self, path: &Path) -> bool {
        let current_ts = Self::file_mtime_ms(path);
        if current_ts == 0 {
            return false;
        }
        self.file_state
            .lock()
            .ok()
            .and_then(|s| {
                s.get(path)
                    .map(|fs| fs.from_read && fs.is_partial && fs.timestamp_ms == current_ts)
            })
            .unwrap_or(false)
    }

    /// Clear all file state (call after compaction to avoid stale dedup).
    #[allow(dead_code)] // Public API for compaction cleanup
    pub fn clear_file_state(&self) {
        if let Ok(mut state) = self.file_state.lock() {
            state.clear();
        }
    }

    /// Remove a single file from state tracking (call after delete).
    pub(super) fn remove_file_state(&self, path: &Path) {
        if let Ok(mut state) = self.file_state.lock() {
            state.remove(path);
        }
    }

    /// Return recently-read file paths sorted by recency (most recent first).
    /// Used for post-compact file restoration — re-inject the N most recently
    /// accessed files so the LLM retains working context after compaction.
    #[allow(dead_code)] // Public API for post-compact file restoration
    pub fn recently_read_files(&self, max: usize) -> Vec<PathBuf> {
        if let Ok(state) = self.file_state.lock() {
            let mut entries: Vec<_> = state.iter().filter(|(_, fs)| fs.from_read).collect();
            entries.sort_by(|a, b| b.1.timestamp_ms.cmp(&a.1.timestamp_ms));
            entries
                .into_iter()
                .take(max)
                .map(|(p, _)| p.clone())
                .collect()
        } else {
            Vec::new()
        }
    }
}
