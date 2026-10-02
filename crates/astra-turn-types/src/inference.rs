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

/// A caller's fixed-model request, before Server resolves it to an exact
/// Offering. Names are lookup keys only; execution and authorization always
/// use the returned [`ModelSelection`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelSelector {
    OfferingId {
        offering_id: String,
    },
    ConfiguredName {
        model_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
}

impl ModelSelector {
    pub fn validate(&self) -> Result<(), &'static str> {
        let valid = |value: &str, max_chars: usize| {
            !value.is_empty()
                && value.trim() == value
                && value.chars().count() <= max_chars
                && !value.chars().any(char::is_control)
        };
        match self {
            Self::OfferingId { offering_id } if valid(offering_id, 64) => Ok(()),
            Self::OfferingId { .. } => Err("Offering ID selector is invalid"),
            Self::ConfiguredName { model_name, source }
                if valid(model_name, 256)
                    && source.as_deref().is_none_or(|source| valid(source, 128)) =>
            {
                Ok(())
            }
            Self::ConfiguredName { .. } => Err("configured model-name selector is invalid"),
        }
    }
}

/// The user's requested model behavior before it is resolved to an Offering.
///
/// This is intentionally distinct from [`ModelSelection`]: `inherit` and
/// `auto` can resolve to the same Offering as a fixed request while retaining
/// different semantics for nested delegation, retries, and explanation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum RequestedModelPolicy {
    Inherit,
    Fixed { selector: ModelSelector },
    Auto { strategy: AutoModelStrategy },
}

/// Resolve the caller's fixed selector without consulting a model catalog.
/// Configured names remain selectors until authenticated Server admission;
/// inherited choices are converted back to their canonical Offering ID.
pub fn resolve_requested_model_selector(
    requested: Option<&RequestedModelPolicy>,
    inherited: Option<&ModelSelection>,
) -> Result<Option<ModelSelector>, RequestedModelPolicyError> {
    match requested {
        None | Some(RequestedModelPolicy::Inherit) => {
            Ok(inherited.map(|selection| ModelSelector::OfferingId {
                offering_id: selection.offering_id.clone(),
            }))
        }
        Some(RequestedModelPolicy::Fixed { selector }) => {
            selector
                .validate()
                .map_err(|_| RequestedModelPolicyError::InvalidSelector)?;
            Ok(Some(selector.clone()))
        }
        Some(RequestedModelPolicy::Auto { .. }) => {
            Err(RequestedModelPolicyError::AutomaticRoutingUnavailable)
        }
    }
}

/// The optimization objective for a requested automatic model choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoModelStrategy {
    CostPriority,
    Balanced,
}

/// Resolve the model policy when automatic routing is not installed.
///
/// `None` on the request means ordinary inheritance, just like an explicit
/// `inherit`; callers retain the original optional policy separately for
/// precedence and durable provenance.
pub fn resolve_requested_model_selection(
    requested: Option<&RequestedModelPolicy>,
    inherited: Option<&ModelSelection>,
) -> Result<Option<ModelSelection>, RequestedModelPolicyError> {
    match resolve_requested_model_selector(requested, inherited)? {
        None => Ok(None),
        Some(ModelSelector::OfferingId { offering_id }) => Ok(Some(ModelSelection { offering_id })),
        Some(ModelSelector::ConfiguredName { .. }) => {
            Err(RequestedModelPolicyError::ConfiguredNameRequiresCatalog)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestedModelPolicyError {
    AutomaticRoutingUnavailable,
    ConfiguredNameRequiresCatalog,
    InvalidSelector,
}

impl std::fmt::Display for RequestedModelPolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AutomaticRoutingUnavailable => {
                f.write_str("automatic model routing is not available yet: comparable task-level cost, quality, and completion-time evidence is unavailable; choose a fixed model")
            }
            Self::ConfiguredNameRequiresCatalog => {
                f.write_str("configured model names must be resolved by Server admission")
            }
            Self::InvalidSelector => f.write_str("requested model selector is invalid"),
        }
    }
}

impl std::error::Error for RequestedModelPolicyError {}

/// Durable owner and causal coordinates for one logical model invocation.
///
/// Auxiliary work such as memory extraction can belong to a session without
/// belonging to an active agent run.
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
}

