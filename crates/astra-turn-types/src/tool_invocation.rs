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
/// Maximum durable projection of one acknowledged result. Raw provider bodies
/// above this boundary belong in an owner-scoped artifact, never inline in the
/// replay ledger.
pub const TOOL_INVOCATION_RESULT_MAX_BYTES: usize = 256 * 1024;
pub const TOOL_INVOCATION_RESULT_OUTPUT_MAX_BYTES: usize = 192 * 1024;
pub const TOOL_INVOCATION_RESULT_METADATA_MAX_BYTES: usize = 64 * 1024;
pub const TOOL_INVOCATION_RESULT_METADATA_MAX_DEPTH: usize = 16;
pub const TOOL_INVOCATION_RESULT_METADATA_MAX_NODES: usize = 4_096;
pub const TOOL_INVOCATION_RESULT_CLASSIFIER_MAX_BYTES: usize = 256;
pub const TOOL_INVOCATION_RESULT_ARTIFACT_METADATA_KEY: &str = "astraResultArtifact";
const TOOL_INVOCATION_RESULT_PROJECTED_OUTPUT_BYTES: usize = 180 * 1024;
const TOOL_INVOCATION_RESULT_PROJECTION_CONTRACT_VERSION: &str = "tool-result-projection-v1";
pub const TOOL_INVOCATION_CACHE_COMPLETION_CONTRACT_VERSION: &str =
    "tool-invocation-cache-completion-v1";
pub const TOOL_INVOCATION_RUN_CLOSURE_CONTRACT_VERSION: &str = "tool-invocation-run-closure-v1";
pub const TOOL_INVOCATION_GUIDANCE_COMPLETION_CONTRACT_VERSION: &str =
    "tool-invocation-guidance-completion-v1";

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
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ToolInvocationResultPayload {
    pub output: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_semantics: Option<String>,
}

impl ToolInvocationResultPayload {
    pub fn new(
        output: impl Into<String>,
        metadata: BTreeMap<String, Value>,
        exit_semantics: Option<String>,
    ) -> Result<Self, ToolInvocationContractError> {
        let result = Self {
            output: output.into(),
            metadata,
            exit_semantics,
        };
        result.validate()?;
        Ok(result)
    }

    pub fn validate(&self) -> Result<(), ToolInvocationContractError> {
        validate_bounded_string(
            "output",
            &self.output,
            TOOL_INVOCATION_RESULT_OUTPUT_MAX_BYTES,
        )?;
        if let Some(exit_semantics) = &self.exit_semantics {
            validate_bounded_string(
                "exit_semantics",
                exit_semantics,
                TOOL_INVOCATION_RESULT_CLASSIFIER_MAX_BYTES,
            )?;
        }
        validate_result_metadata(&self.metadata)?;
        let encoded_bytes = serde_json::to_vec(self)
            .map_err(|error| ToolInvocationContractError::ResultSerialization(error.to_string()))?
            .len();
        if encoded_bytes > TOOL_INVOCATION_RESULT_MAX_BYTES {
            return Err(ToolInvocationContractError::ResultPayloadTooLarge {
                actual_bytes: encoded_bytes,
                max_bytes: TOOL_INVOCATION_RESULT_MAX_BYTES,
            });
        }
        Ok(())
    }

