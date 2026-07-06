//! Session continuation helpers: load previous conversation from heavy checkpoints,
//! strip runtime-injected scaffolding messages, and trim incomplete tool rounds.
//!
//! Used by one-shot mode (`-m "..." --session-id <id>`) to provide multi-turn continuity.

use crate::tui::turn_event::TurnEvent;
use serde_json::{Value, json};

/// Load conversation messages from a session's latest heavy checkpoint.
/// Used by one-shot mode (`-m "..." --session-id <id>`) to provide
/// conversation history that the model needs for multi-turn continuity.
///
/// Returns `None` if the session has no checkpoint (first turn) or
/// the checkpoint is unreadable.
pub(crate) fn load_session_messages_for_continuation(
    session_id: &str,
) -> Option<Vec<Value>> {
    let user_id = crate::cli::cli_config::cli_utils::cli_user_id();
    match astra_pipeline::step_checkpoint::read_latest_heavy_checkpoint(&user_id, session_id) {
        Ok(Some(cp)) if !cp.messages.is_empty() => {
            let prompt_state = heavy_checkpoint_prompt_state(&cp);
            Some(
                astra_turn_core::prompt_facing::sanitize_prompt_facing_messages_with_state(
                    cp.messages,
                    &prompt_state,
                ),
            )
        }
        Err(error) => {
            tracing::warn!(
                user_id = %user_id,
                session_id = %session_id,
                error = %error,
                "failed to read continuation checkpoint"
            );
            None
        }
        _ => load_transcript_messages_for_continuation(session_id),
    }
}

fn heavy_checkpoint_prompt_state(
    cp: &astra_pipeline::step_protocol::HeavyCheckpoint,
) -> astra_turn_core::conversation_log::SessionStateCompact {
    astra_turn_core::conversation_log::SessionStateCompact {
        recent_tools: cp.recent_tools.clone(),
        budget_remaining_tokens: cp.budget_remaining_tokens,
        budget_remaining_rounds: cp.budget_remaining_rounds,
        consecutive_ctx_errors: cp.consecutive_context_window_errors,
        delegation: cp.delegation_id.as_ref().map(|id| {
            astra_turn_core::conversation_log::DelegationCompact {
                id: id.clone(),
                pattern: cp.delegation_pattern.clone().unwrap_or_default(),
                completed_sub_runs: cp.delegation_sub_run_summaries.clone(),
            }
        }),
        ..Default::default()
    }
}

/// Strip runtime-injected scaffolding messages that must not persist across
/// turn boundaries. Without this, harness nudges (injected as "user" role)
/// bias the model toward tool usage on the next turn even when the user's
/// new message is purely conversational.
pub(crate) fn sanitize_continuation_messages(
    mut msgs: Vec<Value>,
) -> Vec<Value> {
    msgs = astra_turn_core::prompt_facing::sanitize_prompt_facing_messages(msgs);
    msgs.retain(|m| {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let content_text = extract_text_content(m);
        let content = content_text.as_deref().unwrap_or("");
        !astra_turn_core::runtime_scaffolding::is_continuation_scaffolding_for_role(role, content)
    });
    msgs
}

/// Extract text content from a message regardless of format.
/// Handles both string content and array-format content blocks.
pub(crate) fn extract_text_content(msg: &Value) -> Option<String> {
    astra_turn_core::prompt_facing::extract_text_content(msg)
}

pub(crate) fn load_transcript_messages_for_continuation(session_id: &str) -> Option<Vec<Value>> {
    let events = crate::tui::transcript_jsonl::load(session_id);
    let messages = transcript_events_to_messages(&events);
    if messages.is_empty() {
        None
    } else {
        Some(sanitize_continuation_messages(messages))
    }
}

pub(crate) fn transcript_history_pairs_for_session(session_id: &str) -> Vec<(String, String)> {
    load_transcript_messages_for_continuation(session_id)
        .map(|messages| history_pairs_from_messages(&messages))
        .unwrap_or_default()
}

