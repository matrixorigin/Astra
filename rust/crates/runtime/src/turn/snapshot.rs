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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- build_session_history_snapshot ---

    #[test]
    fn snapshot_empty_history() {
        assert!(build_session_history_snapshot(&[], 100).is_empty());
    }

    #[test]
    fn snapshot_non_tool_messages_unchanged() {
        let msg = Map::from_iter([
            ("role".to_string(), json!("user")),
            ("content".to_string(), json!("long content here")),
        ]);
        let result = build_session_history_snapshot(&[msg], 5);
        assert_eq!(result[0]["content"].as_str().unwrap(), "long content here");
    }

    #[test]
    fn snapshot_tool_content_truncated() {
        let msg = Map::from_iter([
            ("role".to_string(), json!("tool")),
            ("content".to_string(), json!("abcdefghij")),
        ]);
        let result = build_session_history_snapshot(&[msg], 5);
        assert_eq!(result[0]["content"].as_str().unwrap(), "abcde [truncated]");
    }

    #[test]
    fn snapshot_tool_content_within_limit() {
        let msg = Map::from_iter([
            ("role".to_string(), json!("tool")),
            ("content".to_string(), json!("abc")),
        ]);
        let result = build_session_history_snapshot(&[msg], 10);
        assert_eq!(result[0]["content"].as_str().unwrap(), "abc");
    }

    #[test]
    fn snapshot_tool_unicode_truncation() {
        let msg = Map::from_iter([
            ("role".to_string(), json!("tool")),
            ("content".to_string(), json!("你好世界测试")),
        ]);
        let result = build_session_history_snapshot(&[msg], 3);
        assert!(result[0]["content"].as_str().unwrap().starts_with("你好世"));
    }

    #[test]
    fn snapshot_tool_missing_content() {
        let msg = Map::from_iter([("role".to_string(), json!("tool"))]);
        let result = build_session_history_snapshot(&[msg], 5);
        assert_eq!(result[0]["content"].as_str().unwrap(), "");
    }

    // --- should_persist_session_history_snapshot ---

    #[test]
    fn should_persist_all_true_interval_match() {
        assert!(should_persist_session_history_snapshot(true, true, 10, 5));
    }

    #[test]
    fn should_persist_no_history() {
        assert!(!should_persist_session_history_snapshot(false, true, 10, 5));
    }

    #[test]
    fn should_persist_no_user_content() {
        assert!(!should_persist_session_history_snapshot(true, false, 10, 5));
    }

    #[test]
    fn should_persist_turn_count_zero() {
        assert!(!should_persist_session_history_snapshot(true, true, 0, 5));
    }

    #[test]
    fn should_persist_not_multiple() {
        assert!(!should_persist_session_history_snapshot(true, true, 7, 5));
    }
}
