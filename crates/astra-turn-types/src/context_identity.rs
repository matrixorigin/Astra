//! Content-addressed prompt-prefix identity and normalized provider cache usage.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const PROMPT_CACHE_IDENTITY_CONTRACT_VERSION: &str = "prompt-cache-identity-v1";
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PromptCacheIdentityV1 {
    pub contract_version: String,
    pub stable_system_prefix_hash: String,
    pub stable_tool_prefix_hash: String,
    pub cache_layout: String,
    pub content_id: String,
}

impl PromptCacheIdentityV1 {
    #[must_use]
    pub fn is_valid(&self) -> bool {
        Self::from_hashes(
            self.stable_system_prefix_hash.clone(),
            self.stable_tool_prefix_hash.clone(),
            self.cache_layout.clone(),
        )
        .is_ok_and(|rebuilt| rebuilt == *self)
    }

    pub fn from_prefixes(
        stable_system_prefix: &[Value],
        stable_tool_prefix: &[Value],
        cache_layout: impl Into<String>,
    ) -> Result<Self, ContextIdentityError> {
        let cache_layout = cache_layout.into();
        validate_field("cache_layout", &cache_layout, 128)?;
        let stable_system_prefix_hash = canonical_hash(&Value::Array(
            stable_system_prefix.iter().map(canonical_json).collect(),
        ));
        let stable_tool_prefix_hash = canonical_hash(&Value::Array(
            stable_tool_prefix.iter().map(canonical_json).collect(),
        ));
        Self::from_hashes(
            stable_system_prefix_hash,
            stable_tool_prefix_hash,
            cache_layout,
        )
    }

    pub fn from_hashes(
        stable_system_prefix_hash: impl Into<String>,
        stable_tool_prefix_hash: impl Into<String>,
        cache_layout: impl Into<String>,
    ) -> Result<Self, ContextIdentityError> {
        let stable_system_prefix_hash = stable_system_prefix_hash.into();
        let stable_tool_prefix_hash = stable_tool_prefix_hash.into();
        let cache_layout = cache_layout.into();
        validate_hash(&stable_system_prefix_hash)?;
        validate_hash(&stable_tool_prefix_hash)?;
        validate_field("cache_layout", &cache_layout, 128)?;
        let content_id = canonical_hash(&serde_json::json!({
            "contract_version": PROMPT_CACHE_IDENTITY_CONTRACT_VERSION,
            "stable_system_prefix_hash": stable_system_prefix_hash,
            "stable_tool_prefix_hash": stable_tool_prefix_hash,
            "cache_layout": cache_layout,
        }));
        Ok(Self {
            contract_version: PROMPT_CACHE_IDENTITY_CONTRACT_VERSION.to_string(),
            stable_system_prefix_hash,
            stable_tool_prefix_hash,
            cache_layout,
            content_id,
        })
    }

    pub fn invalidation_reasons(&self, current: &Self) -> Vec<PromptCacheInvalidationReason> {
        let mut reasons = Vec::new();
        if self.stable_system_prefix_hash != current.stable_system_prefix_hash {
            reasons.push(PromptCacheInvalidationReason::SystemPrefixChanged);
        }
        if self.stable_tool_prefix_hash != current.stable_tool_prefix_hash {
            reasons.push(PromptCacheInvalidationReason::ToolPrefixChanged);
        }
        if self.cache_layout != current.cache_layout {
            reasons.push(PromptCacheInvalidationReason::CacheLayoutChanged);
        }
        reasons
    }
}

