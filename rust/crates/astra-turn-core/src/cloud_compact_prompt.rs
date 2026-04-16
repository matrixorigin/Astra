//! Compaction prompt templates.
//!
//! The prompt instructs the LLM to produce a dense summary of conversation
//! history that preserves task context, key decisions, and open questions —
//! while discarding verbatim tool outputs and redundant back-and-forth.

use serde_json::Value;

use astra_text_utils::str_preview::truncate_str;

/// System prompt used when asking the LLM to summarize conversation history.
pub const COMPACT_SYSTEM_PROMPT: &str = "You are a conversation summarizer. Create a structured summary preserving all essential context.\n\n\
IMPORTANT: First write an <analysis> block where you reason about what to preserve. \
Then write a <summary> block with the actual summary. The <analysis> block will be stripped.\n\n\
## Output format\n\n\
<analysis>\n\
Think step by step:\n\
1. What is the user's primary goal and current sub-task?\n\
2. What key decisions were made and WHY?\n\
3. What files are actively being worked on? What are the exact current contents/state?\n\
4. What approaches were tried? Which succeeded, which failed, and why?\n\
5. What errors occurred and how were they fixed (or are they still open)?\n\
6. What tasks remain and what is the immediate next step?\n\
</analysis>\n\n\
<summary>\n\
### Primary Request\nThe user's original task/goal in 1-2 sentences.\n\n\
### Key Technical Concepts\nDomain knowledge, architecture decisions, constraints discovered.\n\n\
### Files & Code Modified\nFile paths and what changed. Include brief code snippets for actively-edited sections. One bullet per file.\n\n\
### Problem Solving\nApproaches tried, what worked, what failed, and why. Include specific error messages that led to pivots.\n\n\
### Errors & Fixes\nErrors encountered and how they were resolved (or still open).\n\n\
### All User Messages\nEvery user intent/instruction, preserving their exact meaning.\n\n\
### Pending Tasks\nWhat remains to be done. Ordered by priority.\n\n\
### Current Work\nExact files being edited right now. Relevant code snippets. Immediate focus area.\n\n\
### Current State\nWhat was just completed. What the next step should be.\n\
</summary>\n\n\
## Rules\n\
- Be dense and factual. No filler.\n\
- Paraphrase tool outputs, don't reproduce verbatim.\n\
- For ### Files & Code Modified and ### Current Work, include brief code snippets when they help \
the reader understand the current state (function signatures, struct definitions, key logic).\n\
- Omit superseded decisions unless the failure is informative.\n\
- The reader is an LLM that needs only the essentials to continue the task seamlessly.";

/// Build the user message that presents the conversation history for summarization.
pub fn build_compact_user_prompt(conversation_text: &str) -> String {
    format!(
        "Please summarize the following conversation history. \
This summary will replace the full history to stay within context limits.\n\n\
---\n{conversation_text}\n---\n\n\
Provide a complete, dense summary following the format described."
    )
}

/// Render a slice of messages into plain text for inclusion in the compaction prompt.
///
/// Each message is formatted as `[ROLE]: content` with a separator.
/// Tool call arguments and tool results are included but truncated to avoid
/// re-inflating the context we're trying to compress.
pub fn render_messages_for_summary(messages: &[serde_json::Value]) -> String {
    const MAX_CONTENT_CHARS: usize = 2_000;
    const TOOL_RESULT_MAX_CHARS: usize = 800;

    let mut out = String::new();
    for msg in messages {
        let role = msg
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        let content = content_as_text_for_summary(msg.get("content"));

        match role {
            "system" => {
                // Skip system messages — they'll be re-injected by the runtime
                continue;
            }
            "tool" => {
                let truncated = truncate_str(&content, TOOL_RESULT_MAX_CHARS);
                out.push_str(&format!("[TOOL RESULT]: {truncated}\n\n"));
            }
            "assistant" => {
                // Include tool_calls summary if present
                if let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tool_calls {
                        let name = tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|n| n.as_str())
                            .unwrap_or("unknown_tool");
                        let args_scrubbed = tool_arguments_for_summary(
                            tc.get("function").and_then(|f| f.get("arguments")),
                        );
                        let args_short = truncate_str(&args_scrubbed, 300);
                        out.push_str(&format!("[ASSISTANT calls {name}({args_short})]\n"));
                    }
                }
                if !content.is_empty() {
                    let truncated = truncate_str(&content, MAX_CONTENT_CHARS);
                    out.push_str(&format!("[ASSISTANT]: {truncated}\n\n"));
                }
            }
            "user" => {
                // Skip attachment messages injected by the runtime
                if msg.get("attachment_metadata").is_some() {
                    continue;
                }
                let truncated = truncate_str(&content, MAX_CONTENT_CHARS);
                out.push_str(&format!("[USER]: {truncated}\n\n"));
            }
            _ => {
                let truncated = truncate_str(&content, MAX_CONTENT_CHARS);
                out.push_str(&format!("[{role}]: {truncated}\n\n"));
            }
        }
    }
    out
}

