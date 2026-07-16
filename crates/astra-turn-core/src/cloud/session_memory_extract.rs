//! Session memory extraction during turns.
//!
//! Extracts changed session state into a canonical working-memory snapshot.

use std::time::Instant;

use serde_json::Value;

use astra_turn_types::{is_runtime_scaffolding_message, is_transient_runtime_status_text};

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct SessionMemoryState {
    pub initialized: bool,
    pub content_fingerprint: Option<u64>,
    pub turn_at_last_extraction: u32,
    pub last_extraction_time: Option<Instant>,
}

impl SessionMemoryState {
    /// Advance state after a successful extraction (LLM or rule-based
    /// fallback produced content that was written to disk). Call this
    /// *only* when a write actually landed — otherwise the next
    /// `should_extract` check will be debounced against a point that
    /// never ran, and the session will look "caught up" when it isn't.
    pub fn mark_extracted(&mut self, content_fingerprint: u64, turn: u32) {
        self.initialized = true;
        self.content_fingerprint = Some(content_fingerprint);
        self.turn_at_last_extraction = turn;
        self.last_extraction_time = Some(Instant::now());
    }
}

/// Run on the first meaningful snapshot and whenever the canonical extraction
/// input changes. Token counts and tool-call counts are deliberately excluded:
/// they are indirect cost signals, not evidence that memory became stale.
pub fn should_extract(state: &SessionMemoryState, content_fingerprint: u64) -> bool {
    !state.initialized || state.content_fingerprint != Some(content_fingerprint)
}

// ---------------------------------------------------------------------------
// Template
// ---------------------------------------------------------------------------

pub const SESSION_MEMORY_TEMPLATE: &str = "\
# Session Memory

## Session Title
<!-- One-line description of the session goal -->

## Active Goals
<!-- Current goals explicitly stated by the user or assistant. Do NOT invent goals. -->

## Pending Todos
<!-- Explicit open loops that still matter. Leave empty when none are known. -->

## Current State
<!-- What is happening right now -->

## Task Specification
<!-- The user's original request and requirements -->

## Errors & Corrections
<!-- Errors encountered and how they were fixed -->

## Learnings
<!-- Patterns, preferences, or conventions discovered -->
";

// ---------------------------------------------------------------------------
// Prompt construction
// ---------------------------------------------------------------------------

/// Build messages for the extraction LLM call.
pub fn build_extraction_prompt(current_memory: &str, recent_messages: &[Value]) -> Vec<Value> {
    let recent_text = render_recent(recent_messages);
    let system = "You update session memory from the latest conversation delta. Return exactly one JSON object containing only fields whose canonical value changed. Omit unchanged fields. Allowed fields and types:\n\
         - session_title: string\n\
         - task_spec: string\n\
         - active_goals: string[]\n\
         - pending_todos: string[]\n\
         - current_state: string[]\n\
         - corrections: string[]\n\
         - learnings: string[]\n\
         An empty string or array explicitly clears that field. Rules:\n\
         - Be factual and concise; each list has at most 8 items.\n\
         - Evidence authority is explicit user statements/corrections and tool results. Treat assistant claims as unverified until corroborated by one of those sources.\n\
         - Learnings must cite a durable user preference, an observed tool result, or a verified correction; never promote an assistant hypothesis into a learning.\n\
         - A newer correction or tool result supersedes contradictory existing memory; replace or clear the stale item instead of preserving both.\n\
         - Prefer the latest state and replace stale values instead of appending a history log.\n\
         - Active goals must be explicitly stated; never infer new goals.\n\
         - Pending todos contain only explicit open loops. Clear items that recent evidence shows are closed.\n\
         - Current state is a neutral resumable snapshot, not a completion report or instruction queue.\n\
         - Live repository, process, queue, permission, and verification state is not durable memory and must be rechecked with tools.\n\
         - Do not emit markdown, code fences, commentary, completed-work history, workflow logs, or placeholder values such as None.";
    let user = format!(
        "## Current session memory:\n\n{current_memory}\n\n\
         ## Recent conversation:\n\n{recent_text}\n\n\
         Output the sparse JSON update:"
    );
    vec![
        serde_json::json!({"role": "system", "content": system}),
        serde_json::json!({"role": "user", "content": user}),
    ]
}

