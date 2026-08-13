//! Host-owned resolution of provider discovery claims into immutable semantic
//! descriptors. Protocol adapters stop at `ProviderDiscoverySnapshot`; this
//! module is the only layer allowed to assign claim trust or relax the
//! conservative dynamic-tool baseline.

use std::collections::BTreeMap;

use astra_turn_types::{
    NativeToolId, ProviderClaim, ProviderClaimSource, ProviderClaimTrust, ProviderContractError,
    ProviderDiscoverySnapshot, ProviderResolverVersion, ProviderSemanticDiagnostic,
    ProviderSemanticDiagnosticCode, PublicToolAlias, ResolvedConcurrencyBaseline,
    ResolvedProviderClaim, ResolvedProviderSnapshot, ResolvedProviderToolClaims,
    ResolvedSemanticCacheBaseline, ResolvedToolDescriptorDraft, ResolvedToolDescriptorRef,
    ResolvedToolEffect, ResolvedToolIdempotency, ResolvedToolSemantics,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

const RESOLVER_VERSION: &str = "provider-semantic-resolver-v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderApprovalBaseline {
    NoAdditionalApproval,
    RequiresApproval,
}

/// One immutable policy object consumed by provider-tool admission,
/// permission, batching, retry and cache layers. Arguments may refine this in
/// later phases, but consumers must never reinterpret the raw claims.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedInvocationPolicy {
    pub descriptor: ResolvedToolDescriptorRef,
    pub effect: ResolvedToolEffect,
    pub parallelizable: bool,
    pub approval: ProviderApprovalBaseline,
    pub idempotency: ResolvedToolIdempotency,
    pub semantic_cache: ResolvedSemanticCacheBaseline,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<ProviderSemanticDiagnostic>,
}

impl ResolvedInvocationPolicy {
    pub fn requires_approval(&self) -> bool {
        self.approval == ProviderApprovalBaseline::RequiresApproval
    }

    pub fn is_read_only(&self) -> bool {
        self.effect == ResolvedToolEffect::ReadOnly
    }

    /// Content-addressed descriptor semantic baseline. Runtime admission,
    /// authority, route, approval outcome and argument refinements must be
    /// combined with this ID to form the final per-invocation decision.
    pub fn baseline_content_id(&self) -> Result<String, ProviderResolutionError> {
        hash_serializable(self)
    }
}

/// Request/session-scoped alias index. It is deliberately not process-global:
/// identical public aliases in concurrent sessions cannot overwrite each
/// other's descriptor version or execution policy.
#[derive(Clone, Debug, Default)]
pub struct ResolvedProviderPolicyIndex {
    by_alias: BTreeMap<String, ResolvedInvocationPolicy>,
}

impl ResolvedProviderPolicyIndex {
    pub fn from_snapshots(
        snapshots: &[ResolvedProviderSnapshot],
    ) -> Result<Self, ProviderResolutionError> {
        let mut by_alias = BTreeMap::new();
        for snapshot in snapshots {
            let descriptors = snapshot
                .descriptors
                .iter()
                .map(|descriptor| (descriptor.descriptor_ref(), descriptor))
                .collect::<BTreeMap<_, _>>();
            for (alias, descriptor_ref) in &snapshot.alias_index {
                let descriptor = descriptors.get(descriptor_ref).ok_or_else(|| {
                    ProviderResolutionError::MissingResolvedDescriptor {
                        alias: alias.to_string(),
                        native_tool_id: descriptor_ref.identity.native_tool_id.to_string(),
                    }
                })?;
                let semantics = &descriptor.semantic_baseline;
                let policy = ResolvedInvocationPolicy {
                    descriptor: descriptor_ref.clone(),
                    effect: semantics.effect,
                    parallelizable: semantics.concurrency
                        == ResolvedConcurrencyBaseline::ParallelReadOnly,
                    approval: if semantics.effect == ResolvedToolEffect::ReadOnly {
                        ProviderApprovalBaseline::NoAdditionalApproval
                    } else {
                        ProviderApprovalBaseline::RequiresApproval
                    },
                    idempotency: semantics.idempotency,
                    semantic_cache: semantics.semantic_cache,
                    diagnostics: semantics.diagnostics.clone(),
                };
                if by_alias.insert(alias.to_string(), policy).is_some() {
                    return Err(ProviderResolutionError::CrossSnapshotAliasCollision {
                        alias: alias.to_string(),
                    });
                }
            }
        }
        Ok(Self { by_alias })
    }

