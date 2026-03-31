//! Compaction prompt templates.
//!
//! The prompt instructs the LLM to produce a dense summary of conversation
//! history that preserves task context, key decisions, and open questions —
//! while discarding verbatim tool outputs and redundant back-and-forth.

/// System prompt used when asking the LLM to summarize conversation history.
pub const COMPACT_SYSTEM_PROMPT: &str = "You are a conversation summarizer. Your task is to create a concise but complete summary of the conversation history provided.\n\nYour summary must preserve:\n- The user's original task/goal\n- Key decisions made and their rationale\n- Important facts discovered (file contents, search results, error messages)\n- The current state of any ongoing work\n- Open questions or blockers\n\nYour summary must omit:\n- Verbatim repetition of long tool outputs (paraphrase the key findings)\n- Conversational filler and acknowledgments\n- Superseded decisions or failed attempts (unless the failure is informative)\n\nFormat your summary with clear markdown sections covering: Task, Progress, Current State, and (if applicable) Open Questions. Be dense and factual. Assume the reader is an LLM that needs only the essentials.";

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

        let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");

        match role {
            "system" => {
                // Skip system messages — they'll be re-injected by the runtime
                continue;
            }
            "tool" => {
                let truncated = truncate_str(content, TOOL_RESULT_MAX_CHARS);
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
                        let args = tc
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(|a| a.as_str())
                            .unwrap_or("{}");
                        let args_short = truncate_str(args, 300);
                        out.push_str(&format!("[ASSISTANT calls {name}({args_short})]\n"));
                    }
                }
                if !content.is_empty() {
                    let truncated = truncate_str(content, MAX_CONTENT_CHARS);
                    out.push_str(&format!("[ASSISTANT]: {truncated}\n\n"));
                }
            }
            "user" => {
                // Skip attachment messages injected by the runtime
                if msg.get("attachment_metadata").is_some() {
                    continue;
                }
                let truncated = truncate_str(content, MAX_CONTENT_CHARS);
                out.push_str(&format!("[USER]: {truncated}\n\n"));
            }
            _ => {
                let truncated = truncate_str(content, MAX_CONTENT_CHARS);
                out.push_str(&format!("[{role}]: {truncated}\n\n"));
            }
        }
    }
    out
}

fn truncate_str(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max_chars).collect();
    format!("{truncated}...[truncated]")
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
        assert!(rendered.contains("[truncated]"));
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
    fn build_compact_user_prompt_wraps_conversation() {
        let prompt = build_compact_user_prompt("some conversation");
        assert!(prompt.contains("some conversation"));
        assert!(prompt.contains("summarize"));
    }
}
