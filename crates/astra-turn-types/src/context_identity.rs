//! Content-addressed prompt-prefix and resource-manifest contracts.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const PROMPT_CACHE_IDENTITY_CONTRACT_VERSION: &str = "prompt-cache-identity-v1";
pub const RESOURCE_MANIFEST_CONTRACT_VERSION: &str = "resource-manifest-v1";
pub const RESOURCE_MANIFEST_MAX_ENTRIES: usize = 4_096;
pub const RESOURCE_MANIFEST_PAGE_MAX_ENTRIES: usize = 100;
pub const RESOURCE_MANIFEST_PROJECTION_MAX_BYTES: usize = 64 * 1024;
const RESOURCE_ID_MAX_BYTES: usize = 512;
const RESOURCE_LABEL_MAX_BYTES: usize = 1_024;
const RESOURCE_REFERENCE_MAX_BYTES: usize = 2_048;
const RESOURCE_MEDIA_TYPE_MAX_BYTES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceManifestEntryV1 {
    pub resource_id: String,
    pub label: String,
    pub durable_reference: String,
    pub revision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_len: Option<u64>,
}

impl ResourceManifestEntryV1 {
    pub fn new(
        resource_id: impl Into<String>,
        label: impl Into<String>,
        durable_reference: impl Into<String>,
        revision: impl Into<String>,
    ) -> Result<Self, ContextIdentityError> {
        let entry = Self {
            resource_id: resource_id.into(),
            label: label.into(),
            durable_reference: durable_reference.into(),
            revision: revision.into(),
            media_type: None,
            byte_len: None,
        };
        entry.validate()?;
        Ok(entry)
    }

