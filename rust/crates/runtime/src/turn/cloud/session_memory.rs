//! Session Memory (SM) system for fast compaction.
//!
//! Instead of generating an LLM summary at compaction time, this module
//! continuously extracts conversation notes into a structured markdown file.
//! When compaction is needed, we use the pre-extracted memory directly,
//! avoiding the cost and latency of an additional LLM call.
//!
//! Inspired by claudecode's session memory system, adapted for mo-agent.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for session memory extraction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMemoryConfig {
    /// Minimum total tokens before SM extraction is initialized.
    /// Default: 10,000 tokens.
    pub min_tokens_to_init: usize,
    /// Minimum token growth since last extraction to trigger a new extraction.
    /// Default: 5,000 tokens.
    pub min_tokens_between_updates: usize,
    /// Minimum tool calls since last extraction to trigger (combined with token threshold).
    /// Default: 3 tool calls.
    pub tool_calls_between_updates: usize,
    /// Maximum tokens for the session memory content itself.
    /// Default: 12,000 tokens (~48K chars).
    pub max_memory_tokens: usize,
    /// Maximum tokens per section in session memory.
    /// Default: 2,000 tokens.
    pub max_tokens_per_section: usize,
}

impl Default for SessionMemoryConfig {
    fn default() -> Self {
        Self {
            min_tokens_to_init: 10_000,
            min_tokens_between_updates: 5_000,
            tool_calls_between_updates: 3,
            max_memory_tokens: 12_000,
            max_tokens_per_section: 2_000,
        }
    }
}

// ---------------------------------------------------------------------------
// State Tracking
// ---------------------------------------------------------------------------

/// Tracks the current state of session memory extraction.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMemoryState {
    /// Whether SM has been initialized (reached min_tokens_to_init).
    pub initialized: bool,
    /// Turn number when last extraction completed.
    pub last_extracted_turn: Option<u32>,
    /// Message UUID of the last summarized message (boundary marker).
    pub last_summarized_message_id: Option<String>,
    /// Total tokens when last extraction occurred.
    pub tokens_at_last_extraction: usize,
    /// Tool calls since last extraction.
    pub tool_calls_since_extraction: u32,
    /// Whether an extraction is currently in progress.
    pub extraction_in_progress: bool,
    /// Timestamp when extraction started (for timeout).
    pub extraction_started_at: Option<u64>,
}

impl SessionMemoryState {
    /// Check if extraction should be triggered.
    pub fn should_extract(
        &self,
        config: &SessionMemoryConfig,
        current_tokens: usize,
        has_pending_tool_calls: bool,
    ) -> bool {
        // Don't extract if already in progress
        if self.extraction_in_progress {
            return false;
        }

        // Check initialization threshold (one-time)
        if !self.initialized && current_tokens < config.min_tokens_to_init {
            return false;
        }

        // Check token growth since last extraction
        let token_growth = current_tokens.saturating_sub(self.tokens_at_last_extraction);
        if token_growth < config.min_tokens_between_updates {
            return false;
        }

        // Trigger if:
        // 1. Tool calls threshold met, OR
        // 2. No pending tool calls (natural pause point)
        self.tool_calls_since_extraction >= config.tool_calls_between_updates as u32
            || !has_pending_tool_calls
    }

    /// Mark extraction as started.
    pub fn start_extraction(&mut self) {
        self.extraction_in_progress = true;
        self.extraction_started_at = Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        );
    }

    /// Mark extraction as completed.
    pub fn complete_extraction(&mut self, turn: u32, message_id: Option<String>, tokens: usize) {
        self.initialized = true;
        self.extraction_in_progress = false;
        self.extraction_started_at = None;
        self.last_extracted_turn = Some(turn);
        self.last_summarized_message_id = message_id;
        self.tokens_at_last_extraction = tokens;
        self.tool_calls_since_extraction = 0;
    }

    /// Record a tool call (for extraction trigger tracking).
    pub fn record_tool_call(&mut self) {
        self.tool_calls_since_extraction += 1;
    }

    /// Check if extraction has timed out (15 second default).
    pub fn is_extraction_timed_out(&self, timeout_secs: u64) -> bool {
        if let Some(started) = self.extraction_started_at {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            now.saturating_sub(started) > timeout_secs
        } else {
            false
        }
    }

    /// Cancel a timed-out extraction.
    pub fn cancel_extraction(&mut self) {
        self.extraction_in_progress = false;
        self.extraction_started_at = None;
    }
}

// ---------------------------------------------------------------------------
// Session Memory Entry
// ---------------------------------------------------------------------------

