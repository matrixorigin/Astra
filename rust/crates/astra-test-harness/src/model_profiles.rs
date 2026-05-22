use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::case::PromptCacheReuseScope;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelReuseSupport {
    Unknown,
    IntraTurnRounds,
    ConversationTurns,
}

impl ModelReuseSupport {
    pub fn supports(self, required: PromptCacheReuseScope) -> bool {
        match (self, required) {
            (Self::Unknown, _) => true,
            (Self::ConversationTurns, _) => true,
            (Self::IntraTurnRounds, PromptCacheReuseScope::IntraTurnRounds) => true,
            (Self::IntraTurnRounds, PromptCacheReuseScope::ConversationTurns) => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelPromptCacheProfile {
    pub reuse_support: ModelReuseSupport,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    name: String,
    #[serde(default)]
    prompt_cache_capability: Option<astra_services::PromptCacheCapabilityData>,
    #[serde(default)]
    quirks: Option<ModelQuirksYaml>,
}

#[derive(Debug, Deserialize, Default)]
struct ModelQuirksYaml {
    #[serde(default)]
    prompt_cache_capability: Option<astra_services::PromptCacheCapabilityData>,
}

fn models_yaml_path(working_dir: Option<&Path>) -> Option<PathBuf> {
    let base = match working_dir {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().ok()?,
    };
    let path = base.join(".models.yaml");
    path.exists().then_some(path)
}

fn parse_profiles_str(yaml: &str) -> HashMap<String, ModelPromptCacheProfile> {
    let entries: Vec<ModelEntry> = serde_yaml_ng::from_str(yaml).unwrap_or_default();
    entries
        .into_iter()
        .filter_map(|entry| {
            let cap = entry
                .prompt_cache_capability
                .or_else(|| entry.quirks.and_then(|q| q.prompt_cache_capability));
            let reuse_support = match cap.and_then(|c| c.reuse_scope) {
                Some(PromptCacheReuseScope::ConversationTurns) => {
                    ModelReuseSupport::ConversationTurns
                }
                Some(PromptCacheReuseScope::IntraTurnRounds) => ModelReuseSupport::IntraTurnRounds,
                None => ModelReuseSupport::Unknown,
            };
            Some((entry.name, ModelPromptCacheProfile { reuse_support }))
        })
        .collect()
}

pub fn load_profiles(working_dir: Option<&Path>) -> HashMap<String, ModelPromptCacheProfile> {
    let Some(path) = models_yaml_path(working_dir) else {
        return HashMap::new();
    };
    let Ok(yaml) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    parse_profiles_str(&yaml)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_profiles_reads_top_level_reuse_scope() {
        let yaml = r#"
- name: kimi-k2.6
  provider: openai
  prompt_cache_capability:
    protocol: openai_auto_prefix
    volatile_placement: tail_suffix
    reuse_scope: intra_turn_rounds
- name: deepseek-v4-flash
  provider: openai
"#;
        let profiles = parse_profiles_str(yaml);
        assert_eq!(
            profiles["kimi-k2.6"].reuse_support,
            ModelReuseSupport::IntraTurnRounds
        );
        assert_eq!(
            profiles["deepseek-v4-flash"].reuse_support,
            ModelReuseSupport::Unknown
        );
    }

    #[test]
    fn conversation_turns_supports_intra_turn_requirements() {
        assert!(
            ModelReuseSupport::ConversationTurns.supports(PromptCacheReuseScope::IntraTurnRounds)
        );
        assert!(
            !ModelReuseSupport::IntraTurnRounds.supports(PromptCacheReuseScope::ConversationTurns)
        );
    }
}
