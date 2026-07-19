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
    messages
        .into_iter()
        .filter(|message| {
            !is_runtime_owned_message(message) && !is_internal_skill_auto_route_message(message)
        })
        .collect()
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
}
