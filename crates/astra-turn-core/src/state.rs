use serde_json::{Map, Value, json};

pub fn new_session_entry(created_at: &str) -> Value {
    json!({
        "history": Value::Null,
        "tools": [],
        "sections": Value::Null,
        "spend_usd": 0.0,
        "turn_count": 0,
        "created_at": created_at,
    })
}

pub fn resolve_turn_identifiers(
    messages: &[Value],
    has_tool_results: bool,
    prev_entry: Option<&mut Map<String, Value>>,
    new_turn_chain_id: &str,
    new_user_query_event_id: &str,
) -> (String, String) {
    let latest_conversation_role = messages.iter().rev().find_map(|message| {
        match message.get("role").and_then(Value::as_str) {
            Some("user" | "assistant" | "tool") => message.get("role").and_then(Value::as_str),
            _ => None,
        }
    });
    let has_new_user_query = latest_conversation_role == Some("user");

    if !has_new_user_query
        && has_tool_results
        && let Some(entry) = prev_entry
    {
        return (
            entry
                .get("turn_chain_id")
                .and_then(Value::as_str)
                .unwrap_or(new_turn_chain_id)
                .to_string(),
            entry
                .get("user_query_event_id")
                .and_then(Value::as_str)
                .unwrap_or(new_user_query_event_id)
                .to_string(),
        );
    }

    if let Some(entry) = prev_entry {
        entry.remove("tool_sigs");
    }
    (
        new_turn_chain_id.to_string(),
        new_user_query_event_id.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- new_session_entry ---

    #[test]
    fn session_entry_schema() {
        let entry = new_session_entry("2025-01-01T00:00:00Z");
        assert!(entry["history"].is_null());
        assert!(entry["tools"].as_array().unwrap().is_empty());
        assert!(entry["sections"].is_null());
        assert_eq!(entry["spend_usd"].as_f64().unwrap(), 0.0);
        assert_eq!(entry["turn_count"].as_i64().unwrap(), 0);
        assert_eq!(
            entry["created_at"].as_str().unwrap(),
            "2025-01-01T00:00:00Z"
        );
    }

    #[test]
    fn session_entry_empty_created_at() {
        let entry = new_session_entry("");
        assert_eq!(entry["created_at"].as_str().unwrap(), "");
    }

    // --- resolve_turn_identifiers ---

    #[test]
    fn resolve_new_user_message_uses_new_ids() {
        let msgs = [json!({"role": "user", "content": "hi"})];
        let (chain, query) = resolve_turn_identifiers(&msgs, false, None, "new-chain", "new-query");
        assert_eq!(chain, "new-chain");
        assert_eq!(query, "new-query");
    }

    #[test]
    fn resolve_tool_results_reuses_prev_ids() {
        let msgs = [json!({"role": "system", "content": "sys"})];
        let mut prev = Map::from_iter([
            ("turn_chain_id".to_string(), json!("old-chain")),
            ("user_query_event_id".to_string(), json!("old-query")),
        ]);
        let (chain, query) =
            resolve_turn_identifiers(&msgs, true, Some(&mut prev), "new-chain", "new-query");
        assert_eq!(chain, "old-chain");
        assert_eq!(query, "old-query");
    }

    #[test]
    fn resolve_tool_results_no_prev_uses_new() {
        let msgs = [json!({"role": "system", "content": "sys"})];
        let (chain, query) = resolve_turn_identifiers(&msgs, true, None, "new-chain", "new-query");
        assert_eq!(chain, "new-chain");
        assert_eq!(query, "new-query");
    }

    #[test]
    fn resolve_new_turn_removes_tool_sigs() {
        let msgs = [json!({"role": "user", "content": "hi"})];
        let mut prev = Map::from_iter([("tool_sigs".to_string(), json!("data"))]);
        resolve_turn_identifiers(&msgs, false, Some(&mut prev), "c", "q");
        assert!(!prev.contains_key("tool_sigs"));
    }

    #[test]
    fn resolve_tool_results_prev_missing_chain_id() {
        let msgs: Vec<Value> = vec![];
        let mut prev = Map::new();
        let (chain, query) =
            resolve_turn_identifiers(&msgs, true, Some(&mut prev), "fallback-c", "fallback-q");
        assert_eq!(chain, "fallback-c");
        assert_eq!(query, "fallback-q");
    }

    #[test]
    fn resolve_tool_results_with_history_reuses_prev_ids() {
        let msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "find the bug"}),
            json!({"role": "assistant", "content": null, "tool_calls": [{"id": "call-1"}]}),
            json!({"role": "tool", "tool_call_id": "call-1", "content": "done"}),
        ];
        let mut prev = Map::from_iter([
            ("turn_chain_id".to_string(), json!("old-chain")),
            ("user_query_event_id".to_string(), json!("old-query")),
        ]);

        let (chain, query) =
            resolve_turn_identifiers(&msgs, true, Some(&mut prev), "new-chain", "new-query");

        assert_eq!(chain, "old-chain");
        assert_eq!(query, "old-query");
    }
}
