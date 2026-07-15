//! Durable tool-invocation identity and state-machine contracts.
//!
//! Invocation delivery deduplication is intentionally keyed by logical call
//! identity. It is separate from semantic read caching, which may compare
//! descriptor, arguments, and freshness facts across distinct invocations.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::ResolvedToolDescriptorRef;

pub const TOOL_INVOCATION_CONTRACT_VERSION: &str = "tool-invocation-v1";

const INTERNAL_TRANSPORT_ARGUMENTS: [&str; 3] = ["_run_id", "_tool_call_id", "_turn_chain_id"];

/// Stable identity of one logical tool invocation across retry, reconnect,
/// process restart, and resume.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct ToolInvocationIdentity {
    pub user_id: String,
    pub session_id: String,
    pub run_id: String,
    pub turn_chain_id: String,
    pub invocation_id: String,
}

impl ToolInvocationIdentity {
    pub fn new(
        user_id: impl Into<String>,
        session_id: impl Into<String>,
        run_id: impl Into<String>,
        turn_chain_id: impl Into<String>,
        invocation_id: impl Into<String>,
    ) -> Result<Self, ToolInvocationContractError> {
        let identity = Self {
            user_id: user_id.into(),
            session_id: session_id.into(),
            run_id: run_id.into(),
            turn_chain_id: turn_chain_id.into(),
            invocation_id: invocation_id.into(),
        };
        identity.validate()?;
        Ok(identity)
    }

    fn validate(&self) -> Result<(), ToolInvocationContractError> {
        for (field, value) in [
            ("user_id", self.user_id.as_str()),
            ("session_id", self.session_id.as_str()),
            ("run_id", self.run_id.as_str()),
            ("turn_chain_id", self.turn_chain_id.as_str()),
            ("invocation_id", self.invocation_id.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(ToolInvocationContractError::EmptyIdentityField { field });
            }
        }
        Ok(())
    }

    /// Content-addressed storage key. The full structured identity remains the
    /// source of truth and must still be compared on lookup.
    pub fn storage_key(&self) -> String {
        let encoded = serde_json::to_vec(self)
            .expect("validated tool invocation identity must serialize to JSON");
        format!("sha256:{:x}", Sha256::digest(encoded))
    }
}

impl<'de> Deserialize<'de> for ToolInvocationIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawIdentity {
            user_id: String,
            session_id: String,
            run_id: String,
            turn_chain_id: String,
            invocation_id: String,
        }

        let raw = RawIdentity::deserialize(deserializer)?;
        Self::new(
            raw.user_id,
            raw.session_id,
            raw.run_id,
            raw.turn_chain_id,
            raw.invocation_id,
        )
        .map_err(serde::de::Error::custom)
    }
}

/// Exact execution contract selected for an invocation. Built-ins carry a
/// registry contract version; provider tools carry the exact resolved
/// descriptor reference. A public alias is deliberately absent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DurableToolReference {
    BuiltIn {
        tool_name: String,
        registry_contract_version: String,
    },
    Provider {
        descriptor: ResolvedToolDescriptorRef,
    },
}

impl DurableToolReference {
    pub fn built_in(
        tool_name: impl Into<String>,
        registry_contract_version: impl Into<String>,
    ) -> Result<Self, ToolInvocationContractError> {
        let tool_name = tool_name.into();
        let registry_contract_version = registry_contract_version.into();
        if tool_name.trim().is_empty() {
            return Err(ToolInvocationContractError::EmptyToolReferenceField {
                field: "tool_name",
            });
        }
        if registry_contract_version.trim().is_empty() {
            return Err(ToolInvocationContractError::EmptyToolReferenceField {
                field: "registry_contract_version",
            });
        }
        Ok(Self::BuiltIn {
            tool_name,
            registry_contract_version,
        })
    }
}

impl<'de> Deserialize<'de> for DurableToolReference {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum RawToolReference {
            BuiltIn {
                tool_name: String,
                registry_contract_version: String,
            },
            Provider {
                descriptor: ResolvedToolDescriptorRef,
            },
        }

        match RawToolReference::deserialize(deserializer)? {
            RawToolReference::BuiltIn {
                tool_name,
                registry_contract_version,
            } => Self::built_in(tool_name, registry_contract_version)
                .map_err(serde::de::Error::custom),
            RawToolReference::Provider { descriptor } => Ok(Self::Provider { descriptor }),
        }
    }
}

/// Immutable fingerprint checked whenever the same invocation identity is
/// delivered again. A mismatch is an identity conflict, never a new call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ToolInvocationFingerprint {
    pub tool: DurableToolReference,
    pub canonical_arguments_hash: String,
    pub policy_decision_id: String,
}

