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

pub fn normalize_bridge_cache_entry(entry: &Map<String, Value>) -> Option<Map<String, Value>> {
    let has_seed_state = entry.contains_key("created_at")
        || ["history", "sections", "turn_count"]
            .into_iter()
            .any(|field| entry.contains_key(field));
    if !has_seed_state {
        return None;
    }

    let mut normalized = Map::new();
    normalized.insert(
        "history".to_string(),
        entry.get("history").cloned().unwrap_or(Value::Null),
    );
    normalized.insert(
        "sections".to_string(),
        entry.get("sections").cloned().unwrap_or(Value::Null),
    );
    normalized.insert(
        "tool_quality_assessments".to_string(),
        entry
            .get("tool_quality_assessments")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new())),
    );
    normalized.insert(
        "turn_count".to_string(),
        entry.get("turn_count").cloned().unwrap_or_else(|| json!(0)),
    );

    if let Some(created_at) = entry.get("created_at") {
        let normalized_created_at = match created_at {
            Value::String(value) => Value::String(normalize_bridge_created_at(value)),
            other => other.clone(),
        };
        normalized.insert("created_at".to_string(), normalized_created_at);
    }

    Some(normalized)
}

pub fn resolve_turn_identifiers(
    messages: &[Value],
    has_tool_results: bool,
    prev_entry: Option<&mut Map<String, Value>>,
    new_turn_chain_id: &str,
    new_user_query_event_id: &str,
) -> (String, String) {
    let has_new_user_query = messages.iter().any(|message| {
        message
            .get("role")
            .and_then(Value::as_str)
            .map(|role| role == "user")
            .unwrap_or(false)
    });

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

fn normalize_bridge_created_at(created_at: &str) -> String {
    let trimmed = created_at.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    chrono::DateTime::parse_from_rfc3339(trimmed)
        .map(|dt| {
            dt.with_timezone(&chrono::Utc)
                .to_rfc3339()
                .replace("+00:00", "Z")
        })
        .ok()
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S")
                .ok()
                .map(|naive| {
                    chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(naive, chrono::Utc)
                        .to_rfc3339()
                        .replace("+00:00", "Z")
                })
        })
        .unwrap_or_else(|| trimmed.to_string())
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

    // --- normalize_bridge_created_at ---

    #[test]
    fn created_at_empty() {
        assert_eq!(normalize_bridge_created_at(""), "");
    }

    #[test]
    fn created_at_whitespace() {
        assert_eq!(normalize_bridge_created_at("   "), "");
    }

    #[test]
    fn created_at_rfc3339_with_offset() {
        let result = normalize_bridge_created_at("2025-06-15T10:00:00+08:00");
        assert_eq!(result, "2025-06-15T02:00:00Z");
    }

    #[test]
    fn created_at_rfc3339_utc() {
        let result = normalize_bridge_created_at("2025-06-15T10:00:00Z");
        assert_eq!(result, "2025-06-15T10:00:00Z");
    }

    #[test]
    fn created_at_naive_datetime() {
        let result = normalize_bridge_created_at("2025-06-15T10:00:00");
        assert_eq!(result, "2025-06-15T10:00:00Z");
    }

    #[test]
    fn created_at_invalid_passthrough() {
        assert_eq!(normalize_bridge_created_at("not-a-date"), "not-a-date");
    }

    // --- normalize_bridge_cache_entry ---

    #[test]
    fn cache_entry_no_seed_fields() {
        let entry = Map::from_iter([("random".to_string(), json!("value"))]);
        assert!(normalize_bridge_cache_entry(&entry).is_none());
    }

    #[test]
    fn cache_entry_with_created_at_only() {
        let entry = Map::from_iter([("created_at".to_string(), json!("2025-01-01T00:00:00Z"))]);
        let norm = normalize_bridge_cache_entry(&entry).unwrap();
        assert!(norm["history"].is_null());
        assert_eq!(norm["turn_count"].as_i64().unwrap(), 0);
        assert_eq!(norm["created_at"].as_str().unwrap(), "2025-01-01T00:00:00Z");
    }

    #[test]
    fn cache_entry_with_history_field() {
        let entry = Map::from_iter([("history".to_string(), json!([{"role": "user"}]))]);
        let norm = normalize_bridge_cache_entry(&entry).unwrap();
        assert_eq!(norm["history"].as_array().unwrap().len(), 1);
        assert!(norm.get("created_at").is_none());
    }

    #[test]
    fn cache_entry_normalizes_created_at_offset() {
        let entry =
            Map::from_iter([("created_at".to_string(), json!("2025-01-01T08:00:00+08:00"))]);
        let norm = normalize_bridge_cache_entry(&entry).unwrap();
        assert_eq!(norm["created_at"].as_str().unwrap(), "2025-01-01T00:00:00Z");
    }

    #[test]
    fn cache_entry_tool_quality_defaults_empty_array() {
        let entry = Map::from_iter([("turn_count".to_string(), json!(5))]);
        let norm = normalize_bridge_cache_entry(&entry).unwrap();
        assert!(
            norm["tool_quality_assessments"]
                .as_array()
                .unwrap()
                .is_empty()
        );
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
}
