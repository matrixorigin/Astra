//! File state tracking for staleness detection and read deduplication.
//!
//! Tracks mtime after each read/write/edit to prevent overwriting user edits
//! and skip re-reading unchanged files.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use super::{ToolExecutor, passive_cargo_check, passive_tsc_check};

/// Maximum number of entries in the file state cache. When exceeded, the
/// entry with the oldest timestamp is evicted.
const MAX_FILE_STATE_ENTRIES: usize = 200;

/// Maximum size of a single file's cached content (256 KB).
/// Larger files are tracked for dedup/staleness but content is not cached.
const MAX_CACHED_FILE_BYTES: usize = 256 * 1024;

/// Maximum total size of all cached file content (8 MB).
/// When exceeded, cached content is evicted from the oldest entries first,
/// keeping metadata intact for dedup/staleness tracking.
const MAX_TOTAL_CACHED_BYTES: usize = 8 * 1024 * 1024;

/// Shape of the last `read_file` call, for consecutive-request dedup (same
/// offset+limit + unchanged mtime → stub before I/O).
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
    /// Cached full file content. Stored on reads/writes when the full content
    /// is available and fits within `MAX_CACHED_FILE_BYTES`. Serves subsequent
    /// reads without disk I/O when mtime is unchanged.
    pub(super) cached_content: Option<String>,
    /// Merged line ranges already read (sorted, non-overlapping).
    /// Used to detect when a new ranged read is fully covered by prior reads.
    /// Reset on write or mtime change.
    pub(super) read_ranges: Vec<(u64, u64)>,
}

/// Merge a new `(start, end)` into a sorted, non-overlapping range list.
/// Adjacent ranges (e.g. `1..100` + `101..200`) are coalesced.
fn merge_range(ranges: &mut Vec<(u64, u64)>, start: u64, end: u64) {
    ranges.push((start, end));
    ranges.sort_unstable();
    let mut merged = Vec::with_capacity(ranges.len());
    for &(s, e) in ranges.iter() {
        if let Some(last) = merged.last_mut() {
            let (_, le): &mut (u64, u64) = last;
            if s <= le.saturating_add(1) {
                *le = (*le).max(e);
                continue;
            }
        }
        merged.push((s, e));
    }
    *ranges = merged;
}

