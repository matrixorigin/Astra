use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Client request fields that would bypass Offering admission by selecting
/// execution material directly.
///
/// Exact object-key matching is intentional: this is a wire-schema boundary,
/// not prose classification. Server-owned resolved route fields use different
/// names and are added only after authenticated admission.
pub const CLIENT_DIRECT_EXECUTION_FIELDS: [&str; 12] = [
    "runtime_bindings",
    "api_key",
    "authorization",
    "base_url",
    "provider",
    "gateway",
    "gateway_id",
    "connection_id",
    "execution_placement",
    "endpoint",
    "endpoint_url",
    "request_headers",
];

#[must_use]
pub fn client_direct_execution_field(payload: &Map<String, Value>) -> Option<&'static str> {
    CLIENT_DIRECT_EXECUTION_FIELDS
        .into_iter()
        .find(|field| payload.contains_key(*field))
}

/// Opaque product-level model choice shared by every client and Server API.
///
/// Provider names, endpoints, credentials, gateways, and execution placement
/// are deliberately absent. They are resolved only after authenticated Server
/// admission of this Offering identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSelection {
    pub offering_id: String,
}

/// Durable owner and causal coordinates for one logical model invocation.
///
/// Auxiliary work such as memory extraction can belong to a session without
/// belonging to an active agent run. Product harness work can likewise belong
/// to a durable harness run without fabricating conversation coordinates.
/// Keeping those distinctions explicit preserves one stable idempotency key
/// across the Server, Edge, SDK, and persistence boundaries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InferenceInvocationScope {
    Run {
        session_id: String,
        run_id: String,
        turn: u32,
        round: u32,
        operation_id: String,
        logical_attempt: u32,
    },
    Session {
        session_id: String,
        turn: u32,
        round: u32,
        operation_id: String,
        logical_attempt: u32,
    },
    HarnessRun {
        harness_run_id: String,
        operation_id: String,
        logical_attempt: u32,
    },
}

impl InferenceInvocationScope {
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::Run { session_id, .. } | Self::Session { session_id, .. } => Some(session_id),
            Self::HarnessRun { .. } => None,
        }
    }

    #[must_use]
    pub fn run_id(&self) -> Option<&str> {
        match self {
            Self::Run { run_id, .. } => Some(run_id),
            Self::Session { .. } | Self::HarnessRun { .. } => None,
        }
    }

    #[must_use]
    pub fn harness_run_id(&self) -> Option<&str> {
        match self {
            Self::HarnessRun { harness_run_id, .. } => Some(harness_run_id),
            Self::Run { .. } | Self::Session { .. } => None,
        }
    }

    #[must_use]
    pub fn turn(&self) -> Option<u32> {
        match self {
            Self::Run { turn, .. } | Self::Session { turn, .. } => Some(*turn),
            Self::HarnessRun { .. } => None,
        }
    }

    #[must_use]
    pub fn round(&self) -> Option<u32> {
        match self {
            Self::Run { round, .. } | Self::Session { round, .. } => Some(*round),
            Self::HarnessRun { .. } => None,
        }
    }

    #[must_use]
    pub fn logical_attempt(&self) -> u32 {
        match self {
            Self::Run {
                logical_attempt, ..
            }
            | Self::Session {
                logical_attempt, ..
            }
            | Self::HarnessRun {
                logical_attempt, ..
            } => *logical_attempt,
        }
    }

    #[must_use]
    pub fn operation_id(&self) -> &str {
        match self {
            Self::Run { operation_id, .. }
            | Self::Session { operation_id, .. }
            | Self::HarnessRun { operation_id, .. } => operation_id,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Run { .. } => "run",
            Self::Session { .. } => "session",
            Self::HarnessRun { .. } => "harness_run",
        }
    }

    #[must_use]
    pub fn with_logical_attempt(&self, logical_attempt: u32) -> Self {
        match self {
            Self::Run {
                session_id,
                run_id,
                turn,
                round,
                operation_id,
                ..
            } => Self::Run {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                turn: *turn,
                round: *round,
                operation_id: operation_id.clone(),
                logical_attempt,
            },
            Self::Session {
                session_id,
                turn,
                round,
                operation_id,
                ..
            } => Self::Session {
                session_id: session_id.clone(),
                turn: *turn,
                round: *round,
                operation_id: operation_id.clone(),
                logical_attempt,
            },
            Self::HarnessRun {
                harness_run_id,
                operation_id,
                ..
            } => Self::HarnessRun {
                harness_run_id: harness_run_id.clone(),
                operation_id: operation_id.clone(),
                logical_attempt,
            },
        }
    }
}

