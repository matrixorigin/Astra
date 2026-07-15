//! Provider-neutral runtime source for semantic read freshness evidence.
//!
//! The source supplies provider/resource revision facts for an exact native
//! descriptor. Astra remains responsible for trust, cache eligibility,
//! security scoping, key construction, and observation reuse.

use astra_turn_types::{
    ResolvedToolDescriptorRef, SemanticFreshnessFact, SemanticReadFreshnessUnavailableReason,
    canonical_public_tool_arguments,
};
use serde_json::Value;
use thiserror::Error;

pub struct ProviderSemanticFreshnessRequest<'a> {
    pub descriptor: &'a ResolvedToolDescriptorRef,
    pub public_arguments: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderSemanticFreshnessEvidence {
    Current { facts: Vec<SemanticFreshnessFact> },
    Unavailable,
}

/// A freshness source is a snapshot/evidence lookup, not a second provider
/// execution path. Implementations must be side-effect free and non-blocking;
/// network revalidation belongs to an explicit conditional provider call.
pub trait ProviderSemanticFreshnessSource: Send + Sync {
    fn resolve(
        &self,
        request: ProviderSemanticFreshnessRequest<'_>,
    ) -> Result<ProviderSemanticFreshnessEvidence, ProviderSemanticFreshnessSourceError>;
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ProviderSemanticFreshnessSourceError {
    #[error("provider semantic freshness source failed")]
    SourceFailed,
}

pub(crate) fn resolve_provider_semantic_freshness(
    source: Option<&dyn ProviderSemanticFreshnessSource>,
    descriptor: &ResolvedToolDescriptorRef,
    arguments: &Value,
) -> Result<Vec<SemanticFreshnessFact>, SemanticReadFreshnessUnavailableReason> {
    let Some(source) = source else {
        return Err(SemanticReadFreshnessUnavailableReason::SourceNotConfigured);
    };
    let request = ProviderSemanticFreshnessRequest {
        descriptor,
        public_arguments: canonical_public_tool_arguments(arguments),
    };
    match source.resolve(request) {
        Ok(ProviderSemanticFreshnessEvidence::Current { facts }) => Ok(facts),
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

    impl ProviderSemanticFreshnessSource for RecordingSource {
        fn resolve(
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

    #[test]
    fn source_receives_native_descriptor_and_only_canonical_public_arguments() {
        let descriptor = descriptor();
        let source = RecordingSource {
            seen: Mutex::new(Vec::new()),
            response: Ok(ProviderSemanticFreshnessEvidence::Current {
                facts: vec![
                    SemanticFreshnessFact::new(
                        SemanticFreshnessScope::Resource,
                        "resource-a",
                        "rev-7",
                    )
                    .unwrap(),
                ],
            }),
        };
        let facts = resolve_provider_semantic_freshness(
            Some(&source),
            &descriptor,
            &serde_json::json!({
                "z": 1,
                "a": {"z": 2, "a": 1},
                "_run_id": "run-secret",
                "_tool_call_id": "call-secret",
            }),
        )
        .unwrap();

        assert_eq!(facts.len(), 1);
        let seen = source.seen.lock().unwrap();
        assert_eq!(seen[0].0, descriptor);
        assert_eq!(
            seen[0].1,
            serde_json::json!({"a": {"a": 1, "z": 2}, "z": 1})
        );
        assert!(!seen[0].1.to_string().contains("secret"));
    }

    #[test]
    fn unavailable_and_failed_sources_remain_distinct() {
        let descriptor = descriptor();
        assert_eq!(
            resolve_provider_semantic_freshness(None, &descriptor, &Value::Null),
            Err(SemanticReadFreshnessUnavailableReason::SourceNotConfigured)
        );
        let unavailable = RecordingSource {
            seen: Mutex::new(Vec::new()),
            response: Ok(ProviderSemanticFreshnessEvidence::Unavailable),
        };
        assert_eq!(
            resolve_provider_semantic_freshness(Some(&unavailable), &descriptor, &Value::Null),
            Err(SemanticReadFreshnessUnavailableReason::RevisionUnavailable)
        );
        let failed = RecordingSource {
            seen: Mutex::new(Vec::new()),
            response: Err(ProviderSemanticFreshnessSourceError::SourceFailed),
        };
        assert_eq!(
            resolve_provider_semantic_freshness(Some(&failed), &descriptor, &Value::Null),
            Err(SemanticReadFreshnessUnavailableReason::SourceFailed)
        );
    }
}