    fn validate(&self) -> Result<(), ContextIdentityError> {
        validate_field("resource_id", &self.resource_id, RESOURCE_ID_MAX_BYTES)?;
        validate_field("label", &self.label, RESOURCE_LABEL_MAX_BYTES)?;
        validate_field(
            "durable_reference",
            &self.durable_reference,
            RESOURCE_REFERENCE_MAX_BYTES,
        )?;
        validate_field("revision", &self.revision, RESOURCE_REFERENCE_MAX_BYTES).and_then(|()| {
            match self.media_type.as_deref() {
                Some(media_type) => {
                    validate_field("media_type", media_type, RESOURCE_MEDIA_TYPE_MAX_BYTES)
                }
                None => Ok(()),
            }
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ResourceManifestV1 {
    pub contract_version: String,
    pub owner_id: String,
    pub session_id: String,
    pub as_of_cursor: String,
    pub entries: Vec<ResourceManifestEntryV1>,
    pub content_id: String,
}

impl ResourceManifestV1 {
    pub fn new(
        owner_id: impl Into<String>,
        session_id: impl Into<String>,
        as_of_cursor: impl Into<String>,
        mut entries: Vec<ResourceManifestEntryV1>,
    ) -> Result<Self, ContextIdentityError> {
        let owner_id = owner_id.into();
        let session_id = session_id.into();
        let as_of_cursor = as_of_cursor.into();
        validate_field("owner_id", &owner_id, RESOURCE_ID_MAX_BYTES)?;
        validate_field("session_id", &session_id, RESOURCE_ID_MAX_BYTES)?;
        validate_field("as_of_cursor", &as_of_cursor, RESOURCE_REFERENCE_MAX_BYTES)?;
        if entries.len() > RESOURCE_MANIFEST_MAX_ENTRIES {
            return Err(ContextIdentityError::TooManyResources {
                count: entries.len(),
            });
        }
        for entry in &entries {
            entry.validate()?;
        }
        entries.sort_by(|left, right| left.resource_id.cmp(&right.resource_id));
        for pair in entries.windows(2) {
            if pair[0].resource_id == pair[1].resource_id {
                return Err(ContextIdentityError::DuplicateResourceId {
                    resource_id: pair[0].resource_id.clone(),
                });
            }
        }
        let content_id =
            resource_manifest_content_id(&owner_id, &session_id, &as_of_cursor, &entries);
        Ok(Self {
            contract_version: RESOURCE_MANIFEST_CONTRACT_VERSION.to_string(),
            owner_id,
            session_id,
            as_of_cursor,
            entries,
            content_id,
        })
    }

    pub fn page(
        &self,
        after_resource_id: Option<&str>,
        limit: usize,
        query: Option<&str>,
    ) -> Result<ResourceManifestPageV1, ContextIdentityError> {
        if limit == 0 || limit > RESOURCE_MANIFEST_PAGE_MAX_ENTRIES {
            return Err(ContextIdentityError::InvalidPageLimit { limit });
        }
        let query = query.map(str::trim).filter(|query| !query.is_empty());
        if query.is_some_and(|query| query.len() > 256) {
            return Err(ContextIdentityError::SearchQueryTooLong);
        }
        let query = query.map(str::to_lowercase);
        let mut matching = self.entries.iter().filter(|entry| {
            after_resource_id.is_none_or(|cursor| entry.resource_id.as_str() > cursor)
                && query.as_ref().is_none_or(|query| {
                    entry.resource_id.to_lowercase().contains(query)
                        || entry.label.to_lowercase().contains(query)
                        || entry.durable_reference.to_lowercase().contains(query)
                })
        });
        let mut entries = matching
            .by_ref()
            .take(limit.saturating_add(1))
            .cloned()
            .collect::<Vec<_>>();
        let has_more = entries.len() > limit;
        entries.truncate(limit);
        let next_cursor = has_more
            .then(|| entries.last().map(|entry| entry.resource_id.clone()))
            .flatten();
        Ok(ResourceManifestPageV1 {
            manifest_content_id: self.content_id.clone(),
            entries,
            next_cursor,
        })
    }

    /// Produce a bounded volatile prompt projection. The full manifest remains
    /// addressable through `manifest_content_id` and deterministic paging.
    pub fn prompt_projection(
        &self,
        max_entries: usize,
        max_encoded_bytes: usize,
    ) -> Result<ResourceManifestProjectionV1, ContextIdentityError> {
        if max_entries > RESOURCE_MANIFEST_PAGE_MAX_ENTRIES
            || max_encoded_bytes > RESOURCE_MANIFEST_PROJECTION_MAX_BYTES
        {
            return Err(ContextIdentityError::ProjectionLimitExceeded);
        }
        let mut entries = Vec::new();
        for entry in self.entries.iter().take(max_entries) {
            let mut candidate = entries.clone();
            candidate.push(ResourceManifestProjectionEntryV1 {
                resource_id: entry.resource_id.clone(),
                label: entry.label.clone(),
                revision: entry.revision.clone(),
            });
            let candidate_projection = ResourceManifestProjectionV1 {
                manifest_content_id: self.content_id.clone(),
                as_of_cursor: self.as_of_cursor.clone(),
                total_entries: self.entries.len(),
                truncated: candidate.len() < self.entries.len(),
                entries: candidate.clone(),
            };
            let encoded = serde_json::to_vec(&candidate_projection)
                .expect("validated resource projection must serialize");
            if encoded.len() > max_encoded_bytes {
                break;
            }
            entries = candidate;
        }
        let projection = ResourceManifestProjectionV1 {
            manifest_content_id: self.content_id.clone(),
            as_of_cursor: self.as_of_cursor.clone(),
            total_entries: self.entries.len(),
            truncated: entries.len() < self.entries.len(),
            entries,
        };
        if serde_json::to_vec(&projection)
            .expect("validated resource projection must serialize")
            .len()
            > max_encoded_bytes
        {
            return Err(ContextIdentityError::ProjectionLimitExceeded);
        }
        Ok(projection)
    }
}

impl<'de> Deserialize<'de> for ResourceManifestV1 {
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
            entries: Vec<ResourceManifestEntryV1>,
            content_id: String,
        }
        let raw = Raw::deserialize(deserializer)?;
        if raw.contract_version != RESOURCE_MANIFEST_CONTRACT_VERSION {
            return Err(serde::de::Error::custom(
                "unsupported resource manifest version",
            ));
        }
        let rebuilt = Self::new(raw.owner_id, raw.session_id, raw.as_of_cursor, raw.entries)
            .map_err(serde::de::Error::custom)?;
        if rebuilt.content_id != raw.content_id {
            return Err(serde::de::Error::custom(
                "resource manifest content id mismatch",
            ));
        }
        Ok(rebuilt)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceManifestPageV1 {
    pub manifest_content_id: String,
    pub entries: Vec<ResourceManifestEntryV1>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceManifestProjectionEntryV1 {
    pub resource_id: String,
    pub label: String,
    pub revision: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceManifestProjectionV1 {
    pub manifest_content_id: String,
    pub as_of_cursor: String,
    pub total_entries: usize,
    pub truncated: bool,
    pub entries: Vec<ResourceManifestProjectionEntryV1>,
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
        "resource manifest contains {count} entries; maximum is {RESOURCE_MANIFEST_MAX_ENTRIES}"
    )]
    TooManyResources { count: usize },
    #[error("duplicate resource id {resource_id}")]
    DuplicateResourceId { resource_id: String },
    #[error("resource manifest page limit {limit} is invalid")]
    InvalidPageLimit { limit: usize },
    #[error("resource manifest search query exceeds 256 bytes")]
    SearchQueryTooLong,
    #[error("resource prompt projection limit exceeds the contract boundary")]
    ProjectionLimitExceeded,
    #[error("content hash is not a sha256 identifier")]
    InvalidContentHash,
}

fn resource_manifest_content_id(
    owner_id: &str,
    session_id: &str,
    as_of_cursor: &str,
    entries: &[ResourceManifestEntryV1],
) -> String {
    canonical_hash(&serde_json::json!({
        "contract_version": RESOURCE_MANIFEST_CONTRACT_VERSION,
        "owner_id": owner_id,
        "session_id": session_id,
        "as_of_cursor": as_of_cursor,
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

    fn entry(id: &str) -> ResourceManifestEntryV1 {
        ResourceManifestEntryV1::new(
            id,
            format!("Resource {id}"),
            format!("artifact://{id}"),
            "rev-1",
        )
        .unwrap()
    }

    #[test]
    fn resource_manifest_is_order_independent_and_tamper_evident() {
        let left =
            ResourceManifestV1::new("user", "session", "cursor-7", vec![entry("b"), entry("a")])
                .unwrap();
        let right =
            ResourceManifestV1::new("user", "session", "cursor-7", vec![entry("a"), entry("b")])
                .unwrap();
        assert_eq!(left, right);
        let mut encoded = serde_json::to_value(left).unwrap();
        encoded["entries"][0]["revision"] = json!("forged");
        assert!(serde_json::from_value::<ResourceManifestV1>(encoded).is_err());
    }

    #[test]
    fn paging_search_and_projection_are_deterministic_and_bounded() {
        let manifest = ResourceManifestV1::new(
            "user",
            "session",
            "cursor-9",
            vec![entry("c"), entry("a"), entry("b")],
        )
        .unwrap();
        let first = manifest.page(None, 2, None).unwrap();
        assert_eq!(first.entries[0].resource_id, "a");
        assert_eq!(first.next_cursor.as_deref(), Some("b"));
        let second = manifest
            .page(first.next_cursor.as_deref(), 2, None)
            .unwrap();
        assert_eq!(second.entries[0].resource_id, "c");
        let searched = manifest.page(None, 10, Some("RESOURCE B")).unwrap();
        assert_eq!(searched.entries[0].resource_id, "b");
        let projection = manifest.prompt_projection(2, 1_024).unwrap();
        assert_eq!(projection.entries.len(), 2);
        assert!(projection.truncated);
        assert!(serde_json::to_vec(&projection).unwrap().len() <= 1_024);
    }

    #[test]
    fn volatile_resource_manifest_never_changes_prompt_prefix_identity() {
        let system = vec![json!({"type":"text","text":"stable contract"})];
        let tools = vec![json!({"name":"read_file","schema":{"type":"object"}})];
        let identity =
            PromptCacheIdentityV1::from_prefixes(&system, &tools, "explicit-v1").unwrap();
        let first_resources =
            ResourceManifestV1::new("user", "session", "cursor-1", vec![entry("a")]).unwrap();
        let second_resources =
            ResourceManifestV1::new("user", "session", "cursor-2", vec![entry("b")]).unwrap();
        assert_ne!(first_resources.content_id, second_resources.content_id);
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
