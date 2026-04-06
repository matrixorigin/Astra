//! Session memory extraction during turns.
//!
//! Periodically extracts key information from conversation into a structured
//! markdown file (edge) and Memoria working memory (cloud), triggered by
//! dual thresholds: token growth AND tool call count.

use std::path::Path;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMemoryExtractConfig {
    pub min_tokens_to_init: usize,
    pub min_tokens_between_updates: usize,
    pub min_tool_calls_between_updates: usize,
    pub max_section_tokens: usize,
    pub max_total_tokens: usize,
}

impl Default for SessionMemoryExtractConfig {
    fn default() -> Self {
        Self {
            min_tokens_to_init: 10_000,
            min_tokens_between_updates: 5_000,
            min_tool_calls_between_updates: 3,
            max_section_tokens: 2_000,
            max_total_tokens: 12_000,
        }
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SessionMemoryState {
    pub initialized: bool,
    pub tokens_at_last_extraction: usize,
    pub tool_calls_at_last_extraction: usize,
    pub last_extraction_time: Option<Instant>,
}

impl Default for SessionMemoryState {
    fn default() -> Self {
        Self {
            initialized: false,
            tokens_at_last_extraction: 0,
            tool_calls_at_last_extraction: 0,
            last_extraction_time: None,
        }
    }
}

/// Check whether extraction should run based on dual thresholds.
pub fn should_extract(
    state: &SessionMemoryState,
    current_tokens: usize,
    current_tool_calls: usize,
    config: &SessionMemoryExtractConfig,
) -> bool {
    if !state.initialized {
        return current_tokens >= config.min_tokens_to_init;
    }
    let token_growth = current_tokens.saturating_sub(state.tokens_at_last_extraction);
    let tool_growth = current_tool_calls.saturating_sub(state.tool_calls_at_last_extraction);
    token_growth >= config.min_tokens_between_updates
        && tool_growth >= config.min_tool_calls_between_updates
}

// ---------------------------------------------------------------------------
// Template
// ---------------------------------------------------------------------------

pub const SESSION_MEMORY_TEMPLATE: &str = "\
# Session Memory

## Session Title
<!-- One-line description of the session goal -->

## Current State
<!-- What is happening right now -->

## Task Specification
<!-- The user's original request and requirements -->

## Files and Functions
<!-- Key files and functions being worked on -->

## Workflow
<!-- Steps taken so far, in order -->

## Errors & Corrections
<!-- Errors encountered and how they were fixed -->

## Learnings
<!-- Patterns, preferences, or conventions discovered -->

## Worklog
<!-- Chronological log of significant actions -->
";

// ---------------------------------------------------------------------------
// Prompt construction
// ---------------------------------------------------------------------------

/// Build messages for the extraction LLM call.
pub fn build_extraction_prompt(current_memory: &str, recent_messages: &[Value]) -> Vec<Value> {
    let recent_text = render_recent(recent_messages);
    let system = "You are a session memory updater. Update the session memory document below \
         based on the recent conversation. Rules:\n\
         - NEVER add, remove, or rename section headers.\n\
         - Only update content below each section's comment line.\n\
         - Keep each section under 200 words.\n\
         - Be factual and concise.\n\
         - Output the complete updated document.";
    let user = format!(
        "## Current session memory:\n\n{current_memory}\n\n\
         ## Recent conversation:\n\n{recent_text}\n\n\
         Output the updated session memory document:"
    );
    vec![
        serde_json::json!({"role": "system", "content": system}),
        serde_json::json!({"role": "user", "content": user}),
    ]
}

fn render_recent(messages: &[Value]) -> String {
    let mut out = String::new();
    let recent: Vec<_> = messages.iter().rev().take(20).collect();
    for msg in recent.into_iter().rev() {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("?");
        let content = msg.get("content").and_then(Value::as_str).unwrap_or("");
        let trunc = if content.len() > 500 {
            // Safe truncation: walk backwards from 500 to find a valid char boundary
            let mut end = 500;
            while end > 0 && !content.is_char_boundary(end) {
                end -= 1;
            }
            &content[..end]
        } else {
            content
        };
        match role {
            "user" => out.push_str(&format!("[USER]: {trunc}\n")),
            "assistant" => out.push_str(&format!("[ASSISTANT]: {trunc}\n")),
            "tool" => out.push_str(&format!("[TOOL]: {trunc}\n")),
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// File I/O
// ---------------------------------------------------------------------------

/// Atomic write: write to `.tmp` then rename.
pub fn write_session_memory_file(path: &Path, content: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("md.tmp");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn should_extract_false_below_init_threshold() {
        let state = SessionMemoryState::default();
        let config = SessionMemoryExtractConfig::default();
        assert!(!should_extract(&state, 5_000, 10, &config));
    }

    #[test]
    fn should_extract_true_above_init_threshold() {
        let state = SessionMemoryState::default();
        let config = SessionMemoryExtractConfig::default();
        assert!(should_extract(&state, 12_000, 0, &config));
    }

    #[test]
    fn should_extract_true_after_growth() {
        let state = SessionMemoryState {
            initialized: true,
            tokens_at_last_extraction: 10_000,
            tool_calls_at_last_extraction: 5,
            last_extraction_time: None,
        };
        let config = SessionMemoryExtractConfig::default();
        assert!(should_extract(&state, 16_000, 9, &config));
    }

    #[test]
    fn should_extract_false_insufficient_growth() {
        let state = SessionMemoryState {
            initialized: true,
            tokens_at_last_extraction: 10_000,
            tool_calls_at_last_extraction: 5,
            last_extraction_time: None,
        };
        let config = SessionMemoryExtractConfig::default();
        // Token growth OK (2K) but below 5K threshold
        assert!(!should_extract(&state, 12_000, 6, &config));
    }

    #[test]
    fn build_extraction_prompt_includes_current_memory() {
        let msgs = vec![json!({"role": "user", "content": "hello"})];
        let result = build_extraction_prompt("## Session Title\nTest", &msgs);
        let user_content = result[1]["content"].as_str().unwrap();
        assert!(user_content.contains("## Session Title"));
        assert!(user_content.contains("Test"));
    }

    #[test]
    fn write_session_memory_file_atomic() {
        let dir = std::env::temp_dir().join("sm_test_atomic");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("summary.md");
        write_session_memory_file(&path, "# Test\ncontent").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# Test\ncontent");
        assert!(!path.with_extension("md.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_memory_template_has_all_sections() {
        let sections = [
            "## Session Title",
            "## Current State",
            "## Task Specification",
            "## Files and Functions",
            "## Workflow",
            "## Errors & Corrections",
            "## Learnings",
            "## Worklog",
        ];
        for s in sections {
            assert!(SESSION_MEMORY_TEMPLATE.contains(s), "missing: {s}");
        }
    }
}
