use astra_core::canonical_names::normalize_name;
use serde_json::Value;

pub fn tool_call_name(tool_call: &Value) -> Option<&str> {
    tool_call
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
        .or_else(|| tool_call.get("name").and_then(Value::as_str))
        .and_then(normalize_name)
}

pub fn canonicalize_tool_call_name_in_place(tool_call: &mut Value) -> Option<String> {
    let name = tool_call_name(tool_call)?.to_string();
    if let Some(function) = tool_call.get_mut("function").and_then(Value::as_object_mut) {
        function.insert("name".to_string(), Value::String(name.clone()));
    } else if let Some(object) = tool_call.as_object_mut() {
        object.insert("name".to_string(), Value::String(name.clone()));
    }
    Some(name)
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
    fn tool_call_name_supports_openai_and_internal_shapes() {
        let canonical = json!({
            "id": "call_1",
            "type": "function",
            "function": {"name": "bash", "arguments": "{}"}
        });
        let internal = json!({
            "id": "call_2",
            "name": "grep",
            "arguments": {"pattern": "foo"}
        });
        assert_eq!(tool_call_name(&canonical), Some("bash"));
        assert_eq!(tool_call_name(&internal), Some("grep"));
    }

    #[test]
    fn tool_call_name_trims_and_drops_blank_names() {
        let canonical = json!({
            "id": "call_1",
            "type": "function",
            "function": {"name": " bash ", "arguments": "{}"}
        });
        let internal = json!({
            "id": "call_2",
            "name": "  ",
            "arguments": {"pattern": "foo"}
        });

        assert_eq!(tool_call_name(&canonical), Some("bash"));
        assert_eq!(tool_call_name(&internal), None);
    }

    #[test]
    fn tool_call_name_prefers_function_name_when_both_shapes_exist() {
        let mixed = json!({
            "id": "call_1",
            "name": "legacy_name",
            "function": {"name": " canonical_name ", "arguments": "{}"}
        });

        assert_eq!(tool_call_name(&mixed), Some("canonical_name"));
    }

    #[test]
    fn canonicalize_tool_call_name_in_place_updates_execution_shape() {
        let mut canonical = json!({
            "id": "call_1",
            "type": "function",
            "function": {"name": " bash ", "arguments": "{}"}
        });
        assert_eq!(
            canonicalize_tool_call_name_in_place(&mut canonical).as_deref(),
            Some("bash")
        );
        assert_eq!(canonical["function"]["name"], "bash");

        let mut flat = json!({"id": "call_2", "name": " grep ", "arguments": {}});
        assert_eq!(
            canonicalize_tool_call_name_in_place(&mut flat).as_deref(),
            Some("grep")
        );
        assert_eq!(flat["name"], "grep");
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
