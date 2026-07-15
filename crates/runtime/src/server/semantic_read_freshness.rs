//! Provider-neutral runtime source for semantic read freshness evidence.
//!
//! The source supplies provider/resource revision facts for an exact native
//! descriptor. Astra remains responsible for trust, cache eligibility,
//! security scoping, key construction, and observation reuse.

use astra_turn_types::{
    ResolvedToolDescriptorRef, SemanticFreshnessFact, SemanticReadFreshnessUnavailableReason,
    canonical_public_tool_arguments,
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
}