/// Strip `<analysis>...</analysis>` and extract content from `<summary>...</summary>`.
///
/// Falls back to the full input if no `<summary>` tags are found.
pub fn strip_analysis_block(raw: &str) -> String {
    if let Some(start) = raw.find("<summary>") {
        let after_tag = start + "<summary>".len();
        let end = raw[after_tag..]
            .find("</summary>")
            .map(|e| after_tag + e)
            .unwrap_or(raw.len());
        return raw[after_tag..end].trim().to_string();
    }
    let mut result = raw.to_string();
    while let Some(start) = result.find("<analysis>") {
        let end = result[start..]
            .find("</analysis>")
            .map(|e| start + e + "</analysis>".len())
            .unwrap_or(result.len());
        result = format!("{}{}", &result[..start], &result[end..]);
    }
    result.trim().to_string()
}

/// Strip analysis block and validate structured section headers.
///
/// If the summary lacks key section headers, prepends a warning so the LLM
/// (on the next turn) knows the summary may be incomplete.
pub fn format_structured_summary(raw: &str) -> String {
    let summary = strip_analysis_block(raw);
    const REQUIRED: &[&str] = &[
        "### Primary Request",
        "### Pending Tasks",
        "### Current Work",
        "### Current State",
    ];
    let missing: Vec<&&str> = REQUIRED.iter().filter(|h| !summary.contains(**h)).collect();
    if missing.is_empty() {
        summary
    } else {
        let names: Vec<&str> = missing.iter().map(|s| **s).collect();
        format!(
            "[compact warning: missing sections: {}]\n\n{}",
            names.join(", "),
            summary
        )
    }
}

/// Strip inline media / huge base64 from text before sending history to the summary LLM.
pub(crate) fn scrub_string_for_summary(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    const LARGE_THRESHOLD: usize = 12_000;
    if s.len() > LARGE_THRESHOLD && looks_like_base64_body(s) {
        return "[omitted: large base64-like payload]".to_string();
    }

    let mut out = String::with_capacity(s.len().min(16_384));
    let bytes = s.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() {
        let Some(rel) = s[idx..].find("data:") else {
            out.push_str(&s[idx..]);
            break;
        };
        let abs = idx + rel;
        out.push_str(&s[idx..abs]);
        let tail = &s[abs..];
        let Some(b64_pos) = tail.find("base64,") else {
            out.push_str("data:");
            idx = abs + 5;
            continue;
        };
        let payload_start = abs + b64_pos + 7;
        let mut end = payload_start;
        while end < bytes.len() {
            let c = bytes[end];
            // Strict base64 alphabet only — do not treat spaces or letters after the payload
            // (e.g. "…base64,AAA tail") as part of the data URL.
            if c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b'=') {
                end += 1;
                if end - payload_start > 512_000 {
                    break;
                }
            } else {
                break;
            }
        }
        out.push_str("[omitted: base64 or media data]");
        idx = end;
    }
    out
}

fn tool_arguments_for_summary(args: Option<&Value>) -> String {
    match args {
        None | Some(Value::Null) => "{}".to_string(),
        Some(Value::String(s)) => scrub_string_for_summary(s),
        Some(v) => scrub_string_for_summary(&v.to_string()),
    }
}

fn looks_like_base64_body(s: &str) -> bool {
    const NEED: usize = 2_000;
    let mut count = 0usize;
    for ch in s.chars() {
        if ch.is_whitespace() {
            continue;
        }
        if !matches!(
            ch,
            'A'..='Z' | 'a'..='z' | '0'..='9' | '+' | '/' | '='
        ) {
            return false;
        }
        count += 1;
        if count >= NEED {
            return true;
        }
    }
    false
}