    pub fn resolve(&self, public_alias: &str) -> Option<&ResolvedInvocationPolicy> {
        self.by_alias.get(public_alias)
    }

    pub fn len(&self) -> usize {
        self.by_alias.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_alias.is_empty()
    }
}

/// Host-owned authority configuration. Absence is untrusted; adapters cannot
/// add entries to this policy while decoding their own wire declarations.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderClaimTrustPolicy {
    #[serde(default)]
    pub standard_protocols: BTreeMap<String, ProviderClaimTrust>,
    #[serde(default)]
    pub provider_extensions: BTreeMap<String, ProviderClaimTrust>,
    #[serde(default)]
    pub astra_components: BTreeMap<String, ProviderClaimTrust>,
}

impl ProviderClaimTrustPolicy {
    pub fn trust_for(&self, source: &ProviderClaimSource) -> ProviderClaimTrust {
        match source {
            ProviderClaimSource::StandardProtocol { protocol, .. } => self
                .standard_protocols
                .get(protocol.as_str())
                .copied()
                .unwrap_or_default(),
            ProviderClaimSource::ProviderExtension { namespace, .. } => self
                .provider_extensions
                .get(namespace)
                .copied()
                .unwrap_or_default(),
            ProviderClaimSource::AstraOwned { component, .. } => self
                .astra_components
                .get(component)
                .copied()
                .unwrap_or_default(),
        }
    }

    fn content_hash(&self) -> Result<String, ProviderResolutionError> {
        let encoded = serde_json::to_vec(self)
            .map_err(|error| ProviderResolutionError::Serialization(error.to_string()))?;
        Ok(format!("sha256:{:x}", Sha256::digest(encoded)))
    }
}