fn transcript_events_to_messages(events: &[TurnEvent]) -> Vec<Value> {
    events.iter().filter_map(transcript_event_to_message).collect()
}

fn transcript_event_to_message(event: &TurnEvent) -> Option<Value> {
    match event {
        TurnEvent::User { text, .. } => non_empty_message("user", text),
        TurnEvent::Assistant { markdown, .. } => non_empty_message("assistant", markdown),
        TurnEvent::System { text, .. } => non_empty_message("system", text),
        TurnEvent::Tool {
            name,
            description,
            output_summary,
            output,
            ..
        } => {
            let detail = output
                .as_deref()
                .or(output_summary.as_deref())
                .unwrap_or_default()
                .trim();
            let description = description.trim();
            let body = match (description.is_empty(), detail.is_empty()) {
                (true, true) => "completed".to_string(),
                (false, true) => description.to_string(),
                (true, false) => detail.to_string(),
                (false, false) => format!("{description} | {detail}"),
            };
            Some(json!({
                "role": "system",
                "content": format!("[Runtime tool result]\n{}: {}", name.trim(), body),
            }))
        }
        TurnEvent::Thinking { .. } | TurnEvent::TurnSummary { .. } => None,
    }
}

fn non_empty_message(role: &str, text: &str) -> Option<Value> {
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(json!({"role": role, "content": text}))
    }
}

