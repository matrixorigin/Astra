use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecoveredEventRow {
    pub event_type: String,
    pub content: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
    pub reasoning_content: Option<String>,
}

pub fn find_tool_call_safe_split(messages: &[Value], target_tail: usize) -> usize {
    if target_tail == 0 || target_tail >= messages.len() {
        return 0;
    }

    let mut idx = messages.len() - target_tail;
    while idx > 0 && role_at(messages, idx) == Some("tool") {
        idx -= 1;
    }
    idx
}

pub fn merge_tool_results_into_history(
    history: &mut Vec<Value>,
    tool_results: Option<&[Value]>,
) -> BTreeSet<String> {
    let mut consumed = BTreeSet::new();

    if let Some(tool_results) = tool_results {
        let mut pending = Map::new();
        for tool_result in tool_results {
            let Some(object) = tool_result.as_object() else {
                continue;
            };
            let tool_call_id = object
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !tool_call_id.is_empty() && !pending.contains_key(tool_call_id) {
                pending.insert(tool_call_id.to_string(), tool_result.clone());
            }
        }

        let mut inserts: Vec<(usize, Value)> = Vec::new();
        for index in 0..history.len() {
            let Some(message) = history[index].as_object() else {
                continue;
            };
            if message.get("role").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array).cloned()
            else {
                continue;
            };

            let mut block_end = index + 1;
            for follow_index in (index + 1)..history.len() {
                if role_at(history, follow_index) == Some("tool") {
                    block_end = follow_index + 1;
                } else {
                    break;
                }
            }

            let mut existing: Map<String, Value> = Map::new();
            for (i, item) in history[(index + 1)..block_end].iter().enumerate() {
                let follow_index = index + 1 + i;
                let Some(tool_msg) = item.as_object() else {
                    continue;
                };
                let tool_call_id = tool_msg
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if tool_call_id.is_empty() {
                    continue;
                }
                let is_placeholder = tool_msg
                    .get("content")
                    .and_then(Value::as_str)
                    .map(|content| content.contains("[not executed"))
                    .unwrap_or(false);
                existing.insert(
                    tool_call_id.to_string(),
                    json!({
                        "index": follow_index,
                        "is_placeholder": is_placeholder,
                    }),
                );
            }

            let mut insert_at = block_end;
            for tool_call in &tool_calls {
                let tool_call_id = tool_call
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if tool_call_id.is_empty() || !pending.contains_key(tool_call_id) {
                    continue;
                }

                if let Some(existing_entry) = existing.get(tool_call_id) {
                    let existing_index = existing_entry
                        .get("index")
                        .and_then(Value::as_u64)
                        .unwrap_or_default() as usize;
                    let is_placeholder = existing_entry
                        .get("is_placeholder")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if is_placeholder
                        && let Some(message) = history
                            .get_mut(existing_index)
                            .and_then(Value::as_object_mut)
                    {
                        message.insert(
                            "content".to_string(),
                            Value::String(result_content(
                                pending
                                    .get(tool_call_id)
                                    .expect("pending tool_result exists"),
                            )),
                        );
                    }
                    consumed.insert(tool_call_id.to_string());
                } else {
                    inserts.push((
                        insert_at,
                        json!({
                            "role": "tool",
                            "tool_call_id": tool_call_id,
                            "content": result_content(
                                pending.get(tool_call_id).expect("pending tool_result exists")
                            ),
                        }),
                    ));
                    consumed.insert(tool_call_id.to_string());
                    insert_at += 1;
                }
            }
        }

        for (position, tool_msg) in inserts.into_iter().rev() {
            history.insert(position, tool_msg);
        }
    }

    let mut heals: Vec<(usize, Value)> = Vec::new();
    for index in 0..history.len() {
        let Some(message) = history[index].as_object() else {
            continue;
        };
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array).cloned() else {
            continue;
        };

        let expected = tool_calls
            .iter()
            .filter_map(|tool_call| tool_call.get("id").and_then(Value::as_str))
            .map(ToOwned::to_owned)
            .collect::<BTreeSet<_>>();
        let mut found = BTreeSet::new();
        for follow_index in (index + 1)..history.len() {
            if role_at(history, follow_index) == Some("tool") {
                if let Some(tool_call_id) = history[follow_index]
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                {
                    found.insert(tool_call_id.to_string());
                }
            } else {
                break;
            }
        }

        let missing = expected.difference(&found).cloned().collect::<Vec<_>>();
        if missing.is_empty() {
            continue;
        }

        let mut insert_at = index + 1 + found.len();
        for tool_call in &tool_calls {
            let tool_call_id = tool_call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if missing.iter().any(|missing_id| missing_id == tool_call_id) {
                heals.push((
                    insert_at,
                    json!({
                        "role": "tool",
                        "tool_call_id": tool_call_id,
                        "content": "[not executed -- edge disconnected]",
                    }),
                ));
                insert_at += 1;
            }
        }
    }

    for (position, placeholder) in heals.into_iter().rev() {
        history.insert(position, placeholder);
    }

    consumed
}

