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

#[derive(Debug, Clone, Default)]
pub struct SessionMemoryState {
    pub initialized: bool,
    pub tokens_at_last_extraction: usize,
    pub tool_calls_at_last_extraction: usize,
    pub last_extraction_time: Option<Instant>,
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

/// Check whether extraction should run, with error-triggered override.
/// When `had_error_this_turn` is true and the session is past the init gate,
/// extraction triggers immediately to capture user corrections before compaction.
pub fn should_extract_with_error_trigger(
    state: &SessionMemoryState,
    current_tokens: usize,
    current_tool_calls: usize,
    config: &SessionMemoryExtractConfig,
    had_error_this_turn: bool,
) -> bool {
    let normal = should_extract(state, current_tokens, current_tool_calls, config);
    if normal {
        return true;
    }
    // Error trigger: if an error occurred and we're past the init gate,
    // extract immediately to capture user corrections.
    had_error_this_turn && current_tokens >= config.min_tokens_to_init
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
// Section extraction for knowledge backflow
// ---------------------------------------------------------------------------

/// Extract the content under a specific `## Section` header from the session memory markdown.
/// Returns None if the section is empty or missing (beyond the HTML comment placeholder).
pub fn extract_section(memory_md: &str, section_name: &str) -> Option<String> {
    let header = format!("## {section_name}");
    // Match only `## ` (h2), not `### ` (h3) that also contains the substring.
    let start = if memory_md.starts_with(&header) {
        Some(0)
    } else {
        memory_md.find(&format!("\n{header}")).map(|i| i + 1)
    }?;
    let after_header = start + header.len();
    // Find the next ## header or end of string
    let rest = &memory_md[after_header..];
    let end = rest
        .find("\n## ")
        .map(|i| after_header + i)
        .unwrap_or(memory_md.len());
    let section_body = memory_md[after_header..end].trim();
    // Skip if empty or only contains a single HTML comment placeholder
    if section_body.is_empty()
        || (section_body.starts_with("<!--")
            && section_body.ends_with("-->")
            && section_body.matches("-->").count() == 1)
    {
        return None;
    }
    // Strip the leading HTML comment if present
    let content = if let Some(after_comment) = section_body.find("-->") {
        section_body[after_comment + 3..].trim()
    } else {
        section_body
    };
    if content.is_empty() {
        None
    } else {
        Some(content.to_string())
    }
}

/// Extract "Learnings" and "Errors & Corrections" sections from session memory markdown.
/// These sections have cross-session reuse value and should be stored as semantic memory.
pub fn extract_learnings_for_backflow(memory_md: &str) -> Vec<(String, String)> {
    let mut results = Vec::new();
    if let Some(content) = extract_section(memory_md, "Learnings") {
        results.push(("learnings".to_string(), content));
    }
    if let Some(content) = extract_section(memory_md, "Errors & Corrections") {
        results.push(("error-corrections".to_string(), content));
    }
    results
}

// ---------------------------------------------------------------------------
// Final learnings extraction (session-end → .astra/knowledge.md)
// ---------------------------------------------------------------------------

/// Maximum context length (bytes) for the learnings extraction prompt.
const MAX_LEARNINGS_PROMPT_BYTES: usize = 30_000;

/// Truncate text to fit within `max_bytes`, breaking at a char boundary.
pub fn truncate_for_prompt(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Build the LLM prompt for extracting learnings at session end.
///
/// Returns a messages array (system + user) for the LLM call.
/// The prompt asks the LLM to produce a concise list of learnings suitable
/// for appending to `.astra/knowledge.md`.
pub fn build_learnings_extraction_prompt(
    session_memory: &str,
    recent_messages: &[Value],
) -> Vec<Value> {
    let recent_text = render_recent(recent_messages);
    let combined = format!(
        "## Session memory:\n\n{session_memory}\n\n## Recent conversation:\n\n{recent_text}"
    );
    let truncated = truncate_for_prompt(&combined, MAX_LEARNINGS_PROMPT_BYTES);

    let system = "\
You are a knowledge extractor. From the session context below, extract \
reusable learnings that would help in future sessions on this project.

Rules:
- Output ONLY a markdown list (- item) of learnings. No headers, no preamble.
- Each item should be a single, actionable insight (command, pattern, gotcha, preference).
- Be specific: include file paths, command names, flag names where relevant.
- Skip session-specific state (what was done, current progress). Focus on reusable knowledge.
- If there are no meaningful learnings, output exactly: NONE
- Maximum 10 items. Prefer quality over quantity.

Example output:
- `cargo test -p astra-runtime --lib` runs only library unit tests for the runtime crate
- The `session_memory_extract` module uses dual thresholds (tokens + tool calls) before triggering extraction
- `.astra/instructions.md` is automatically injected into every turn as project instructions";

    let user = format!(
        "{truncated}\n\n\
         Extract reusable learnings from this session:"
    );
    vec![
        serde_json::json!({"role": "system", "content": system}),
        serde_json::json!({"role": "user", "content": user}),
    ]
}

/// Parse the LLM response for learnings extraction.
///
/// Returns `None` if the model said "NONE" or the response is empty.
/// Otherwise returns the cleaned list of learnings (markdown bullet items).
pub fn parse_learnings_response(response: &str) -> Option<String> {
    let trimmed = response.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
        return None;
    }
    // Keep only lines that look like bullet items (- or *)
    let lines: Vec<&str> = trimmed
        .lines()
        .filter(|l| {
            let s = l.trim_start();
            s.starts_with("- ") || s.starts_with("* ")
        })
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(lines.join("\n"))
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

    // ── Error-Triggered Extraction Tests ─────────────────────────────

    #[test]
    fn error_trigger_fires_when_past_init_gate() {
        let state = SessionMemoryState {
            initialized: true,
            tokens_at_last_extraction: 10_000,
            tool_calls_at_last_extraction: 5,
            last_extraction_time: None,
        };
        let config = SessionMemoryExtractConfig::default();
        // Normal threshold NOT met (insufficient growth)
        assert!(!should_extract(&state, 12_000, 6, &config));
        // But error trigger fires
        assert!(should_extract_with_error_trigger(
            &state, 12_000, 6, &config, true
        ));
    }

    #[test]
    fn error_trigger_does_not_fire_below_init_gate() {
        let state = SessionMemoryState::default();
        let config = SessionMemoryExtractConfig::default();
        // Below init gate (5K < 10K)
        assert!(!should_extract_with_error_trigger(
            &state, 5_000, 0, &config, true
        ));
    }

    #[test]
    fn error_trigger_does_not_fire_without_error() {
        let state = SessionMemoryState {
            initialized: true,
            tokens_at_last_extraction: 10_000,
            tool_calls_at_last_extraction: 5,
            last_extraction_time: None,
        };
        let config = SessionMemoryExtractConfig::default();
        // No error, insufficient growth
        assert!(!should_extract_with_error_trigger(
            &state, 12_000, 6, &config, false
        ));
    }

    #[test]
    fn error_trigger_redundant_when_normal_threshold_met() {
        let state = SessionMemoryState {
            initialized: true,
            tokens_at_last_extraction: 10_000,
            tool_calls_at_last_extraction: 5,
            last_extraction_time: None,
        };
        let config = SessionMemoryExtractConfig::default();
        // Normal threshold met
        assert!(should_extract_with_error_trigger(
            &state, 16_000, 9, &config, false
        ));
        // Also met with error (redundant but should still return true)
        assert!(should_extract_with_error_trigger(
            &state, 16_000, 9, &config, true
        ));
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

    #[test]
    fn extract_section_learnings() {
        let md = "## Session Title\nTest session\n\n## Learnings\n<!-- Patterns -->\n- Use `cargo test` for validation\n- Prefer `edit` over `create` for existing files\n\n## Worklog\n<!-- Log -->\n";
        let section = extract_section(md, "Learnings").unwrap();
        assert!(section.contains("cargo test"));
        assert!(section.contains("Prefer `edit`"));
    }

    #[test]
    fn extract_section_empty_returns_none() {
        let md = "## Learnings\n<!-- Patterns -->\n\n## Worklog\n<!-- Log -->\n";
        assert!(extract_section(md, "Learnings").is_none());
    }

    #[test]
    fn extract_section_missing_returns_none() {
        let md = "## Session Title\nTest\n";
        assert!(extract_section(md, "Learnings").is_none());
    }

    #[test]
    fn extract_section_with_interleaved_html_comments() {
        // Regression: section with real content between two HTML comments
        // should NOT be treated as "only a placeholder".
        let md = "## Learnings\n<!-- Patterns -->\n- Always run clippy\n<!-- TODO -->\n";
        let section = extract_section(md, "Learnings");
        assert!(section.is_some(), "should extract content between comments");
        let content = section.unwrap();
        assert!(content.contains("Always run clippy"));
    }

    #[test]
    fn extract_section_single_comment_is_empty() {
        // A section with only a single HTML comment should be treated as empty.
        let md = "## Learnings\n<!-- placeholder -->\n## Next\n";
        assert!(extract_section(md, "Learnings").is_none());
    }

    #[test]
    fn extract_learnings_for_backflow_both_sections() {
        let md = "\
## Session Title\nBuild fix session\n\n\
## Current State\n<!-- state -->\nDone\n\n\
## Errors & Corrections\n<!-- errors -->\n- Compile error in auth.rs: missing lifetime → added 'a\n\n\
## Learnings\n<!-- patterns -->\n- Always run clippy before commit\n- Use #[derive(Clone)] for config structs\n\n\
## Worklog\n<!-- log -->\n";
        let sections = extract_learnings_for_backflow(md);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].0, "learnings");
        assert!(sections[0].1.contains("clippy"));
        assert_eq!(sections[1].0, "error-corrections");
        assert!(sections[1].1.contains("lifetime"));
    }

    #[test]
    fn extract_learnings_for_backflow_empty_session() {
        let sections = extract_learnings_for_backflow(SESSION_MEMORY_TEMPLATE);
        assert!(sections.is_empty(), "template has no content to extract");
    }

    // Tests for P4: learnings extraction → knowledge.md

    #[test]
    fn truncate_for_prompt_no_truncation() {
        assert_eq!(truncate_for_prompt("hello", 10), "hello");
    }

    #[test]
    fn truncate_for_prompt_exact_boundary() {
        assert_eq!(truncate_for_prompt("hello", 5), "hello");
    }

    #[test]
    fn truncate_for_prompt_truncates() {
        assert_eq!(truncate_for_prompt("hello world", 5), "hello");
    }

    #[test]
    fn truncate_for_prompt_cjk_boundary() {
        let text = "你好世界"; // 4 CJK chars = 12 bytes
        let result = truncate_for_prompt(text, 7);
        // Should truncate at char boundary (6 bytes = "你好")
        assert_eq!(result, "你好");
    }

    #[test]
    fn build_learnings_extraction_prompt_structure() {
        let msgs = vec![serde_json::json!({"role": "user", "content": "fix the bug"})];
        let result = build_learnings_extraction_prompt("## Session memory content", &msgs);
        assert_eq!(result.len(), 2); // system + user
        let system = result[0]["content"].as_str().unwrap();
        assert!(system.contains("knowledge extractor"));
        assert!(system.contains("NONE"));
        let user = result[1]["content"].as_str().unwrap();
        assert!(user.contains("Session memory"));
    }

    #[test]
    fn parse_learnings_response_none_cases() {
        assert!(parse_learnings_response("").is_none());
        assert!(parse_learnings_response("NONE").is_none());
        assert!(parse_learnings_response("  none  ").is_none());
        assert!(parse_learnings_response("No learnings here").is_none());
    }

    #[test]
    fn parse_learnings_response_extracts_bullets() {
        let resp = "Here are the learnings:\n\
                    - Always run `cargo check` before committing\n\
                    - Use `edit` instead of `create` for existing files\n\
                    Some trailing text";
        let result = parse_learnings_response(resp).unwrap();
        assert!(result.contains("cargo check"));
        assert!(result.contains("`edit`"));
        assert!(!result.contains("trailing text"));
        assert!(!result.contains("Here are"));
    }

    #[test]
    fn parse_learnings_response_star_bullets() {
        let resp = "* Insight one\n* Insight two";
        let result = parse_learnings_response(resp).unwrap();
        assert!(result.contains("Insight one"));
        assert!(result.contains("Insight two"));
    }
}
