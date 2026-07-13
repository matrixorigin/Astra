//! Session continuation helpers: load previous conversation from the durable
//! conversation projection or journal, then strip runtime-injected scaffolding.
//!
//! Used by one-shot mode (`-m "..." --session-id <id>`) to provide multi-turn continuity.

use serde_json::{Value, json};

/// Load prompt-facing continuation from canonical local session state.
/// Used by one-shot mode (`-m "..." --session-id <id>`) to provide
/// conversation history that the model needs for multi-turn continuity.
///
/// CSL preserves full canonical runtime history. The primary session journal
/// is the durable source of completed turns and provides a fresh
/// prompt-facing fallback while a CSL projection is delayed or unavailable.
/// A heavy checkpoint is the final recovery fallback. The TUI transcript is a
/// display projection and is deliberately never used as model history.
pub(crate) fn load_session_messages_for_continuation(session_id: &str) -> Option<Vec<Value>> {
    match load_csl_messages_for_continuation(session_id) {
        Ok(Some(messages)) => return Some(messages),
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(
                session_id = %session_id,
                error = %error,
                "failed to read CSL continuation projection; falling back to durable journal"
            );
        }
    }

    match load_journal_messages_for_continuation(session_id) {
        Ok(Some(messages)) => return Some(messages),
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(
                session_id = %session_id,
                error = %error,
                "failed to read journal continuation fallback; falling back to heavy checkpoint"
            );
        }
    }

    let user_id = crate::cli::cli_config::cli_utils::cli_user_id();
    match astra_pipeline::step_checkpoint::read_latest_heavy_checkpoint(&user_id, session_id) {
        Ok(Some(cp)) if !cp.messages.is_empty() => {
            let prompt_state = heavy_checkpoint_prompt_state(&cp);
            let messages =
                astra_turn_core::prompt_facing::sanitize_prompt_facing_messages_with_state(
                    cp.messages,
                    &prompt_state,
                );
            if messages.is_empty() {
                tracing::warn!(
                    user_id = %user_id,
                    session_id = %session_id,
                    "continuation checkpoint sanitized to no prompt-facing messages; falling back to transcript"
                );
                None
            } else {
                Some(messages)
            }
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
        _ => None,
    }
}

/// Rebuild a prompt-facing history from completed durable journal turns.
///
/// The journal intentionally records only real user input and assistant
/// output, so this fallback cannot smuggle runtime scaffolding, tool payloads
/// or UI transcript artifacts into the next prompt.
fn load_journal_messages_for_continuation(session_id: &str) -> Result<Option<Vec<Value>>, String> {
    let restored = crate::cli::session::session_runtime::restored_journal_state(session_id)?;
    if !restored.exists {
        return Ok(None);
    }

    let messages = restored
        .session
        .history
        .into_iter()
        .flat_map(|(user, assistant)| {
            let mut turn = Vec::with_capacity(2);
            if !user.trim().is_empty() {
                turn.push(json!({"role": "user", "content": user}));
            }
            if !assistant.trim().is_empty() {
                turn.push(json!({"role": "assistant", "content": assistant}));
            }
            turn
        })
        .collect::<Vec<_>>();
    Ok((!messages.is_empty()).then_some(messages))
}