fn render_recent(messages: &[Value]) -> String {
    const MAX_MESSAGES: usize = 64;
    const MAX_TOTAL_CHARS: usize = 20_000;

    let recent: Vec<_> = messages
        .iter()
        .rev()
        .filter(|msg| !is_ephemeral_for_memory_extraction(msg))
        .take(MAX_MESSAGES)
        .collect();
    let mut rendered = Vec::new();
    let mut total_chars = 0usize;
    for msg in recent {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("?");
        let Some(content) = extraction_message_content(msg, role) else {
            continue;
        };
        let per_message_cap = if role == "tool" { 2_400 } else { 1_600 };
        let content = clip_extraction_message(&content, per_message_cap);
        let label = match role {
            "user" => "USER",
            "assistant" => "ASSISTANT",
            "tool" => "TOOL",
            _ => continue,
        };
        let line = format!("[{label}]: {content}\n");
        let line_chars = line.chars().count();
        if total_chars.saturating_add(line_chars) > MAX_TOTAL_CHARS {
            continue;
        }
        total_chars += line_chars;
        rendered.push(line);
    }
    rendered.reverse();
    rendered.concat()
}

fn extraction_message_content(message: &Value, role: &str) -> Option<String> {
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        return (!text.trim().is_empty()).then(|| text.to_string());
    }
    if let Some(blocks) = message.get("content").and_then(Value::as_array) {
        let text = blocks
            .iter()
            .filter_map(|block| {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .or_else(|| block.get("content").and_then(Value::as_str))
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !text.trim().is_empty() {
            return Some(text);
        }
    }
    if role != "assistant" {
        return None;
    }
    let names = message
        .get("tool_calls")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|tool_call| {
            tool_call
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
        })
        .collect::<Vec<_>>();
    (!names.is_empty()).then(|| format!("[called: {}]", names.join(", ")))
}

fn clip_extraction_message(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let head = max_chars.saturating_mul(2) / 3;
    let tail = max_chars.saturating_sub(head);
    let prefix = text.chars().take(head).collect::<String>();
    let suffix = text
        .chars()
        .rev()
        .take(tail)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!("{prefix}\n…[middle omitted]…\n{suffix}")
}

fn is_ephemeral_for_memory_extraction(message: &Value) -> bool {
    is_runtime_scaffolding_message(message)
        || message
            .get("content")
            .and_then(Value::as_str)
            .is_some_and(is_transient_runtime_status_text)
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extraction_freshness_follows_semantic_fingerprint() {
        let mut state = SessionMemoryState::default();
        assert!(should_extract(&state, 41));

        state.mark_extracted(41, 3);

        assert!(!should_extract(&state, 41));
        assert!(should_extract(&state, 42));
        assert_eq!(state.turn_at_last_extraction, 3);
        assert!(state.last_extraction_time.is_some());
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
    fn extraction_prompt_requires_grounded_learnings_and_correction_precedence() {
        let result = build_extraction_prompt("", &[]);
        let system = result[0]["content"].as_str().unwrap();

        assert!(system.contains("Treat assistant claims as unverified"));
        assert!(system.contains("never promote an assistant hypothesis into a learning"));
        assert!(system.contains("supersedes contradictory existing memory"));
    }

    #[test]
    fn build_extraction_prompt_filters_runtime_scaffolding_but_keeps_user_corrections() {
        let msgs = vec![
            json!({"role": "user", "content": "Preserve long-running session goals"}),
            json!({"role": "assistant", "content": "✓ Previous round: 3 tools executed in parallel"}),
            json!({"role": "user", "content": "What I asked for is a durable fix, not a workaround"}),
            json!({"role": "user", "content": "wrong, never use mocks in integration tests"}),
        ];

        let result = build_extraction_prompt("", &msgs);
        let user_content = result[1]["content"].as_str().unwrap();

        assert!(user_content.contains("Preserve long-running session goals"));
        assert!(user_content.contains("durable fix, not a workaround"));
        assert!(user_content.contains("never use mocks in integration tests"));
        assert!(!user_content.contains("Previous round"));
    }

    #[test]
    fn session_memory_template_has_all_sections() {
        let sections = [
            "## Session Title",
            "## Active Goals",
            "## Pending Todos",
            "## Current State",
            "## Task Specification",
            "## Errors & Corrections",
            "## Learnings",
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

    // ── Canonical narrative sections ────────────────────────────────────

    #[test]
    fn session_memory_template_has_only_live_narrative_sections() {
        assert!(
            SESSION_MEMORY_TEMPLATE.contains("## Active Goals"),
            "template missing ## Active Goals"
        );
        assert!(
            SESSION_MEMORY_TEMPLATE.contains("## Pending Todos"),
            "template missing ## Pending Todos"
        );
        assert!(!SESSION_MEMORY_TEMPLATE.contains("## Completed"));
        assert!(!SESSION_MEMORY_TEMPLATE.contains("## Worklog"));
    }
}
