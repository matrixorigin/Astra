//! Shared wire rules for chat inference and credential/connectivity probes.
//! Keep this below services/runtime so probes cannot invent a second contract.
use serde_json::{Value, json};

pub mod thinking;

/// Apply a caller-bounded output budget to Anthropic Messages or OpenAI-style
/// chat completions. Bedrock Converse has a separate inferenceConfig shape.
/// Thinking-budget policy belongs to the caller, not this serialization helper.
/// DeepSeek shares the message shape, but its documented output bound remains
/// [`max_tokens`](https://api-docs.deepseek.com/api/create-chat-completion/).
pub fn apply_chat_output_token_limit(body: &mut Value, provider: &str, tokens: usize) {
    let (field, obsolete) = if matches!(provider, "anthropic" | "deepseek") {
        ("max_tokens", "max_completion_tokens")
    } else {
        ("max_completion_tokens", "max_tokens")
    };
    if let Some(object) = body.as_object_mut() {
        object.remove(obsolete);
        object.insert(field.into(), json!(tokens));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_limit_uses_the_protocol_not_the_model_name() {
        // Expectations come from each provider's wire contract, not the
        // production branch condition. The model name must not select it.
        for (provider, field, absent) in [
            ("openai", "max_completion_tokens", "max_tokens"),
            ("openai-compatible", "max_completion_tokens", "max_tokens"),
            ("deepseek", "max_tokens", "max_completion_tokens"),
            ("anthropic", "max_tokens", "max_completion_tokens"),
        ] {
            let mut body = json!({"model":"o3", "max_tokens":1, "max_completion_tokens":2});
            apply_chat_output_token_limit(&mut body, provider, 32);
            assert_eq!(body[field], 32, "{provider}");
            assert!(body.get(absent).is_none(), "{provider}: {absent}");
        }
    }
}
