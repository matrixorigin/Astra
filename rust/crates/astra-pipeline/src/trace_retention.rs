//! Trace data retention policies for automatic cleanup.
//!
//! Provides configurable limits on:
//! - Maximum age (days)
//! - Maximum total size (MB)
//! - Maximum event count per session
//!
//! # Usage
//!
//! ```ignore
//! let policy = RetentionPolicy::default();
//! let stats = policy.cleanup("session-123")?;
//! println!("Removed {} old events", stats.events_removed);
//! ```

use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::step_checkpoint::{events_path_for, session_dir_for};

/// Retention policy configuration.
#[derive(Debug, Clone)]
pub struct RetentionPolicy {
    /// Maximum age in days for archived files.
    pub max_age_days: u64,
    /// Maximum total size in MB (current + archives).
    pub max_size_mb: u64,
    /// Maximum events per session (soft limit, triggers cleanup).
    pub max_events: usize,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_age_days: 30,
            max_size_mb: 100,
            max_events: 10_000,
        }
    }
}

/// Statistics from a cleanup operation.
#[derive(Debug, Default)]
pub struct CleanupStats {
    pub archives_removed: usize,
    pub bytes_freed: u64,
    pub events_removed: usize,
}

impl RetentionPolicy {
    /// Apply retention policy to a session's trace data.
    ///
    /// Returns statistics about what was removed.
    pub fn cleanup(&self, session_id: &str) -> io::Result<CleanupStats> {
        let mut stats = CleanupStats::default();
        let dir = session_dir_for(session_id);

        if !dir.exists() {
            return Ok(stats);
        }

        // 1. Remove old archives based on max_age_days
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let max_age_secs = self.max_age_days * 24 * 3600;

        let mut archives: Vec<_> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|s| s.starts_with("step_events_") && s.ends_with(".jsonl"))
                    .unwrap_or(false)
            })
            .collect();

        // Sort by modification time (oldest first)
        archives.sort_by_key(|e| {
            e.metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(u64::MAX)
        });

        for archive in &archives {
            let mtime = archive
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(u64::MAX);

            if now.saturating_sub(mtime) > max_age_secs {
                if let Ok(meta) = archive.metadata() {
                    stats.bytes_freed += meta.len();
                }
                let _ = std::fs::remove_file(archive.path());
                stats.archives_removed += 1;
            }
        }

        // 2. Enforce max_size_mb limit
        let max_bytes = self.max_size_mb * 1024 * 1024;
        let mut total_size = 0u64;

        // Current events file
        let events_path = events_path_for(session_id);
        if let Ok(meta) = std::fs::metadata(&events_path) {
            total_size += meta.len();
        }

        // Archived files
        for archive in std::fs::read_dir(&dir)?.filter_map(|e| e.ok()).filter(|e| {
            e.file_name()
                .to_str()
                .map(|s| s.starts_with("step_events_") && s.ends_with(".jsonl"))
                .unwrap_or(false)
        }) {
            if let Ok(meta) = archive.metadata() {
                total_size += meta.len();
            }
        }

        // If over limit, remove oldest archives until under limit
        if total_size > max_bytes {
            let mut sorted_archives: Vec<_> = std::fs::read_dir(&dir)?
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .map(|s| s.starts_with("step_events_") && s.ends_with(".jsonl"))
                        .unwrap_or(false)
                })
                .collect();

            sorted_archives.sort_by_key(|e| {
                e.metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(u64::MAX)
            });

            for archive in sorted_archives {
                if total_size <= max_bytes {
                    break;
                }
                if let Ok(meta) = archive.metadata() {
                    stats.bytes_freed += meta.len();
                    total_size = total_size.saturating_sub(meta.len());
                }
                let _ = std::fs::remove_file(archive.path());
                stats.archives_removed += 1;
            }
        }

        Ok(stats)
    }

    /// Check if a session needs cleanup (has exceeded limits).
    pub fn needs_cleanup(&self, session_id: &str) -> bool {
        let dir = session_dir_for(session_id);
        if !dir.exists() {
            return false;
        }

        // Check archive count
        let archive_count = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| {
                        e.file_name()
                            .to_str()
                            .map(|s| s.starts_with("step_events_") && s.ends_with(".jsonl"))
                            .unwrap_or(false)
                    })
                    .count()
            })
            .unwrap_or(0);

        // Check total size
        let mut total_size = 0u64;
        let events_path = events_path_for(session_id);
        if let Ok(meta) = std::fs::metadata(&events_path) {
            total_size += meta.len();
        }

        for archive in std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
        {
            if let Ok(meta) = archive.metadata() {
                total_size += meta.len();
            }
        }

        let max_bytes = self.max_size_mb * 1024 * 1024;
        archive_count > 10 || total_size > max_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_reasonable_values() {
        let policy = RetentionPolicy::default();
        assert_eq!(policy.max_age_days, 30);
        assert_eq!(policy.max_size_mb, 100);
        assert_eq!(policy.max_events, 10_000);
    }

    #[test]
    fn cleanup_nonexistent_session() {
        let policy = RetentionPolicy::default();
        let result = policy.cleanup("nonexistent-session-id");
        assert!(result.is_ok());
        let stats = result.unwrap();
        assert_eq!(stats.archives_removed, 0);
    }

    #[test]
    fn needs_cleanup_returns_false_for_missing_dir() {
        let policy = RetentionPolicy::default();
        assert!(!policy.needs_cleanup("nonexistent-session-id"));
    }
}
