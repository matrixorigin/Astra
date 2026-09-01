use astra_core::canonical_names::normalize_name;
use serde_json::Value;

pub fn tool_call_name(tool_call: &Value) -> Option<&str> {
    if tool_call.get("name").is_some()
        || tool_call.get("arguments").is_some()
        || tool_call.get("args").is_some()
    {
        return None;
    }
    tool_call
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
        .and_then(normalize_name)
}

pub fn canonicalize_tool_call_name_in_place(tool_call: &mut Value) -> Option<String> {
    let name = tool_call_name(tool_call)?.to_string();
    if let Some(function) = tool_call.get_mut("function").and_then(Value::as_object_mut) {
        function.insert("name".to_string(), Value::String(name.clone()));
    }
    Some(name)
}

/// Parse a tool call's arguments without silently replacing malformed input.
///
/// The lenient [`tool_call_arguments_value`] helper remains available for
/// display and legacy classification. Execution boundaries must use this
/// strict parser first so a provider-truncated JSON string cannot become `{}`.
pub fn parse_tool_call_arguments(tool_call: &Value) -> Result<Value, &'static str> {
    fn parse(raw: &Value) -> Result<Value, &'static str> {
        let arguments = match raw {
            Value::Object(_) => raw.clone(),
            Value::String(raw) => serde_json::from_str::<Value>(raw)
                .map_err(|_| "arguments contain incomplete or invalid JSON")?,
            _ => return Err("arguments must be a JSON object"),
        };
        if !arguments.is_object() {
            return Err("arguments must be a JSON object");
        }
        Ok(arguments)
    }

    if tool_call.get("name").is_some()
        || tool_call.get("arguments").is_some()
        || tool_call.get("args").is_some()
    {
        return Err("top-level tool name or arguments are not supported");
    }
    tool_call
        .get("function")
        .and_then(|function| function.get("arguments"))
        .ok_or("arguments are missing")
        .and_then(parse)
}

/// Normalize an admitted tool call into Astra's canonical OpenAI-compatible
/// execution shape. Provider adapters must normalize their native payloads
/// before this boundary; flat and mixed legacy shapes fail closed.
pub fn canonicalize_tool_call_for_execution(tool_call: &Value) -> Result<Value, &'static str> {
    if tool_call.get("name").is_some()
        || tool_call.get("arguments").is_some()
        || tool_call.get("args").is_some()
    {
        return Err("top-level tool name or arguments are not supported");
    }
    if tool_call
        .get("type")
        .is_some_and(|kind| kind.as_str() != Some("function"))
    {
        return Err("tool call type must be function");
    }
    let function = tool_call
        .get("function")
        .and_then(Value::as_object)
        .ok_or("tool function is missing")?;
    if function.get("id").is_some() || function.get("call_id").is_some() {
        return Err("tool call id must be top-level");
    }
    let name = tool_call
        .get("function")
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)
        .and_then(normalize_name)
        .ok_or("tool name is missing")?;
    let arguments = parse_tool_call_arguments(tool_call)?;
    let id = tool_call
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.trim() == *id)
        .ok_or("tool call id is missing")?;
    let arguments =
        serde_json::to_string(&arguments).map_err(|_| "arguments could not be serialized")?;
    Ok(serde_json::json!({
        "id": id,
        "type": "function",
        "function": {
            "name": name,
            "arguments": arguments,
        },
    }))
}

pub fn tool_call_arguments_value(tool_call: &Value) -> Value {
    if tool_call.get("name").is_some()
        || tool_call.get("arguments").is_some()
        || tool_call.get("args").is_some()
    {
        return serde_json::json!({});
    }
    tool_call
        .get("function")
        .and_then(|f| f.get("arguments"))
        .and_then(|arguments| match arguments {
            Value::Object(_) => Some(arguments.clone()),
            Value::String(raw) => serde_json::from_str::<Value>(raw).ok(),
            _ => None,
        })
        .unwrap_or_else(|| serde_json::json!({}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_call_name_accepts_only_canonical_function_shape() {
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
        assert_eq!(tool_call_name(&internal), None);
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
    fn tool_call_name_rejects_mixed_legacy_fields() {
        let mixed = json!({
            "id": "call_1",
            "name": "legacy_name",
            "function": {"name": " canonical_name ", "arguments": "{}"}
        });

        assert_eq!(tool_call_name(&mixed), None);
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
        assert_eq!(canonicalize_tool_call_name_in_place(&mut flat), None);
        assert_eq!(flat["name"], " grep ");
    }

    #[test]
    fn strict_argument_parser_rejects_truncated_json_instead_of_returning_empty_object() {
        let truncated = json!({
            "id": "call_1",
            "type": "function",
            "function": {
                "name": "python",
                "arguments": "{\"code\":\"from docx import Document"
            }
        });

        assert_eq!(
            parse_tool_call_arguments(&truncated),
            Err("arguments contain incomplete or invalid JSON")
        );
        assert_eq!(tool_call_arguments_value(&truncated), json!({}));
    }

    #[test]
    fn strict_argument_parser_accepts_nested_string_and_object_arguments() {
        let string_arguments = json!({
            "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}
        });
        let object_arguments = json!({
            "function": {"name": "bash", "arguments": {"command": "ls"}}
        });

        assert_eq!(
            parse_tool_call_arguments(&string_arguments).unwrap(),
            json!({"command": "ls"})
        );
        assert_eq!(
            parse_tool_call_arguments(&object_arguments).unwrap(),
            json!({"command": "ls"})
        );
    }

    #[test]
    fn strict_argument_parser_rejects_flat_and_mixed_legacy_shapes() {
        let flat = json!({
            "id": "call_0",
            "name": "bash",
            "arguments": {"command": "ls"}
        });
        let mixed = json!({
            "id": "call_1",
            "name": "bash",
            "arguments": {"command": "ls"},
            "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}
        });

        let expected = Err("top-level tool name or arguments are not supported");
        assert_eq!(canonicalize_tool_call_for_execution(&flat), expected);
        assert_eq!(canonicalize_tool_call_for_execution(&mixed), expected);
        assert_eq!(parse_tool_call_arguments(&flat), expected);
        assert_eq!(parse_tool_call_arguments(&mixed), expected);
    }

    #[test]
    fn strict_argument_parser_rejects_malformed_nested_shape() {
        let malformed_nested = json!({
            "id": "call_2",
            "function": {"name": "bash", "arguments": "{\"command\":"}
        });

        assert_eq!(
            parse_tool_call_arguments(&malformed_nested),
            Err("arguments contain incomplete or invalid JSON")
        );
    }

    #[test]
    fn execution_shape_rejects_nested_or_non_exact_ids_and_wrong_type() {
        for malformed in [
            json!({
                "function": {"id": "nested", "name": "bash", "arguments": "{}"}
            }),
            json!({
                "id": " call-1 ",
                "type": "function",
                "function": {"name": "bash", "arguments": "{}"}
            }),
            json!({
                "id": "call-1",
                "type": "custom",
                "function": {"name": "bash", "arguments": "{}"}
            }),
        ] {
            assert!(canonicalize_tool_call_for_execution(&malformed).is_err());
        }
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