fn load_csl_messages_for_continuation(session_id: &str) -> Result<Option<Vec<Value>>, String> {
    let store = astra_turn_core::conversation_log::file_store::FileCslStore::new(
        crate::cli::session::session_recovery::io::csl_store_base_dir(),
    );
    let materialized = store
        .load_materialized_blocking(session_id)
        .map_err(|error| error.to_string())?;
    let Some(materialized) = materialized else {
        return Ok(None);
    };
    let messages = astra_turn_core::prompt_facing::sanitize_prompt_facing_messages_with_state(
        materialized.messages,
        &materialized.session_state,
    );
    Ok((!messages.is_empty()).then_some(messages))
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
pub(crate) fn sanitize_continuation_messages(mut msgs: Vec<Value>) -> Vec<Value> {
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

/// Reconstruct CLI `(user, assistant)` history pairs from OpenAI-style messages.
///
/// Rules:
/// - ignore tool/system messages,
/// - ignore assistant tool-call stubs that have no visible text,
/// - concatenate multiple visible assistant chunks in the same turn.
pub(crate) fn history_pairs_from_messages(msgs: &[Value]) -> Vec<(String, String)> {
    let visible_msgs =
        astra_turn_core::prompt_facing::sanitize_user_visible_messages(msgs.to_vec());
    let mut pairs = Vec::new();
    let mut current_user = String::new();
    let mut current_assistant = String::new();

    for msg in &visible_msgs {
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
    use astra_pipeline::step_protocol::{ExecutionCursor, StepCheckpoint};
    use astra_services::session_journal;
    use serde_json::json;

    #[test]
    #[serial_test::serial]
    fn load_session_messages_uses_durable_journal_when_csl_is_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let _journal_dir = session_journal::JournalDirGuard::new(temp.path());
        let session_id = format!("journal-continuation-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&session_id).unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&session_id),
                1,
                Some("test-model"),
                "keep the result",
                "the result is durable",
                0,
                5,
                3,
                10,
            ))
            .unwrap();

        let messages = super::load_session_messages_for_continuation(&session_id)
            .expect("journal turn should provide continuation while CSL is absent");

        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[0],
            json!({"role": "user", "content": "keep the result"})
        );
        assert_eq!(
            messages[1],
            json!({"role": "assistant", "content": "the result is durable"})
        );
    }

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
    fn load_session_messages_uses_csl_when_checkpoint_errors() {
        let session_id = format!("test-session-cont-corrupt-{}", uuid::Uuid::new_v4());
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
            json!({"role": "user", "content": "checkpoint history"}),
            json!({"role": "assistant", "content": "checkpoint answer"}),
        ];
        let user_id = crate::cli::cli_config::cli_utils::cli_user_id();
        let path = astra_pipeline::step_checkpoint::write_step_checkpoint(
            &user_id,
            &session_id,
            9,
            &checkpoint,
        )
        .unwrap();
        let encoded = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            encoded.replacen(
                &format!(r#""user_id":"{user_id}""#),
                r#""user_id":"wrong-owner""#,
                1,
            ),
        )
        .unwrap();

        crate::cli::session::session_recovery::csl::write_full_csl_snapshot_atomic(
            &session_id,
            9,
            &[
                json!({"role": "user", "content": "canonical question"}),
                json!({"role": "assistant", "content": "canonical answer"}),
            ],
            &astra_turn_core::conversation_log::SessionStateCompact::default(),
        )
        .unwrap();

        let messages = super::load_session_messages_for_continuation(&session_id)
            .expect("canonical CSL should not depend on checkpoint validity");

        let _ = std::fs::remove_dir_all(
            astra_pipeline::step_checkpoint::owner_session_dir_for(&user_id, &session_id).unwrap(),
        );

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["content"], "canonical question");
        assert_eq!(messages[1]["content"], "canonical answer");
    }

    #[test]
    fn load_session_messages_prefers_csl_over_valid_checkpoint() {
        let session_id = format!("test-session-csl-priority-{}", uuid::Uuid::new_v4());
        let user_id = crate::cli::cli_config::cli_utils::cli_user_id();
        let checkpoint = StepCheckpoint::heavy(
            "s1".to_string(),
            "t1".to_string(),
            "astra-cli".to_string(),
            ExecutionCursor::default(),
        );
        let mut checkpoint = checkpoint;
        let StepCheckpoint::Heavy(heavy) = &mut checkpoint else {
            unreachable!("StepCheckpoint::heavy must create a heavy checkpoint");
        };
        heavy.messages = vec![json!({"role": "user", "content": "older checkpoint"})];
        astra_pipeline::step_checkpoint::write_step_checkpoint(
            &user_id,
            &session_id,
            1,
            &checkpoint,
        )
        .unwrap();
        crate::cli::session::session_recovery::csl::write_full_csl_snapshot_atomic(
            &session_id,
            2,
            &[
                json!({"role": "user", "content": "canonical current"}),
                json!({"role": "assistant", "content": "current answer"}),
            ],
            &astra_turn_core::conversation_log::SessionStateCompact::default(),
        )
        .unwrap();

        let messages = super::load_session_messages_for_continuation(&session_id).unwrap();
        let _ = std::fs::remove_dir_all(
            astra_pipeline::step_checkpoint::owner_session_dir_for(&user_id, &session_id).unwrap(),
        );

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["content"], "canonical current");
        assert_eq!(messages[1]["content"], "current answer");
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
    fn history_pairs_use_user_visible_projection_not_prompt_recaps() {
        let msgs = vec![
            json!({"role": "user", "content": "continue\u{0}"}),
            json!({"role": "system", "content": "[Runtime tool result]\nbash: binary-looking trace"}),
            json!({"role": "system", "content": "[Session runtime recap]\nRecent tools: bash"}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "assistant", "content": "\u{1b}[32mvisible answer\u{1b}[0m"}),
            json!({"role": "tool", "content": "raw tool payload"}),
        ];

        let pairs = super::history_pairs_from_messages(&msgs);

        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "continue");
        assert_eq!(pairs[0].1, "visible answer");
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
        assert_eq!(result.len(), 3);
        assert_eq!(result[2]["content"], "hi");
        assert!(result.iter().all(|message| {
            !message["content"]
                .as_str()
                .unwrap_or_default()
                .contains("[Runtime tool result]")
        }));
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
