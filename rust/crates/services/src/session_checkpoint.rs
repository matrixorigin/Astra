//! Session checkpoints — periodic snapshots for debugging and replay.
//!
//! A checkpoint captures a summary of the session state at a specific turn.
//! Checkpoints are stored as individual files in the owner-bound session
//! directory:
//! `~/.astra/sessions/v1/users/<owner>/sessions/<session_id>/checkpoints/<number>-<slug>.md`
//!
//! Checkpoints enable:
//! - Quick overview of session progress without reading JSONL
//! - Foundation for future session resumption/rewind
//! - Audit trail of key decisions

use std::path::{Path, PathBuf};

/// Default interval: create a checkpoint every N turns.
pub const CHECKPOINT_INTERVAL: u32 = 5;

/// A session checkpoint record.
#[derive(Debug, Clone)]
pub struct Checkpoint {
    /// Checkpoint number (1-based).
    pub number: u32,
    /// Turn number when checkpoint was created.
    pub turn: u32,
    /// Brief title/slug for the checkpoint.
    pub title: String,
    /// Summary of what happened since last checkpoint.
    pub summary: String,
    /// Tools used since last checkpoint.
    pub tools_used: Vec<String>,
    /// Cumulative token usage.
    pub total_tokens: u64,
    /// Whether any stalls were detected.
    pub had_stalls: bool,
    /// Number of errors since last checkpoint.
    pub error_count: u32,
    /// Serialized durable task contract state (JSON) — enables verification
    /// context to survive session interruption and cloud-based restore.
    pub contract_state_json: Option<String>,
}

impl Checkpoint {
    /// Format as markdown for storage.
    pub fn to_markdown(&self) -> String {
        let mut md = String::new();
        md.push_str(&format!(
            "# Checkpoint {} — Turn {}\n\n",
            self.number, self.turn
        ));
        md.push_str(&format!("**Title:** {}\n\n", self.title));
        md.push_str(&format!("## Summary\n\n{}\n\n", self.summary));
        md.push_str("## Stats\n\n");
        md.push_str(&format!("- Total tokens: {}\n", self.total_tokens));
        md.push_str(&format!(
            "- Tools used: {}\n",
            if self.tools_used.is_empty() {
                "none".to_string()
            } else {
                self.tools_used.join(", ")
            }
        ));
        if self.had_stalls {
            md.push_str("- ⚠ Stalls detected\n");
        }
        if self.error_count > 0 {
            md.push_str(&format!("- Errors: {}\n", self.error_count));
        }
        md
    }
}

/// Determine if a checkpoint should be created at this turn.
pub fn should_checkpoint(turn: u32, interval: u32) -> bool {
    turn > 0 && turn.is_multiple_of(interval)
}

/// Generate a URL-safe slug from a title.
fn slugify(title: &str) -> String {
    title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .chars()
        .take(50)
        .collect()
}

/// Write a checkpoint file to the session directory.
pub fn write_checkpoint(session_id: &str, checkpoint: &Checkpoint) -> std::io::Result<PathBuf> {
    let dir = super::session_workspace::workspace_dir_for(session_id).join("checkpoints");
    std::fs::create_dir_all(&dir)?;

    ensure_checkpoint_dir_within_session(session_id, &dir)?;

    let slug = slugify(&checkpoint.title);
    let path = checkpoint_file_path(&dir, checkpoint, &slug);
    std::fs::write(&path, checkpoint.to_markdown())?;

    // Update checkpoint index
    if let Err(error) = update_index(&dir, checkpoint) {
        let _ = std::fs::remove_file(&path);
        return Err(error);
    }

    Ok(path)
}

