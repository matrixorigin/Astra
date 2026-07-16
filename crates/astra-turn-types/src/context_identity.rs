//! Content-addressed prompt-prefix and per-attempt artifact-evidence contracts.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const PROMPT_CACHE_IDENTITY_CONTRACT_VERSION: &str = "prompt-cache-identity-v1";
pub const LLM_ARTIFACT_EVIDENCE_CONTRACT_VERSION: &str = "llm-artifact-evidence-v1";
pub const LLM_ARTIFACT_EVIDENCE_MAX_ENTRIES: usize = 64;
const ARTIFACT_ID_MAX_BYTES: usize = 512;
const TOOL_NAME_MAX_BYTES: usize = 1_024;
const ARTIFACT_CURSOR_MAX_BYTES: usize = 2_048;
const ARTIFACT_MEDIA_TYPE_MAX_BYTES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmArtifactEvidenceEntryV1 {
    pub artifact_id: String,
    pub tool_name: String,
    pub content_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoded_bytes: Option<u64>,
}

impl LlmArtifactEvidenceEntryV1 {
    pub fn new(
        artifact_id: impl Into<String>,
        tool_name: impl Into<String>,
        content_hash: impl Into<String>,
    ) -> Result<Self, ContextIdentityError> {
        let entry = Self {
            artifact_id: artifact_id.into(),
            tool_name: tool_name.into(),
            content_hash: content_hash.into(),
            media_type: None,
            encoded_bytes: None,
        };
        entry.validate()?;
        Ok(entry)
    }