/// Policy- and attribution-relevant reason for one logical model invocation.
///
/// This taxonomy describes why Astra is spending model capacity. It is not a
/// provider adapter, product source, or UI label. Every model call must choose
/// one variant before reaching an executor so policy, budgets, usage, and
/// recovery can share the same fact across Server and Edge paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferencePurpose {
    PrimaryAgent,
    SubAgent,
    RequiredCompaction,
    MemoryExtraction,
    MemoryRetrievalRerank,
    Reflection,
    Introspection,
    VerificationJudge,
    SkillSynthesis,
    Embedding,
}

impl InferencePurpose {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PrimaryAgent => "primary_agent",
            Self::SubAgent => "sub_agent",
            Self::RequiredCompaction => "required_compaction",
            Self::MemoryExtraction => "memory_extraction",
            Self::MemoryRetrievalRerank => "memory_retrieval_rerank",
            Self::Reflection => "reflection",
            Self::Introspection => "introspection",
            Self::VerificationJudge => "verification_judge",
            Self::SkillSynthesis => "skill_synthesis",
            Self::Embedding => "embedding",
        }
    }
}

impl std::fmt::Display for InferencePurpose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_execution_material_is_outside_the_inference_selection_contract() {
        for field in CLIENT_DIRECT_EXECUTION_FIELDS {
            let payload = Map::from_iter([(field.to_string(), Value::Null)]);
            assert_eq!(client_direct_execution_field(&payload), Some(field));
        }
        assert_eq!(
            client_direct_execution_field(&Map::from_iter([(
                "model_selection".to_string(),
                serde_json::json!({"offering_id": "offer-1"}),
            )])),
            None
        );
    }

    #[test]
    fn wire_identity_round_trips_every_purpose() {
        let purposes = [
            InferencePurpose::PrimaryAgent,
            InferencePurpose::SubAgent,
            InferencePurpose::RequiredCompaction,
            InferencePurpose::MemoryExtraction,
            InferencePurpose::MemoryRetrievalRerank,
            InferencePurpose::Reflection,
            InferencePurpose::Introspection,
            InferencePurpose::VerificationJudge,
            InferencePurpose::SkillSynthesis,
            InferencePurpose::Embedding,
        ];

        for purpose in purposes {
            let encoded = serde_json::to_value(purpose).expect("serialize inference purpose");
            let decoded: InferencePurpose = serde_json::from_value(encoded.clone())
                .expect("deserialize serialized inference purpose");
            assert_eq!(decoded, purpose);
            assert_eq!(encoded.as_str(), Some(purpose.as_str()));
        }
    }

    #[test]
    fn unknown_purpose_is_rejected_instead_of_silently_reclassified() {
        let result = serde_json::from_value::<InferencePurpose>(serde_json::json!("other"));
        assert!(result.is_err());
    }

    #[test]
    fn invocation_scope_wire_shape_preserves_owner_and_attempt() {
        let scope = InferenceInvocationScope::Session {
            session_id: "session-1".to_string(),
            turn: 4,
            round: 2,
            operation_id: "memory_extraction".to_string(),
            logical_attempt: 3,
        };

        let encoded = serde_json::to_value(&scope).expect("serialize invocation scope");
        assert_eq!(encoded["kind"], "session");
        assert!(encoded.get("run_id").is_none());
        assert_eq!(
            serde_json::from_value::<InferenceInvocationScope>(encoded)
                .expect("deserialize invocation scope"),
            scope
        );
    }

    #[test]
    fn invocation_scope_rejects_ambiguous_wire_fields() {
        let scope = serde_json::json!({
            "kind": "session",
            "session_id": "session-1",
            "run_id": "fake-run",
            "turn": 1,
            "round": 0,
            "operation_id": "memory_extraction",
            "logical_attempt": 0
        });

        assert!(serde_json::from_value::<InferenceInvocationScope>(scope).is_err());
    }

    #[test]
    fn harness_scope_has_no_fabricated_conversation_coordinates() {
        let scope = InferenceInvocationScope::HarnessRun {
            harness_run_id: "harness-run-1".to_string(),
            operation_id: "skillify_extract".to_string(),
            logical_attempt: 2,
        };

        let encoded = serde_json::to_value(&scope).expect("serialize harness scope");
        assert_eq!(encoded["kind"], "harness_run");
        assert!(encoded.get("session_id").is_none());
        assert!(encoded.get("run_id").is_none());
        assert!(encoded.get("turn").is_none());
        assert!(encoded.get("round").is_none());
        assert_eq!(scope.harness_run_id(), Some("harness-run-1"));
    }

    #[test]
    fn model_selection_rejects_provider_routing_material() {
        assert!(
            serde_json::from_value::<ModelSelection>(serde_json::json!({
                "offering_id": "offer-1",
                "model": "provider-model",
                "gateway": "provider-gateway"
            }))
            .is_err()
        );
    }
}
