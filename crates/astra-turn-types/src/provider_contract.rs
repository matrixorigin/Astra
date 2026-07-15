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
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    pub schema_hash: String,
    pub claims: ResolvedProviderToolClaims,
    pub task_support: ProviderTaskSupport,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extension_fields: Map<String, Value>,
    pub semantic_baseline: ResolvedToolSemantics,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedToolDescriptor {
    pub identity: ToolIdentity,
    pub native_tool_name: String,
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
            title: draft.title,
            description: draft.description,
            input_schema: draft.input_schema,
            output_schema: draft.output_schema,
            schema_hash: draft.schema_hash,
            claims: draft.claims,
            task_support: draft.task_support,
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
            title: self.title.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
            output_schema: self.output_schema.clone(),
            schema_hash: self.schema_hash.clone(),
            claims: self.claims.clone(),
            task_support: self.task_support,
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

/// Losslessly normalized tool declaration before Astra policy resolution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderToolDeclaration {
    pub native_tool_id: NativeToolId,
    pub native_tool_name: String,
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
    pub fn validate(&self) -> Result<(), ProviderContractError> {
        if self.native_tool_name.trim().is_empty() {
            return Err(ProviderContractError::EmptyIdentifier {
                kind: "native_tool_name",
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
}

impl ProviderCallOutcome {
    pub fn payload(&self) -> Option<&ProviderCallPayload> {
        match self {
            Self::Success(payload) | Self::ToolFailure(payload) => Some(payload),
            Self::Rejected(_) => None,
        }
    }

    pub fn is_error(&self) -> bool {
        !matches!(self, Self::Success(_))
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ProviderContractError {
    #[error("{kind} must not be empty")]
    EmptyIdentifier { kind: &'static str },
    #[error("duplicate native tool id '{native_tool_id}' in provider snapshot")]
    DuplicateNativeToolId { native_tool_id: String },
    #[error("tool '{native_tool_id}' {field} must be a JSON object")]
    SchemaMustBeObject {
        native_tool_id: String,
        field: &'static str,
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
}
