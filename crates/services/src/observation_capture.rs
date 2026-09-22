//! Shared identity-fencing primitives for durable observation writes.
//!
//! A caller owns the persisted identity and payload schema. This module owns
//! only deterministic hashing and the three outcomes a storage implementation
//! may expose after an owner-scoped identity readback.

use serde_json::Value;
use sha2::{Digest, Sha256};

const HASH_ALGORITHM_VERSION: &str = "sha256:v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservationPayloadDomain {
    AgentEvent,
    ContextManifest,
}

impl ObservationPayloadDomain {
    pub fn identity_kind(self) -> &'static str {
        match self {
            Self::AgentEvent => "agent_event",
            Self::ContextManifest => "context_manifest",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::AgentEvent => "astra.agent-event",
            Self::ContextManifest => "astra.context-manifest",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DurableCaptureOutcome {
    Inserted,
    Replayed,
    Collision {
        stored_payload_hash: String,
        attempted_payload_hash: String,
    },
}

/// Classify a row read back in the inserting transaction. Write markers must
/// be fresh for every database attempt, including retries after unknown commit.
pub fn classify_capture(
    stored_hash: &str,
    stored_write_id: &str,
    attempted_hash: &str,
    attempted_write_id: &str,
) -> DurableCaptureOutcome {
    if stored_hash != attempted_hash {
        DurableCaptureOutcome::Collision {
            stored_payload_hash: stored_hash.to_owned(),
            attempted_payload_hash: attempted_hash.to_owned(),
        }
    } else if stored_write_id == attempted_write_id {
        DurableCaptureOutcome::Inserted
    } else {
        DurableCaptureOutcome::Replayed
    }
}

/// A receipt contains identifiers and digests only, never observation content.
pub struct ObservationCollisionReceipt<'a> {
    pub user_id: &'a str,
    pub domain: ObservationPayloadDomain,
    pub identity_id: &'a str,
    pub session_id: &'a str,
    pub stored_payload_hash: &'a str,
    pub attempted_payload_hash: &'a str,
    pub source: &'a str,
}

/// Aggregate conflicts into one row per identity. Expiry is fixed at first
/// observation so continuous conflicts cannot extend retention indefinitely.
/// Preserve input order, including identities equivalent under database collation.
/// Each chunk has at most 128 receipts; all chunks belong to the
/// caller's transaction, which must be rolled back on error. Empty input does no SQL.
/// Schema-bounded identifiers and hashes keep valid receipt bytes bounded as well.
pub async fn record_observation_collisions(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    receipts: &[ObservationCollisionReceipt<'_>],
) -> Result<(), sqlx::Error> {
    for chunk in receipts.chunks(128) {
        // Reduction must not hide malformed fields in intermediate receipts.
        for receipt in chunk {
            for (value, limit) in [
                (receipt.user_id, 128),
                (receipt.identity_id, 128),
                (receipt.session_id, 128),
                (receipt.stored_payload_hash, 80),
                (receipt.attempted_payload_hash, 80),
                (receipt.source, 64),
            ] {
                if value.chars().take(limit + 1).count() > limit {
                    return Err(sqlx::Error::Protocol(
                        "collision receipt exceeds schema bounds".into(),
                    ));
                }
            }
        }
        // Fresh duplicate keys in a multi-row UPSERT are not safe on MatrixOne.
        // Let the database group using the target columns' types/collation;
        // Rust only uses the returned ordinals, never normalizes identities.
        let groups: Vec<(i64, i64, i64)> = if chunk.len() == 1 {
            vec![(0, 0, 1)]
        } else {
            let mut grouping = sqlx::QueryBuilder::<sqlx::MySql>::new(
                "SELECT MIN(ord), MAX(ord), COUNT(*) FROM (
                 SELECT user_id, identity_kind, identity_id, 0 AS ord
                 FROM observation_identity_collisions WHERE 1 = 0",
            );
            for (ordinal, receipt) in chunk.iter().enumerate() {
                grouping
                    .push(" UNION ALL SELECT ")
                    .push_bind(receipt.user_id)
                    .push(", ")
                    .push_bind(receipt.domain.identity_kind())
                    .push(", ")
                    .push_bind(receipt.identity_id)
                    .push(", ")
                    .push(ordinal);
            }
            grouping.push(
                ") AS incoming GROUP BY user_id, identity_kind, identity_id ORDER BY MIN(ord)",
            );
            grouping.build_query_as().fetch_all(&mut **tx).await?
        };
        let mut query = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "INSERT INTO observation_identity_collisions
         (user_id, identity_kind, identity_id, session_id, stored_payload_hash,
          attempted_payload_hash, source, collision_count, first_seen_at, last_seen_at, expires_at) ",
        );
        query.push_values(&groups, |mut row, &(first, last, count)| {
            let receipt = &chunk[first as usize];
            row.push_bind(receipt.user_id)
                .push_bind(receipt.domain.identity_kind())
                .push_bind(receipt.identity_id)
                .push_bind(receipt.session_id)
                .push_bind(receipt.stored_payload_hash)
                .push_bind(chunk[last as usize].attempted_payload_hash)
                .push_bind(receipt.source)
                .push_bind(count)
                .push("NOW(6)")
                .push("NOW(6)")
                .push("DATE_ADD(NOW(6), INTERVAL 7 DAY)");
        });
        query.push(
            " ON DUPLICATE KEY UPDATE
             attempted_payload_hash = VALUES(attempted_payload_hash),
             collision_count = collision_count + VALUES(collision_count),
             last_seen_at = NOW(6)",
        );
        query.build().execute(&mut **tx).await?;
    }
    Ok(())
}

/// Hash a JSON payload after recursively sorting object keys.
///
/// Arrays retain their order. Nulls and scalar values retain their JSON
/// meaning. Length framing keeps the domain, algorithm version, and payload
/// unambiguous if this preimage format grows in a later version.
pub fn canonical_observation_payload_hash(
    domain: ObservationPayloadDomain,
    payload: &Value,
) -> String {
    let canonical_payload = astra_core::canonical_json_string(payload);
    let mut hasher = Sha256::new();
    hash_frame(&mut hasher, HASH_ALGORITHM_VERSION.as_bytes());
    hash_frame(&mut hasher, domain.as_str().as_bytes());
    hash_frame(&mut hasher, canonical_payload.as_bytes());
    format!("{HASH_ALGORITHM_VERSION}:{:x}", hasher.finalize())
}

fn hash_frame(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classification_requires_both_matching_hash_and_attempt() {
        assert_eq!(
            classify_capture("a", "first", "a", "first"),
            DurableCaptureOutcome::Inserted
        );
        assert_eq!(
            classify_capture("a", "first", "a", "retry"),
            DurableCaptureOutcome::Replayed
        );
        // A matching write marker cannot mask a changed payload in one batch.
        assert!(matches!(
            classify_capture("a", "first", "b", "first"),
            DurableCaptureOutcome::Collision { .. }
        ));
        assert!(matches!(
            classify_capture("a", "first", "b", "retry"),
            DurableCaptureOutcome::Collision { .. }
        ));
    }

    #[test]
    fn object_key_order_does_not_change_hash() {
        let left = json!({"outer": {"b": 2, "a": 1}, "items": [null, true]});
        let right = json!({"items": [null, true], "outer": {"a": 1, "b": 2}});

        assert_eq!(
            canonical_observation_payload_hash(ObservationPayloadDomain::AgentEvent, &left),
            canonical_observation_payload_hash(ObservationPayloadDomain::AgentEvent, &right)
        );
    }

    #[test]
    fn array_order_and_nulls_remain_semantic() {
        let ordered = json!({"items": [null, 1, 2]});
        let reordered = json!({"items": [1, null, 2]});

        assert_ne!(
            canonical_observation_payload_hash(ObservationPayloadDomain::AgentEvent, &ordered),
            canonical_observation_payload_hash(ObservationPayloadDomain::AgentEvent, &reordered)
        );
    }

    #[test]
    fn domains_separate_identical_json() {
        let payload = json!({"id": "same", "value": 7});

        assert_ne!(
            canonical_observation_payload_hash(ObservationPayloadDomain::AgentEvent, &payload),
            canonical_observation_payload_hash(ObservationPayloadDomain::ContextManifest, &payload)
        );
    }

    #[test]
    fn hash_has_stable_versioned_shape() {
        let hash = canonical_observation_payload_hash(
            ObservationPayloadDomain::AgentEvent,
            &json!({"value": "test"}),
        );

        assert!(hash.starts_with("sha256:v1:"));
        assert_eq!(hash.len(), "sha256:v1:".len() + 64);
        assert!(
            hash["sha256:v1:".len()..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
    }
}
