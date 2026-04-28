use serde_json::{Map, Value, json};

#[allow(clippy::too_many_arguments)]
pub fn build_turn_hook_args(
    user_id: &str,
    session_id: &str,
    messages: &[Value],
    tool_results: &[Value],
    full_text: &str,
    tool_calls: &[Value],
    context_capture_id: Option<&str>,
    model_used: Option<&str>,
    agent_id: Option<&str>,
    parent_event_id: Option<&str>,
    turn_count: i64,
    session_start: Option<Value>,
    run_hook_db_writes: bool,
    run_observer: bool,
    run_implicit_feedback: bool,
    run_reflection_learning: bool,
) -> Map<String, Value> {
    Map::from_iter([
        ("user_id".to_string(), Value::String(user_id.to_string())),
        (
            "session_id".to_string(),
            Value::String(session_id.to_string()),
        ),
        ("messages".to_string(), Value::Array(messages.to_vec())),
        (
            "tool_results".to_string(),
            Value::Array(tool_results.to_vec()),
        ),
        (
            "full_text".to_string(),
            Value::String(full_text.to_string()),
        ),
        ("tool_calls".to_string(), Value::Array(tool_calls.to_vec())),
        (
            "context_capture_id".to_string(),
            context_capture_id.map(|v| json!(v)).unwrap_or(Value::Null),
        ),
        (
            "model_used".to_string(),
            model_used.map(|v| json!(v)).unwrap_or(Value::Null),
        ),
        (
            "agent_id".to_string(),
            agent_id.map(|v| json!(v)).unwrap_or(Value::Null),
        ),
        (
            "parent_event_id".to_string(),
            parent_event_id.map(|v| json!(v)).unwrap_or(Value::Null),
        ),
        ("turn_count".to_string(), json!(turn_count)),
        (
            "session_start".to_string(),
            session_start.unwrap_or(Value::Null),
        ),
        (
            "run_hook_db_writes".to_string(),
            Value::Bool(run_hook_db_writes),
        ),
        ("run_observer".to_string(), Value::Bool(run_observer)),
        (
            "run_implicit_feedback".to_string(),
            Value::Bool(run_implicit_feedback),
        ),
        (
            "run_reflection_learning".to_string(),
            Value::Bool(run_reflection_learning),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- build_turn_hook_args ---

    #[test]
    fn hook_args_minimal() {
        let args = build_turn_hook_args(
            "u1",
            "s1",
            &[],
            &[],
            "",
            &[],
            None,
            None,
            None,
            None,
            0,
            None,
            false,
            false,
            false,
            false,
        );
        assert_eq!(args["user_id"].as_str().unwrap(), "u1");
        assert!(args["context_capture_id"].is_null());
        assert_eq!(args["turn_count"].as_i64().unwrap(), 0);
        assert!(!args["run_hook_db_writes"].as_bool().unwrap());
    }

    #[test]
    fn hook_args_with_all_options() {
        let args = build_turn_hook_args(
            "u1",
            "s1",
            &[json!({"role": "user"})],
            &[],
            "response",
            &[json!({"id": "tc1"})],
            Some("cap1"),
            Some("gpt-4"),
            Some("agent1"),
            Some("evt1"),
            5,
            Some(json!("2025-01-01")),
            true,
            true,
            true,
            true,
        );
        assert_eq!(args["context_capture_id"].as_str().unwrap(), "cap1");
        assert_eq!(args["model_used"].as_str().unwrap(), "gpt-4");
        assert_eq!(args["turn_count"].as_i64().unwrap(), 5);
        assert!(args["run_hook_db_writes"].as_bool().unwrap());
        assert!(args["run_observer"].as_bool().unwrap());
    }
}
