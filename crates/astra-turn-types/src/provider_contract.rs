//! Protocol-independent provider discovery and tool identity contracts.
//!
//! Provider adapters decode wire-specific declarations into these portable
//! facts. They intentionally do not decide permission, retry, caching, prompt
//! placement, or result projection policy.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Internal function-schema extension carrying the producer-owned alias that
/// capability contracts use to select a tool independently of its runtime
/// public name. Model adapters strip all `x-astra-*` extensions before wire
/// serialization.
pub const STABLE_TOOL_ALIAS_SCHEMA_KEY: &str = "x-astra-stable-tool-alias";

/// Namespaced MCP `_meta` field through which a provider publishes the stable
/// alias used by capability contracts. Adapters validate and carry this value;
/// consumers must never infer it from a runtime-qualified tool name.
pub const STABLE_TOOL_ALIAS_METADATA_KEY: &str = "astra/stableToolAlias";

pub const PROVIDER_RUNTIME_REQUIREMENTS_KEY: &str = "astra.runtimeRequirements";

/// Bounded observation of one native stage, not execution authority or a
/// physical model-attempt receipt. Internal tool activity is not reported by
/// this protocol and must not be inferred from the outer invocation result.
pub const NATIVE_COLLABORATOR_OBSERVATION_KEY: &str = "native_stage_observation";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeCollaboratorObservation {
    pub native_session_id: Option<String>,
    pub native_turn_id: Option<String>,
    pub dispatch_state: NativeStageDispatchState,
    pub native_terminal: Option<String>,
    pub settlement_authoritative: bool,
    pub stage_inclusive_input_tokens: Option<u64>,
    pub stage_usage: Option<crate::CanonicalTokenUsage>,
    pub last_request_input_tokens: Option<u64>,
    pub model_context_window: Option<u64>,
    pub acknowledged_model: Option<String>,
    pub provider_error_code: Option<i64>,
    pub provider_error_class: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeStageDispatchState {
    Acknowledged,
    Unknown,
    NotDispatched,
}

/// One projection used by the producer, durable event and external observation
/// boundaries. No provider payload, prompt or unbounded error string survives.
pub fn project_native_collaborator_observation(value: &Value) -> Option<Value> {
    let object = value.as_object()?;
    if object.len() > 12
        || object.iter().any(|(key, value)| {
            key.len() > 64 || value.as_str().is_some_and(|text| text.len() > 256)
        })
        || object.get("stage_usage").is_some_and(|usage| {
            !usage.is_null()
                && !usage
                    .as_object()
                    .is_some_and(|usage| usage.len() <= 6 && usage.values().all(Value::is_u64))
        })
    {
        return None;
    }
    let observation = NativeCollaboratorObservation::deserialize(value).ok()?;
    if matches!(
        observation.dispatch_state,
        NativeStageDispatchState::Acknowledged
    ) && (observation
        .native_session_id
        .as_deref()
        .is_none_or(str::is_empty)
        || observation
            .native_turn_id
            .as_deref()
            .is_none_or(str::is_empty))
    {
        return None;
    }
    if observation
        .stage_inclusive_input_tokens
        .is_some_and(|input| {
            input > i64::MAX as u64
                || observation.stage_usage.is_some_and(|usage| {
                    [
                        usage.input_tokens(),
                        usage.cached_input_tokens(),
                        usage.cache_creation_tokens(),
                    ]
                    .into_iter()
                    .flatten()
                    .try_fold(0_u64, u64::checked_add)
                    .is_none_or(|known| known > input)
                })
        })
    {
        return None;
    }
    let projected = serde_json::to_value(observation).ok()?;
    (serde_json::to_vec(&projected).ok()?.len() <= 4096).then_some(projected)
}

#[cfg(test)]
mod native_observation_tests {
    use super::*;

    #[test]
    fn scoped_native_observation_is_bounded_nullable_and_not_authority() {
        let value = serde_json::json!({
            "native_session_id": "thread", "native_turn_id": "turn",
            "dispatch_state": "acknowledged", "native_terminal": "completed",
            "settlement_authoritative": true,
            "stage_inclusive_input_tokens": 30,
            "stage_usage": {"cached_input_tokens": 10, "output_tokens": 5},
            "last_request_input_tokens": 12, "model_context_window": 100,
            "acknowledged_model": "model", "provider_error_code": null,
            "provider_error_class": null,
        });
        assert_eq!(
            project_native_collaborator_observation(&value),
            Some(value.clone())
        );
        for (key, invalid) in [
            ("native_session_id", Value::Null),
            ("native_turn_id", Value::String(String::new())),
            ("acknowledged_model", Value::String("x".repeat(257))),
            ("stage_inclusive_input_tokens", serde_json::json!(9)),
            (
                "stage_usage",
                serde_json::json!({"cached_input_tokens": -1}),
            ),
            (
                "stage_usage",
                serde_json::json!({"cached_input_tokens": {"raw":"payload"}}),
            ),
        ] {
            let mut invalid_value = value.clone();
            invalid_value[key] = invalid;
            assert!(
                project_native_collaborator_observation(&invalid_value).is_none(),
                "{key}"
            );
        }
        let mut unknown = value;
        unknown["dispatch_state"] = serde_json::json!("unknown");
        unknown["native_turn_id"] = Value::Null;
        unknown["stage_inclusive_input_tokens"] = Value::Null;
        unknown["stage_usage"] = Value::Null;
        assert!(project_native_collaborator_observation(&unknown).is_some());
        unknown["raw_provider_payload"] = serde_json::json!("not permitted");
        assert!(project_native_collaborator_observation(&unknown).is_none());
    }
}
/// Lossless provider-owned model evidence. Keep this in the existing
/// extension map so peers that do not project the typed catalog still retain
/// it when they recompute the discovery snapshot hash.
pub const PROVIDER_MODEL_CATALOG_KEY: &str = "astra.modelCatalog";

/// Typed declaration marker for a provider capacity that can continue an
/// agent stage.  This is deliberately separate from `task_support`: ordinary
/// asynchronous tools may require task support without being a collaborator
/// transport.
pub const PROVIDER_COLLABORATOR_STAGE_KEY: &str = "astra.collaboratorStage";

/// Maximum number of provider-owned model records retained in one discovery
/// snapshot. The catalog is capability evidence, not an unbounded provider
/// response cache.
pub const MAX_PROVIDER_MODEL_CATALOG_ITEMS: usize = 512;
/// Bound provider-owned model evidence for transport and execution. Prompt
/// publication has its own smaller budget; it must not make an otherwise
/// valid exact provider selector unexecutable.
pub const MAX_PROVIDER_MODEL_CATALOG_BYTES: usize = 192 * 1024;

/// Installed-provider dependencies, not an authorization grant. The local
/// runtime owner supplies these facts; canonical admission approves them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderRuntimeRequirements {
    pub executable: String,
    pub read_paths: Vec<String>,
}

/// One model selector exposed by a provider-owned execution capacity.
/// `selector` is the exact value sent back to that provider. Display names
/// and aliases are evidence for model-side selection only; Astra never turns
/// them into an Offering or guesses a nearby model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderModelDescriptor {
    pub selector: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_efforts: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Bounded, provider-owned model capability evidence captured during
/// discovery. It is deliberately optional: providers without a portable
/// catalog can still execute their default model, while explicit selection
/// must then be resolved by that provider's own adapter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderModelCatalog {
    pub models: Vec<ProviderModelDescriptor>,
    /// `false` means discovery/authentication succeeded but the provider's
    /// model directory was not available for this snapshot.  It is distinct
    /// from `None` on a declaration, which means the protocol does not publish
    /// a portable catalog at all.
    #[serde(default = "default_complete_model_catalog")]
    pub complete: bool,
}

