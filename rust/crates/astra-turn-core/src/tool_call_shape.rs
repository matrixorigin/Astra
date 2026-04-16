use serde_json::Value;

pub fn tool_call_name(tool_call: &Value) -> Option<&str> {
    tool_call.get("name").and_then(Value::as_str).or_else(|| {
        tool_call
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
    })
}

pub fn tool_call_arguments_value(tool_call: &Value) -> Value {
    tool_call
        .get("arguments")
        .or_else(|| tool_call.get("args"))
        .cloned()
        .or_else(|| {
            tool_call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        })
        .unwrap_or_else(|| serde_json::json!({}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_call_name_supports_canonical_and_legacy_shapes() {
        let canonical = json!({
            "id": "call_1",
            "type": "function",
            "function": {"name": "bash", "arguments": "{}"}
        });
        let legacy = json!({
            "id": "call_2",
            "name": "grep",
            "arguments": {"pattern": "foo"}
        });
        assert_eq!(tool_call_name(&canonical), Some("bash"));
        assert_eq!(tool_call_name(&legacy), Some("grep"));
    }

    #[test]
    fn tool_call_arguments_value_parses_canonical_string_arguments() {
        let canonical = json!({
            "id": "call_1",
            "type": "function",
            "function": {"name": "delegate", "arguments": "{\"task\":\"review\"}"}
        });
        assert_eq!(
            tool_call_arguments_value(&canonical),
            json!({"task":"review"})
        );
    }
}
