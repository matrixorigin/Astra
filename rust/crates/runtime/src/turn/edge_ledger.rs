//! §5.5 edge callback ledger — single source of truth for keys and consume semantics.
//!
//! HTTP handlers insert; [`super::bridge_inprocess::InProcessChatTurnBridge`] removes entries
//! (poll + take) so each callback is delivered at most once.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use uuid::Uuid;

/// Cap in-memory map size; evicted wholesale when exceeded (handlers + design tradeoff).
pub const LEDGER_MAX_ENTRIES: usize = 4096;

pub const DEFAULT_POLL_INTERVAL_MS: u64 = 50;

pub const MSG_TOOL_LEDGER_TIMEOUT: &str =
    "timed out waiting for edge POST /tools/result (§5.5 ledger)";

/// Returns `true` if any assistant message in `messages` carries a
/// `reasoning_content` field, indicating a thinking-enabled model session.
/// When true, **all** subsequent assistant messages must include the field
/// (even as an empty string) to satisfy the LLM API contract.
pub fn history_has_reasoning(messages: &[Value]) -> bool {
    messages.iter().any(|m| {
        m.get("role").and_then(Value::as_str) == Some("assistant")
            && m.get("reasoning_content").is_some()
    })
}

#[inline]
pub fn tool_callback_key(user_id: &str, request_id: &str) -> String {
    format!("{user_id}:tool:{request_id}")
}

#[inline]
pub fn approval_callback_key(user_id: &str, request_id: &str) -> String {
    format!("{user_id}:approval:{request_id}")
}

/// Remove and return the value for `key`, waiting up to `timeout` (50ms polling).
pub async fn take_ledger_entry(
    ledger: &Arc<tokio::sync::Mutex<HashMap<String, Value>>>,
    key: &str,
    timeout: Duration,
) -> Option<Value> {
    let poll = Duration::from_millis(DEFAULT_POLL_INTERVAL_MS);
    let started = Instant::now();
    loop {
        {
            let mut g = ledger.lock().await;
            if let Some(v) = g.remove(key) {
                return Some(v);
            }
        }
        if started.elapsed() >= timeout {
            return None;
        }
        tokio::time::sleep(poll).await;
    }
}

/// Fill missing or empty `id` on each tool call so SSE + `POST /tools/result` agree.
pub fn ensure_tool_call_ids(tool_calls: &mut [Value]) {
    for tc in tool_calls.iter_mut() {
        let Some(obj) = tc.as_object_mut() else {
            continue;
        };
        let id_empty = obj
            .get("id")
            .map(|v| v.as_str().map(|s| s.is_empty()).unwrap_or(true))
            .unwrap_or(true);
        if id_empty {
            obj.insert("id".to_string(), Value::String(Uuid::now_v7().to_string()));
        }
    }
}

pub fn tool_content_from_ledger_entry(entry: &Value) -> String {
    let body = entry.get("body").unwrap_or(entry);
    let status = body
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let output = body.get("output").and_then(Value::as_str).unwrap_or("");
    if output.is_empty() {
        serde_json::to_string(&json!({"status": status})).unwrap_or_else(|_| status.to_string())
    } else if matches!(status, "ok" | "success" | "completed") {
        output.to_string()
    } else {
        format!("status={status}\n{output}")
    }
}

pub fn persist_value_for_ledger_tool_result(
    tc: &Value,
    ledger_entry: Option<&Value>,
    timed_out: bool,
) -> Value {
    let id = tc.get("id").and_then(Value::as_str).unwrap_or("");
    let name = tc
        .get("function")
        .and_then(Value::as_object)
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let result = if timed_out {
        MSG_TOOL_LEDGER_TIMEOUT.to_string()
    } else if let Some(e) = ledger_entry {
        tool_content_from_ledger_entry(e)
    } else {
        "missing tool_call id".to_string()
    };
    json!({
        "tool_call_id": id,
        "name": name,
        "result": result,
    })
}

pub fn assistant_message_with_tool_calls(tool_calls: &[Value]) -> Value {
    assistant_message_with_tool_calls_and_reasoning(tool_calls, "", false)
}