fn default_complete_model_catalog() -> bool {
    true
}

impl ProviderModelCatalog {
    pub fn new(models: Vec<ProviderModelDescriptor>) -> Result<Self, ProviderContractError> {
        let catalog = Self {
            models,
            complete: true,
        };
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn unavailable() -> Self {
        Self {
            models: Vec::new(),
            complete: false,
        }
    }

    pub fn is_complete(&self) -> bool {
        self.complete
    }

    pub fn validate(&self) -> Result<(), ProviderContractError> {
        if self.models.len() > MAX_PROVIDER_MODEL_CATALOG_ITEMS {
            return Err(ProviderContractError::InvalidModelCatalog(
                "model catalog exceeds its bounded item limit".into(),
            ));
        }
        let mut selectors_and_aliases = BTreeSet::new();
        for model in &self.models {
            let valid = |value: &str| {
                !value.is_empty()
                    && value.len() <= 256
                    && value.trim() == value
                    && !value.chars().any(char::is_control)
            };
            if !valid(&model.selector)
                || !valid(&model.display_name)
                || model.aliases.len() > 16
                || model.aliases.iter().any(|alias| !valid(alias))
                || model.reasoning_efforts.len() > 16
                || model.reasoning_efforts.iter().any(|effort| !valid(effort))
                || !selectors_and_aliases.insert(model.selector.clone())
            {
                return Err(ProviderContractError::InvalidModelCatalog(
                    "model catalog contains an invalid or duplicate model".into(),
                ));
            }
            for alias in &model.aliases {
                if alias != &model.selector && !selectors_and_aliases.insert(alias.clone()) {
                    return Err(ProviderContractError::InvalidModelCatalog(
                        "model catalog contains a duplicate selector or alias".into(),
                    ));
                }
            }
        }
        if !self.complete && !self.models.is_empty() {
            return Err(ProviderContractError::InvalidModelCatalog(
                "an incomplete model catalog must not contain model records".into(),
            ));
        }
        let encoded = serde_json::to_vec(self)
            .map_err(|error| ProviderContractError::Serialization(error.to_string()))?;
        if encoded.len() > MAX_PROVIDER_MODEL_CATALOG_BYTES {
            return Err(ProviderContractError::InvalidModelCatalog(
                "model catalog exceeds its serialized byte limit".into(),
            ));
        }
        Ok(())
    }

    /// Return only provider-declared visible models for model-facing context.
    /// Execution retains hidden entries for exact provider validation.
    pub fn visible_models(&self, limit: usize) -> impl Iterator<Item = &ProviderModelDescriptor> {
        self.models
            .iter()
            .filter(move |model| self.complete && !model.hidden)
            .take(limit)
    }
}

impl ProviderRuntimeRequirements {
    pub fn from_extension_fields(
        fields: &Map<String, Value>,
    ) -> Result<Option<Self>, ProviderContractError> {
        let Some(value) = fields.get(PROVIDER_RUNTIME_REQUIREMENTS_KEY) else {
            return Ok(None);
        };
        let requirements: Self = serde_json::from_value(value.clone())
            .map_err(|_| ProviderContractError::InvalidRuntimeRequirements)?;
        let bounded = |path: &str| {
            !path.trim().is_empty() && path.len() <= 4096 && !path.chars().any(char::is_control)
        };
        if !bounded(&requirements.executable)
            || requirements.read_paths.len() > 32
            || requirements.read_paths.iter().any(|path| !bounded(path))
            || requirements
                .read_paths
                .iter()
                .map(String::len)
                .sum::<usize>()
                > 16 * 1024
        {
            return Err(ProviderContractError::InvalidRuntimeRequirements);
        }
        // Platform path resolution and sensitive/bootstrap classification
        // belong to the selected local owner, not this portable wire type.
        Ok(Some(requirements))
    }
}

macro_rules! non_empty_id {
    ($name:ident, $kind:literal) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, ProviderContractError> {
                let value = value.into();
                if value.trim().is_empty() {
                    return Err(ProviderContractError::EmptyIdentifier { kind: $kind });
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = ProviderContractError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

non_empty_id!(ProviderIdentity, "provider_identity");
non_empty_id!(ProviderBindingRef, "provider_binding_ref");
non_empty_id!(ProviderProtocolId, "provider_protocol_id");
non_empty_id!(NativeToolId, "native_tool_id");
non_empty_id!(DescriptorVersion, "descriptor_version");
non_empty_id!(ProviderRejectionCode, "provider_rejection_code");
non_empty_id!(PublicToolAlias, "public_tool_alias");
non_empty_id!(StableToolAlias, "stable_tool_alias");
non_empty_id!(ProviderResolverVersion, "provider_resolver_version");

/// Stable internal tool identity. Model-visible aliases are deliberately not
/// part of this key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ToolIdentity {
    pub provider_binding: ProviderBindingRef,
    pub native_tool_id: NativeToolId,
}

impl ToolIdentity {
    pub fn new(provider_binding: ProviderBindingRef, native_tool_id: NativeToolId) -> Self {
        Self {
            provider_binding,
            native_tool_id,
        }
    }
}

/// Exact resolved descriptor used by an invocation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ResolvedToolDescriptorRef {
    pub identity: ToolIdentity,
    pub descriptor_version: DescriptorVersion,
}

impl ResolvedToolDescriptorRef {
    pub fn new(
        identity: ToolIdentity,
        descriptor_version: impl Into<String>,
    ) -> Result<Self, ProviderContractError> {
        Ok(Self {
            identity,
            descriptor_version: DescriptorVersion::new(descriptor_version)?,
        })
    }
}

/// Resolver-assigned confidence in one provider claim. Only `Trusted` claims
/// may relax Astra's conservative execution baseline. Advisory and untrusted
/// claims remain observable evidence, but never silently become policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderClaimTrust {
    Trusted,
    Advisory,
    #[default]
    Untrusted,
}

/// A provider claim after Astra has assigned trust from host-owned authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedProviderClaim<T> {
    pub value: T,
    pub source: ProviderClaimSource,
    pub trust: ProviderClaimTrust,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedProviderToolClaims {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<ResolvedProviderClaim<bool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive: Option<ResolvedProviderClaim<bool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotent: Option<ResolvedProviderClaim<bool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_world: Option<ResolvedProviderClaim<bool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_cache: Option<ResolvedProviderClaim<ProviderSemanticCacheContract>>,
}

/// Side-effect baseline resolved from trusted declaration facts. `Unknown` is
/// deliberately not represented as `Mutating`: policy may treat both
/// conservatively while diagnostics and future reconciliation retain truth.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedToolEffect {
    ReadOnly,
    Mutating,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedConcurrencyBaseline {
    ParallelReadOnly,
    #[default]
    Serial,
}

/// Semantic result reuse is independent from effect and retry safety. A pure
/// read can return changing data, so discovery hints alone never enable it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedSemanticCacheBaseline {
    #[default]
    Disabled,
    FreshnessBound,
}

/// Provider capability required before Astra may consider semantic result
/// reuse. This is eligibility only: every invocation still needs a concrete
/// trusted revision fact for the resource it reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderSemanticCacheContract {
    RevisionBound,
}

