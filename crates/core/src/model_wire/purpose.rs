//! Protocol capabilities shared by catalog projection and inference admission.
//! Availability, ownership and credentials remain independent gates.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelRequestPurpose {
    Chat,
    TypedJudgment,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelCatalogPurpose {
    #[default]
    Chat,
    TypedJudgment,
    /// Registry inspection, not permission to execute an inference.
    All,
}

impl ModelCatalogPurpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::TypedJudgment => "typed_judgment",
            Self::All => "all",
        }
    }

    pub fn supports_provider(self, provider: &str) -> bool {
        match self {
            Self::Chat => ModelRequestPurpose::Chat.supported_by(provider),
            Self::TypedJudgment => ModelRequestPurpose::TypedJudgment.supported_by(provider),
            Self::All => true,
        }
    }
}

impl ModelRequestPurpose {
    pub fn supported_by(self, provider: &str) -> bool {
        // TypeSafe's adapter implements only the typed nonstream judgment
        // protocol. Other current adapters implement chat and can carry the
        // canonical discrete judgment contract. No model-name heuristics.
        match provider {
            "typesafe" => self == Self::TypedJudgment,
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_purpose_does_not_remove_explicit_llm_judges() {
        assert!(!ModelRequestPurpose::Chat.supported_by("typesafe"));
        assert!(ModelRequestPurpose::TypedJudgment.supported_by("typesafe"));
        for provider in [
            "openai",
            "deepseek",
            "anthropic",
            "openai-compatible",
            "mock",
        ] {
            assert!(ModelRequestPurpose::Chat.supported_by(provider));
            assert!(ModelRequestPurpose::TypedJudgment.supported_by(provider));
        }
        assert!(ModelCatalogPurpose::All.supports_provider("typesafe"));
    }
}
