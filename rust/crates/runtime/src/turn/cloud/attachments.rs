//! Post-compaction attachment recovery.
//!
//! After compacting message history, important context (recently-read files,
//! active skills, session plan) is re-injected into the conversation so the
//! LLM doesn't lose access to it.
//!
//! Inspired by claudecode's post-compact attachment restoration, but adapted
//! for astra's architecture: attachment selection uses ToolSelector's
//! learned_context for file hotness rather than a separate read-cache.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Token budgets (chars ≈ tokens × 4)
// ---------------------------------------------------------------------------

/// Maximum number of files to restore post-compaction.
pub const MAX_FILES_TO_RESTORE: usize = 5;
/// Total char budget for all file attachments.
pub const FILE_ATTACHMENT_CHAR_BUDGET: usize = 200_000; // ~50K tokens
/// Max chars per individual file.
pub const MAX_CHARS_PER_FILE: usize = 20_000; // ~5K tokens
/// Total char budget for skill attachments.
pub const SKILL_ATTACHMENT_CHAR_BUDGET: usize = 100_000; // ~25K tokens
/// Max chars per individual skill.
pub const MAX_CHARS_PER_SKILL: usize = 20_000; // ~5K tokens

// ---------------------------------------------------------------------------
// Attachment Types
// ---------------------------------------------------------------------------

/// A recently-accessed file to re-inject after compaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileAttachment {
    /// Absolute path of the file.
    pub path: String,
    /// File content (possibly truncated to [`MAX_CHARS_PER_FILE`]).
    pub content: String,
    /// True if the content was truncated.
    pub truncated: bool,
    /// Recency score (higher = more recently accessed). Used for sorting.
    pub recency: f64,
}

impl FileAttachment {
    /// Create a file attachment, truncating content if it exceeds the budget.
    pub fn new(path: impl Into<String>, content: impl Into<String>, recency: f64) -> Self {
        let path = path.into();
        let raw = content.into();
        let char_limit = MAX_CHARS_PER_FILE.saturating_sub(TRUNCATION_MARKER.len());
        if raw.chars().count() > MAX_CHARS_PER_FILE {
            let truncated: String = raw.chars().take(char_limit).collect();
            Self {
                path,
                content: truncated + TRUNCATION_MARKER,
                truncated: true,
                recency,
            }
        } else {
            Self {
                path,
                content: raw,
                truncated: false,
                recency,
            }
        }
    }

    /// Format as a user message for injection into the LLM conversation.
    pub fn to_message(&self) -> Value {
        serde_json::json!({
            "role": "user",
            "content": format!(
                "[Post-compaction context: file {}]\n```\n{}\n```",
                self.path, self.content
            ),
            "attachment_metadata": {
                "kind": "file",
                "path": self.path,
                "truncated": self.truncated,
            }
        })
    }
}

/// A skill (reusable agent capability) to re-inject after compaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillAttachment {
    /// Skill name / identifier.
    pub name: String,
    /// Skill content (possibly truncated).
    pub content: String,
    /// True if the content was truncated.
    pub truncated: bool,
}

impl SkillAttachment {
    /// Create a skill attachment, truncating if needed.
    pub fn new(name: impl Into<String>, content: impl Into<String>) -> Self {
        let name = name.into();
        let raw = content.into();
        let char_limit = MAX_CHARS_PER_SKILL.saturating_sub(SKILL_TRUNCATION_MARKER.len());
        if raw.chars().count() > MAX_CHARS_PER_SKILL {
            let truncated: String = raw.chars().take(char_limit).collect();
            Self {
                name,
                content: truncated + SKILL_TRUNCATION_MARKER,
                truncated: true,
            }
        } else {
            Self {
                name,
                content: raw,
                truncated: false,
            }
        }
    }

    /// Format as a user message for injection.
    pub fn to_message(&self) -> Value {
        serde_json::json!({
            "role": "user",
            "content": format!(
                "[Post-compaction context: skill '{}']\n{}", self.name, self.content
            ),
            "attachment_metadata": {
                "kind": "skill",
                "name": self.name,
                "truncated": self.truncated,
            }
        })
    }
}

/// The session plan to re-inject after compaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanAttachment {
    /// Plan content.
    pub content: String,
}