    /// Build the replay-safe inline projection of an arbitrary provider
    /// result. Any omitted evidence is explicit and content-addressed; the
    /// caller can persist the original body as an owner-scoped artifact and
    /// replace `artifactRequired` with a concrete reference.
    pub fn bounded_projection(
        output: String,
        metadata: BTreeMap<String, Value>,
        exit_semantics: Option<String>,
    ) -> Self {
        if let Ok(payload) = Self::new(output.clone(), metadata.clone(), exit_semantics.clone()) {
            return payload;
        }

        let artifact_reference = metadata
            .get(TOOL_INVOCATION_RESULT_ARTIFACT_METADATA_KEY)
            .cloned();
        let output_too_large = output.len() > TOOL_INVOCATION_RESULT_OUTPUT_MAX_BYTES;
        let metadata_error = validate_result_metadata(&metadata).err();
        let exit_too_large = exit_semantics
            .as_ref()
            .is_some_and(|value| value.len() > TOOL_INVOCATION_RESULT_CLASSIFIER_MAX_BYTES);
        let projected_output = if output_too_large {
            truncate_utf8_head_tail(
                &output,
                TOOL_INVOCATION_RESULT_PROJECTED_OUTPUT_BYTES,
                "\n[… durable result projection omitted bytes; see astraResultProjection …]\n",
            )
        } else {
            output.clone()
        };

        let mut projection = serde_json::Map::new();
        projection.insert(
            "contractVersion".to_string(),
            Value::String(TOOL_INVOCATION_RESULT_PROJECTION_CONTRACT_VERSION.to_string()),
        );
        projection.insert(
            "artifactRequired".to_string(),
            Value::Bool(artifact_reference.is_none()),
        );
        if output_too_large {
            projection.insert(
                "output".to_string(),
                projection_evidence(
                    output.len(),
                    digest_bytes(output.as_bytes()),
                    "too_large",
                    "utf8",
                ),
            );
        }
        if let Some(error) = metadata_error.as_ref() {
            let (observed_bytes, size_kind) = metadata_observed_size(&metadata, error);
            projection.insert(
                "metadata".to_string(),
                projection_evidence(
                    observed_bytes,
                    stable_metadata_digest(&metadata),
                    result_metadata_error_code(error),
                    size_kind,
                ),
            );
        }
        if exit_too_large {
            let exit = exit_semantics.as_deref().unwrap_or_default();
            projection.insert(
                "exitSemantics".to_string(),
                projection_evidence(
                    exit.len(),
                    digest_bytes(exit.as_bytes()),
                    "too_large",
                    "utf8",
                ),
            );
        }

        let mut projected_metadata = if metadata_error.is_some() {
            BTreeMap::new()
        } else {
            metadata
        };
        if let Some(reference) = artifact_reference.clone() {
            projected_metadata.insert(
                TOOL_INVOCATION_RESULT_ARTIFACT_METADATA_KEY.to_string(),
                reference,
            );
        }
        projected_metadata.insert(
            "astraResultProjection".to_string(),
            Value::Object(projection.clone()),
        );
        let projected_exit = (!exit_too_large).then_some(exit_semantics).flatten();
        let candidate = Self {
            output: projected_output.clone(),
            metadata: projected_metadata,
            exit_semantics: projected_exit.clone(),
        };
        if candidate.validate().is_ok() {
            return candidate;
        }

        // Near-limit valid metadata may no longer fit after adding the
        // mandatory projection evidence. Replace it with its content address
        // rather than silently dropping the explanation.
        projection.entry("metadata".to_string()).or_insert_with(|| {
            projection_evidence(
                stable_metadata_size_hint(&candidate.metadata),
                stable_metadata_digest(&candidate.metadata),
                "projection_budget",
                "structural",
            )
        });
        let mut fallback_metadata = BTreeMap::from([(
            "astraResultProjection".to_string(),
            Value::Object(projection),
        )]);
        if let Some(reference) = artifact_reference {
            fallback_metadata.insert(
                TOOL_INVOCATION_RESULT_ARTIFACT_METADATA_KEY.to_string(),
                reference,
            );
        }
        let fallback = Self {
            output: projected_output,
            metadata: fallback_metadata,
            exit_semantics: projected_exit,
        };
        debug_assert!(fallback.validate().is_ok());
        fallback
    }
}

impl<'de> Deserialize<'de> for ToolInvocationResultPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawResultPayload {
            output: String,
            #[serde(default)]
            metadata: BTreeMap<String, Value>,
            #[serde(default)]
            exit_semantics: Option<String>,
        }

        let raw = RawResultPayload::deserialize(deserializer)?;
        Self::new(raw.output, raw.metadata, raw.exit_semantics).map_err(serde::de::Error::custom)
    }
}