impl<'de> Deserialize<'de> for ToolInvocationFingerprint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawFingerprint {
            tool: DurableToolReference,
            canonical_arguments_hash: String,
            policy_decision_id: String,
        }

        let raw = RawFingerprint::deserialize(deserializer)?;
        if raw.canonical_arguments_hash.trim().is_empty() {
            return Err(serde::de::Error::custom(
                ToolInvocationContractError::EmptyCanonicalArgumentsHash,
            ));
        }
        if raw.policy_decision_id.trim().is_empty() {
            return Err(serde::de::Error::custom(
                ToolInvocationContractError::EmptyPolicyDecisionId,
            ));
        }
        Ok(Self {
            tool: raw.tool,
            canonical_arguments_hash: raw.canonical_arguments_hash,
            policy_decision_id: raw.policy_decision_id,
        })
    }
}

impl ToolInvocationFingerprint {
    pub fn new(
        tool: DurableToolReference,
        arguments: &Value,
        policy_decision_id: impl Into<String>,
    ) -> Result<Self, ToolInvocationContractError> {
        let policy_decision_id = policy_decision_id.into();
        if policy_decision_id.trim().is_empty() {
            return Err(ToolInvocationContractError::EmptyPolicyDecisionId);
        }
        Ok(Self {
            tool,
            canonical_arguments_hash: canonical_public_arguments_hash(arguments),
            policy_decision_id,
        })
    }
}

/// Durable invocation state. `OutcomeUnknown` can only be resolved by
/// reconciliation; it cannot transition back to dispatch implicitly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolInvocationState {
    Prepared,
    Dispatched,
    Succeeded,
    Failed,
    Rejected,
    OutcomeUnknown,
}

impl ToolInvocationState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Rejected | Self::OutcomeUnknown
        )
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Prepared, Self::Dispatched)
                | (
                    Self::Dispatched,
                    Self::Succeeded | Self::Failed | Self::Rejected | Self::OutcomeUnknown
                )
                | (
                    Self::OutcomeUnknown,
                    Self::Succeeded | Self::Failed | Self::Rejected
                )
        )
    }

    pub fn required_dispatch_certainty(self) -> DispatchCertainty {
        match self {
            Self::Prepared => DispatchCertainty::NotDispatched,
            Self::OutcomeUnknown => DispatchCertainty::Unknown,
            Self::Dispatched | Self::Succeeded | Self::Failed | Self::Rejected => {
                DispatchCertainty::Dispatched
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchCertainty {
    NotDispatched,
    Dispatched,
    Unknown,
}

/// Durable, replayable result returned by one acknowledged invocation. The
/// payload is the bounded runtime projection, not an unbounded provider body;
/// large raw evidence belongs in an owner-scoped artifact referenced by
/// metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInvocationResultPayload {
    pub output: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_semantics: Option<String>,
}

/// Typed acknowledged outcome. Transport ambiguity is represented by the
/// ledger's `OutcomeUnknown` state and therefore has no fabricated payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolInvocationTerminalOutcome {
    Succeeded {
        result: ToolInvocationResultPayload,
    },
    Failed {
        result: ToolInvocationResultPayload,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_kind: Option<String>,
        retryable: bool,
    },
    Rejected {
        result: ToolInvocationResultPayload,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rejection_code: Option<String>,
        retryable: bool,
    },
}

impl ToolInvocationTerminalOutcome {
    pub fn state(&self) -> ToolInvocationState {
        match self {
            Self::Succeeded { .. } => ToolInvocationState::Succeeded,
            Self::Failed { .. } => ToolInvocationState::Failed,
            Self::Rejected { .. } => ToolInvocationState::Rejected,
        }
    }

    pub fn result(&self) -> &ToolInvocationResultPayload {
        match self {
            Self::Succeeded { result }
            | Self::Failed { result, .. }
            | Self::Rejected { result, .. } => result,
        }
    }
}

/// Portable durable ledger row. Storage adapters may add database timestamps,
/// but identity, fingerprint, state, certainty, and attempts are shared facts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ToolInvocationRecord {
    pub identity: ToolInvocationIdentity,
    pub fingerprint: ToolInvocationFingerprint,
    pub state: ToolInvocationState,
    pub dispatch_certainty: DispatchCertainty,
    pub attempt_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<ToolInvocationTerminalOutcome>,
}

impl ToolInvocationRecord {
    pub fn validate(&self) -> Result<(), ToolInvocationContractError> {
        let required_certainty = self.state.required_dispatch_certainty();
        if self.dispatch_certainty != required_certainty {
            return Err(ToolInvocationContractError::RecordCertaintyMismatch {
                state: self.state,
                expected: required_certainty,
                actual: self.dispatch_certainty,
            });
        }
        match (&self.outcome, self.state) {
            (Some(outcome), state) if outcome.state() == state => Ok(()),
            (
                None,
                ToolInvocationState::Succeeded
                | ToolInvocationState::Failed
                | ToolInvocationState::Rejected,
            ) => Err(ToolInvocationContractError::MissingTerminalOutcome { state: self.state }),
            (None, _) => Ok(()),
            (Some(outcome), _) => Err(ToolInvocationContractError::OutcomeStateMismatch {
                state: self.state,
                outcome_state: outcome.state(),
            }),
        }
    }
}

