use serde_json::Value;

pub fn compact_cloud_loop_history(
    history: &[Value],
    keep_chars: usize,
    keep_recent: usize,
) -> Vec<Value> {
    let mut compacted = history.to_vec();
    let tool_indices = compacted
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            (message.get("role").and_then(Value::as_str) == Some("tool")).then_some(index)
        })
        .collect::<Vec<_>>();
    let compact_limit = tool_indices.len().saturating_sub(keep_recent);
    for index in tool_indices.into_iter().take(compact_limit) {
        let content = compacted[index]
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if content.chars().count() > keep_chars {
            let truncated = content.chars().take(keep_chars).collect::<String>();
            compacted[index]["content"] = Value::String(truncated + "\n...[compacted]");
        }
    }
    compacted
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_msg(content: &str) -> Value {
        json!({"role": "tool", "content": content})
    }

    #[test]
    fn empty_history() {
        let result = compact_cloud_loop_history(&[], 100, 2);
        assert!(result.is_empty());
    }

    #[test]
    fn no_tool_messages_unchanged() {
        let history = vec![json!({"role": "assistant", "content": "hi"})];
        let result = compact_cloud_loop_history(&history, 10, 0);
        assert_eq!(result, history);
    }

    #[test]
    fn short_content_not_compacted() {
        let history = vec![tool_msg("short")];
        let result = compact_cloud_loop_history(&history, 100, 0);
        assert_eq!(result[0]["content"].as_str().unwrap(), "short");
    }

    #[test]
    fn long_content_compacted() {
        let long = "a".repeat(200);
        let history = vec![tool_msg(&long)];
        let result = compact_cloud_loop_history(&history, 50, 0);
        let content = result[0]["content"].as_str().unwrap();
        assert!(content.ends_with("...[compacted]"));
        assert!(content.len() < 200);
    }

    #[test]
    fn keep_recent_preserves_last_n() {
        let history = vec![
            tool_msg(&"a".repeat(200)),
            tool_msg(&"b".repeat(200)),
            tool_msg(&"c".repeat(200)),
        ];
        let result = compact_cloud_loop_history(&history, 50, 2);
        // First tool message compacted, last 2 preserved
        assert!(
            result[0]["content"]
                .as_str()
                .unwrap()
                .contains("[compacted]")
        );
        assert!(
            !result[1]["content"]
                .as_str()
                .unwrap()
                .contains("[compacted]")
        );
        assert!(
            !result[2]["content"]
                .as_str()
                .unwrap()
                .contains("[compacted]")
        );
    }

    #[test]
    fn non_tool_messages_not_compacted() {
        let history = vec![
            json!({"role": "assistant", "content": "a".repeat(200)}),
            tool_msg(&"b".repeat(200)),
        ];
        let result = compact_cloud_loop_history(&history, 50, 0);
        // assistant message untouched
        assert!(
            !result[0]["content"]
                .as_str()
                .unwrap()
                .contains("[compacted]")
        );
        // tool message compacted
        assert!(
            result[1]["content"]
                .as_str()
                .unwrap()
                .contains("[compacted]")
        );
    }
}
