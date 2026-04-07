use serde_json::{Map, Value, json};

pub fn build_cached_assistant_message(
    full_text: &str,
    tool_calls: &[Value],
    reasoning_content: &str,
) -> Map<String, Value> {
    let mut assistant_message = Map::from_iter([
        ("role".to_string(), Value::String("assistant".to_string())),
        ("content".to_string(), Value::String(full_text.to_string())),
    ]);
    if !tool_calls.is_empty() {
        assistant_message.insert("tool_calls".to_string(), Value::Array(tool_calls.to_vec()));
    }
    if !reasoning_content.is_empty() {
        assistant_message.insert(
            "reasoning_content".to_string(),
            Value::String(reasoning_content.to_string()),
        );
    }
    assistant_message
}

#[allow(clippy::too_many_arguments)]
pub fn build_persist_thread_args(
    user_id: &str,
    session_id: &str,
    messages: &[Value],
    tool_results: &[Value],
    full_text: &str,
    cloud_tool_calls: &[Value],
    edge_tool_calls: &[Value],
    reasoning_content: &str,
    cloud_tool_results: &[Value],
    context_capture_id: Option<&str>,
    model_used: Option<&str>,
    token_usage: Option<Value>,
    llm_params: Option<Value>,
    history: &[Value],
    turn_count: i64,
    agent_id: Option<&str>,
    turn_chain_id: Option<&str>,
    user_query_event_id: Option<&str>,
    session_start: Option<Value>,
    tool_quality_assessments: Option<Value>,
    routing_meta: Option<Value>,
    run_request_response_persist: bool,
    run_snapshot_link_update: bool,
    run_tool_event_persist: bool,
    run_auxiliary_event_persist: bool,
    run_session_activity: bool,
    run_turn_hooks: bool,
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
        (
            "tool_calls".to_string(),
            Value::Array(
                cloud_tool_calls
                    .iter()
                    .chain(edge_tool_calls.iter())
                    .cloned()
                    .collect(),
            ),
        ),
        (
            "reasoning_content".to_string(),
            Value::String(reasoning_content.to_string()),
        ),
        (
            "cloud_tool_results".to_string(),
            if cloud_tool_results.is_empty() {
                Value::Null
            } else {
                Value::Array(cloud_tool_results.to_vec())
            },
        ),
        (
            "context_capture_id".to_string(),
            context_capture_id.map(|v| json!(v)).unwrap_or(Value::Null),
        ),
        (
            "model_used".to_string(),
            model_used.map(|v| json!(v)).unwrap_or(Value::Null),
        ),
        (
            "token_usage".to_string(),
            token_usage.unwrap_or(Value::Null),
        ),
        ("llm_params".to_string(), llm_params.unwrap_or(Value::Null)),
        ("history".to_string(), Value::Array(history.to_vec())),
        ("turn_count".to_string(), json!(turn_count)),
        (
            "agent_id".to_string(),
            agent_id.map(|v| json!(v)).unwrap_or(Value::Null),
        ),
        (
            "turn_chain_id".to_string(),
            turn_chain_id.map(|v| json!(v)).unwrap_or(Value::Null),
        ),
        (
            "user_query_event_id".to_string(),
            user_query_event_id.map(|v| json!(v)).unwrap_or(Value::Null),
        ),
        (
            "session_start".to_string(),
            session_start.unwrap_or(Value::Null),
        ),
        (
            "tool_quality_assessments".to_string(),
            tool_quality_assessments.unwrap_or(Value::Null),
        ),
        (
            "routing_meta".to_string(),
            match routing_meta {
                Some(Value::Object(map)) if map.is_empty() => Value::Null,
                Some(value) => value,
                None => Value::Null,
            },
        ),
        (
            "run_session_activity".to_string(),
            Value::Bool(run_session_activity),
        ),
        (
            "run_request_response_persist".to_string(),
            Value::Bool(run_request_response_persist),
        ),
        (
            "run_snapshot_link_update".to_string(),
            Value::Bool(run_snapshot_link_update),
        ),
        (
            "run_auxiliary_event_persist".to_string(),
            Value::Bool(run_auxiliary_event_persist),
        ),
        (
            "run_tool_event_persist".to_string(),
            Value::Bool(run_tool_event_persist),
        ),
        ("run_turn_hooks".to_string(), Value::Bool(run_turn_hooks)),
    ])
}

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

    // --- build_cached_assistant_message ---

    #[test]
    fn cached_msg_basic() {
        let msg = build_cached_assistant_message("hello", &[], "");
        assert_eq!(msg["role"].as_str().unwrap(), "assistant");
        assert_eq!(msg["content"].as_str().unwrap(), "hello");
        assert!(msg.get("tool_calls").is_none());
        assert!(msg.get("reasoning_content").is_none());
    }

    #[test]
    fn cached_msg_with_tool_calls() {
        let calls = vec![json!({"id": "tc1"})];
        let msg = build_cached_assistant_message("", &calls, "");
        assert_eq!(msg["tool_calls"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn cached_msg_with_reasoning() {
        let msg = build_cached_assistant_message("hi", &[], "thought process");
        assert_eq!(
            msg["reasoning_content"].as_str().unwrap(),
            "thought process"
        );
    }

    #[test]
    fn cached_msg_empty_tool_calls_excluded() {
        let msg = build_cached_assistant_message("hi", &[], "");
        assert!(msg.get("tool_calls").is_none());
    }

    // --- build_persist_thread_args ---

    #[test]
    fn persist_thread_minimal() {
        let args = build_persist_thread_args(
            "u1",
            "s1",
            &[],
            &[],
            "",
            &[],
            &[],
            "",
            &[],
            None,
            None,
            None,
            None,
            &[],
            0,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        );
        assert_eq!(args["user_id"].as_str().unwrap(), "u1");
        assert_eq!(args["session_id"].as_str().unwrap(), "s1");
        assert!(args["context_capture_id"].is_null());
        assert!(args["cloud_tool_results"].is_null());
        assert!(args["routing_meta"].is_null());
    }

    #[test]
    fn persist_thread_merges_tool_calls() {
        let cloud = vec![json!({"id": "c1"})];
        let edge = vec![json!({"id": "e1"})];
        let args = build_persist_thread_args(
            "u1",
            "s1",
            &[],
            &[],
            "",
            &cloud,
            &edge,
            "",
            &[],
            None,
            None,
            None,
            None,
            &[],
            0,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        );
        let calls = args["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn persist_thread_empty_routing_meta_is_null() {
        let args = build_persist_thread_args(
            "u1",
            "s1",
            &[],
            &[],
            "",
            &[],
            &[],
            "",
            &[],
            None,
            None,
            None,
            None,
            &[],
            0,
            None,
            None,
            None,
            None,
            None,
            Some(Value::Object(Map::new())),
            false,
            false,
            false,
            false,
            false,
            false,
        );
        assert!(args["routing_meta"].is_null());
    }

    #[test]
    fn persist_thread_nonempty_routing_meta() {
        let args = build_persist_thread_args(
            "u1",
            "s1",
            &[],
            &[],
            "",
            &[],
            &[],
            "",
            &[],
            None,
            None,
            None,
            None,
            &[],
            0,
            None,
            None,
            None,
            None,
            None,
            Some(json!({"model": "gpt-4"})),
            false,
            false,
            false,
            false,
            false,
            false,
        );
        assert_eq!(args["routing_meta"]["model"].as_str().unwrap(), "gpt-4");
    }

    #[test]
    fn persist_thread_cloud_results_nonempty() {
        let cloud_results = vec![json!({"tool_call_id": "tc1"})];
        let args = build_persist_thread_args(
            "u1",
            "s1",
            &[],
            &[],
            "",
            &[],
            &[],
            "",
            &cloud_results,
            None,
            None,
            None,
            None,
            &[],
            0,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        );
        assert_eq!(args["cloud_tool_results"].as_array().unwrap().len(), 1);
    }

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
