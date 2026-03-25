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
