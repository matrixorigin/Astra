//! Provider-neutral semantic read-cache identity.
//!
//! Delivery deduplication identifies one logical invocation. This module
//! instead identifies when two distinct pure-read invocations may reuse one
//! successful observation. No provider declaration is sufficient by itself:
//! reuse also requires concrete revision facts and an owner/security scope.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    DurableToolReference, ToolInvocationResultPayload, ToolInvocationTerminalOutcome,
    canonical_public_arguments_hash,
};

pub const SEMANTIC_READ_CACHE_CONTRACT_VERSION: &str = "semantic-read-cache-v1";
pub const SEMANTIC_READ_OBSERVATION_CONTRACT_VERSION: &str = "semantic-read-observation-v1";
pub const SEMANTIC_READ_OBSERVATION_MAX_BYTES: usize = 256 * 1024;
const MAX_FRESHNESS_COMPONENT_BYTES: usize = 4096;
const MAX_FRESHNESS_FACTS: usize = 64;
const MAX_POLICY_DECISION_ID_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticFreshnessScope {
    Workspace,
    Provider,
    Resource,
}

/// One opaque revision fact. Raw provider/resource identifiers and revision
/// tokens are hashed at construction so cache identity can be persisted and
/// observed without copying potentially sensitive values.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct SemanticFreshnessFact {
    pub scope: SemanticFreshnessScope,
    pub subject_id: String,
    pub revision_id: String,
}

impl SemanticFreshnessFact {
    pub fn new(
        scope: SemanticFreshnessScope,
        subject: &str,
        revision: &str,
    ) -> Result<Self, SemanticReadCacheContractError> {
        validate_raw_component("subject", subject)?;
        validate_raw_component("revision", revision)?;
        Ok(Self {
            scope,
            subject_id: tagged_content_id("freshness-subject", subject.as_bytes()),
            revision_id: tagged_content_id("freshness-revision", revision.as_bytes()),
        })
    }

    pub fn validate(&self) -> Result<(), SemanticReadCacheContractError> {
        validate_content_id("subject_id", &self.subject_id)?;
        validate_content_id("revision_id", &self.revision_id)
    }
}

impl<'de> Deserialize<'de> for SemanticFreshnessFact {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawFact {
            scope: SemanticFreshnessScope,
            subject_id: String,
            revision_id: String,
        }

        let raw = RawFact::deserialize(deserializer)?;
        let fact = Self {
            scope: raw.scope,
            subject_id: raw.subject_id,
            revision_id: raw.revision_id,
        };
        fact.validate().map_err(serde::de::Error::custom)?;
        Ok(fact)
    }
}

/// Canonical, content-addressed freshness and security namespace for one
/// semantic read-cache decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SemanticReadFreshnessContext {
    pub contract_version: String,
    pub security_scope_id: String,
    pub facts: Vec<SemanticFreshnessFact>,
    pub context_id: String,
}

/// Runtime evidence state presented to the frozen invocation decision. Cache
/// availability is an optimization: an unavailable source never fabricates a
/// context and never blocks the underlying tool call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticReadFreshnessUnavailableReason {
    SourceNotConfigured,
    RevisionUnavailable,
    SourceFailed,
    InvalidEvidence,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SemanticReadFreshnessResolution {
    Available(SemanticReadFreshnessContext),
    Unavailable(SemanticReadFreshnessUnavailableReason),
}

impl SemanticReadFreshnessContext {
    pub fn new(
        security_scope: &str,
        mut facts: Vec<SemanticFreshnessFact>,
    ) -> Result<Self, SemanticReadCacheContractError> {
        validate_raw_component("security_scope", security_scope)?;
        if facts.is_empty() {
            return Err(SemanticReadCacheContractError::EmptyFreshnessFacts);
        }
        validate_fact_count(facts.len())?;
        facts.sort();
        validate_canonical_facts(&facts)?;
        let mut context = Self {
            contract_version: SEMANTIC_READ_CACHE_CONTRACT_VERSION.to_string(),
            security_scope_id: tagged_content_id(
                "semantic-cache-security-scope",
                security_scope.as_bytes(),
            ),
            facts,
            context_id: String::new(),
        };
        context.context_id = context_content_id(&context)?;
        Ok(context)
    }

