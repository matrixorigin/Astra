//! Shared wire rules for chat inference and credential/connectivity probes.
//! Keep this below services/runtime so probes cannot invent a second contract.
use serde_json::{Value, json};

pub mod purpose;
pub mod responses;
pub mod thinking;

/// Wire format used for inference, independent of credential ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmProviderProtocol {
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    #[serde(rename = "openai_compatible")]
    OpenAiCompatible,
    AnthropicMessages,
    BedrockConverse,
    #[serde(rename = "typesafe_systemone")]
    TypeSafeSystemOne,
}

impl LlmProviderProtocol {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiResponses => "openai_responses",
            Self::OpenAiCompatible => "openai_compatible",
            Self::AnthropicMessages => "anthropic_messages",
            Self::BedrockConverse => "bedrock_converse",
            Self::TypeSafeSystemOne => "typesafe_systemone",
        }
    }

    #[must_use]
    pub fn preserves_appended_message_boundaries(self) -> bool {
        matches!(self, Self::OpenAiCompatible)
    }
}

#[must_use]
pub fn llm_provider_protocol(provider: &str) -> LlmProviderProtocol {
    match provider {
        "chatgpt" => LlmProviderProtocol::OpenAiResponses,
        "anthropic" => LlmProviderProtocol::AnthropicMessages,
        "bedrock" => LlmProviderProtocol::BedrockConverse,
        "typesafe" => LlmProviderProtocol::TypeSafeSystemOne,
        _ => LlmProviderProtocol::OpenAiCompatible,
    }
}

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
