//! Session Memory extraction prompt and logic.
//!
//! This module handles the LLM-based extraction of key information from
//! conversation history into the session memory document.

use serde_json::Value;

use super::session_memory::{SESSION_MEMORY_SECTIONS, SessionMemory};

/// Build the extraction prompt for updating session memory.
///
/// The prompt instructs the LLM to extract key information from the
/// conversation and update the appropriate sections of the session memory.
pub fn build_extraction_prompt(
    current_memory: &SessionMemory,
    messages_since_last_extraction: &[Value],
) -> String {
    let messages_text = format_messages_for_extraction(messages_since_last_extraction);

    format!(
        r#"You are updating a session memory document that tracks the important context of a coding session.

## Current Session Memory

```markdown
{current_memory}
```

## New Conversation Since Last Update

{messages_text}

## Instructions

Update the session memory based on the new conversation above. Follow these rules:

1. **Preserve existing valuable information** — only update sections that have new relevant content.
2. **Be concise** — each section should capture the essence, not transcript everything.
3. **Focus on actionable context** — what would help a fresh assistant continue this work?
4. **Keep section headers exactly as they are** — do not rename sections.

## Section Guidelines

- **Session Title**: Update only if the focus has changed significantly.
- **Current State**: What's the latest status? What was just completed or is in progress?
- **Task Specification**: User's goals, requirements, constraints.
- **Files and Functions**: Key files, functions, classes referenced.
- **Workflow**: Patterns, processes, build/test commands used.
- **Errors and Corrections**: Notable errors encountered and how they were resolved.
- **Codebase Documentation**: Important architectural or design notes discovered.
- **Learnings**: Conventions, best practices, gotchas learned.
- **Key Results**: Artifacts produced, commits made, significant outcomes.
- **Worklog**: Brief chronological log of major actions.

## Output Format

Output ONLY the updated session memory document in markdown format, starting with `# Session Title`.
Do not include any explanation or commentary outside the markdown."#,
        current_memory = current_memory.content,
        messages_text = messages_text,
    )
}

/// Format messages for inclusion in the extraction prompt.
fn format_messages_for_extraction(messages: &[Value]) -> String {
    let mut parts = Vec::new();

    for msg in messages.iter().take(50) {
        // Limit to last 50 messages
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("unknown");
        let content = msg.get("content").and_then(Value::as_str).unwrap_or("");

        // Skip empty content
        if content.trim().is_empty() && msg.get("tool_calls").is_none() {
            continue;
        }

        // Format role
        let role_label = match role {
            "user" => "**User**",
            "assistant" => "**Assistant**",
            "tool" => "**Tool Result**",
            "system" => continue, // Skip system messages
            _ => role,
        };

        // Truncate very long content
        let truncated = if content.len() > 2000 {
            format!(
                "{}... [truncated, {} chars total]",
                &content[..2000],
                content.len()
            )
        } else {
            content.to_string()
        };

        parts.push(format!("{role_label}: {truncated}"));

        // Include tool call names if present
        if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
            let tool_names: Vec<&str> = tool_calls
                .iter()
                .filter_map(|tc| {
                    tc.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                })
                .collect();
            if !tool_names.is_empty() {
                parts.push(format!("  _Tools called: {}_", tool_names.join(", ")));
            }
        }
    }

    if parts.is_empty() {
        "(No new messages)".to_string()
    } else {
        parts.join("\n\n")
    }
}

/// Parse LLM response and update session memory.
///
/// Returns `true` if the memory was successfully updated.
pub fn parse_extraction_response(
    session_memory: &mut SessionMemory,
    response: &str,
) -> Result<(), String> {
    // Validate that the response contains the expected sections
    let has_title = response.contains("# Session Title");
    if !has_title {
        return Err("Response missing '# Session Title' header".to_string());
    }

    // Check that we have at least a few sections
    let section_count = SESSION_MEMORY_SECTIONS
        .iter()
        .filter(|s| response.contains(&format!("# {s}")))
        .count();

    if section_count < 3 {
        return Err(format!(
            "Response has only {section_count} sections, expected at least 3"
        ));
    }

    // Update the memory content
    session_memory.content = response.trim().to_string();

    Ok(())
}

/// Estimate tokens needed for the extraction prompt.
pub fn estimate_extraction_prompt_tokens(
    session_memory: &SessionMemory,
    messages_since_last: &[Value],
) -> usize {
    let base_prompt_tokens = 500; // Template overhead
    let memory_tokens = session_memory.estimate_tokens();
    let messages_tokens: usize = messages_since_last
        .iter()
        .map(|m| {
            let content_len = m
                .get("content")
                .and_then(Value::as_str)
                .map(|s| s.len().min(2000)) // Truncation applied
                .unwrap_or(0);
            crate::prompts::estimate_str_tokens(&"x".repeat(content_len))
        })
        .sum();

    base_prompt_tokens + memory_tokens + messages_tokens
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_extraction_prompt_includes_memory() {
        let mut sm = SessionMemory::new(
            "/tmp/test",
            super::super::session_memory::SessionMemoryConfig::default(),
        );
        sm.init_with_template();
        sm.content = sm.content.replace(
            "# Session Title\n*A brief title describing what this session is about*",
            "# Session Title\n*A brief title describing what this session is about*\nImplementing feature X",
        );

        let messages = vec![
            json!({"role": "user", "content": "Please add tests"}),
            json!({"role": "assistant", "content": "I'll add tests for the new feature."}),
        ];

        let prompt = build_extraction_prompt(&sm, &messages);

        assert!(prompt.contains("Implementing feature X"));
        assert!(prompt.contains("Please add tests"));
        assert!(prompt.contains("I'll add tests"));
        assert!(prompt.contains("## Instructions"));
    }

    #[test]
    fn format_messages_truncates_long_content() {
        let long_content = "x".repeat(5000);
        let messages = vec![json!({"role": "user", "content": long_content})];

        let formatted = format_messages_for_extraction(&messages);

        assert!(formatted.contains("[truncated, 5000 chars total]"));
        assert!(formatted.len() < 3000);
    }

    #[test]
    fn format_messages_includes_tool_calls() {
        let messages = vec![json!({
            "role": "assistant",
            "content": "Let me check that file.",
            "tool_calls": [{
                "function": {"name": "read_file", "arguments": "{}"}
            }, {
                "function": {"name": "grep", "arguments": "{}"}
            }]
        })];

        let formatted = format_messages_for_extraction(&messages);

        assert!(formatted.contains("Tools called: read_file, grep"));
    }

    #[test]
    fn parse_extraction_response_validates_sections() {
        let mut sm = SessionMemory::new(
            "/tmp/test",
            super::super::session_memory::SessionMemoryConfig::default(),
        );
        sm.init_with_template();

        // Invalid: missing sections
        let bad_response = "# Session Title\nSome title";
        assert!(parse_extraction_response(&mut sm, bad_response).is_err());

        // Valid: has required sections
        let good_response = "# Session Title\nTest\n\n# Current State\nWorking\n\n# Task Specification\nDoing X\n\n# Files and Functions\nfile.rs".to_string();
        assert!(parse_extraction_response(&mut sm, &good_response).is_ok());
        assert!(sm.content.contains("Test"));
    }
}
