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
