//! Prompt-facing conversation message normalization.
//!
//! Runtime state may contain provider tool-call frames, tool outputs, cache
//! markers, reasoning-only assistant frames, and compaction boundaries. Those
//! are execution trace, not stable cross-turn chat history. Use this module at
//! session restore and CSL prompt-materialization boundaries.

use serde_json::{Value, json};

const MAX_PROMPT_FACING_MESSAGES: usize = 40;

pub fn extract_text_content(msg: &Value) -> Option<String> {
    if let Some(s) = msg.get("content").and_then(|c| c.as_str()) {
        return Some(s.to_string());
    }
    if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
        let texts: Vec<&str> = arr
            .iter()
            .filter_map(|block| {
                let kind = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match kind {
                    "text" | "output_text" => block
                        .get("text")
                        .or_else(|| block.get("content"))
                        .and_then(|t| t.as_str()),
                    _ => None,
                }
            })
            .collect();
        if !texts.is_empty() {
            return Some(texts.join("\n"));
        }
    }
    None
}

pub fn sanitize_prompt_facing_messages(messages: Vec<Value>) -> Vec<Value> {
    let mut out = Vec::new();
    let start = latest_compaction_boundary_start(&messages).unwrap_or(0);
    let mut has_user_context = false;

    for msg in messages.into_iter().skip(start) {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        if !matches!(role, "user" | "assistant" | "system") {
            continue;
        }
        if role == "assistant" && has_tool_use_content(&msg) {
            continue;
        }

        let Some(content) = extract_text_content(&msg) else {
            continue;
        };
        if content.trim().is_empty() {
            continue;
        }
        if crate::runtime_scaffolding::is_continuation_scaffolding_for_role(role, &content) {
            continue;
        }

        if role == "assistant" && !has_user_context {
            continue;
        }
        out.push(json!({
            "role": role,
            "content": content,
        }));
        if role == "user" {
            has_user_context = true;
        }
    }

    trim_trailing_incomplete_tool_round(&mut out);
    trim_to_recent_messages(out)
}

fn latest_compaction_boundary_start(messages: &[Value]) -> Option<usize> {
    messages.iter().rposition(|message| {
        extract_text_content(message)
            .map(|content| is_compaction_boundary_text(&content))
            .unwrap_or(false)
    })
}

fn is_compaction_boundary_text(content: &str) -> bool {
    let trimmed = content.trim_start();
    trimmed.starts_with("[Context compacted:")
        || trimmed.starts_with("[Conversation summary")
        || trimmed.starts_with("Context was compacted before this point.")
        || trimmed.starts_with("前文上下文已压缩")
}

fn has_tool_use_content(msg: &Value) -> bool {
    if msg
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .is_some_and(|a| !a.is_empty())
    {
        return true;
    }
    if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
        return content
            .iter()
            .any(|c| c.get("type").and_then(|t| t.as_str()) == Some("tool_use"));
    }
    false
}

fn trim_trailing_incomplete_tool_round(msgs: &mut Vec<Value>) {
    let mut cut_at = None;
    for i in (0..msgs.len()).rev() {
        let role = msgs[i].get("role").and_then(|r| r.as_str()).unwrap_or("");
        match role {
            "tool" => continue,
            "assistant" => {
                if has_tool_use_content(&msgs[i]) {
                    cut_at = Some(i);
                    continue;
                }
                break;
            }
            _ => break,
        }
    }
    if let Some(cut) = cut_at {
        msgs.truncate(cut);
    }
}

fn trim_to_recent_messages(mut messages: Vec<Value>) -> Vec<Value> {
    if messages.len() <= MAX_PROMPT_FACING_MESSAGES {
        return messages;
    }
    messages.drain(0..messages.len() - MAX_PROMPT_FACING_MESSAGES);
    messages
}

#[cfg(test)]
mod tests {
    use super::sanitize_prompt_facing_messages;
    use serde_json::json;

    #[test]
    fn drops_execution_trace_and_reasoning_only_messages() {
        let messages = vec![
            json!({"role": "user", "content": "fix it"}),
            json!({"role": "assistant", "reasoning_content": "I should inspect files"}),
            json!({"role": "assistant", "tool_calls": [{"id": "c1"}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "file"}),
            json!({"role": "assistant", "content": "done"}),
        ];

        let got = sanitize_prompt_facing_messages(messages);

        assert_eq!(
            got,
            vec![
                json!({"role": "user", "content": "fix it"}),
                json!({"role": "assistant", "content": "done"}),
            ]
        );
    }

    #[test]
    fn drops_assistant_only_visible_text_without_user_context() {
        let messages = vec![
            json!({"role": "assistant", "content": "Earlier context compacted."}),
            json!({"role": "system", "content": "status note"}),
            json!({"role": "assistant", "content": "orphan answer"}),
            json!({"role": "user", "content": "continue"}),
            json!({"role": "assistant", "content": "ok"}),
        ];

        let got = sanitize_prompt_facing_messages(messages);

        assert_eq!(
            got,
            vec![
                json!({"role": "system", "content": "status note"}),
                json!({"role": "user", "content": "continue"}),
                json!({"role": "assistant", "content": "ok"}),
            ]
        );
    }

    #[test]
    fn compaction_boundary_replaces_older_goal_stack() {
        let messages = vec![
            json!({"role": "user", "content": "3 agents review everything"}),
            json!({"role": "assistant", "content": "review summary"}),
            json!({"role": "system", "content": "[Context compacted: older messages were removed to reduce token pressure. The conversation continues below.]"}),
            json!({"role": "user", "content": "不要review啊！"}),
            json!({"role": "assistant", "reasoning_content": "Maybe review anyway"}),
            json!({"role": "assistant", "content": "明白，不做 review。"}),
        ];

        let got = sanitize_prompt_facing_messages(messages);

        assert_eq!(got.len(), 3);
        assert_eq!(got[0]["role"], "system");
        assert!(
            got[0]["content"]
                .as_str()
                .unwrap()
                .contains("Context compacted")
        );
        assert_eq!(got[1]["content"], "不要review啊！");
        assert_eq!(got[2]["content"], "明白，不做 review。");
        assert!(
            got.iter()
                .all(|msg| !msg["content"].as_str().unwrap_or("").contains("3 agents"))
        );
    }
}
