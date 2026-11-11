//! Versioned wire contract for durable edge-to-cloud synchronization.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;

pub const SYNC_OUTBOX_SIGNATURE_HEADER: &str = "x-astra-sync-outbox-signature";
pub const SYNC_OUTBOX_BATCH_SCHEMA_VERSION: u32 = 1;
pub const MAX_SYNC_OUTBOX_BATCH_EVENTS: usize = 100;
pub const SYNC_OUTBOX_RECORD_CONFLICT_ERROR_CODE: &str = "sync_outbox_record_conflict";
pub const SYNC_OUTBOX_SESSION_UNAVAILABLE_ERROR_CODE: &str = "sync_outbox_session_unavailable";
pub const SYNC_OUTBOX_INVALID_RECORD_ERROR_CODE: &str = "sync_outbox_invalid_record";

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
            record_id: record_id.into(),
            payload_hash: payload_hash.into(),
            ingestion_status,
        }
    }

    /// The stored receipt matches the record we sent. Protocol version
    /// negotiation lives on the batch envelope, so a version anomaly can never
    /// be mistaken for a conflicting payload.
    pub fn matches(&self, record_id: &str, payload_hash: &str) -> bool {
        self.record_id == record_id && self.payload_hash == payload_hash
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncOutboxEventBatch {
    pub schema_version: u32,
    pub events: Vec<Value>,
}

impl SyncOutboxEventBatch {
    pub fn new(events: Vec<Value>) -> Self {
        Self {
            schema_version: SYNC_OUTBOX_BATCH_SCHEMA_VERSION,
            events,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncOutboxReconcileRequest {
    pub schema_version: u32,
    pub record_ids: Vec<String>,
}

impl SyncOutboxReconcileRequest {
    pub fn new(record_ids: Vec<String>) -> Self {
        Self {
            schema_version: SYNC_OUTBOX_BATCH_SCHEMA_VERSION,
            record_ids,
        }
    }
}

/// A permanent, record-scoped verdict. The edge must not retry this record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncOutboxRejection {
    pub record_id: String,
    pub error_code: String,
    pub detail: String,
}

impl SyncOutboxRejection {
    pub fn new(
        record_id: impl Into<String>,
        error_code: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            record_id: record_id.into(),
            error_code: error_code.into(),
            detail: detail.into(),
        }
    }
}

/// One batch verdict. `acks` covers accepted records, `rejections` covers
/// permanent per-record verdicts, and any record in neither list was never
/// processed and must be retried. Transport or request failures are HTTP
/// errors, never per-record statuses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncOutboxBatchResponse {
    pub schema_version: u32,
    pub acks: Vec<SyncOutboxAck>,
    /// Absent means "no permanent rejections"; reconcile responses never carry
    /// any, while a batch response always lists them explicitly.
    #[serde(default)]
    pub rejections: Vec<SyncOutboxRejection>,
}

impl SyncOutboxBatchResponse {
    pub fn new(acks: Vec<SyncOutboxAck>, rejections: Vec<SyncOutboxRejection>) -> Self {
        Self {
            schema_version: SYNC_OUTBOX_BATCH_SCHEMA_VERSION,
            acks,
            rejections,
        }
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
    fn ack_identity_and_payload_hash_must_match() {
        let ack = SyncOutboxAck::new("record-1", "sha256:abc", SyncOutboxIngestionStatus::Created);
        assert!(ack.matches("record-1", "sha256:abc"));
        assert!(!ack.matches("record-2", "sha256:abc"));
        assert!(!ack.matches("record-1", "sha256:def"));
    }
}
