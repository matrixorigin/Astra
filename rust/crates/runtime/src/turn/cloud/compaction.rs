use serde_json::Value;

pub fn compact_cloud_loop_messages(
    messages: &[Value],
    budget_chars: usize,
    keep_chars: usize,
) -> Vec<Value> {
    let total_chars = messages
        .iter()
        .map(|message| {
            message
                .get("content")
                .and_then(Value::as_str)
                .map(|content| content.chars().count())
                .unwrap_or(0)
        })
        .sum::<usize>();
    if total_chars <= budget_chars {
        return messages.to_vec();
    }

    let mut compacted = messages.to_vec();
    let tool_indices = compacted
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            (message.get("role").and_then(Value::as_str) == Some("tool")).then_some(index)
        })
        .collect::<Vec<_>>();
    let compact_limit = tool_indices.len().saturating_sub(1);
    for index in tool_indices.into_iter().take(compact_limit) {
        let content = compacted[index]
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let truncated = content.chars().take(keep_chars).collect::<String>();
        if content.chars().count() > keep_chars {
            compacted[index]["content"] =
                Value::String(truncated + "\n...[compacted for context budget]");
        }
    }
    compacted
}
