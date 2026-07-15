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

pub const TOOL_INVOCATION_CONTRACT_VERSION: &str = "tool-invocation-v2";
pub const TOOL_INVOCATION_DISPATCH_OWNER_MAX_BYTES: usize = 64;
pub const TOOL_INVOCATION_CACHE_COMPLETION_CONTRACT_VERSION: &str =
    "tool-invocation-cache-completion-v1";

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

    /// Facts that identify the caller's immutable tool input. A mutable
    /// policy environment may differ on resume; the durable decision stored
    /// with the original invocation remains authoritative in that case.
    pub fn same_tool_and_arguments(&self, other: &Self) -> bool {
        self.tool == other.tool && self.canonical_arguments_hash == other.canonical_arguments_hash
    }
}

/// Protocol-independent durable envelope for the complete frozen dispatch
/// decision. Runtime-specific fields remain opaque to storage adapters, while
/// the content ID is validated at every deserialize boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ToolInvocationDecision {
    pub decision_id: String,
    pub snapshot: Value,
}

impl ToolInvocationDecision {
    pub fn new<T: Serialize>(snapshot: &T) -> Result<Self, ToolInvocationContractError> {
        let snapshot = serde_json::to_value(snapshot).map_err(|error| {
            ToolInvocationContractError::DecisionSerialization(error.to_string())
        })?;
        Self::from_snapshot(snapshot)
    }

    pub fn from_snapshot(snapshot: Value) -> Result<Self, ToolInvocationContractError> {
        if !snapshot.is_object() {
            return Err(ToolInvocationContractError::InvalidDecisionSnapshot);
        }
        let snapshot = canonical_json(&snapshot);
        let decision_id = decision_content_id(&snapshot);
        Ok(Self {
            decision_id,
            snapshot,
        })
    }

    fn validate(&self) -> Result<(), ToolInvocationContractError> {
        if !self.snapshot.is_object() {
            return Err(ToolInvocationContractError::InvalidDecisionSnapshot);
        }
        let expected = decision_content_id(&canonical_json(&self.snapshot));
        if self.decision_id != expected {
            return Err(ToolInvocationContractError::DecisionContentIdMismatch {
                expected,
                actual: self.decision_id.clone(),
            });
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for ToolInvocationDecision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawDecision {
            decision_id: String,
            snapshot: Value,
        }
        let raw = RawDecision::deserialize(deserializer)?;
        let decision = Self {
            decision_id: raw.decision_id,
            snapshot: raw.snapshot,
        };
        decision.validate().map_err(serde::de::Error::custom)?;
        Ok(decision)
    }
}

fn decision_content_id(snapshot: &Value) -> String {
    let encoded = serde_json::to_vec(snapshot)
        .expect("canonical tool invocation decision must serialize to JSON");
    format!("sha256:{:x}", Sha256::digest(encoded))
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

/// Exclusive ownership proof for one route-boundary dispatch. The owner token
/// is opaque and unguessable; the deadline is a liveness lease, not an
/// execution timeout. A live worker renews it while the provider call runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ToolInvocationDispatchLease {
    pub owner_id: String,
    pub expires_at_epoch_ms: u64,
}

impl ToolInvocationDispatchLease {
    pub fn new(
        owner_id: impl Into<String>,
        expires_at_epoch_ms: u64,
    ) -> Result<Self, ToolInvocationContractError> {
        let owner_id = owner_id.into();
        if owner_id.trim().is_empty() {
            return Err(ToolInvocationContractError::EmptyDispatchOwner);
        }
        if owner_id.len() > TOOL_INVOCATION_DISPATCH_OWNER_MAX_BYTES {
            return Err(ToolInvocationContractError::DispatchOwnerTooLong {
                actual_bytes: owner_id.len(),
                max_bytes: TOOL_INVOCATION_DISPATCH_OWNER_MAX_BYTES,
            });
        }
        if expires_at_epoch_ms == 0 {
            return Err(ToolInvocationContractError::InvalidDispatchLeaseExpiry);
        }
        Ok(Self {
            owner_id,
            expires_at_epoch_ms,
        })
    }