impl<'de> Deserialize<'de> for ToolInvocationRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawRecord {
            identity: ToolInvocationIdentity,
            fingerprint: ToolInvocationFingerprint,
            state: ToolInvocationState,
            dispatch_certainty: DispatchCertainty,
            attempt_count: u32,
            #[serde(default)]
            outcome: Option<ToolInvocationTerminalOutcome>,
        }

        let raw = RawRecord::deserialize(deserializer)?;
        let record = Self {
            identity: raw.identity,
            fingerprint: raw.fingerprint,
            state: raw.state,
            dispatch_certainty: raw.dispatch_certainty,
            attempt_count: raw.attempt_count,
            outcome: raw.outcome,
        };
        record.validate().map_err(serde::de::Error::custom)?;
        Ok(record)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "record", rename_all = "snake_case")]
pub enum ToolInvocationPrepareOutcome {
    Prepared(ToolInvocationRecord),
    Existing(ToolInvocationRecord),
}

/// Remove only Astra-owned transport fields. Arbitrary underscore-prefixed
/// fields remain semantic provider input and therefore remain in the hash.
pub fn canonical_public_tool_arguments(arguments: &Value) -> Value {
    let mut public = arguments.clone();
    if let Some(object) = public.as_object_mut() {
        for field in INTERNAL_TRANSPORT_ARGUMENTS {
            object.remove(field);
        }
    }
    canonical_json(&public)
}