#[derive(Debug, Error)]
pub enum ProviderResolutionError {
    #[error(transparent)]
    Contract(#[from] ProviderContractError),
    #[error("failed to serialize provider resolution input: {0}")]
    Serialization(String),
    #[error("resolved alias '{alias}' references missing descriptor '{native_tool_id}'")]
    MissingResolvedDescriptor {
        alias: String,
        native_tool_id: String,
    },
    #[error("public provider alias '{alias}' collides across resolved snapshots")]
    CrossSnapshotAliasCollision { alias: String },
}

/// Resolve one complete discovery snapshot. `aliases` is supplied by Astra's
/// model/client projection layer and must contain exactly one alias per native
/// tool. The shared snapshot constructor rejects collisions, omissions and
/// references outside this binding.
pub fn resolve_provider_snapshot(
    discovery: &ProviderDiscoverySnapshot,
    trust_policy: &ProviderClaimTrustPolicy,
    aliases: &BTreeMap<NativeToolId, PublicToolAlias>,
) -> Result<ResolvedProviderSnapshot, ProviderResolutionError> {
    let mut drafts = Vec::with_capacity(discovery.tool_declarations.len());
    let mut projected_aliases = Vec::with_capacity(aliases.len());

    for declaration in &discovery.tool_declarations {
        let identity = discovery.tool_identity(declaration);
        if let Some(alias) = aliases.get(&declaration.native_tool_id) {
            projected_aliases.push((alias.clone(), identity.clone()));
        }
        let claims = resolve_claims(&declaration.claims, trust_policy);
        let semantic_baseline = resolve_semantics(&claims);
        let schema_hash =
            hash_serializable(&(&declaration.input_schema, &declaration.output_schema))?;
        drafts.push(ResolvedToolDescriptorDraft {
            identity,
            native_tool_name: declaration.native_tool_name.clone(),
            stable_tool_alias: declaration.stable_tool_alias.clone(),
            title: declaration.title.clone(),
            description: declaration.description.clone(),
            input_schema: declaration.input_schema.clone(),
            output_schema: declaration.output_schema.clone(),
            schema_hash,
            claims,
            task_support: declaration.task_support,
            extension_fields: declaration.extension_fields.clone(),
            semantic_baseline,
        });
    }

    // Preserve aliases targeting unknown tools so the contract layer can
    // reject them explicitly instead of degrading them into an omission.
    for (native_tool_id, alias) in aliases {
        if !discovery
            .tool_declarations
            .iter()
            .any(|declaration| &declaration.native_tool_id == native_tool_id)
        {
            projected_aliases.push((
                alias.clone(),
                astra_turn_types::ToolIdentity::new(
                    discovery.binding_ref.clone(),
                    native_tool_id.clone(),
                ),
            ));
        }
    }

    Ok(ResolvedProviderSnapshot::new(
        discovery.provider_identity.clone(),
        discovery.binding_ref.clone(),
        discovery.protocol.clone(),
        discovery.content_hash.clone(),
        ProviderResolverVersion::new(RESOLVER_VERSION)?,
        trust_policy.content_hash()?,
        drafts,
        projected_aliases,
    )?)
}

fn resolve_claims(
    claims: &astra_turn_types::ProviderToolClaims,
    trust_policy: &ProviderClaimTrustPolicy,
) -> ResolvedProviderToolClaims {
    ResolvedProviderToolClaims {
        read_only: resolve_claim(claims.read_only.as_ref(), trust_policy),
        destructive: resolve_claim(claims.destructive.as_ref(), trust_policy),
        idempotent: resolve_claim(claims.idempotent.as_ref(), trust_policy),
        open_world: resolve_claim(claims.open_world.as_ref(), trust_policy),
        semantic_cache: resolve_claim(claims.semantic_cache.as_ref(), trust_policy),
    }
}

fn resolve_claim<T: Clone>(
    claim: Option<&ProviderClaim<T>>,
    trust_policy: &ProviderClaimTrustPolicy,
) -> Option<ResolvedProviderClaim<T>> {
    claim.map(|claim| ResolvedProviderClaim {
        value: claim.value.clone(),
        trust: trust_policy.trust_for(&claim.source),
        source: claim.source.clone(),
    })
}

fn resolve_semantics(claims: &ResolvedProviderToolClaims) -> ResolvedToolSemantics {
    let mut diagnostics = Vec::new();
    let trusted_read_only = trusted_value(claims.read_only.as_ref());
    let trusted_destructive = trusted_value(claims.destructive.as_ref());
    let risk_increasing_claim = claims.destructive.as_ref().is_some_and(|claim| claim.value)
        || claims.read_only.as_ref().is_some_and(|claim| !claim.value);

    let effect = if trusted_read_only == Some(true) && risk_increasing_claim {
        diagnostics.push(diagnostic(
            ProviderSemanticDiagnosticCode::ContradictoryEffectClaims,
            "read-only conflicts with a risk-increasing effect claim",
        ));
        ResolvedToolEffect::Unknown
    } else if trusted_destructive == Some(true) || trusted_read_only == Some(false) {
        ResolvedToolEffect::Mutating
    } else if trusted_read_only == Some(true) {
        ResolvedToolEffect::ReadOnly
    } else {
        if claims.read_only.is_none() && claims.destructive.is_none() {
            diagnostics.push(diagnostic(
                ProviderSemanticDiagnosticCode::MissingEffectClaim,
                "provider supplied no effect claim",
            ));
        } else {
            diagnostics.push(diagnostic(
                ProviderSemanticDiagnosticCode::InsufficientEffectTrust,
                "provider effect claims are insufficiently trusted",
            ));
        }
        ResolvedToolEffect::Unknown
    };

    let trusted_idempotent = trusted_value(claims.idempotent.as_ref());
    let idempotency = match (effect, trusted_idempotent) {
        (_, Some(false)) => ResolvedToolIdempotency::NonIdempotent,
        (ResolvedToolEffect::ReadOnly, _) => ResolvedToolIdempotency::PureRead,
        (ResolvedToolEffect::Mutating, Some(true)) => ResolvedToolIdempotency::IdempotentEffect,
        (ResolvedToolEffect::Unknown, Some(true)) => {
            diagnostics.push(diagnostic(
                ProviderSemanticDiagnosticCode::IdempotencyWithoutKnownEffect,
                "idempotency alone cannot establish unknown side-effect semantics",
            ));
            ResolvedToolIdempotency::NonIdempotent
        }
        (_, None) => {
            if claims.idempotent.as_ref().is_some_and(|claim| claim.value) {
                diagnostics.push(diagnostic(
                    ProviderSemanticDiagnosticCode::InsufficientIdempotencyTrust,
                    "provider idempotency claim is insufficiently trusted",
                ));
            }
            ResolvedToolIdempotency::NonIdempotent
        }
    };

    let semantic_cache = match claims.semantic_cache.as_ref() {
        None => ResolvedSemanticCacheBaseline::Disabled,
        Some(claim) if claim.trust != ProviderClaimTrust::Trusted => {
            diagnostics.push(diagnostic(
                ProviderSemanticDiagnosticCode::InsufficientSemanticCacheTrust,
                "provider semantic-cache contract is insufficiently trusted",
            ));
            ResolvedSemanticCacheBaseline::Disabled
        }
        Some(_)
            if effect == ResolvedToolEffect::ReadOnly
                && idempotency == ResolvedToolIdempotency::PureRead =>
        {
            ResolvedSemanticCacheBaseline::FreshnessBound
        }
        Some(_) => {
            diagnostics.push(diagnostic(
                ProviderSemanticDiagnosticCode::SemanticCacheWithoutPureRead,
                "a semantic-cache contract cannot make a non-pure-read tool cacheable",
            ));
            ResolvedSemanticCacheBaseline::Disabled
        }
    };

    ResolvedToolSemantics {
        effect,
        idempotency,
        concurrency: if effect == ResolvedToolEffect::ReadOnly {
            ResolvedConcurrencyBaseline::ParallelReadOnly
        } else {
            ResolvedConcurrencyBaseline::Serial
        },
        // Eligibility never substitutes for the per-invocation revision fact.
        semantic_cache,
        diagnostics,
    }
}

fn trusted_value(claim: Option<&ResolvedProviderClaim<bool>>) -> Option<bool> {
    claim
        .filter(|claim| claim.trust == ProviderClaimTrust::Trusted)
        .map(|claim| claim.value)
}

fn diagnostic(
    code: ProviderSemanticDiagnosticCode,
    message: impl Into<String>,
) -> ProviderSemanticDiagnostic {
    ProviderSemanticDiagnostic {
        code,
        message: message.into(),
    }
}

fn hash_serializable(value: &impl Serialize) -> Result<String, ProviderResolutionError> {
    let encoded = serde_json::to_vec(value)
        .map_err(|error| ProviderResolutionError::Serialization(error.to_string()))?;
    Ok(format!("sha256:{:x}", Sha256::digest(encoded)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_turn_types::{
        ProviderBindingRef, ProviderClaimSource, ProviderIdentity, ProviderProtocolId,
        ProviderSemanticCacheContract, ProviderTaskSupport, ProviderToolClaims,
        ProviderToolDeclaration,
    };
    use serde_json::{Map, Value, json};

    fn protocol_claim(value: bool, field: &str) -> ProviderClaim<bool> {
        ProviderClaim::new(
            value,
            ProviderClaimSource::StandardProtocol {
                protocol: ProviderProtocolId::new("mcp").unwrap(),
                field: field.to_string(),
            },
        )
    }

    fn protocol_cache_claim(field: &str) -> ProviderClaim<ProviderSemanticCacheContract> {
        ProviderClaim::new(
            ProviderSemanticCacheContract::RevisionBound,
            ProviderClaimSource::StandardProtocol {
                protocol: ProviderProtocolId::new("mcp").unwrap(),
                field: field.to_string(),
            },
        )
    }

    fn extension_cache_claim() -> ProviderClaim<ProviderSemanticCacheContract> {
        ProviderClaim::new(
            ProviderSemanticCacheContract::RevisionBound,
            ProviderClaimSource::ProviderExtension {
                namespace: "example.cache".to_string(),
                field: "revisionBound".to_string(),
            },
        )
    }

    fn declaration(id: &str, claims: ProviderToolClaims) -> ProviderToolDeclaration {
        ProviderToolDeclaration {
            native_tool_id: NativeToolId::new(id).unwrap(),
            native_tool_name: id.to_string(),
            stable_tool_alias: None,
            title: None,
            description: None,
            input_schema: json!({"type": "object", "properties": {}}),
            output_schema: None,
            claims,
            task_support: ProviderTaskSupport::Unspecified,
            extension_fields: Map::new(),
        }
    }

    fn discovery(tools: Vec<ProviderToolDeclaration>) -> ProviderDiscoverySnapshot {
        discovery_for_binding("binding", tools)
    }

    fn discovery_for_binding(
        binding: &str,
        tools: Vec<ProviderToolDeclaration>,
    ) -> ProviderDiscoverySnapshot {
        ProviderDiscoverySnapshot::new(
            ProviderIdentity::new(format!("provider-{binding}")).unwrap(),
            ProviderBindingRef::new(binding).unwrap(),
            ProviderProtocolId::new("mcp").unwrap(),
            tools,
        )
        .unwrap()
    }

    fn aliases(ids: &[&str]) -> BTreeMap<NativeToolId, PublicToolAlias> {
        ids.iter()
            .map(|id| {
                (
                    NativeToolId::new(*id).unwrap(),
                    PublicToolAlias::new(format!("provider__{id}")).unwrap(),
                )
            })
            .collect()
    }

    fn trusted_mcp() -> ProviderClaimTrustPolicy {
        ProviderClaimTrustPolicy {
            standard_protocols: BTreeMap::from([("mcp".to_string(), ProviderClaimTrust::Trusted)]),
            ..ProviderClaimTrustPolicy::default()
        }
    }

    #[test]
    fn missing_and_untrusted_claims_fail_closed_with_diagnostics() {
        let no_claims = declaration("missing", ProviderToolClaims::default());
        let hinted = declaration(
            "hinted",
            ProviderToolClaims {
                read_only: Some(protocol_claim(true, "readOnlyHint")),
                idempotent: Some(protocol_claim(true, "idempotentHint")),
                ..ProviderToolClaims::default()
            },
        );
        let snapshot = resolve_provider_snapshot(
            &discovery(vec![no_claims, hinted]),
            &ProviderClaimTrustPolicy::default(),
            &aliases(&["missing", "hinted"]),
        )
        .unwrap();

        for descriptor in &snapshot.descriptors {
            let semantics = &descriptor.semantic_baseline;
            assert_eq!(semantics.effect, ResolvedToolEffect::Unknown);
            assert_eq!(
                semantics.idempotency,
                ResolvedToolIdempotency::NonIdempotent
            );
            assert_eq!(semantics.concurrency, ResolvedConcurrencyBaseline::Serial);
            assert_eq!(
                semantics.semantic_cache,
                ResolvedSemanticCacheBaseline::Disabled
            );
            assert!(!semantics.diagnostics.is_empty());
        }
    }

    #[test]
    fn trusted_read_only_relaxes_retry_and_concurrency_but_not_semantic_cache() {
        let tool = declaration(
            "read",
            ProviderToolClaims {
                read_only: Some(protocol_claim(true, "readOnlyHint")),
                ..ProviderToolClaims::default()
            },
        );
        let snapshot =
            resolve_provider_snapshot(&discovery(vec![tool]), &trusted_mcp(), &aliases(&["read"]))
                .unwrap();
        let semantics = &snapshot.descriptors[0].semantic_baseline;

        assert_eq!(semantics.effect, ResolvedToolEffect::ReadOnly);
        assert_eq!(semantics.idempotency, ResolvedToolIdempotency::PureRead);
        assert_eq!(
            semantics.concurrency,
            ResolvedConcurrencyBaseline::ParallelReadOnly
        );
        assert_eq!(
            semantics.semantic_cache,
            ResolvedSemanticCacheBaseline::Disabled
        );
    }

    #[test]
    fn trusted_revision_contract_marks_only_pure_reads_cache_eligible() {
        let tool = declaration(
            "read",
            ProviderToolClaims {
                read_only: Some(protocol_claim(true, "readOnlyHint")),
                semantic_cache: Some(protocol_cache_claim("revisionBound")),
                ..ProviderToolClaims::default()
            },
        );
        let snapshot =
            resolve_provider_snapshot(&discovery(vec![tool]), &trusted_mcp(), &aliases(&["read"]))
                .unwrap();
        let semantics = &snapshot.descriptors[0].semantic_baseline;

        assert_eq!(semantics.effect, ResolvedToolEffect::ReadOnly);
        assert_eq!(semantics.idempotency, ResolvedToolIdempotency::PureRead);
        assert_eq!(
            semantics.semantic_cache,
            ResolvedSemanticCacheBaseline::FreshnessBound
        );
    }

    #[test]
    fn untrusted_revision_contract_does_not_enable_cache() {
        let tool = declaration(
            "read",
            ProviderToolClaims {
                read_only: Some(protocol_claim(true, "readOnlyHint")),
                semantic_cache: Some(extension_cache_claim()),
                ..ProviderToolClaims::default()
            },
        );
        let snapshot =
            resolve_provider_snapshot(&discovery(vec![tool]), &trusted_mcp(), &aliases(&["read"]))
                .unwrap();
        let semantics = &snapshot.descriptors[0].semantic_baseline;

        assert_eq!(semantics.effect, ResolvedToolEffect::ReadOnly);
        assert_eq!(
            semantics.semantic_cache,
            ResolvedSemanticCacheBaseline::Disabled
        );
        assert!(semantics.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == ProviderSemanticDiagnosticCode::InsufficientSemanticCacheTrust
        }));
    }

    #[test]
    fn trusted_revision_contract_cannot_make_mutation_cacheable() {
        let tool = declaration(
            "write",
            ProviderToolClaims {
                read_only: Some(protocol_claim(false, "readOnlyHint")),
                semantic_cache: Some(protocol_cache_claim("revisionBound")),
                ..ProviderToolClaims::default()
            },
        );
        let snapshot =
            resolve_provider_snapshot(&discovery(vec![tool]), &trusted_mcp(), &aliases(&["write"]))
                .unwrap();
        let semantics = &snapshot.descriptors[0].semantic_baseline;

        assert_eq!(semantics.effect, ResolvedToolEffect::Mutating);
        assert_eq!(
            semantics.semantic_cache,
            ResolvedSemanticCacheBaseline::Disabled
        );
        assert!(semantics.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == ProviderSemanticDiagnosticCode::SemanticCacheWithoutPureRead
        }));
    }

    #[test]
    fn contradictory_claims_remain_visible_and_fail_closed() {
        let tool = declaration(
            "contradictory",
            ProviderToolClaims {
                read_only: Some(protocol_claim(true, "readOnlyHint")),
                destructive: Some(protocol_claim(true, "destructiveHint")),
                idempotent: Some(protocol_claim(true, "idempotentHint")),
                ..ProviderToolClaims::default()
            },
        );
        let snapshot = resolve_provider_snapshot(
            &discovery(vec![tool]),
            &trusted_mcp(),
            &aliases(&["contradictory"]),
        )
        .unwrap();
        let semantics = &snapshot.descriptors[0].semantic_baseline;

        assert_eq!(semantics.effect, ResolvedToolEffect::Unknown);
        assert_eq!(
            semantics.idempotency,
            ResolvedToolIdempotency::NonIdempotent
        );
        assert_eq!(semantics.concurrency, ResolvedConcurrencyBaseline::Serial);
        assert!(semantics.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == ProviderSemanticDiagnosticCode::ContradictoryEffectClaims
        }));
        assert!(semantics.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == ProviderSemanticDiagnosticCode::IdempotencyWithoutKnownEffect
        }));
    }

    #[test]
    fn trusted_idempotent_effect_does_not_become_read_or_cacheable() {
        let tool = declaration(
            "upsert",
            ProviderToolClaims {
                read_only: Some(protocol_claim(false, "readOnlyHint")),
                destructive: Some(protocol_claim(false, "destructiveHint")),
                idempotent: Some(protocol_claim(true, "idempotentHint")),
                ..ProviderToolClaims::default()
            },
        );
        let snapshot = resolve_provider_snapshot(
            &discovery(vec![tool]),
            &trusted_mcp(),
            &aliases(&["upsert"]),
        )
        .unwrap();
        let semantics = &snapshot.descriptors[0].semantic_baseline;

        assert_eq!(semantics.effect, ResolvedToolEffect::Mutating);
        assert_eq!(
            semantics.idempotency,
            ResolvedToolIdempotency::IdempotentEffect
        );
        assert_eq!(semantics.concurrency, ResolvedConcurrencyBaseline::Serial);
        assert_eq!(
            semantics.semantic_cache,
            ResolvedSemanticCacheBaseline::Disabled
        );
    }

    #[test]
    fn alias_rename_changes_projection_snapshot_but_not_identity_or_descriptor_version() {
        let discovery = discovery(vec![declaration("native", ProviderToolClaims::default())]);
        let first = resolve_provider_snapshot(
            &discovery,
            &ProviderClaimTrustPolicy::default(),
            &BTreeMap::from([(
                NativeToolId::new("native").unwrap(),
                PublicToolAlias::new("first_alias").unwrap(),
            )]),
        )
        .unwrap();
        let renamed = resolve_provider_snapshot(
            &discovery,
            &ProviderClaimTrustPolicy::default(),
            &BTreeMap::from([(
                NativeToolId::new("native").unwrap(),
                PublicToolAlias::new("renamed_alias").unwrap(),
            )]),
        )
        .unwrap();

        assert_eq!(
            first.descriptors[0].identity,
            renamed.descriptors[0].identity
        );
        assert_eq!(
            first.descriptors[0].descriptor_version,
            renamed.descriptors[0].descriptor_version
        );
        assert_ne!(first.alias_index, renamed.alias_index);
        assert_ne!(first.content_hash, renamed.content_hash);
    }

    #[test]
    fn policy_index_preserves_exact_descriptor_and_conservative_decisions() {
        let read_claims = ProviderToolClaims {
            read_only: Some(protocol_claim(true, "readOnlyHint")),
            ..ProviderToolClaims::default()
        };
        let snapshot = resolve_provider_snapshot(
            &discovery(vec![
                declaration("read", read_claims),
                declaration("unknown", ProviderToolClaims::default()),
            ]),
            &trusted_mcp(),
            &aliases(&["read", "unknown"]),
        )
        .unwrap();
        let index =
            ResolvedProviderPolicyIndex::from_snapshots(std::slice::from_ref(&snapshot)).unwrap();
        let read = index.resolve("provider__read").unwrap();
        let unknown = index.resolve("provider__unknown").unwrap();

        assert_eq!(index.len(), 2);
        assert!(read.is_read_only());
        assert!(read.parallelizable);
        assert!(!read.requires_approval());
        assert_eq!(
            read.descriptor,
            snapshot.alias_index[&PublicToolAlias::new("provider__read").unwrap()]
        );
        assert_eq!(unknown.effect, ResolvedToolEffect::Unknown);
        assert!(!unknown.parallelizable);
        assert!(unknown.requires_approval());
        assert_eq!(
            unknown.semantic_cache,
            ResolvedSemanticCacheBaseline::Disabled
        );
        assert_eq!(
            read.baseline_content_id().unwrap(),
            read.clone().baseline_content_id().unwrap()
        );
        let mut changed = read.clone();
        changed.approval = ProviderApprovalBaseline::RequiresApproval;
        assert_ne!(
            read.baseline_content_id().unwrap(),
            changed.baseline_content_id().unwrap()
        );
    }

    #[test]
    fn policy_index_rejects_cross_snapshot_alias_collisions() {
        let resolve = |binding: &str| {
            resolve_provider_snapshot(
                &discovery_for_binding(
                    binding,
                    vec![declaration("native", ProviderToolClaims::default())],
                ),
                &ProviderClaimTrustPolicy::default(),
                &BTreeMap::from([(
                    NativeToolId::new("native").unwrap(),
                    PublicToolAlias::new("shared_alias").unwrap(),
                )]),
            )
            .unwrap()
        };
        let error =
            ResolvedProviderPolicyIndex::from_snapshots(&[resolve("a"), resolve("b")]).unwrap_err();

        assert!(matches!(
            error,
            ProviderResolutionError::CrossSnapshotAliasCollision { .. }
        ));
    }

    #[test]
    fn policy_indexes_isolate_the_same_alias_across_sessions() {
        let discovery = discovery(vec![declaration(
            "read",
            ProviderToolClaims {
                read_only: Some(protocol_claim(true, "readOnlyHint")),
                ..ProviderToolClaims::default()
            },
        )]);
        let aliases = aliases(&["read"]);
        let trusted = resolve_provider_snapshot(&discovery, &trusted_mcp(), &aliases).unwrap();
        let advisory = resolve_provider_snapshot(
            &discovery,
            &ProviderClaimTrustPolicy {
                standard_protocols: BTreeMap::from([(
                    "mcp".to_string(),
                    ProviderClaimTrust::Advisory,
                )]),
                ..Default::default()
            },
            &aliases,
        )
        .unwrap();
        let trusted_index = ResolvedProviderPolicyIndex::from_snapshots(&[trusted]).unwrap();
        let advisory_index = ResolvedProviderPolicyIndex::from_snapshots(&[advisory]).unwrap();

        assert!(
            trusted_index
                .resolve("provider__read")
                .unwrap()
                .parallelizable
        );
        assert!(
            !advisory_index
                .resolve("provider__read")
                .unwrap()
                .parallelizable
        );
        assert!(
            advisory_index
                .resolve("provider__read")
                .unwrap()
                .requires_approval()
        );
    }

    #[test]
    fn resolution_is_order_stable_and_sibling_changes_are_descriptor_local() {
        let read_claims = ProviderToolClaims {
            read_only: Some(protocol_claim(true, "readOnlyHint")),
            ..ProviderToolClaims::default()
        };
        let first = resolve_provider_snapshot(
            &discovery(vec![
                declaration("b", ProviderToolClaims::default()),
                declaration("a", read_claims.clone()),
            ]),
            &trusted_mcp(),
            &aliases(&["a", "b"]),
        )
        .unwrap();
        let reordered = resolve_provider_snapshot(
            &discovery(vec![
                declaration("a", read_claims.clone()),
                declaration("b", ProviderToolClaims::default()),
            ]),
            &trusted_mcp(),
            &aliases(&["b", "a"]),
        )
        .unwrap();
        assert_eq!(first, reordered);

        let mut changed_b = declaration("b", ProviderToolClaims::default());
        changed_b.description = Some("changed sibling".to_string());
        let changed = resolve_provider_snapshot(
            &discovery(vec![declaration("a", read_claims), changed_b]),
            &trusted_mcp(),
            &aliases(&["a", "b"]),
        )
        .unwrap();
        let version = |snapshot: &ResolvedProviderSnapshot, id: &str| {
            snapshot
                .descriptors
                .iter()
                .find(|descriptor| descriptor.identity.native_tool_id.as_str() == id)
                .unwrap()
                .descriptor_version
                .clone()
        };
        assert_eq!(version(&first, "a"), version(&changed, "a"));
        assert_ne!(version(&first, "b"), version(&changed, "b"));
        assert_ne!(first.content_hash, changed.content_hash);
    }

    #[test]
    fn alias_collision_and_unknown_target_are_rejected() {
        let discovery = discovery(vec![
            declaration("a", ProviderToolClaims::default()),
            declaration("b", ProviderToolClaims::default()),
        ]);
        let collision = BTreeMap::from([
            (
                NativeToolId::new("a").unwrap(),
                PublicToolAlias::new("same").unwrap(),
            ),
            (
                NativeToolId::new("b").unwrap(),
                PublicToolAlias::new("same").unwrap(),
            ),
        ]);
        let error =
            resolve_provider_snapshot(&discovery, &ProviderClaimTrustPolicy::default(), &collision)
                .unwrap_err();
        assert!(matches!(
            error,
            ProviderResolutionError::Contract(ProviderContractError::DuplicatePublicAlias { .. })
        ));

        let unknown = BTreeMap::from([
            (
                NativeToolId::new("a").unwrap(),
                PublicToolAlias::new("a").unwrap(),
            ),
            (
                NativeToolId::new("b").unwrap(),
                PublicToolAlias::new("b").unwrap(),
            ),
            (
                NativeToolId::new("ghost").unwrap(),
                PublicToolAlias::new("ghost").unwrap(),
            ),
        ]);
        let error =
            resolve_provider_snapshot(&discovery, &ProviderClaimTrustPolicy::default(), &unknown)
                .unwrap_err();
        assert!(matches!(
            error,
            ProviderResolutionError::Contract(
                ProviderContractError::AliasTargetsUnknownTool { .. }
            )
        ));
    }

    #[test]
    fn serialized_snapshot_rejects_tampered_descriptor_and_parent_hash() {
        let snapshot = resolve_provider_snapshot(
            &discovery(vec![declaration("read", ProviderToolClaims::default())]),
            &ProviderClaimTrustPolicy::default(),
            &aliases(&["read"]),
        )
        .unwrap();
        let mut value = serde_json::to_value(&snapshot).unwrap();
        value["descriptors"][0]["descriptor_version"] = Value::String("forged".to_string());
        assert!(serde_json::from_value::<ResolvedProviderSnapshot>(value).is_err());

        let mut value = serde_json::to_value(&snapshot).unwrap();
        value["content_hash"] = Value::String("forged".to_string());
        assert!(serde_json::from_value::<ResolvedProviderSnapshot>(value).is_err());
    }
}