/// Check if any assistant message in the history has reasoning_content.
/// When thinking is enabled, ALL assistant messages with tool_calls must have
/// this field (even empty string) or the API returns 400.
fn history_has_reasoning(history: &[Value]) -> bool {
    history.iter().any(|m| {
        m.get("role").and_then(Value::as_str) == Some("assistant")
            && m.get("reasoning_content").is_some()
    })
}

pub fn append_recovered_events(history: &mut Vec<Value>, rows: &[RecoveredEventRow]) {
    let mut pending_tool_calls: Vec<Value> = Vec::new();
    let mut pending_reasoning = String::new();
    let mut in_tool_batch = false;

    // Detect if thinking is enabled: check existing history OR incoming rows
    let force_reasoning_field = history_has_reasoning(history)
        || rows
            .iter()
            .any(|r| r.reasoning_content.as_ref().is_some_and(|s| !s.is_empty()));

    for row in rows {
        let content = row.content.clone().unwrap_or_default();
        let metadata = parsed_metadata(row.metadata.clone());
        let row_reasoning = row.reasoning_content.clone().unwrap_or_default();

        match row.event_type.as_str() {
            "user_query" => {
                in_tool_batch = false;
                history.push(json!({
                    "role": "user",
                    "content": content,
                }));
            }
            "tool_call" => {
                let tool_call_data = parsed_object(&content);
                if pending_tool_calls.is_empty() && !row_reasoning.is_empty() {
                    pending_reasoning = row_reasoning;
                }
                pending_tool_calls.push(json!({
                    "id": tool_call_data
                        .get("tool_call_id")
                        .and_then(Value::as_str)
                        .or_else(|| metadata.get("tool_call_id").and_then(Value::as_str))
                        .unwrap_or_default(),
                    "type": "function",
                    "function": {
                        "name": tool_call_data
                            .get("name")
                            .and_then(Value::as_str)
                            .or_else(|| metadata.get("name").and_then(Value::as_str))
                            .unwrap_or_default(),
                        "arguments": tool_call_data
                            .get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}"),
                    },
                }));
            }
            "tool_result" => {
                let tool_call_id = metadata
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let tool_name = metadata
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();

                if !pending_tool_calls.is_empty() {
                    let mut assistant = Map::from_iter([
                        ("role".to_string(), Value::String("assistant".to_string())),
                        ("content".to_string(), Value::String(String::new())),
                        (
                            "tool_calls".to_string(),
                            Value::Array(pending_tool_calls.clone()),
                        ),
                    ]);
                    // When thinking is enabled, ALL assistant+tool_calls messages must have reasoning_content
                    if force_reasoning_field || !pending_reasoning.is_empty() {
                        assistant.insert(
                            "reasoning_content".to_string(),
                            Value::String(std::mem::take(&mut pending_reasoning)),
                        );
                    }
                    history.push(Value::Object(assistant));
                    pending_tool_calls.clear();
                    in_tool_batch = true;
                } else if !in_tool_batch {
                    if tool_call_id.is_empty() {
                        continue;
                    }
                    let mut orphan = Map::from_iter([
                        ("role".to_string(), Value::String("assistant".to_string())),
                        ("content".to_string(), Value::String(String::new())),
                        (
                            "tool_calls".to_string(),
                            json!([{
                                "id": tool_call_id,
                                "type": "function",
                                "function": {
                                    "name": tool_name,
                                    "arguments": "{}",
                                },
                            }]),
                        ),
                    ]);
                    if force_reasoning_field {
                        orphan.insert(
                            "reasoning_content".to_string(),
                            Value::String(String::new()),
                        );
                    }
                    history.push(Value::Object(orphan));
                    in_tool_batch = true;
                }

                let result_data = parsed_object(&content);
                let result_content = result_data
                    .get("result")
                    .map(|value| match value {
                        Value::String(string) => string.clone(),
                        _ => value.to_string(),
                    })
                    .unwrap_or(content)
                    .chars()
                    .take(4000)
                    .collect::<String>();
                history.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": result_content,
                }));
            }
            "llm_response" => {
                in_tool_batch = false;
                if !pending_tool_calls.is_empty() {
                    let mut flushed = Map::from_iter([
                        ("role".to_string(), Value::String("assistant".to_string())),
                        ("content".to_string(), Value::String(String::new())),
                        (
                            "tool_calls".to_string(),
                            Value::Array(pending_tool_calls.clone()),
                        ),
                    ]);
                    // When thinking is enabled, ALL assistant+tool_calls messages must have reasoning_content
                    if force_reasoning_field || !pending_reasoning.is_empty() {
                        flushed.insert(
                            "reasoning_content".to_string(),
                            Value::String(std::mem::take(&mut pending_reasoning)),
                        );
                    }
                    history.push(Value::Object(flushed));
                    pending_tool_calls.clear();
                }

                let mut assistant = Map::from_iter([
                    ("role".to_string(), Value::String("assistant".to_string())),
                    ("content".to_string(), Value::String(content)),
                ]);
                if !row_reasoning.is_empty() {
                    assistant.insert(
                        "reasoning_content".to_string(),
                        Value::String(row_reasoning),
                    );
                }
                history.push(Value::Object(assistant));
            }
            _ => {}
        }
    }

    if !pending_tool_calls.is_empty() {
        let mut assistant = Map::from_iter([
            ("role".to_string(), Value::String("assistant".to_string())),
            ("content".to_string(), Value::String(String::new())),
            ("tool_calls".to_string(), Value::Array(pending_tool_calls)),
        ]);
        // When thinking is enabled, ALL assistant+tool_calls messages must have reasoning_content
        if force_reasoning_field || !pending_reasoning.is_empty() {
            assistant.insert(
                "reasoning_content".to_string(),
                Value::String(pending_reasoning),
            );
        }
        history.push(Value::Object(assistant));
    }
}

