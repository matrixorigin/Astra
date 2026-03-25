use serde_json::{Map, Value, json};

#[allow(clippy::too_many_arguments)]
pub fn build_explain_event(
    total_ms: i64,
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
    tools_selected: usize,
    tools_available: usize,
    tool_selection: Option<Value>,
    steps: Vec<Value>,
    memory: Option<Value>,
    routing: Option<Value>,
    auxiliary_llm_calls: Option<Vec<Value>>,
) -> Map<String, Value> {
    let mut explain_event = Map::from_iter([
        ("type".to_string(), Value::String("explain".to_string())),
        ("total_ms".to_string(), json!(total_ms)),
        (
            "prompt_tokens".to_string(),
            prompt_tokens
                .map(|value| json!(value))
                .unwrap_or(Value::Null),
        ),
        (
            "completion_tokens".to_string(),
            completion_tokens
                .map(|value| json!(value))
                .unwrap_or(Value::Null),
        ),
        ("tools_selected".to_string(), json!(tools_selected)),
        ("tools_available".to_string(), json!(tools_available)),
        (
            "tool_selection".to_string(),
            tool_selection.unwrap_or(Value::Null),
        ),
        ("tool_selection_fallback".to_string(), Value::Null),
        ("steps".to_string(), Value::Array(steps)),
    ]);
    if let Some(memory) = memory {
        explain_event.insert("memory".to_string(), memory);
    }
    if let Some(routing) = routing {
        explain_event.insert("routing".to_string(), routing);
    }
    if let Some(auxiliary_llm_calls) = auxiliary_llm_calls {
        explain_event.insert(
            "auxiliary_llm_calls".to_string(),
            Value::Array(auxiliary_llm_calls),
        );
    }
    explain_event
}