/// Remove a checkpoint file and its index entry after a failed commit.
///
/// This is best-effort cleanup for callers that wrote a checkpoint but could
/// not durably publish the workspace metadata that references it.
///
/// The operation intentionally updates `index.md` before deleting the file: a
/// crash after index update leaves an unreferenced file, while the inverse can
/// leave recovery reading a stale index entry. An advisory `flock` on
/// `index.lock` serializes concurrent checkpoint writes against the same
/// session.
pub fn remove_checkpoint(session_id: &str, checkpoint: &Checkpoint) -> std::io::Result<()> {
    let dir = super::session_workspace::workspace_dir_for(session_id).join("checkpoints");
    if !dir.exists() {
        return Ok(());
    }
    ensure_checkpoint_dir_within_session(session_id, &dir)?;
    let slug = slugify(&checkpoint.title);
    let path = checkpoint_file_path(&dir, checkpoint, &slug);

    remove_index_entry(&dir, checkpoint)?;

    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn ensure_checkpoint_dir_within_session(session_id: &str, dir: &Path) -> std::io::Result<()> {
    let canonical_dir = dir.canonicalize()?;
    let session_dir = super::session_workspace::workspace_dir_for(session_id).canonicalize()?;
    if !canonical_dir.starts_with(&session_dir) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "checkpoint directory escapes session boundary: {} is not under {}",
                canonical_dir.display(),
                session_dir.display()
            ),
        ));
    }
    Ok(())
}

fn checkpoint_file_path(dir: &Path, checkpoint: &Checkpoint, slug: &str) -> PathBuf {
    dir.join(format!("{:03}-{}.md", checkpoint.number, slug))
}

fn index_entry(checkpoint: &Checkpoint) -> String {
    format!(
        "  {:03} - Turn {:>2} - {}\n",
        checkpoint.number, checkpoint.turn, checkpoint.title
    )
}

