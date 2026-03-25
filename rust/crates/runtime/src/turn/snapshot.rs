use serde_json::{Map, Value};

pub fn build_session_history_snapshot(
    history: &[Map<String, Value>],
    tool_content_limit: usize,
) -> Vec<Map<String, Value>> {
    history
        .iter()
        .map(|message| {
            let mut cloned = message.clone();
            if cloned.get("role").and_then(Value::as_str) == Some("tool") {
                let content = cloned
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let truncated = if content.chars().count() > tool_content_limit {
                    format!(
                        "{} [truncated]",
                        content.chars().take(tool_content_limit).collect::<String>()
                    )
                } else {
                    content
                };
                cloned.insert("content".to_string(), Value::from(truncated));
            }
            cloned
        })
        .collect()
}

pub fn should_persist_session_history_snapshot(
    has_history: bool,
    has_user_content: bool,
    turn_count: usize,
    snapshot_turn_interval: usize,
) -> bool {
    has_history
        && has_user_content
        && turn_count > 0
        && turn_count.is_multiple_of(snapshot_turn_interval)
}