/// Provider-neutral idempotency semantics. This intentionally does not reuse
/// the legacy built-in `IdempotentWrite` label: a remote idempotent effect is
/// not necessarily an overwrite, and retry still depends on dispatch certainty
/// and provider idempotency-key support.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedToolIdempotency {
    PureRead,
    IdempotentEffect,
    #[default]
    NonIdempotent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderSemanticDiagnosticCode {
    MissingEffectClaim,
    InsufficientEffectTrust,
    ContradictoryEffectClaims,
    InsufficientIdempotencyTrust,
    IdempotencyWithoutKnownEffect,
    InsufficientSemanticCacheTrust,
    SemanticCacheWithoutPureRead,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSemanticDiagnostic {
    pub code: ProviderSemanticDiagnosticCode,
    pub message: String,
}

/// Primitive semantic baseline shared by permission, batching, retry and
/// cache policy. Per-invocation arguments and authority can only refine this
/// object; downstream consumers must not reinterpret raw provider hints.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedToolSemantics {
    pub effect: ResolvedToolEffect,
    pub idempotency: ResolvedToolIdempotency,
    pub concurrency: ResolvedConcurrencyBaseline,
    pub semantic_cache: ResolvedSemanticCacheBaseline,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<ProviderSemanticDiagnostic>,
}

/// Content-addressed parent snapshot reference embedded in every descriptor.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ResolvedProviderSnapshotRef {
    pub provider_binding: ProviderBindingRef,
    pub content_hash: String,
}

/// Resolver output before the parent snapshot reference is known. The public
/// constructor for `ResolvedProviderSnapshot` consumes drafts atomically so a
/// descriptor cannot be attached to a different snapshot accidentally.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedToolDescriptorDraft {
    pub identity: ToolIdentity,
    pub native_tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable_tool_alias: Option<StableToolAlias>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    pub schema_hash: String,
    pub claims: ResolvedProviderToolClaims,
    pub task_support: ProviderTaskSupport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_catalog: Option<ProviderModelCatalog>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extension_fields: Map<String, Value>,
    pub semantic_baseline: ResolvedToolSemantics,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedToolDescriptor {
    pub identity: ToolIdentity,
    pub native_tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable_tool_alias: Option<StableToolAlias>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    pub schema_hash: String,
    pub claims: ResolvedProviderToolClaims,
    pub task_support: ProviderTaskSupport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_catalog: Option<ProviderModelCatalog>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extension_fields: Map<String, Value>,
    pub semantic_baseline: ResolvedToolSemantics,
    pub provider_snapshot: ResolvedProviderSnapshotRef,
    pub descriptor_version: DescriptorVersion,
}

impl ResolvedToolDescriptor {
    pub fn descriptor_ref(&self) -> ResolvedToolDescriptorRef {
        ResolvedToolDescriptorRef {
            identity: self.identity.clone(),
            descriptor_version: self.descriptor_version.clone(),
        }
    }

    fn from_draft(
        draft: ResolvedToolDescriptorDraft,
        provider_snapshot: ResolvedProviderSnapshotRef,
        descriptor_version: DescriptorVersion,
    ) -> Self {
        Self {
            identity: draft.identity,
            native_tool_name: draft.native_tool_name,
            stable_tool_alias: draft.stable_tool_alias,
            title: draft.title,
            description: draft.description,
            input_schema: draft.input_schema,
            output_schema: draft.output_schema,
            schema_hash: draft.schema_hash,
            claims: draft.claims,
            task_support: draft.task_support,
            model_catalog: draft.model_catalog,
            extension_fields: draft.extension_fields,
            semantic_baseline: draft.semantic_baseline,
            provider_snapshot,
            descriptor_version,
        }
    }

    fn to_draft(&self) -> ResolvedToolDescriptorDraft {
        ResolvedToolDescriptorDraft {
            identity: self.identity.clone(),
            native_tool_name: self.native_tool_name.clone(),
            stable_tool_alias: self.stable_tool_alias.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
            output_schema: self.output_schema.clone(),
            schema_hash: self.schema_hash.clone(),
            claims: self.claims.clone(),
            task_support: self.task_support,
            model_catalog: self.model_catalog.clone(),
            extension_fields: self.extension_fields.clone(),
            semantic_baseline: self.semantic_baseline.clone(),
        }
    }
}

/// Immutable semantic snapshot. Aliases are a projection index into exact
/// descriptor references; they never redefine internal tool identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ResolvedProviderSnapshot {
    pub provider_identity: ProviderIdentity,
    pub binding_ref: ProviderBindingRef,
    pub protocol: ProviderProtocolId,
    pub discovery_snapshot_hash: String,
    pub resolver_version: ProviderResolverVersion,
    pub resolution_policy_hash: String,
    pub descriptors: Vec<ResolvedToolDescriptor>,
    pub alias_index: BTreeMap<PublicToolAlias, ResolvedToolDescriptorRef>,
    pub content_hash: String,
}

/// Provenance for one provider declaration claim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderClaimSource {
    StandardProtocol {
        protocol: ProviderProtocolId,
        field: String,
    },
    ProviderExtension {
        namespace: String,
        field: String,
    },
    AstraOwned {
        component: String,
        field: String,
    },
}

/// A claim and its origin. Trust is assigned by Astra's resolver, not by the
/// adapter that decoded the claim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderClaim<T> {
    pub value: T,
    pub source: ProviderClaimSource,
}

impl<T> ProviderClaim<T> {
    pub fn new(value: T, source: ProviderClaimSource) -> Self {
        Self { value, source }
    }
}

/// Orthogonal provider hints. Absence remains distinct from `false`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderToolClaims {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<ProviderClaim<bool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive: Option<ProviderClaim<bool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotent: Option<ProviderClaim<bool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_world: Option<ProviderClaim<bool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_cache: Option<ProviderClaim<ProviderSemanticCacheContract>>,
}

/// Provider-declared support for asynchronous/task-augmented execution.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderTaskSupport {
    #[default]
    Unspecified,
    Forbidden,
    Optional,
    Required,
}

/// Maximum serialized size of one semantic input sent to an active provider
/// stage.  Inputs are control messages, not a second transcript; large
/// context belongs in an existing artifact or the next stage request.
pub const MAX_PROVIDER_STAGE_INPUT_BYTES: usize = 32 * 1024;

/// Provider-neutral input that can be delivered at a safe boundary of an
/// active collaborator run.  The canonical run/message owner supplies the
/// identity; adapters only translate this value to their wire protocol.
///
/// This is deliberately smaller than [`AgentMessage`].  Progress, shutdown,
/// and permission traffic keep their existing owners and must not be smuggled
/// into a provider's user prompt. Structured provider questions and answers
/// continue through the existing interaction-gate contract; this type only
/// represents an unsolicited text supplement to an active turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderStageInput {
    Text {
        /// Stable logical identity used to deduplicate a retry after a
        /// transport acknowledgement becomes unknown.
        input_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        correlation_id: Option<String>,
        /// The adapter fills this from its currently acknowledged turn when
        /// the canonical owner has not observed one yet.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_turn_id: Option<String>,
    },
}

impl ProviderStageInput {
    pub fn input_id(&self) -> &str {
        let Self::Text { input_id, .. } = self;
        input_id
    }

    pub fn expected_turn_id(&self) -> Option<&str> {
        let Self::Text {
            expected_turn_id, ..
        } = self;
        expected_turn_id.as_deref()
    }