/// Per-session turn commits are serialized by the CLI, but `save_checkpoint`
/// and `remove_checkpoint` are `pub` and may be called in concurrent contexts.
/// Use an advisory `flock` on `index.lock` to serialize reads and writes of
/// `index.md`. On Linux ≥ 2.6.12, `flock()` detects same-process conflicts,
/// protecting against accidental multi-threaded checkpoint access.
fn lock_checkpoint_index(dir: &Path) -> std::io::Result<std::fs::File> {
    use fs2::FileExt;
    let lock_path = dir.join("index.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    file.lock_exclusive()?;
    Ok(file)
}

fn remove_index_entry(dir: &Path, checkpoint: &Checkpoint) -> std::io::Result<()> {
    let index_path = dir.join("index.md");
    let _lock = lock_checkpoint_index(dir)?;
    match std::fs::read_to_string(&index_path) {
        Ok(content) => {
            let entry = index_entry(checkpoint);
            let updated = content.replace(&entry, "");
            if updated != content {
                let tmp_path = index_path.with_extension("md.tmp");
                std::fs::write(&tmp_path, updated)?;
                std::fs::rename(&tmp_path, &index_path).inspect_err(|_| {
                    let _ = std::fs::remove_file(&tmp_path);
                })?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Update the checkpoint index file.
fn update_index(dir: &Path, checkpoint: &Checkpoint) -> std::io::Result<()> {
    let index_path = dir.join("index.md");
    let entry = index_entry(checkpoint);

    let _lock = lock_checkpoint_index(dir)?;

    let mut content = match std::fs::read_to_string(&index_path) {
        Ok(c) => c,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            "# Checkpoint Index\n\n".to_string()
        }
        Err(error) => return Err(error),
    };
    content.push_str(&entry);
    // Atomic write: tmp file + rename to prevent corruption on crash.
    let tmp_path = index_path.with_extension("md.tmp");
    std::fs::write(&tmp_path, &content)?;
    std::fs::rename(&tmp_path, &index_path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp_path);
    })?;
    Ok(())
}

/// Read checkpoint index to get list of checkpoint titles.
pub fn read_checkpoint_index(session_id: &str) -> std::io::Result<Vec<String>> {
    let path = super::session_workspace::workspace_dir_for(session_id)
        .join("checkpoints")
        .join("index.md");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&path)?;
    let entries: Vec<String> = content
        .lines()
        .filter(|line| line.starts_with("  "))
        .map(|line| line.trim().to_string())
        .collect();
    Ok(entries)
}

/// Read checkpoint turn numbers from the checkpoint index.
pub fn read_checkpoint_turns(session_id: &str) -> std::io::Result<Vec<u32>> {
    read_checkpoint_index(session_id)?
        .into_iter()
        .map(|entry| {
            let turn_text = entry
                .split(" - Turn ")
                .nth(1)
                .and_then(|rest| rest.split(" - ").next())
                .map(str::trim)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("malformed checkpoint index entry: {entry}"),
                    )
                })?;
            turn_text.parse::<u32>().map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid checkpoint turn '{turn_text}': {error}"),
                )
            })
        })
        .collect()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_checkpoint_at_interval() {
        assert!(!should_checkpoint(0, 5));
        assert!(!should_checkpoint(1, 5));
        assert!(!should_checkpoint(4, 5));
        assert!(should_checkpoint(5, 5));
        assert!(should_checkpoint(10, 5));
        assert!(!should_checkpoint(7, 5));
    }

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Hello World"), "hello-world");
        assert_eq!(slugify("Fix bug #123"), "fix-bug-123");
        assert_eq!(slugify("  spaces  "), "spaces");
        assert_eq!(slugify("CJK 测试"), "cjk-测试");
    }

    #[test]
    fn slugify_truncates_long() {
        let long = "a".repeat(100);
        assert!(slugify(&long).len() <= 50);
    }

    #[test]
    fn checkpoint_to_markdown_format() {
        let cp = Checkpoint {
            number: 1,
            turn: 5,
            title: "Initial exploration".to_string(),
            summary: "Explored the codebase and identified key files.".to_string(),
            tools_used: vec!["bash".to_string(), "read_file".to_string()],
            total_tokens: 5000,
            had_stalls: false,
            error_count: 0,
            contract_state_json: None,
        };
        let md = cp.to_markdown();
        assert!(md.contains("# Checkpoint 1 — Turn 5"));
        assert!(md.contains("Initial exploration"));
        assert!(md.contains("Total tokens: 5000"));
        assert!(md.contains("bash, read_file"));
        assert!(!md.contains("Stalls"));
    }

    #[test]
    fn checkpoint_to_markdown_with_stalls() {
        let cp = Checkpoint {
            number: 2,
            turn: 10,
            title: "Debugging issues".to_string(),
            summary: "Hit a stall while trying bash commands.".to_string(),
            tools_used: vec!["bash".to_string()],
            total_tokens: 10000,
            had_stalls: true,
            error_count: 3,
            contract_state_json: None,
        };
        let md = cp.to_markdown();
        assert!(md.contains("⚠ Stalls detected"));
        assert!(md.contains("Errors: 3"));
    }

    #[test]
    fn write_read_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = "test-cp-1";
        let dir = tmp
            .path()
            .join(".astra")
            .join("sessions")
            .join(session_id)
            .join("checkpoints");
        std::fs::create_dir_all(&dir).unwrap();

        let cp = Checkpoint {
            number: 1,
            turn: 5,
            title: "First checkpoint".to_string(),
            summary: "Did some stuff.".to_string(),
            tools_used: vec!["bash".to_string()],
            total_tokens: 1000,
            had_stalls: false,
            error_count: 0,
            contract_state_json: None,
        };

        // Write checkpoint directly to temp dir
        let slug = slugify(&cp.title);
        let filename = format!("{:03}-{}.md", cp.number, slug);
        let path = dir.join(&filename);
        std::fs::write(&path, cp.to_markdown()).unwrap();

        // Read back
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("# Checkpoint 1 — Turn 5"));
        assert!(content.contains("First checkpoint"));
    }

    #[test]
    fn remove_checkpoint_removes_file_and_index_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        let _guard = crate::session_journal::JournalDirGuard::new(&sessions);
        let session_id = format!("test-cp-remove-{}", uuid::Uuid::new_v4());
        let cp = Checkpoint {
            number: 1,
            turn: 5,
            title: "First checkpoint".to_string(),
            summary: "Did some stuff.".to_string(),
            tools_used: vec!["bash".to_string()],
            total_tokens: 1000,
            had_stalls: false,
            error_count: 0,
            contract_state_json: None,
        };

        let path = write_checkpoint(&session_id, &cp).unwrap();
        assert!(path.exists());
        assert_eq!(read_checkpoint_index(&session_id).unwrap().len(), 1);

        remove_checkpoint(&session_id, &cp).unwrap();

        assert!(!path.exists());
        assert!(
            read_checkpoint_index(&session_id).unwrap().is_empty(),
            "checkpoint index must not reference a removed checkpoint"
        );
    }

    #[test]
    fn write_checkpoint_removes_file_when_index_update_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        let _guard = crate::session_journal::JournalDirGuard::new(&sessions);
        let session_id = format!("test-cp-index-fail-{}", uuid::Uuid::new_v4());
        let dir =
            super::super::session_workspace::workspace_dir_for(&session_id).join("checkpoints");
        std::fs::create_dir_all(dir.join("index.md.tmp")).unwrap();
        let cp = Checkpoint {
            number: 1,
            turn: 5,
            title: "First checkpoint".to_string(),
            summary: "Did some stuff.".to_string(),
            tools_used: vec!["bash".to_string()],
            total_tokens: 1000,
            had_stalls: false,
            error_count: 0,
            contract_state_json: None,
        };
        let path = dir.join("001-first-checkpoint.md");

        let error = write_checkpoint(&session_id, &cp).expect_err("index update should fail");

        assert!(
            !path.exists(),
            "checkpoint file must be removed when index update fails: {error}"
        );
    }

    #[test]
    fn remove_checkpoint_removes_index_entry_when_file_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        let _guard = crate::session_journal::JournalDirGuard::new(&sessions);
        let session_id = format!("test-cp-remove-missing-file-{}", uuid::Uuid::new_v4());
        let dir =
            super::super::session_workspace::workspace_dir_for(&session_id).join("checkpoints");
        std::fs::create_dir_all(&dir).unwrap();
        let cp = Checkpoint {
            number: 1,
            turn: 5,
            title: "First checkpoint".to_string(),
            summary: "Did some stuff.".to_string(),
            tools_used: vec!["bash".to_string()],
            total_tokens: 1000,
            had_stalls: false,
            error_count: 0,
            contract_state_json: None,
        };
        std::fs::write(
            dir.join("index.md"),
            format!("# Checkpoint Index\n\n{}", index_entry(&cp)),
        )
        .unwrap();

        remove_checkpoint(&session_id, &cp).unwrap();

        assert!(
            read_checkpoint_index(&session_id).unwrap().is_empty(),
            "remove_checkpoint must remove stale index entries even when file is missing"
        );
    }

    #[test]
    fn remove_checkpoint_noops_when_checkpoint_dir_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        let _guard = crate::session_journal::JournalDirGuard::new(&sessions);
        let session_id = format!("test-cp-remove-missing-dir-{}", uuid::Uuid::new_v4());
        let cp = Checkpoint {
            number: 1,
            turn: 5,
            title: "Missing checkpoint".to_string(),
            summary: "No file was ever written.".to_string(),
            tools_used: Vec::new(),
            total_tokens: 0,
            had_stalls: false,
            error_count: 0,
            contract_state_json: None,
        };

        remove_checkpoint(&session_id, &cp).unwrap();

        assert!(
            read_checkpoint_index(&session_id).unwrap().is_empty(),
            "best-effort removal should stay a no-op when checkpoint storage was never created"
        );
    }

    #[test]
    #[cfg(unix)]
    fn remove_checkpoint_rejects_checkpoint_dir_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        let _guard = crate::session_journal::JournalDirGuard::new(&sessions);
        let session_id = format!("test-cp-remove-escape-{}", uuid::Uuid::new_v4());
        let session_dir = super::super::session_workspace::workspace_dir_for(&session_id);
        std::fs::create_dir_all(&session_dir).unwrap();
        let outside = tmp.path().join("outside-checkpoints");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, session_dir.join("checkpoints")).unwrap();

        let cp = Checkpoint {
            number: 1,
            turn: 5,
            title: "Escaped checkpoint".to_string(),
            summary: "Should not be removed.".to_string(),
            tools_used: Vec::new(),
            total_tokens: 0,
            had_stalls: false,
            error_count: 0,
            contract_state_json: None,
        };
        let escaped_path = outside.join("001-escaped-checkpoint.md");
        std::fs::write(&escaped_path, cp.to_markdown()).unwrap();

        let error = remove_checkpoint(&session_id, &cp)
            .expect_err("checkpoint cleanup must reject session-boundary escapes");

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            escaped_path.exists(),
            "cleanup must not delete checkpoint files outside the session boundary"
        );
    }

    #[test]
    fn remove_checkpoint_keeps_file_when_index_update_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        let _guard = crate::session_journal::JournalDirGuard::new(&sessions);
        let session_id = format!("test-cp-remove-index-fail-{}", uuid::Uuid::new_v4());
        let dir =
            super::super::session_workspace::workspace_dir_for(&session_id).join("checkpoints");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir(dir.join("index.md.tmp")).unwrap();
        let cp = Checkpoint {
            number: 1,
            turn: 5,
            title: "First checkpoint".to_string(),
            summary: "Did some stuff.".to_string(),
            tools_used: vec!["bash".to_string()],
            total_tokens: 1000,
            had_stalls: false,
            error_count: 0,
            contract_state_json: None,
        };
        let path = dir.join("001-first-checkpoint.md");
        std::fs::write(&path, cp.to_markdown()).unwrap();
        std::fs::write(
            dir.join("index.md"),
            format!("# Checkpoint Index\n\n{}", index_entry(&cp)),
        )
        .unwrap();

        let error = remove_checkpoint(&session_id, &cp).expect_err("index update should fail");

        assert!(
            path.exists(),
            "checkpoint file should remain when index update fails first: {error}"
        );
    }

    #[test]
    fn checkpoint_no_tools_shows_none() {
        let cp = Checkpoint {
            number: 1,
            turn: 5,
            title: "No tools".to_string(),
            summary: "Text-only response.".to_string(),
            tools_used: vec![],
            total_tokens: 500,
            had_stalls: false,
            error_count: 0,
            contract_state_json: None,
        };
        let md = cp.to_markdown();
        assert!(md.contains("Tools used: none"));
    }

    #[test]
    fn read_checkpoint_turns_parses_index_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = "test-cp-turns";
        let sessions_dir = tmp.path().join(".astra").join("sessions");
        let _guard = crate::session_journal::JournalDirGuard::new(&sessions_dir);
        let dir = crate::session_workspace::workspace_dir_for(session_id).join("checkpoints");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("index.md"),
            "# Checkpoint Index\n\n  001 - Turn  5 - Initial exploration\n  002 - Turn 12 - Follow-up\n",
        )
        .unwrap();

        let turns = read_checkpoint_turns(session_id).unwrap();
        assert_eq!(turns, vec![5, 12]);
    }

    #[test]
    fn read_checkpoint_turns_rejects_malformed_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = "test-cp-turns-bad";
        let sessions_dir = tmp.path().join(".astra").join("sessions");
        let _guard = crate::session_journal::JournalDirGuard::new(&sessions_dir);
        let dir = crate::session_workspace::workspace_dir_for(session_id).join("checkpoints");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.md"), "# Checkpoint Index\n\n  nonsense\n").unwrap();

        let error = read_checkpoint_turns(session_id).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
