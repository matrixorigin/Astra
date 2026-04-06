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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explain_minimal() {
        let event = build_explain_event(0, None, None, 0, 0, None, vec![], None, None, None);
        assert_eq!(event["type"].as_str().unwrap(), "explain");
        assert_eq!(event["total_ms"].as_i64().unwrap(), 0);
        assert!(event["prompt_tokens"].is_null());
        assert!(event["completion_tokens"].is_null());
        assert!(event["tool_selection"].is_null());
        assert!(event["tool_selection_fallback"].is_null());
        assert!(event["steps"].as_array().unwrap().is_empty());
        assert!(event.get("memory").is_none());
        assert!(event.get("routing").is_none());
        assert!(event.get("auxiliary_llm_calls").is_none());
    }

    #[test]
    fn explain_with_tokens() {
        let event = build_explain_event(150, Some(1000), Some(500), 5, 20, None, vec![], None, None, None);
        assert_eq!(event["total_ms"].as_i64().unwrap(), 150);
        assert_eq!(event["prompt_tokens"].as_i64().unwrap(), 1000);
        assert_eq!(event["completion_tokens"].as_i64().unwrap(), 500);
        assert_eq!(event["tools_selected"].as_u64().unwrap(), 5);
        assert_eq!(event["tools_available"].as_u64().unwrap(), 20);
    }

    #[test]
    fn explain_with_optional_fields() {
        let event = build_explain_event(
            100,
            None,
            None,
            0,
            0,
            Some(json!({"method": "scoring"})),
            vec![json!("step1")],
            Some(json!({"recall": 3})),
            Some(json!({"model": "gpt-4"})),
            Some(vec![json!({"type": "aux"})]),
        );
        assert_eq!(event["memory"]["recall"].as_i64().unwrap(), 3);
        assert_eq!(event["routing"]["model"].as_str().unwrap(), "gpt-4");
        assert_eq!(event["auxiliary_llm_calls"].as_array().unwrap().len(), 1);
        assert_eq!(event["steps"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn explain_negative_total_ms() {
        let event = build_explain_event(-10, None, None, 0, 0, None, vec![], None, None, None);
        assert_eq!(event["total_ms"].as_i64().unwrap(), -10);
    }
}
