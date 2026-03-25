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

pub fn append_recovered_events(history: &mut Vec<Value>, rows: &[RecoveredEventRow]) {
    let mut pending_tool_calls: Vec<Value> = Vec::new();
    let mut pending_reasoning = String::new();
    let mut in_tool_batch = false;

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
                    if !pending_reasoning.is_empty() {
                        assistant.insert(
                            "reasoning_content".to_string(),
                            Value::String(pending_reasoning.clone()),
                        );
                    }
                    history.push(Value::Object(assistant));
                    pending_tool_calls.clear();
                    pending_reasoning.clear();
                    in_tool_batch = true;
                } else if !in_tool_batch {
                    if tool_call_id.is_empty() {
                        continue;
                    }
                    history.push(json!({
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [{
                            "id": tool_call_id,
                            "type": "function",
                            "function": {
                                "name": tool_name,
                                "arguments": "{}",
                            },
                        }],
                    }));
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
                    history.push(json!({
                        "role": "assistant",
                        "content": "",
                        "tool_calls": pending_tool_calls.clone(),
                    }));
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
        if !pending_reasoning.is_empty() {
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
