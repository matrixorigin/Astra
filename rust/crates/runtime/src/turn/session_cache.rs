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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_entry_gets_history_and_turn_count() {
        let entry = Map::new();
        let result = apply_turn_to_session_entry(&entry, "hello", &[], "", &[], None, None);
        assert_eq!(result["turn_count"].as_i64().unwrap(), 1);
        assert_eq!(result["history"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn increments_existing_turn_count() {
        let mut entry = Map::new();
        entry.insert("turn_count".to_string(), json!(5));
        let result = apply_turn_to_session_entry(&entry, "hi", &[], "", &[], None, None);
        assert_eq!(result["turn_count"].as_i64().unwrap(), 6);
    }

    #[test]
    fn preserves_existing_history() {
        let mut entry = Map::new();
        entry.insert(
            "history".to_string(),
            json!([{"role": "user", "content": "old"}]),
        );
        let result = apply_turn_to_session_entry(&entry, "new", &[], "", &[], None, None);
        let hist = result["history"].as_array().unwrap();
        assert!(hist.len() >= 2);
    }

    #[test]
    fn sets_turn_chain_id() {
        let entry = Map::new();
        let result = apply_turn_to_session_entry(&entry, "", &[], "", &[], Some("chain-1"), None);
        assert_eq!(result["turn_chain_id"].as_str().unwrap(), "chain-1");
    }

    #[test]
    fn null_turn_chain_id_when_none() {
        let entry = Map::new();
        let result = apply_turn_to_session_entry(&entry, "", &[], "", &[], None, None);
        assert!(result["turn_chain_id"].is_null());
    }

    #[test]
    fn sets_user_query_event_id() {
        let entry = Map::new();
        let result = apply_turn_to_session_entry(&entry, "", &[], "", &[], None, Some("evt-1"));
        assert_eq!(result["user_query_event_id"].as_str().unwrap(), "evt-1");
    }

    #[test]
    fn non_integer_turn_count_treated_as_zero() {
        let mut entry = Map::new();
        entry.insert("turn_count".to_string(), json!("not_a_number"));
        let result = apply_turn_to_session_entry(&entry, "", &[], "", &[], None, None);
        assert_eq!(result["turn_count"].as_i64().unwrap(), 1);
    }
}