    pub fn validate(&self) -> Result<(), SemanticReadCacheContractError> {
        if self.contract_version != SEMANTIC_READ_CACHE_CONTRACT_VERSION {
            return Err(SemanticReadCacheContractError::UnsupportedContractVersion(
                self.contract_version.clone(),
            ));
        }
        validate_content_id("security_scope_id", &self.security_scope_id)?;
        validate_content_id("context_id", &self.context_id)?;
        if self.facts.is_empty() {
            return Err(SemanticReadCacheContractError::EmptyFreshnessFacts);
        }
        validate_fact_count(self.facts.len())?;
        for fact in &self.facts {
            fact.validate()?;
        }
        validate_canonical_facts(&self.facts)?;
        let expected = context_content_id(self)?;
        if self.context_id != expected {
            return Err(SemanticReadCacheContractError::ContentIdMismatch {
                field: "context_id",
                expected,
                actual: self.context_id.clone(),
            });
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for SemanticReadFreshnessContext {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawContext {
            contract_version: String,
            security_scope_id: String,
            facts: Vec<SemanticFreshnessFact>,
            context_id: String,
        }

        let raw = RawContext::deserialize(deserializer)?;
        let context = Self {
            contract_version: raw.contract_version,
            security_scope_id: raw.security_scope_id,
            facts: raw.facts,
            context_id: raw.context_id,
        };
        context.validate().map_err(serde::de::Error::custom)?;
        Ok(context)
    }
}

/// Full key for reuse across distinct pure-read invocation identities.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SemanticReadCacheKey {
    pub contract_version: String,
    pub tool: DurableToolReference,
    pub canonical_arguments_hash: String,
    pub policy_decision_id: String,
    pub freshness_context_id: String,
    pub key_id: String,
}

impl SemanticReadCacheKey {
    pub fn new(
        tool: DurableToolReference,
        arguments: &Value,
        policy_decision_id: &str,
        freshness: &SemanticReadFreshnessContext,
    ) -> Result<Self, SemanticReadCacheContractError> {
        freshness.validate()?;
        validate_policy_decision_id(policy_decision_id)?;
        let mut key = Self {
            contract_version: SEMANTIC_READ_CACHE_CONTRACT_VERSION.to_string(),
            tool,
            canonical_arguments_hash: canonical_public_arguments_hash(arguments),
            policy_decision_id: policy_decision_id.to_string(),
            freshness_context_id: freshness.context_id.clone(),
            key_id: String::new(),
        };
        key.key_id = key_content_id(&key)?;
        Ok(key)
    }

    pub fn validate(&self) -> Result<(), SemanticReadCacheContractError> {
        if self.contract_version != SEMANTIC_READ_CACHE_CONTRACT_VERSION {
            return Err(SemanticReadCacheContractError::UnsupportedContractVersion(
                self.contract_version.clone(),
            ));
        }
        validate_content_id("canonical_arguments_hash", &self.canonical_arguments_hash)?;
        validate_policy_decision_id(&self.policy_decision_id)?;
        validate_content_id("freshness_context_id", &self.freshness_context_id)?;
        validate_content_id("key_id", &self.key_id)?;
        let expected = key_content_id(self)?;
        if self.key_id != expected {
            return Err(SemanticReadCacheContractError::ContentIdMismatch {
                field: "key_id",
                expected,
                actual: self.key_id.clone(),
            });
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for SemanticReadCacheKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawKey {
            contract_version: String,
            tool: DurableToolReference,
            canonical_arguments_hash: String,
            policy_decision_id: String,
            freshness_context_id: String,
            key_id: String,
        }

        let raw = RawKey::deserialize(deserializer)?;
        let key = Self {
            contract_version: raw.contract_version,
            tool: raw.tool,
            canonical_arguments_hash: raw.canonical_arguments_hash,
            policy_decision_id: raw.policy_decision_id,
            freshness_context_id: raw.freshness_context_id,
            key_id: raw.key_id,
        };
        key.validate().map_err(serde::de::Error::custom)?;
        Ok(key)
    }
}

/// Content-addressed successful provider observation. Invocation-specific
/// presentation and cache-hit metadata are added after lookup and therefore
/// are not part of this reusable payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SemanticReadObservation {
    pub contract_version: String,
    pub key: SemanticReadCacheKey,
    pub result: ToolInvocationResultPayload,
    pub observation_id: String,
}

impl SemanticReadObservation {
    pub fn from_terminal_outcome(
        key: SemanticReadCacheKey,
        outcome: &ToolInvocationTerminalOutcome,
    ) -> Result<Self, SemanticReadCacheContractError> {
        let ToolInvocationTerminalOutcome::Succeeded { result } = outcome else {
            return Err(SemanticReadCacheContractError::NonSuccessfulObservation);
        };
        key.validate()?;
        let mut observation = Self {
            contract_version: SEMANTIC_READ_OBSERVATION_CONTRACT_VERSION.to_string(),
            key,
            result: result.clone(),
            observation_id: String::new(),
        };
        observation.observation_id = observation_content_id(&observation)?;
        observation.validate()?;
        Ok(observation)
    }

