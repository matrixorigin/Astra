use serde_json::{Map, Value};

/// Returns whether a message structurally invokes the memory tool.
///
/// Observer input is a knowledge-extraction surface, not a replay of the
/// agent's control transcript.  Memory operations are already persisted by
/// the memory tool itself; feeding their tool call/result/confirmation back
/// into the observer creates active semantic echoes.  Keep this predicate
/// deliberately structural: never classify ordinary assistant prose by
/// matching words such as "stored" or "deleted".
pub fn is_memory_tool_message(message: &Value) -> bool {
    let role = message.get("role").and_then(Value::as_str);

    if role == Some("tool") && message.get("name").and_then(Value::as_str) == Some("memory") {
        return true;
    }

    if role == Some("assistant")
        && let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array)
        && tool_calls.iter().any(|tool_call| {
            tool_call
                .get("function")
                .and_then(Value::as_object)
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                == Some("memory")
        })
    {
        return true;
    }

    role == Some("assistant")
        && message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|blocks| {
                blocks.iter().any(|block| {
                    block.get("type").and_then(Value::as_str) == Some("tool_use")
                        && block.get("name").and_then(Value::as_str) == Some("memory")
                })
            })
}

fn is_user_turn_start(message: &Value) -> bool {
    message.get("role").and_then(Value::as_str) == Some("user")
        && !message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|blocks| {
                blocks
                    .iter()
                    .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
            })
}

/// Remove complete user-turn segments containing a structural memory
/// tool-use from the knowledge-extraction history.
///
/// A segment is bounded by real user messages; Anthropic `tool_result` user
/// messages do not open a new segment.  Dropping the whole segment prevents
/// the operation request, its tool result, and the assistant confirmation
/// from being re-learned as independent memories.  Other turns remain
/// untouched, including ordinary assistant text and non-memory knowledge.
pub fn filter_memory_operation_turns(messages: &[Value]) -> Vec<Value> {
    if messages.is_empty() {
        return Vec::new();
    }

    let mut filtered = Vec::with_capacity(messages.len());
    let mut segment_start = 0usize;

    for (index, message) in messages.iter().enumerate() {
        if index > segment_start && is_user_turn_start(message) {
            let segment = &messages[segment_start..index];
            if !segment.iter().any(is_memory_tool_message) {
                filtered.extend_from_slice(segment);
            }
            segment_start = index;
        }
    }

    let segment = &messages[segment_start..];
    if !segment.iter().any(is_memory_tool_message) {
        filtered.extend_from_slice(segment);
    }

    filtered
}

pub fn should_run_observer(full_text: &str, has_tool_calls: bool) -> bool {
    !full_text.is_empty() && !has_tool_calls
}

pub fn build_observer_messages(
    user_content: Option<&str>,
    full_text: &str,
) -> Vec<Map<String, Value>> {
    let mut messages = Vec::new();
    if let Some(user_content) = user_content.filter(|content| !content.is_empty()) {
        messages.push(Map::from_iter([
            ("role".to_string(), Value::from("user")),
            ("content".to_string(), Value::from(user_content)),
        ]));
    }
    messages.push(Map::from_iter([
        ("role".to_string(), Value::from("assistant")),
        ("content".to_string(), Value::from(full_text)),
    ]));
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_run_text_no_calls() {
        assert!(should_run_observer("hello", false));
    }

    #[test]
    fn should_run_empty_text() {
        assert!(!should_run_observer("", false));
    }

    #[test]
    fn should_run_has_tool_calls() {
        assert!(!should_run_observer("hello", true));
    }

    #[test]
    fn observer_messages_with_user() {
        let msgs = build_observer_messages(Some("question"), "answer");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"].as_str().unwrap(), "user");
        assert_eq!(msgs[1]["role"].as_str().unwrap(), "assistant");
    }

    #[test]
    fn observer_messages_no_user() {
        let msgs = build_observer_messages(None, "answer");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"].as_str().unwrap(), "assistant");
    }

    #[test]
    fn observer_messages_empty_user_content() {
        let msgs = build_observer_messages(Some(""), "answer");
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn memory_operation_turn_is_excluded_without_matching_confirmation_prose() {
        let messages = vec![
            serde_json::json!({"role":"user","content":"Remember the launch date."}),
            serde_json::json!({
                "role":"assistant",
                "tool_calls":[{"function":{"name":"memory","arguments":"{\"action\":\"remember\"}"}}]
            }),
            serde_json::json!({"role":"tool","name":"memory","content":"{\"memory_id\":\"m1\"}"}),
            serde_json::json!({"role":"assistant","content":"Stored. Memory m1 is active."}),
            serde_json::json!({"role":"user","content":"Rust uses ownership."}),
            serde_json::json!({"role":"assistant","content":"Ownership prevents data races."}),
        ];

        let filtered = filter_memory_operation_turns(&messages);
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0]["content"], "Rust uses ownership.");
        assert_eq!(filtered[1]["content"], "Ownership prevents data races.");
    }

    #[test]
    fn recall_and_anthropic_memory_tool_use_are_excluded_fail_closed() {
        let messages = vec![
            serde_json::json!({"role":"user","content":"Recall the project date."}),
            serde_json::json!({
                "role":"assistant",
                "content":[{"type":"tool_use","name":"memory","input":{"action":"recall"}}]
            }),
            serde_json::json!({
                "role":"user",
                "content":[{"type":"tool_result","tool_use_id":"m1","content":"..."}]
            }),
            serde_json::json!({"role":"assistant","content":"The project date is June 15th."}),
            serde_json::json!({"role":"user","content":"The repository is Rust."}),
        ];

        let filtered = filter_memory_operation_turns(&messages);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["content"], "The repository is Rust.");
    }

    #[test]
    fn ordinary_confirmation_prose_without_memory_tool_use_is_preserved() {
        let messages = vec![
            serde_json::json!({"role":"user","content":"Tell me how this works."}),
            serde_json::json!({"role":"assistant","content":"Stored as an example in the explanation."}),
        ];

        let filtered = filter_memory_operation_turns(&messages);
        assert_eq!(filtered, messages);
    }

    #[test]
    fn memory_name_without_a_tool_message_shape_is_not_filtered() {
        let messages = vec![
            serde_json::json!({"role":"user","content":"Explain memory safety."}),
            serde_json::json!({
                "role":"assistant",
                "name":"memory",
                "content":"Memory safety prevents data races."
            }),
        ];

        assert_eq!(filter_memory_operation_turns(&messages), messages);
    }
}