/// Typed acknowledged outcome. Transport ambiguity is represented by the
/// ledger's `OutcomeUnknown` state and therefore has no fabricated payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
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
    RunClosure {
        contract_version: String,
        run_status: String,
    },
    SupersededByGuidance {
        contract_version: String,
        user_intent_event_index: i64,
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

    pub fn run_closure(run_status: impl Into<String>) -> Result<Self, ToolInvocationContractError> {
        let source = Self::RunClosure {
            contract_version: TOOL_INVOCATION_RUN_CLOSURE_CONTRACT_VERSION.to_string(),
            run_status: run_status.into(),
        };
        source.validate()?;
        Ok(source)
    }

    pub fn superseded_by_guidance(
        user_intent_event_index: i64,
    ) -> Result<Self, ToolInvocationContractError> {
        let source = Self::SupersededByGuidance {
            contract_version: TOOL_INVOCATION_GUIDANCE_COMPLETION_CONTRACT_VERSION.to_string(),
            user_intent_event_index,
        };
        source.validate()?;
        Ok(source)
    }

    pub fn validate(&self) -> Result<(), ToolInvocationContractError> {
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
            Self::RunClosure {
                contract_version,
                run_status,
            } => {
                if contract_version != TOOL_INVOCATION_RUN_CLOSURE_CONTRACT_VERSION {
                    return Err(
                        ToolInvocationContractError::UnsupportedRunClosureContractVersion(
                            contract_version.clone(),
                        ),
                    );
                }
                if !matches!(
                    run_status.as_str(),
                    "completed" | "delegated" | "failed" | "cancelled"
                ) {
                    return Err(ToolInvocationContractError::InvalidRunClosureStatus(
                        run_status.clone(),
                    ));
                }
                Ok(())
            }
            Self::SupersededByGuidance {
                contract_version,
                user_intent_event_index,
            } => {
                if contract_version != TOOL_INVOCATION_GUIDANCE_COMPLETION_CONTRACT_VERSION {
                    return Err(
                        ToolInvocationContractError::UnsupportedGuidanceCompletionContractVersion(
                            contract_version.clone(),
                        ),
                    );
                }
                if *user_intent_event_index < 0 {
                    return Err(
                        ToolInvocationContractError::InvalidGuidanceCompletionEventIndex(
                            *user_intent_event_index,
                        ),
                    );
                }
                Ok(())
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
            RunClosure {
                contract_version: String,
                run_status: String,
            },
            SupersededByGuidance {
                contract_version: String,
                user_intent_event_index: i64,
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
            RawSource::RunClosure {
                contract_version,
                run_status,
            } => Self::RunClosure {
                contract_version,
                run_status,
            },
            RawSource::SupersededByGuidance {
                contract_version,
                user_intent_event_index,
            } => Self::SupersededByGuidance {
                contract_version,
                user_intent_event_index,
            },
        };
        source.validate().map_err(serde::de::Error::custom)?;
        Ok(source)
    }
}

impl ToolInvocationTerminalOutcome {
    pub fn validate(&self) -> Result<(), ToolInvocationContractError> {
        self.result().validate()?;
        match self {
            Self::Succeeded { .. } => Ok(()),
            Self::Failed { error_kind, .. } => {
                validate_optional_classifier("error_kind", error_kind)
            }
            Self::Rejected { rejection_code, .. } => {
                validate_optional_classifier("rejection_code", rejection_code)
            }
        }
    }

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

impl<'de> Deserialize<'de> for ToolInvocationTerminalOutcome {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum RawTerminalOutcome {
            Succeeded {
                result: ToolInvocationResultPayload,
            },
            Failed {
                result: ToolInvocationResultPayload,
                #[serde(default)]
                error_kind: Option<String>,
                retryable: bool,
            },
            Rejected {
                result: ToolInvocationResultPayload,
                #[serde(default)]
                rejection_code: Option<String>,
                retryable: bool,
            },
        }

        let outcome = match RawTerminalOutcome::deserialize(deserializer)? {
            RawTerminalOutcome::Succeeded { result } => Self::Succeeded { result },
            RawTerminalOutcome::Failed {
                result,
                error_kind,
                retryable,
            } => Self::Failed {
                result,
                error_kind,
                retryable,
            },
            RawTerminalOutcome::Rejected {
                result,
                rejection_code,
                retryable,
            } => Self::Rejected {
                result,
                rejection_code,
                retryable,
            },
        };
        outcome.validate().map_err(serde::de::Error::custom)?;
        Ok(outcome)
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
        let non_dispatch_completion = self.completion_source.is_some();
        let required_certainty = if non_dispatch_completion {
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
        if let Some(completion_source) = &self.completion_source {
            match completion_source {
                ToolInvocationCompletionSource::SemanticReadCache { .. }
                    if self.state != ToolInvocationState::Succeeded =>
                {
                    return Err(ToolInvocationContractError::InvalidCacheCompletionState {
                        state: self.state,
                    });
                }
                ToolInvocationCompletionSource::RunClosure { .. }
                    if self.state != ToolInvocationState::Rejected =>
                {
                    return Err(
                        ToolInvocationContractError::InvalidRunClosureCompletionState {
                            state: self.state,
                        },
                    );
                }
                ToolInvocationCompletionSource::SupersededByGuidance { .. }
                    if self.state != ToolInvocationState::Rejected =>
                {
                    return Err(
                        ToolInvocationContractError::InvalidGuidanceCompletionState {
                            state: self.state,
                        },
                    );
                }
                _ => {}
            }
            if self.attempt_count != 0 {
                return Err(
                    ToolInvocationContractError::InvalidNonDispatchAttemptCount {
                        attempt_count: self.attempt_count,
                    },
                );
            }
            if self.dispatch_lease.is_some() {
                return Err(ToolInvocationContractError::NonDispatchCompletionHasDispatchLease);
            }
        }
        if let Some(outcome) = &self.outcome {
            outcome.validate()?;
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

fn validate_optional_classifier(
    field: &'static str,
    value: &Option<String>,
) -> Result<(), ToolInvocationContractError> {
    value.as_ref().map_or(Ok(()), |value| {
        validate_bounded_string(field, value, TOOL_INVOCATION_RESULT_CLASSIFIER_MAX_BYTES)
    })
}

fn truncate_utf8_head_tail(value: &str, max_bytes: usize, marker: &str) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let content_budget = max_bytes.saturating_sub(marker.len());
    let head_budget = content_budget / 2;
    let tail_budget = content_budget.saturating_sub(head_budget);
    let mut head_end = head_budget.min(value.len());
    while !value.is_char_boundary(head_end) {
        head_end = head_end.saturating_sub(1);
    }
    let mut tail_start = value.len().saturating_sub(tail_budget);
    while tail_start < value.len() && !value.is_char_boundary(tail_start) {
        tail_start = tail_start.saturating_add(1);
    }
    let mut projected = String::with_capacity(max_bytes);
    projected.push_str(&value[..head_end]);
    projected.push_str(marker);
    projected.push_str(&value[tail_start..]);
    projected
}

fn projection_evidence(
    observed_bytes: usize,
    content_hash: String,
    reason: &'static str,
    size_kind: &'static str,
) -> Value {
    serde_json::json!({
        "contentHash": content_hash,
        "observedBytes": observed_bytes,
        "sizeKind": size_kind,
        "reason": reason,
        "inline": false,
    })
}

fn result_metadata_error_code(error: &ToolInvocationContractError) -> &'static str {
    match error {
        ToolInvocationContractError::ResultMetadataTooLarge { .. } => "too_large",
        ToolInvocationContractError::ResultMetadataTooDeep { .. } => "too_deep",
        ToolInvocationContractError::ResultMetadataTooManyNodes { .. } => "too_many_nodes",
        ToolInvocationContractError::ResultSerialization(_) => "serialization_failed",
        _ => "payload_budget",
    }
}

fn metadata_observed_size(
    metadata: &BTreeMap<String, Value>,
    error: &ToolInvocationContractError,
) -> (usize, &'static str) {
    match error {
        ToolInvocationContractError::ResultMetadataTooLarge { actual_bytes, .. } => {
            (*actual_bytes, "json")
        }
        _ => (stable_metadata_size_hint(metadata), "structural"),
    }
}

fn digest_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn stable_metadata_digest(metadata: &BTreeMap<String, Value>) -> String {
    let mut digest = Sha256::new();
    digest.update(b"astra-tool-result-metadata-v1\0");
    update_len(&mut digest, metadata.len());
    for (key, value) in metadata {
        update_tagged_bytes(&mut digest, b'K', key.as_bytes());
        update_json_value_digest(&mut digest, value);
    }
    format!("sha256:{:x}", digest.finalize())
}

fn stable_metadata_size_hint(metadata: &BTreeMap<String, Value>) -> usize {
    metadata.iter().fold(0_usize, |size, (key, value)| {
        size.saturating_add(key.len())
            .saturating_add(json_value_structural_size(value))
    })
}

enum JsonDigestFrame<'a> {
    Value(&'a Value),
    Key(&'a str),
}

fn update_json_value_digest(digest: &mut Sha256, root: &Value) {
    let mut pending = vec![JsonDigestFrame::Value(root)];
    while let Some(frame) = pending.pop() {
        match frame {
            JsonDigestFrame::Key(key) => update_tagged_bytes(digest, b'K', key.as_bytes()),
            JsonDigestFrame::Value(value) => match value {
                Value::Null => digest.update(b"N"),
                Value::Bool(value) => digest.update(if *value { b"T" } else { b"F" }),
                Value::Number(value) => {
                    update_tagged_bytes(digest, b'D', value.to_string().as_bytes());
                }
                Value::String(value) => update_tagged_bytes(digest, b'S', value.as_bytes()),
                Value::Array(values) => {
                    digest.update(b"A");
                    update_len(digest, values.len());
                    pending.extend(values.iter().rev().map(JsonDigestFrame::Value));
                }
                Value::Object(object) => {
                    digest.update(b"O");
                    update_len(digest, object.len());
                    let mut entries = object.iter().collect::<Vec<_>>();
                    entries.sort_unstable_by_key(|(key, _)| *key);
                    for (key, value) in entries.into_iter().rev() {
                        pending.push(JsonDigestFrame::Value(value));
                        pending.push(JsonDigestFrame::Key(key));
                    }
                }
            },
        }
    }
}

fn json_value_structural_size(root: &Value) -> usize {
    let mut size = 0_usize;
    let mut pending = vec![root];
    while let Some(value) = pending.pop() {
        size = size.saturating_add(1);
        match value {
            Value::Null | Value::Bool(_) => {}
            Value::Number(value) => size = size.saturating_add(value.to_string().len()),
            Value::String(value) => size = size.saturating_add(value.len()),
            Value::Array(values) => pending.extend(values),
            Value::Object(object) => {
                size = object
                    .keys()
                    .fold(size, |size, key| size.saturating_add(key.len()));
                pending.extend(object.values());
            }
        }
    }
    size
}

fn update_tagged_bytes(digest: &mut Sha256, tag: u8, bytes: &[u8]) {
    digest.update([tag]);
    update_len(digest, bytes.len());
    digest.update(bytes);
}

fn update_len(digest: &mut Sha256, len: usize) {
    digest.update(u64::try_from(len).unwrap_or(u64::MAX).to_be_bytes());
}

fn validate_bounded_string(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), ToolInvocationContractError> {
    if value.len() > max_bytes {
        return Err(ToolInvocationContractError::ResultFieldTooLarge {
            field,
            actual_bytes: value.len(),
            max_bytes,
        });
    }
    Ok(())
}

fn validate_result_metadata(
    metadata: &BTreeMap<String, Value>,
) -> Result<(), ToolInvocationContractError> {
    let mut nodes = metadata.len();
    let mut pending = metadata
        .values()
        .map(|value| (value, 1_usize))
        .collect::<Vec<_>>();
    while let Some((value, depth)) = pending.pop() {
        if depth > TOOL_INVOCATION_RESULT_METADATA_MAX_DEPTH {
            return Err(ToolInvocationContractError::ResultMetadataTooDeep {
                actual_depth: depth,
                max_depth: TOOL_INVOCATION_RESULT_METADATA_MAX_DEPTH,
            });
        }
        nodes = nodes.saturating_add(1);
        if nodes > TOOL_INVOCATION_RESULT_METADATA_MAX_NODES {
            return Err(ToolInvocationContractError::ResultMetadataTooManyNodes {
                actual_nodes: nodes,
                max_nodes: TOOL_INVOCATION_RESULT_METADATA_MAX_NODES,
            });
        }
        match value {
            Value::Array(values) => pending.extend(
                values
                    .iter()
                    .map(|nested| (nested, depth.saturating_add(1))),
            ),
            Value::Object(object) => {
                nodes = nodes.saturating_add(object.len());
                if nodes > TOOL_INVOCATION_RESULT_METADATA_MAX_NODES {
                    return Err(ToolInvocationContractError::ResultMetadataTooManyNodes {
                        actual_nodes: nodes,
                        max_nodes: TOOL_INVOCATION_RESULT_METADATA_MAX_NODES,
                    });
                }
                pending.extend(
                    object
                        .values()
                        .map(|nested| (nested, depth.saturating_add(1))),
                );
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }
    let encoded_bytes = serde_json::to_vec(metadata)
        .map_err(|error| ToolInvocationContractError::ResultSerialization(error.to_string()))?
        .len();
    if encoded_bytes > TOOL_INVOCATION_RESULT_METADATA_MAX_BYTES {
        return Err(ToolInvocationContractError::ResultMetadataTooLarge {
            actual_bytes: encoded_bytes,
            max_bytes: TOOL_INVOCATION_RESULT_METADATA_MAX_BYTES,
        });
    }
    Ok(())
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
    #[error("unsupported tool invocation run-closure contract version '{0}'")]
    UnsupportedRunClosureContractVersion(String),
    #[error("unsupported tool invocation guidance-completion contract version '{0}'")]
    UnsupportedGuidanceCompletionContractVersion(String),
    #[error("tool invocation run-closure status is not terminal: '{0}'")]
    InvalidRunClosureStatus(String),
    #[error("tool invocation cache-completion {field} is not a canonical SHA-256 content ID")]
    InvalidCompletionContentId { field: &'static str },
    #[error("semantic cache completion cannot produce ledger state {state:?}")]
    InvalidCacheCompletionState { state: ToolInvocationState },
    #[error("run closure completion cannot produce ledger state {state:?}")]
    InvalidRunClosureCompletionState { state: ToolInvocationState },
    #[error("guidance completion cannot produce ledger state {state:?}")]
    InvalidGuidanceCompletionState { state: ToolInvocationState },
    #[error("guidance completion event index must be non-negative, got {0}")]
    InvalidGuidanceCompletionEventIndex(i64),
    #[error("a non-dispatch completion cannot have {attempt_count} provider dispatch attempts")]
    InvalidNonDispatchAttemptCount { attempt_count: u32 },
    #[error("a non-dispatch completion cannot retain a provider dispatch lease")]
    NonDispatchCompletionHasDispatchLease,
    #[error("serialize tool invocation result payload: {0}")]
    ResultSerialization(String),
    #[error(
        "tool invocation result field '{field}' is too large: {actual_bytes} bytes exceeds {max_bytes}"
    )]
    ResultFieldTooLarge {
        field: &'static str,
        actual_bytes: usize,
        max_bytes: usize,
    },
    #[error(
        "tool invocation result metadata is too large: {actual_bytes} bytes exceeds {max_bytes}"
    )]
    ResultMetadataTooLarge {
        actual_bytes: usize,
        max_bytes: usize,
    },
    #[error(
        "tool invocation result metadata is too deep: depth {actual_depth} exceeds {max_depth}"
    )]
    ResultMetadataTooDeep {
        actual_depth: usize,
        max_depth: usize,
    },
    #[error(
        "tool invocation result metadata has too many nodes: {actual_nodes} exceeds {max_nodes}"
    )]
    ResultMetadataTooManyNodes {
        actual_nodes: usize,
        max_nodes: usize,
    },
    #[error(
        "tool invocation result payload is too large: {actual_bytes} bytes exceeds {max_bytes}"
    )]
    ResultPayloadTooLarge {
        actual_bytes: usize,
        max_bytes: usize,
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
    fn durable_result_bounds_are_utf8_byte_exact_and_rechecked_on_decode() {
        let exact = "界".repeat(TOOL_INVOCATION_RESULT_OUTPUT_MAX_BYTES / 3);
        let payload = ToolInvocationResultPayload::new(exact, BTreeMap::new(), None).unwrap();
        assert_eq!(
            payload.output.len(),
            TOOL_INVOCATION_RESULT_OUTPUT_MAX_BYTES
        );

        let oversized = "界".repeat(TOOL_INVOCATION_RESULT_OUTPUT_MAX_BYTES / 3 + 1);
        let error =
            ToolInvocationResultPayload::new(oversized.clone(), BTreeMap::new(), None).unwrap_err();
        assert!(matches!(
            error,
            ToolInvocationContractError::ResultFieldTooLarge {
                field: "output",
                ..
            }
        ));

        let forged = json!({"output": oversized});
        let decode_error =
            serde_json::from_value::<ToolInvocationResultPayload>(forged).unwrap_err();
        assert!(
            decode_error.to_string().contains("output"),
            "{decode_error}"
        );
    }

    #[test]
    fn durable_result_rejects_deep_and_high_cardinality_metadata() {
        let mut nested = Value::String("leaf".to_string());
        for _ in 0..=TOOL_INVOCATION_RESULT_METADATA_MAX_DEPTH {
            nested = json!({"nested": nested});
        }
        let deep = ToolInvocationResultPayload::new(
            "ok",
            BTreeMap::from([("root".to_string(), nested)]),
            None,
        )
        .unwrap_err();
        assert!(matches!(
            deep,
            ToolInvocationContractError::ResultMetadataTooDeep { .. }
        ));

        let many = Value::Array(
            (0..TOOL_INVOCATION_RESULT_METADATA_MAX_NODES)
                .map(|_| Value::Null)
                .collect(),
        );
        let high_cardinality = ToolInvocationResultPayload::new(
            "ok",
            BTreeMap::from([("items".to_string(), many)]),
            None,
        )
        .unwrap_err();
        assert!(matches!(
            high_cardinality,
            ToolInvocationContractError::ResultMetadataTooManyNodes { .. }
        ));
    }

    #[test]
    fn terminal_outcome_rejects_unbounded_classifier_without_a_record_wrapper() {
        let encoded = json!({
            "kind": "failed",
            "result": {"output": "failed"},
            "error_kind": "x".repeat(TOOL_INVOCATION_RESULT_CLASSIFIER_MAX_BYTES + 1),
            "retryable": false
        });
        let error = serde_json::from_value::<ToolInvocationTerminalOutcome>(encoded).unwrap_err();
        assert!(error.to_string().contains("error_kind"), "{error}");
    }

    #[test]
    fn bounded_projection_is_valid_explicit_and_content_addressed() {
        let raw_output = "界".repeat(TOOL_INVOCATION_RESULT_OUTPUT_MAX_BYTES);
        let raw_metadata = BTreeMap::from([(
            "payload".to_string(),
            Value::Array(
                (0..TOOL_INVOCATION_RESULT_METADATA_MAX_NODES)
                    .map(|_| Value::Null)
                    .collect(),
            ),
        )]);

        let projected = ToolInvocationResultPayload::bounded_projection(
            raw_output.clone(),
            raw_metadata.clone(),
            None,
        );
        projected.validate().unwrap();
        assert!(projected.output.len() <= TOOL_INVOCATION_RESULT_PROJECTED_OUTPUT_BYTES);
        assert!(projected.output.contains("durable result projection"));
        let evidence = &projected.metadata["astraResultProjection"];
        assert_eq!(evidence["artifactRequired"], true);
        assert_eq!(evidence["output"]["observedBytes"], raw_output.len());
        assert_eq!(evidence["output"]["sizeKind"], "utf8");
        assert_eq!(
            evidence["output"]["contentHash"],
            digest_bytes(raw_output.as_bytes())
        );
        assert_eq!(evidence["metadata"]["reason"], "too_many_nodes");
        assert_eq!(
            evidence["metadata"]["contentHash"],
            stable_metadata_digest(&raw_metadata)
        );
    }

    #[test]
    fn bounded_projection_preserves_already_valid_payload_exactly() {
        let metadata = BTreeMap::from([("trace".to_string(), json!({"request": "r-1"}))]);
        let projected = ToolInvocationResultPayload::bounded_projection(
            "ok".to_string(),
            metadata.clone(),
            Some("success".to_string()),
        );
        assert_eq!(projected.output, "ok");
        assert_eq!(projected.metadata, metadata);
        assert_eq!(projected.exit_semantics.as_deref(), Some("success"));
    }

    #[test]
    fn bounded_projection_preserves_owner_artifact_reference_when_metadata_is_replaced() {
        let artifact = json!({
            "artifactId": "artifact-1",
            "artifactKind": "tool_result_evidence_v1",
            "contentHash": format!("sha256:{}", "a".repeat(64)),
        });
        let metadata = BTreeMap::from([
            (
                "payload".to_string(),
                Value::Array(
                    (0..TOOL_INVOCATION_RESULT_METADATA_MAX_NODES)
                        .map(|_| Value::Null)
                        .collect(),
                ),
            ),
            (
                TOOL_INVOCATION_RESULT_ARTIFACT_METADATA_KEY.to_string(),
                artifact.clone(),
            ),
        ]);
        let projected =
            ToolInvocationResultPayload::bounded_projection("ok".to_string(), metadata, None);
        projected.validate().unwrap();
        assert_eq!(
            projected.metadata[TOOL_INVOCATION_RESULT_ARTIFACT_METADATA_KEY],
            artifact
        );
        assert_eq!(
            projected.metadata["astraResultProjection"]["artifactRequired"],
            false
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
    fn run_closure_rejects_prepared_work_without_fabricating_dispatch() {
        let decision = decision();
        let record = ToolInvocationRecord {
            identity: identity("call-closed"),
            fingerprint: ToolInvocationFingerprint::new(
                tool_ref(),
                &json!({"command": "mutate"}),
                &decision.decision_id,
            )
            .unwrap(),
            decision,
            state: ToolInvocationState::Rejected,
            dispatch_certainty: DispatchCertainty::NotDispatched,
            attempt_count: 0,
            dispatch_lease: None,
            outcome: Some(ToolInvocationTerminalOutcome::Rejected {
                result: ToolInvocationResultPayload::new(
                    "run closed".to_string(),
                    BTreeMap::new(),
                    None,
                )
                .unwrap(),
                rejection_code: Some("run_closed".to_string()),
                retryable: false,
            }),
            completion_source: Some(
                ToolInvocationCompletionSource::run_closure("completed").unwrap(),
            ),
        };
        record.validate().unwrap();
        let encoded = serde_json::to_value(&record).unwrap();
        assert_eq!(
            serde_json::from_value::<ToolInvocationRecord>(encoded.clone()).unwrap(),
            record
        );

        let mut wrong_state = encoded.clone();
        wrong_state["state"] = json!("succeeded");
        wrong_state["outcome"] = json!({
            "kind": "succeeded",
            "result": {"output": "forged"}
        });
        assert!(serde_json::from_value::<ToolInvocationRecord>(wrong_state).is_err());

        let mut non_terminal_status = encoded;
        non_terminal_status["completion_source"]["run_status"] = json!("running");
        assert!(serde_json::from_value::<ToolInvocationRecord>(non_terminal_status).is_err());
    }

    #[test]
    fn guidance_supersession_is_terminal_without_fabricating_dispatch() {
        let decision = decision();
        let record = ToolInvocationRecord {
            identity: identity("call-superseded"),
            fingerprint: ToolInvocationFingerprint::new(
                tool_ref(),
                &json!({"command": "mutate"}),
                &decision.decision_id,
            )
            .unwrap(),
            decision,
            state: ToolInvocationState::Rejected,
            dispatch_certainty: DispatchCertainty::NotDispatched,
            attempt_count: 0,
            dispatch_lease: None,
            outcome: Some(ToolInvocationTerminalOutcome::Rejected {
                result: ToolInvocationResultPayload::new(
                    "superseded by guidance".to_string(),
                    BTreeMap::new(),
                    None,
                )
                .unwrap(),
                rejection_code: Some("superseded_by_guidance".to_string()),
                retryable: false,
            }),
            completion_source: Some(
                ToolInvocationCompletionSource::superseded_by_guidance(7).unwrap(),
            ),
        };
        record.validate().unwrap();
        let encoded = serde_json::to_value(&record).unwrap();
        assert_eq!(
            serde_json::from_value::<ToolInvocationRecord>(encoded.clone()).unwrap(),
            record
        );

        let mut attempted = encoded.clone();
        attempted["attempt_count"] = json!(1);
        assert!(serde_json::from_value::<ToolInvocationRecord>(attempted).is_err());

        let mut wrong_state = encoded.clone();
        wrong_state["state"] = json!("succeeded");
        wrong_state["outcome"] = json!({
            "kind": "succeeded",
            "result": {"output": "forged"}
        });
        assert!(serde_json::from_value::<ToolInvocationRecord>(wrong_state).is_err());

        let mut invalid_event = encoded;
        invalid_event["completion_source"]["user_intent_event_index"] = json!(-1);
        assert!(serde_json::from_value::<ToolInvocationRecord>(invalid_event).is_err());
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