impl PlanAttachment {
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
        }
    }

    /// Format as a user message for injection.
    pub fn to_message(&self) -> Value {
        serde_json::json!({
            "role": "user",
            "content": format!("[Post-compaction context: current plan]\n{}", self.content),
            "attachment_metadata": { "kind": "plan" }
        })
    }
}

/// Truncation marker appended to truncated file content.
pub const TRUNCATION_MARKER: &str =
    "\n...[file content truncated for compaction; re-read the file if you need the full text]";

/// Truncation marker for skill content.
pub const SKILL_TRUNCATION_MARKER: &str = "\n...[skill content truncated for compaction; use Read on the skill path if you need the full text]";

// ---------------------------------------------------------------------------
// PostCompactAttachments — collected set of attachments for a single compaction
// ---------------------------------------------------------------------------

/// The set of attachments to re-inject after a compaction event.
#[derive(Debug, Default, Clone)]
pub struct PostCompactAttachments {
    /// Recently-accessed files, sorted by recency (most recent first).
    pub files: Vec<FileAttachment>,
    /// Active skills.
    pub skills: Vec<SkillAttachment>,
    /// Session plan, if one exists.
    pub plan: Option<PlanAttachment>,
}

impl PostCompactAttachments {
    /// Returns true if there are no attachments to inject.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.skills.is_empty() && self.plan.is_none()
    }

    /// Convert all attachments to injectable messages, ordered:
    /// 1. Plan (if any)
    /// 2. Skills (if any)
    /// 3. Files (most recent last, so they appear closest to the next turn)
    pub fn to_messages(&self) -> Vec<Value> {
        let mut msgs = Vec::new();
        if let Some(plan) = &self.plan {
            msgs.push(plan.to_message());
        }
        for skill in &self.skills {
            msgs.push(skill.to_message());
        }
        for file in self.files.iter().rev() {
            msgs.push(file.to_message());
        }
        msgs
    }

    /// Total char count across all attachments (for budget tracking).
    pub fn total_chars(&self) -> usize {
        let file_chars: usize = self.files.iter().map(|f| f.content.len()).sum();
        let skill_chars: usize = self.skills.iter().map(|s| s.content.len()).sum();
        let plan_chars = self.plan.as_ref().map(|p| p.content.len()).unwrap_or(0);
        file_chars + skill_chars + plan_chars
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Builder for [`PostCompactAttachments`] that enforces token budgets.
#[derive(Debug, Default)]
pub struct AttachmentBuilder {
    files: Vec<FileAttachment>,
    skills: Vec<SkillAttachment>,
    plan: Option<PlanAttachment>,
    file_chars_used: usize,
    skill_chars_used: usize,
}

impl AttachmentBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a file attachment if within budget limits.
    /// Files must be added in recency order (most recent first) so budget
    /// eviction drops the least-recently-used files.
    pub fn add_file(
        &mut self,
        path: impl Into<String>,
        content: impl Into<String>,
        recency: f64,
    ) -> bool {
        if self.files.len() >= MAX_FILES_TO_RESTORE {
            return false;
        }
        if self.file_chars_used >= FILE_ATTACHMENT_CHAR_BUDGET {
            return false;
        }
        let attachment = FileAttachment::new(path, content, recency);
        let cost = attachment.content.len();
        if self.file_chars_used + cost > FILE_ATTACHMENT_CHAR_BUDGET {
            return false;
        }
        self.file_chars_used += cost;
        self.files.push(attachment);
        true
    }

    /// Add a skill attachment if within budget limits.
    pub fn add_skill(&mut self, name: impl Into<String>, content: impl Into<String>) -> bool {
        if self.skill_chars_used >= SKILL_ATTACHMENT_CHAR_BUDGET {
            return false;
        }
        let attachment = SkillAttachment::new(name, content);
        let cost = attachment.content.len();
        if self.skill_chars_used + cost > SKILL_ATTACHMENT_CHAR_BUDGET {
            return false;
        }
        self.skill_chars_used += cost;
        self.skills.push(attachment);
        true
    }

    /// Set the plan attachment (replaces any previous value).
    pub fn set_plan(&mut self, content: impl Into<String>) {
        self.plan = Some(PlanAttachment::new(content));
    }

    /// Finalize and return the collected attachments.
    pub fn build(self) -> PostCompactAttachments {
        // Sort files most-recent first
        let mut files = self.files;
        files.sort_by(|a, b| {
            b.recency
                .partial_cmp(&a.recency)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        PostCompactAttachments {
            files,
            skills: self.skills,
            plan: self.plan,
        }
    }
}

// ---------------------------------------------------------------------------
// Standalone file restoration helper
// ---------------------------------------------------------------------------

/// Read recently-accessed files from disk and produce user messages for
/// post-compaction injection.
///
/// `recent_reads` is `(path, turn_number)` — sorted by turn number ascending
/// (oldest first). We take the most recent [`MAX_FILES_TO_RESTORE`] entries,
/// read each from disk, and use [`AttachmentBuilder`] to enforce per-file
/// and total char budgets.
///
/// If `cwd` is provided, relative paths are resolved against it.
pub fn restore_recent_files(
    recent_reads: &[(String, u32)],
    cwd: Option<&str>,
) -> Vec<Value> {
    if recent_reads.is_empty() {
        return Vec::new();
    }

    // Deduplicate by path, keeping the highest turn number (most recent access).
    let mut by_path: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    for (path, turn) in recent_reads {
        let entry = by_path.entry(path.as_str()).or_insert(0);
        if *turn > *entry {
            *entry = *turn;
        }
    }

    // Sort by turn number descending (most recent first), take top N.
    let mut candidates: Vec<(&str, u32)> = by_path.into_iter().collect();
    candidates.sort_by(|a, b| b.1.cmp(&a.1));
    candidates.truncate(MAX_FILES_TO_RESTORE);

    let mut builder = AttachmentBuilder::new();
    for (path, turn) in &candidates {
        let abs_path = if std::path::Path::new(path).is_absolute() {
            std::path::PathBuf::from(path)
        } else if let Some(base) = cwd {
            std::path::PathBuf::from(base).join(path)
        } else {
            std::path::PathBuf::from(path)
        };

        match std::fs::read_to_string(&abs_path) {
            Ok(content) => {
                // Use turn number as recency score (higher = more recent).
                builder.add_file(path.to_string(), content, *turn as f64);
            }
            Err(_) => {
                // File deleted or inaccessible — skip silently.
            }
        }
    }

    let attachments = builder.build();
    attachments.to_messages()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_attachment_no_truncation() {
        let content = "fn main() {}".to_string();
        let a = FileAttachment::new("src/main.rs", content.clone(), 1.0);
        assert_eq!(a.content, content);
        assert!(!a.truncated);
        assert_eq!(a.path, "src/main.rs");
    }

    #[test]
    fn file_attachment_truncates_at_limit() {
        let long = "x".repeat(MAX_CHARS_PER_FILE + 100);
        let a = FileAttachment::new("big.rs", long, 1.0);
        assert!(a.truncated);
        assert!(a.content.contains(TRUNCATION_MARKER));
        assert!(a.content.len() <= MAX_CHARS_PER_FILE + TRUNCATION_MARKER.len());
    }

    #[test]
    fn skill_attachment_truncates_at_limit() {
        let long = "y".repeat(MAX_CHARS_PER_SKILL + 100);
        let a = SkillAttachment::new("my-skill", long);
        assert!(a.truncated);
        assert!(a.content.contains(SKILL_TRUNCATION_MARKER));
    }

    #[test]
    fn builder_enforces_file_count_limit() {
        let mut b = AttachmentBuilder::new();
        for i in 0..MAX_FILES_TO_RESTORE + 2 {
            b.add_file(format!("file{i}.rs"), "content", i as f64);
        }
        assert_eq!(b.files.len(), MAX_FILES_TO_RESTORE);
    }

    #[test]
    fn builder_enforces_file_char_budget() {
        let mut b = AttachmentBuilder::new();
        // Each file ~MAX_CHARS_PER_FILE; after enough files we hit budget
        let big_content = "a".repeat(MAX_CHARS_PER_FILE - 10);
        let capacity = FILE_ATTACHMENT_CHAR_BUDGET / (MAX_CHARS_PER_FILE - 10);
        let mut accepted = 0;
        for i in 0..capacity + 5 {
            if b.add_file(format!("f{i}.rs"), big_content.clone(), i as f64) {
                accepted += 1;
            }
        }
        assert!(accepted <= capacity + 1); // at most one extra due to rounding
    }

    #[test]
    fn builder_sorts_files_by_recency() {
        let mut b = AttachmentBuilder::new();
        b.add_file("old.rs", "old content", 0.1);
        b.add_file("new.rs", "new content", 0.9);
        b.add_file("mid.rs", "mid content", 0.5);
        let result = b.build();
        // Most recent first
        assert_eq!(result.files[0].path, "new.rs");
        assert_eq!(result.files[1].path, "mid.rs");
        assert_eq!(result.files[2].path, "old.rs");
    }

    #[test]
    fn post_compact_attachments_to_messages_order() {
        let mut b = AttachmentBuilder::new();
        b.set_plan("do things");
        b.add_skill("my-skill", "skill content");
        b.add_file("a.rs", "code", 1.0);
        let result = b.build();
        let msgs = result.to_messages();
        // Order: plan, skill, file (reversed — most recent last)
        assert_eq!(msgs.len(), 3);
        assert!(
            msgs[0]["content"]
                .as_str()
                .unwrap()
                .contains("current plan")
        );
        assert!(msgs[1]["content"].as_str().unwrap().contains("skill"));
        assert!(msgs[2]["content"].as_str().unwrap().contains("a.rs"));
    }

    #[test]
    fn to_message_includes_metadata() {
        let a = FileAttachment::new("foo.rs", "content", 1.0);
        let msg = a.to_message();
        assert!(msg.get("attachment_metadata").is_some());
        assert_eq!(msg["attachment_metadata"]["kind"].as_str().unwrap(), "file");
        assert_eq!(
            msg["attachment_metadata"]["path"].as_str().unwrap(),
            "foo.rs"
        );
    }

    #[test]
    fn restore_recent_files_empty_input() {
        let msgs = restore_recent_files(&[], None);
        assert!(msgs.is_empty());
    }

    #[test]
    fn restore_recent_files_reads_real_files() {
        let dir = std::env::temp_dir().join("restore_test_reads");
        let _ = std::fs::create_dir_all(&dir);
        let f1 = dir.join("hello.txt");
        std::fs::write(&f1, "hello world").unwrap();
        let f2 = dir.join("goodbye.txt");
        std::fs::write(&f2, "goodbye world").unwrap();

        let reads = vec![
            (f1.to_string_lossy().to_string(), 1),
            (f2.to_string_lossy().to_string(), 5),
        ];
        let msgs = restore_recent_files(&reads, None);
        assert_eq!(msgs.len(), 2);
        // Most recent (turn 5 = goodbye) should appear last in messages
        // (AttachmentBuilder sorts most-recent first, to_messages reverses)
        let last_content = msgs.last().unwrap()["content"].as_str().unwrap();
        assert!(last_content.contains("goodbye"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_recent_files_skips_missing() {
        let reads = vec![
            ("/nonexistent/path/xyz.rs".to_string(), 1),
        ];
        let msgs = restore_recent_files(&reads, None);
        assert!(msgs.is_empty());
    }

    #[test]
    fn restore_recent_files_deduplicates_by_path() {
        let dir = std::env::temp_dir().join("restore_test_dedup");
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("dup.txt");
        std::fs::write(&f, "content").unwrap();

        let path = f.to_string_lossy().to_string();
        let reads = vec![
            (path.clone(), 1),
            (path.clone(), 5),
            (path.clone(), 3),
        ];
        let msgs = restore_recent_files(&reads, None);
        // Should only produce one message despite 3 entries for same path
        assert_eq!(msgs.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_recent_files_respects_max_limit() {
        let dir = std::env::temp_dir().join("restore_test_limit");
        let _ = std::fs::create_dir_all(&dir);

        let mut reads = Vec::new();
        for i in 0..MAX_FILES_TO_RESTORE + 5 {
            let f = dir.join(format!("file_{i}.txt"));
            std::fs::write(&f, format!("content {i}")).unwrap();
            reads.push((f.to_string_lossy().to_string(), i as u32));
        }

        let msgs = restore_recent_files(&reads, None);
        assert!(msgs.len() <= MAX_FILES_TO_RESTORE);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_recent_files_resolves_relative_with_cwd() {
        let dir = std::env::temp_dir().join("restore_test_cwd");
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("relative.txt");
        std::fs::write(&f, "relative content").unwrap();

        let reads = vec![("relative.txt".to_string(), 1)];
        let msgs = restore_recent_files(&reads, Some(dir.to_str().unwrap()));
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0]["content"].as_str().unwrap().contains("relative content"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