    pub fn validate(&self) -> Result<(), ProviderContractError> {
        let valid_id = |value: &str| !value.trim().is_empty() && value == value.trim();
        if !valid_id(self.input_id()) {
            return Err(ProviderContractError::InvalidProviderStageInput(
                "input_id must be a non-empty identifier".into(),
            ));
        }
        let Self::Text { content, .. } = self;
        if content.trim().is_empty() {
            return Err(ProviderContractError::InvalidProviderStageInput(
                "text input must not be empty".into(),
            ));
        }
        if self
            .expected_turn_id()
            .is_some_and(|turn_id| !valid_id(turn_id))
        {
            return Err(ProviderContractError::InvalidProviderStageInput(
                "expected_turn_id must be a non-empty identifier".into(),
            ));
        }
        let encoded = serde_json::to_vec(self)
            .map_err(|error| ProviderContractError::Serialization(error.to_string()))?;
        if encoded.len() > MAX_PROVIDER_STAGE_INPUT_BYTES {
            return Err(ProviderContractError::InvalidProviderStageInput(
                "provider stage input exceeds its byte budget".into(),
            ));
        }
        Ok(())
    }
}

/// Evidence returned by the provider adapter for one stage input. `accepted`
/// is the only provider-level decision. A missing acknowledgement is a
/// transport failure and is handled by the caller's bounded retry path; it is
/// not another business state. Acceptance does not claim that a model has
/// already emitted a response; durable application remains owned by the
/// existing run-control facts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderStageInputAck {
    pub input_id: String,
    pub accepted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl ProviderStageInputAck {
    pub fn accepted(input: &ProviderStageInput, provider_turn_id: Option<String>) -> Self {
        Self {
            input_id: input.input_id().to_owned(),
            accepted: true,
            provider_turn_id,
            reason: None,
        }
    }

    pub fn rejected(input: &ProviderStageInput, reason: impl Into<String>) -> Self {
        Self {
            input_id: input.input_id().to_owned(),
            accepted: false,
            provider_turn_id: None,
            reason: Some(reason.into()),
        }
    }

    pub fn validate_for(&self, input: &ProviderStageInput) -> Result<(), ProviderContractError> {
        if self.input_id != input.input_id() {
            return Err(ProviderContractError::InvalidProviderStageInput(
                "input acknowledgement does not match input_id".into(),
            ));
        }
        if self.accepted && self.reason.is_some() {
            return Err(ProviderContractError::InvalidProviderStageInput(
                "accepted input acknowledgement must not carry a rejection reason".into(),
            ));
        }
        if !self.accepted && self.provider_turn_id.is_some() {
            return Err(ProviderContractError::InvalidProviderStageInput(
                "rejected input acknowledgement must not carry a provider turn".into(),
            ));
        }
        if self
            .reason
            .as_deref()
            .is_some_and(|reason| reason.len() > 4096)
        {
            return Err(ProviderContractError::InvalidProviderStageInput(
                "input acknowledgement reason exceeds its byte budget".into(),
            ));
        }
        if self
            .provider_turn_id
            .as_deref()
            .is_some_and(|turn_id| turn_id.trim().is_empty() || turn_id != turn_id.trim())
        {
            return Err(ProviderContractError::InvalidProviderStageInput(
                "provider_turn_id must be a non-empty identifier".into(),
            ));
        }
        Ok(())
    }
}

/// Losslessly normalized tool declaration before Astra policy resolution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderToolDeclaration {
    pub native_tool_id: NativeToolId,
    pub native_tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable_tool_alias: Option<StableToolAlias>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub claims: ProviderToolClaims,
    #[serde(default)]
    pub task_support: ProviderTaskSupport,
    /// Protocol/provider fields that do not yet have a portable Astra
    /// semantic. Keys must be namespace-qualified by the adapter.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extension_fields: Map<String, Value>,
}

impl ProviderToolDeclaration {
    /// Decode the provider-owned model evidence from the lossless extension
    /// map. The declaration remains the single source of truth; resolved
    /// descriptors may cache the typed value only after snapshot validation.
    pub fn model_catalog(&self) -> Result<Option<ProviderModelCatalog>, ProviderContractError> {
        let Some(value) = self.extension_fields.get(PROVIDER_MODEL_CATALOG_KEY) else {
            return Ok(None);
        };
        let catalog: ProviderModelCatalog = serde_json::from_value(value.clone())
            .map_err(|error| ProviderContractError::InvalidModelCatalog(error.to_string()))?;
        catalog.validate()?;
        Ok(Some(catalog))
    }

    pub fn is_collaborator_stage(&self) -> bool {
        self.task_support == ProviderTaskSupport::Required
            && self
                .extension_fields
                .get(PROVIDER_COLLABORATOR_STAGE_KEY)
                .and_then(Value::as_bool)
                == Some(true)
    }

    pub fn validate(&self) -> Result<(), ProviderContractError> {
        if self.native_tool_name.trim().is_empty() {
            return Err(ProviderContractError::EmptyIdentifier {
                kind: "native_tool_name",
            });
        }
        if let Some(alias) = &self.stable_tool_alias
            && (alias.as_str() != alias.as_str().trim()
                || !alias.as_str().chars().all(|character| {
                    character.is_alphanumeric() || character == '_' || character == '-'
                }))
        {
            return Err(ProviderContractError::InvalidStableToolAlias {
                native_tool_id: self.native_tool_id.to_string(),
                alias: alias.to_string(),
            });
        }
        if !self.input_schema.is_object() {
            return Err(ProviderContractError::SchemaMustBeObject {
                native_tool_id: self.native_tool_id.to_string(),
                field: "input_schema",
            });
        }
        if self
            .output_schema
            .as_ref()
            .is_some_and(|schema| !schema.is_object())
        {
            return Err(ProviderContractError::SchemaMustBeObject {
                native_tool_id: self.native_tool_id.to_string(),
                field: "output_schema",
            });
        }
        self.model_catalog()?;
        for source in [
            self.claims.read_only.as_ref().map(|claim| &claim.source),
            self.claims.destructive.as_ref().map(|claim| &claim.source),
            self.claims.idempotent.as_ref().map(|claim| &claim.source),
            self.claims.open_world.as_ref().map(|claim| &claim.source),
        ]
        .into_iter()
        .flatten()
        {
            validate_claim_source(source)?;
        }
        for key in self.extension_fields.keys() {
            let qualified = key
                .split_once('.')
                .is_some_and(|(namespace, field)| !namespace.is_empty() && !field.is_empty());
            if !qualified {
                return Err(ProviderContractError::UnqualifiedExtensionField {
                    native_tool_id: self.native_tool_id.to_string(),
                    field: key.clone(),
                });
            }
        }
        Ok(())
    }

    fn canonicalize_json(&mut self) {
        self.input_schema = canonical_json(&self.input_schema);
        self.output_schema = self.output_schema.as_ref().map(canonical_json);
        let extension_fields = Value::Object(std::mem::take(&mut self.extension_fields));
        let Value::Object(extension_fields) = canonical_json(&extension_fields) else {
            unreachable!("canonicalizing a JSON object must preserve its value kind");
        };
        self.extension_fields = extension_fields;
    }
}

/// Immutable, content-addressed discovery snapshot for one provider binding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProviderDiscoverySnapshot {
    pub provider_identity: ProviderIdentity,
    pub binding_ref: ProviderBindingRef,
    pub protocol: ProviderProtocolId,
    pub tool_declarations: Vec<ProviderToolDeclaration>,
    pub content_hash: String,
}

#[derive(Deserialize)]
struct ProviderDiscoverySnapshotWire {
    provider_identity: ProviderIdentity,
    binding_ref: ProviderBindingRef,
    protocol: ProviderProtocolId,
    tool_declarations: Vec<ProviderToolDeclaration>,
    content_hash: String,
}

