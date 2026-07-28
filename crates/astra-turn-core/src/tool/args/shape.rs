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

    let top_level = match (tool_call.get("arguments"), tool_call.get("args")) {
        (Some(arguments), Some(args)) => {
            let arguments = parse(arguments)?;
            let args = parse(args)?;
            if arguments != args {
                return Err("top-level arguments and args conflict");
            }
            Some(arguments)
        }
        (Some(arguments), None) | (None, Some(arguments)) => Some(parse(arguments)?),
        (None, None) => None,
    };
    let nested = tool_call
        .get("function")
        .and_then(|function| function.get("arguments"))
        .map(parse)
        .transpose()?;

    match (top_level, nested) {
        (Some(top_level), Some(nested)) if top_level != nested => {
            Err("top-level and function arguments conflict")
        }
        (Some(arguments), _) | (None, Some(arguments)) => Ok(arguments),
        (None, None) => Err("arguments are missing"),
    }
}

/// Normalize an admitted tool call into Astra's canonical OpenAI-compatible
/// execution shape. Mixed provider shapes are accepted only when every
/// argument representation resolves to the same JSON object.
pub fn canonicalize_tool_call_for_execution(tool_call: &Value) -> Result<Value, &'static str> {
    let top_level_name = tool_call
        .get("name")
        .and_then(Value::as_str)
        .and_then(normalize_name);
    let nested_name = tool_call
        .get("function")
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)
        .and_then(normalize_name);
    let name = match (top_level_name, nested_name) {
        (Some(top_level), Some(nested)) if top_level != nested => {
            return Err("top-level and function names conflict");
        }
        (Some(name), _) | (None, Some(name)) => name,
        (None, None) => return Err("tool name is missing"),
    };
    let arguments = parse_tool_call_arguments(tool_call)?;
    let id = tool_call
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
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
    fn strict_argument_parser_accepts_string_and_object_arguments() {
        let string_arguments = json!({
            "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}
        });
        let object_arguments = json!({
            "name": "bash",
            "arguments": {"command": "ls"}
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
    fn strict_argument_parser_accepts_equal_mixed_shapes_and_canonicalizes_once() {
        let mixed = json!({
            "id": "call_1",
            "name": "bash",
            "arguments": {"command": "ls"},
            "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}
        });

        assert_eq!(
            canonicalize_tool_call_for_execution(&mixed).unwrap(),
            json!({
                "id": "call_1",
                "type": "function",
                "function": {
                    "name": "bash",
                    "arguments": "{\"command\":\"ls\"}"
                }
            })
        );
    }

    #[test]
    fn strict_argument_parser_rejects_conflicting_or_malformed_mixed_shapes() {
        let conflicting = json!({
            "id": "call_1",
            "name": "bash",
            "arguments": {"command": "ls"},
            "function": {"name": "bash", "arguments": "{\"command\":\"pwd\"}"}
        });
        let malformed_nested = json!({
            "id": "call_2",
            "name": "bash",
            "arguments": {"command": "ls"},
            "function": {"name": "bash", "arguments": "{\"command\":"}
        });

        assert_eq!(
            parse_tool_call_arguments(&conflicting),
            Err("top-level and function arguments conflict")
        );
        assert_eq!(
            parse_tool_call_arguments(&malformed_nested),
            Err("arguments contain incomplete or invalid JSON")
        );

        let conflicting_name = json!({
            "id": "call_3",
            "name": "bash",
            "arguments": {"command": "ls"},
            "function": {"name": "python", "arguments": "{\"command\":\"ls\"}"}
        });
        assert_eq!(
            canonicalize_tool_call_for_execution(&conflicting_name),
            Err("top-level and function names conflict")
        );
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