    fn validate(&self) -> Result<(), ContextIdentityError> {
        validate_field("artifact_id", &self.artifact_id, ARTIFACT_ID_MAX_BYTES)?;
        validate_field("tool_name", &self.tool_name, TOOL_NAME_MAX_BYTES)?;
        validate_hash(&self.content_hash)?;
        match self.media_type.as_deref() {
            Some(media_type) => {
                validate_field("media_type", media_type, ARTIFACT_MEDIA_TYPE_MAX_BYTES)
            }
            None => Ok(()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LlmArtifactEvidenceManifestV1 {
    pub contract_version: String,
    pub owner_id: String,
    pub session_id: String,
    pub as_of_cursor: String,
    pub observed_reference_count: usize,
    pub omitted_reference_count: usize,
    pub invalid_reference_count: usize,
    pub entries: Vec<LlmArtifactEvidenceEntryV1>,
    pub content_id: String,
}

impl LlmArtifactEvidenceManifestV1 {
    pub fn new(
        owner_id: impl Into<String>,
        session_id: impl Into<String>,
        as_of_cursor: impl Into<String>,
        mut entries: Vec<LlmArtifactEvidenceEntryV1>,
        observed_reference_count: usize,
        invalid_reference_count: usize,
    ) -> Result<Self, ContextIdentityError> {
        let owner_id = owner_id.into();
        let session_id = session_id.into();
        let as_of_cursor = as_of_cursor.into();
        validate_field("owner_id", &owner_id, ARTIFACT_ID_MAX_BYTES)?;
        validate_field("session_id", &session_id, ARTIFACT_ID_MAX_BYTES)?;
        validate_field("as_of_cursor", &as_of_cursor, ARTIFACT_CURSOR_MAX_BYTES)?;
        if entries.len() > LLM_ARTIFACT_EVIDENCE_MAX_ENTRIES {
            return Err(ContextIdentityError::TooManyArtifactEvidenceEntries {
                count: entries.len(),
            });
        }
        if observed_reference_count < entries.len().saturating_add(invalid_reference_count) {
            return Err(ContextIdentityError::InvalidArtifactEvidenceCounts);
        }
        for entry in &entries {
            entry.validate()?;
        }
        entries.sort_by(|left, right| left.artifact_id.cmp(&right.artifact_id));
        for pair in entries.windows(2) {
            if pair[0].artifact_id == pair[1].artifact_id {
                return Err(ContextIdentityError::DuplicateArtifactId {
                    artifact_id: pair[0].artifact_id.clone(),
                });
            }
        }
        let omitted_reference_count = observed_reference_count
            .saturating_sub(entries.len())
            .saturating_sub(invalid_reference_count);
        let content_id = llm_artifact_evidence_content_id(
            &owner_id,
            &session_id,
            &as_of_cursor,
            observed_reference_count,
            omitted_reference_count,
            invalid_reference_count,
            &entries,
        );
        Ok(Self {
            contract_version: LLM_ARTIFACT_EVIDENCE_CONTRACT_VERSION.to_string(),
            owner_id,
            session_id,
            as_of_cursor,
            observed_reference_count,
            omitted_reference_count,
            invalid_reference_count,
            entries,
            content_id,
        })
    }
}

impl<'de> Deserialize<'de> for LlmArtifactEvidenceManifestV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            contract_version: String,
            owner_id: String,
            session_id: String,
            as_of_cursor: String,
            observed_reference_count: usize,
            omitted_reference_count: usize,
            invalid_reference_count: usize,
            entries: Vec<LlmArtifactEvidenceEntryV1>,
            content_id: String,
        }
        let raw = Raw::deserialize(deserializer)?;
        if raw.contract_version != LLM_ARTIFACT_EVIDENCE_CONTRACT_VERSION {
            return Err(serde::de::Error::custom(
                "unsupported LLM artifact evidence version",
            ));
        }
        let rebuilt = Self::new(
            raw.owner_id,
            raw.session_id,
            raw.as_of_cursor,
            raw.entries,
            raw.observed_reference_count,
            raw.invalid_reference_count,
        )
        .map_err(serde::de::Error::custom)?;
        if rebuilt.content_id != raw.content_id
            || rebuilt.omitted_reference_count != raw.omitted_reference_count
        {
            return Err(serde::de::Error::custom(
                "LLM artifact evidence content mismatch",
            ));
        }
        Ok(rebuilt)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PromptCacheIdentityV1 {
    pub contract_version: String,
    pub stable_system_prefix_hash: String,
    pub stable_tool_prefix_hash: String,
    pub cache_layout: String,
    pub content_id: String,
}

impl PromptCacheIdentityV1 {
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
    pub fn total_input_tokens(self) -> u64 {
        self.fresh_input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_creation_tokens)
    }

    pub fn read_share_basis_points(self) -> u16 {
        let total = self.total_input_tokens();
        if total == 0 {
            return 0;
        }
        ((self.cache_read_tokens.saturating_mul(10_000) / total).min(10_000)) as u16
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
    #[error(
        "LLM artifact evidence contains {count} entries; maximum is {LLM_ARTIFACT_EVIDENCE_MAX_ENTRIES}"
    )]
    TooManyArtifactEvidenceEntries { count: usize },
    #[error("duplicate artifact id {artifact_id}")]
    DuplicateArtifactId { artifact_id: String },
    #[error("LLM artifact evidence counts are inconsistent")]
    InvalidArtifactEvidenceCounts,
    #[error("content hash is not a sha256 identifier")]
    InvalidContentHash,
}

fn llm_artifact_evidence_content_id(
    owner_id: &str,
    session_id: &str,
    as_of_cursor: &str,
    observed_reference_count: usize,
    omitted_reference_count: usize,
    invalid_reference_count: usize,
    entries: &[LlmArtifactEvidenceEntryV1],
) -> String {
    canonical_hash(&serde_json::json!({
        "contract_version": LLM_ARTIFACT_EVIDENCE_CONTRACT_VERSION,
        "owner_id": owner_id,
        "session_id": session_id,
        "as_of_cursor": as_of_cursor,
        "observed_reference_count": observed_reference_count,
        "omitted_reference_count": omitted_reference_count,
        "invalid_reference_count": invalid_reference_count,
        "entries": entries,
    }))
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

    fn entry(id: &str) -> LlmArtifactEvidenceEntryV1 {
        LlmArtifactEvidenceEntryV1::new(id, format!("tool-{id}"), format!("sha256:{:064x}", 1))
            .unwrap()
    }

    #[test]
    fn llm_artifact_evidence_is_order_independent_and_tamper_evident() {
        let left = LlmArtifactEvidenceManifestV1::new(
            "user",
            "session",
            "cursor-7",
            vec![entry("b"), entry("a")],
            2,
            0,
        )
        .unwrap();
        let right = LlmArtifactEvidenceManifestV1::new(
            "user",
            "session",
            "cursor-7",
            vec![entry("a"), entry("b")],
            2,
            0,
        )
        .unwrap();
        assert_eq!(left, right);
        let mut encoded = serde_json::to_value(left).unwrap();
        encoded["entries"][0]["content_hash"] = json!("forged");
        assert!(serde_json::from_value::<LlmArtifactEvidenceManifestV1>(encoded).is_err());
    }

    #[test]
    fn evidence_counts_make_partial_collection_explicit_and_tamper_evident() {
        let manifest = LlmArtifactEvidenceManifestV1::new(
            "user",
            "session",
            "cursor-9",
            vec![entry("a"), entry("b")],
            5,
            1,
        )
        .unwrap();
        assert_eq!(manifest.observed_reference_count, 5);
        assert_eq!(manifest.invalid_reference_count, 1);
        assert_eq!(manifest.omitted_reference_count, 2);
        let mut encoded = serde_json::to_value(manifest).unwrap();
        encoded["omitted_reference_count"] = json!(1);
        assert!(serde_json::from_value::<LlmArtifactEvidenceManifestV1>(encoded).is_err());
    }

    #[test]
    fn volatile_artifact_evidence_never_changes_prompt_prefix_identity() {
        let system = vec![json!({"type":"text","text":"stable contract"})];
        let tools = vec![json!({"name":"read_file","schema":{"type":"object"}})];
        let identity =
            PromptCacheIdentityV1::from_prefixes(&system, &tools, "explicit-v1").unwrap();
        let first_evidence = LlmArtifactEvidenceManifestV1::new(
            "user",
            "session",
            "cursor-1",
            vec![entry("a")],
            1,
            0,
        )
        .unwrap();
        let second_evidence = LlmArtifactEvidenceManifestV1::new(
            "user",
            "session",
            "cursor-2",
            vec![entry("b")],
            1,
            0,
        )
        .unwrap();
        assert_ne!(first_evidence.content_id, second_evidence.content_id);
        assert_eq!(
            identity,
            PromptCacheIdentityV1::from_prefixes(&system, &tools, "explicit-v1").unwrap()
        );
    }

    #[test]
    fn prompt_identity_reports_only_the_changed_contract_region() {
        let baseline = PromptCacheIdentityV1::from_prefixes(
            &[json!({"text":"system-a"})],
            &[json!({"name":"tool-a"})],
            "explicit-v1",
        )
        .unwrap();
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
    }

    #[test]
    fn normalized_usage_is_provider_neutral_and_saturating() {
        let usage = NormalizedPromptCacheUsage {
            fresh_input_tokens: 100,
            cache_read_tokens: 300,
            cache_creation_tokens: 100,
        };
        assert_eq!(usage.total_input_tokens(), 500);
        assert_eq!(usage.read_share_basis_points(), 6_000);
    }
}