impl<'de> Deserialize<'de> for ProviderDiscoverySnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ProviderDiscoverySnapshotWire::deserialize(deserializer)?;
        let supplied_hash = wire.content_hash;
        let snapshot = Self::new(
            wire.provider_identity,
            wire.binding_ref,
            wire.protocol,
            wire.tool_declarations,
        )
        .map_err(serde::de::Error::custom)?;
        if supplied_hash != snapshot.content_hash {
            return Err(serde::de::Error::custom(
                ProviderContractError::ContentHashMismatch {
                    supplied: supplied_hash,
                    computed: snapshot.content_hash,
                },
            ));
        }
        Ok(snapshot)
    }
}

impl ProviderDiscoverySnapshot {
    pub fn new(
        provider_identity: ProviderIdentity,
        binding_ref: ProviderBindingRef,
        protocol: ProviderProtocolId,
        mut tool_declarations: Vec<ProviderToolDeclaration>,
    ) -> Result<Self, ProviderContractError> {
        for declaration in &mut tool_declarations {
            declaration.validate()?;
            for source in provider_claim_sources(&declaration.claims) {
                if let ProviderClaimSource::StandardProtocol {
                    protocol: claim_protocol,
                    field,
                } = source
                    && claim_protocol != &protocol
                {
                    return Err(ProviderContractError::ClaimProtocolMismatch {
                        native_tool_id: declaration.native_tool_id.to_string(),
                        field: field.clone(),
                        snapshot_protocol: protocol.to_string(),
                        claim_protocol: claim_protocol.to_string(),
                    });
                }
            }
            declaration.canonicalize_json();
        }
        tool_declarations.sort_by(|left, right| {
            left.native_tool_id
                .cmp(&right.native_tool_id)
                .then_with(|| left.native_tool_name.cmp(&right.native_tool_name))
        });

        let mut seen = BTreeSet::new();
        for declaration in &tool_declarations {
            if !seen.insert(declaration.native_tool_id.clone()) {
                return Err(ProviderContractError::DuplicateNativeToolId {
                    native_tool_id: declaration.native_tool_id.to_string(),
                });
            }
        }

        let hash_input = (
            &provider_identity,
            &binding_ref,
            &protocol,
            &tool_declarations,
        );
        let encoded = serde_json::to_vec(&hash_input)
            .map_err(|error| ProviderContractError::Serialization(error.to_string()))?;
        let content_hash = format!("{:x}", Sha256::digest(encoded));

        Ok(Self {
            provider_identity,
            binding_ref,
            protocol,
            tool_declarations,
            content_hash,
        })
    }

    pub fn tool_identity(&self, declaration: &ProviderToolDeclaration) -> ToolIdentity {
        ToolIdentity::new(self.binding_ref.clone(), declaration.native_tool_id.clone())
    }
}

#[derive(Deserialize)]
struct ResolvedProviderSnapshotWire {
    provider_identity: ProviderIdentity,
    binding_ref: ProviderBindingRef,
    protocol: ProviderProtocolId,
    discovery_snapshot_hash: String,
    resolver_version: ProviderResolverVersion,
    resolution_policy_hash: String,
    descriptors: Vec<ResolvedToolDescriptor>,
    alias_index: BTreeMap<PublicToolAlias, ResolvedToolDescriptorRef>,
    content_hash: String,
}

impl<'de> Deserialize<'de> for ResolvedProviderSnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ResolvedProviderSnapshotWire::deserialize(deserializer)?;
        let supplied_descriptors = wire.descriptors.clone();
        let supplied_alias_index = wire.alias_index.clone();
        let aliases = wire
            .alias_index
            .iter()
            .map(|(alias, descriptor)| (alias.clone(), descriptor.identity.clone()))
            .collect();
        let drafts = wire
            .descriptors
            .iter()
            .map(ResolvedToolDescriptor::to_draft)
            .collect();
        let rebuilt = Self::new(
            wire.provider_identity,
            wire.binding_ref,
            wire.protocol,
            wire.discovery_snapshot_hash,
            wire.resolver_version,
            wire.resolution_policy_hash,
            drafts,
            aliases,
        )
        .map_err(serde::de::Error::custom)?;
        if wire.content_hash != rebuilt.content_hash {
            return Err(serde::de::Error::custom(
                ProviderContractError::ResolvedContentHashMismatch {
                    supplied: wire.content_hash,
                    computed: rebuilt.content_hash,
                },
            ));
        }
        if supplied_descriptors != rebuilt.descriptors
            || supplied_alias_index != rebuilt.alias_index
        {
            return Err(serde::de::Error::custom(
                ProviderContractError::ResolvedSnapshotInvariantMismatch,
            ));
        }
        Ok(rebuilt)
    }
}

