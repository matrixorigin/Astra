//! Versioned wire contract for durable edge-to-cloud synchronization.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;

pub const SYNC_OUTBOX_SIGNATURE_HEADER: &str = "x-astra-sync-outbox-signature";
pub const SYNC_OUTBOX_ACK_SCHEMA_VERSION: u32 = 1;

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
}
