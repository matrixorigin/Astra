//! Session continuation helpers: load previous conversation from heavy checkpoints,
//! strip runtime-injected scaffolding messages, and trim incomplete tool rounds.
//!
//! Used by one-shot mode (`-m "..." --session-id <id>`) to provide multi-turn continuity.

/// Load conversation messages from a session's latest heavy checkpoint.
/// Used by one-shot mode (`-m "..." --session-id <id>`) to provide
/// conversation history that the model needs for multi-turn continuity.
///
/// Returns `None` if the session has no checkpoint (first turn) or
/// the checkpoint is unreadable.
pub(crate) fn load_session_messages_for_continuation(
    session_id: &str,
) -> Option<Vec<serde_json::Value>> {
    match astra_pipeline::step_checkpoint::read_latest_heavy_checkpoint(session_id) {
        Ok(Some(cp)) if !cp.messages.is_empty() => {
            Some(sanitize_continuation_messages(cp.messages))
        }
        Err(error) => {
            tracing::warn!(
                session_id = %session_id,
                error = %error,
                "failed to read continuation checkpoint"
            );
            None
        }
        _ => None,
    }
}

/// Strip runtime-injected scaffolding messages that must not persist across
/// turn boundaries. Without this, harness nudges (injected as "user" role)
/// bias the model toward tool usage on the next turn even when the user's
/// new message is purely conversational.
pub(crate) fn sanitize_continuation_messages(
    mut msgs: Vec<serde_json::Value>,
) -> Vec<serde_json::Value> {
    msgs.retain(|m| {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let content_text = extract_text_content(m);
        let content = content_text.as_deref().unwrap_or("");
        !astra_turn_core::runtime_scaffolding::is_continuation_scaffolding_for_role(role, content)
    });
    trim_trailing_incomplete_tool_round(&mut msgs);
    msgs
}

/// Extract text content from a message regardless of format.
/// Handles both string content and array-format content blocks.
pub(crate) fn extract_text_content(msg: &serde_json::Value) -> Option<String> {
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

/// Reconstruct CLI `(user, assistant)` history pairs from OpenAI-style messages.
///
/// Rules:
/// - preserve assistant-only context entries such as manual compaction summaries,
/// - ignore tool/system messages,
/// - ignore assistant tool-call stubs that have no visible text,
/// - concatenate multiple visible assistant chunks in the same turn.
pub(crate) fn history_pairs_from_messages(msgs: &[serde_json::Value]) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut current_user = String::new();
    let mut current_assistant = String::new();

    for msg in msgs {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let text = extract_text_content(msg).unwrap_or_default();
        match role {
            "user" => {
                if !current_user.is_empty() || !current_assistant.is_empty() {
                    pairs.push((
                        std::mem::take(&mut current_user),
                        std::mem::take(&mut current_assistant),
                    ));
                }
                if !text.trim().is_empty() {
                    current_user = text;
                }
            }
            "assistant" => {
                if text.trim().is_empty() {
                    continue;
                }
                if current_assistant.is_empty() {
                    current_assistant = text;
                } else {
                    current_assistant.push_str("\n\n");
                    current_assistant.push_str(&text);
                }
            }
            _ => {}
        }
    }

    if !current_user.is_empty() || !current_assistant.is_empty() {
        pairs.push((current_user, current_assistant));
    }

    pairs
}