    pub fn encoded_len(&self) -> Result<usize, SemanticReadCacheContractError> {
        serde_json::to_vec(self)
            .map(|encoded| encoded.len())
            .map_err(|error| SemanticReadCacheContractError::Serialization(error.to_string()))
    }

    pub fn validate(&self) -> Result<(), SemanticReadCacheContractError> {
        if self.contract_version != SEMANTIC_READ_OBSERVATION_CONTRACT_VERSION {
            return Err(SemanticReadCacheContractError::UnsupportedContractVersion(
                self.contract_version.clone(),
            ));
        }
        self.key.validate()?;
        validate_content_id("observation_id", &self.observation_id)?;
        let expected = observation_content_id(self)?;
        if self.observation_id != expected {
            return Err(SemanticReadCacheContractError::ContentIdMismatch {
                field: "observation_id",
                expected,
                actual: self.observation_id.clone(),
            });
        }
        let encoded_bytes = self.encoded_len()?;
        if encoded_bytes > SEMANTIC_READ_OBSERVATION_MAX_BYTES {
            return Err(SemanticReadCacheContractError::ObservationTooLarge {
                max_bytes: SEMANTIC_READ_OBSERVATION_MAX_BYTES,
                actual_bytes: encoded_bytes,
            });
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for SemanticReadObservation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawObservation {
            contract_version: String,
            key: SemanticReadCacheKey,
            result: ToolInvocationResultPayload,
            observation_id: String,
        }

        let raw = RawObservation::deserialize(deserializer)?;
        let observation = Self {
            contract_version: raw.contract_version,
            key: raw.key,
            result: raw.result,
            observation_id: raw.observation_id,
        };
        observation.validate().map_err(serde::de::Error::custom)?;
        Ok(observation)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SemanticReadCacheContractError {
    #[error("semantic read-cache {field} must not be empty")]
    EmptyComponent { field: &'static str },
    #[error("semantic read-cache {field} exceeds {max_bytes} bytes (actual {actual_bytes} bytes)")]
    ComponentTooLong {
        field: &'static str,
        max_bytes: usize,
        actual_bytes: usize,
    },
    #[error("semantic read-cache {field} is not a full sha256 content ID")]
    InvalidContentId { field: &'static str },
    #[error("semantic read-cache freshness facts must not be empty")]
    EmptyFreshnessFacts,
    #[error(
        "semantic read-cache freshness facts exceed the {max_facts} fact limit (actual {actual_facts})"
    )]
    TooManyFreshnessFacts {
        max_facts: usize,
        actual_facts: usize,
    },
    #[error("semantic read-cache freshness facts are not in canonical order")]
    NonCanonicalFreshnessFacts,
    #[error("semantic read-cache contains multiple revisions for {scope:?} subject {subject_id}")]
    ConflictingFreshnessFacts {
        scope: SemanticFreshnessScope,
        subject_id: String,
    },
    #[error("unsupported semantic read-cache contract version: {0}")]
    UnsupportedContractVersion(String),
    #[error("semantic read-cache {field} content ID mismatch: expected {expected}, got {actual}")]
    ContentIdMismatch {
        field: &'static str,
        expected: String,
        actual: String,
    },
    #[error("semantic read-cache serialization failed: {0}")]
    Serialization(String),
    #[error("only a typed successful pure-read outcome can become a semantic observation")]
    NonSuccessfulObservation,
    #[error(
        "semantic read observation exceeds the {max_bytes} byte limit (actual {actual_bytes} bytes)"
    )]
    ObservationTooLarge {
        max_bytes: usize,
        actual_bytes: usize,
    },
}

fn validate_raw_component(
    field: &'static str,
    value: &str,
) -> Result<(), SemanticReadCacheContractError> {
    if value.trim().is_empty() {
        return Err(SemanticReadCacheContractError::EmptyComponent { field });
    }
    if value.len() > MAX_FRESHNESS_COMPONENT_BYTES {
        return Err(SemanticReadCacheContractError::ComponentTooLong {
            field,
            max_bytes: MAX_FRESHNESS_COMPONENT_BYTES,
            actual_bytes: value.len(),
        });
    }
    Ok(())
}

fn validate_policy_decision_id(value: &str) -> Result<(), SemanticReadCacheContractError> {
    if value.trim().is_empty() {
        return Err(SemanticReadCacheContractError::EmptyComponent {
            field: "policy_decision_id",
        });
    }
    if value.len() > MAX_POLICY_DECISION_ID_BYTES {
        return Err(SemanticReadCacheContractError::ComponentTooLong {
            field: "policy_decision_id",
            max_bytes: MAX_POLICY_DECISION_ID_BYTES,
            actual_bytes: value.len(),
        });
    }
    validate_content_id("policy_decision_id", value)
}

fn validate_fact_count(count: usize) -> Result<(), SemanticReadCacheContractError> {
    if count > MAX_FRESHNESS_FACTS {
        return Err(SemanticReadCacheContractError::TooManyFreshnessFacts {
            max_facts: MAX_FRESHNESS_FACTS,
            actual_facts: count,
        });
    }
    Ok(())
}

fn validate_content_id(
    field: &'static str,
    value: &str,
) -> Result<(), SemanticReadCacheContractError> {
    let valid = value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    });
    if valid {
        Ok(())
    } else {
        Err(SemanticReadCacheContractError::InvalidContentId { field })
    }
}

