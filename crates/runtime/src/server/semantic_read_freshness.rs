//! Provider-neutral runtime source for semantic read freshness evidence.
//!
//! The source supplies provider/resource revision facts for an exact native
//! descriptor. Astra remains responsible for trust, cache eligibility,
//! security scoping, key construction, and observation reuse.

use std::collections::BTreeMap;
use std::sync::Arc;

use astra_turn_types::{
    ProviderBindingRef, ResolvedToolDescriptorRef, SemanticFreshnessFact,
    SemanticReadFreshnessUnavailableReason, canonical_public_tool_arguments,
};
use async_trait::async_trait;
use serde_json::Value;
use thiserror::Error;

pub struct ProviderSemanticFreshnessRequest<'a> {
    pub descriptor: &'a ResolvedToolDescriptorRef,
    pub public_arguments: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderSemanticFreshnessEvidence {
    Conditional {
        facts: Vec<SemanticFreshnessFact>,
        protocol: String,
        token: String,
    },
    Unavailable,
}

/// Prepares provider freshness evidence and the opaque precondition for the
/// exact native read that follows. Preparation may perform side-effect-free
/// I/O, but must never execute the requested tool itself.
#[async_trait]
pub trait ProviderSemanticFreshnessSource: Send + Sync {
    async fn prepare(
        &self,
        request: ProviderSemanticFreshnessRequest<'_>,
    ) -> Result<ProviderSemanticFreshnessEvidence, ProviderSemanticFreshnessSourceError>;
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ProviderSemanticFreshnessSourceError {
    #[error("provider semantic freshness source failed")]
    SourceFailed,
}

/// Immutable production capability registry keyed by the exact provider
/// binding carried by a resolved descriptor. Protocol names and model-facing
/// aliases are deliberately absent from the routing key.
#[derive(Clone, Default)]
pub struct ProviderSemanticFreshnessRegistry {
    sources: BTreeMap<ProviderBindingRef, Arc<dyn ProviderSemanticFreshnessSource>>,
}

impl ProviderSemanticFreshnessRegistry {
    pub fn register(
        &mut self,
        binding: ProviderBindingRef,
        source: Arc<dyn ProviderSemanticFreshnessSource>,
    ) -> Result<(), ProviderSemanticFreshnessRegistryError> {
        match self.sources.entry(binding) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(source);
                Ok(())
            }
            std::collections::btree_map::Entry::Occupied(entry) => {
                Err(ProviderSemanticFreshnessRegistryError::DuplicateBinding {
                    binding: entry.key().to_string(),
                })
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    pub fn len(&self) -> usize {
        self.sources.len()
    }
}

#[async_trait]
impl ProviderSemanticFreshnessSource for ProviderSemanticFreshnessRegistry {
    async fn prepare(
        &self,
        request: ProviderSemanticFreshnessRequest<'_>,
    ) -> Result<ProviderSemanticFreshnessEvidence, ProviderSemanticFreshnessSourceError> {
        let Some(source) = self
            .sources
            .get(&request.descriptor.identity.provider_binding)
        else {
            return Ok(ProviderSemanticFreshnessEvidence::Unavailable);
        };
        source.prepare(request).await
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ProviderSemanticFreshnessRegistryError {
    #[error("semantic freshness capability already registered for provider binding {binding}")]
    DuplicateBinding { binding: String },
}

pub(crate) async fn prepare_provider_semantic_freshness(
    source: Option<&dyn ProviderSemanticFreshnessSource>,
    descriptor: &ResolvedToolDescriptorRef,
    arguments: &Value,
) -> Result<ProviderSemanticFreshnessEvidence, SemanticReadFreshnessUnavailableReason> {
    let Some(source) = source else {
        return Err(SemanticReadFreshnessUnavailableReason::SourceNotConfigured);
    };
    let request = ProviderSemanticFreshnessRequest {
        descriptor,
        public_arguments: canonical_public_tool_arguments(arguments),
    };
    match source.prepare(request).await {
        Ok(evidence @ ProviderSemanticFreshnessEvidence::Conditional { .. }) => Ok(evidence),
        Ok(ProviderSemanticFreshnessEvidence::Unavailable) => {
            Err(SemanticReadFreshnessUnavailableReason::RevisionUnavailable)
        }
        Err(ProviderSemanticFreshnessSourceError::SourceFailed) => {
            Err(SemanticReadFreshnessUnavailableReason::SourceFailed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_turn_types::{
        NativeToolId, ProviderBindingRef, SemanticFreshnessScope, ToolIdentity,
    };
    use std::sync::Mutex;

    struct RecordingSource {
        seen: Mutex<Vec<(ResolvedToolDescriptorRef, Value)>>,
        response: Result<ProviderSemanticFreshnessEvidence, ProviderSemanticFreshnessSourceError>,
    }

    #[async_trait]
    impl ProviderSemanticFreshnessSource for RecordingSource {
        async fn prepare(
            &self,
            request: ProviderSemanticFreshnessRequest<'_>,
        ) -> Result<ProviderSemanticFreshnessEvidence, ProviderSemanticFreshnessSourceError>
        {
            self.seen
                .lock()
                .unwrap()
                .push((request.descriptor.clone(), request.public_arguments));
            self.response.clone()
        }
    }

    fn descriptor() -> ResolvedToolDescriptorRef {
        ResolvedToolDescriptorRef::new(
            ToolIdentity::new(
                ProviderBindingRef::new("binding-a").unwrap(),
                NativeToolId::new("native-read").unwrap(),
            ),
            "descriptor-v1",
        )
        .unwrap()
    }

    #[tokio::test]
    async fn source_receives_native_descriptor_and_only_canonical_public_arguments() {
        let descriptor = descriptor();
        let source = RecordingSource {
            seen: Mutex::new(Vec::new()),
            response: Ok(ProviderSemanticFreshnessEvidence::Conditional {
                facts: vec![
                    SemanticFreshnessFact::new(
                        SemanticFreshnessScope::Resource,
                        "resource-a",
                        "rev-7",
                    )
                    .unwrap(),
                ],
                protocol: "if-match".to_string(),
                token: "etag-7".to_string(),
            }),
        };
        let evidence = prepare_provider_semantic_freshness(
            Some(&source),
            &descriptor,
            &serde_json::json!({
                "z": 1,
                "a": {"z": 2, "a": 1},
                "_run_id": "run-secret",
                "_tool_call_id": "call-secret",
            }),
        )
        .await
        .unwrap();

        assert!(matches!(
            evidence,
            ProviderSemanticFreshnessEvidence::Conditional { ref facts, .. } if facts.len() == 1
        ));
        let seen = source.seen.lock().unwrap();
        assert_eq!(seen[0].0, descriptor);
        assert_eq!(
            seen[0].1,
            serde_json::json!({"a": {"a": 1, "z": 2}, "z": 1})
        );
        assert!(!seen[0].1.to_string().contains("secret"));
    }

    #[tokio::test]
    async fn unavailable_and_failed_sources_remain_distinct() {
        let descriptor = descriptor();
        assert_eq!(
            prepare_provider_semantic_freshness(None, &descriptor, &Value::Null).await,
            Err(SemanticReadFreshnessUnavailableReason::SourceNotConfigured)
        );
        let unavailable = RecordingSource {
            seen: Mutex::new(Vec::new()),
            response: Ok(ProviderSemanticFreshnessEvidence::Unavailable),
        };
        assert_eq!(
            prepare_provider_semantic_freshness(Some(&unavailable), &descriptor, &Value::Null)
                .await,
            Err(SemanticReadFreshnessUnavailableReason::RevisionUnavailable)
        );
        let failed = RecordingSource {
            seen: Mutex::new(Vec::new()),
            response: Err(ProviderSemanticFreshnessSourceError::SourceFailed),
        };
        assert_eq!(
            prepare_provider_semantic_freshness(Some(&failed), &descriptor, &Value::Null).await,
            Err(SemanticReadFreshnessUnavailableReason::SourceFailed)
        );
    }

    #[tokio::test]
    async fn registry_routes_mcp_shaped_and_native_capabilities_by_exact_binding() {
        let mcp = Arc::new(RecordingSource {
            seen: Mutex::new(Vec::new()),
            response: Ok(ProviderSemanticFreshnessEvidence::Conditional {
                facts: vec![
                    SemanticFreshnessFact::new(
                        SemanticFreshnessScope::Resource,
                        "mcp-resource",
                        "etag-4",
                    )
                    .unwrap(),
                ],
                protocol: "mcp.etag".to_string(),
                token: "if-none-match:etag-4".to_string(),
            }),
        });
        let native = Arc::new(RecordingSource {
            seen: Mutex::new(Vec::new()),
            response: Ok(ProviderSemanticFreshnessEvidence::Conditional {
                facts: vec![
                    SemanticFreshnessFact::new(
                        SemanticFreshnessScope::Provider,
                        "catalog",
                        "revision-9",
                    )
                    .unwrap(),
                ],
                protocol: "catalog.revision".to_string(),
                token: "revision-9".to_string(),
            }),
        });
        let mut registry = ProviderSemanticFreshnessRegistry::default();
        registry
            .register(ProviderBindingRef::new("binding-mcp").unwrap(), mcp.clone())
            .unwrap();
        registry
            .register(
                ProviderBindingRef::new("binding-native").unwrap(),
                native.clone(),
            )
            .unwrap();

        let mcp_descriptor = ResolvedToolDescriptorRef::new(
            ToolIdentity::new(
                ProviderBindingRef::new("binding-mcp").unwrap(),
                NativeToolId::new("read").unwrap(),
            ),
            "descriptor-v1",
        )
        .unwrap();
        let native_descriptor = ResolvedToolDescriptorRef::new(
            ToolIdentity::new(
                ProviderBindingRef::new("binding-native").unwrap(),
                NativeToolId::new("read").unwrap(),
            ),
            "descriptor-v1",
        )
        .unwrap();
        let mcp_evidence = prepare_provider_semantic_freshness(
            Some(&registry),
            &mcp_descriptor,
            &serde_json::json!({"resource": "a"}),
        )
        .await
        .unwrap();
        let native_evidence = prepare_provider_semantic_freshness(
            Some(&registry),
            &native_descriptor,
            &serde_json::json!({"resource": "b"}),
        )
        .await
        .unwrap();

        assert!(matches!(
            mcp_evidence,
            ProviderSemanticFreshnessEvidence::Conditional { ref protocol, .. }
                if protocol == "mcp.etag"
        ));
        assert!(matches!(
            native_evidence,
            ProviderSemanticFreshnessEvidence::Conditional { ref protocol, .. }
                if protocol == "catalog.revision"
        ));
        assert_eq!(mcp.seen.lock().unwrap().len(), 1);
        assert_eq!(native.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn registry_rejects_duplicate_authority_and_unknown_binding_is_unavailable() {
        let source = Arc::new(RecordingSource {
            seen: Mutex::new(Vec::new()),
            response: Ok(ProviderSemanticFreshnessEvidence::Conditional {
                facts: vec![
                    SemanticFreshnessFact::new(
                        SemanticFreshnessScope::Resource,
                        "resource-a",
                        "revision-1",
                    )
                    .unwrap(),
                ],
                protocol: "revision".to_string(),
                token: "revision-1".to_string(),
            }),
        });
        let replacement = Arc::new(RecordingSource {
            seen: Mutex::new(Vec::new()),
            response: Ok(ProviderSemanticFreshnessEvidence::Unavailable),
        });
        let binding = ProviderBindingRef::new("binding-a").unwrap();
        let mut registry = ProviderSemanticFreshnessRegistry::default();
        registry.register(binding.clone(), source.clone()).unwrap();
        assert_eq!(
            registry.register(binding, replacement),
            Err(ProviderSemanticFreshnessRegistryError::DuplicateBinding {
                binding: "binding-a".to_string(),
            })
        );
        assert_eq!(registry.len(), 1);
        assert!(matches!(
            prepare_provider_semantic_freshness(Some(&registry), &descriptor(), &Value::Null).await,
            Ok(ProviderSemanticFreshnessEvidence::Conditional { .. })
        ));

        let unknown = ResolvedToolDescriptorRef::new(
            ToolIdentity::new(
                ProviderBindingRef::new("binding-unknown").unwrap(),
                NativeToolId::new("read").unwrap(),
            ),
            "descriptor-v1",
        )
        .unwrap();
        assert_eq!(
            prepare_provider_semantic_freshness(Some(&registry), &unknown, &Value::Null).await,
            Err(SemanticReadFreshnessUnavailableReason::RevisionUnavailable)
        );
    }
}