/// Represents the session memory file and its location.
#[derive(Debug, Clone)]
pub struct SessionMemory {
    /// Path to the session memory markdown file.
    pub path: PathBuf,
    /// Current content of the memory file.
    pub content: String,
    /// State tracking for extraction.
    pub state: SessionMemoryState,
    /// Configuration.
    pub config: SessionMemoryConfig,
}

impl SessionMemory {
    /// Create a new session memory instance.
    pub fn new(session_dir: impl Into<PathBuf>, config: SessionMemoryConfig) -> Self {
        let dir: PathBuf = session_dir.into();
        Self {
            path: dir.join("session_memory.md"),
            content: String::new(),
            state: SessionMemoryState::default(),
            config,
        }
    }

    /// Initialize with the template content.
    pub fn init_with_template(&mut self) {
        self.content = SESSION_MEMORY_TEMPLATE.to_string();
    }

    /// Load existing memory file from disk.
    pub fn load(&mut self) -> std::io::Result<()> {
        if self.path.exists() {
            self.content = std::fs::read_to_string(&self.path)?;
        } else {
            self.init_with_template();
        }
        Ok(())
    }

    /// Save current content to disk.
    pub fn save(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, &self.content)
    }

    /// Check if the memory has meaningful content (not just template).
    pub fn has_content(&self) -> bool {
        // Check if any section has content beyond the italic template lines
        for section in SESSION_MEMORY_SECTIONS {
            if let Some(section_content) = self.get_section_content(section) {
                // Section has content if there's text after the italic description
                let lines: Vec<&str> = section_content.lines().collect();
                // First line is italic description, check if there's more
                if lines.len() > 1 {
                    let has_real_content = lines[1..].iter().any(|line| !line.trim().is_empty());
                    if has_real_content {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Extract content of a specific section.
    fn get_section_content(&self, section_header: &str) -> Option<String> {
        let header_line = format!("# {section_header}");
        let start = self.content.find(&header_line)?;
        let content_start = start + header_line.len();

        // Find the next section header or end of file
        let rest = &self.content[content_start..];
        let end = rest
            .find("\n# ")
            .map(|i| content_start + i)
            .unwrap_or(self.content.len());

        Some(self.content[content_start..end].trim().to_string())
    }

    /// Estimate token count of current content.
    pub fn estimate_tokens(&self) -> usize {
        // Simple estimate: ~4 chars per token for ASCII, ~1.5 tokens per CJK char
        crate::prompts::estimate_str_tokens(&self.content)
    }

    /// Load or create Session Memory for a given session ID.
    ///
    /// This is a convenience method for server-side integration.
    /// Returns `Some(memory)` if the memory exists and has meaningful content,
    /// or `None` if the memory doesn't exist or only has template content.
    pub fn load_or_create_for_session(session_id: &str) -> Option<Self> {
        // Use a standard location based on session_id
        // In production this would be under the session's working directory
        let session_dir = std::env::var("MO_SESSION_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                std::env::var("HOME")
                    .map(|h| {
                        PathBuf::from(h)
                            .join(".mo-agent")
                            .join("sessions")
                            .join(session_id)
                    })
                    .unwrap_or_else(|_| PathBuf::from("/tmp").join("mo-sessions").join(session_id))
            });

        let mut memory = Self::new(session_dir, SessionMemoryConfig::default());

        // Try to load existing memory
        if memory.load().is_ok() && memory.has_content() {
            Some(memory)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Template
// ---------------------------------------------------------------------------

/// Section headers in the session memory template.
pub const SESSION_MEMORY_SECTIONS: &[&str] = &[
    "Session Title",
    "Current State",
    "Task Specification",
    "Files and Functions",
    "Workflow",
    "Errors and Corrections",
    "Codebase Documentation",
    "Learnings",
    "Key Results",
    "Worklog",
];

/// The session memory markdown template.
/// Each section has an italic description line that guides the LLM.
pub const SESSION_MEMORY_TEMPLATE: &str = r#"# Session Title
*A brief title describing what this session is about*

# Current State
*What is the current state of the work? What was just completed or is in progress?*

# Task Specification
*What is the user trying to accomplish? Include key requirements and constraints.*

# Files and Functions
*Important files, functions, and code structures referenced in this session.*

# Workflow
*The approach being taken to solve the problem. Key steps and decisions.*

# Errors and Corrections
*Errors encountered and how they were resolved. What didn't work and why.*

# Codebase Documentation
*Important facts about the codebase learned during this session.*

# Learnings
*Key insights, patterns, and lessons learned.*

# Key Results
*Important outputs, artifacts, and accomplishments.*

# Worklog
*Chronological log of significant actions and their outcomes.*
"#;

// ---------------------------------------------------------------------------
// SM-Based Compaction Config
// ---------------------------------------------------------------------------

/// Configuration for session memory-based compaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmCompactConfig {
    /// Minimum tokens to keep after SM compaction.
    /// Default: 10,000 tokens.
    pub min_tokens_to_keep: usize,
    /// Minimum messages with text content to keep.
    /// Default: 5 messages.
    pub min_text_messages_to_keep: usize,
    /// Maximum tokens to keep (hard cap).
    /// Default: 40,000 tokens.
    pub max_tokens_to_keep: usize,
}

impl Default for SmCompactConfig {
    fn default() -> Self {
        Self {
            min_tokens_to_keep: 10_000,
            min_text_messages_to_keep: 5,
            max_tokens_to_keep: 40_000,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_values() {
        let cfg = SessionMemoryConfig::default();
        assert_eq!(cfg.min_tokens_to_init, 10_000);
        assert_eq!(cfg.min_tokens_between_updates, 5_000);
        assert_eq!(cfg.tool_calls_between_updates, 3);
    }

    #[test]
    fn state_should_extract_not_initialized() {
        let state = SessionMemoryState::default();
        let cfg = SessionMemoryConfig::default();
        // Below init threshold
        assert!(!state.should_extract(&cfg, 5_000, false));
    }

    #[test]
    fn state_should_extract_after_init() {
        let state = SessionMemoryState {
            initialized: true,
            tokens_at_last_extraction: 10_000,
            ..Default::default()
        };
        let cfg = SessionMemoryConfig::default();

        // Not enough token growth
        assert!(!state.should_extract(&cfg, 12_000, false));
        // Enough growth + no pending tools
        assert!(state.should_extract(&cfg, 16_000, false));
    }

    #[test]
    fn state_should_extract_with_tool_calls() {
        let state = SessionMemoryState {
            initialized: true,
            tokens_at_last_extraction: 10_000,
            tool_calls_since_extraction: 3,
            ..Default::default()
        };
        let cfg = SessionMemoryConfig::default();

        // Enough growth + tool calls threshold
        assert!(state.should_extract(&cfg, 16_000, true));
    }

    #[test]
    fn state_extraction_lifecycle() {
        let mut state = SessionMemoryState::default();

        // Start extraction
        state.start_extraction();
        assert!(state.extraction_in_progress);
        assert!(state.extraction_started_at.is_some());

        // Complete extraction
        state.complete_extraction(5, Some("msg-123".into()), 20_000);
        assert!(!state.extraction_in_progress);
        assert!(state.initialized);
        assert_eq!(state.last_extracted_turn, Some(5));
        assert_eq!(state.last_summarized_message_id.as_deref(), Some("msg-123"));
        assert_eq!(state.tokens_at_last_extraction, 20_000);
        assert_eq!(state.tool_calls_since_extraction, 0);
    }

    #[test]
    fn state_extraction_timeout() {
        let mut state = SessionMemoryState::default();
        state.start_extraction();
        // Manually set started_at to simulate timeout
        state.extraction_started_at = Some(0);
        assert!(state.is_extraction_timed_out(15));

        state.cancel_extraction();
        assert!(!state.extraction_in_progress);
    }

    #[test]
    fn session_memory_template_has_all_sections() {
        for section in SESSION_MEMORY_SECTIONS {
            assert!(
                SESSION_MEMORY_TEMPLATE.contains(&format!("# {section}")),
                "Missing section: {section}"
            );
        }
    }

    #[test]
    fn session_memory_has_content_empty_template() {
        let mut sm = SessionMemory::new("/tmp/test", SessionMemoryConfig::default());
        sm.init_with_template();
        assert!(!sm.has_content(), "Empty template should have no content");
    }

    #[test]
    fn session_memory_has_content_with_additions() {
        let mut sm = SessionMemory::new("/tmp/test", SessionMemoryConfig::default());
        sm.content = SESSION_MEMORY_TEMPLATE.to_string();
        // Add content under "Current State"
        sm.content = sm.content.replace(
            "# Current State\n*What is the current state",
            "# Current State\n*What is the current state of the work?*\nWorking on feature X",
        );
        assert!(sm.has_content(), "Should detect content under section");
    }

    #[test]
    fn sm_compact_config_defaults() {
        let cfg = SmCompactConfig::default();
        assert_eq!(cfg.min_tokens_to_keep, 10_000);
        assert_eq!(cfg.min_text_messages_to_keep, 5);
        assert_eq!(cfg.max_tokens_to_keep, 40_000);
    }
}