impl ResolvedProviderSnapshot {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider_identity: ProviderIdentity,
        binding_ref: ProviderBindingRef,
        protocol: ProviderProtocolId,
        discovery_snapshot_hash: String,
        resolver_version: ProviderResolverVersion,
        resolution_policy_hash: String,
        mut drafts: Vec<ResolvedToolDescriptorDraft>,
        aliases: Vec<(PublicToolAlias, ToolIdentity)>,
    ) -> Result<Self, ProviderContractError> {
        if discovery_snapshot_hash.trim().is_empty() {
            return Err(ProviderContractError::EmptyIdentifier {
                kind: "discovery_snapshot_hash",
            });
        }
        if resolution_policy_hash.trim().is_empty() {
            return Err(ProviderContractError::EmptyIdentifier {
                kind: "resolution_policy_hash",
            });
        }

        drafts.sort_by(|left, right| left.identity.cmp(&right.identity));
        let mut identities = BTreeSet::new();
        for draft in &drafts {
            if draft.identity.provider_binding != binding_ref {
                return Err(ProviderContractError::DescriptorBindingMismatch {
                    native_tool_id: draft.identity.native_tool_id.to_string(),
                    expected: binding_ref.to_string(),
                    actual: draft.identity.provider_binding.to_string(),
                });
            }
            if !identities.insert(draft.identity.clone()) {
                return Err(ProviderContractError::DuplicateResolvedToolIdentity {
                    native_tool_id: draft.identity.native_tool_id.to_string(),
                });
            }
        }

        let mut versioned_drafts = Vec::with_capacity(drafts.len());
        for draft in drafts {
            let encoded = serde_json::to_vec(&draft)
                .map_err(|error| ProviderContractError::Serialization(error.to_string()))?;
            let descriptor_version =
                DescriptorVersion::new(format!("sha256:{:x}", Sha256::digest(encoded)))?;
            versioned_drafts.push((draft, descriptor_version));
        }

        let descriptor_refs = versioned_drafts
            .iter()
            .map(|(draft, version)| {
                (
                    draft.identity.clone(),
                    ResolvedToolDescriptorRef {
                        identity: draft.identity.clone(),
                        descriptor_version: version.clone(),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let descriptor_ref_list = descriptor_refs.values().cloned().collect::<Vec<_>>();

        let mut alias_index = BTreeMap::new();
        let mut projected_identities = BTreeSet::new();
        for (alias, identity) in aliases {
            let Some(descriptor_ref) = descriptor_refs.get(&identity) else {
                return Err(ProviderContractError::AliasTargetsUnknownTool {
                    alias: alias.to_string(),
                    native_tool_id: identity.native_tool_id.to_string(),
                });
            };
            if alias_index
                .insert(alias.clone(), descriptor_ref.clone())
                .is_some()
            {
                return Err(ProviderContractError::DuplicatePublicAlias {
                    alias: alias.to_string(),
                });
            }
            if !projected_identities.insert(identity.clone()) {
                return Err(ProviderContractError::DuplicateToolProjection {
                    native_tool_id: identity.native_tool_id.to_string(),
                });
            }
        }
        if projected_identities != identities {
            let missing = identities
                .difference(&projected_identities)
                .next()
                .expect("different identity sets must have a missing descriptor");
            return Err(ProviderContractError::MissingToolProjection {
                native_tool_id: missing.native_tool_id.to_string(),
            });
        }

        // Descriptors cannot contain their own parent content hash while that
        // hash is being derived. Hash their independent versions plus all
        // resolver/projection inputs, then attach the resulting parent ref.
        let snapshot_hash_input = (
            &provider_identity,
            &binding_ref,
            &protocol,
            &discovery_snapshot_hash,
            &resolver_version,
            &resolution_policy_hash,
            &descriptor_ref_list,
            &alias_index,
        );
        let encoded = serde_json::to_vec(&snapshot_hash_input)
            .map_err(|error| ProviderContractError::Serialization(error.to_string()))?;
        let content_hash = format!("sha256:{:x}", Sha256::digest(encoded));
        let provider_snapshot = ResolvedProviderSnapshotRef {
            provider_binding: binding_ref.clone(),
            content_hash: content_hash.clone(),
        };
        let descriptors = versioned_drafts
            .into_iter()
            .map(|(draft, descriptor_version)| {
                ResolvedToolDescriptor::from_draft(
                    draft,
                    provider_snapshot.clone(),
                    descriptor_version,
                )
            })
            .collect();

        Ok(Self {
            provider_identity,
            binding_ref,
            protocol,
            discovery_snapshot_hash,
            resolver_version,
            resolution_policy_hash,
            descriptors,
            alias_index,
            content_hash,
        })
    }
}

/// Provider result payload before model/client projection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderCallPayload {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_metadata: Option<Value>,
}

/// Namespaced MCP result metadata carrying a provider-owned user interaction.
///
/// Astra persists and transports this envelope without interpreting `payload`.
/// The provider remains the sole owner of the interaction's business meaning
/// and validates the submitted response when the same tool call is resumed.
pub const PROVIDER_INTERACTION_REQUEST_METADATA_KEY: &str = "astra/providerInteraction";

/// Namespaced MCP request metadata carrying the response to a previously
/// requested provider interaction.
pub const PROVIDER_INTERACTION_RESPONSE_METADATA_KEY: &str = "astra/providerInteractionResponse";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderInteractionRequest {
    pub request_id: String,
    pub payload: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// The provider-stage input that was still awaiting its steer ACK when
    /// this interaction arrived. It is an internal coordination fact, not
    /// provider business payload; the server uses it only as a provisional
    /// durable fence and applies current-run guidance only after an accepted
    /// provider steer ACK.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_stage_input_id: Option<String>,
}

impl ProviderInteractionRequest {
    pub const MAX_TIMEOUT_MS: u64 = 60 * 60 * 1_000;

    pub fn validate(&self) -> Result<(), ProviderContractError> {
        if self.request_id.is_empty() || self.request_id != self.request_id.trim() {
            return Err(ProviderContractError::InvalidProviderInteraction(
                "request_id must be a non-empty string without surrounding whitespace".into(),
            ));
        }
        if !self.payload.is_object() {
            return Err(ProviderContractError::InvalidProviderInteraction(
                "payload must be an object".into(),
            ));
        }
        if self
            .timeout_ms
            .is_some_and(|timeout| timeout == 0 || timeout > Self::MAX_TIMEOUT_MS)
        {
            return Err(ProviderContractError::InvalidProviderInteraction(format!(
                "timeout_ms must be between 1 and {}",
                Self::MAX_TIMEOUT_MS
            )));
        }
        if self
            .provider_stage_input_id
            .as_deref()
            .is_some_and(|input_id| {
                input_id.trim().is_empty() || input_id != input_id.trim() || input_id.len() > 512
            })
        {
            return Err(ProviderContractError::InvalidProviderInteraction(
                "provider_stage_input_id must be a bounded identifier".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderInteractionOutcome {
    Submitted,
    Cancelled,
    TimedOut,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderInteractionResponse {
    pub request_id: String,
    pub outcome: ProviderInteractionOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
}

impl ProviderInteractionResponse {
    pub fn validate_for(
        &self,
        request: &ProviderInteractionRequest,
    ) -> Result<(), ProviderContractError> {
        if self.request_id != request.request_id {
            return Err(ProviderContractError::InvalidProviderInteraction(
                "response request_id does not match the pending request".into(),
            ));
        }
        match self.outcome {
            ProviderInteractionOutcome::Submitted => {
                if !self.payload.as_ref().is_some_and(Value::is_object) {
                    return Err(ProviderContractError::InvalidProviderInteraction(
                        "a submitted response requires an object payload".into(),
                    ));
                }
            }
            ProviderInteractionOutcome::Cancelled | ProviderInteractionOutcome::TimedOut => {
                if self.payload.is_some() {
                    return Err(ProviderContractError::InvalidProviderInteraction(
                        "cancelled and timed_out responses must not carry a payload".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// A provider acknowledged the request but declined to execute it. This is
/// distinct from an Astra admission rejection and from a transport failure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRejection {
    pub code: ProviderRejectionCode,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
}

impl ProviderRejection {
    pub fn new(
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
    ) -> Result<Self, ProviderContractError> {
        Ok(Self {
            code: ProviderRejectionCode::new(code)?,
            message: message.into(),
            retryable,
        })
    }
}

/// Acknowledged provider tool outcome. Transport/protocol failures remain in
/// the adapter's error channel and carry dispatch certainty there.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "payload", rename_all = "snake_case")]
pub enum ProviderCallOutcome {
    Success(ProviderCallPayload),
    ToolFailure(ProviderCallPayload),
    Rejected(ProviderRejection),
    InteractionRequired(ProviderInteractionRequest),
}

impl ProviderCallOutcome {
    pub fn payload(&self) -> Option<&ProviderCallPayload> {
        match self {
            Self::Success(payload) | Self::ToolFailure(payload) => Some(payload),
            Self::Rejected(_) | Self::InteractionRequired(_) => None,
        }
    }

    pub fn is_error(&self) -> bool {
        !matches!(self, Self::Success(_))
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ProviderContractError {
    #[error("invalid installed-provider runtime requirements")]
    InvalidRuntimeRequirements,
    #[error("invalid provider model catalog: {0}")]
    InvalidModelCatalog(String),
    #[error("{kind} must not be empty")]
    EmptyIdentifier { kind: &'static str },
    #[error("invalid provider interaction: {0}")]
    InvalidProviderInteraction(String),
    #[error("invalid provider stage input: {0}")]
    InvalidProviderStageInput(String),
    #[error("duplicate native tool id '{native_tool_id}' in provider snapshot")]
    DuplicateNativeToolId { native_tool_id: String },
    #[error("tool '{native_tool_id}' {field} must be a JSON object")]
    SchemaMustBeObject {
        native_tool_id: String,
        field: &'static str,
    },
    #[error("tool '{native_tool_id}' has invalid stable tool alias '{alias}'")]
    InvalidStableToolAlias {
        native_tool_id: String,
        alias: String,
    },
    #[error("tool '{native_tool_id}' extension field '{field}' must be namespace-qualified")]
    UnqualifiedExtensionField {
        native_tool_id: String,
        field: String,
    },
    #[error(
        "provider snapshot content hash mismatch: supplied '{supplied}', computed '{computed}'"
    )]
    ContentHashMismatch { supplied: String, computed: String },
    #[error(
        "resolved provider snapshot content hash mismatch: supplied '{supplied}', computed '{computed}'"
    )]
    ResolvedContentHashMismatch { supplied: String, computed: String },
    #[error("resolved provider snapshot contains fields inconsistent with its canonical content")]
    ResolvedSnapshotInvariantMismatch,
    #[error(
        "resolved tool '{native_tool_id}' belongs to binding '{actual}', expected '{expected}'"
    )]
    DescriptorBindingMismatch {
        native_tool_id: String,
        expected: String,
        actual: String,
    },
    #[error("duplicate resolved native tool identity '{native_tool_id}'")]
    DuplicateResolvedToolIdentity { native_tool_id: String },
    #[error("public alias '{alias}' targets unknown tool '{native_tool_id}'")]
    AliasTargetsUnknownTool {
        alias: String,
        native_tool_id: String,
    },
    #[error("duplicate public tool alias '{alias}'")]
    DuplicatePublicAlias { alias: String },
    #[error("tool '{native_tool_id}' has more than one public alias")]
    DuplicateToolProjection { native_tool_id: String },
    #[error("tool '{native_tool_id}' is missing a public alias projection")]
    MissingToolProjection { native_tool_id: String },
    #[error(
        "tool '{native_tool_id}' claim '{field}' names protocol '{claim_protocol}', but its snapshot protocol is '{snapshot_protocol}'"
    )]
    ClaimProtocolMismatch {
        native_tool_id: String,
        field: String,
        snapshot_protocol: String,
        claim_protocol: String,
    },
    #[error("failed to serialize provider snapshot: {0}")]
    Serialization(String),
}

fn validate_claim_source(source: &ProviderClaimSource) -> Result<(), ProviderContractError> {
    let (kind, first, field) = match source {
        ProviderClaimSource::StandardProtocol { field, .. } => {
            ("provider_claim_protocol", None, field)
        }
        ProviderClaimSource::ProviderExtension { namespace, field } => {
            ("provider_claim_extension", Some(namespace), field)
        }
        ProviderClaimSource::AstraOwned { component, field } => {
            ("provider_claim_astra_component", Some(component), field)
        }
    };
    if first.is_some_and(|value| value.trim().is_empty()) || field.trim().is_empty() {
        return Err(ProviderContractError::EmptyIdentifier { kind });
    }
    Ok(())
}

fn provider_claim_sources(
    claims: &ProviderToolClaims,
) -> impl Iterator<Item = &ProviderClaimSource> {
    [
        claims.read_only.as_ref().map(|claim| &claim.source),
        claims.destructive.as_ref().map(|claim| &claim.source),
        claims.idempotent.as_ref().map(|claim| &claim.source),
        claims.open_world.as_ref().map(|claim| &claim.source),
    ]
    .into_iter()
    .flatten()
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(canonical_json).collect()),
        Value::Object(object) => {
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            let mut canonical = Map::new();
            for key in keys {
                canonical.insert(key.clone(), canonical_json(&object[key]));
            }
            Value::Object(canonical)
        }
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn declaration(id: &str, schema: Value) -> ProviderToolDeclaration {
        ProviderToolDeclaration {
            native_tool_id: NativeToolId::new(id).unwrap(),
            native_tool_name: id.to_string(),
            stable_tool_alias: None,
            title: None,
            description: None,
            input_schema: schema,
            output_schema: None,
            claims: ProviderToolClaims::default(),
            task_support: ProviderTaskSupport::Unspecified,
            extension_fields: Map::new(),
        }
    }

    fn snapshot(tools: Vec<ProviderToolDeclaration>) -> ProviderDiscoverySnapshot {
        ProviderDiscoverySnapshot::new(
            ProviderIdentity::new("provider-a").unwrap(),
            ProviderBindingRef::new("binding-a").unwrap(),
            ProviderProtocolId::new("test").unwrap(),
            tools,
        )
        .unwrap()
    }

    #[test]
    fn identifiers_reject_whitespace_only_values_including_deserialization() {
        assert!(ProviderIdentity::new("  ").is_err());
        let parsed = serde_json::from_str::<ProviderBindingRef>(r#"""#);
        assert!(parsed.is_err());
    }

    #[test]
    fn provider_model_catalog_is_bounded_and_preserves_capability_evidence() {
        let catalog = ProviderModelCatalog::new(vec![ProviderModelDescriptor {
            selector: "provider-model-v2".into(),
            display_name: "Provider Model V2".into(),
            aliases: vec!["v2".into()],
            reasoning_efforts: vec!["high".into()],
            hidden: false,
        }])
        .unwrap();
        assert_eq!(catalog.visible_models(8).count(), 1);
        let encoded = serde_json::to_value(&catalog).unwrap();
        assert_eq!(encoded["models"][0]["selector"], "provider-model-v2");
        assert_eq!(encoded["models"][0]["aliases"][0], "v2");
        assert_eq!(encoded["models"][0]["reasoning_efforts"][0], "high");

        assert!(
            ProviderModelCatalog::new(
                (0..=MAX_PROVIDER_MODEL_CATALOG_ITEMS)
                    .map(|index| ProviderModelDescriptor {
                        selector: format!("model-{index}"),
                        display_name: format!("Model {index}"),
                        aliases: Vec::new(),
                        reasoning_efforts: Vec::new(),
                        hidden: false,
                    })
                    .collect()
            )
            .is_err()
        );

        let larger_catalog = ProviderModelCatalog::new(
            (0..150)
                .map(|index| ProviderModelDescriptor {
                    selector: format!("provider-model-{index}"),
                    display_name: "x".repeat(256),
                    aliases: vec![format!("provider-alias-{index}")],
                    reasoning_efforts: vec!["high".into()],
                    hidden: false,
                })
                .collect(),
        );
        assert!(larger_catalog.is_ok());
    }

    #[test]
    fn model_catalog_extension_is_lossless_in_discovery_snapshot() {
        let catalog = ProviderModelCatalog::new(vec![ProviderModelDescriptor {
            selector: "provider-model-v2".into(),
            display_name: "Provider Model V2".into(),
            aliases: vec!["v2".into()],
            reasoning_efforts: vec!["high".into()],
            hidden: false,
        }])
        .unwrap();
        let mut tool = declaration("native", json!({"type": "object"}));
        tool.extension_fields.insert(
            PROVIDER_MODEL_CATALOG_KEY.into(),
            serde_json::to_value(&catalog).unwrap(),
        );
        let snapshot = snapshot(vec![tool]);
        let encoded = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(
            encoded["tool_declarations"][0]["extension_fields"][PROVIDER_MODEL_CATALOG_KEY],
            serde_json::to_value(&catalog).unwrap()
        );
        let decoded: ProviderDiscoverySnapshot = serde_json::from_value(encoded).unwrap();
        assert_eq!(
            decoded.tool_declarations[0].model_catalog().unwrap(),
            Some(catalog)
        );
    }

    #[test]
    fn snapshot_hash_is_independent_of_discovery_and_object_key_order() {
        let first = snapshot(vec![
            declaration(
                "z",
                json!({"type": "object", "properties": {"b": {}, "a": {}}}),
            ),
            declaration("a", json!({"required": ["q"], "type": "object"})),
        ]);

        let mut reversed_properties = Map::new();
        reversed_properties.insert("a".to_string(), json!({}));
        reversed_properties.insert("b".to_string(), json!({}));
        let second = snapshot(vec![
            declaration("a", json!({"type": "object", "required": ["q"]})),
            declaration(
                "z",
                json!({"properties": reversed_properties, "type": "object"}),
            ),
        ]);

        assert_eq!(first.content_hash, second.content_hash);
        assert_eq!(first.tool_declarations, second.tool_declarations);
    }

    #[test]
    fn snapshot_rejects_duplicate_native_identity() {
        let error = ProviderDiscoverySnapshot::new(
            ProviderIdentity::new("provider-a").unwrap(),
            ProviderBindingRef::new("binding-a").unwrap(),
            ProviderProtocolId::new("test").unwrap(),
            vec![
                declaration("same", json!({"type": "object"})),
                declaration("same", json!({"type": "object"})),
            ],
        )
        .unwrap_err();

        assert!(matches!(
            error,
            ProviderContractError::DuplicateNativeToolId { .. }
        ));
    }

    #[test]
    fn snapshot_rejects_unqualified_extension_fields() {
        let mut tool = declaration("read", json!({"type": "object"}));
        tool.extension_fields
            .insert("metadata".to_string(), json!({"safe": true}));

        let error = ProviderDiscoverySnapshot::new(
            ProviderIdentity::new("provider-a").unwrap(),
            ProviderBindingRef::new("binding-a").unwrap(),
            ProviderProtocolId::new("test").unwrap(),
            vec![tool],
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ProviderContractError::UnqualifiedExtensionField { .. }
        ));
    }

    #[test]
    fn snapshot_rejects_a_claim_borrowing_another_protocols_authority() {
        let mut tool = declaration("read", json!({"type": "object"}));
        tool.claims.read_only = Some(ProviderClaim::new(
            true,
            ProviderClaimSource::StandardProtocol {
                protocol: ProviderProtocolId::new("trusted-other-protocol").unwrap(),
                field: "readOnlyHint".to_string(),
            },
        ));

        let error = ProviderDiscoverySnapshot::new(
            ProviderIdentity::new("provider-a").unwrap(),
            ProviderBindingRef::new("binding-a").unwrap(),
            ProviderProtocolId::new("mcp").unwrap(),
            vec![tool],
        )
        .unwrap_err();

        assert!(matches!(
            error,
            ProviderContractError::ClaimProtocolMismatch { .. }
        ));
    }

    #[test]
    fn deserialization_recomputes_and_rejects_a_tampered_snapshot_hash() {
        let snapshot = snapshot(vec![declaration("read", json!({"type": "object"}))]);
        let mut serialized = serde_json::to_value(&snapshot).unwrap();
        let restored =
            serde_json::from_value::<ProviderDiscoverySnapshot>(serialized.clone()).unwrap();
        assert_eq!(restored, snapshot);

        serialized["content_hash"] = Value::String("forged".to_string());

        let error = serde_json::from_value::<ProviderDiscoverySnapshot>(serialized).unwrap_err();
        assert!(error.to_string().contains("content hash mismatch"));
    }

    #[test]
    fn real_semantic_changes_invalidate_snapshot_hash() {
        let original = snapshot(vec![declaration(
            "read",
            json!({"type": "object", "properties": {}}),
        )]);
        let changed = snapshot(vec![declaration(
            "read",
            json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        )]);

        assert_ne!(original.content_hash, changed.content_hash);
    }

    #[test]
    fn public_alias_is_not_part_of_internal_identity() {
        let snapshot = snapshot(vec![declaration("native.tool", json!({"type": "object"}))]);
        let identity = snapshot.tool_identity(&snapshot.tool_declarations[0]);

        assert_eq!(identity.native_tool_id.as_str(), "native.tool");
        assert_eq!(identity.provider_binding.as_str(), "binding-a");
    }

    #[test]
    fn typed_provider_outcome_never_infers_failure_from_text() {
        let success = ProviderCallOutcome::Success(ProviderCallPayload {
            text: "error: this is quoted documentation".to_string(),
            structured_content: None,
            protocol_metadata: None,
        });
        let failure = ProviderCallOutcome::ToolFailure(ProviderCallPayload {
            text: "ok".to_string(),
            structured_content: None,
            protocol_metadata: None,
        });

        assert!(!success.is_error());
        assert!(failure.is_error());
    }

    #[test]
    fn provider_rejection_requires_a_machine_readable_code() {
        assert!(ProviderRejection::new(" ", "busy", true).is_err());
        let rejection = ProviderCallOutcome::Rejected(
            ProviderRejection::new("capacity_exhausted", "busy", true).unwrap(),
        );
        assert!(rejection.is_error());
        assert_eq!(rejection.payload(), None);
    }

    #[test]
    fn provider_interaction_contract_keeps_business_payload_opaque() {
        let request = ProviderInteractionRequest {
            request_id: "call-1:select-instance".to_string(),
            payload: json!({
                "type": "provider.example.select",
                "version": 7,
                "options": [{"opaque": "value"}],
            }),
            timeout_ms: Some(600_000),
            provider_stage_input_id: None,
        };
        request.validate().unwrap();

        ProviderInteractionResponse {
            request_id: request.request_id.clone(),
            outcome: ProviderInteractionOutcome::Submitted,
            payload: Some(json!({"provider_owned_response": {"id": "opaque-1"}})),
        }
        .validate_for(&request)
        .unwrap();
    }

    #[test]
    fn provider_interaction_response_requires_matching_identity_and_outcome_shape() {
        let request = ProviderInteractionRequest {
            request_id: "interaction-1".to_string(),
            payload: json!({}),
            timeout_ms: None,
            provider_stage_input_id: None,
        };

        for response in [
            ProviderInteractionResponse {
                request_id: "interaction-2".to_string(),
                outcome: ProviderInteractionOutcome::Submitted,
                payload: Some(json!({})),
            },
            ProviderInteractionResponse {
                request_id: request.request_id.clone(),
                outcome: ProviderInteractionOutcome::Submitted,
                payload: None,
            },
            ProviderInteractionResponse {
                request_id: request.request_id.clone(),
                outcome: ProviderInteractionOutcome::Cancelled,
                payload: Some(json!({})),
            },
        ] {
            assert!(response.validate_for(&request).is_err());
        }
    }

    #[test]
    fn provider_stage_input_is_bounded_and_acknowledgements_are_fenced() {
        let input = ProviderStageInput::Text {
            input_id: "message-1".into(),
            content: "please continue with the failing test".into(),
            correlation_id: Some("turn-1".into()),
            expected_turn_id: Some("turn-7".into()),
        };
        input.validate().unwrap();
        let encoded = serde_json::to_vec(&input).unwrap();
        assert!(encoded.len() <= MAX_PROVIDER_STAGE_INPUT_BYTES);

        let ack = ProviderStageInputAck::accepted(&input, Some("turn-7".into()));
        ack.validate_for(&input).unwrap();
        let wrong = ProviderStageInputAck {
            input_id: "message-2".into(),
            ..ack
        };
        assert!(wrong.validate_for(&input).is_err());
        assert!(
            ProviderStageInputAck {
                input_id: input.input_id().into(),
                accepted: true,
                provider_turn_id: None,
                reason: Some("not actually accepted".into()),
            }
            .validate_for(&input)
            .is_err()
        );
        assert!(
            ProviderStageInputAck {
                input_id: input.input_id().into(),
                accepted: false,
                provider_turn_id: Some("turn-7".into()),
                reason: Some("rejected".into()),
            }
            .validate_for(&input)
            .is_err()
        );
    }

    #[test]
    fn provider_stage_input_rejects_empty_and_oversized_content() {
        assert!(
            ProviderStageInput::Text {
                input_id: "message-1".into(),
                content: "   ".into(),
                correlation_id: None,
                expected_turn_id: None,
            }
            .validate()
            .is_err()
        );

        assert!(
            ProviderStageInput::Text {
                input_id: "message-1".into(),
                content: "x".repeat(MAX_PROVIDER_STAGE_INPUT_BYTES),
                correlation_id: None,
                expected_turn_id: None,
            }
            .validate()
            .is_err()
        );
    }
}
