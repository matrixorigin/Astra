use serde::{Deserialize, Serialize};

/// Identity admitted from a typed skill directory. Aliases select this
/// identity; they never authorize a different canonical skill.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillCatalogIdentity {
    pub name: String,
    pub aliases: Vec<String>,
}

impl SkillCatalogIdentity {
    pub fn routing_metadata_is_valid(name: &str, aliases: &[String]) -> bool {
        let valid_name = |name: &str| !name.trim().is_empty() && name.len() <= 128;
        valid_name(name) && aliases.len() <= 32 && aliases.iter().all(|alias| valid_name(alias))
    }
    pub fn matches_selector(&self, selector: &str) -> bool {
        let selector = selector.trim();
        !selector.is_empty()
            && (self.name.trim().eq_ignore_ascii_case(selector)
                || self
                    .aliases
                    .iter()
                    .any(|alias| alias.trim().eq_ignore_ascii_case(selector)))
    }
}