/// If the conversation ends with an incomplete tool round (assistant tool_use
/// → tool results, but no final assistant text), trim back to the last
/// complete exchange. This prevents the model from continuing a stale tool
/// loop from the previous turn.
fn trim_trailing_incomplete_tool_round(msgs: &mut Vec<serde_json::Value>) {
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

fn has_tool_use_content(msg: &serde_json::Value) -> bool {
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn load_session_messages_returns_checkpoint_messages() {
        let session_id = format!("test-session-cont-{}", uuid::Uuid::new_v4());
        let home = dirs::home_dir().unwrap();
        let cp_dir = home
            .join(".astra/sessions")
            .join(&session_id)
            .join("step_checkpoints");
        std::fs::create_dir_all(&cp_dir).unwrap();

        let checkpoint_json = r#"{
            "Heavy": {
                "light": {
                    "protocol_version": 1,
                    "cursor": {"phase": "Done", "slots": [], "parallel": false, "wait_trigger": null, "sub_step": null},
                    "step_id": "s1",
                    "task_id": "t1",
                    "agent_id": "astra-cli",
                    "progress": 1.0,
                    "total_tokens": 100,
                    "created_at": 1700000000
                },
                "messages": [
                    {"role": "user", "content": "Remember: code is ZEBRA-99"},
                    {"role": "assistant", "content": "OK, noted."}
                ],
                "budget_remaining_tokens": 100000,
                "budget_remaining_rounds": 50,
                "blocked_tools": [],
                "recent_tools": []
            }
        }"#;
        std::fs::write(cp_dir.join("000002-heavy.json"), checkpoint_json).unwrap();

        let messages = super::load_session_messages_for_continuation(&session_id);

        let home = dirs::home_dir().unwrap();
        let _ = std::fs::remove_dir_all(home.join(".astra/sessions").join(&session_id));

        let messages = messages.expect("should load messages from checkpoint");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "Remember: code is ZEBRA-99");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"], "OK, noted.");
    }

    #[test]
    fn load_session_messages_returns_none_for_missing_session() {
        let messages = super::load_session_messages_for_continuation("nonexistent-session-xyz-42");
        assert!(messages.is_none());
    }

    #[test]
    fn sanitize_strips_runtime_injected_messages() {
        let msgs = vec![
            json!({"role": "user", "content": "review code"}),
            json!({"role": "assistant", "content": "Here is the review..."}),
            json!({"role": "system", "content": "[working-set:v1]\ngoal: review code\npending_work: none"}),
            json!({"role": "user", "content": "\n\n## ⚠ Sequential Tool Calls Detected\nYour last 4 rounds..."}),
            json!({"role": "system", "content": "## Already Fetched (do NOT re-read)\nContext already fetched:\nGit: status"}),
            json!({"role": "system", "content": "## Cross-Session Project Context\nBelow are summaries..."}),
            json!({"role": "user", "content": "\n\n✓ Previous round: 2 tools executed in parallel — excellent."}),
            json!({"role": "user", "content": "[attention:v1]\ngoal: stale goal\ncurrent_todo: none"}),
            json!({"role": "user", "content": "[working-set:v1]\ngoal: stale\npending_work: none"}),
            json!({"role": "user", "content": "[session-anchor] Goal: stale. State: t1."}),
            json!({"role": "system", "content": "[attention:v1]\ngoal: stale system-role manifest"}),
            json!({"role": "system", "content": "[session-anchor] Goal: stale. State: t1."}),
            json!({"role": "user", "content": "你好"}),
        ];
        let result = super::sanitize_continuation_messages(msgs);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0]["content"], "review code");
        assert_eq!(result[1]["content"], "Here is the review...");
        assert_eq!(result[2]["content"], "你好");
    }

    #[test]
    fn sanitize_strips_active_task_attachment_wrappers() {
        let msgs = vec![
            json!({"role": "user", "content": "review code"}),
            json!({"role": "user", "content": "[Active task attachment]\nResume the active task/thread below unless the user explicitly changes topic.\n[User follow-up]\n继续"}),
            json!({"role": "assistant", "content": "Here is the review..."}),
        ];

        let result = super::sanitize_continuation_messages(msgs);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["content"], "review code");
        assert_eq!(result[1]["content"], "Here is the review...");
    }

    #[test]
    fn sanitize_strips_runtime_injected_array_format_messages() {
        let msgs = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "user", "content": [{"type": "text", "text": "[working-set:v1]\ngoal: stale"}]}),
            json!({"role": "system", "content": [{"type": "text", "text": "## Already Fetched\nContext already fetched"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "✓ Previous round: 2 tools executed in parallel — excellent."}]}),
            json!({"role": "assistant", "content": "still here"}),
        ];

        let result = super::sanitize_continuation_messages(msgs);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["content"], "hello");
        assert_eq!(result[1]["content"], "still here");
    }

    #[test]
    fn sanitize_trims_trailing_tool_round() {
        let msgs = vec![
            json!({"role": "user", "content": "check status"}),
            json!({"role": "assistant", "content": "Here is the status."}),
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "tool_calls": [{"id": "1", "type": "function", "function": {"name": "git_status", "arguments": "{}"}}]}),
            json!({"role": "tool", "content": "M file.rs", "tool_call_id": "1"}),
            json!({"role": "assistant", "tool_calls": [{"id": "2", "type": "function", "function": {"name": "git_diff", "arguments": "{}"}}]}),
            json!({"role": "tool", "content": "+line", "tool_call_id": "2"}),
        ];
        let result = super::sanitize_continuation_messages(msgs);
        assert_eq!(result.len(), 3);
        assert_eq!(result[2]["content"], "hi");
    }

    #[test]
    fn sanitize_preserves_complete_conversation() {
        let msgs = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "Hi! How can I help?"}),
            json!({"role": "user", "content": "thanks"}),
            json!({"role": "assistant", "content": "You're welcome!"}),
        ];
        let result = super::sanitize_continuation_messages(msgs.clone());
        assert_eq!(result.len(), 4);
    }

    #[test]
    fn sanitize_keeps_legitimate_system_checkmark_messages() {
        let msgs = vec![
            json!({"role": "system", "content": "✓ Deployment finished successfully."}),
            json!({"role": "assistant", "content": "done"}),
        ];

        let result = super::sanitize_continuation_messages(msgs);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["content"], "✓ Deployment finished successfully.");
    }

    #[test]
    fn history_pairs_preserve_assistant_only_summary_and_structured_text() {
        let msgs = vec![
            json!({"role": "assistant", "content": "Earlier context compacted."}),
            json!({"role": "user", "content": "continue"}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "Sure."}]}),
            json!({"role": "assistant", "tool_calls": [{"id": "1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": "1", "content": "ok"}),
            json!({"role": "assistant", "content": [{"type": "output_text", "text": "Done."}]}),
        ];

        let pairs = super::history_pairs_from_messages(&msgs);

        assert_eq!(pairs.len(), 2);
        assert_eq!(
            pairs[0],
            ("".to_string(), "Earlier context compacted.".to_string())
        );
        assert_eq!(pairs[1].0, "continue");
        assert_eq!(pairs[1].1, "Sure.\n\nDone.");
    }
}