/// Reconstruct CLI `(user, assistant)` history pairs from OpenAI-style messages.
///
/// Rules:
/// - ignore tool/system messages,
/// - ignore assistant tool-call stubs that have no visible text,
/// - concatenate multiple visible assistant chunks in the same turn.
pub(crate) fn history_pairs_from_messages(msgs: &[Value]) -> Vec<(String, String)> {
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
                if current_user.is_empty() {
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
#[cfg(test)]
mod tests {
    use crate::tui::turn_event::{ToolStatus, TurnEvent};
    use astra_pipeline::step_protocol::{ExecutionCursor, StepCheckpoint};
    use serde_json::json;

    #[test]
    fn load_session_messages_returns_checkpoint_messages() {
        let session_id = format!("test-session-cont-{}", uuid::Uuid::new_v4());
        let mut checkpoint = StepCheckpoint::heavy(
            "s1".to_string(),
            "t1".to_string(),
            "astra-cli".to_string(),
            ExecutionCursor::default(),
        );
        let StepCheckpoint::Heavy(heavy) = &mut checkpoint else {
            unreachable!("StepCheckpoint::heavy must create a heavy checkpoint");
        };
        heavy.messages = vec![
            json!({"role": "user", "content": "Remember: code is ZEBRA-99"}),
            json!({"role": "assistant", "content": "OK, noted."}),
        ];
        heavy.budget_remaining_tokens = 100000;
        heavy.budget_remaining_rounds = 50;
        let user_id = crate::cli::cli_config::cli_utils::cli_user_id();
        astra_pipeline::step_checkpoint::write_step_checkpoint(
            &user_id,
            &session_id,
            2,
            &checkpoint,
        )
        .unwrap();

        let messages = super::load_session_messages_for_continuation(&session_id);

        let _ = std::fs::remove_dir_all(
            astra_pipeline::step_checkpoint::owner_session_dir_for(&user_id, &session_id).unwrap(),
        );

        let messages = messages.expect("should load messages from checkpoint");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "Remember: code is ZEBRA-99");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"], "OK, noted.");
        assert_eq!(messages[2]["role"], "system");
        assert!(
            messages[2]["content"]
                .as_str()
                .unwrap()
                .contains("Last checkpoint budget: tokens=100000, rounds=50")
        );
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
            json!({"role": "user", "content": "[session-resume:v1]\nHydrated previous session context"}),
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
    fn sanitize_compaction_boundary_drops_pre_boundary_stale_goal() {
        let msgs = vec![
            json!({"role": "user", "content": "3 agents 不同角度review这个分支的所有changes"}),
            json!({"role": "assistant", "content": "review summary"}),
            json!({"role": "system", "content": "[Context compacted: older messages were removed to reduce token pressure. The conversation continues below.]"}),
            json!({"role": "user", "content": "不要review啊！"}),
            json!({"role": "assistant", "reasoning_content": "Maybe continue the old review"}),
            json!({"role": "tool", "content": "No matches found", "tool_call_id": "c1"}),
            json!({"role": "assistant", "content": "明白，不做 review。"}),
        ];

        let result = super::sanitize_continuation_messages(msgs);

        assert_eq!(result.len(), 3);
        assert_eq!(result[0]["role"], "system");
        assert_eq!(result[1]["content"], "不要review啊！");
        assert_eq!(result[2]["content"], "明白，不做 review。");
        assert!(
            result
                .iter()
                .all(|msg| !msg["content"].as_str().unwrap_or("").contains("3 agents"))
        );
    }

    #[test]
    fn sanitize_strips_obsolete_active_task_attachment_garbage() {
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
    fn transcript_events_restore_prompt_facing_stage_history() {
        let events = vec![
            TurnEvent::User {
                ts: None,
                text: "review current branch".into(),
            },
            TurnEvent::Tool {
                ts: None,
                name: "git".into(),
                description: "diff --stat".into(),
                status: ToolStatus::Success,
                duration_ms: 10,
                output_summary: Some("202 files changed".into()),
                output: None,
            },
            TurnEvent::Assistant {
                ts: None,
                markdown: "I found a large runtime change set.".into(),
            },
        ];

        let messages = super::sanitize_continuation_messages(super::transcript_events_to_messages(
            &events,
        ));
        let pairs = super::history_pairs_from_messages(&messages);

        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "review current branch");
        assert!(
            messages
                .iter()
                .any(|msg| msg["content"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("[Runtime tool result]\ngit: diff --stat | 202 files changed"))
        );
        assert_eq!(pairs[0].1, "I found a large runtime change set.");
    }

    #[test]
    fn sanitize_compacts_trailing_completed_tool_round() {
        let msgs = vec![
            json!({"role": "user", "content": "check status"}),
            json!({"role": "assistant", "content": "Here is the status."}),
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "tool_calls": [{"id": "1", "type": "function", "function": {"name": "git", "arguments": "{\"action\":\"status\"}"}}]}),
            json!({"role": "tool", "content": "M file.rs", "tool_call_id": "1"}),
            json!({"role": "assistant", "tool_calls": [{"id": "2", "type": "function", "function": {"name": "git", "arguments": "{\"action\":\"diff\"}"}}]}),
            json!({"role": "tool", "content": "+line", "tool_call_id": "2"}),
        ];
        let result = super::sanitize_continuation_messages(msgs);
        assert_eq!(result.len(), 5);
        assert_eq!(result[2]["content"], "hi");
        assert!(
            result[3]["content"]
                .as_str()
                .unwrap()
                .contains("[Runtime tool result]\ngit: M file.rs")
        );
        assert!(
            result[4]["content"]
                .as_str()
                .unwrap()
                .contains("[Runtime tool result]\ngit: +line")
        );
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

        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["content"], "✓ Deployment finished successfully.");
    }

    #[test]
    fn history_pairs_drop_assistant_only_trace_and_preserve_structured_text() {
        let msgs = vec![
            json!({"role": "assistant", "content": "Earlier context compacted."}),
            json!({"role": "user", "content": "continue"}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "Sure."}]}),
            json!({"role": "assistant", "tool_calls": [{"id": "1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": "1", "content": "ok"}),
            json!({"role": "assistant", "content": [{"type": "output_text", "text": "Done."}]}),
        ];

        let pairs = super::history_pairs_from_messages(&msgs);

        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "continue");
        assert_eq!(pairs[0].1, "Sure.\n\nDone.");
    }
}
