use astra_turn_types::{
    RuntimeMessageDelivery, is_runtime_owned_message, runtime_message_delivery,
};
use serde_json::Value;

pub const INTERNAL_SKILL_AUTO_ROUTE_TOOL_CALL_ID_PREFIX: &str = "skill-auto-route";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PromptFacingRuntimeNormalization {
    pub messages: Vec<Value>,
    pub required_runtime_texts: Vec<String>,
}

/// Remove runtime-owned messages from conversational history and re-route
/// required context to its dedicated lane. Ownership comes exclusively from
/// typed producer metadata; message text is never classified.
pub fn normalize_prompt_facing_runtime_messages(
    messages: Vec<Value>,
) -> PromptFacingRuntimeNormalization {
    let mut normalized = PromptFacingRuntimeNormalization::default();
    for message in messages {
        if is_internal_skill_auto_route_message(&message) {
            continue;
        }

        if let Some(delivery) = runtime_message_delivery(&message) {
            if delivery == RuntimeMessageDelivery::RequiredContext
                && let Some(content) = message.get("content").and_then(Value::as_str)
                && !content.trim().is_empty()
            {
                normalized
                    .required_runtime_texts
                    .push(content.trim().to_string());
            }
            continue;
        }

        normalized.messages.push(message);
    }
    normalized
}

/// Preserve provider tool frames for recovery while excluding messages owned
/// by the runtime and internal skill auto-route roundtrips.
pub fn sanitize_recoverable_runtime_messages(messages: Vec<Value>) -> Vec<Value> {
    sanitize_durable_message_values(
        messages
            .into_iter()
            .filter(|message| {
                !is_runtime_owned_message(message) && !is_internal_skill_auto_route_message(message)
            })
            .collect(),
    )
}

/// Redact a cloned message graph before it crosses a checkpoint/journal
/// boundary. The caller retains the original graph for live provider use.
pub fn sanitize_durable_message_values(mut messages: Vec<Value>) -> Vec<Value> {
    for message in &mut messages {
        sanitize_embedded_assistant_tool_arguments(message);
        let assistant_frame = message.get("role").and_then(Value::as_str) == Some("assistant");
        sanitize_json_except_assistant_tool_arguments(
            message,
            assistant_frame,
            AssistantToolPath::None,
            true,
            None,
        );
    }
    messages
}

/// Apply the generic display boundary while leaving the already-normalized
/// assistant tool-argument string opaque. That string is a nested JSON
/// document: rescanning its serialized form can match the quotes/field name
/// of an inner credential and make the inner document unparsable. Only the
/// exact `assistant → tool_calls[] → function → arguments` string path is
/// skipped; every other same-named field goes through the generic sanitizer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AssistantToolPath {
    None,
    ToolCallsArray,
    ToolCallObject,
    FunctionObject,
}

