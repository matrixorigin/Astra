//! Versioned wire contract for durable edge-to-cloud synchronization.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

pub const SYNC_OUTBOX_SIGNATURE_HEADER: &str = "x-astra-sync-outbox-signature";
pub const SYNC_OUTBOX_ACK_SCHEMA_VERSION: u32 = 1;
pub const SESSION_STATE_MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const SESSION_STATE_MANIFEST_ID_MAX_BYTES: usize = 512;
pub const SESSION_STATE_MANIFEST_LOCATOR_MAX_BYTES: usize = 2_048;
pub const SESSION_STATE_MANIFEST_GAP_REASON_MAX_BYTES: usize = 1_024;

pub fn sync_outbox_request_signature(token: &str, body: &Value) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(token.as_bytes())
        .expect("HMAC-SHA256 accepts keys of any length");
    mac.update(astra_core::canonical_json_string(body).as_bytes());
    format!("sha256={}", hex_encode(&mac.finalize().into_bytes()))
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncOutboxIngestionStatus {
    Created,
    IdempotentReplay,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncOutboxAck {
    pub schema_version: u32,
    pub record_id: String,
    pub payload_hash: String,
    pub ingestion_status: SyncOutboxIngestionStatus,
}

impl SyncOutboxAck {
    pub fn new(
        record_id: impl Into<String>,
        payload_hash: impl Into<String>,
        ingestion_status: SyncOutboxIngestionStatus,
    ) -> Self {
        Self {
            schema_version: SYNC_OUTBOX_ACK_SCHEMA_VERSION,
            record_id: record_id.into(),
            payload_hash: payload_hash.into(),
            ingestion_status,
        }
    }

    pub fn confirms(&self, record_id: &str, payload_hash: &str) -> bool {
        self.schema_version == SYNC_OUTBOX_ACK_SCHEMA_VERSION
            && self.record_id == record_id
            && self.payload_hash == payload_hash
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStateDimension {
    Transcript,
    Checkpoint,
    Workspace,
    Task,
    Artifact,
    Invocation,
    Memory,
}

impl SessionStateDimension {
    pub const ALL: [Self; 7] = [
        Self::Transcript,
        Self::Checkpoint,
        Self::Workspace,
        Self::Task,
        Self::Artifact,
        Self::Invocation,
        Self::Memory,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SessionStateDimensionEvidence {
    Referenced {
        locator: String,
        content_hash: String,
    },
    Gap {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStateManifestEntry {
    pub dimension: SessionStateDimension,
    pub evidence: SessionStateDimensionEvidence,
}

impl SessionStateManifestEntry {
    pub fn referenced(
        dimension: SessionStateDimension,
        locator: impl Into<String>,
        bytes: &[u8],
    ) -> Self {
        Self {
            dimension,
            evidence: SessionStateDimensionEvidence::Referenced {
                locator: locator.into(),
                content_hash: format!("sha256:{:x}", Sha256::digest(bytes)),
            },
        }
    }

    pub fn gap(dimension: SessionStateDimension, reason: impl Into<String>) -> Self {
        Self {
            dimension,
            evidence: SessionStateDimensionEvidence::Gap {
                reason: reason.into(),
            },
        }
    }

    pub fn referenced_hash(
        dimension: SessionStateDimension,
        locator: impl Into<String>,
        content_hash: impl Into<String>,
    ) -> Result<Self, String> {
        let entry = Self {
            dimension,
            evidence: SessionStateDimensionEvidence::Referenced {
                locator: locator.into(),
                content_hash: content_hash.into(),
            },
        };
        match &entry.evidence {
            SessionStateDimensionEvidence::Referenced {
                locator,
                content_hash,
            } if !locator.trim().is_empty()
                && locator.len() <= SESSION_STATE_MANIFEST_LOCATOR_MAX_BYTES
                && content_hash.len() == 71
                && content_hash.starts_with("sha256:")
                && content_hash[7..]
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit()) =>
            {
                Ok(entry)
            }
            _ => Err("session state manifest reference is invalid".to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionStateManifestV1 {
    pub schema_version: u32,
    pub source_session_id: String,
    pub target_session_id: String,
    pub as_of_cursor: String,
    pub entries: Vec<SessionStateManifestEntry>,
    pub content_id: String,
}

impl SessionStateManifestV1 {
    pub fn new(
        source_session_id: impl Into<String>,
        target_session_id: impl Into<String>,
        as_of_cursor: impl Into<String>,
        mut entries: Vec<SessionStateManifestEntry>,
    ) -> Result<Self, String> {
        let source_session_id = source_session_id.into();
        let target_session_id = target_session_id.into();
        let as_of_cursor = as_of_cursor.into();
        for (field, value) in [
            ("source_session_id", source_session_id.as_str()),
            ("target_session_id", target_session_id.as_str()),
            ("as_of_cursor", as_of_cursor.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(format!("session state manifest {field} must not be empty"));
            }
            if value.len() > SESSION_STATE_MANIFEST_ID_MAX_BYTES {
                return Err(format!(
                    "session state manifest {field} exceeds {SESSION_STATE_MANIFEST_ID_MAX_BYTES} bytes"
                ));
            }
        }
        entries.sort_by_key(|entry| entry.dimension);
        if entries.len() != SessionStateDimension::ALL.len()
            || entries
                .iter()
                .map(|entry| entry.dimension)
                .ne(SessionStateDimension::ALL)
        {
            return Err(
                "session state manifest must contain every dimension exactly once".to_string(),
            );
        }
        for entry in &entries {
            match &entry.evidence {
                SessionStateDimensionEvidence::Referenced {
                    locator,
                    content_hash,
                } => {
                    if locator.trim().is_empty()
                        || locator.len() > SESSION_STATE_MANIFEST_LOCATOR_MAX_BYTES
                        || content_hash.len() != 71
                        || !content_hash.starts_with("sha256:")
                        || !content_hash[7..]
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit())
                    {
                        return Err(format!(
                            "session state manifest {:?} reference is invalid",
                            entry.dimension
                        ));
                    }
                }
                SessionStateDimensionEvidence::Gap { reason }
                    if reason.trim().is_empty()
                        || reason.len() > SESSION_STATE_MANIFEST_GAP_REASON_MAX_BYTES =>
                {
                    return Err(format!(
                        "session state manifest {:?} gap reason is invalid",
                        entry.dimension
                    ));
                }
                SessionStateDimensionEvidence::Gap { .. } => {}
            }
        }
        let content_id = session_state_manifest_content_id(
            &source_session_id,
            &target_session_id,
            &as_of_cursor,
            &entries,
        );
        Ok(Self {
            schema_version: SESSION_STATE_MANIFEST_SCHEMA_VERSION,
            source_session_id,
            target_session_id,
            as_of_cursor,
            entries,
            content_id,
        })
    }

    pub fn is_complete(&self) -> bool {
        self.entries.iter().all(|entry| {
            matches!(
                entry.evidence,
                SessionStateDimensionEvidence::Referenced { .. }
            )
        })
    }

    pub fn gaps(&self) -> impl Iterator<Item = &SessionStateManifestEntry> {
        self.entries
            .iter()
            .filter(|entry| matches!(entry.evidence, SessionStateDimensionEvidence::Gap { .. }))
    }
}

impl<'de> Deserialize<'de> for SessionStateManifestV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            schema_version: u32,
            source_session_id: String,
            target_session_id: String,
            as_of_cursor: String,
            entries: Vec<SessionStateManifestEntry>,
            content_id: String,
        }
        let raw = Raw::deserialize(deserializer)?;
        if raw.schema_version != SESSION_STATE_MANIFEST_SCHEMA_VERSION {
            return Err(serde::de::Error::custom(
                "unsupported session state manifest version",
            ));
        }
        let rebuilt = Self::new(
            raw.source_session_id,
            raw.target_session_id,
            raw.as_of_cursor,
            raw.entries,
        )
        .map_err(serde::de::Error::custom)?;
        if rebuilt.content_id != raw.content_id {
            return Err(serde::de::Error::custom(
                "session state manifest content id mismatch",
            ));
        }
        Ok(rebuilt)
    }
}

fn session_state_manifest_content_id(
    source_session_id: &str,
    target_session_id: &str,
    as_of_cursor: &str,
    entries: &[SessionStateManifestEntry],
) -> String {
    let value = serde_json::json!({
        "schema_version": SESSION_STATE_MANIFEST_SCHEMA_VERSION,
        "source_session_id": source_session_id,
        "target_session_id": target_session_id,
        "as_of_cursor": as_of_cursor,
        "entries": entries,
    });
    format!(
        "sha256:{:x}",
        Sha256::digest(astra_core::canonical_json_string(&value))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_signature_is_canonical_and_keyed() {
        let left = serde_json::json!({"b": 2, "a": 1});
        let right = serde_json::json!({"a": 1, "b": 2});
        assert_eq!(
            sync_outbox_request_signature("token-a", &left),
            sync_outbox_request_signature("token-a", &right)
        );
        assert_ne!(
            sync_outbox_request_signature("token-a", &left),
            sync_outbox_request_signature("token-b", &left)
        );
    }

    #[test]
    fn ack_requires_version_identity_and_payload_hash() {
        let ack = SyncOutboxAck::new("record-1", "sha256:abc", SyncOutboxIngestionStatus::Created);
        assert!(ack.confirms("record-1", "sha256:abc"));
        assert!(!ack.confirms("record-2", "sha256:abc"));
        assert!(!ack.confirms("record-1", "sha256:def"));
    }

    fn complete_entries() -> Vec<SessionStateManifestEntry> {
        SessionStateDimension::ALL
            .into_iter()
            .map(|dimension| {
                SessionStateManifestEntry::referenced(
                    dimension,
                    format!("state://{dimension:?}"),
                    format!("bytes-{dimension:?}").as_bytes(),
                )
            })
            .collect()
    }

    #[test]
    fn session_manifest_requires_every_dimension_and_detects_tampering() {
        assert!(SessionStateManifestV1::new("source", "target", "turn:7", vec![]).is_err());
        let manifest =
            SessionStateManifestV1::new("source", "target", "turn:7", complete_entries()).unwrap();
        assert!(manifest.is_complete());
        let mut encoded = serde_json::to_value(manifest).unwrap();
        encoded["as_of_cursor"] = serde_json::json!("turn:8");
        assert!(serde_json::from_value::<SessionStateManifestV1>(encoded).is_err());
    }

    #[test]
    fn explicit_gap_is_degraded_not_partial_success() {
        let mut entries = complete_entries();
        entries[3] = SessionStateManifestEntry::gap(
            SessionStateDimension::Task,
            "task snapshot unavailable",
        );
        let manifest = SessionStateManifestV1::new("source", "target", "turn:7", entries).unwrap();
        assert!(!manifest.is_complete());
        assert_eq!(manifest.gaps().count(), 1);
    }

    #[test]
    fn session_manifest_rejects_unbounded_reference_and_gap_fields() {
        let mut entries = complete_entries();
        entries[0] = SessionStateManifestEntry::referenced(
            SessionStateDimension::Transcript,
            "x".repeat(SESSION_STATE_MANIFEST_LOCATOR_MAX_BYTES + 1),
            b"transcript",
        );
        assert!(SessionStateManifestV1::new("source", "target", "turn:7", entries).is_err());

        let mut entries = complete_entries();
        entries[3] = SessionStateManifestEntry::gap(
            SessionStateDimension::Task,
            "x".repeat(SESSION_STATE_MANIFEST_GAP_REASON_MAX_BYTES + 1),
        );
        assert!(SessionStateManifestV1::new("source", "target", "turn:7", entries).is_err());
    }
}
