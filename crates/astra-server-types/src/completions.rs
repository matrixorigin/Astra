use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Astra's authenticated, non-streaming model invocation contract.
///
/// This intentionally selects a governed Offering rather than accepting a
/// provider URL, model name, or credential. The producer supplies causal
/// ownership; the Server derives the user from authentication and validates
/// the scope before provider I/O.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionRequest {
    pub purpose: astra_turn_types::InferencePurpose,
    pub invocation_scope: astra_turn_types::InferenceInvocationScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_selection: Option<astra_turn_types::ModelSelection>,
    pub messages: Vec<Value>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_temperature")]
    pub temperature: f64,
}

impl CompletionRequest {
    #[must_use]
    pub fn new(
        purpose: astra_turn_types::InferencePurpose,
        invocation_scope: astra_turn_types::InferenceInvocationScope,
        messages: Vec<Value>,
    ) -> Self {
        Self {
            purpose,
            invocation_scope,
            model_selection: None,
            messages,
            max_tokens: default_max_tokens(),
            temperature: default_temperature(),
        }
    }

    #[must_use]
    pub fn with_offering_id(mut self, offering_id: impl Into<String>) -> Self {
        self.model_selection = Some(astra_turn_types::ModelSelection {
            offering_id: offering_id.into(),
        });
        self
    }
}

const fn default_max_tokens() -> u32 {
    512
}

const fn default_temperature() -> f64 {
    0.1
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionResponse {
    pub id: String,
    pub object: String,
    pub offering_id: String,
    pub model: String,
    pub choices: Vec<CompletionChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<CompletionUsage>,
}

impl CompletionResponse {
    #[must_use]
    pub fn first_text(&self) -> Option<&str> {
        self.choices
            .first()
            .map(|choice| choice.message.content.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionChoice {
    pub index: u32,
    pub message: CompletionMessage,
    pub finish_reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionMessage {
    pub role: String,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_defaults_and_scope_are_one_wire_contract() {
        let request: CompletionRequest = serde_json::from_value(serde_json::json!({
            "purpose": "verification_judge",
            "invocation_scope": {
                "kind": "session",
                "session_id": "session-1",
                "turn": 2,
                "round": 1,
                "operation_id": "plan_verification",
                "logical_attempt": 0
            },
            "messages": [{"role": "user", "content": "verify"}]
        }))
        .expect("typed completion request");

        assert_eq!(request.max_tokens, 512);
        assert!((request.temperature - 0.1).abs() < f64::EPSILON);
        assert!(request.model_selection.is_none());
    }

    #[test]
    fn request_rejects_provider_material_and_ambiguous_scope() {
        for forbidden in [
            serde_json::json!({
                "purpose": "memory_extraction",
                "invocation_scope": {
                    "kind": "session", "session_id": "session-1", "run_id": "fake",
                    "turn": 1, "round": 0, "operation_id": "memory", "logical_attempt": 0
                },
                "messages": []
            }),
            serde_json::json!({
                "purpose": "memory_extraction",
                "invocation_scope": {
                    "kind": "session", "session_id": "session-1",
                    "turn": 1, "round": 0, "operation_id": "memory", "logical_attempt": 0
                },
                "model": "provider-model",
                "messages": []
            }),
        ] {
            assert!(serde_json::from_value::<CompletionRequest>(forbidden).is_err());
        }
    }
}