fn validate_canonical_facts(
    facts: &[SemanticFreshnessFact],
) -> Result<(), SemanticReadCacheContractError> {
    for pair in facts.windows(2) {
        let [left, right] = pair else {
            unreachable!("windows(2) always yields two elements")
        };
        if left > right {
            return Err(SemanticReadCacheContractError::NonCanonicalFreshnessFacts);
        }
        if left.scope == right.scope && left.subject_id == right.subject_id {
            return Err(SemanticReadCacheContractError::ConflictingFreshnessFacts {
                scope: left.scope,
                subject_id: left.subject_id.clone(),
            });
        }
    }
    Ok(())
}

fn context_content_id(
    context: &SemanticReadFreshnessContext,
) -> Result<String, SemanticReadCacheContractError> {
    #[derive(Serialize)]
    struct ContextContent<'a> {
        contract_version: &'a str,
        security_scope_id: &'a str,
        facts: &'a [SemanticFreshnessFact],
    }

    content_id(&ContextContent {
        contract_version: &context.contract_version,
        security_scope_id: &context.security_scope_id,
        facts: &context.facts,
    })
}

fn key_content_id(key: &SemanticReadCacheKey) -> Result<String, SemanticReadCacheContractError> {
    #[derive(Serialize)]
    struct KeyContent<'a> {
        contract_version: &'a str,
        tool: &'a DurableToolReference,
        canonical_arguments_hash: &'a str,
        policy_decision_id: &'a str,
        freshness_context_id: &'a str,
    }

    content_id(&KeyContent {
        contract_version: &key.contract_version,
        tool: &key.tool,
        canonical_arguments_hash: &key.canonical_arguments_hash,
        policy_decision_id: &key.policy_decision_id,
        freshness_context_id: &key.freshness_context_id,
    })
}