pub fn canonical_public_arguments_hash(arguments: &Value) -> String {
    let canonical = canonical_public_tool_arguments(arguments);
    let encoded =
        serde_json::to_vec(&canonical).expect("canonical tool arguments must serialize to JSON");
    format!("sha256:{:x}", Sha256::digest(encoded))
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(canonical_json).collect()),
        Value::Object(object) => {
            let sorted = object
                .iter()
                .map(|(key, value)| (key.clone(), canonical_json(value)))
                .collect::<BTreeMap<_, _>>();
            Value::Object(sorted.into_iter().collect())
        }
        _ => value.clone(),
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ToolInvocationContractError {
    #[error("tool invocation identity field '{field}' must not be empty")]
    EmptyIdentityField { field: &'static str },
    #[error("tool invocation policy decision id must not be empty")]
    EmptyPolicyDecisionId,
    #[error("tool invocation reference field '{field}' must not be empty")]
    EmptyToolReferenceField { field: &'static str },
    #[error("tool invocation canonical arguments hash must not be empty")]
    EmptyCanonicalArgumentsHash,
    #[error("tool invocation terminal state {state:?} is missing its typed outcome")]
    MissingTerminalOutcome { state: ToolInvocationState },
    #[error(
        "tool invocation outcome state {outcome_state:?} is inconsistent with ledger state {state:?}"
    )]
    OutcomeStateMismatch {
        state: ToolInvocationState,
        outcome_state: ToolInvocationState,
    },
    #[error(
        "tool invocation dispatch certainty {actual:?} is inconsistent with state {state:?}; expected {expected:?}"
    )]
    RecordCertaintyMismatch {
        state: ToolInvocationState,
        expected: DispatchCertainty,
        actual: DispatchCertainty,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn identity(invocation_id: &str) -> ToolInvocationIdentity {
        ToolInvocationIdentity::new("user", "session", "run", "turn", invocation_id).unwrap()
    }

    fn tool_ref() -> DurableToolReference {
        DurableToolReference::built_in("bash", "registry-v1").unwrap()
    }

    #[test]
    fn equal_arguments_with_distinct_invocation_ids_remain_distinct_intent() {
        let first = identity("call-1");
        let second = identity("call-2");
        let args = json!({"command": "deploy"});
        let fingerprint = ToolInvocationFingerprint::new(tool_ref(), &args, "policy-v1").unwrap();

        assert_ne!(first, second);
        assert_ne!(first.storage_key(), second.storage_key());
        assert_eq!(
            fingerprint,
            ToolInvocationFingerprint::new(tool_ref(), &args, "policy-v1").unwrap()
        );
    }

    #[test]
    fn canonical_argument_hash_excludes_only_owned_transport_fields() {
        let first = json!({
            "nested": {"z": 1, "a": 2},
            "_provider_semantic": "keep",
            "_tool_call_id": "call-1",
            "_run_id": "run-1",
            "_turn_chain_id": "turn-1"
        });
        let replay = json!({
            "_turn_chain_id": "turn-2",
            "_run_id": "run-2",
            "_tool_call_id": "call-2",
            "_provider_semantic": "keep",
            "nested": {"a": 2, "z": 1}
        });
        let changed_public_input = json!({
            "_provider_semantic": "changed",
            "nested": {"a": 2, "z": 1}
        });

        assert_eq!(
            canonical_public_arguments_hash(&first),
            canonical_public_arguments_hash(&replay)
        );
        assert_ne!(
            canonical_public_arguments_hash(&first),
            canonical_public_arguments_hash(&changed_public_input)
        );
    }

    #[test]
    fn invocation_state_machine_never_redispatches_unknown_or_terminal_outcomes() {
        assert!(ToolInvocationState::Prepared.can_transition_to(ToolInvocationState::Dispatched));
        assert!(
            ToolInvocationState::Dispatched.can_transition_to(ToolInvocationState::OutcomeUnknown)
        );
        assert!(
            ToolInvocationState::OutcomeUnknown.can_transition_to(ToolInvocationState::Succeeded)
        );
        assert!(
            !ToolInvocationState::OutcomeUnknown.can_transition_to(ToolInvocationState::Dispatched)
        );
        assert!(!ToolInvocationState::Succeeded.can_transition_to(ToolInvocationState::Dispatched));
        assert!(ToolInvocationState::OutcomeUnknown.is_terminal());
    }

    #[test]
    fn deserialization_rejects_incomplete_invocation_identity() {
        let error = serde_json::from_value::<ToolInvocationIdentity>(json!({
            "user_id": "user",
            "session_id": "session",
            "run_id": "",
            "turn_chain_id": "turn",
            "invocation_id": "call"
        }))
        .unwrap_err();

        assert!(error.to_string().contains("run_id"), "{error}");
    }

    #[test]
    fn deserialization_rejects_incomplete_tool_and_policy_fingerprints() {
        let empty_tool = serde_json::from_value::<DurableToolReference>(json!({
            "kind": "built_in",
            "tool_name": "",
            "registry_contract_version": "registry-v1"
        }))
        .unwrap_err();
        assert!(empty_tool.to_string().contains("tool_name"), "{empty_tool}");

        let empty_policy = serde_json::from_value::<ToolInvocationFingerprint>(json!({
            "tool": {
                "kind": "built_in",
                "tool_name": "bash",
                "registry_contract_version": "registry-v1"
            },
            "canonical_arguments_hash": "sha256:abc",
            "policy_decision_id": ""
        }))
        .unwrap_err();
        assert!(
            empty_policy.to_string().contains("policy decision"),
            "{empty_policy}"
        );
    }

    #[test]
    fn durable_record_rejects_terminal_state_without_matching_outcome() {
        let base = json!({
            "identity": identity("call-1"),
            "fingerprint": ToolInvocationFingerprint::new(
                tool_ref(),
                &json!({"command": "deploy"}),
                "policy-v1"
            ).unwrap(),
            "state": "succeeded",
            "dispatch_certainty": "dispatched",
            "attempt_count": 1
        });
        let missing = serde_json::from_value::<ToolInvocationRecord>(base.clone()).unwrap_err();
        assert!(missing.to_string().contains("missing"), "{missing}");

        let mut mismatched = base;
        mismatched["outcome"] = json!({
            "kind": "failed",
            "result": {"output": "failed"},
            "retryable": false
        });
        let mismatch = serde_json::from_value::<ToolInvocationRecord>(mismatched).unwrap_err();
        assert!(mismatch.to_string().contains("inconsistent"), "{mismatch}");
    }

    #[test]
    fn typed_terminal_outcome_round_trips_replay_payload() {
        let outcome = ToolInvocationTerminalOutcome::Failed {
            result: ToolInvocationResultPayload {
                output: "permission denied".to_string(),
                metadata: BTreeMap::from([(
                    "provider_trace".to_string(),
                    json!({"request": "req-7"}),
                )]),
                exit_semantics: Some("execution_error".to_string()),
            },
            error_kind: Some("permission_denied".to_string()),
            retryable: false,
        };
        let record = ToolInvocationRecord {
            identity: identity("call-1"),
            fingerprint: ToolInvocationFingerprint::new(
                tool_ref(),
                &json!({"command": "deploy"}),
                "policy-v1",
            )
            .unwrap(),
            state: ToolInvocationState::Failed,
            dispatch_certainty: DispatchCertainty::Dispatched,
            attempt_count: 1,
            outcome: Some(outcome),
        };

        let restored: ToolInvocationRecord =
            serde_json::from_value(serde_json::to_value(&record).unwrap()).unwrap();
        assert_eq!(restored, record);
        assert_eq!(
            restored.outcome.unwrap().result().output,
            "permission denied"
        );
    }
}