fn role_at(messages: &[Value], index: usize) -> Option<&str> {
    messages
        .get(index)
        .and_then(Value::as_object)
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)
}

fn result_content(tool_result: &Value) -> String {
    let Some(object) = tool_result.as_object() else {
        return String::new();
    };
    match object.get("result") {
        Some(Value::String(string)) => string.clone(),
        Some(value) => json_stringify(value),
        None => String::new(),
    }
}

fn json_stringify(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn parsed_object(content: &str) -> Map<String, Value> {
    serde_json::from_str::<Value>(content)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

fn parsed_metadata(metadata: Option<Value>) -> Map<String, Value> {
    match metadata {
        Some(Value::Object(object)) => object,
        Some(Value::String(string)) => parsed_object(&string),
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── helpers ──────────────────────────────────────────────────────

    fn user_msg(content: &str) -> Value {
        json!({ "role": "user", "content": content })
    }

    fn assistant_msg(content: &str) -> Value {
        json!({ "role": "assistant", "content": content })
    }

    fn assistant_with_tool_calls(tool_call_ids: &[&str]) -> Value {
        let calls: Vec<Value> = tool_call_ids
            .iter()
            .map(|id| {
                json!({
                    "id": *id,
                    "type": "function",
                    "function": { "name": "some_tool", "arguments": "{}" }
                })
            })
            .collect();
        json!({ "role": "assistant", "content": "", "tool_calls": calls })
    }

    fn tool_msg(tool_call_id: &str, content: &str) -> Value {
        json!({ "role": "tool", "tool_call_id": tool_call_id, "content": content })
    }

    fn tool_result(tool_call_id: &str, result: &str) -> Value {
        json!({ "tool_call_id": tool_call_id, "result": result })
    }

    // ── find_tool_call_safe_split ───────────────────────────────────

    #[test]
    fn safe_split_basic() {
        // user, assistant+tool_calls, tool, user, assistant
        let msgs = vec![
            user_msg("hi"),
            assistant_with_tool_calls(&["c1"]),
            tool_msg("c1", "ok"),
            user_msg("thanks"),
            assistant_msg("bye"),
        ];
        // target_tail=2 → naive split at index 3 (user "thanks"); no tool
        // role there, so split stays at 3.
        assert_eq!(find_tool_call_safe_split(&msgs, 2), 3);
    }

    #[test]
    fn safe_split_no_tool_calls() {
        let msgs = vec![
            user_msg("a"),
            assistant_msg("b"),
            user_msg("c"),
            assistant_msg("d"),
        ];
        // No tool messages anywhere; every naive split point is safe.
        assert_eq!(find_tool_call_safe_split(&msgs, 2), 2);
        assert_eq!(find_tool_call_safe_split(&msgs, 1), 3);
    }

    #[test]
    fn safe_split_tool_call_at_boundary() {
        // user, assistant+tool_calls, tool, tool, user
        let msgs = vec![
            user_msg("q"),
            assistant_with_tool_calls(&["c1", "c2"]),
            tool_msg("c1", "r1"),
            tool_msg("c2", "r2"),
            user_msg("next"),
        ];
        // target_tail=3 → naive idx=2 (tool "r1"), back up past tools → idx=1
        assert_eq!(find_tool_call_safe_split(&msgs, 3), 1);
        // target_tail=1 → naive idx=4 (user "next"), safe already
        assert_eq!(find_tool_call_safe_split(&msgs, 1), 4);
    }

    #[test]
    fn safe_split_target_zero_returns_zero() {
        let msgs = vec![user_msg("a"), assistant_msg("b")];
        assert_eq!(find_tool_call_safe_split(&msgs, 0), 0);
    }

    #[test]
    fn safe_split_target_ge_len_returns_zero() {
        let msgs = vec![user_msg("a")];
        assert_eq!(find_tool_call_safe_split(&msgs, 1), 0);
        assert_eq!(find_tool_call_safe_split(&msgs, 5), 0);
    }

    #[test]
    fn safe_split_single_message() {
        let msgs = vec![user_msg("only")];
        assert_eq!(find_tool_call_safe_split(&msgs, 1), 0);
        assert_eq!(find_tool_call_safe_split(&msgs, 0), 0);
    }

    // ── merge_tool_results_into_history ─────────────────────────────

    #[test]
    fn merge_basic() {
        let mut history = vec![
            user_msg("q"),
            assistant_with_tool_calls(&["c1"]),
            // no tool result yet
        ];
        let results = vec![tool_result("c1", "answer")];
        let consumed = merge_tool_results_into_history(&mut history, Some(&results));

        assert!(consumed.contains("c1"));
        // A tool message should have been inserted after the assistant msg
        assert_eq!(history.len(), 3);
        assert_eq!(history[2]["role"], "tool");
        assert_eq!(history[2]["tool_call_id"], "c1");
        assert_eq!(history[2]["content"], "answer");
    }

    #[test]
    fn merge_empty_tool_results() {
        let mut history = vec![user_msg("q"), assistant_with_tool_calls(&["c1"])];
        let original_len = history.len();

        // None case
        let consumed = merge_tool_results_into_history(&mut history, None);
        assert!(consumed.is_empty());
        // Healing should add placeholder for missing c1
        assert_eq!(history.len(), original_len + 1);
        assert!(
            history[2]["content"]
                .as_str()
                .unwrap()
                .contains("not executed")
        );

        // Empty slice case
        let mut history2 = vec![user_msg("q"), assistant_with_tool_calls(&["c1"])];
        let consumed2 = merge_tool_results_into_history(&mut history2, Some(&[]));
        assert!(consumed2.is_empty());
        // Healing again
        assert!(
            history2[2]["content"]
                .as_str()
                .unwrap()
                .contains("not executed")
        );
    }

    #[test]
    fn merge_results_for_nonexistent_tool_calls() {
        let mut history = vec![user_msg("q"), assistant_msg("no tool calls here")];
        let results = vec![tool_result("nonexistent", "data")];
        let consumed = merge_tool_results_into_history(&mut history, Some(&results));

        // Nothing matched, nothing consumed
        assert!(consumed.is_empty());
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn merge_replaces_placeholder() {
        let mut history = vec![
            user_msg("q"),
            assistant_with_tool_calls(&["c1"]),
            json!({
                "role": "tool",
                "tool_call_id": "c1",
                "content": "[not executed -- edge disconnected]"
            }),
        ];
        let results = vec![tool_result("c1", "real answer")];
        let consumed = merge_tool_results_into_history(&mut history, Some(&results));

        assert!(consumed.contains("c1"));
        // Placeholder replaced in-place, no extra message
        assert_eq!(history.len(), 3);
        assert_eq!(history[2]["content"], "real answer");
    }

    #[test]
    fn merge_heals_missing_tool_results() {
        // Two tool calls but only one result provided
        let mut history = vec![
            user_msg("q"),
            assistant_with_tool_calls(&["c1", "c2"]),
            tool_msg("c1", "ok"),
        ];
        let consumed = merge_tool_results_into_history(&mut history, None);
        assert!(consumed.is_empty());

        // c2 should get a healing placeholder
        assert_eq!(history.len(), 4);
        let healed = &history[3];
        assert_eq!(healed["role"], "tool");
        assert_eq!(healed["tool_call_id"], "c2");
        assert!(healed["content"].as_str().unwrap().contains("not executed"));
    }

    #[test]
    fn merge_multiple_assistant_blocks() {
        // Two separate assistant messages, each with one tool call
        let mut history = vec![
            user_msg("q1"),
            assistant_with_tool_calls(&["c1"]),
            tool_msg("c1", "placeholder"),
            user_msg("q2"),
            assistant_with_tool_calls(&["c2"]),
        ];
        let results = vec![tool_result("c2", "res2")];
        let consumed = merge_tool_results_into_history(&mut history, Some(&results));

        assert!(consumed.contains("c2"));
        // c2 result inserted after second assistant block
        assert_eq!(history[5]["role"], "tool");
        assert_eq!(history[5]["tool_call_id"], "c2");
        assert_eq!(history[5]["content"], "res2");
    }

    // ── append_recovered_events ─────────────────────────────────────

    #[test]
    fn append_basic() {
        let mut history = Vec::new();
        let rows = vec![
            RecoveredEventRow {
                event_type: "user_query".to_string(),
                content: Some("hello".to_string()),
                metadata: None,
                reasoning_content: None,
            },
            RecoveredEventRow {
                event_type: "llm_response".to_string(),
                content: Some("hi there".to_string()),
                metadata: None,
                reasoning_content: None,
            },
        ];

        append_recovered_events(&mut history, &rows);

        assert_eq!(history.len(), 2);
        assert_eq!(history[0]["role"], "user");
        assert_eq!(history[0]["content"], "hello");
        assert_eq!(history[1]["role"], "assistant");
        assert_eq!(history[1]["content"], "hi there");
    }

    #[test]
    fn append_empty_input() {
        let mut history = vec![user_msg("existing")];
        append_recovered_events(&mut history, &[]);
        assert_eq!(history.len(), 1);
    }

    #[test]
    fn append_tool_call_then_result() {
        let mut history = Vec::new();
        let rows = vec![
            RecoveredEventRow {
                event_type: "tool_call".to_string(),
                content: Some(
                    r#"{"tool_call_id":"c1","name":"bash","arguments":"{\"cmd\":\"ls\"}"}"#
                        .to_string(),
                ),
                metadata: None,
                reasoning_content: Some("let me check".to_string()),
            },
            RecoveredEventRow {
                event_type: "tool_result".to_string(),
                content: Some(r#"{"result":"file1 file2"}"#.to_string()),
                metadata: Some(json!({"tool_call_id": "c1", "name": "bash"})),
                reasoning_content: None,
            },
        ];

        append_recovered_events(&mut history, &rows);

        // Should produce: assistant (with tool_calls + reasoning), tool result
        assert_eq!(history.len(), 2);
        assert_eq!(history[0]["role"], "assistant");
        assert!(history[0]["tool_calls"].is_array());
        assert_eq!(history[0]["reasoning_content"], "let me check");
        assert_eq!(history[0]["tool_calls"][0]["id"], "c1");
        assert_eq!(history[1]["role"], "tool");
        assert_eq!(history[1]["tool_call_id"], "c1");
        assert_eq!(history[1]["content"], "file1 file2");
    }

    #[test]
    fn append_llm_response_with_reasoning() {
        let mut history = Vec::new();
        let rows = vec![RecoveredEventRow {
            event_type: "llm_response".to_string(),
            content: Some("The answer is 42".to_string()),
            metadata: None,
            reasoning_content: Some("thinking hard".to_string()),
        }];

        append_recovered_events(&mut history, &rows);

        assert_eq!(history.len(), 1);
        assert_eq!(history[0]["content"], "The answer is 42");
        assert_eq!(history[0]["reasoning_content"], "thinking hard");
    }

    #[test]
    fn append_flushes_pending_tool_calls_at_end() {
        let mut history = Vec::new();
        let rows = vec![RecoveredEventRow {
            event_type: "tool_call".to_string(),
            content: Some(r#"{"tool_call_id":"c1","name":"search","arguments":"{}"}"#.to_string()),
            metadata: None,
            reasoning_content: None,
        }];

        append_recovered_events(&mut history, &rows);

        // Pending tool call flushed as an assistant message
        assert_eq!(history.len(), 1);
        assert_eq!(history[0]["role"], "assistant");
        assert!(history[0]["tool_calls"].is_array());
    }

    #[test]
    fn append_tool_result_without_prior_tool_call_synthesizes_assistant() {
        let mut history = Vec::new();
        let rows = vec![RecoveredEventRow {
            event_type: "tool_result".to_string(),
            content: Some(r#"{"result":"orphan result"}"#.to_string()),
            metadata: Some(json!({"tool_call_id": "c99", "name": "grep"})),
            reasoning_content: None,
        }];

        append_recovered_events(&mut history, &rows);

        // Should synthesize an assistant msg with tool_calls, then tool result
        assert_eq!(history.len(), 2);
        assert_eq!(history[0]["role"], "assistant");
        assert_eq!(history[0]["tool_calls"][0]["id"], "c99");
        assert_eq!(history[1]["role"], "tool");
        assert_eq!(history[1]["content"], "orphan result");
    }

    // ── edge cases ──────────────────────────────────────────────────

    #[test]
    fn single_message_history_merge() {
        let mut history = vec![user_msg("only")];
        let consumed = merge_tool_results_into_history(&mut history, None);
        assert!(consumed.is_empty());
        assert_eq!(history.len(), 1);
    }

    #[test]
    fn all_assistant_with_single_tool_calls_healed() {
        // Multiple assistant messages each with one tool call, no results
        let mut history = vec![
            user_msg("q"),
            assistant_with_tool_calls(&["a1"]),
            assistant_with_tool_calls(&["b1"]),
        ];

        let consumed = merge_tool_results_into_history(&mut history, None);
        assert!(consumed.is_empty());

        // Healing should add placeholders for a1 and b1
        let tool_msgs: Vec<_> = history
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
            .collect();
        assert_eq!(tool_msgs.len(), 2);
        for tm in &tool_msgs {
            assert!(tm["content"].as_str().unwrap().contains("not executed"));
        }
    }

    #[test]
    fn safe_split_all_tools_backs_up_to_zero() {
        // Degenerate: all messages are tool role (shouldn't happen in
        // practice but exercises the while-loop boundary).
        let msgs = vec![
            tool_msg("c1", "r1"),
            tool_msg("c2", "r2"),
            tool_msg("c3", "r3"),
        ];
        // target_tail=1 → naive idx=2 (tool), backs up to 0
        assert_eq!(find_tool_call_safe_split(&msgs, 1), 0);
    }

    /// When an llm_response follows pending tool_calls that had reasoning,
    /// the flushed assistant message must carry reasoning_content so that
    /// thinking-enabled models don't reject the history with HTTP 400.
    #[test]
    fn append_llm_response_flushes_pending_reasoning_into_tool_call_message() {
        let mut history = Vec::new();
        let rows = vec![
            RecoveredEventRow {
                event_type: "tool_call".to_string(),
                content: Some(
                    r#"{"tool_call_id":"c1","name":"bash","arguments":"{\"cmd\":\"ls\"}"}"#
                        .to_string(),
                ),
                metadata: None,
                reasoning_content: Some("I need to list files".to_string()),
            },
            RecoveredEventRow {
                event_type: "tool_result".to_string(),
                content: Some(r#"{"result":"file.txt"}"#.to_string()),
                metadata: Some(json!({"tool_call_id": "c1", "name": "bash"})),
                reasoning_content: None,
            },
            RecoveredEventRow {
                event_type: "llm_response".to_string(),
                content: Some("Here are the files.".to_string()),
                metadata: None,
                reasoning_content: None,
            },
        ];

        append_recovered_events(&mut history, &rows);

        // assistant(tool_calls) + tool(result) + assistant(text)
        assert_eq!(history.len(), 3);
        let tc_msg = &history[0];
        assert_eq!(tc_msg["role"], "assistant");
        assert!(tc_msg["tool_calls"].is_array());
        assert_eq!(
            tc_msg["reasoning_content"].as_str(),
            Some("I need to list files"),
            "flushed assistant tool-call message must carry reasoning_content"
        );
    }

    // ──────────────────────────────────────────────────────────
    // json_stringify
    // ──────────────────────────────────────────────────────────

    #[test]
    fn json_stringify_object() {
        let v = json!({"a": 1});
        let s = json_stringify(&v);
        assert!(s.contains("\"a\""));
        assert!(s.contains('1'));
    }

    #[test]
    fn json_stringify_string() {
        let v = json!("hello");
        assert_eq!(json_stringify(&v), "\"hello\"");
    }

    #[test]
    fn json_stringify_null() {
        assert_eq!(json_stringify(&Value::Null), "null");
    }

    // ──────────────────────────────────────────────────────────
    // parsed_object
    // ──────────────────────────────────────────────────────────

    #[test]
    fn parsed_object_valid_json() {
        let map = parsed_object(r#"{"key": "value"}"#);
        assert_eq!(map.get("key").unwrap(), "value");
    }

    #[test]
    fn parsed_object_invalid_json() {
        let map = parsed_object("not json");
        assert!(map.is_empty());
    }

    #[test]
    fn parsed_object_json_array() {
        // Array is valid JSON but not an object
        let map = parsed_object("[1, 2, 3]");
        assert!(map.is_empty());
    }

    #[test]
    fn parsed_object_empty_string() {
        let map = parsed_object("");
        assert!(map.is_empty());
    }

    #[test]
    fn parsed_object_json_string() {
        let map = parsed_object("\"hello\"");
        assert!(map.is_empty());
    }

    // ──────────────────────────────────────────────────────────
    // parsed_metadata
    // ──────────────────────────────────────────────────────────

    #[test]
    fn parsed_metadata_object() {
        let map = parsed_metadata(Some(json!({"tool_call_id": "tc1"})));
        assert_eq!(map.get("tool_call_id").unwrap(), "tc1");
    }

    #[test]
    fn parsed_metadata_string_json() {
        let map = parsed_metadata(Some(json!(r#"{"tool_call_id": "tc2"}"#)));
        assert_eq!(map.get("tool_call_id").unwrap(), "tc2");
    }

    #[test]
    fn parsed_metadata_invalid_string() {
        let map = parsed_metadata(Some(json!("not json")));
        assert!(map.is_empty());
    }

    #[test]
    fn parsed_metadata_none() {
        let map = parsed_metadata(None);
        assert!(map.is_empty());
    }

    #[test]
    fn parsed_metadata_null() {
        let map = parsed_metadata(Some(Value::Null));
        assert!(map.is_empty());
    }

    #[test]
    fn parsed_metadata_number() {
        let map = parsed_metadata(Some(json!(42)));
        assert!(map.is_empty());
    }

    // ──────────────────────────────────────────────────────────
    // result_content
    // ──────────────────────────────────────────────────────────

    #[test]
    fn result_content_string_result() {
        let v = json!({"result": "success"});
        assert_eq!(result_content(&v), "success");
    }

    #[test]
    fn result_content_object_result() {
        let v = json!({"result": {"code": 0}});
        let s = result_content(&v);
        assert!(s.contains("\"code\""));
    }

    #[test]
    fn result_content_missing_result() {
        let v = json!({"other": "data"});
        assert_eq!(result_content(&v), "");
    }

    #[test]
    fn result_content_non_object() {
        assert_eq!(result_content(&json!("string")), "");
        assert_eq!(result_content(&json!(42)), "");
        assert_eq!(result_content(&Value::Null), "");
    }

    // ──────────────────────────────────────────────────────────
    // role_at
    // ──────────────────────────────────────────────────────────

    #[test]
    fn role_at_valid() {
        let msgs = vec![json!({"role": "user"}), json!({"role": "assistant"})];
        assert_eq!(role_at(&msgs, 0), Some("user"));
        assert_eq!(role_at(&msgs, 1), Some("assistant"));
    }

    #[test]
    fn role_at_out_of_bounds() {
        let msgs = vec![json!({"role": "user"})];
        assert_eq!(role_at(&msgs, 5), None);
    }

    #[test]
    fn role_at_no_role_field() {
        let msgs = vec![json!({"content": "hi"})];
        assert_eq!(role_at(&msgs, 0), None);
    }

    #[test]
    fn role_at_non_string_role() {
        let msgs = vec![json!({"role": 42})];
        assert_eq!(role_at(&msgs, 0), None);
    }

    // ──────────────────────────────────────────────────────────
    // history_has_reasoning
    // ──────────────────────────────────────────────────────────

    #[test]
    fn history_has_reasoning_present() {
        let h = vec![json!({"role": "assistant", "reasoning_content": "thinking..."})];
        assert!(history_has_reasoning(&h));
    }

    #[test]
    fn history_has_reasoning_empty_string() {
        // Even empty string counts as "present"
        let h = vec![json!({"role": "assistant", "reasoning_content": ""})];
        assert!(history_has_reasoning(&h));
    }

    #[test]
    fn history_has_reasoning_absent() {
        let h = vec![json!({"role": "assistant", "content": "hi"})];
        assert!(!history_has_reasoning(&h));
    }

    #[test]
    fn history_has_reasoning_only_user() {
        let h = vec![json!({"role": "user", "reasoning_content": "x"})];
        assert!(!history_has_reasoning(&h));
    }

    #[test]
    fn history_has_reasoning_empty() {
        assert!(!history_has_reasoning(&[]));
    }
}
