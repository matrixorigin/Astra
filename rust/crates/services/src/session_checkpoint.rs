//! Session checkpoints — periodic snapshots for debugging and replay.
//!
//! A checkpoint captures a summary of the session state at a specific turn.
//! Checkpoints are stored as individual files in the session directory:
//! `~/.mo-agent/sessions/<session_id>/checkpoints/<number>-<slug>.md`
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

    let slug = slugify(&checkpoint.title);
    let filename = format!("{:03}-{}.md", checkpoint.number, slug);
    let path = dir.join(&filename);
    std::fs::write(&path, checkpoint.to_markdown())?;

    // Update checkpoint index
    update_index(&dir, checkpoint)?;

    Ok(path)
}

/// Update the checkpoint index file.
fn update_index(dir: &Path, checkpoint: &Checkpoint) -> std::io::Result<()> {
    let index_path = dir.join("index.md");
    let entry = format!(
        "  {:03} - Turn {:>2} - {}\n",
        checkpoint.number, checkpoint.turn, checkpoint.title
    );

    let mut content = if index_path.exists() {
        std::fs::read_to_string(&index_path)?
    } else {
        "# Checkpoint Index\n\n".to_string()
    };
    content.push_str(&entry);
    std::fs::write(&index_path, content)?;
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
            .join(".mo-agent")
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
        };
        let md = cp.to_markdown();
        assert!(md.contains("Tools used: none"));
    }
}
