use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Server-owned auxiliary operations available through the authenticated
/// non-streaming completion endpoint.
///
/// Each variant fixes both inference purpose and durable operation identity.
/// Clients provide only session coordinates; they cannot impersonate an agent
/// run, a harness run, or another internal inference producer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionOperation {
    MemoryExtraction,
    MemoryRetrievalRerank,
    SkillAutoRoute,
    VerificationJudge,
}

impl CompletionOperation {
    #[must_use]
    pub const fn purpose(self) -> astra_turn_types::InferencePurpose {
        match self {
            Self::MemoryExtraction => astra_turn_types::InferencePurpose::MemoryExtraction,
            Self::MemoryRetrievalRerank => {
                astra_turn_types::InferencePurpose::MemoryRetrievalRerank
            }
            Self::SkillAutoRoute => astra_turn_types::InferencePurpose::Introspection,
            Self::VerificationJudge => astra_turn_types::InferencePurpose::VerificationJudge,
        }
    }

    #[must_use]
    pub const fn operation_id(self) -> &'static str {
        match self {
            Self::MemoryExtraction => "completion_proxy:memory_extraction",
            Self::MemoryRetrievalRerank => "completion_proxy:memory_retrieval_rerank",
            Self::SkillAutoRoute => "completion_proxy:skill_auto_route",
            Self::VerificationJudge => "completion_proxy:verification_judge",
        }
    }
}

/// Astra's authenticated, non-streaming auxiliary inference contract.
///
/// This intentionally selects a governed Offering rather than accepting a
/// provider URL, model name, credential, purpose, or arbitrary lifecycle
/// scope. The Server owns producer identity and derives the durable session
/// scope from these causal coordinates.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionRequest {
    pub operation: CompletionOperation,
    pub session_id: String,
    pub turn: u32,
    pub round: u32,
    pub logical_attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_selection: Option<astra_turn_types::ModelSelection>,
    pub messages: Vec<Value>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_temperature")]
    pub temperature: f64,
    /// Provider execution deadline owned and enforced by the Server. Client
    /// transports may add a small settlement grace, but must not race a
    /// second semantic inference deadline against this value.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

impl CompletionRequest {
    #[must_use]
    pub fn new(
        operation: CompletionOperation,
        session_id: impl Into<String>,
        turn: u32,
        round: u32,
        logical_attempt: u32,
        messages: Vec<Value>,
    ) -> Self {
        Self {
            operation,
            session_id: session_id.into(),
            turn,
            round,
            logical_attempt,
            model_selection: None,
            messages,
            max_tokens: default_max_tokens(),
            temperature: default_temperature(),
            timeout_ms: default_timeout_ms(),
        }
    }

    /// Build the public proxy request from an internal typed scope without
    /// exposing that scope on the wire.
    pub fn from_session_scope(
        operation: CompletionOperation,
        scope: &astra_turn_types::InferenceInvocationScope,
        messages: Vec<Value>,
    ) -> Result<Self, &'static str> {
        let astra_turn_types::InferenceInvocationScope::Session {
            session_id,
            turn,
            round,
            logical_attempt,
            ..
        } = scope
        else {
            return Err("completion proxy accepts session-scoped auxiliary work only");
        };
        Ok(Self::new(
            operation,
            session_id,
            *turn,
            *round,
            *logical_attempt,
            messages,
        ))
    }

    #[must_use]
    pub fn with_offering_id(mut self, offering_id: impl Into<String>) -> Self {
        self.model_selection = Some(astra_turn_types::ModelSelection {
            offering_id: offering_id.into(),
        });
        self
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout_ms = timeout.as_millis().min(u128::from(u64::MAX)) as u64;
        self
    }

    #[must_use]
    pub fn purpose(&self) -> astra_turn_types::InferencePurpose {
        self.operation.purpose()
    }

    #[must_use]
    pub fn invocation_scope(&self) -> astra_turn_types::InferenceInvocationScope {
        astra_turn_types::InferenceInvocationScope::Session {
            session_id: self.session_id.clone(),
            turn: self.turn,
            round: self.round,
            operation_id: self.operation.operation_id().to_string(),
            logical_attempt: self.logical_attempt,
        }
    }
}

const fn default_max_tokens() -> u32 {
    512
}

const fn default_temperature() -> f64 {
    0.1
}

const fn default_timeout_ms() -> u64 {
    120_000
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
    fn request_defaults_and_server_owned_identity_are_one_wire_contract() {
        let request: CompletionRequest = serde_json::from_value(serde_json::json!({
            "operation": "verification_judge",
            "session_id": "session-1",
            "turn": 2,
            "round": 1,
            "logical_attempt": 0,
            "messages": [{"role": "user", "content": "verify"}]
        }))
        .expect("typed completion request");

        assert_eq!(request.max_tokens, 512);
        assert!((request.temperature - 0.1).abs() < f64::EPSILON);
        assert_eq!(request.timeout_ms, 120_000);
        assert!(request.model_selection.is_none());
        assert_eq!(
            request.invocation_scope().operation_id(),
            "completion_proxy:verification_judge"
        );
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
                "operation": "memory_extraction",
                "session_id": "session-1", "turn": 1, "round": 0, "logical_attempt": 0,
                "model": "provider-model",
                "messages": []
            }),
            serde_json::json!({
                "operation": "memory_extraction",
                "session_id": "session-1", "turn": 1, "round": 0, "logical_attempt": 0,
                "invocation_scope": {"kind": "run", "run_id": "fake"},
                "messages": []
            }),
        ] {
            assert!(serde_json::from_value::<CompletionRequest>(forbidden).is_err());
        }
    }
}