fn observation_content_id(
    observation: &SemanticReadObservation,
) -> Result<String, SemanticReadCacheContractError> {
    #[derive(Serialize)]
    struct ObservationContent<'a> {
        contract_version: &'a str,
        key: &'a SemanticReadCacheKey,
        result: &'a ToolInvocationResultPayload,
    }

    content_id(&ObservationContent {
        contract_version: &observation.contract_version,
        key: &observation.key,
        result: &observation.result,
    })
}

fn content_id(value: &impl Serialize) -> Result<String, SemanticReadCacheContractError> {
    let encoded = serde_json::to_vec(value)
        .map_err(|error| SemanticReadCacheContractError::Serialization(error.to_string()))?;
    Ok(format!("sha256:{:x}", Sha256::digest(encoded)))
}

fn tagged_content_id(tag: &str, value: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(tag.as_bytes());
    digest.update([0]);
    digest.update(value);
    format!("sha256:{:x}", digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProviderBindingRef, ResolvedToolDescriptorRef, ToolIdentity};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn fact(scope: SemanticFreshnessScope, subject: &str, revision: &str) -> SemanticFreshnessFact {
        SemanticFreshnessFact::new(scope, subject, revision).unwrap()
    }

    fn provider_tool(version: &str) -> DurableToolReference {
        DurableToolReference::Provider {
            descriptor: ResolvedToolDescriptorRef::new(
                ToolIdentity::new(
                    ProviderBindingRef::new("provider-binding").unwrap(),
                    crate::NativeToolId::new("native-read").unwrap(),
                ),
                version,
            )
            .unwrap(),
        }
    }

    fn decision_id(label: &str) -> String {
        tagged_content_id("test-decision", label.as_bytes())
    }

    fn cache_key() -> SemanticReadCacheKey {
        let freshness = SemanticReadFreshnessContext::new(
            "owner",
            vec![fact(SemanticFreshnessScope::Resource, "resource", "rev-1")],
        )
        .unwrap();
        SemanticReadCacheKey::new(
            provider_tool("descriptor-v1"),
            &json!({"query": "status"}),
            &decision_id("policy"),
            &freshness,
        )
        .unwrap()
    }

    fn success(output: String) -> ToolInvocationTerminalOutcome {
        ToolInvocationTerminalOutcome::Succeeded {
            result: ToolInvocationResultPayload {
                output,
                metadata: BTreeMap::from([("provider_status".to_string(), json!("fresh"))]),
                exit_semantics: None,
            },
        }
    }

    #[test]
    fn context_is_order_independent_but_fact_complete() {
        let workspace = fact(SemanticFreshnessScope::Workspace, "workspace-1", "rev-9");
        let resource = fact(SemanticFreshnessScope::Resource, "resource-a", "etag-7");
        let first = SemanticReadFreshnessContext::new(
            "tenant:user-1",
            vec![resource.clone(), workspace.clone()],
        )
        .unwrap();
        let reordered =
            SemanticReadFreshnessContext::new("tenant:user-1", vec![workspace, resource]).unwrap();

        assert_eq!(first, reordered);
        assert_eq!(first.facts.len(), 2);
    }

    #[test]
    fn raw_security_resource_and_revision_values_are_not_serialized() {
        let context = SemanticReadFreshnessContext::new(
            "secret-owner-scope",
            vec![fact(
                SemanticFreshnessScope::Resource,
                "secret-resource-uri",
                "secret-etag",
            )],
        )
        .unwrap();
        let encoded = serde_json::to_string(&context).unwrap();

        assert!(!encoded.contains("secret-owner-scope"));
        assert!(!encoded.contains("secret-resource-uri"));
        assert!(!encoded.contains("secret-etag"));
    }

    #[test]
    fn shared_prefix_revisions_do_not_alias() {
        let first = fact(
            SemanticFreshnessScope::Resource,
            "resource",
            "same-prefix-revision-a",
        );
        let second = fact(
            SemanticFreshnessScope::Resource,
            "resource",
            "same-prefix-revision-b",
        );

        assert_ne!(first.revision_id, second.revision_id);
    }

    #[test]
    fn conflicting_revisions_for_one_subject_fail_loudly() {
        let error = SemanticReadFreshnessContext::new(
            "owner",
            vec![
                fact(SemanticFreshnessScope::Resource, "resource", "rev-1"),
                fact(SemanticFreshnessScope::Resource, "resource", "rev-2"),
            ],
        )
        .unwrap_err();

        assert!(matches!(
            error,
            SemanticReadCacheContractError::ConflictingFreshnessFacts { .. }
        ));
    }

    #[test]
    fn empty_and_oversized_inputs_are_rejected() {
        assert!(matches!(
            SemanticFreshnessFact::new(SemanticFreshnessScope::Provider, " ", "rev"),
            Err(SemanticReadCacheContractError::EmptyComponent { field: "subject" })
        ));
        assert!(matches!(
            SemanticFreshnessFact::new(
                SemanticFreshnessScope::Provider,
                "provider",
                &"x".repeat(MAX_FRESHNESS_COMPONENT_BYTES + 1),
            ),
            Err(SemanticReadCacheContractError::ComponentTooLong {
                field: "revision",
                ..
            })
        ));
        assert!(matches!(
            SemanticReadFreshnessContext::new("owner", Vec::new()),
            Err(SemanticReadCacheContractError::EmptyFreshnessFacts)
        ));
        let too_many = (0..=MAX_FRESHNESS_FACTS)
            .map(|index| {
                fact(
                    SemanticFreshnessScope::Resource,
                    &format!("resource-{index}"),
                    "rev",
                )
            })
            .collect();
        assert!(matches!(
            SemanticReadFreshnessContext::new("owner", too_many),
            Err(SemanticReadCacheContractError::TooManyFreshnessFacts {
                max_facts: MAX_FRESHNESS_FACTS,
                actual_facts,
            }) if actual_facts == MAX_FRESHNESS_FACTS + 1
        ));
    }

    #[test]
    fn content_ids_are_canonical_lowercase_sha256() {
        let context = SemanticReadFreshnessContext::new(
            "owner",
            vec![fact(SemanticFreshnessScope::Provider, "provider", "rev-1")],
        )
        .unwrap();
        let mut encoded = serde_json::to_value(&context).unwrap();
        encoded["security_scope_id"] = Value::String(context.security_scope_id.to_uppercase());

        assert!(serde_json::from_value::<SemanticReadFreshnessContext>(encoded).is_err());
    }

    #[test]
    fn forged_or_noncanonical_contexts_do_not_deserialize() {
        let context = SemanticReadFreshnessContext::new(
            "owner",
            vec![
                fact(SemanticFreshnessScope::Workspace, "workspace", "rev-1"),
                fact(SemanticFreshnessScope::Resource, "resource", "rev-1"),
            ],
        )
        .unwrap();
        let mut forged = serde_json::to_value(&context).unwrap();
        forged["context_id"] = Value::String(decision_id("forged"));
        assert!(serde_json::from_value::<SemanticReadFreshnessContext>(forged).is_err());

        let mut reordered = serde_json::to_value(&context).unwrap();
        reordered["facts"].as_array_mut().unwrap().reverse();
        assert!(serde_json::from_value::<SemanticReadFreshnessContext>(reordered).is_err());
    }

    #[test]
    fn key_covers_tool_arguments_decision_freshness_and_security_scope() {
        let args = json!({"city": "Shanghai"});
        let freshness = SemanticReadFreshnessContext::new(
            "owner-a",
            vec![fact(
                SemanticFreshnessScope::Provider,
                "weather-provider",
                "rev-1",
            )],
        )
        .unwrap();
        let base = SemanticReadCacheKey::new(
            provider_tool("descriptor-v1"),
            &args,
            &decision_id("policy-a"),
            &freshness,
        )
        .unwrap();

        let cases = [
            SemanticReadCacheKey::new(
                provider_tool("descriptor-v2"),
                &args,
                &decision_id("policy-a"),
                &freshness,
            )
            .unwrap(),
            SemanticReadCacheKey::new(
                provider_tool("descriptor-v1"),
                &json!({"city": "Beijing"}),
                &decision_id("policy-a"),
                &freshness,
            )
            .unwrap(),
            SemanticReadCacheKey::new(
                provider_tool("descriptor-v1"),
                &args,
                &decision_id("policy-b"),
                &freshness,
            )
            .unwrap(),
            SemanticReadCacheKey::new(
                provider_tool("descriptor-v1"),
                &args,
                &decision_id("policy-a"),
                &SemanticReadFreshnessContext::new(
                    "owner-a",
                    vec![fact(
                        SemanticFreshnessScope::Provider,
                        "weather-provider",
                        "rev-2",
                    )],
                )
                .unwrap(),
            )
            .unwrap(),
            SemanticReadCacheKey::new(
                provider_tool("descriptor-v1"),
                &args,
                &decision_id("policy-a"),
                &SemanticReadFreshnessContext::new(
                    "owner-b",
                    vec![fact(
                        SemanticFreshnessScope::Provider,
                        "weather-provider",
                        "rev-1",
                    )],
                )
                .unwrap(),
            )
            .unwrap(),
        ];

        assert!(
            cases
                .iter()
                .all(|candidate| candidate.key_id != base.key_id)
        );
    }

    #[test]
    fn internal_transport_arguments_do_not_change_semantic_key() {
        let freshness = SemanticReadFreshnessContext::new(
            "owner",
            vec![fact(SemanticFreshnessScope::Provider, "provider", "rev-1")],
        )
        .unwrap();
        let first = SemanticReadCacheKey::new(
            provider_tool("descriptor-v1"),
            &json!({"query": "status", "_run_id": "run-a"}),
            &decision_id("policy"),
            &freshness,
        )
        .unwrap();
        let second = SemanticReadCacheKey::new(
            provider_tool("descriptor-v1"),
            &json!({"query": "status", "_run_id": "run-b"}),
            &decision_id("policy"),
            &freshness,
        )
        .unwrap();

        assert_eq!(first, second);
    }

    #[test]
    fn forged_key_does_not_deserialize() {
        let freshness = SemanticReadFreshnessContext::new(
            "owner",
            vec![fact(SemanticFreshnessScope::Provider, "provider", "rev-1")],
        )
        .unwrap();
        let key = SemanticReadCacheKey::new(
            provider_tool("descriptor-v1"),
            &json!({"query": "status"}),
            &decision_id("policy"),
            &freshness,
        )
        .unwrap();
        let mut encoded = serde_json::to_value(&key).unwrap();
        encoded["key_id"] = Value::String(decision_id("forged"));

        assert!(serde_json::from_value::<SemanticReadCacheKey>(encoded).is_err());
    }

    #[test]
    fn observation_accepts_only_typed_success_and_is_content_addressed() {
        let key = cache_key();
        let observation =
            SemanticReadObservation::from_terminal_outcome(key.clone(), &success("ok".into()))
                .unwrap();
        assert_eq!(observation.key, key);
        assert!(observation.encoded_len().unwrap() <= SEMANTIC_READ_OBSERVATION_MAX_BYTES);
        let restored: SemanticReadObservation =
            serde_json::from_value(serde_json::to_value(&observation).unwrap()).unwrap();
        assert_eq!(restored, observation);

        let failure = ToolInvocationTerminalOutcome::Failed {
            result: ToolInvocationResultPayload {
                output: "failed".to_string(),
                metadata: BTreeMap::new(),
                exit_semantics: None,
            },
            error_kind: Some("provider_failure".to_string()),
            retryable: false,
        };
        assert!(matches!(
            SemanticReadObservation::from_terminal_outcome(key, &failure),
            Err(SemanticReadCacheContractError::NonSuccessfulObservation)
        ));
    }

    #[test]
    fn oversized_or_forged_observations_fail_loudly() {
        assert!(matches!(
            SemanticReadObservation::from_terminal_outcome(
                cache_key(),
                &success("x".repeat(SEMANTIC_READ_OBSERVATION_MAX_BYTES)),
            ),
            Err(SemanticReadCacheContractError::ObservationTooLarge { .. })
        ));

        let observation = SemanticReadObservation::from_terminal_outcome(
            cache_key(),
            &success("valid".to_string()),
        )
        .unwrap();
        let mut encoded = serde_json::to_value(&observation).unwrap();
        encoded["result"]["output"] = json!("tampered");
        assert!(serde_json::from_value::<SemanticReadObservation>(encoded).is_err());
    }
}