    pub fn is_expired_at(&self, now_epoch_ms: u64) -> bool {
        self.expires_at_epoch_ms <= now_epoch_ms
    }
}

impl<'de> Deserialize<'de> for ToolInvocationDispatchLease {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawLease {
            owner_id: String,
            expires_at_epoch_ms: u64,
        }

        let raw = RawLease::deserialize(deserializer)?;
        Self::new(raw.owner_id, raw.expires_at_epoch_ms).map_err(serde::de::Error::custom)
    }
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

/// Provenance for a terminal invocation that did not cross the selected
/// provider route boundary. Absence means the terminal outcome followed a
/// provider dispatch, preserving compatibility with pre-cache ledger rows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolInvocationCompletionSource {
    SemanticReadCache {
        contract_version: String,
        cache_key_id: String,
        observation_id: String,
    },
}

impl ToolInvocationCompletionSource {
    pub fn semantic_read_cache(
        cache_key_id: impl Into<String>,
        observation_id: impl Into<String>,
    ) -> Result<Self, ToolInvocationContractError> {
        let source = Self::SemanticReadCache {
            contract_version: TOOL_INVOCATION_CACHE_COMPLETION_CONTRACT_VERSION.to_string(),
            cache_key_id: cache_key_id.into(),
            observation_id: observation_id.into(),
        };
        source.validate()?;
        Ok(source)
    }

    fn validate(&self) -> Result<(), ToolInvocationContractError> {
        match self {
            Self::SemanticReadCache {
                contract_version,
                cache_key_id,
                observation_id,
            } => {
                if contract_version != TOOL_INVOCATION_CACHE_COMPLETION_CONTRACT_VERSION {
                    return Err(
                        ToolInvocationContractError::UnsupportedCacheCompletionContractVersion(
                            contract_version.clone(),
                        ),
                    );
                }
                validate_sha256_content_id("cache_key_id", cache_key_id)?;
                validate_sha256_content_id("observation_id", observation_id)
            }
        }
    }
}

impl<'de> Deserialize<'de> for ToolInvocationCompletionSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum RawSource {
            SemanticReadCache {
                contract_version: String,
                cache_key_id: String,
                observation_id: String,
            },
        }

        let source = match RawSource::deserialize(deserializer)? {
            RawSource::SemanticReadCache {
                contract_version,
                cache_key_id,
                observation_id,
            } => Self::SemanticReadCache {
                contract_version,
                cache_key_id,
                observation_id,
            },
        };
        source.validate().map_err(serde::de::Error::custom)?;
        Ok(source)
    }
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
    pub decision: ToolInvocationDecision,
    pub state: ToolInvocationState,
    pub dispatch_certainty: DispatchCertainty,
    pub attempt_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_lease: Option<ToolInvocationDispatchLease>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<ToolInvocationTerminalOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_source: Option<ToolInvocationCompletionSource>,
}

