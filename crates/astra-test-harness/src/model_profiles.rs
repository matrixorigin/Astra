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
    // Walk ancestor directories so the harness finds the repo-root
    // `.models.yaml` whether invoked from the repo root, `rust/`, or
    // any nested working tree path. Mirrors
    // `astra_services::prompt_cache_capability_from_models_yaml` —
    // when the two disagreed, harness silently saw an empty profile
    // map and `skip_for_unsupported_cache_scope` defaulted to
    // `Unknown`, so cases requiring `conversation_turns` ran on
    // `intra_turn_rounds`-only models and failed in confusing ways.
    let base = match working_dir {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().ok()?,
    };
    let start = if base.is_dir() {
        base
    } else {
        base.parent()?.to_path_buf()
    };
    start
        .ancestors()
        .map(|dir| dir.join(".models.yaml"))
        .find(|path| path.is_file())
}

fn parse_profiles_str(yaml: &str) -> HashMap<String, ModelPromptCacheProfile> {
    let entries: Vec<ModelEntry> = serde_yaml_ng::from_str(yaml).unwrap_or_default();
    entries
        .into_iter()
        .map(|entry| {
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
            (entry.name, ModelPromptCacheProfile { reuse_support })
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

    #[test]
    fn load_profiles_walks_ancestor_directories_for_models_yaml() {
        // Regression: when astra-test is invoked from a nested working
        // dir (e.g. `rust/`) — as Make targets and many CI scripts do —
        // `models_yaml_path` MUST climb to find the repo-root file the
        // way the runtime's `prompt_cache_capability_from_models_yaml`
        // already does. Otherwise `skip_for_unsupported_cache_scope`
        // saw an empty profile map and ran intra-turn-only models
        // through cases requiring conversation-turn cache reuse.
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        let nested = repo.join("rust").join("crates").join("astra-cli");
        std::fs::create_dir_all(&nested).expect("create nested dirs");
        std::fs::write(
            repo.join(".models.yaml"),
            r#"
- name: kimi-k2.6
  provider: openai
  prompt_cache_capability:
    protocol: openai_auto_prefix
    volatile_placement: tail_suffix
    reuse_scope: intra_turn_rounds
"#,
        )
        .expect("write models yaml");

        let profiles = load_profiles(Some(&nested));
        assert_eq!(
            profiles
                .get("kimi-k2.6")
                .map(|profile| profile.reuse_support),
            Some(ModelReuseSupport::IntraTurnRounds),
            "load_profiles must locate ancestor `.models.yaml` (got profiles: {profiles:?})",
        );
    }
}