/// Check if `(start, end)` is fully covered by the merged range list.
fn ranges_cover(ranges: &[(u64, u64)], start: u64, end: u64) -> bool {
    ranges.iter().any(|&(s, e)| s <= start && end <= e)
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
        self.record_read_impl(path, is_partial, last_dedup_key, None);
    }

    /// Record file state after a read, caching the full file content for
    /// subsequent reads without disk I/O. Content is only cached if it fits
    /// within the per-file size limit (`MAX_CACHED_FILE_BYTES`).
    pub(super) fn record_read_cached(
        &self,
        path: &Path,
        is_partial: bool,
        last_dedup_key: ReadDedupKey,
        content: String,
    ) {
        self.record_read_impl(path, is_partial, last_dedup_key, Some(content));
    }

    fn record_read_impl(
        &self,
        path: &Path,
        is_partial: bool,
        last_dedup_key: ReadDedupKey,
        content: Option<String>,
    ) {
        let ts = Self::file_mtime_ms(path);
        let cached_content = content.filter(|c| c.len() <= MAX_CACHED_FILE_BYTES);
        if let Ok(mut state) = self.file_state.lock() {
            let prev = state.get(path);
            let prev_count = prev.map(|fs| fs.read_count).unwrap_or(0);
            let prev_ranged = prev.map(|fs| fs.ranged_read_count).unwrap_or(0);
            // Carry forward read_ranges if mtime unchanged, else reset.
            let mut ranges = prev
                .filter(|fs| fs.timestamp_ms == ts)
                .map(|fs| fs.read_ranges.clone())
                .unwrap_or_default();
            // Merge the new range into the list.
            if let ReadDedupKey::Range {
                start_line,
                end_line,
            } = &last_dedup_key
            {
                let s = start_line.unwrap_or(1);
                let e = end_line.unwrap_or(u64::MAX);
                merge_range(&mut ranges, s, e);
            } else if matches!(last_dedup_key, ReadDedupKey::Full) {
                // Full read covers everything.
                ranges = vec![(1, u64::MAX)];
            }
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
                    cached_content,
                    read_ranges: ranges,
                },
            );
            enforce_limits(&mut state);
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
        let cached_content = content
            .filter(|c| c.len() <= MAX_CACHED_FILE_BYTES)
            .map(String::from);
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
                    cached_content,
                    read_ranges: vec![],
                },
            );
            enforce_limits(&mut state);
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
    /// mtime — stub **before** disk read for
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
    /// Respects `MO_DEDUP_DISABLED=1` env var killswitch.
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

    /// Check if a ranged read is fully covered by previously read ranges
    /// (file unchanged). Returns true when the requested `start..end` is a
    /// subset of the union of all prior reads — the content is already in
    /// the conversation context.
    pub(super) fn is_range_already_read(&self, path: &Path, start: u64, end: u64) -> bool {
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
                s.get(path).and_then(|fs| {
                    (fs.from_read
                        && fs.timestamp_ms == current_ts
                        && ranges_cover(&fs.read_ranges, start, end))
                    .then_some(())
                })
            })
            .is_some()
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

    /// Try to retrieve cached file content. Returns `Some(content)` if:
    /// - The file was previously read or written with content caching
    /// - The content was small enough to be cached
    /// - The file mtime hasn't changed since caching
    ///
    /// This avoids disk I/O for repeated reads of unchanged files, even when
    /// dedup stubs don't apply (e.g., outline → full read, write → read).
    pub(super) fn get_cached_content(&self, path: &Path) -> Option<String> {
        let current_ts = Self::file_mtime_ms(path);
        if current_ts == 0 {
            return None;
        }
        self.file_state.lock().ok().and_then(|s| {
            s.get(path).and_then(|fs| {
                if fs.timestamp_ms == current_ts {
                    fs.cached_content.clone()
                } else {
                    None
                }
            })
        })
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
            entries.sort_by_key(|b| std::cmp::Reverse(b.1.timestamp_ms));
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

/// Enforce file state cache limits: LRU eviction for entry count, then
/// content budget eviction (drop cached content from oldest entries first).
fn enforce_limits(state: &mut HashMap<PathBuf, FileState>) {
    // LRU eviction for entry count
    if state.len() > MAX_FILE_STATE_ENTRIES {
        if let Some(oldest_key) = state
            .iter()
            .min_by_key(|(_, fs)| fs.timestamp_ms)
            .map(|(k, _)| k.clone())
        {
            state.remove(&oldest_key);
        }
    }

    // Content budget eviction: drop cached content from oldest entries
    let total: usize = state
        .values()
        .filter_map(|fs| fs.cached_content.as_ref().map(String::len))
        .sum();
    if total <= MAX_TOTAL_CACHED_BYTES {
        return;
    }

    let mut entries_with_content: Vec<_> = state
        .iter()
        .filter(|(_, fs)| fs.cached_content.is_some())
        .map(|(k, fs)| (k.clone(), fs.timestamp_ms))
        .collect();
    entries_with_content.sort_by_key(|(_, ts)| *ts);

    let mut to_free = total - MAX_TOTAL_CACHED_BYTES;
    for (key, _) in entries_with_content {
        if to_free == 0 {
            break;
        }
        if let Some(fs) = state.get_mut(&key) {
            if let Some(content) = fs.cached_content.take() {
                to_free = to_free.saturating_sub(content.len());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_range_u64_max_no_overflow() {
        // Full read sets (1, u64::MAX). Merging a new range must not overflow.
        let mut ranges = vec![(1, u64::MAX)];
        merge_range(&mut ranges, 50, 100);
        assert_eq!(ranges, vec![(1, u64::MAX)]);
    }

    #[test]
    fn ranges_cover_after_full_read() {
        let ranges = vec![(1, u64::MAX)];
        assert!(ranges_cover(&ranges, 1, 500));
        assert!(ranges_cover(&ranges, 100, u64::MAX));
    }

    #[test]
    fn merge_range_adjacent_coalesces() {
        let mut ranges = vec![(1, 100)];
        merge_range(&mut ranges, 101, 200);
        assert_eq!(ranges, vec![(1, 200)]);
    }

    #[test]
    fn merge_range_gap_stays_separate() {
        let mut ranges = vec![(1, 100)];
        merge_range(&mut ranges, 103, 200);
        assert_eq!(ranges, vec![(1, 100), (103, 200)]);
    }
}