impl ToolInvocationRecord {
    pub fn validate(&self) -> Result<(), ToolInvocationContractError> {
        self.decision.validate()?;
        if self.fingerprint.policy_decision_id != self.decision.decision_id {
            return Err(ToolInvocationContractError::RecordDecisionMismatch {
                fingerprint_decision_id: self.fingerprint.policy_decision_id.clone(),
                durable_decision_id: self.decision.decision_id.clone(),
            });
        }
        if let Some(source) = &self.completion_source {
            source.validate()?;
        }
        let cache_completion = self.completion_source.is_some();
        let required_certainty = if cache_completion {
            DispatchCertainty::NotDispatched
        } else {
            self.state.required_dispatch_certainty()
        };
        if self.dispatch_certainty != required_certainty {
            return Err(ToolInvocationContractError::RecordCertaintyMismatch {
                state: self.state,
                expected: required_certainty,
                actual: self.dispatch_certainty,
            });
        }
        match (&self.dispatch_lease, self.state) {
            (None, ToolInvocationState::Dispatched) => {
                return Err(ToolInvocationContractError::MissingDispatchLease);
            }
            (Some(_), ToolInvocationState::Prepared) => {
                return Err(ToolInvocationContractError::UnexpectedDispatchLease);
            }
            _ => {}
        }
        if cache_completion {
            if self.state != ToolInvocationState::Succeeded {
                return Err(ToolInvocationContractError::InvalidCacheCompletionState {
                    state: self.state,
                });
            }
            if self.attempt_count != 0 {
                return Err(
                    ToolInvocationContractError::InvalidCacheCompletionAttemptCount {
                        attempt_count: self.attempt_count,
                    },
                );
            }
            if self.dispatch_lease.is_some() {
                return Err(ToolInvocationContractError::CacheCompletionHasDispatchLease);
            }
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
            decision: ToolInvocationDecision,
            state: ToolInvocationState,
            dispatch_certainty: DispatchCertainty,
            attempt_count: u32,
            #[serde(default)]
            dispatch_lease: Option<ToolInvocationDispatchLease>,
            #[serde(default)]
            outcome: Option<ToolInvocationTerminalOutcome>,
            #[serde(default)]
            completion_source: Option<ToolInvocationCompletionSource>,
        }

        let raw = RawRecord::deserialize(deserializer)?;
        let record = Self {
            identity: raw.identity,
            fingerprint: raw.fingerprint,
            decision: raw.decision,
            state: raw.state,
            dispatch_certainty: raw.dispatch_certainty,
            attempt_count: raw.attempt_count,
            dispatch_lease: raw.dispatch_lease,
            outcome: raw.outcome,
            completion_source: raw.completion_source,
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

fn validate_sha256_content_id(
    field: &'static str,
    value: &str,
) -> Result<(), ToolInvocationContractError> {
    let valid = value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    });
    if valid {
        Ok(())
    } else {
        Err(ToolInvocationContractError::InvalidCompletionContentId { field })
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
    #[error("tool invocation dispatch owner must not be empty")]
    EmptyDispatchOwner,
    #[error("tool invocation dispatch owner is too long: {actual_bytes} bytes exceeds {max_bytes}")]
    DispatchOwnerTooLong {
        actual_bytes: usize,
        max_bytes: usize,
    },
    #[error("tool invocation dispatch lease expiry must be a positive epoch timestamp")]
    InvalidDispatchLeaseExpiry,
    #[error("tool invocation decision snapshot must be a JSON object")]
    InvalidDecisionSnapshot,
    #[error("serialize tool invocation decision: {0}")]
    DecisionSerialization(String),
    #[error("tool invocation decision content id mismatch: expected {expected}, found {actual}")]
    DecisionContentIdMismatch { expected: String, actual: String },
    #[error(
        "tool invocation fingerprint decision {fingerprint_decision_id} does not match durable decision {durable_decision_id}"
    )]
    RecordDecisionMismatch {
        fingerprint_decision_id: String,
        durable_decision_id: String,
    },
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
    #[error("a dispatched tool invocation is missing its owner lease")]
    MissingDispatchLease,
    #[error("a prepared tool invocation must not already have a dispatch lease")]
    UnexpectedDispatchLease,
    #[error("unsupported tool invocation cache-completion contract version '{0}'")]
    UnsupportedCacheCompletionContractVersion(String),
    #[error("tool invocation cache-completion {field} is not a canonical SHA-256 content ID")]
    InvalidCompletionContentId { field: &'static str },
    #[error("semantic cache completion cannot produce ledger state {state:?}")]
    InvalidCacheCompletionState { state: ToolInvocationState },
    #[error("semantic cache completion cannot have {attempt_count} provider dispatch attempts")]
    InvalidCacheCompletionAttemptCount { attempt_count: u32 },
    #[error("semantic cache completion cannot retain a provider dispatch lease")]
    CacheCompletionHasDispatchLease,
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

    fn decision() -> ToolInvocationDecision {
        ToolInvocationDecision::new(&json!({
            "contract_version": "test-v1",
            "route": "server_local"
        }))
        .unwrap()
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
        let decision = decision();
        let base = json!({
            "identity": identity("call-1"),
            "fingerprint": ToolInvocationFingerprint::new(
                tool_ref(),
                &json!({"command": "deploy"}),
                &decision.decision_id
            ).unwrap(),
            "decision": decision,
            "state": "succeeded",
            "dispatch_certainty": "dispatched",
            "attempt_count": 1
        });
        let missing = serde_json::from_value::<ToolInvocationRecord>(base.clone()).unwrap_err();
        assert!(missing.to_string().contains("missing"), "{missing}");

        let mut dispatched_without_owner = base.clone();
        dispatched_without_owner["state"] = json!("dispatched");
        let missing_lease =
            serde_json::from_value::<ToolInvocationRecord>(dispatched_without_owner).unwrap_err();
        assert!(
            missing_lease.to_string().contains("owner lease"),
            "{missing_lease}"
        );

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
        let decision = decision();
        let record = ToolInvocationRecord {
            identity: identity("call-1"),
            fingerprint: ToolInvocationFingerprint::new(
                tool_ref(),
                &json!({"command": "deploy"}),
                &decision.decision_id,
            )
            .unwrap(),
            decision,
            state: ToolInvocationState::Failed,
            dispatch_certainty: DispatchCertainty::Dispatched,
            attempt_count: 1,
            dispatch_lease: None,
            outcome: Some(outcome),
            completion_source: None,
        };

        let restored: ToolInvocationRecord =
            serde_json::from_value(serde_json::to_value(&record).unwrap()).unwrap();
        assert_eq!(restored, record);
        assert_eq!(
            restored.outcome.unwrap().result().output,
            "permission denied"
        );
    }

    #[test]
    fn cache_completion_is_successful_without_provider_dispatch() {
        let decision = decision();
        let record = ToolInvocationRecord {
            identity: identity("call-cache"),
            fingerprint: ToolInvocationFingerprint::new(
                tool_ref(),
                &json!({"command": "read"}),
                &decision.decision_id,
            )
            .unwrap(),
            decision,
            state: ToolInvocationState::Succeeded,
            dispatch_certainty: DispatchCertainty::NotDispatched,
            attempt_count: 0,
            dispatch_lease: None,
            outcome: Some(ToolInvocationTerminalOutcome::Succeeded {
                result: ToolInvocationResultPayload {
                    output: "cached observation".to_string(),
                    metadata: BTreeMap::new(),
                    exit_semantics: None,
                },
            }),
            completion_source: Some(
                ToolInvocationCompletionSource::semantic_read_cache(
                    format!("sha256:{}", "a".repeat(64)),
                    format!("sha256:{}", "b".repeat(64)),
                )
                .unwrap(),
            ),
        };
        record.validate().unwrap();
        let encoded = serde_json::to_value(&record).unwrap();
        assert_eq!(
            serde_json::from_value::<ToolInvocationRecord>(encoded.clone()).unwrap(),
            record
        );

        let mut dispatched = encoded.clone();
        dispatched["dispatch_certainty"] = json!("dispatched");
        assert!(serde_json::from_value::<ToolInvocationRecord>(dispatched).is_err());

        let mut attempted = encoded.clone();
        attempted["attempt_count"] = json!(1);
        assert!(serde_json::from_value::<ToolInvocationRecord>(attempted).is_err());

        let mut forged = encoded;
        forged["completion_source"]["cache_key_id"] = json!("sha256:short");
        assert!(serde_json::from_value::<ToolInvocationRecord>(forged).is_err());
    }

    #[test]
    fn durable_decision_rejects_forged_content_id() {
        let decision = decision();
        let mut encoded = serde_json::to_value(decision).unwrap();
        encoded["snapshot"]["route"] = json!("edge_bound");

        let error = serde_json::from_value::<ToolInvocationDecision>(encoded).unwrap_err();
        assert!(error.to_string().contains("content id mismatch"), "{error}");
    }

    #[test]
    fn dispatch_lease_rejects_empty_owner_and_zero_deadline() {
        for invalid in [
            json!({"owner_id": "", "expires_at_epoch_ms": 1}),
            json!({"owner_id": "worker-1", "expires_at_epoch_ms": 0}),
            json!({
                "owner_id": "x".repeat(TOOL_INVOCATION_DISPATCH_OWNER_MAX_BYTES + 1),
                "expires_at_epoch_ms": 1
            }),
        ] {
            assert!(serde_json::from_value::<ToolInvocationDispatchLease>(invalid).is_err());
        }
    }
}
