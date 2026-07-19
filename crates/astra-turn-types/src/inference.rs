use serde::{Deserialize, Serialize};

/// Policy- and attribution-relevant reason for one logical model invocation.
///
/// This taxonomy describes why Astra is spending model capacity. It is not a
/// provider adapter, product source, or UI label. Every model call must choose
/// one variant before reaching an executor so policy, budgets, usage, and
/// recovery can share the same fact across Server and Edge paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferencePurpose {
    PrimaryAgent,
    SubAgent,
    RequiredCompaction,
    MemoryExtraction,
    MemoryRetrievalRerank,
    Reflection,
    Introspection,
    VerificationJudge,
    Embedding,
}

impl InferencePurpose {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PrimaryAgent => "primary_agent",
            Self::SubAgent => "sub_agent",
            Self::RequiredCompaction => "required_compaction",
            Self::MemoryExtraction => "memory_extraction",
            Self::MemoryRetrievalRerank => "memory_retrieval_rerank",
            Self::Reflection => "reflection",
            Self::Introspection => "introspection",
            Self::VerificationJudge => "verification_judge",
            Self::Embedding => "embedding",
        }
    }
}

impl std::fmt::Display for InferencePurpose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_identity_round_trips_every_purpose() {
        let purposes = [
            InferencePurpose::PrimaryAgent,
            InferencePurpose::SubAgent,
            InferencePurpose::RequiredCompaction,
            InferencePurpose::MemoryExtraction,
            InferencePurpose::MemoryRetrievalRerank,
            InferencePurpose::Reflection,
            InferencePurpose::Introspection,
            InferencePurpose::VerificationJudge,
            InferencePurpose::Embedding,
        ];

        for purpose in purposes {
            let encoded = serde_json::to_value(purpose).expect("serialize inference purpose");
            let decoded: InferencePurpose = serde_json::from_value(encoded.clone())
                .expect("deserialize serialized inference purpose");
            assert_eq!(decoded, purpose);
            assert_eq!(encoded.as_str(), Some(purpose.as_str()));
        }
    }

    #[test]
    fn unknown_purpose_is_rejected_instead_of_silently_reclassified() {
        let result = serde_json::from_value::<InferencePurpose>(serde_json::json!("other"));
        assert!(result.is_err());
    }
}
