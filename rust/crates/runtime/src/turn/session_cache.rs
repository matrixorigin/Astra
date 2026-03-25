use serde_json::{Map, Value, json};

use crate::{build_cached_assistant_message, compact_cloud_loop_history};

pub fn apply_turn_to_session_entry(
    entry: &Map<String, Value>,
    full_text: &str,
    tool_calls: &[Value],
    reasoning_content: &str,
    cloud_loop_history: &[Value],
    turn_chain_id: Option<&str>,
    user_query_event_id: Option<&str>,
) -> Map<String, Value> {
    let mut updated_entry = entry.clone();
    let mut history = updated_entry
        .get("history")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !cloud_loop_history.is_empty() {
        history.extend(compact_cloud_loop_history(cloud_loop_history, 500, 2));
    }
    history.push(Value::Object(build_cached_assistant_message(
        full_text,
        tool_calls,
        reasoning_content,
    )));
    updated_entry.insert("history".to_string(), Value::Array(history));
    let turn_count = updated_entry
        .get("turn_count")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        + 1;
    updated_entry.insert("turn_count".to_string(), json!(turn_count));
    updated_entry.insert(
        "turn_chain_id".to_string(),
        turn_chain_id.map(|v| json!(v)).unwrap_or(Value::Null),
    );
    updated_entry.insert(
        "user_query_event_id".to_string(),
        user_query_event_id.map(|v| json!(v)).unwrap_or(Value::Null),
    );
    updated_entry
}