impl<'de> Deserialize<'de> for PromptCacheIdentityV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            contract_version: String,
            stable_system_prefix_hash: String,
            stable_tool_prefix_hash: String,
            cache_layout: String,
            content_id: String,
        }
        let raw = Raw::deserialize(deserializer)?;
        if raw.contract_version != PROMPT_CACHE_IDENTITY_CONTRACT_VERSION {
            return Err(serde::de::Error::custom(
                "unsupported prompt cache identity version",
            ));
        }
        let rebuilt = Self::from_hashes(
            raw.stable_system_prefix_hash,
            raw.stable_tool_prefix_hash,
            raw.cache_layout,
        )
        .map_err(serde::de::Error::custom)?;
        if rebuilt.content_id != raw.content_id {
            return Err(serde::de::Error::custom(
                "prompt cache identity content id mismatch",
            ));
        }
        Ok(rebuilt)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheInvalidationReason {
    SystemPrefixChanged,
    ToolPrefixChanged,
    CacheLayoutChanged,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedPromptCacheUsage {
    pub fresh_input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

impl NormalizedPromptCacheUsage {
    pub const fn new(
        fresh_input_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
    ) -> Self {
        Self {
            fresh_input_tokens,
            cache_read_tokens,
            cache_creation_tokens,
        }
    }

    pub fn total_input_tokens(self) -> u64 {
        self.fresh_input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_creation_tokens)
    }

    pub fn checked_total_input_tokens(self) -> Option<u64> {
        self.fresh_input_tokens
            .checked_add(self.cache_read_tokens)?
            .checked_add(self.cache_creation_tokens)
    }

    pub fn total_tokens_with_output(self, output_tokens: u64) -> u64 {
        self.total_input_tokens().saturating_add(output_tokens)
    }

    pub fn checked_total_tokens_with_output(self, output_tokens: u64) -> Option<u64> {
        self.checked_total_input_tokens()?
            .checked_add(output_tokens)
    }

    pub fn read_share_basis_points(self) -> u16 {
        let total = u128::from(self.fresh_input_tokens)
            + u128::from(self.cache_read_tokens)
            + u128::from(self.cache_creation_tokens);
        if total == 0 {
            return 0;
        }
        ((u128::from(self.cache_read_tokens) * 10_000 / total).min(10_000)) as u16
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ContextIdentityError {
    #[error("{field} must not be empty")]
    EmptyField { field: &'static str },
    #[error("{field} exceeds {max_bytes} encoded bytes")]
    FieldTooLong {
        field: &'static str,
        max_bytes: usize,
    },
    #[error("content hash is not a sha256 identifier")]
    InvalidContentHash,
}

fn validate_field(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), ContextIdentityError> {
    if value.trim().is_empty() {
        return Err(ContextIdentityError::EmptyField { field });
    }
    if value.len() > max_bytes {
        return Err(ContextIdentityError::FieldTooLong { field, max_bytes });
    }
    Ok(())
}

fn validate_hash(hash: &str) -> Result<(), ContextIdentityError> {
    if hash.len() != 71
        || !hash.starts_with("sha256:")
        || !hash[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(ContextIdentityError::InvalidContentHash);
    }
    Ok(())
}

fn canonical_hash(value: &Value) -> String {
    let encoded = serde_json::to_vec(&canonical_json(value))
        .expect("canonical context identity input must serialize");
    format!("sha256:{:x}", Sha256::digest(encoded))
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(canonical_json).collect()),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), canonical_json(value)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        scalar => scalar.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prompt_identity_reports_only_the_changed_contract_region() {
        let baseline = PromptCacheIdentityV1::from_prefixes(
            &[json!({"text":"system-a"})],
            &[json!({"name":"tool-a"})],
            "explicit-v1",
        )
        .unwrap();
        assert!(baseline.is_valid());
        let tool_change = PromptCacheIdentityV1::from_prefixes(
            &[json!({"text":"system-a"})],
            &[json!({"name":"tool-b"})],
            "explicit-v1",
        )
        .unwrap();
        assert_eq!(
            baseline.invalidation_reasons(&tool_change),
            vec![PromptCacheInvalidationReason::ToolPrefixChanged]
        );
        let mut forged = serde_json::to_value(&baseline).unwrap();
        forged["content_id"] = json!(format!("sha256:{:064x}", 1));
        assert!(serde_json::from_value::<PromptCacheIdentityV1>(forged).is_err());
        let mut forged_struct = baseline;
        forged_struct.content_id = format!("sha256:{:064x}", 1);
        assert!(!forged_struct.is_valid());
    }

    #[test]
    fn normalized_usage_is_provider_neutral_and_saturating() {
        let usage = NormalizedPromptCacheUsage::new(100, 300, 100);
        assert_eq!(usage.total_input_tokens(), 500);
        assert_eq!(usage.total_tokens_with_output(20), 520);
        assert_eq!(usage.read_share_basis_points(), 6_000);

        let saturated = NormalizedPromptCacheUsage::new(u64::MAX, 1, 1);
        assert_eq!(saturated.total_input_tokens(), u64::MAX);
        assert_eq!(saturated.total_tokens_with_output(1), u64::MAX);
        assert_eq!(saturated.checked_total_input_tokens(), None);
        assert_eq!(saturated.checked_total_tokens_with_output(1), None);

        let all_cache_read = NormalizedPromptCacheUsage::new(0, u64::MAX, 0);
        assert_eq!(all_cache_read.read_share_basis_points(), 10_000);
    }
}