fn sanitize_json_except_assistant_tool_arguments(
    value: &mut Value,
    assistant_frame: bool,
    path: AssistantToolPath,
    at_message_root: bool,
    object_key: Option<&str>,
) {
    match value {
        Value::String(_) => {
            let mut leaf = std::mem::replace(value, Value::Null);
            if let Some(key) = object_key {
                astra_tools::credential_redaction::redact_credentials_in_json_field(&mut leaf, key);
            } else {
                astra_tools::credential_redaction::redact_credentials_in_json(&mut leaf);
            }
            *value = leaf;
        }
        Value::Array(values) => {
            for child in values {
                let child_path = if path == AssistantToolPath::ToolCallsArray && child.is_object() {
                    AssistantToolPath::ToolCallObject
                } else {
                    AssistantToolPath::None
                };
                sanitize_json_except_assistant_tool_arguments(
                    child,
                    assistant_frame,
                    child_path,
                    false,
                    None,
                );
            }
        }
        Value::Object(values) => {
            for (key, child) in values {
                if assistant_frame
                    && path == AssistantToolPath::FunctionObject
                    && key == "arguments"
                    && child.is_string()
                {
                    continue;
                }
                let next_path = if at_message_root
                    && assistant_frame
                    && key == "tool_calls"
                    && child.is_array()
                {
                    AssistantToolPath::ToolCallsArray
                } else if path == AssistantToolPath::ToolCallObject
                    && key == "function"
                    && child.is_object()
                {
                    AssistantToolPath::FunctionObject
                } else {
                    AssistantToolPath::None
                };
                sanitize_json_except_assistant_tool_arguments(
                    child,
                    assistant_frame,
                    next_path,
                    false,
                    Some(key),
                );
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Assistant tool calls carry a second JSON document in
/// `tool_calls[].function.arguments`. This protocol-specific parse belongs at
/// the message boundary, not in the generic JSON metadata sanitizer. Invalid
/// inner JSON is replaced with a parseable sentinel instead of being copied to
/// a durable checkpoint.
fn sanitize_embedded_assistant_tool_arguments(message: &mut Value) {
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return;
    }
    let Some(tool_calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) else {
        return;
    };
    for tool_call in tool_calls {
        let Some(arguments) = tool_call
            .get("function")
            .and_then(Value::as_object)
            .and_then(|function| function.get("arguments"))
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        let safe_arguments = match serde_json::from_str::<Value>(&arguments) {
            Ok(mut parsed) => {
                astra_tools::credential_redaction::redact_credentials_in_json(&mut parsed);
                serde_json::to_string(&parsed).unwrap_or_else(|_| {
                    r#"{"_astra_redaction":"arguments_unavailable"}"#.to_string()
                })
            }
            Err(_) => r#"{"_astra_redaction":"arguments_unavailable"}"#.to_string(),
        };
        if let Some(arguments_value) = tool_call
            .get_mut("function")
            .and_then(Value::as_object_mut)
            .and_then(|function| function.get_mut("arguments"))
        {
            *arguments_value = Value::String(safe_arguments);
        }
    }
}

fn is_internal_skill_auto_route_message(message: &Value) -> bool {
    if message.get("role").and_then(Value::as_str) == Some("tool") {
        return message
            .get("tool_call_id")
            .and_then(Value::as_str)
            .is_some_and(is_internal_skill_auto_route_tool_call_id);
    }

    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return false;
    }
    let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) else {
        return false;
    };
    !tool_calls.is_empty()
        && tool_calls.iter().all(|tool_call| {
            tool_call
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(is_internal_skill_auto_route_tool_call_id)
        })
}

fn is_internal_skill_auto_route_tool_call_id(id: &str) -> bool {
    id == INTERNAL_SKILL_AUTO_ROUTE_TOOL_CALL_ID_PREFIX
        || id
            .strip_prefix(INTERNAL_SKILL_AUTO_ROUTE_TOOL_CALL_ID_PREFIX)
            .is_some_and(|suffix| suffix.starts_with('-'))
}

#[cfg(test)]
mod tests {
    use astra_turn_types::{RuntimeMessageDelivery, runtime_owned_message};
    use serde_json::json;

    use super::*;

    #[test]
    fn normalization_routes_by_typed_delivery_without_inspecting_text() {
        let ordinary = json!({
            "role": "user",
            "content": "<system-reminder> is part of my literal request",
        });
        let required = runtime_owned_message(
            "user",
            "required payload without a magic prefix",
            RuntimeMessageDelivery::RequiredContext,
        );
        let ephemeral = runtime_owned_message(
            "assistant",
            "another arbitrary payload",
            RuntimeMessageDelivery::EphemeralControl,
        );

        let got =
            normalize_prompt_facing_runtime_messages(vec![ordinary.clone(), required, ephemeral]);

        assert_eq!(got.messages, vec![ordinary]);
        assert_eq!(
            got.required_runtime_texts,
            vec!["required payload without a magic prefix"]
        );
    }

    #[test]
    fn recovery_drops_owned_messages_but_preserves_identical_unowned_text() {
        let ordinary = json!({"role": "assistant", "content": "Tools used: bash"});
        let owned = runtime_owned_message(
            "assistant",
            "Tools used: bash",
            RuntimeMessageDelivery::EphemeralControl,
        );

        assert_eq!(
            sanitize_recoverable_runtime_messages(vec![ordinary.clone(), owned]),
            vec![ordinary]
        );
    }

    #[test]
    fn internal_auto_route_roundtrip_is_removed_by_protocol_identity() {
        let messages = vec![
            json!({"role": "user", "content": "review changes"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "skill-auto-route-code-review",
                    "type": "function",
                    "function": {"name": "skill", "arguments": "{}"},
                }],
            }),
            json!({
                "role": "tool",
                "tool_call_id": "skill-auto-route-code-review",
                "content": "loaded",
            }),
        ];

        assert_eq!(
            sanitize_recoverable_runtime_messages(messages),
            vec![json!({"role": "user", "content": "review changes"})]
        );
    }

    #[test]
    fn recoverable_checkpoint_messages_redact_tool_call_arguments() {
        let secret = "hf_abcdefghijklmnopqrstuvwxyz123456";
        let messages = vec![json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call-secret",
                "type": "function",
                "function": {
                    "name": "bash",
                    "arguments": format!("{{\"command\":\"tool --token {secret}\"}}")
                }
            }]
        })];

        let safe = sanitize_recoverable_runtime_messages(messages);
        let encoded = serde_json::to_string(&safe).expect("checkpoint messages serialize");
        assert!(!encoded.contains(secret));
        assert!(encoded.contains("[REDACTED:TOKEN_ARGUMENT]"));
        assert_eq!(safe[0]["tool_calls"][0]["function"]["name"], "bash");
    }

    #[test]
    fn durable_checkpoint_redacts_quoted_and_indexed_embedded_arguments() {
        let token = "hf_abcdefghijklmnopqrstuvwxyz123456";
        let access_key = "SECRET_ACCESS_VALUE_abcdefghijklmnopqrstuvwxyz";
        let arguments = serde_json::to_string(&json!({
            "command": format!(
                r#"tool --token "{token}"; python3 -c 'os.environ["AWS_SECRET_ACCESS_KEY"] = "{access_key}"'"#
            ),
            "api_key": token,
            "path": "src/main.rs"
        }))
        .unwrap();
        let messages = vec![json!({
            "role": "assistant",
            "tool_calls": [{
                "id": "call-quoted",
                "function": {"name": "bash", "arguments": arguments}
            }]
        })];

        let safe = sanitize_durable_message_values(messages);
        let encoded = serde_json::to_string(&safe).unwrap();
        assert!(!encoded.contains(token));
        assert!(!encoded.contains(access_key));
        let inner: Value = serde_json::from_str(
            safe[0]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .expect("arguments remain a JSON string"),
        )
        .expect("sanitized arguments remain parseable");
        assert!(inner["command"].as_str().unwrap().contains("[REDACTED:"));
        assert_eq!(inner["path"], "src/main.rs");
        assert_eq!(inner["api_key"], "[REDACTED:SECRET_FIELD]");
    }

    #[test]
    fn assistant_metadata_function_arguments_are_not_exempt() {
        let secret = "hf_abcdefghijklmnopqrstuvwxyz123456";
        let messages = vec![json!({
            "role": "assistant",
            "content": {
                "function": {"arguments": format!("tool --token {secret}")}
            }
        })];

        let safe = sanitize_durable_message_values(messages);
        let encoded = serde_json::to_string(&safe).unwrap();
        assert!(!encoded.contains(secret));
        assert!(encoded.contains("[REDACTED:"));
    }

    #[test]
    fn nested_content_tool_calls_do_not_get_protocol_exemption() {
        let secret = "hf_abcdefghijklmnopqrstuvwxyz123456";
        let messages = vec![json!({
            "role": "assistant",
            "content": {
                "tool_calls": [{
                    "function": {"arguments": format!("tool --token {secret}")}
                }]
            }
        })];

        let safe = sanitize_durable_message_values(messages);
        let encoded = serde_json::to_string(&safe).unwrap();
        assert!(!encoded.contains(secret));
        assert!(encoded.contains("[REDACTED:"));
    }

    #[test]
    fn malformed_tool_frame_shapes_fail_closed_through_generic_sanitizer() {
        let secret = "hf_abcdefghijklmnopqrstuvwxyz123456";
        let messages = vec![
            json!({
                "role": "assistant",
                "tool_calls": {"function": {"arguments": format!("--token {secret}")}}
            }),
            json!({
                "role": "assistant",
                "tool_calls": [[{"function": {"arguments": format!("--token {secret}")}}]]
            }),
            json!({
                "role": "assistant",
                "tool_calls": [{"function": format!("--token {secret}")}]
            }),
        ];

        let safe = sanitize_durable_message_values(messages);
        let encoded = serde_json::to_string(&safe).unwrap();
        assert!(!encoded.contains(secret));
        assert_eq!(encoded.matches("[REDACTED:").count(), 3);
    }

    #[test]
    fn non_string_tool_arguments_are_sanitized_by_key() {
        let messages = vec![json!({
            "role": "assistant",
            "tool_calls": [{
                "id": "call-object-arguments",
                "function": {
                    "name": "bash",
                    "arguments": {"api_key": "short-but-still-secret"}
                }
            }]
        })];

        let safe = sanitize_durable_message_values(messages);
        assert_eq!(
            safe[0]["tool_calls"][0]["function"]["arguments"]["api_key"],
            "[REDACTED:SECRET_FIELD]"
        );
    }

    #[test]
    fn malformed_embedded_tool_arguments_are_replaced_fail_closed() {
        let secret = "hf_abcdefghijklmnopqrstuvwxyz123456";
        let messages = vec![json!({
            "role": "assistant",
            "tool_calls": [{
                "id": "call-malformed",
                "function": {
                    "name": "bash",
                    "arguments": format!(r#"{{\"command\":\"tool --token {secret}"#)
                }
            }]
        })];

        let safe = sanitize_durable_message_values(messages);
        let encoded = serde_json::to_string(&safe).unwrap();
        assert!(!encoded.contains(secret));
        assert_eq!(
            safe[0]["tool_calls"][0]["function"]["arguments"],
            r#"{"_astra_redaction":"arguments_unavailable"}"#
        );
    }
}