/// Build an assistant message with tool_calls, optionally including `reasoning_content`.
///
/// When `force_reasoning_field` is true, the field is always present (empty string if
/// `reasoning_content` is blank).  Thinking-enabled models (Claude extended thinking,
/// Kimi-k2.5, DeepSeek-R1, …) require `reasoning_content` on **every** assistant
/// message once thinking mode is active — even when the model produced no reasoning
/// for a particular turn.
pub fn assistant_message_with_tool_calls_and_reasoning(
    tool_calls: &[Value],
    reasoning_content: &str,
    force_reasoning_field: bool,
) -> Value {
    let mut msg = json!({
        "role": "assistant",
        "content": Value::Null,
        "tool_calls": tool_calls,
    });
    if !reasoning_content.is_empty() {
        msg["reasoning_content"] = Value::String(reasoning_content.to_string());
    } else if force_reasoning_field {
        msg["reasoning_content"] = Value::String(String::new());
    }
    msg
}

/// Strip `reasoning_content` values from older assistant messages to reduce token usage.
///
/// Thinking-model sessions accumulate large reasoning chains on every assistant message.
/// Since the LLM gains no benefit from re-reading old reasoning, we clear the value
/// (replace with empty string) on all assistant messages **except** the last one.
/// The field is kept (as empty string) so thinking-model API contracts are satisfied.
///
/// **Only affects the in-flight messages array** — heavy checkpoints and persisted events
/// retain the full reasoning for debugging and audit.
pub fn strip_stale_reasoning(messages: &mut [Value]) {
    // Find index of the last assistant message that has non-empty reasoning.
    let last_reasoning_idx = messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, m)| {
            m.get("role").and_then(Value::as_str) == Some("assistant")
                && m.get("reasoning_content")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty())
        })
        .map(|(i, _)| i);

    let Some(last_idx) = last_reasoning_idx else {
        return; // No reasoning in history — nothing to strip.
    };

    // Thinking is active — every assistant message with tool_calls must have
    // `reasoning_content` (even empty string) or providers like Kimi return 400.
    for (i, msg) in messages.iter_mut().enumerate() {
        if msg.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if i < last_idx {
            if msg
                .get("reasoning_content")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
            {
                // Replace with empty string (keep field for API compat).
                msg["reasoning_content"] = Value::String(String::new());
            } else if msg.get("reasoning_content").is_none() && msg.get("tool_calls").is_some() {
                // Assistant tool_call message from before thinking was enabled —
                // add the field so the provider doesn't reject it.
                msg["reasoning_content"] = Value::String(String::new());
            }
        } else if msg.get("reasoning_content").is_none() && msg.get("tool_calls").is_some() {
            // Even at or after last_idx, ensure the field exists on tool_call messages.
            msg["reasoning_content"] = Value::String(String::new());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{RecoveredEventRow, append_recovered_events};

    #[test]
    fn callback_keys_match_handler_convention() {
        assert_eq!(tool_callback_key("u42", "r7"), "u42:tool:r7");
        assert_eq!(approval_callback_key("u42", "a1"), "u42:approval:a1");
    }

    #[test]
    fn ensure_tool_call_ids_fills_empty_and_skips_nonempty() {
        let mut calls = vec![
            json!({"id": "", "function": {"name": "x", "arguments": "{}"}}),
            json!({"id": "keep-me", "function": {"name": "y", "arguments": "{}"}}),
        ];
        ensure_tool_call_ids(&mut calls);
        let id0 = calls[0].get("id").and_then(Value::as_str).unwrap();
        assert!(!id0.is_empty());
        assert_eq!(calls[1].get("id").and_then(Value::as_str), Some("keep-me"));
    }

    #[test]
    fn tool_content_prefers_output_on_success_status() {
        let entry = json!({
            "kind": "tool_result",
            "body": {"status": "ok", "output": "hello"}
        });
        assert_eq!(tool_content_from_ledger_entry(&entry), "hello");
    }

    #[test]
    fn tool_content_wraps_non_success_with_status() {
        let entry = json!({
            "body": {"status": "error", "output": "boom"}
        });
        assert_eq!(tool_content_from_ledger_entry(&entry), "status=error\nboom");
    }

    #[test]
    fn tool_content_reads_handler_shaped_wrapper() {
        let entry = json!({
            "kind": "tool_result",
            "user_id": "u1",
            "edge_id": "e1",
            "body": {"request_id": "c1", "status": "ok", "output": "done"}
        });
        assert_eq!(tool_content_from_ledger_entry(&entry), "done");
    }

    #[test]
    fn persist_value_matches_timeout_constant() {
        let tc = json!({"id": "i", "function": {"name": "n", "arguments": "{}"}});
        let v = persist_value_for_ledger_tool_result(&tc, None, true);
        assert_eq!(v["result"].as_str().unwrap(), MSG_TOOL_LEDGER_TIMEOUT);
    }

    #[tokio::test]
    async fn take_ledger_entry_immediate_remove() {
        let ledger = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let key = tool_callback_key("u", "1");
        ledger.lock().await.insert(key.clone(), json!({"k": 1}));
        let got = take_ledger_entry(&ledger, &key, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(got["k"], 1);
        let again = take_ledger_entry(&ledger, &key, Duration::from_millis(80)).await;
        assert!(again.is_none());
    }

    #[tokio::test]
    async fn take_ledger_entry_waits_for_late_insert() {
        let ledger = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let key = tool_callback_key("u", "late");
        let l2 = ledger.clone();
        let k2 = key.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            l2.lock().await.insert(k2, json!("ready"));
        });
        let got = take_ledger_entry(&ledger, &key, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(got, json!("ready"));
    }

    #[tokio::test]
    async fn take_ledger_entry_times_out_when_never_inserted() {
        let ledger = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let key = tool_callback_key("u", "missing");
        let started = Instant::now();
        let got = take_ledger_entry(&ledger, &key, Duration::from_millis(60)).await;
        assert!(got.is_none());
        assert!(started.elapsed() >= Duration::from_millis(50));
    }

    #[test]
    fn assistant_message_with_reasoning_includes_field_when_non_empty() {
        let tc =
            vec![json!({"id":"c1","type":"function","function":{"name":"bash","arguments":"{}"}})];
        let msg = assistant_message_with_tool_calls_and_reasoning(&tc, "I should run bash", false);
        assert_eq!(msg["role"], "assistant");
        assert_eq!(
            msg["reasoning_content"].as_str(),
            Some("I should run bash"),
            "reasoning_content must be present for thinking models"
        );
        assert!(msg["tool_calls"].as_array().unwrap().len() == 1);
    }

    #[test]
    fn assistant_message_with_reasoning_omits_field_when_empty() {
        let tc =
            vec![json!({"id":"c1","type":"function","function":{"name":"bash","arguments":"{}"}})];
        let msg = assistant_message_with_tool_calls_and_reasoning(&tc, "", false);
        assert!(
            msg.get("reasoning_content").is_none(),
            "reasoning_content must NOT be present for non-thinking models"
        );
    }

    #[test]
    fn assistant_message_force_reasoning_includes_empty_string() {
        let tc =
            vec![json!({"id":"c1","type":"function","function":{"name":"bash","arguments":"{}"}})];
        let msg = assistant_message_with_tool_calls_and_reasoning(&tc, "", true);
        assert_eq!(
            msg["reasoning_content"].as_str(),
            Some(""),
            "force_reasoning_field must include empty string for thinking models"
        );
    }

    #[test]
    fn assistant_message_without_reasoning_backward_compat() {
        let tc =
            vec![json!({"id":"c1","type":"function","function":{"name":"bash","arguments":"{}"}})];
        let msg = assistant_message_with_tool_calls(&tc);
        assert!(msg.get("reasoning_content").is_none());
        assert_eq!(msg["role"], "assistant");
    }

    #[test]
    fn history_has_reasoning_detects_thinking_session() {
        let messages = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "hello", "reasoning_content": "thinking..."}),
        ];
        assert!(history_has_reasoning(&messages));
    }

    #[test]
    fn history_has_reasoning_false_for_non_thinking_session() {
        let messages = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "hello"}),
        ];
        assert!(!history_has_reasoning(&messages));
    }

    // ── strip_stale_reasoning tests ──────────────────────────────────────

    #[test]
    fn strip_stale_reasoning_clears_old_keeps_latest() {
        let mut msgs = vec![
            json!({"role": "user", "content": "q1"}),
            json!({"role": "assistant", "content": null, "reasoning_content": "think1", "tool_calls": []}),
            json!({"role": "tool", "tool_call_id": "t1", "content": "r1"}),
            json!({"role": "user", "content": "q2"}),
            json!({"role": "assistant", "content": null, "reasoning_content": "think2", "tool_calls": []}),
            json!({"role": "tool", "tool_call_id": "t2", "content": "r2"}),
        ];
        strip_stale_reasoning(&mut msgs);
        // First assistant: reasoning cleared to empty
        assert_eq!(msgs[1]["reasoning_content"].as_str(), Some(""));
        // Second assistant: reasoning preserved
        assert_eq!(msgs[4]["reasoning_content"].as_str(), Some("think2"));
    }

    #[test]
    fn strip_stale_reasoning_noop_without_reasoning() {
        let mut msgs = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "hello"}),
        ];
        let original = msgs.clone();
        strip_stale_reasoning(&mut msgs);
        assert_eq!(msgs, original);
    }

    #[test]
    fn strip_stale_reasoning_noop_single_reasoning() {
        let mut msgs = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "reasoning_content": "deep thought", "content": "42"}),
        ];
        strip_stale_reasoning(&mut msgs);
        assert_eq!(msgs[1]["reasoning_content"].as_str(), Some("deep thought"));
    }

    #[test]
    fn strip_stale_reasoning_preserves_empty_field() {
        let mut msgs = vec![
            json!({"role": "assistant", "reasoning_content": "", "content": "a"}),
            json!({"role": "assistant", "reasoning_content": "real", "content": "b"}),
        ];
        strip_stale_reasoning(&mut msgs);
        // Already-empty field stays empty (not removed).
        assert_eq!(msgs[0]["reasoning_content"].as_str(), Some(""));
        assert_eq!(msgs[1]["reasoning_content"].as_str(), Some("real"));
    }

    #[test]
    fn strip_stale_reasoning_adds_field_to_tool_call_msg_missing_it() {
        // Simulates mid-session model switch: old assistant+tool_calls messages
        // lack reasoning_content, but a later thinking-model message has it.
        let mut msgs = vec![
            json!({"role": "user", "content": "q1"}),
            json!({"role": "assistant", "content": null, "tool_calls": [{"id":"t1","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
            json!({"role": "tool", "tool_call_id": "t1", "content": "ok"}),
            json!({"role": "assistant", "content": "done"}),
            json!({"role": "user", "content": "q2"}),
            json!({"role": "assistant", "content": null, "reasoning_content": "think", "tool_calls": [{"id":"t2","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
            json!({"role": "tool", "tool_call_id": "t2", "content": "ok2"}),
        ];
        strip_stale_reasoning(&mut msgs);
        // Old assistant+tool_calls at index 1 must now have reasoning_content
        assert_eq!(
            msgs[1]["reasoning_content"].as_str(),
            Some(""),
            "missing reasoning_content must be added as empty string"
        );
        // Latest reasoning preserved
        assert_eq!(msgs[5]["reasoning_content"].as_str(), Some("think"));
    }

    // ── Thinking-model / Kimi: reasoning_content on every assistant+tool_calls ──

    fn build_mid_switch_session() -> Vec<Value> {
        vec![
            json!({"role": "system", "content": "You are a helpful assistant."}),
            json!({"role": "user", "content": "list files"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "tc-1", "type": "function", "function": {"name": "bash", "arguments": "{\"cmd\":\"ls\"}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "tc-1", "content": "file1.txt\nfile2.txt"}),
            json!({"role": "assistant", "content": "Here are the files: file1.txt, file2.txt"}),
            json!({"role": "user", "content": "read both files"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "tc-2a", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"file1.txt\"}"}},
                    {"id": "tc-2b", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"file2.txt\"}"}}
                ]
            }),
            json!({"role": "tool", "tool_call_id": "tc-2a", "content": "contents of file1"}),
            json!({"role": "tool", "tool_call_id": "tc-2b", "content": "contents of file2"}),
            json!({"role": "assistant", "content": "File1 contains... File2 contains..."}),
            json!({"role": "user", "content": "write a new file"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "tc-3", "type": "function", "function": {"name": "write_file", "arguments": "{\"path\":\"new.txt\",\"content\":\"hello\"}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "tc-3", "content": "ok"}),
            json!({"role": "assistant", "content": "Done, created new.txt"}),
            json!({"role": "user", "content": "refactor the code"}),
            json!({
                "role": "assistant",
                "content": null,
                "reasoning_content": "I need to read the code first, then plan the refactoring.",
                "tool_calls": [{"id": "tc-4", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"main.rs\"}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "tc-4", "content": "fn main() { println!(\"hello\"); }"}),
            json!({
                "role": "assistant",
                "content": null,
                "reasoning_content": "Now I'll write the refactored version.",
                "tool_calls": [{"id": "tc-5", "type": "function", "function": {"name": "write_file", "arguments": "{\"path\":\"main.rs\",\"content\":\"fn main() { greet(); }\"}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "tc-5", "content": "ok"}),
            json!({
                "role": "assistant",
                "content": "Refactored the code.",
                "reasoning_content": "The refactoring is complete."
            }),
        ]
    }

    fn assert_all_assistant_tool_calls_have_reasoning(messages: &[Value]) {
        for (i, msg) in messages.iter().enumerate() {
            if msg.get("role").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            if msg.get("tool_calls").is_none() {
                continue;
            }
            assert!(
                msg.get("reasoning_content").is_some(),
                "assistant tool_call message at index {i} is missing reasoning_content"
            );
        }
    }

    #[test]
    fn mid_session_switch_to_thinking_model_all_tool_call_msgs_get_reasoning() {
        let mut msgs = build_mid_switch_session();
        strip_stale_reasoning(&mut msgs);
        assert!(history_has_reasoning(&msgs));
        assert_all_assistant_tool_calls_have_reasoning(&msgs);
    }

    #[test]
    fn mid_session_switch_preserves_latest_reasoning_content() {
        let mut msgs = build_mid_switch_session();
        strip_stale_reasoning(&mut msgs);
        let last_reasoning = msgs
            .iter()
            .rev()
            .find(|m| {
                m.get("role").and_then(Value::as_str) == Some("assistant")
                    && m.get("reasoning_content")
                        .and_then(Value::as_str)
                        .is_some_and(|s| !s.is_empty())
            })
            .expect("should have at least one message with non-empty reasoning");
        assert!(
            !last_reasoning["reasoning_content"]
                .as_str()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn old_non_thinking_tool_call_msgs_get_empty_reasoning() {
        let mut msgs = build_mid_switch_session();
        strip_stale_reasoning(&mut msgs);
        assert_eq!(msgs[2]["reasoning_content"].as_str(), Some(""));
        assert_eq!(msgs[6]["reasoning_content"].as_str(), Some(""));
        assert_eq!(msgs[11]["reasoning_content"].as_str(), Some(""));
    }

    #[test]
    fn recovered_history_with_model_switch_has_reasoning_on_all_tool_call_msgs() {
        let rows = vec![
            RecoveredEventRow {
                event_type: "user_query".into(),
                content: Some("list files".into()),
                metadata: None,
                reasoning_content: None,
            },
            RecoveredEventRow {
                event_type: "tool_call".into(),
                content: Some(
                    r#"{"tool_call_id":"tc-1","name":"bash","arguments":"{\"cmd\":\"ls\"}"}"#.into(),
                ),
                metadata: None,
                reasoning_content: None,
            },
            RecoveredEventRow {
                event_type: "tool_result".into(),
                content: Some(r#"{"result":"file1.txt"}"#.into()),
                metadata: Some(r#"{"tool_call_id":"tc-1","name":"bash"}"#.into()),
                reasoning_content: None,
            },
            RecoveredEventRow {
                event_type: "llm_response".into(),
                content: Some("Here are the files.".into()),
                metadata: None,
                reasoning_content: None,
            },
            RecoveredEventRow {
                event_type: "user_query".into(),
                content: Some("refactor".into()),
                metadata: None,
                reasoning_content: None,
            },
            RecoveredEventRow {
                event_type: "tool_call".into(),
                content: Some(
                    r#"{"tool_call_id":"tc-2","name":"read_file","arguments":"{\"path\":\"main.rs\"}"}"#
                        .into(),
                ),
                metadata: None,
                reasoning_content: Some("Let me read the code first.".into()),
            },
            RecoveredEventRow {
                event_type: "tool_result".into(),
                content: Some(r#"{"result":"fn main() {}"}"#.into()),
                metadata: Some(r#"{"tool_call_id":"tc-2","name":"read_file"}"#.into()),
                reasoning_content: None,
            },
            RecoveredEventRow {
                event_type: "llm_response".into(),
                content: Some("Refactored.".into()),
                metadata: None,
                reasoning_content: Some("Done with refactoring.".into()),
            },
        ];

        let mut history: Vec<Value> = Vec::new();
        append_recovered_events(&mut history, &rows);
        assert_all_assistant_tool_calls_have_reasoning(&history);
    }

    #[test]
    fn full_pipeline_recovery_then_strip_all_tool_call_msgs_valid() {
        let rows = vec![
            RecoveredEventRow {
                event_type: "user_query".into(),
                content: Some("hello".into()),
                metadata: None,
                reasoning_content: None,
            },
            RecoveredEventRow {
                event_type: "tool_call".into(),
                content: Some(r#"{"tool_call_id":"tc-1","name":"bash","arguments":"{}"}"#.into()),
                metadata: None,
                reasoning_content: None,
            },
            RecoveredEventRow {
                event_type: "tool_result".into(),
                content: Some(r#"{"result":"ok"}"#.into()),
                metadata: Some(r#"{"tool_call_id":"tc-1","name":"bash"}"#.into()),
                reasoning_content: None,
            },
            RecoveredEventRow {
                event_type: "llm_response".into(),
                content: Some("done".into()),
                metadata: None,
                reasoning_content: None,
            },
        ];

        let mut history: Vec<Value> = Vec::new();
        append_recovered_events(&mut history, &rows);

        history.push(json!({"role": "user", "content": "now use kimi"}));
        history.push(json!({
            "role": "assistant",
            "content": null,
            "reasoning_content": "I need to search for this.",
            "tool_calls": [{"id": "tc-new", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]
        }));
        history.push(json!({"role": "tool", "tool_call_id": "tc-new", "content": "result"}));

        strip_stale_reasoning(&mut history);
        assert_all_assistant_tool_calls_have_reasoning(&history);
    }

    #[test]
    fn pure_non_thinking_session_no_reasoning_added() {
        let mut msgs = vec![
            json!({"role": "user", "content": "hi"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "tc-1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "tc-1", "content": "ok"}),
            json!({"role": "assistant", "content": "done"}),
        ];
        let original = msgs.clone();
        strip_stale_reasoning(&mut msgs);
        assert_eq!(msgs, original);
    }

    #[test]
    fn last_message_is_thinking_tool_call_still_gets_field() {
        let mut msgs = vec![
            json!({"role": "user", "content": "hi"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "tc-1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "tc-1", "content": "ok"}),
            json!({"role": "assistant", "content": "done"}),
            json!({"role": "user", "content": "more"}),
            json!({
                "role": "assistant",
                "content": null,
                "reasoning_content": "thinking...",
                "tool_calls": [{"id": "tc-2", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]
            }),
        ];
        strip_stale_reasoning(&mut msgs);
        assert_all_assistant_tool_calls_have_reasoning(&msgs);
        assert_eq!(msgs[1]["reasoning_content"].as_str(), Some(""));
        assert_eq!(msgs[5]["reasoning_content"].as_str(), Some("thinking..."));
    }
}