fn content_as_text_for_summary(content: Option<&Value>) -> String {
    let Some(v) = content else {
        return String::new();
    };
    let raw = match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => {
            let mut pieces = Vec::new();
            for p in parts {
                if p.get("image_url").is_some() {
                    pieces.push("[omitted: image]".to_string());
                    continue;
                }
                let t = p.get("type").and_then(|x| x.as_str()).unwrap_or("");
                match t {
                    "text" => {
                        if let Some(txt) = p.get("text").and_then(|x| x.as_str()) {
                            pieces.push(txt.to_string());
                        }
                    }
                    "image_url" | "image" | "input_image" => {
                        pieces.push("[omitted: image]".to_string());
                    }
                    _ => {
                        if let Some(txt) = p.get("text").and_then(|x| x.as_str()) {
                            pieces.push(txt.to_string());
                        }
                    }
                }
            }
            pieces.join(" ")
        }
        Value::Null => String::new(),
        _ => v
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| v.to_string()),
    };
    scrub_string_for_summary(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn render_skips_system_messages() {
        let msgs = vec![
            json!({"role": "system", "content": "You are helpful"}),
            json!({"role": "user", "content": "hello"}),
        ];
        let rendered = render_messages_for_summary(&msgs);
        assert!(!rendered.contains("You are helpful"));
        assert!(rendered.contains("[USER]: hello"));
    }

    #[test]
    fn render_skips_attachment_messages() {
        let msgs = vec![
            json!({"role": "user", "content": "file content", "attachment_metadata": {"kind": "file"}}),
            json!({"role": "user", "content": "real question"}),
        ];
        let rendered = render_messages_for_summary(&msgs);
        assert!(!rendered.contains("file content"));
        assert!(rendered.contains("[USER]: real question"));
    }

    #[test]
    fn render_truncates_long_tool_results() {
        let long = "x".repeat(2000);
        let msgs = vec![json!({"role": "tool", "content": long})];
        let rendered = render_messages_for_summary(&msgs);
        assert!(rendered.contains("[TOOL RESULT]"));
        assert!(rendered.contains('…'));
        assert!(rendered.len() < 2000);
    }

    #[test]
    fn render_includes_tool_call_names() {
        let msgs = vec![json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "function": {
                    "name": "bash",
                    "arguments": "{\"command\": \"ls\"}"
                }
            }]
        })];
        let rendered = render_messages_for_summary(&msgs);
        assert!(rendered.contains("bash"));
    }

    #[test]
    fn render_scrubs_data_url_inside_tool_call_arguments_string() {
        let inner = format!(
            r#"{{"img":"data:image/png;base64,{}","cmd":"ls"}}"#,
            "y".repeat(100)
        );
        let msgs = vec![json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "function": {
                    "name": "upload",
                    "arguments": inner
                }
            }]
        })];
        let rendered = render_messages_for_summary(&msgs);
        assert!(rendered.contains("upload"));
        assert!(rendered.contains("[omitted: base64 or media data]"));
        assert!(!rendered.contains("data:image/png;base64,"));
        assert!(!rendered.contains(&"y".repeat(100)));
        assert!(rendered.contains("cmd"));
    }

    #[test]
    fn render_scrubs_tool_call_arguments_when_json_object() {
        let msgs = vec![json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "function": {
                    "name": "x",
                    "arguments": {
                        "blob": format!("data:text/plain;base64,{} tail", "z".repeat(50))
                    }
                }
            }]
        })];
        let rendered = render_messages_for_summary(&msgs);
        assert!(rendered.contains("x"));
        assert!(rendered.contains("[omitted: base64 or media data]"));
        assert!(!rendered.contains(&"z".repeat(50)));
    }

    #[test]
    fn build_compact_user_prompt_wraps_conversation() {
        let prompt = build_compact_user_prompt("some conversation");
        assert!(prompt.contains("some conversation"));
        assert!(prompt.contains("summarize"));
    }

    #[test]
    fn render_multimodal_user_drops_image_keeps_text() {
        let msgs = vec![json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "What is in this?"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
            ]
        })];
        let rendered = render_messages_for_summary(&msgs);
        assert!(rendered.contains("What is in this?"));
        assert!(rendered.contains("[omitted: image]"));
        assert!(!rendered.contains("AAAA"));
        assert!(!rendered.contains("data:image"));
    }

    #[test]
    fn render_strips_embedded_data_url_in_string_content() {
        let payload = format!("See: data:image/png;base64,{} tail", "x".repeat(120));
        let msgs = vec![json!({"role": "user", "content": payload})];
        let rendered = render_messages_for_summary(&msgs);
        assert!(rendered.contains("See:"));
        assert!(rendered.contains("tail"));
        assert!(rendered.contains("[omitted: base64 or media data]"));
        assert!(!rendered.contains("data:image/png;base64,"));
        assert!(!rendered.contains(&"x".repeat(120)));
    }

    #[test]
    fn render_collapses_large_plain_base64_tool_result() {
        let blob: String = "A".repeat(15_000);
        let msgs = vec![json!({"role": "tool", "content": blob})];
        let rendered = render_messages_for_summary(&msgs);
        assert!(rendered.contains("[omitted: large base64-like payload]"));
        assert!(!rendered.contains("AAAAA"));
    }

    #[test]
    fn scrub_string_empty() {
        assert!(scrub_string_for_summary("").is_empty());
    }

    #[test]
    fn strip_analysis_removes_thinking() {
        let input = "<analysis>thinking here</analysis>\n<summary>the real content</summary>";
        assert_eq!(strip_analysis_block(input), "the real content");
    }

    #[test]
    fn strip_analysis_fallback_no_tags() {
        assert_eq!(strip_analysis_block("just plain text"), "just plain text");
    }

    #[test]
    fn strip_analysis_only_removes_analysis() {
        let input = "<analysis>drop this</analysis>\nkeep this part";
        assert_eq!(strip_analysis_block(input), "keep this part");
    }

    #[test]
    fn format_structured_summary_preserves_sections() {
        let input = "<analysis>x</analysis><summary>### Primary Request\nDo stuff\n### Pending Tasks\nMore\n### Current Work\nEditing foo.rs\n### Current State\nDone</summary>";
        let result = format_structured_summary(input);
        assert!(result.contains("### Primary Request"));
        assert!(result.contains("### Pending Tasks"));
        assert!(result.contains("### Current Work"));
        assert!(result.contains("### Current State"));
        assert!(!result.contains("[compact warning"));
    }

    #[test]
    fn format_structured_summary_warns_on_missing_sections() {
        let input = "<summary>### Primary Request\nDo stuff</summary>";
        let result = format_structured_summary(input);
        assert!(result.contains("[compact warning"));
        assert!(result.contains("### Pending Tasks"));
        assert!(result.contains("### Current Work"));
    }

    #[test]
    fn compact_system_prompt_has_analysis_instruction() {
        assert!(COMPACT_SYSTEM_PROMPT.contains("<analysis>"));
        assert!(COMPACT_SYSTEM_PROMPT.contains("<summary>"));
    }

    #[test]
    fn compact_system_prompt_has_nine_sections() {
        let sections = [
            "### Primary Request",
            "### Key Technical Concepts",
            "### Files & Code Modified",
            "### Problem Solving",
            "### Errors & Fixes",
            "### All User Messages",
            "### Pending Tasks",
            "### Current Work",
            "### Current State",
        ];
        for s in &sections {
            assert!(COMPACT_SYSTEM_PROMPT.contains(s), "missing section: {s}");
        }
    }

    #[test]
    fn format_structured_summary_all_sections_missing() {
        let input = "Just some plain text with no section headers at all.";
        let result = format_structured_summary(input);
        assert!(result.contains("[compact warning: missing sections:"));
        assert!(result.contains("### Primary Request"));
        assert!(result.contains("### Pending Tasks"));
        assert!(result.contains("### Current Work"));
        assert!(result.contains("### Current State"));
        assert!(result.contains("Just some plain text"));
    }

    #[test]
    fn format_structured_summary_partial_empty_sections() {
        let input = "### Primary Request\n\n### Current State\n";
        let result = format_structured_summary(input);
        // Missing Pending Tasks and Current Work
        assert!(result.contains("[compact warning: missing sections:"));
        assert!(result.contains("### Pending Tasks"));
        assert!(result.contains("### Current Work"));
        // The present (even if empty) sections are preserved
        assert!(result.contains("### Primary Request"));
        assert!(result.contains("### Current State"));
    }

    #[test]
    fn render_messages_for_summary_empty_input() {
        let rendered = render_messages_for_summary(&[]);
        assert!(rendered.is_empty());
    }

    #[test]
    fn render_messages_tool_result_null_content() {
        let msgs = vec![json!({"role": "tool", "content": null, "tool_call_id": "c1"})];
        // Should not panic
        let rendered = render_messages_for_summary(&msgs);
        assert!(rendered.contains("[TOOL RESULT]"));
    }

    #[test]
    fn strip_analysis_nested_tags() {
        // Nested analysis tags: the while-loop strips the first <analysis>…</analysis>
        // pair (inner close), leaving the outer close tag as a remnant.
        // This documents current behaviour — no panic, outer content preserved.
        let input = "<analysis>outer<analysis>inner</analysis>more</analysis>rest";
        let result = strip_analysis_block(input);
        assert!(
            !result.contains("<analysis>"),
            "opening tags should be removed"
        );
        // The trailing </analysis> remains because there is no matching open tag
        assert!(result.contains("rest"));
        assert!(result.contains("more"));
    }
}