impl InferenceInvocationScope {
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::Run { session_id, .. } | Self::Session { session_id, .. } => Some(session_id),
        }
    }

    #[must_use]
    pub fn run_id(&self) -> Option<&str> {
        match self {
            Self::Run { run_id, .. } => Some(run_id),
            Self::Session { .. } => None,
        }
    }

    #[must_use]
    pub fn turn(&self) -> Option<u32> {
        match self {
            Self::Run { turn, .. } | Self::Session { turn, .. } => Some(*turn),
        }
    }

    #[must_use]
    pub fn round(&self) -> Option<u32> {
        match self {
            Self::Run { round, .. } | Self::Session { round, .. } => Some(*round),
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
            } => *logical_attempt,
        }
    }

    #[must_use]
    pub fn operation_id(&self) -> &str {
        match self {
            Self::Run { operation_id, .. } | Self::Session { operation_id, .. } => operation_id,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Run { .. } => "run",
            Self::Session { .. } => "session",
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
        }
    }

    #[must_use]
    pub fn with_round(&self, round: u32) -> Self {
        match self {
            Self::Run {
                session_id,
                run_id,
                turn,
                operation_id,
                logical_attempt,
                ..
            } => Self::Run {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                turn: *turn,
                round,
                operation_id: operation_id.clone(),
                logical_attempt: *logical_attempt,
            },
            Self::Session {
                session_id,
                turn: turn_index,
                operation_id,
                logical_attempt,
                ..
            } => Self::Session {
                session_id: session_id.clone(),
                turn: *turn_index,
                round,
                operation_id: operation_id.clone(),
                logical_attempt: *logical_attempt,
            },
        }
    }

    #[must_use]
    pub fn with_operation_id(&self, operation_id: impl Into<String>) -> Self {
        let operation_id = operation_id.into();
        match self {
            Self::Run {
                session_id,
                run_id,
                turn,
                round,
                logical_attempt,
                ..
            } => Self::Run {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                turn: *turn,
                round: *round,
                operation_id,
                logical_attempt: *logical_attempt,
            },
            Self::Session {
                session_id,
                turn,
                round,
                logical_attempt,
                ..
            } => Self::Session {
                session_id: session_id.clone(),
                turn: *turn,
                round: *round,
                operation_id,
                logical_attempt: *logical_attempt,
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
    ToolResultRerank,
    Reflection,
    Introspection,
    VerificationJudge,
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
            Self::ToolResultRerank => "tool_result_rerank",
            Self::Reflection => "reflection",
            Self::Introspection => "introspection",
            Self::VerificationJudge => "verification_judge",
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
            InferencePurpose::ToolResultRerank,
            InferencePurpose::Reflection,
            InferencePurpose::Introspection,
            InferencePurpose::VerificationJudge,
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
    fn selection_identity_can_freeze_round_and_operation_without_changing_owner() {
        let scope = InferenceInvocationScope::Run {
            session_id: "session-1".to_string(),
            run_id: "run-1".to_string(),
            turn: 4,
            round: 7,
            operation_id: "tool_result_rerank".to_string(),
            logical_attempt: 3,
        };

        let frozen = scope
            .with_round(0)
            .with_operation_id("f".repeat(64))
            .with_logical_attempt(0);
        assert_eq!(frozen.session_id(), Some("session-1"));
        assert_eq!(frozen.run_id(), Some("run-1"));
        assert_eq!(frozen.turn(), Some(4));
        assert_eq!(frozen.round(), Some(0));
        assert_eq!(frozen.logical_attempt(), 0);
        assert_eq!(frozen.operation_id(), "f".repeat(64));
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
    fn retired_product_scope_and_purpose_are_rejected() {
        assert!(
            serde_json::from_value::<InferenceInvocationScope>(serde_json::json!({
                "kind": "harness_run",
                "harness_run_id": "retired-product-owner",
                "operation_id": "skillify_extract",
                "logical_attempt": 0
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<InferencePurpose>(serde_json::json!("skill_synthesis"))
                .is_err()
        );
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

    #[test]
    fn requested_policy_remains_distinct_from_its_resolved_offering() {
        let inherited = ModelSelection {
            offering_id: "offer-parent".to_string(),
        };
        assert_eq!(
            resolve_requested_model_selection(
                Some(&RequestedModelPolicy::Inherit),
                Some(&inherited)
            )
            .unwrap(),
            Some(inherited.clone())
        );
        assert_eq!(
            resolve_requested_model_selection(
                Some(&RequestedModelPolicy::Fixed {
                    selector: ModelSelector::OfferingId {
                        offering_id: "offer-child".to_string(),
                    },
                }),
                Some(&inherited),
            )
            .unwrap()
            .map(|selection| selection.offering_id),
            Some("offer-child".to_string())
        );
        assert_eq!(
            resolve_requested_model_selection(
                Some(&RequestedModelPolicy::Auto {
                    strategy: AutoModelStrategy::Balanced,
                }),
                Some(&inherited),
            ),
            Err(RequestedModelPolicyError::AutomaticRoutingUnavailable)
        );
        assert_eq!(
            resolve_requested_model_selection(
                Some(&RequestedModelPolicy::Fixed {
                    selector: ModelSelector::ConfiguredName {
                        model_name: "glm-5.2".to_string(),
                        source: None,
                    },
                }),
                Some(&inherited),
            ),
            Err(RequestedModelPolicyError::ConfiguredNameRequiresCatalog)
        );
        assert_eq!(
            resolve_requested_model_selector(
                Some(&RequestedModelPolicy::Fixed {
                    selector: ModelSelector::ConfiguredName {
                        model_name: "glm-5.2".to_string(),
                        source: Some("provider-a".to_string()),
                    },
                }),
                Some(&inherited),
            )
            .unwrap(),
            Some(ModelSelector::ConfiguredName {
                model_name: "glm-5.2".to_string(),
                source: Some("provider-a".to_string()),
            })
        );
        assert_eq!(
            resolve_requested_model_selector(
                Some(&RequestedModelPolicy::Fixed {
                    selector: ModelSelector::ConfiguredName {
                        model_name: " glm-5.2".to_string(),
                        source: None,
                    },
                }),
                Some(&inherited),
            ),
            Err(RequestedModelPolicyError::InvalidSelector)
        );
    }
}
