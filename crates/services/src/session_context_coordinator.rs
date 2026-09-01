//! Canonical database-backed session coordination.
//!
//! The mutable record is intentionally small: one branch head, one writer
//! lease, and one turn reservation. Conversation payloads and manifest nodes
//! are immutable and durable before a head can reference them.

use std::{collections::HashSet, time::Duration};

use astra_core::{
    SharedPool, matrixone_statement_with_null_shape, push_matrixone_bound_string_set,
};
use astra_turn_types::{
    ActorContextV1, AuthorityEpochsV1, CANONICAL_TURN_DELTA_SCHEMA_VERSION, CanonicalDeltaModeV1,
    CanonicalTurnDeltaV1, ContextManifestNodeV1, ConversationSegmentV1, ConversationWriterLeaseV1,
    CoordinatorConflictOptionV1, CoordinatorMutationV1, HandoffRiskEvidenceV1,
    MANIFEST_DELTA_SCHEMA_VERSION, ManifestDeltaV1, SESSION_COORDINATION_SCHEMA_VERSION,
    SessionContextHeadV1, SessionCoordinationValidationError, SessionCursorV1,
    SessionForkManifestV1, SessionForkStateV1, SessionHandoffModeV1, SessionKeyV1,
    SharedManifestPrefixV1, TurnReservationV1, canonical_conversation_root,
    canonical_conversation_serialized_len,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{MySql, QueryBuilder, Row, Transaction};
use thiserror::Error;
use uuid::Uuid;

const MAX_IDEMPOTENCY_KEY_BYTES: usize = 512;
const COORDINATOR_STATE_SCHEMA_VERSION: u32 = 1;
const MAX_SEGMENT_BATCH: usize = 256;
const MAX_STAGED_SEGMENT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_STAGED_BATCH_BYTES: u64 = 32 * 1024 * 1024;
const MAX_LEASE_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_RESERVATION_TTL: Duration = Duration::from_secs(15 * 60);
const RECEIPT_HASH_DOMAIN: &[u8] = b"astra.session-coordinator-receipt.v1\0";
const TURN_DELTA_HASH_DOMAIN: &[u8] = b"astra.canonical-turn-delta.v1\0";

#[derive(Debug, Error)]
pub enum SessionContextCoordinatorError {
    #[error("session actor is not authorized for this owner")]
    Unauthorized,
    #[error("invalid coordinator request: {0}")]
    Invalid(String),
    #[error("writer authority was fenced by a newer epoch")]
    Fenced,
    #[error("writer lease or turn reservation expired")]
    Expired,
    #[error("idempotency key was reused for a different request")]
    IdempotencyMismatch,
    #[error("coordinator state requires repair: {0}")]
    NeedsRepair(String),
    #[error("observed manifest is not an ancestor of the current branch head")]
    DivergentManifest,
    #[error("one or more requested conversation segments do not exist for this owner")]
    SegmentNotFound,
    #[error("coordinator clock is outside the supported range")]
    Clock,
    #[error("coordinator database operation {operation} failed: {source}")]
    Database {
        operation: &'static str,
        #[source]
        source: sqlx::Error,
    },
    #[error("coordinator database JSON for {entity} failed: {source}")]
    DatabaseJson {
        entity: &'static str,
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireWriterOutcome {
    Acquired(ConversationWriterLeaseV1),
    AlreadyAcquired(ConversationWriterLeaseV1),
    Conflict {
        current_head: Option<SessionContextHeadV1>,
        active_lease_expires_at_unix_ms: Option<i64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReserveTurnOutcome {
    Reserved(TurnReservationV1),
    AlreadyReserved(TurnReservationV1),
    Conflict {
        current_head: Option<SessionContextHeadV1>,
    },
}

/// One atomically renewed writer/reservation pair.
///
/// A canonical turn is writable only while both authorities are live. Renewing
/// them in separate transactions creates a state in which the writer has been
/// extended but its turn reservation has not, so callers must not compose the
/// two narrower renewal operations when they own an active turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenewedTurnAuthority {
    pub writer_lease: ConversationWriterLeaseV1,
    pub turn_reservation: TurnReservationV1,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum WriterTransferConflictV1 {
    CursorChanged,
    SourceWriterChanged,
    ActiveTurn,
}

#[derive(Debug, Clone)]
pub struct WriterTransferRequestV1 {
    pub handoff_id: String,
    pub idempotency_key: String,
    pub key: SessionKeyV1,
    pub mode: SessionHandoffModeV1,
    pub source_lease: Option<ConversationWriterLeaseV1>,
    /// Optional command-level CAS fence for writer-only authority changes.
    pub expected_writer_epoch: Option<u64>,
    pub expected_cursor: Option<SessionCursorV1>,
    pub target_actor: ActorContextV1,
    pub risk: HandoffRiskEvidenceV1,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TransferWriterOutcome {
    Transferred(ConversationWriterLeaseV1),
    AlreadyTransferred(ConversationWriterLeaseV1),
    Conflict {
        reason: WriterTransferConflictV1,
        current_head: Option<SessionContextHeadV1>,
        active_lease_expires_at_unix_ms: Option<i64>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct MaterializedConversationV1 {
    pub head: SessionContextHeadV1,
    pub messages: Vec<Value>,
    pub logical_segment_count: u64,
    pub canonical_segment_bytes: u64,
}

#[async_trait]
pub trait SessionContextCoordinator: Send + Sync {
    async fn load_head(
        &self,
        key: &SessionKeyV1,
    ) -> Result<Option<SessionContextHeadV1>, SessionContextCoordinatorError>;

    async fn materialize(
        &self,
        head: &SessionContextHeadV1,
    ) -> Result<MaterializedConversationV1, SessionContextCoordinatorError>;

    /// Load only the manifest nodes after a verified ancestor. Segment
    /// payloads are fetched separately, so warm attach/handoff is
    /// O(changed manifests) rather than O(history bytes).
    async fn load_manifest_delta(
        &self,
        key: &SessionKeyV1,
        after_manifest_root: Option<&str>,
    ) -> Result<ManifestDeltaV1, SessionContextCoordinatorError>;

    /// Fetch an explicitly requested, bounded set of immutable payloads.
    /// Request order is preserved so clients can checkpoint resumable
    /// hydration without loading unrelated history.
    async fn load_segments(
        &self,
        key: &SessionKeyV1,
        segment_hashes: &[String],
    ) -> Result<Vec<ConversationSegmentV1>, SessionContextCoordinatorError>;

    /// Idempotently stage a bounded batch of owner-scoped immutable payloads.
    ///
    /// Staging never changes a branch head or writer authority. A separate
    /// canonical journal import must prove the ordered lineage before any
    /// staged payload becomes reachable from a head.
    async fn store_segments(
        &self,
        key: &SessionKeyV1,
        segments: &[ConversationSegmentV1],
    ) -> Result<(), SessionContextCoordinatorError>;

    async fn load_authority_epochs(
        &self,
        key: &SessionKeyV1,
    ) -> Result<Option<AuthorityEpochsV1>, SessionContextCoordinatorError>;

    /// Return the currently valid controller lease without mutating it.
    async fn load_active_writer(
        &self,
        key: &SessionKeyV1,
    ) -> Result<Option<ConversationWriterLeaseV1>, SessionContextCoordinatorError>;

    async fn load_fork_prefix(
        &self,
        key: &SessionKeyV1,
    ) -> Result<Option<SharedManifestPrefixV1>, SessionContextCoordinatorError>;

    /// Atomically activate a prepared copy-on-write child head and its durable
    /// fork record. No writer/run/tool authority is inherited.
    async fn activate_fork(
        &self,
        manifest: &SessionForkManifestV1,
    ) -> Result<SessionContextHeadV1, SessionContextCoordinatorError>;

    async fn acquire_writer(
        &self,
        key: &SessionKeyV1,
        expected_cursor: Option<&SessionCursorV1>,
        actor: &ActorContextV1,
        ttl: Duration,
        idempotency_key: &str,
    ) -> Result<AcquireWriterOutcome, SessionContextCoordinatorError>;

    async fn renew_writer(
        &self,
        lease: &ConversationWriterLeaseV1,
        ttl: Duration,
    ) -> Result<ConversationWriterLeaseV1, SessionContextCoordinatorError>;

    async fn release_writer(
        &self,
        lease: &ConversationWriterLeaseV1,
    ) -> Result<(), SessionContextCoordinatorError>;

    /// Atomically fence the previous controller and install the target
    /// controller. Graceful transfer requires the live source lease and a
    /// drained turn slot. Forced transfer requires a server-verified
    /// authorization identity and preserves explicit unresolved-risk facts.
    async fn transfer_writer(
        &self,
        request: &WriterTransferRequestV1,
        ttl: Duration,
    ) -> Result<TransferWriterOutcome, SessionContextCoordinatorError>;

    async fn reserve_turn(
        &self,
        lease: &ConversationWriterLeaseV1,
        expected_cursor: Option<&SessionCursorV1>,
        ttl: Duration,
        idempotency_key: &str,
    ) -> Result<ReserveTurnOutcome, SessionContextCoordinatorError>;

    async fn renew_turn_reservation(
        &self,
        reservation: &TurnReservationV1,
        ttl: Duration,
    ) -> Result<TurnReservationV1, SessionContextCoordinatorError>;

    /// Atomically renew the complete authority required to commit one turn.
    async fn renew_turn_authority(
        &self,
        lease: &ConversationWriterLeaseV1,
        reservation: &TurnReservationV1,
        ttl: Duration,
    ) -> Result<RenewedTurnAuthority, SessionContextCoordinatorError>;

    async fn commit_turn(
        &self,
        reservation: &TurnReservationV1,
        delta: CanonicalTurnDeltaV1,
        idempotency_key: &str,
    ) -> Result<CoordinatorMutationV1, SessionContextCoordinatorError>;

    async fn advance_authority_epochs(
        &self,
        key: &SessionKeyV1,
        epochs: AuthorityEpochsV1,
    ) -> Result<(), SessionContextCoordinatorError>;
}

#[derive(Clone)]
pub struct DatabaseSessionContextCoordinator {
    pool: SharedPool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionAuthorityEventV1 {
    pub event_id: String,
    pub operation_kind: String,
    pub outcome: String,
    pub writer_epoch: u64,
    pub actor_id: Option<String>,
    pub device_id: Option<String>,
    pub lease_id: Option<String>,
    pub reservation_id: Option<String>,
    pub expected_root: Option<String>,
    pub observed_root: Option<String>,
    pub authorization_epoch: u64,
    pub device_trust_epoch: u64,
    pub permission_epoch: u64,
    pub created_at: chrono::NaiveDateTime,
}

impl DatabaseSessionContextCoordinator {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    pub async fn list_authority_events(
        &self,
        key: &SessionKeyV1,
        limit: u32,
    ) -> Result<Vec<SessionAuthorityEventV1>, SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        let rows = sqlx::query(
            "SELECT event_id, operation_kind, outcome, writer_epoch, actor_id, device_id,
                    lease_id, reservation_id, expected_root, observed_root,
                    authorization_epoch, device_trust_epoch, permission_epoch, created_at
             FROM session_context_authority_events
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?
             ORDER BY created_at DESC, event_id DESC
             LIMIT ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .bind(i64::from(limit.clamp(1, 500)))
        .fetch_all(self.pool.get())
        .await
        .map_err(|source| database_error("list_authority_events", source))?;
        rows.into_iter()
            .map(|row| {
                let nonnegative = |column: &'static str| {
                    let value: i64 = row
                        .try_get(column)
                        .map_err(|source| database_error("decode_authority_event", source))?;
                    u64::try_from(value).map_err(|_| {
                        SessionContextCoordinatorError::NeedsRepair(format!(
                            "authority event column {column} is negative"
                        ))
                    })
                };
                Ok(SessionAuthorityEventV1 {
                    event_id: row
                        .try_get("event_id")
                        .map_err(|source| database_error("decode_authority_event", source))?,
                    operation_kind: row
                        .try_get("operation_kind")
                        .map_err(|source| database_error("decode_authority_event", source))?,
                    outcome: row
                        .try_get("outcome")
                        .map_err(|source| database_error("decode_authority_event", source))?,
                    writer_epoch: nonnegative("writer_epoch")?,
                    actor_id: row
                        .try_get("actor_id")
                        .map_err(|source| database_error("decode_authority_event", source))?,
                    device_id: row
                        .try_get("device_id")
                        .map_err(|source| database_error("decode_authority_event", source))?,
                    lease_id: row
                        .try_get("lease_id")
                        .map_err(|source| database_error("decode_authority_event", source))?,
                    reservation_id: row
                        .try_get("reservation_id")
                        .map_err(|source| database_error("decode_authority_event", source))?,
                    expected_root: row
                        .try_get("expected_root")
                        .map_err(|source| database_error("decode_authority_event", source))?,
                    observed_root: row
                        .try_get("observed_root")
                        .map_err(|source| database_error("decode_authority_event", source))?,
                    authorization_epoch: nonnegative("authorization_epoch")?,
                    device_trust_epoch: nonnegative("device_trust_epoch")?,
                    permission_epoch: nonnegative("permission_epoch")?,
                    created_at: row
                        .try_get("created_at")
                        .map_err(|source| database_error("decode_authority_event", source))?,
                })
            })
            .collect()
    }
}

#[async_trait]
impl SessionContextCoordinator for DatabaseSessionContextCoordinator {
    async fn load_head(
        &self,
        key: &SessionKeyV1,
    ) -> Result<Option<SessionContextHeadV1>, SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        let row = sqlx::query(
            "SELECT head_json FROM session_context_heads
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| database_error("load_head", source))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let head_json = row
            .try_get::<Option<String>, _>("head_json")
            .map_err(|source| database_error("decode_head", source))?;
        let head = head_json
            .as_deref()
            .map(|json| database_json("head", json))
            .transpose()?;
        if let Some(head) = &head {
            validate_head(head)?;
            if head.key != *key {
                return Err(SessionContextCoordinatorError::NeedsRepair(
                    "database head key mismatch".into(),
                ));
            }
        }
        Ok(head)
    }

    async fn materialize(
        &self,
        head: &SessionContextHeadV1,
    ) -> Result<MaterializedConversationV1, SessionContextCoordinatorError> {
        validate_head(head)?;
        let fork_base = self.load_fork_prefix(&head.key).await?;
        let mut rows = sqlx::query(
            "SELECT manifest_json FROM conversation_manifest_nodes
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?
               AND compaction_generation = ? AND reachable = 1",
        )
        .bind(&head.key.isolation_domain)
        .bind(&head.key.owner_user_id)
        .bind(&head.key.session_id)
        .bind(&head.key.branch_id)
        .bind(i64_from_u64(
            "head compaction generation",
            head.cursor.compaction_generation,
        )?)
        .fetch_all(self.pool.get())
        .await
        .map_err(|source| database_error("load_manifests", source))?;
        if let Some(prefix) = &fork_base
            && prefix.parent_cursor.compaction_generation == head.cursor.compaction_generation
        {
            rows.extend(
                sqlx::query(
                    "SELECT manifest_json FROM conversation_manifest_nodes
                     WHERE isolation_domain = ? AND owner_user_id = ?
                       AND session_id = ? AND branch_id = ?
                       AND compaction_generation = ? AND reachable = 1
                       AND conversation_seq <= ?",
                )
                .bind(&prefix.parent_key.isolation_domain)
                .bind(&prefix.parent_key.owner_user_id)
                .bind(&prefix.parent_key.session_id)
                .bind(&prefix.parent_key.branch_id)
                .bind(i64_from_u64(
                    "fork parent compaction generation",
                    prefix.parent_cursor.compaction_generation,
                )?)
                .bind(i64_from_u64(
                    "fork parent conversation sequence",
                    prefix.parent_cursor.conversation_seq,
                )?)
                .fetch_all(self.pool.get())
                .await
                .map_err(|source| database_error("load_fork_parent_manifests", source))?,
            );
        }
        let mut nodes = std::collections::HashMap::with_capacity(rows.len());
        for row in rows {
            let json = row
                .try_get::<String, _>("manifest_json")
                .map_err(|source| database_error("decode_manifest_row", source))?;
            let node: ContextManifestNodeV1 = database_json("manifest", &json)?;
            node.validate()
                .map_err(|error| SessionContextCoordinatorError::NeedsRepair(error.to_string()))?;
            if node.key != head.key
                && fork_base
                    .as_ref()
                    .is_none_or(|prefix| node.key != prefix.parent_key)
            {
                return Err(SessionContextCoordinatorError::NeedsRepair(
                    "database manifest owner or branch mismatch".into(),
                ));
            }
            nodes.insert(node.manifest_root.clone(), node);
        }

        let ordered_nodes = order_manifest_chain(head, nodes)?;
        let mut unique_hashes = HashSet::new();
        for node in &ordered_nodes {
            unique_hashes.extend(
                node.appended_segments
                    .iter()
                    .map(|segment| segment.segment_hash.clone()),
            );
        }
        let mut segment_map = self
            .load_database_segments(&head.key, unique_hashes.into_iter().collect())
            .await?;
        materialize_nodes(head, ordered_nodes, &mut segment_map)
    }

    async fn load_manifest_delta(
        &self,
        key: &SessionKeyV1,
        after_manifest_root: Option<&str>,
    ) -> Result<ManifestDeltaV1, SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        validate_optional_manifest_root(after_manifest_root)?;
        let head = self.load_head(key).await?;
        let Some(head) = head else {
            if after_manifest_root.is_some() {
                return Err(SessionContextCoordinatorError::DivergentManifest);
            }
            return Ok(ManifestDeltaV1 {
                schema_version: MANIFEST_DELTA_SCHEMA_VERSION,
                key: key.clone(),
                after_manifest_root: None,
                head: None,
                shared_prefix: None,
                missing_nodes: Vec::new(),
                missing_canonical_bytes: 0,
                missing_message_count: 0,
            });
        };
        if after_manifest_root == Some(head.latest_manifest_root.as_str()) {
            return manifest_delta(
                key.clone(),
                after_manifest_root.map(str::to_owned),
                Some(head),
                None,
                Vec::new(),
            );
        }

        let fork_base = self.load_fork_prefix(key).await?;
        let (after_sequence, lower_bound, chain_boundary, shared_prefix) = match after_manifest_root
        {
            Some(root) => {
                let sequence = if let Some(prefix) = &fork_base
                    && root == prefix.parent_manifest_root
                {
                    prefix.parent_cursor.conversation_seq
                } else {
                    let row = sqlx::query(
                        "SELECT conversation_seq FROM conversation_manifest_nodes
                         WHERE isolation_domain = ? AND owner_user_id = ?
                           AND session_id = ? AND branch_id = ?
                           AND manifest_root = ? AND reachable = 1",
                    )
                    .bind(&key.isolation_domain)
                    .bind(&key.owner_user_id)
                    .bind(&key.session_id)
                    .bind(&key.branch_id)
                    .bind(root)
                    .fetch_optional(self.pool.get())
                    .await
                    .map_err(|source| database_error("load_manifest_delta_base", source))?;
                    let Some(row) = row else {
                        return Err(SessionContextCoordinatorError::DivergentManifest);
                    };
                    database_u64(&row, "conversation_seq")?
                };
                if sequence >= head.cursor.conversation_seq {
                    return Err(SessionContextCoordinatorError::DivergentManifest);
                }
                (Some(sequence), sequence, Some(root.to_owned()), None)
            }
            None => match &fork_base {
                Some(prefix)
                    if prefix.parent_cursor.compaction_generation
                        == head.cursor.compaction_generation =>
                {
                    (
                        Some(prefix.parent_cursor.conversation_seq),
                        prefix.parent_cursor.conversation_seq,
                        Some(prefix.parent_manifest_root.clone()),
                        Some(prefix.clone()),
                    )
                }
                Some(_) => (None, 0, None, None),
                None => (None, 0, None, None),
            },
        };
        if chain_boundary.as_deref() == Some(head.latest_manifest_root.as_str()) {
            return manifest_delta(
                key.clone(),
                after_manifest_root.map(str::to_owned),
                Some(head),
                shared_prefix,
                Vec::new(),
            );
        }
        let rows = sqlx::query(
            "SELECT manifest_json FROM conversation_manifest_nodes
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?
               AND compaction_generation = ? AND reachable = 1
               AND conversation_seq > ? AND conversation_seq <= ?
             ORDER BY conversation_seq ASC",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .bind(i64_from_u64(
            "manifest delta compaction generation",
            head.cursor.compaction_generation,
        )?)
        .bind(i64_from_u64("manifest delta lower bound", lower_bound)?)
        .bind(i64_from_u64(
            "manifest delta head sequence",
            head.cursor.conversation_seq,
        )?)
        .fetch_all(self.pool.get())
        .await
        .map_err(|source| database_error("load_manifest_delta_suffix", source))?;
        let mut nodes = std::collections::HashMap::with_capacity(rows.len());
        for row in rows {
            let json = row
                .try_get::<String, _>("manifest_json")
                .map_err(|source| database_error("decode_manifest_delta", source))?;
            let node: ContextManifestNodeV1 = database_json("manifest_delta", &json)?;
            node.validate()
                .map_err(|error| SessionContextCoordinatorError::NeedsRepair(error.to_string()))?;
            if node.key != *key {
                return Err(SessionContextCoordinatorError::NeedsRepair(
                    "manifest delta owner or branch mismatch".into(),
                ));
            }
            nodes.insert(node.manifest_root.clone(), node);
        }
        let missing =
            order_manifest_suffix(&head, chain_boundary.as_deref(), after_sequence, nodes)?;
        manifest_delta(
            key.clone(),
            after_manifest_root.map(str::to_owned),
            Some(head),
            shared_prefix,
            missing,
        )
    }

    async fn load_segments(
        &self,
        key: &SessionKeyV1,
        segment_hashes: &[String],
    ) -> Result<Vec<ConversationSegmentV1>, SessionContextCoordinatorError> {
        validate_segment_batch(key, segment_hashes)?;
        let mut segments = self
            .load_database_segments(key, segment_hashes.to_vec())
            .await?;
        segment_hashes
            .iter()
            .map(|hash| {
                segments
                    .remove(hash)
                    .ok_or(SessionContextCoordinatorError::SegmentNotFound)
            })
            .collect()
    }

    async fn store_segments(
        &self,
        key: &SessionKeyV1,
        segments: &[ConversationSegmentV1],
    ) -> Result<(), SessionContextCoordinatorError> {
        validate_segment_upload(key, segments)?;
        self.persist_database_segments(key, segments).await
    }

    async fn load_authority_epochs(
        &self,
        key: &SessionKeyV1,
    ) -> Result<Option<AuthorityEpochsV1>, SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        let row = sqlx::query(
            "SELECT authorization_epoch, device_trust_epoch, permission_epoch
             FROM session_context_heads
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| database_error("load_authority_epochs", source))?;
        row.map(|row| {
            Ok(AuthorityEpochsV1 {
                authorization_epoch: database_u64(&row, "authorization_epoch")?,
                device_trust_epoch: database_u64(&row, "device_trust_epoch")?,
                permission_epoch: database_u64(&row, "permission_epoch")?,
            })
        })
        .transpose()
    }

    async fn load_active_writer(
        &self,
        key: &SessionKeyV1,
    ) -> Result<Option<ConversationWriterLeaseV1>, SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        let mut tx = self
            .pool
            .get()
            .begin()
            .await
            .map_err(|source| database_error("begin_load_active_writer", source))?;
        let now = database_now_ms(&mut tx).await?;
        let row = sqlx::query(
            "SELECT active_writer_json
             FROM session_context_heads
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("load_active_writer", source))?;
        let Some(row) = row else {
            tx.commit()
                .await
                .map_err(|source| database_error("commit_load_active_writer_empty", source))?;
            return Ok(None);
        };
        let lease = row
            .try_get::<Option<String>, _>("active_writer_json")
            .map_err(|source| database_error("decode_active_writer", source))?
            .as_deref()
            .map(|json| database_json::<ConversationWriterLeaseV1>("active_writer", json))
            .transpose()?
            .filter(|lease| lease.expires_at_unix_ms > now);
        if lease.as_ref().is_some_and(|lease| lease.key != *key) {
            return Err(SessionContextCoordinatorError::NeedsRepair(
                "active writer owner-scoped key mismatch".into(),
            ));
        }
        tx.commit()
            .await
            .map_err(|source| database_error("commit_load_active_writer", source))?;
        Ok(lease)
    }

    async fn load_fork_prefix(
        &self,
        key: &SessionKeyV1,
    ) -> Result<Option<SharedManifestPrefixV1>, SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        let row = sqlx::query(
            "SELECT fork_base_json FROM session_context_heads
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| database_error("load_fork_prefix", source))?;
        let prefix = row
            .map(|row| {
                row.try_get::<Option<String>, _>("fork_base_json")
                    .map_err(|source| database_error("decode_fork_prefix", source))?
                    .as_deref()
                    .map(|json| database_json::<SharedManifestPrefixV1>("fork_prefix", json))
                    .transpose()
            })
            .transpose()?
            .flatten();
        if let Some(prefix) = &prefix {
            prefix
                .validate_for_child(key)
                .map_err(|error| SessionContextCoordinatorError::NeedsRepair(error.to_string()))?;
        }
        Ok(prefix)
    }

    async fn activate_fork(
        &self,
        manifest: &SessionForkManifestV1,
    ) -> Result<SessionContextHeadV1, SessionContextCoordinatorError> {
        validate_prepared_fork(manifest)?;
        let mut tx = self
            .pool
            .get()
            .begin()
            .await
            .map_err(|source| database_error("begin_activate_fork", source))?;
        let row = sqlx::query(
            "SELECT manifest_json, state FROM session_forks
             WHERE isolation_domain = ? AND owner_user_id = ? AND fork_id = ?
             FOR UPDATE",
        )
        .bind(&manifest.child_key.isolation_domain)
        .bind(&manifest.child_key.owner_user_id)
        .bind(&manifest.fork_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("lock_prepared_fork", source))?
        .ok_or_else(|| {
            SessionContextCoordinatorError::Invalid("prepared fork record does not exist".into())
        })?;
        let stored_json = row
            .try_get::<String, _>("manifest_json")
            .map_err(|source| database_error("decode_prepared_fork", source))?;
        let stored: SessionForkManifestV1 = database_json("prepared_fork", &stored_json)?;
        let stored_state = row
            .try_get::<String, _>("state")
            .map_err(|source| database_error("decode_prepared_fork_state", source))?;
        if stored_state == "active" {
            stored
                .validate()
                .map_err(|error| SessionContextCoordinatorError::NeedsRepair(error.to_string()))?;
            let mut replay_basis = stored.clone();
            replay_basis.state = SessionForkStateV1::Prepared;
            replay_basis.activated_at_unix_ms = None;
            if replay_basis != *manifest {
                return Err(SessionContextCoordinatorError::Fenced);
            }
            let active_head = sqlx::query(
                "SELECT head_json FROM session_context_heads
                 WHERE isolation_domain = ? AND owner_user_id = ?
                   AND session_id = ? AND branch_id = ?",
            )
            .bind(&stored.child_key.isolation_domain)
            .bind(&stored.child_key.owner_user_id)
            .bind(&stored.child_key.session_id)
            .bind(&stored.child_key.branch_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|source| database_error("load_active_fork_head", source))?
            .try_get::<Option<String>, _>("head_json")
            .map_err(|source| database_error("decode_active_fork_head", source))?
            .ok_or_else(|| {
                SessionContextCoordinatorError::NeedsRepair(
                    "active fork has no canonical child head".into(),
                )
            })?;
            let head: SessionContextHeadV1 = database_json("active_fork_head", &active_head)?;
            validate_head(&head)?;
            if head.key != stored.child_key {
                return Err(SessionContextCoordinatorError::NeedsRepair(
                    "active fork child head escaped its owner scope".into(),
                ));
            }
            tx.commit()
                .await
                .map_err(|source| database_error("commit_active_fork_replay", source))?;
            return Ok(head);
        }
        if stored_state != "prepared" || stored != *manifest {
            return Err(SessionContextCoordinatorError::Fenced);
        }

        let parent_row = sqlx::query(
            "SELECT manifest_json FROM conversation_manifest_nodes
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?
               AND manifest_root = ? AND reachable = 1",
        )
        .bind(&manifest.parent_key.isolation_domain)
        .bind(&manifest.parent_key.owner_user_id)
        .bind(&manifest.parent_key.session_id)
        .bind(&manifest.parent_key.branch_id)
        .bind(&manifest.parent_head.latest_manifest_root)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("verify_fork_parent_manifest", source))?;
        let parent_matches = if let Some(parent_row) = parent_row {
            let parent_json = parent_row
                .try_get::<String, _>("manifest_json")
                .map_err(|source| database_error("decode_fork_parent_manifest", source))?;
            let parent: ContextManifestNodeV1 =
                database_json("fork_parent_manifest", &parent_json)?;
            parent.key == manifest.parent_key && parent.cursor() == manifest.parent_head.cursor
        } else {
            let row = sqlx::query(
                "SELECT fork_base_json FROM session_context_heads
                 WHERE isolation_domain = ? AND owner_user_id = ?
                   AND session_id = ? AND branch_id = ? FOR UPDATE",
            )
            .bind(&manifest.parent_key.isolation_domain)
            .bind(&manifest.parent_key.owner_user_id)
            .bind(&manifest.parent_key.session_id)
            .bind(&manifest.parent_key.branch_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|source| database_error("load_nested_fork_parent_prefix", source))?;
            let prefix_json = row
                .map(|row| {
                    row.try_get::<Option<String>, _>("fork_base_json")
                        .map_err(|source| database_error("decode_nested_fork_prefix", source))
                })
                .transpose()?
                .flatten();
            let prefix = prefix_json
                .map(|json| database_json::<SharedManifestPrefixV1>("nested_fork_prefix", &json))
                .transpose()?;
            prefix.is_some_and(|prefix| {
                prefix.parent_manifest_root == manifest.parent_head.latest_manifest_root
                    && cursor_projection_matches_head(
                        &prefix.parent_cursor,
                        &manifest.parent_head.cursor,
                    )
                    && prefix.total_canonical_bytes == manifest.parent_head.total_canonical_bytes
                    && prefix.total_message_count == manifest.parent_head.total_message_count
            })
        };
        if !parent_matches {
            return Err(SessionContextCoordinatorError::Invalid(
                "fork parent manifest is missing or does not match the prepared cursor".into(),
            ));
        }

        ensure_database_state(&mut tx, &manifest.child_key, AuthorityEpochsV1::default()).await?;
        let mut child_state = lock_database_state(&mut tx, &manifest.child_key).await?;
        if child_state.head.is_some()
            || child_state.active_writer.is_some()
            || child_state.active_reservation.is_some()
            || child_state.fork_base.is_some()
        {
            return Err(SessionContextCoordinatorError::Fenced);
        }
        let now = database_now_ms(&mut tx).await?;
        let mut active = manifest.clone();
        active.state = SessionForkStateV1::Active;
        active.activated_at_unix_ms = Some(now);
        active
            .validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        let child_head = fork_child_head(&active, child_state.writer_epoch);
        child_state.fork_base = Some(active.shared_prefix());
        child_state.head = Some(child_head.clone());
        update_database_state(&mut tx, &child_state).await?;
        sqlx::query(
            "UPDATE session_forks
             SET state = 'active', manifest_json = ?, activated_at_ms = ?, updated_at = NOW(6)
             WHERE isolation_domain = ? AND owner_user_id = ? AND fork_id = ?
               AND state = 'prepared'",
        )
        .bind(database_to_json("active_fork", &active)?)
        .bind(now)
        .bind(&manifest.child_key.isolation_domain)
        .bind(&manifest.child_key.owner_user_id)
        .bind(&manifest.fork_id)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("activate_fork_record", source))?;
        sqlx::query(
            "UPDATE conversation_manifest_pins
             SET pin_state = 'active', updated_at = NOW(6)
             WHERE isolation_domain = ? AND owner_user_id = ? AND pin_id = ?",
        )
        .bind(&manifest.parent_key.isolation_domain)
        .bind(&manifest.parent_key.owner_user_id)
        .bind(&manifest.fork_id)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("activate_fork_pin", source))?;
        insert_fork_event(&mut tx, &active, 1, "prepared", "active").await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_activate_fork", source))?;
        Ok(child_head)
    }

    async fn acquire_writer(
        &self,
        key: &SessionKeyV1,
        expected_cursor: Option<&SessionCursorV1>,
        actor: &ActorContextV1,
        ttl: Duration,
        idempotency_key: &str,
    ) -> Result<AcquireWriterOutcome, SessionContextCoordinatorError> {
        validate_ttl(ttl, MAX_LEASE_TTL)?;
        validate_idempotency_key(idempotency_key)?;
        actor
            .validate_for(key)
            .map_err(|_| SessionContextCoordinatorError::Unauthorized)?;
        validate_optional_cursor(key, expected_cursor)?;
        let mut tx = self
            .pool
            .get()
            .begin()
            .await
            .map_err(|source| database_error("begin_acquire_writer", source))?;
        let now = database_now_ms(&mut tx).await?;
        let expires_at = checked_expiry(now, ttl)?;
        ensure_database_state(&mut tx, key, actor.authority_epochs).await?;
        let mut state = lock_database_state(&mut tx, key).await?;
        let request_hash = lease_request_hash(key, expected_cursor, actor);
        if let Some(receipt) = load_database_receipt::<LeaseReceiptV1>(
            &mut tx,
            key,
            "acquire",
            idempotency_key,
            &request_hash,
        )
        .await?
        {
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "acquire_writer",
                    outcome: "idempotent_replay",
                    actor: Some(actor),
                    lease_id: Some(&receipt.lease.lease_id),
                    reservation_id: None,
                    expected_cursor,
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_acquire_replay", source))?;
            return Ok(AcquireWriterOutcome::AlreadyAcquired(receipt.lease));
        }
        if let Some(active) = state.active_writer.clone()
            && active.idempotency_key == idempotency_key
        {
            validate_lease_request(&active, key, &expected_cursor.cloned(), actor)?;
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "acquire_writer",
                    outcome: "idempotent_replay",
                    actor: Some(actor),
                    lease_id: Some(&active.lease_id),
                    reservation_id: None,
                    expected_cursor,
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_acquire_retry", source))?;
            return Ok(AcquireWriterOutcome::AlreadyAcquired(active.clone()));
        }
        if state.head.as_ref().map(|head| &head.cursor) != expected_cursor {
            let outcome = AcquireWriterOutcome::Conflict {
                current_head: state.head.clone(),
                active_lease_expires_at_unix_ms: state
                    .active_writer
                    .as_ref()
                    .filter(|lease| lease.expires_at_unix_ms > now)
                    .map(|lease| lease.expires_at_unix_ms),
            };
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "acquire_writer",
                    outcome: "cursor_conflict",
                    actor: Some(actor),
                    lease_id: None,
                    reservation_id: None,
                    expected_cursor,
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_acquire_cursor_conflict", source))?;
            return Ok(outcome);
        }
        if state
            .active_writer
            .as_ref()
            .is_some_and(|lease| lease.expires_at_unix_ms > now)
        {
            let outcome = AcquireWriterOutcome::Conflict {
                current_head: state.head.clone(),
                active_lease_expires_at_unix_ms: state
                    .active_writer
                    .as_ref()
                    .map(|lease| lease.expires_at_unix_ms),
            };
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "acquire_writer",
                    outcome: "writer_conflict",
                    actor: Some(actor),
                    lease_id: None,
                    reservation_id: None,
                    expected_cursor,
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_acquire_lease_conflict", source))?;
            return Ok(outcome);
        }
        if actor.authority_epochs != state.authority_epochs {
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "acquire_writer",
                    outcome: "stale_fenced",
                    actor: Some(actor),
                    lease_id: None,
                    reservation_id: None,
                    expected_cursor,
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_acquire_fenced_audit", source))?;
            return Err(SessionContextCoordinatorError::Fenced);
        }
        archive_database_state_receipts(&mut tx, &state).await?;
        state.active_reservation = None;
        state.writer_epoch = state.writer_epoch.checked_add(1).ok_or_else(|| {
            SessionContextCoordinatorError::NeedsRepair("writer epoch overflow".into())
        })?;
        let lease = ConversationWriterLeaseV1 {
            schema_version: SESSION_COORDINATION_SCHEMA_VERSION,
            key: key.clone(),
            lease_id: Uuid::new_v4().to_string(),
            writer_epoch: state.writer_epoch,
            actor: actor.clone(),
            expected_cursor: expected_cursor.cloned(),
            acquired_at_unix_ms: now,
            expires_at_unix_ms: expires_at,
            idempotency_key: idempotency_key.to_owned(),
        };
        state.active_writer = Some(lease.clone());
        update_database_state(&mut tx, &state).await?;
        record_database_authority_event(
            &mut tx,
            &state,
            AuthorityAuditFact {
                operation: "acquire_writer",
                outcome: "acquired",
                actor: Some(actor),
                lease_id: Some(&lease.lease_id),
                reservation_id: None,
                expected_cursor,
            },
        )
        .await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_acquire_writer", source))?;
        Ok(AcquireWriterOutcome::Acquired(lease))
    }

    async fn renew_writer(
        &self,
        lease: &ConversationWriterLeaseV1,
        ttl: Duration,
    ) -> Result<ConversationWriterLeaseV1, SessionContextCoordinatorError> {
        validate_ttl(ttl, MAX_LEASE_TTL)?;
        let mut tx = self
            .pool
            .get()
            .begin()
            .await
            .map_err(|source| database_error("begin_renew_writer", source))?;
        let now = database_now_ms(&mut tx).await?;
        let mut state = lock_database_state(&mut tx, &lease.key).await?;
        if let Err(error) = validate_active_lease(&state, lease, now) {
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "renew_writer",
                    outcome: authority_error_outcome(&error),
                    actor: Some(&lease.actor),
                    lease_id: Some(&lease.lease_id),
                    reservation_id: None,
                    expected_cursor: lease.expected_cursor.as_ref(),
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_renew_writer_audit", source))?;
            return Err(error);
        }
        let renewed = state.active_writer.as_mut().expect("validated lease");
        renewed.expires_at_unix_ms = checked_expiry(now, ttl)?;
        let renewed = renewed.clone();
        update_database_state(&mut tx, &state).await?;
        record_database_authority_event(
            &mut tx,
            &state,
            AuthorityAuditFact {
                operation: "renew_writer",
                outcome: "renewed",
                actor: Some(&lease.actor),
                lease_id: Some(&lease.lease_id),
                reservation_id: None,
                expected_cursor: lease.expected_cursor.as_ref(),
            },
        )
        .await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_renew_writer", source))?;
        Ok(renewed)
    }

    async fn renew_turn_authority(
        &self,
        lease: &ConversationWriterLeaseV1,
        reservation: &TurnReservationV1,
        ttl: Duration,
    ) -> Result<RenewedTurnAuthority, SessionContextCoordinatorError> {
        validate_ttl(ttl, MAX_LEASE_TTL)?;
        validate_ttl(ttl, MAX_RESERVATION_TTL)?;
        validate_reservation_request(reservation, lease, &lease.expected_cursor)?;
        let mut tx = self
            .pool
            .get()
            .begin()
            .await
            .map_err(|source| database_error("begin_renew_turn_authority", source))?;
        let now = database_now_ms(&mut tx).await?;
        let mut state = lock_database_state(&mut tx, &lease.key).await?;
        let validation = validate_active_lease(&state, lease, now)
            .and_then(|()| validate_active_reservation(&state, reservation, now));
        if let Err(error) = validation {
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "renew_turn_authority",
                    outcome: authority_error_outcome(&error),
                    actor: Some(&lease.actor),
                    lease_id: Some(&lease.lease_id),
                    reservation_id: Some(&reservation.reservation_id),
                    expected_cursor: reservation.expected_cursor.as_ref(),
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_renew_turn_authority_audit", source))?;
            return Err(error);
        }
        let expires_at_unix_ms = checked_expiry(now, ttl)?;
        let writer_lease = state.active_writer.as_mut().expect("validated lease");
        writer_lease.expires_at_unix_ms = expires_at_unix_ms;
        let writer_lease = writer_lease.clone();
        let turn_reservation = state
            .active_reservation
            .as_mut()
            .expect("validated reservation");
        turn_reservation.expires_at_unix_ms = expires_at_unix_ms;
        let turn_reservation = turn_reservation.clone();
        update_database_state(&mut tx, &state).await?;
        record_database_authority_event(
            &mut tx,
            &state,
            AuthorityAuditFact {
                operation: "renew_turn_authority",
                outcome: "renewed",
                actor: Some(&lease.actor),
                lease_id: Some(&lease.lease_id),
                reservation_id: Some(&reservation.reservation_id),
                expected_cursor: reservation.expected_cursor.as_ref(),
            },
        )
        .await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_renew_turn_authority", source))?;
        Ok(RenewedTurnAuthority {
            writer_lease,
            turn_reservation,
        })
    }

    async fn release_writer(
        &self,
        lease: &ConversationWriterLeaseV1,
    ) -> Result<(), SessionContextCoordinatorError> {
        let mut tx = self
            .pool
            .get()
            .begin()
            .await
            .map_err(|source| database_error("begin_release_writer", source))?;
        let mut state = lock_database_state(&mut tx, &lease.key).await?;
        let outcome = if state.active_writer.as_ref().is_some_and(|active| {
            active.lease_id == lease.lease_id && active.writer_epoch == lease.writer_epoch
        }) {
            archive_database_state_receipts(&mut tx, &state).await?;
            state.active_writer = None;
            state.active_reservation = None;
            update_database_state(&mut tx, &state).await?;
            "released"
        } else if state.writer_epoch > lease.writer_epoch {
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "release_writer",
                    outcome: "stale_fenced",
                    actor: Some(&lease.actor),
                    lease_id: Some(&lease.lease_id),
                    reservation_id: None,
                    expected_cursor: lease.expected_cursor.as_ref(),
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_release_fenced_audit", source))?;
            return Err(SessionContextCoordinatorError::Fenced);
        } else {
            "already_released"
        };
        record_database_authority_event(
            &mut tx,
            &state,
            AuthorityAuditFact {
                operation: "release_writer",
                outcome,
                actor: Some(&lease.actor),
                lease_id: Some(&lease.lease_id),
                reservation_id: None,
                expected_cursor: lease.expected_cursor.as_ref(),
            },
        )
        .await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_release_writer", source))?;
        Ok(())
    }

    async fn transfer_writer(
        &self,
        request: &WriterTransferRequestV1,
        ttl: Duration,
    ) -> Result<TransferWriterOutcome, SessionContextCoordinatorError> {
        validate_writer_transfer_request(request)?;
        validate_ttl(ttl, MAX_LEASE_TTL)?;
        let request_hash = writer_transfer_request_hash(request);
        let mut tx = self
            .pool
            .get()
            .begin()
            .await
            .map_err(|source| database_error("begin_transfer_writer", source))?;
        let now = database_now_ms(&mut tx).await?;
        let expires_at = checked_expiry(now, ttl)?;
        let mut state = lock_database_state(&mut tx, &request.key).await?;
        if let Some(receipt) = load_database_receipt::<WriterTransferReceiptV1>(
            &mut tx,
            &request.key,
            "transfer",
            &request.idempotency_key,
            &request_hash,
        )
        .await?
        {
            validate_writer_transfer_receipt(&receipt, &request_hash)?;
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "transfer_writer",
                    outcome: "idempotent_replay",
                    actor: Some(&request.target_actor),
                    lease_id: Some(&receipt.lease.lease_id),
                    reservation_id: None,
                    expected_cursor: request.expected_cursor.as_ref(),
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_transfer_replay", source))?;
            return Ok(TransferWriterOutcome::AlreadyTransferred(receipt.lease));
        }
        if state.head.as_ref().map(|head| &head.cursor) != request.expected_cursor.as_ref() {
            let outcome =
                writer_transfer_conflict(&state, WriterTransferConflictV1::CursorChanged, now);
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "transfer_writer",
                    outcome: "cursor_conflict",
                    actor: Some(&request.target_actor),
                    lease_id: None,
                    reservation_id: None,
                    expected_cursor: request.expected_cursor.as_ref(),
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_transfer_cursor_conflict", source))?;
            return Ok(outcome);
        }
        if request
            .expected_writer_epoch
            .is_some_and(|expected| expected != state.writer_epoch)
        {
            let outcome = writer_transfer_conflict(
                &state,
                WriterTransferConflictV1::SourceWriterChanged,
                now,
            );
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "transfer_writer",
                    outcome: "writer_epoch_conflict",
                    actor: Some(&request.target_actor),
                    lease_id: None,
                    reservation_id: None,
                    expected_cursor: request.expected_cursor.as_ref(),
                },
            )
            .await?;
            tx.commit().await.map_err(|source| {
                database_error("commit_transfer_writer_epoch_conflict", source)
            })?;
            return Ok(outcome);
        }
        if request.target_actor.authority_epochs != state.authority_epochs {
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "transfer_writer",
                    outcome: "stale_fenced",
                    actor: Some(&request.target_actor),
                    lease_id: None,
                    reservation_id: None,
                    expected_cursor: request.expected_cursor.as_ref(),
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_transfer_fenced", source))?;
            return Err(SessionContextCoordinatorError::Fenced);
        }
        if request.mode == SessionHandoffModeV1::Graceful {
            let source = request
                .source_lease
                .as_ref()
                .expect("validated graceful source lease");
            if validate_active_lease(&state, source, now).is_err() {
                let outcome = writer_transfer_conflict(
                    &state,
                    WriterTransferConflictV1::SourceWriterChanged,
                    now,
                );
                record_database_authority_event(
                    &mut tx,
                    &state,
                    AuthorityAuditFact {
                        operation: "transfer_writer",
                        outcome: "source_writer_conflict",
                        actor: Some(&request.target_actor),
                        lease_id: Some(&source.lease_id),
                        reservation_id: None,
                        expected_cursor: request.expected_cursor.as_ref(),
                    },
                )
                .await?;
                tx.commit()
                    .await
                    .map_err(|source| database_error("commit_transfer_source_conflict", source))?;
                return Ok(outcome);
            }
            if state
                .active_reservation
                .as_ref()
                .is_some_and(|reservation| reservation.expires_at_unix_ms > now)
            {
                let outcome =
                    writer_transfer_conflict(&state, WriterTransferConflictV1::ActiveTurn, now);
                record_database_authority_event(
                    &mut tx,
                    &state,
                    AuthorityAuditFact {
                        operation: "transfer_writer",
                        outcome: "active_turn",
                        actor: Some(&request.target_actor),
                        lease_id: Some(&source.lease_id),
                        reservation_id: state
                            .active_reservation
                            .as_ref()
                            .map(|reservation| reservation.reservation_id.as_str()),
                        expected_cursor: request.expected_cursor.as_ref(),
                    },
                )
                .await?;
                tx.commit()
                    .await
                    .map_err(|source| database_error("commit_transfer_active_turn", source))?;
                return Ok(outcome);
            }
        }

        archive_database_state_receipts(&mut tx, &state).await?;
        state.active_reservation = None;
        state.writer_epoch = state.writer_epoch.checked_add(1).ok_or_else(|| {
            SessionContextCoordinatorError::NeedsRepair("writer epoch overflow".into())
        })?;
        let lease = ConversationWriterLeaseV1 {
            schema_version: SESSION_COORDINATION_SCHEMA_VERSION,
            key: request.key.clone(),
            lease_id: Uuid::new_v4().to_string(),
            writer_epoch: state.writer_epoch,
            actor: request.target_actor.clone(),
            expected_cursor: request.expected_cursor.clone(),
            acquired_at_unix_ms: now,
            expires_at_unix_ms: expires_at,
            idempotency_key: request.idempotency_key.clone(),
        };
        let receipt = WriterTransferReceiptV1 {
            idempotency_key: request.idempotency_key.clone(),
            request_hash: request_hash.clone(),
            handoff_id: request.handoff_id.clone(),
            mode: request.mode,
            risk: request.risk.clone(),
            lease: lease.clone(),
        };
        state.active_writer = Some(lease.clone());
        state.last_transfer = Some(receipt.clone());
        update_database_state(&mut tx, &state).await?;
        store_database_receipt(
            &mut tx,
            &request.key,
            "transfer",
            &request.idempotency_key,
            &request_hash,
            &receipt,
        )
        .await?;
        record_database_authority_event(
            &mut tx,
            &state,
            AuthorityAuditFact {
                operation: "transfer_writer",
                outcome: match request.mode {
                    SessionHandoffModeV1::Graceful => "graceful_transferred",
                    SessionHandoffModeV1::Forced => "forced_transferred",
                },
                actor: Some(&request.target_actor),
                lease_id: Some(&lease.lease_id),
                reservation_id: None,
                expected_cursor: request.expected_cursor.as_ref(),
            },
        )
        .await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_transfer_writer", source))?;
        Ok(TransferWriterOutcome::Transferred(lease))
    }

    async fn reserve_turn(
        &self,
        lease: &ConversationWriterLeaseV1,
        expected_cursor: Option<&SessionCursorV1>,
        ttl: Duration,
        idempotency_key: &str,
    ) -> Result<ReserveTurnOutcome, SessionContextCoordinatorError> {
        validate_ttl(ttl, MAX_RESERVATION_TTL)?;
        validate_idempotency_key(idempotency_key)?;
        validate_optional_cursor(&lease.key, expected_cursor)?;
        let mut tx = self
            .pool
            .get()
            .begin()
            .await
            .map_err(|source| database_error("begin_reserve_turn", source))?;
        let now = database_now_ms(&mut tx).await?;
        let mut state = lock_database_state(&mut tx, &lease.key).await?;
        let request_hash = reservation_request_hash(lease, expected_cursor);
        if let Some(receipt) = load_database_receipt::<ReservationReceiptV1>(
            &mut tx,
            &lease.key,
            "reserve",
            idempotency_key,
            &request_hash,
        )
        .await?
        {
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "reserve_turn",
                    outcome: "idempotent_replay",
                    actor: Some(&lease.actor),
                    lease_id: Some(&lease.lease_id),
                    reservation_id: Some(&receipt.reservation.reservation_id),
                    expected_cursor,
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_reservation_replay", source))?;
            return Ok(ReserveTurnOutcome::AlreadyReserved(receipt.reservation));
        }
        if let Some(active) = state.active_reservation.clone()
            && active.idempotency_key == idempotency_key
        {
            validate_reservation_request(&active, lease, &expected_cursor.cloned())?;
            validate_active_lease(&state, lease, now)?;
            if fence_expired_reservation_authority(&mut state, lease, now) {
                // The preceding acquire_writer replay can have refreshed the
                // writer. Fence that refresh in this transaction when its
                // paired reservation has already expired.
                update_database_state(&mut tx, &state).await?;
                record_database_authority_event(
                    &mut tx,
                    &state,
                    AuthorityAuditFact {
                        operation: "reserve_turn",
                        outcome: "expired_authority_fenced",
                        actor: Some(&lease.actor),
                        lease_id: Some(&lease.lease_id),
                        reservation_id: Some(&active.reservation_id),
                        expected_cursor,
                    },
                )
                .await?;
                tx.commit()
                    .await
                    .map_err(|source| database_error("commit_reservation_expiry_fence", source))?;
                return Err(SessionContextCoordinatorError::Expired);
            }
            let expires_at = refreshed_live_expiry(
                now,
                ttl,
                active.expires_at_unix_ms,
                Some(lease.expires_at_unix_ms),
            )?;
            let refreshed = state
                .active_reservation
                .as_mut()
                .expect("matched active reservation");
            refreshed.expires_at_unix_ms = expires_at;
            let refreshed = refreshed.clone();
            update_database_state(&mut tx, &state).await?;
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "reserve_turn",
                    outcome: "idempotent_refreshed",
                    actor: Some(&lease.actor),
                    lease_id: Some(&lease.lease_id),
                    reservation_id: Some(&refreshed.reservation_id),
                    expected_cursor,
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_reservation_retry", source))?;
            return Ok(ReserveTurnOutcome::AlreadyReserved(refreshed));
        }
        if let Err(error) = validate_active_lease(&state, lease, now) {
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "reserve_turn",
                    outcome: authority_error_outcome(&error),
                    actor: Some(&lease.actor),
                    lease_id: Some(&lease.lease_id),
                    reservation_id: None,
                    expected_cursor,
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_reserve_turn_audit", source))?;
            return Err(error);
        }
        if state.head.as_ref().map(|head| &head.cursor) != expected_cursor {
            let outcome = ReserveTurnOutcome::Conflict {
                current_head: state.head.clone(),
            };
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "reserve_turn",
                    outcome: "cursor_conflict",
                    actor: Some(&lease.actor),
                    lease_id: Some(&lease.lease_id),
                    reservation_id: None,
                    expected_cursor,
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_reservation_conflict", source))?;
            return Ok(outcome);
        }
        if state
            .active_reservation
            .as_ref()
            .is_some_and(|reservation| reservation.expires_at_unix_ms > now)
        {
            let outcome = ReserveTurnOutcome::Conflict {
                current_head: state.head.clone(),
            };
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "reserve_turn",
                    outcome: "reservation_conflict",
                    actor: Some(&lease.actor),
                    lease_id: Some(&lease.lease_id),
                    reservation_id: None,
                    expected_cursor,
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_active_reservation_conflict", source))?;
            return Ok(outcome);
        }
        if let Some(previous) = &state.active_reservation {
            archive_database_reservation(&mut tx, previous).await?;
        }
        let reservation = TurnReservationV1 {
            schema_version: SESSION_COORDINATION_SCHEMA_VERSION,
            reservation_id: Uuid::new_v4().to_string(),
            key: lease.key.clone(),
            lease_id: lease.lease_id.clone(),
            writer_epoch: lease.writer_epoch,
            expected_cursor: expected_cursor.cloned(),
            reserved_turn: expected_cursor
                .map_or(1, |cursor| cursor.completed_turn.saturating_add(1)),
            created_at_unix_ms: now,
            expires_at_unix_ms: checked_expiry(now, ttl)?.min(lease.expires_at_unix_ms),
            idempotency_key: idempotency_key.to_owned(),
        };
        state.active_reservation = Some(reservation.clone());
        update_database_state(&mut tx, &state).await?;
        record_database_authority_event(
            &mut tx,
            &state,
            AuthorityAuditFact {
                operation: "reserve_turn",
                outcome: "reserved",
                actor: Some(&lease.actor),
                lease_id: Some(&lease.lease_id),
                reservation_id: Some(&reservation.reservation_id),
                expected_cursor,
            },
        )
        .await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_reserve_turn", source))?;
        Ok(ReserveTurnOutcome::Reserved(reservation))
    }

    async fn commit_turn(
        &self,
        reservation: &TurnReservationV1,
        delta: CanonicalTurnDeltaV1,
        idempotency_key: &str,
    ) -> Result<CoordinatorMutationV1, SessionContextCoordinatorError> {
        delta
            .validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        validate_idempotency_key(idempotency_key)?;
        let request_hash = commit_request_hash(reservation, &delta);
        if let Some(receipt) = load_database_receipt_pool::<CommitReceiptV1>(
            self.pool.get(),
            &reservation.key,
            "commit",
            idempotency_key,
            &request_hash,
        )
        .await?
        {
            record_database_authority_event_pool(
                self.pool.get(),
                &reservation.key,
                AuthorityAuditFact {
                    operation: "commit_turn",
                    outcome: "idempotent_replay",
                    actor: None,
                    lease_id: Some(&reservation.lease_id),
                    reservation_id: Some(&reservation.reservation_id),
                    expected_cursor: reservation.expected_cursor.as_ref(),
                },
            )
            .await?;
            return Ok(CoordinatorMutationV1::AlreadyApplied {
                cursor: receipt.cursor,
            });
        }

        let base_head = self.load_head(&reservation.key).await?;
        if base_head.as_ref().map(|head| &head.cursor) != reservation.expected_cursor.as_ref() {
            record_database_authority_event_pool(
                self.pool.get(),
                &reservation.key,
                AuthorityAuditFact {
                    operation: "commit_turn",
                    outcome: "cursor_conflict",
                    actor: None,
                    lease_id: Some(&reservation.lease_id),
                    reservation_id: Some(&reservation.reservation_id),
                    expected_cursor: reservation.expected_cursor.as_ref(),
                },
            )
            .await?;
            return Ok(CoordinatorMutationV1::Conflict {
                current_cursor: base_head.map(|head| head.cursor),
                safe_options: vec![
                    CoordinatorConflictOptionV1::Refresh,
                    CoordinatorConflictOptionV1::Fork,
                ],
            });
        }
        validate_delta_advance(base_head.as_ref(), reservation, &delta)?;
        let mut segments = Vec::with_capacity(delta.logical_segments.len());
        for messages in delta.logical_segments.iter().cloned() {
            segments.push(
                ConversationSegmentV1::new(&reservation.key, messages)
                    .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?,
            );
        }
        let node = manifest_node_for_delta(&reservation.key, base_head.as_ref(), &delta, &segments)
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        let (prepared_total_canonical_bytes, prepared_total_message_count) =
            next_head_totals(base_head.as_ref(), &segments, delta.mode)?;
        self.persist_database_immutables(
            &reservation.key,
            &segments,
            &node,
            prepared_total_canonical_bytes,
            prepared_total_message_count,
        )
        .await?;

        let mut tx = self
            .pool
            .get()
            .begin()
            .await
            .map_err(|source| database_error("begin_commit_turn", source))?;
        let now = database_now_ms(&mut tx).await?;
        let mut state = lock_database_state(&mut tx, &reservation.key).await?;
        if let Some(last) = state.last_commit.clone()
            && last.idempotency_key == idempotency_key
        {
            validate_commit_request(&last, reservation, &delta)?;
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "commit_turn",
                    outcome: "idempotent_replay",
                    actor: None,
                    lease_id: Some(&reservation.lease_id),
                    reservation_id: Some(&reservation.reservation_id),
                    expected_cursor: reservation.expected_cursor.as_ref(),
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_turn_retry", source))?;
            return Ok(CoordinatorMutationV1::AlreadyApplied {
                cursor: last.cursor.clone(),
            });
        }
        if let Some(receipt) = load_database_receipt::<CommitReceiptV1>(
            &mut tx,
            &reservation.key,
            "commit",
            idempotency_key,
            &request_hash,
        )
        .await?
        {
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "commit_turn",
                    outcome: "idempotent_replay",
                    actor: None,
                    lease_id: Some(&reservation.lease_id),
                    reservation_id: Some(&reservation.reservation_id),
                    expected_cursor: reservation.expected_cursor.as_ref(),
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_turn_replay", source))?;
            return Ok(CoordinatorMutationV1::AlreadyApplied {
                cursor: receipt.cursor,
            });
        }
        if let Err(error) = validate_active_reservation(&state, reservation, now) {
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "commit_turn",
                    outcome: authority_error_outcome(&error),
                    actor: None,
                    lease_id: Some(&reservation.lease_id),
                    reservation_id: Some(&reservation.reservation_id),
                    expected_cursor: reservation.expected_cursor.as_ref(),
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_turn_rejection_audit", source))?;
            return Err(error);
        }
        if state.head.as_ref().map(|head| &head.cursor) != reservation.expected_cursor.as_ref() {
            let current_cursor = state.head.as_ref().map(|head| head.cursor.clone());
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "commit_turn",
                    outcome: "cursor_conflict",
                    actor: None,
                    lease_id: Some(&reservation.lease_id),
                    reservation_id: Some(&reservation.reservation_id),
                    expected_cursor: reservation.expected_cursor.as_ref(),
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_turn_conflict", source))?;
            return Ok(CoordinatorMutationV1::Conflict {
                current_cursor,
                safe_options: vec![
                    CoordinatorConflictOptionV1::Refresh,
                    CoordinatorConflictOptionV1::Fork,
                ],
            });
        }
        validate_delta_advance(state.head.as_ref(), reservation, &delta)?;
        if node.parent_manifest_root
            != state
                .head
                .as_ref()
                .map(|head| head.latest_manifest_root.clone())
        {
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "commit_turn",
                    outcome: "stale_fenced",
                    actor: None,
                    lease_id: Some(&reservation.lease_id),
                    reservation_id: Some(&reservation.reservation_id),
                    expected_cursor: reservation.expected_cursor.as_ref(),
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_manifest_fenced_audit", source))?;
            return Err(SessionContextCoordinatorError::Fenced);
        }
        if let Some(previous) = &state.last_commit {
            archive_database_commit(&mut tx, previous).await?;
        }
        let cursor = node.cursor();
        let (total_canonical_bytes, total_message_count) =
            next_head_totals(state.head.as_ref(), &segments, delta.mode)?;
        if (total_canonical_bytes, total_message_count)
            != (prepared_total_canonical_bytes, prepared_total_message_count)
        {
            return Err(SessionContextCoordinatorError::NeedsRepair(
                "prepared manifest totals do not match the fenced parent head".into(),
            ));
        }
        state.head = Some(SessionContextHeadV1 {
            schema_version: SESSION_COORDINATION_SCHEMA_VERSION,
            key: reservation.key.clone(),
            cursor: cursor.clone(),
            latest_manifest_root: node.manifest_root,
            total_canonical_bytes,
            total_message_count,
            writer_epoch: reservation.writer_epoch,
        });
        state.last_commit = Some(CommitReceiptV1 {
            idempotency_key: idempotency_key.to_owned(),
            reservation_id: reservation.reservation_id.clone(),
            reservation: reservation.clone(),
            delta_hash: turn_delta_hash(&delta),
            cursor: cursor.clone(),
        });
        state.active_reservation = None;
        update_database_state(&mut tx, &state).await?;
        let reachable = sqlx::query(
            "UPDATE conversation_manifest_nodes
             SET reachable = 1
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ? AND manifest_root = ?
               AND compaction_generation = ?
               AND total_canonical_bytes = ? AND total_message_count = ?",
        )
        .bind(&reservation.key.isolation_domain)
        .bind(&reservation.key.owner_user_id)
        .bind(&reservation.key.session_id)
        .bind(&reservation.key.branch_id)
        .bind(&cursor.canonical_root_hash)
        .bind(i64_from_u64(
            "reachable compaction generation",
            cursor.compaction_generation,
        )?)
        .bind(i64_from_u64(
            "reachable total canonical bytes",
            total_canonical_bytes,
        )?)
        .bind(i64_from_u64(
            "reachable total message count",
            total_message_count,
        )?)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("activate_reachable_manifest", source))?;
        if reachable.rows_affected() != 1 {
            return Err(SessionContextCoordinatorError::NeedsRepair(
                "canonical manifest was not durably activated".into(),
            ));
        }
        record_database_authority_event(
            &mut tx,
            &state,
            AuthorityAuditFact {
                operation: "commit_turn",
                outcome: "committed",
                actor: None,
                lease_id: Some(&reservation.lease_id),
                reservation_id: Some(&reservation.reservation_id),
                expected_cursor: reservation.expected_cursor.as_ref(),
            },
        )
        .await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_turn", source))?;
        Ok(CoordinatorMutationV1::Applied { cursor })
    }

    async fn renew_turn_reservation(
        &self,
        reservation: &TurnReservationV1,
        ttl: Duration,
    ) -> Result<TurnReservationV1, SessionContextCoordinatorError> {
        validate_ttl(ttl, MAX_RESERVATION_TTL)?;
        let mut tx = self
            .pool
            .get()
            .begin()
            .await
            .map_err(|source| database_error("begin_renew_turn_reservation", source))?;
        let now = database_now_ms(&mut tx).await?;
        let mut state = lock_database_state(&mut tx, &reservation.key).await?;
        if let Err(error) = validate_active_reservation(&state, reservation, now) {
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "renew_turn",
                    outcome: authority_error_outcome(&error),
                    actor: None,
                    lease_id: Some(&reservation.lease_id),
                    reservation_id: Some(&reservation.reservation_id),
                    expected_cursor: reservation.expected_cursor.as_ref(),
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_renew_turn_audit", source))?;
            return Err(error);
        }
        let lease_expiry = state
            .active_writer
            .as_ref()
            .expect("validated reservation lease")
            .expires_at_unix_ms;
        let renewed = state
            .active_reservation
            .as_mut()
            .expect("validated reservation");
        renewed.expires_at_unix_ms = checked_expiry(now, ttl)?.min(lease_expiry);
        let renewed = renewed.clone();
        update_database_state(&mut tx, &state).await?;
        record_database_authority_event(
            &mut tx,
            &state,
            AuthorityAuditFact {
                operation: "renew_turn",
                outcome: "renewed",
                actor: None,
                lease_id: Some(&reservation.lease_id),
                reservation_id: Some(&reservation.reservation_id),
                expected_cursor: reservation.expected_cursor.as_ref(),
            },
        )
        .await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_renew_turn_reservation", source))?;
        Ok(renewed)
    }

    async fn advance_authority_epochs(
        &self,
        key: &SessionKeyV1,
        epochs: AuthorityEpochsV1,
    ) -> Result<(), SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        let mut tx = self
            .pool
            .get()
            .begin()
            .await
            .map_err(|source| database_error("begin_advance_authority", source))?;
        ensure_database_state(&mut tx, key, epochs).await?;
        let mut state = lock_database_state(&mut tx, key).await?;
        if epochs.authorization_epoch < state.authority_epochs.authorization_epoch
            || epochs.device_trust_epoch < state.authority_epochs.device_trust_epoch
            || epochs.permission_epoch < state.authority_epochs.permission_epoch
        {
            let error =
                SessionContextCoordinatorError::Invalid("authority epochs cannot decrease".into());
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "advance_epochs",
                    outcome: "rejected",
                    actor: None,
                    lease_id: None,
                    reservation_id: None,
                    expected_cursor: state.head.as_ref().map(|head| &head.cursor),
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_advance_epochs_audit", source))?;
            return Err(error);
        }
        let outcome = if epochs != state.authority_epochs {
            archive_database_state_receipts(&mut tx, &state).await?;
            state.authority_epochs = epochs;
            state.active_writer = None;
            state.active_reservation = None;
            state.writer_epoch = state.writer_epoch.checked_add(1).ok_or_else(|| {
                SessionContextCoordinatorError::NeedsRepair("writer epoch overflow".into())
            })?;
            update_database_state(&mut tx, &state).await?;
            "advanced"
        } else {
            "unchanged"
        };
        record_database_authority_event(
            &mut tx,
            &state,
            AuthorityAuditFact {
                operation: "advance_epochs",
                outcome,
                actor: None,
                lease_id: None,
                reservation_id: None,
                expected_cursor: state.head.as_ref().map(|head| &head.cursor),
            },
        )
        .await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_advance_authority", source))?;
        Ok(())
    }
}

impl DatabaseSessionContextCoordinator {
    async fn load_database_segments(
        &self,
        key: &SessionKeyV1,
        hashes: Vec<String>,
    ) -> Result<
        std::collections::HashMap<String, ConversationSegmentV1>,
        SessionContextCoordinatorError,
    > {
        let mut segments = std::collections::HashMap::with_capacity(hashes.len());
        for chunk in hashes.chunks(256) {
            let mut query = QueryBuilder::<MySql>::new(
                "SELECT segment_hash, segment_json FROM conversation_segments \
                 INNER JOIN ",
            );
            push_matrixone_bound_string_set(&mut query, chunk.iter().map(String::as_str));
            query
                .push(" AS requested_segment ON requested_segment.value = segment_hash")
                .push(" WHERE isolation_domain = ")
                .push_bind(&key.isolation_domain)
                .push(" AND owner_user_id = ")
                .push_bind(&key.owner_user_id);
            let rows = query
                .build()
                .fetch_all(self.pool.get())
                .await
                .map_err(|source| database_error("load_segments", source))?;
            for row in rows {
                let stored_hash = row
                    .try_get::<String, _>("segment_hash")
                    .map_err(|source| database_error("decode_segment_hash", source))?;
                let json = row
                    .try_get::<String, _>("segment_json")
                    .map_err(|source| database_error("decode_segment_json", source))?;
                let segment: ConversationSegmentV1 = database_json("segment", &json)?;
                segment.validate_for(key).map_err(|error| {
                    SessionContextCoordinatorError::NeedsRepair(error.to_string())
                })?;
                if segment.segment_hash != stored_hash {
                    return Err(SessionContextCoordinatorError::NeedsRepair(
                        "database segment key does not match content".into(),
                    ));
                }
                segments.insert(stored_hash, segment);
            }
        }
        Ok(segments)
    }

    async fn persist_database_immutables(
        &self,
        key: &SessionKeyV1,
        segments: &[ConversationSegmentV1],
        node: &ContextManifestNodeV1,
        total_canonical_bytes: u64,
        total_message_count: u64,
    ) -> Result<(), SessionContextCoordinatorError> {
        self.persist_database_segments(key, segments).await?;
        let mut tx = self
            .pool
            .get()
            .begin()
            .await
            .map_err(|source| database_error("begin_persist_manifest", source))?;
        let mut lock_segments = QueryBuilder::<MySql>::new(
            "SELECT segment_hash FROM conversation_segments
             WHERE isolation_domain = ",
        );
        lock_segments
            .push_bind(&key.isolation_domain)
            .push(" AND owner_user_id = ")
            .push_bind(&key.owner_user_id)
            .push(" AND segment_hash IN (");
        {
            let mut separated = lock_segments.separated(", ");
            for segment in segments {
                separated.push_bind(&segment.segment_hash);
            }
        }
        lock_segments.push(") FOR UPDATE");
        let locked_segments = lock_segments
            .build()
            .fetch_all(&mut *tx)
            .await
            .map_err(|source| database_error("lock_manifest_segments", source))?;
        let locked_hashes = locked_segments
            .into_iter()
            .map(|row| {
                row.try_get::<String, _>("segment_hash")
                    .map_err(|source| database_error("decode_locked_manifest_segment", source))
            })
            .collect::<Result<HashSet<_>, _>>()?;
        if locked_hashes.len() != segments.len()
            || segments
                .iter()
                .any(|segment| !locked_hashes.contains(&segment.segment_hash))
        {
            return Err(SessionContextCoordinatorError::NeedsRepair(
                "manifest references a segment that is not durably staged".into(),
            ));
        }
        let canonical_segment_bytes =
            node.appended_segments
                .iter()
                .try_fold(0_u64, |total, segment| {
                    total.checked_add(segment.canonical_bytes).ok_or_else(|| {
                        SessionContextCoordinatorError::Invalid(
                            "manifest segment byte count overflow".into(),
                        )
                    })
                })?;
        let manifest_insert_sql = matrixone_statement_with_null_shape(
            "INSERT IGNORE INTO conversation_manifest_nodes
             (isolation_domain, owner_user_id, session_id, branch_id, manifest_root,
              parent_manifest_root, completed_turn, conversation_seq,
              compaction_generation, canonical_segment_bytes, total_canonical_bytes,
              total_message_count, manifest_json)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            [node.parent_manifest_root.is_some()],
        );
        let result = sqlx::query(&manifest_insert_sql)
            .bind(&key.isolation_domain)
            .bind(&key.owner_user_id)
            .bind(&key.session_id)
            .bind(&key.branch_id)
            .bind(&node.manifest_root)
            .bind(&node.parent_manifest_root)
            .bind(i64::from(node.completed_turn))
            .bind(i64_from_u64(
                "conversation sequence",
                node.conversation_seq,
            )?)
            .bind(i64_from_u64(
                "manifest compaction generation",
                node.compaction_generation,
            )?)
            .bind(i64_from_u64(
                "manifest segment bytes",
                canonical_segment_bytes,
            )?)
            .bind(i64_from_u64(
                "manifest total canonical bytes",
                total_canonical_bytes,
            )?)
            .bind(i64_from_u64(
                "manifest total message count",
                total_message_count,
            )?)
            .bind(database_to_json("manifest", node)?)
            .execute(&mut *tx)
            .await
            .map_err(|source| database_error("persist_manifest", source))?;
        if result.rows_affected() == 0 {
            let stored = sqlx::query(
                "SELECT manifest_json FROM conversation_manifest_nodes
                 WHERE isolation_domain = ? AND owner_user_id = ?
                   AND session_id = ? AND branch_id = ? AND manifest_root = ?",
            )
            .bind(&key.isolation_domain)
            .bind(&key.owner_user_id)
            .bind(&key.session_id)
            .bind(&key.branch_id)
            .bind(&node.manifest_root)
            .fetch_one(&mut *tx)
            .await
            .map_err(|source| database_error("verify_existing_manifest", source))?
            .try_get::<String, _>("manifest_json")
            .map_err(|source| database_error("decode_existing_manifest", source))?;
            let stored: ContextManifestNodeV1 = database_json("existing_manifest", &stored)?;
            if stored != *node {
                return Err(SessionContextCoordinatorError::NeedsRepair(
                    "existing immutable manifest does not match its content-addressed key".into(),
                ));
            }
        }

        let mut insert_references = QueryBuilder::<MySql>::new(
            "INSERT IGNORE INTO conversation_manifest_segments
             (isolation_domain, owner_user_id, session_id, branch_id,
              manifest_root, segment_position, segment_hash) ",
        );
        insert_references.push_values(
            node.appended_segments.iter().enumerate(),
            |mut values, (position, segment)| {
                values
                    .push_bind(&key.isolation_domain)
                    .push_bind(&key.owner_user_id)
                    .push_bind(&key.session_id)
                    .push_bind(&key.branch_id)
                    .push_bind(&node.manifest_root)
                    .push_bind(i64::try_from(position).unwrap_or(i64::MAX))
                    .push_bind(&segment.segment_hash);
            },
        );
        insert_references
            .build()
            .execute(&mut *tx)
            .await
            .map_err(|source| database_error("persist_manifest_segment_references", source))?;
        let stored_references = sqlx::query(
            "SELECT segment_position, segment_hash
             FROM conversation_manifest_segments
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ? AND manifest_root = ?
             ORDER BY segment_position ASC",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .bind(&node.manifest_root)
        .fetch_all(&mut *tx)
        .await
        .map_err(|source| database_error("verify_manifest_segment_references", source))?;
        if stored_references.len() != node.appended_segments.len() {
            return Err(SessionContextCoordinatorError::NeedsRepair(
                "immutable manifest segment reference count is inconsistent".into(),
            ));
        }
        for (position, row) in stored_references.iter().enumerate() {
            let stored_position = database_u64(row, "segment_position")?;
            let stored_hash = row
                .try_get::<String, _>("segment_hash")
                .map_err(|source| database_error("decode_manifest_segment_reference", source))?;
            if stored_position != u64::try_from(position).unwrap_or(u64::MAX)
                || stored_hash != node.appended_segments[position].segment_hash
            {
                return Err(SessionContextCoordinatorError::NeedsRepair(
                    "immutable manifest segment reference does not match the manifest".into(),
                ));
            }
        }
        tx.commit()
            .await
            .map_err(|source| database_error("commit_persist_manifest", source))
    }

    async fn persist_database_segments(
        &self,
        key: &SessionKeyV1,
        segments: &[ConversationSegmentV1],
    ) -> Result<(), SessionContextCoordinatorError> {
        for segment in segments {
            let json = database_to_json("segment", segment)?;
            let result = sqlx::query(
                "INSERT IGNORE INTO conversation_segments
                 (isolation_domain, owner_user_id, segment_hash, canonical_root_hash,
                  canonical_bytes, message_count, segment_json)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&key.isolation_domain)
            .bind(&key.owner_user_id)
            .bind(&segment.segment_hash)
            .bind(&segment.canonical_root_hash)
            .bind(i64_from_u64("segment bytes", segment.canonical_bytes)?)
            .bind(i64::from(segment.message_count))
            .bind(json)
            .execute(self.pool.get())
            .await
            .map_err(|source| database_error("persist_segment", source))?;
            if result.rows_affected() == 0 {
                let stored = sqlx::query(
                    "SELECT segment_json FROM conversation_segments
                     WHERE isolation_domain = ? AND owner_user_id = ? AND segment_hash = ?",
                )
                .bind(&key.isolation_domain)
                .bind(&key.owner_user_id)
                .bind(&segment.segment_hash)
                .fetch_one(self.pool.get())
                .await
                .map_err(|source| database_error("verify_existing_segment", source))?
                .try_get::<String, _>("segment_json")
                .map_err(|source| database_error("decode_existing_segment", source))?;
                let stored: ConversationSegmentV1 = database_json("existing_segment", &stored)?;
                if stored != *segment {
                    return Err(SessionContextCoordinatorError::NeedsRepair(
                        "existing immutable segment does not match its content-addressed key"
                            .into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

fn order_manifest_chain(
    head: &SessionContextHeadV1,
    mut nodes: std::collections::HashMap<String, ContextManifestNodeV1>,
) -> Result<Vec<ContextManifestNodeV1>, SessionContextCoordinatorError> {
    let mut root = Some(head.latest_manifest_root.clone());
    let mut seen = HashSet::new();
    let mut reverse = Vec::new();
    while let Some(current) = root {
        if !seen.insert(current.clone()) {
            return Err(SessionContextCoordinatorError::NeedsRepair(
                "manifest cycle detected".into(),
            ));
        }
        let node = nodes.remove(&current).ok_or_else(|| {
            SessionContextCoordinatorError::NeedsRepair(format!("missing manifest {current}"))
        })?;
        root = if node.replaces_history {
            None
        } else {
            node.parent_manifest_root.clone()
        };
        reverse.push(node);
    }
    reverse.reverse();
    if reverse
        .last()
        .is_none_or(|node| !cursor_projection_matches_head(&node.cursor(), &head.cursor))
    {
        return Err(SessionContextCoordinatorError::NeedsRepair(
            "head cursor does not match database manifest".into(),
        ));
    }
    Ok(reverse)
}

fn cursor_projection_matches_head(node: &SessionCursorV1, head: &SessionCursorV1) -> bool {
    node.schema_version == head.schema_version
        && node.completed_turn == head.completed_turn
        && node.journal_event_seq == head.journal_event_seq
        && node.conversation_seq == head.conversation_seq
        && node.canonical_root_hash == head.canonical_root_hash
        && node.projection_schema == head.projection_schema
        && node.compaction_generation == head.compaction_generation
        && node.config_version_id == head.config_version_id
}

fn order_manifest_suffix(
    head: &SessionContextHeadV1,
    after_manifest_root: Option<&str>,
    after_sequence: Option<u64>,
    mut nodes: std::collections::HashMap<String, ContextManifestNodeV1>,
) -> Result<Vec<ContextManifestNodeV1>, SessionContextCoordinatorError> {
    let mut root = Some(head.latest_manifest_root.clone());
    let mut seen = HashSet::new();
    let mut reverse = Vec::new();
    while root.as_deref() != after_manifest_root {
        let current = root.ok_or(SessionContextCoordinatorError::DivergentManifest)?;
        if !seen.insert(current.clone()) {
            return Err(SessionContextCoordinatorError::NeedsRepair(
                "manifest cycle detected while loading delta".into(),
            ));
        }
        let node = nodes.remove(&current).ok_or_else(|| {
            if after_manifest_root.is_some() {
                SessionContextCoordinatorError::DivergentManifest
            } else {
                SessionContextCoordinatorError::NeedsRepair(format!("missing manifest {current}"))
            }
        })?;
        if after_sequence.is_some_and(|sequence| node.conversation_seq <= sequence) {
            return Err(SessionContextCoordinatorError::DivergentManifest);
        }
        if node.replaces_history
            && after_manifest_root.is_some()
            && node.parent_manifest_root.as_deref() != after_manifest_root
        {
            return Err(SessionContextCoordinatorError::DivergentManifest);
        }
        root = if node.replaces_history && after_manifest_root.is_none() {
            None
        } else {
            node.parent_manifest_root.clone()
        };
        reverse.push(node);
    }
    reverse.reverse();
    if reverse
        .last()
        .is_none_or(|node| !cursor_projection_matches_head(&node.cursor(), &head.cursor))
    {
        return Err(SessionContextCoordinatorError::NeedsRepair(
            "head cursor does not match manifest delta".into(),
        ));
    }
    Ok(reverse)
}

fn manifest_delta(
    key: SessionKeyV1,
    after_manifest_root: Option<String>,
    head: Option<SessionContextHeadV1>,
    shared_prefix: Option<SharedManifestPrefixV1>,
    missing_nodes: Vec<ContextManifestNodeV1>,
) -> Result<ManifestDeltaV1, SessionContextCoordinatorError> {
    let (missing_canonical_bytes, missing_message_count) = missing_nodes
        .iter()
        .try_fold((0_u64, 0_u64), |(bytes, messages), node| {
            node.appended_segments.iter().try_fold(
                (bytes, messages),
                |(bytes, messages), segment| {
                    Some((
                        bytes.checked_add(segment.canonical_bytes)?,
                        messages.checked_add(u64::from(segment.message_count))?,
                    ))
                },
            )
        })
        .ok_or_else(|| {
            SessionContextCoordinatorError::NeedsRepair("manifest delta totals overflow".into())
        })?;
    let delta = ManifestDeltaV1 {
        schema_version: MANIFEST_DELTA_SCHEMA_VERSION,
        key,
        after_manifest_root,
        head,
        shared_prefix,
        missing_nodes,
        missing_canonical_bytes,
        missing_message_count,
    };
    delta
        .validate()
        .map_err(|error| SessionContextCoordinatorError::NeedsRepair(error.to_string()))?;
    Ok(delta)
}

fn materialize_nodes(
    head: &SessionContextHeadV1,
    nodes: Vec<ContextManifestNodeV1>,
    segments: &mut std::collections::HashMap<String, ConversationSegmentV1>,
) -> Result<MaterializedConversationV1, SessionContextCoordinatorError> {
    let mut use_counts = std::collections::HashMap::<String, usize>::new();
    for node in &nodes {
        for segment in &node.appended_segments {
            *use_counts.entry(segment.segment_hash.clone()).or_default() += 1;
        }
    }
    let mut messages = Vec::new();
    let mut logical_segment_count = 0_u64;
    let mut canonical_segment_bytes = 0_u64;
    let mut prior_cursor = None;
    for node in nodes {
        validate_manifest_advance(prior_cursor.as_ref(), &node)?;
        let cursor = node.cursor();
        for segment_ref in node.appended_segments {
            let remaining = use_counts
                .get_mut(&segment_ref.segment_hash)
                .ok_or_else(|| {
                    SessionContextCoordinatorError::NeedsRepair(
                        "manifest segment use count is missing".into(),
                    )
                })?;
            *remaining -= 1;
            if *remaining == 0 {
                let segment = segments.remove(&segment_ref.segment_hash).ok_or_else(|| {
                    SessionContextCoordinatorError::NeedsRepair(format!(
                        "missing segment {}",
                        segment_ref.segment_hash
                    ))
                })?;
                if segment.reference() != segment_ref {
                    return Err(SessionContextCoordinatorError::NeedsRepair(
                        "segment metadata does not match manifest reference".into(),
                    ));
                }
                canonical_segment_bytes = canonical_segment_bytes
                    .checked_add(segment.canonical_bytes)
                    .ok_or_else(|| {
                        SessionContextCoordinatorError::NeedsRepair(
                            "materialized byte count overflow".into(),
                        )
                    })?;
                messages.extend(segment.messages);
            } else {
                let segment = segments.get(&segment_ref.segment_hash).ok_or_else(|| {
                    SessionContextCoordinatorError::NeedsRepair(format!(
                        "missing segment {}",
                        segment_ref.segment_hash
                    ))
                })?;
                if segment.reference() != segment_ref {
                    return Err(SessionContextCoordinatorError::NeedsRepair(
                        "segment metadata does not match manifest reference".into(),
                    ));
                }
                canonical_segment_bytes = canonical_segment_bytes
                    .checked_add(segment.canonical_bytes)
                    .ok_or_else(|| {
                        SessionContextCoordinatorError::NeedsRepair(
                            "materialized byte count overflow".into(),
                        )
                    })?;
                messages.extend(segment.messages.iter().cloned());
            }
            logical_segment_count = logical_segment_count.checked_add(1).ok_or_else(|| {
                SessionContextCoordinatorError::NeedsRepair(
                    "materialized segment count overflow".into(),
                )
            })?;
        }
        prior_cursor = Some(cursor);
    }
    if canonical_segment_bytes != head.total_canonical_bytes
        || u64::try_from(messages.len()).ok() != Some(head.total_message_count)
    {
        return Err(SessionContextCoordinatorError::NeedsRepair(
            "materialized totals do not match the canonical head".into(),
        ));
    }
    Ok(MaterializedConversationV1 {
        head: head.clone(),
        messages,
        logical_segment_count,
        canonical_segment_bytes,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CoordinatorStateV1 {
    schema_version: u32,
    key: SessionKeyV1,
    writer_epoch: u64,
    authority_epochs: AuthorityEpochsV1,
    head: Option<SessionContextHeadV1>,
    active_writer: Option<ConversationWriterLeaseV1>,
    active_reservation: Option<TurnReservationV1>,
    last_commit: Option<CommitReceiptV1>,
    #[serde(default)]
    last_transfer: Option<WriterTransferReceiptV1>,
    #[serde(default)]
    fork_base: Option<SharedManifestPrefixV1>,
    #[serde(default)]
    fork_manifest: Option<SessionForkManifestV1>,
}

impl CoordinatorStateV1 {
    fn new(key: SessionKeyV1) -> Self {
        Self {
            schema_version: COORDINATOR_STATE_SCHEMA_VERSION,
            key,
            writer_epoch: 0,
            authority_epochs: AuthorityEpochsV1::default(),
            head: None,
            active_writer: None,
            active_reservation: None,
            last_commit: None,
            last_transfer: None,
            fork_base: None,
            fork_manifest: None,
        }
    }

    fn validate_for(&self, key: &SessionKeyV1) -> Result<(), SessionContextCoordinatorError> {
        if self.schema_version != COORDINATOR_STATE_SCHEMA_VERSION || &self.key != key {
            return Err(SessionContextCoordinatorError::NeedsRepair(
                "state schema or owner-scoped key mismatch".into(),
            ));
        }
        if self
            .head
            .as_ref()
            .is_some_and(|head| head.key != *key || head.writer_epoch > self.writer_epoch)
            || self
                .last_transfer
                .as_ref()
                .is_some_and(|receipt| receipt.lease.key != *key)
            || self
                .fork_base
                .as_ref()
                .is_some_and(|prefix| prefix.validate_for_child(key).is_err())
            || self.fork_manifest.as_ref().is_some_and(|manifest| {
                manifest.child_key != *key
                    || manifest.state != SessionForkStateV1::Active
                    || manifest.validate().is_err()
            })
        {
            return Err(SessionContextCoordinatorError::NeedsRepair(
                "head key or writer epoch is invalid".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LeaseReceiptV1 {
    lease: ConversationWriterLeaseV1,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReservationReceiptV1 {
    reservation: TurnReservationV1,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CommitReceiptV1 {
    idempotency_key: String,
    reservation_id: String,
    reservation: TurnReservationV1,
    delta_hash: String,
    cursor: SessionCursorV1,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WriterTransferReceiptV1 {
    idempotency_key: String,
    request_hash: String,
    handoff_id: String,
    mode: SessionHandoffModeV1,
    risk: HandoffRiskEvidenceV1,
    lease: ConversationWriterLeaseV1,
}

async fn ensure_database_state(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
    epochs: AuthorityEpochsV1,
) -> Result<(), SessionContextCoordinatorError> {
    sqlx::query(
        "INSERT IGNORE INTO session_context_heads
         (isolation_domain, owner_user_id, session_id, branch_id,
          authorization_epoch, device_trust_epoch, permission_epoch)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .bind(i64_from_u64(
        "authorization epoch",
        epochs.authorization_epoch,
    )?)
    .bind(i64_from_u64(
        "device trust epoch",
        epochs.device_trust_epoch,
    )?)
    .bind(i64_from_u64("permission epoch", epochs.permission_epoch)?)
    .execute(&mut **tx)
    .await
    .map_err(|source| database_error("ensure_context_head", source))?;
    Ok(())
}

async fn lock_database_state(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
) -> Result<CoordinatorStateV1, SessionContextCoordinatorError> {
    let row = sqlx::query(
        "SELECT head_json, writer_epoch, authorization_epoch, device_trust_epoch,
                permission_epoch, active_writer_json, active_reservation_json,
                last_commit_json, fork_base_json
         FROM session_context_heads
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ?
         FOR UPDATE",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| database_error("lock_context_head", source))?
    .ok_or_else(|| {
        SessionContextCoordinatorError::NeedsRepair(
            "context head state is missing during mutation".into(),
        )
    })?;
    let optional_json = |column: &'static str| -> Result<Option<String>, _> {
        row.try_get::<Option<String>, _>(column)
    };
    let mut state = CoordinatorStateV1::new(key.clone());
    state.writer_epoch = database_u64(&row, "writer_epoch")?;
    state.authority_epochs = AuthorityEpochsV1 {
        authorization_epoch: database_u64(&row, "authorization_epoch")?,
        device_trust_epoch: database_u64(&row, "device_trust_epoch")?,
        permission_epoch: database_u64(&row, "permission_epoch")?,
    };
    state.head = optional_json("head_json")
        .map_err(|source| database_error("decode_head_json", source))?
        .as_deref()
        .map(|json| database_json("head", json))
        .transpose()?;
    state.active_writer = optional_json("active_writer_json")
        .map_err(|source| database_error("decode_writer_json", source))?
        .as_deref()
        .map(|json| database_json("writer_lease", json))
        .transpose()?;
    state.active_reservation = optional_json("active_reservation_json")
        .map_err(|source| database_error("decode_reservation_json", source))?
        .as_deref()
        .map(|json| database_json("turn_reservation", json))
        .transpose()?;
    state.last_commit = optional_json("last_commit_json")
        .map_err(|source| database_error("decode_commit_json", source))?
        .as_deref()
        .map(|json| database_json("commit_receipt", json))
        .transpose()?;
    state.fork_base = optional_json("fork_base_json")
        .map_err(|source| database_error("decode_fork_base_json", source))?
        .as_deref()
        .map(|json| database_json("fork_base", json))
        .transpose()?;
    state.validate_for(key)?;
    Ok(state)
}

async fn update_database_state(
    tx: &mut Transaction<'_, MySql>,
    state: &CoordinatorStateV1,
) -> Result<(), SessionContextCoordinatorError> {
    let head_json = state
        .head
        .as_ref()
        .map(|head| database_to_json("head", head))
        .transpose()?;
    let active_writer_json = state
        .active_writer
        .as_ref()
        .map(|lease| database_to_json("writer_lease", lease))
        .transpose()?;
    let active_reservation_json = state
        .active_reservation
        .as_ref()
        .map(|reservation| database_to_json("turn_reservation", reservation))
        .transpose()?;
    let last_commit_json = state
        .last_commit
        .as_ref()
        .map(|receipt| database_to_json("commit_receipt", receipt))
        .transpose()?;
    let fork_base_json = state
        .fork_base
        .as_ref()
        .map(|prefix| database_to_json("fork_base", prefix))
        .transpose()?;
    let (canonical_root, manifest_root, completed_turn, journal_event_seq, conversation_seq) =
        if let Some(head) = &state.head {
            (
                Some(head.cursor.canonical_root_hash.as_str()),
                Some(head.latest_manifest_root.as_str()),
                i64::from(head.cursor.completed_turn),
                i64_from_u64("journal event sequence", head.cursor.journal_event_seq)?,
                i64_from_u64("conversation sequence", head.cursor.conversation_seq)?,
            )
        } else {
            (None, None, 0_i64, 0_i64, 0_i64)
        };
    let projection_schema = state
        .head
        .as_ref()
        .map_or(0_i64, |head| i64::from(head.cursor.projection_schema));
    let compaction_generation = state.head.as_ref().map_or(Ok(0_i64), |head| {
        i64_from_u64("compaction generation", head.cursor.compaction_generation)
    })?;
    let result = sqlx::query(
        "UPDATE session_context_heads
         SET head_json = ?, canonical_root_hash = ?, latest_manifest_root = ?,
             total_canonical_bytes = ?, total_message_count = ?,
             completed_turn = ?, journal_event_seq = ?, conversation_seq = ?,
             projection_schema = ?, compaction_generation = ?, writer_epoch = ?,
             authorization_epoch = ?, device_trust_epoch = ?, permission_epoch = ?,
             active_writer_json = ?, active_writer_expires_at_ms = ?,
             active_reservation_json = ?, active_reservation_expires_at_ms = ?,
             last_commit_json = ?, fork_base_json = ?, updated_at = NOW(6)
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ?",
    )
    .bind(head_json)
    .bind(canonical_root)
    .bind(manifest_root)
    .bind(state.head.as_ref().map_or(Ok(0_i64), |head| {
        i64_from_u64("total canonical bytes", head.total_canonical_bytes)
    })?)
    .bind(state.head.as_ref().map_or(Ok(0_i64), |head| {
        i64_from_u64("total message count", head.total_message_count)
    })?)
    .bind(completed_turn)
    .bind(journal_event_seq)
    .bind(conversation_seq)
    .bind(projection_schema)
    .bind(compaction_generation)
    .bind(i64_from_u64("writer epoch", state.writer_epoch)?)
    .bind(i64_from_u64(
        "authorization epoch",
        state.authority_epochs.authorization_epoch,
    )?)
    .bind(i64_from_u64(
        "device trust epoch",
        state.authority_epochs.device_trust_epoch,
    )?)
    .bind(i64_from_u64(
        "permission epoch",
        state.authority_epochs.permission_epoch,
    )?)
    .bind(active_writer_json)
    .bind(
        state
            .active_writer
            .as_ref()
            .map(|lease| lease.expires_at_unix_ms),
    )
    .bind(active_reservation_json)
    .bind(
        state
            .active_reservation
            .as_ref()
            .map(|reservation| reservation.expires_at_unix_ms),
    )
    .bind(last_commit_json)
    .bind(fork_base_json)
    .bind(&state.key.isolation_domain)
    .bind(&state.key.owner_user_id)
    .bind(&state.key.session_id)
    .bind(&state.key.branch_id)
    .execute(&mut **tx)
    .await
    .map_err(|source| database_error("update_context_head", source))?;
    if result.rows_affected() != 1 {
        return Err(SessionContextCoordinatorError::NeedsRepair(
            "context head CAS row disappeared".into(),
        ));
    }
    Ok(())
}

async fn database_now_ms(
    tx: &mut Transaction<'_, MySql>,
) -> Result<i64, SessionContextCoordinatorError> {
    crate::db_row::database_now_unix_ms(tx)
        .await
        .map_err(|source| database_error("load_database_time", source))
}

struct AuthorityAuditFact<'a> {
    operation: &'static str,
    outcome: &'static str,
    actor: Option<&'a ActorContextV1>,
    lease_id: Option<&'a str>,
    reservation_id: Option<&'a str>,
    expected_cursor: Option<&'a SessionCursorV1>,
}

fn authority_error_outcome(error: &SessionContextCoordinatorError) -> &'static str {
    match error {
        SessionContextCoordinatorError::Fenced => "stale_fenced",
        SessionContextCoordinatorError::Expired => "expired",
        SessionContextCoordinatorError::IdempotencyMismatch => "idempotency_mismatch",
        SessionContextCoordinatorError::Unauthorized => "unauthorized",
        SessionContextCoordinatorError::NeedsRepair(_) => "needs_repair",
        _ => "rejected",
    }
}

async fn record_database_authority_event(
    tx: &mut Transaction<'_, MySql>,
    state: &CoordinatorStateV1,
    fact: AuthorityAuditFact<'_>,
) -> Result<(), SessionContextCoordinatorError> {
    let actor = fact
        .actor
        .or_else(|| state.active_writer.as_ref().map(|lease| &lease.actor));
    sqlx::query(
        "INSERT INTO session_context_authority_events
         (isolation_domain, owner_user_id, event_id, session_id, branch_id,
          operation_kind, outcome, writer_epoch, actor_id, device_id, lease_id,
          reservation_id, expected_root, observed_root, authorization_epoch,
          device_trust_epoch, permission_epoch)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&state.key.isolation_domain)
    .bind(&state.key.owner_user_id)
    .bind(Uuid::new_v4().to_string())
    .bind(&state.key.session_id)
    .bind(&state.key.branch_id)
    .bind(fact.operation)
    .bind(fact.outcome)
    .bind(i64_from_u64("audit writer epoch", state.writer_epoch)?)
    .bind(actor.map(|actor| actor.actor_id.as_str()))
    .bind(actor.and_then(|actor| actor.device_id.as_deref()))
    .bind(fact.lease_id)
    .bind(fact.reservation_id)
    .bind(
        fact.expected_cursor
            .map(|cursor| cursor.canonical_root_hash.as_str()),
    )
    .bind(
        state
            .head
            .as_ref()
            .map(|head| head.cursor.canonical_root_hash.as_str()),
    )
    .bind(i64_from_u64(
        "audit authorization epoch",
        state.authority_epochs.authorization_epoch,
    )?)
    .bind(i64_from_u64(
        "audit device trust epoch",
        state.authority_epochs.device_trust_epoch,
    )?)
    .bind(i64_from_u64(
        "audit permission epoch",
        state.authority_epochs.permission_epoch,
    )?)
    .execute(&mut **tx)
    .await
    .map_err(|source| database_error("record_authority_event", source))?;
    Ok(())
}

async fn record_database_authority_event_pool(
    pool: &sqlx::Pool<MySql>,
    key: &SessionKeyV1,
    fact: AuthorityAuditFact<'_>,
) -> Result<(), SessionContextCoordinatorError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|source| database_error("begin_authority_event", source))?;
    let state = lock_database_state(&mut tx, key).await?;
    record_database_authority_event(&mut tx, &state, fact).await?;
    tx.commit()
        .await
        .map_err(|source| database_error("commit_authority_event", source))
}

async fn load_database_receipt<T: DeserializeOwned>(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
    operation: &'static str,
    idempotency_key: &str,
    request_hash: &str,
) -> Result<Option<T>, SessionContextCoordinatorError> {
    let row = sqlx::query(
        "SELECT request_hash, receipt_json FROM session_context_operation_receipts
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ?
           AND operation_kind = ? AND idempotency_hash = ?",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .bind(operation)
    .bind(hash_receipt(operation, idempotency_key))
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| database_error("load_operation_receipt", source))?;
    decode_database_receipt(row, request_hash)
}

async fn load_database_receipt_pool<T: DeserializeOwned>(
    pool: &sqlx::Pool<MySql>,
    key: &SessionKeyV1,
    operation: &'static str,
    idempotency_key: &str,
    request_hash: &str,
) -> Result<Option<T>, SessionContextCoordinatorError> {
    let row = sqlx::query(
        "SELECT request_hash, receipt_json FROM session_context_operation_receipts
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ?
           AND operation_kind = ? AND idempotency_hash = ?",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .bind(operation)
    .bind(hash_receipt(operation, idempotency_key))
    .fetch_optional(pool)
    .await
    .map_err(|source| database_error("load_operation_receipt", source))?;
    decode_database_receipt(row, request_hash)
}

fn decode_database_receipt<T: DeserializeOwned>(
    row: Option<sqlx::mysql::MySqlRow>,
    request_hash: &str,
) -> Result<Option<T>, SessionContextCoordinatorError> {
    let Some(row) = row else {
        return Ok(None);
    };
    let stored_request_hash = row
        .try_get::<String, _>("request_hash")
        .map_err(|source| database_error("decode_receipt_hash", source))?;
    if stored_request_hash != request_hash {
        return Err(SessionContextCoordinatorError::IdempotencyMismatch);
    }
    let receipt_json = row
        .try_get::<String, _>("receipt_json")
        .map_err(|source| database_error("decode_receipt_json", source))?;
    database_json("operation_receipt", &receipt_json).map(Some)
}

async fn store_database_receipt<T: Serialize>(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
    operation: &'static str,
    idempotency_key: &str,
    request_hash: &str,
    receipt: &T,
) -> Result<(), SessionContextCoordinatorError> {
    sqlx::query(
        "INSERT IGNORE INTO session_context_operation_receipts
         (isolation_domain, owner_user_id, session_id, branch_id, operation_kind,
          idempotency_hash, request_hash, receipt_json)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .bind(operation)
    .bind(hash_receipt(operation, idempotency_key))
    .bind(request_hash)
    .bind(database_to_json("operation_receipt", receipt)?)
    .execute(&mut **tx)
    .await
    .map_err(|source| database_error("store_operation_receipt", source))?;
    Ok(())
}

async fn archive_database_state_receipts(
    tx: &mut Transaction<'_, MySql>,
    state: &CoordinatorStateV1,
) -> Result<(), SessionContextCoordinatorError> {
    if let Some(lease) = &state.active_writer {
        store_database_receipt(
            tx,
            &state.key,
            "acquire",
            &lease.idempotency_key,
            &lease_request_hash(&state.key, lease.expected_cursor.as_ref(), &lease.actor),
            &LeaseReceiptV1 {
                lease: lease.clone(),
            },
        )
        .await?;
    }
    if let Some(reservation) = &state.active_reservation {
        archive_database_reservation(tx, reservation).await?;
    }
    Ok(())
}

async fn archive_database_reservation(
    tx: &mut Transaction<'_, MySql>,
    reservation: &TurnReservationV1,
) -> Result<(), SessionContextCoordinatorError> {
    store_database_receipt(
        tx,
        &reservation.key,
        "reserve",
        &reservation.idempotency_key,
        &reservation_identity_hash(
            &reservation.key,
            &reservation.lease_id,
            reservation.writer_epoch,
            reservation.expected_cursor.as_ref(),
        ),
        &ReservationReceiptV1 {
            reservation: reservation.clone(),
        },
    )
    .await
}

async fn archive_database_commit(
    tx: &mut Transaction<'_, MySql>,
    receipt: &CommitReceiptV1,
) -> Result<(), SessionContextCoordinatorError> {
    store_database_receipt(
        tx,
        &receipt.reservation.key,
        "commit",
        &receipt.idempotency_key,
        &commit_receipt_request_hash(receipt),
        receipt,
    )
    .await
}

fn database_error(operation: &'static str, source: sqlx::Error) -> SessionContextCoordinatorError {
    SessionContextCoordinatorError::Database { operation, source }
}

fn validate_prepared_fork(
    manifest: &SessionForkManifestV1,
) -> Result<(), SessionContextCoordinatorError> {
    manifest
        .validate()
        .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
    if manifest.state != SessionForkStateV1::Prepared {
        return Err(SessionContextCoordinatorError::Invalid(
            "fork activation requires a prepared manifest".into(),
        ));
    }
    Ok(())
}

fn fork_child_head(manifest: &SessionForkManifestV1, writer_epoch: u64) -> SessionContextHeadV1 {
    let mut cursor = manifest.parent_head.cursor.clone();
    cursor.owner_id = manifest.child_key.owner_user_id.clone();
    cursor.session_id = manifest.child_key.session_id.clone();
    cursor.branch_id = manifest.child_key.branch_id.clone();
    SessionContextHeadV1 {
        schema_version: SESSION_COORDINATION_SCHEMA_VERSION,
        key: manifest.child_key.clone(),
        cursor,
        latest_manifest_root: manifest.parent_head.latest_manifest_root.clone(),
        total_canonical_bytes: manifest.parent_head.total_canonical_bytes,
        total_message_count: manifest.parent_head.total_message_count,
        writer_epoch,
    }
}

async fn insert_fork_event(
    tx: &mut Transaction<'_, MySql>,
    manifest: &SessionForkManifestV1,
    transition_seq: u64,
    from_state: &str,
    to_state: &str,
) -> Result<(), SessionContextCoordinatorError> {
    sqlx::query(
        "INSERT IGNORE INTO session_fork_events
         (isolation_domain, owner_user_id, fork_id, transition_seq,
          parent_session_id, child_session_id, from_state, to_state, event_json)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&manifest.child_key.isolation_domain)
    .bind(&manifest.child_key.owner_user_id)
    .bind(&manifest.fork_id)
    .bind(i64_from_u64("fork transition sequence", transition_seq)?)
    .bind(&manifest.parent_key.session_id)
    .bind(&manifest.child_key.session_id)
    .bind(from_state)
    .bind(to_state)
    .bind(database_to_json("fork_event", manifest)?)
    .execute(&mut **tx)
    .await
    .map_err(|source| database_error("insert_fork_event", source))?;
    Ok(())
}

fn database_json<T: DeserializeOwned>(
    entity: &'static str,
    json: &str,
) -> Result<T, SessionContextCoordinatorError> {
    serde_json::from_str(json)
        .map_err(|source| SessionContextCoordinatorError::DatabaseJson { entity, source })
}

fn database_to_json<T: Serialize>(
    entity: &'static str,
    value: &T,
) -> Result<String, SessionContextCoordinatorError> {
    serde_json::to_string(value)
        .map_err(|source| SessionContextCoordinatorError::DatabaseJson { entity, source })
}

fn database_u64(
    row: &sqlx::mysql::MySqlRow,
    column: &'static str,
) -> Result<u64, SessionContextCoordinatorError> {
    let value = row
        .try_get::<i64, _>(column)
        .map_err(|source| database_error("decode_context_head_integer", source))?;
    u64::try_from(value)
        .map_err(|_| SessionContextCoordinatorError::NeedsRepair(format!("{column} is negative")))
}

fn i64_from_u64(field: &'static str, value: u64) -> Result<i64, SessionContextCoordinatorError> {
    i64::try_from(value)
        .map_err(|_| SessionContextCoordinatorError::Invalid(format!("{field} exceeds BIGINT")))
}

fn manifest_node_for_delta(
    key: &SessionKeyV1,
    head: Option<&SessionContextHeadV1>,
    delta: &CanonicalTurnDeltaV1,
    segments: &[ConversationSegmentV1],
) -> Result<ContextManifestNodeV1, SessionCoordinationValidationError> {
    let parent = head.map(|head| head.latest_manifest_root.clone());
    let references = segments
        .iter()
        .map(ConversationSegmentV1::reference)
        .collect();
    match delta.mode {
        CanonicalDeltaModeV1::Append => ContextManifestNodeV1::new(
            key.clone(),
            parent,
            delta.completed_turn,
            delta.journal_event_seq,
            delta.conversation_seq,
            delta.compaction_generation,
            delta.config_version_id.clone(),
            references,
        ),
        CanonicalDeltaModeV1::Replace => ContextManifestNodeV1::new_replacement(
            key.clone(),
            parent,
            delta.completed_turn,
            delta.journal_event_seq,
            delta.conversation_seq,
            delta.compaction_generation,
            delta.config_version_id.clone(),
            references,
        ),
    }
}

fn next_head_totals(
    head: Option<&SessionContextHeadV1>,
    segments: &[ConversationSegmentV1],
    mode: CanonicalDeltaModeV1,
) -> Result<(u64, u64), SessionContextCoordinatorError> {
    let appended_bytes = segments.iter().try_fold(0_u64, |total, segment| {
        total.checked_add(segment.canonical_bytes).ok_or_else(|| {
            SessionContextCoordinatorError::NeedsRepair("head canonical byte count overflow".into())
        })
    })?;
    let appended_messages = segments.iter().try_fold(0_u64, |total, segment| {
        total
            .checked_add(u64::from(segment.message_count))
            .ok_or_else(|| {
                SessionContextCoordinatorError::NeedsRepair("head message count overflow".into())
            })
    })?;
    let base_bytes = if mode == CanonicalDeltaModeV1::Replace {
        0
    } else {
        head.map_or(0, |head| head.total_canonical_bytes)
    };
    let base_messages = if mode == CanonicalDeltaModeV1::Replace {
        0
    } else {
        head.map_or(0, |head| head.total_message_count)
    };
    Ok((
        base_bytes.checked_add(appended_bytes).ok_or_else(|| {
            SessionContextCoordinatorError::NeedsRepair("head canonical byte overflow".into())
        })?,
        base_messages
            .checked_add(appended_messages)
            .ok_or_else(|| {
                SessionContextCoordinatorError::NeedsRepair("head message count overflow".into())
            })?,
    ))
}

fn validate_head(head: &SessionContextHeadV1) -> Result<(), SessionContextCoordinatorError> {
    head.key
        .validate()
        .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
    if head.schema_version != SESSION_COORDINATION_SCHEMA_VERSION
        || !head.key.validates_cursor(&head.cursor)
        || head.cursor.canonical_root_hash != head.latest_manifest_root
        || head.total_message_count == 0
    {
        return Err(SessionContextCoordinatorError::NeedsRepair(
            "invalid context head".into(),
        ));
    }
    Ok(())
}

fn validate_optional_cursor(
    key: &SessionKeyV1,
    cursor: Option<&SessionCursorV1>,
) -> Result<(), SessionContextCoordinatorError> {
    if cursor.is_some_and(|cursor| !key.validates_cursor(cursor)) {
        return Err(SessionContextCoordinatorError::Invalid(
            "cursor identity does not match session key".into(),
        ));
    }
    Ok(())
}

fn validate_optional_manifest_root(
    root: Option<&str>,
) -> Result<(), SessionContextCoordinatorError> {
    if root.is_some_and(|root| {
        root.len() != 64
            || !root
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }) {
        return Err(SessionContextCoordinatorError::Invalid(
            "manifest root must be a lowercase SHA-256 digest".into(),
        ));
    }
    Ok(())
}

fn validate_segment_batch(
    key: &SessionKeyV1,
    segment_hashes: &[String],
) -> Result<(), SessionContextCoordinatorError> {
    key.validate()
        .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
    if segment_hashes.is_empty() || segment_hashes.len() > MAX_SEGMENT_BATCH {
        return Err(SessionContextCoordinatorError::Invalid(format!(
            "segment batch must contain between 1 and {MAX_SEGMENT_BATCH} identities"
        )));
    }
    let mut unique = HashSet::with_capacity(segment_hashes.len());
    for hash in segment_hashes {
        validate_optional_manifest_root(Some(hash))?;
        if !unique.insert(hash) {
            return Err(SessionContextCoordinatorError::Invalid(
                "segment batch contains duplicate identities".into(),
            ));
        }
    }
    Ok(())
}

fn validate_segment_upload(
    key: &SessionKeyV1,
    segments: &[ConversationSegmentV1],
) -> Result<(), SessionContextCoordinatorError> {
    let hashes = segments
        .iter()
        .map(|segment| segment.segment_hash.clone())
        .collect::<Vec<_>>();
    validate_segment_batch(key, &hashes)?;
    let mut total_bytes = 0_u64;
    for segment in segments {
        segment
            .validate_for(key)
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        if segment.canonical_bytes > MAX_STAGED_SEGMENT_BYTES {
            return Err(SessionContextCoordinatorError::Invalid(format!(
                "conversation segment exceeds {MAX_STAGED_SEGMENT_BYTES} canonical bytes"
            )));
        }
        total_bytes = total_bytes
            .checked_add(segment.canonical_bytes)
            .ok_or_else(|| {
                SessionContextCoordinatorError::Invalid(
                    "conversation segment batch byte count overflow".into(),
                )
            })?;
    }
    if total_bytes > MAX_STAGED_BATCH_BYTES {
        return Err(SessionContextCoordinatorError::Invalid(format!(
            "conversation segment batch exceeds {MAX_STAGED_BATCH_BYTES} canonical bytes"
        )));
    }
    Ok(())
}

fn validate_ttl(ttl: Duration, maximum: Duration) -> Result<(), SessionContextCoordinatorError> {
    if ttl.is_zero() || ttl > maximum {
        return Err(SessionContextCoordinatorError::Invalid(format!(
            "TTL must be between 1 ms and {} seconds",
            maximum.as_secs()
        )));
    }
    Ok(())
}

fn validate_idempotency_key(value: &str) -> Result<(), SessionContextCoordinatorError> {
    if value.is_empty()
        || value.len() > MAX_IDEMPOTENCY_KEY_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(SessionContextCoordinatorError::Invalid(
            "idempotency key must be non-empty and at most 512 bytes".into(),
        ));
    }
    Ok(())
}

fn validate_writer_transfer_request(
    request: &WriterTransferRequestV1,
) -> Result<(), SessionContextCoordinatorError> {
    request
        .key
        .validate()
        .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
    request
        .target_actor
        .validate_for(&request.key)
        .map_err(|_| SessionContextCoordinatorError::Unauthorized)?;
    validate_optional_cursor(&request.key, request.expected_cursor.as_ref())?;
    validate_idempotency_key(&request.idempotency_key)?;
    if request.handoff_id.is_empty()
        || request.handoff_id.len() > 128
        || request.handoff_id.chars().any(char::is_control)
    {
        return Err(SessionContextCoordinatorError::Invalid(
            "handoff identity must be non-empty and at most 128 bytes".into(),
        ));
    }
    request
        .risk
        .validate()
        .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
    match request.mode {
        SessionHandoffModeV1::Graceful => {
            let source = request.source_lease.as_ref().ok_or_else(|| {
                SessionContextCoordinatorError::Invalid(
                    "graceful transfer requires the source writer lease".into(),
                )
            })?;
            if source.key != request.key || request.risk != HandoffRiskEvidenceV1::default() {
                return Err(SessionContextCoordinatorError::Invalid(
                    "graceful transfer source or risk evidence is invalid".into(),
                ));
            }
        }
        SessionHandoffModeV1::Forced => {
            if request.source_lease.is_some() || !request.risk.permits_forced_fence() {
                return Err(SessionContextCoordinatorError::Invalid(
                    "forced transfer requires verified authorization and no source lease".into(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_writer_transfer_receipt(
    receipt: &WriterTransferReceiptV1,
    request_hash: &str,
) -> Result<(), SessionContextCoordinatorError> {
    if receipt.request_hash != request_hash {
        return Err(SessionContextCoordinatorError::IdempotencyMismatch);
    }
    Ok(())
}

fn writer_transfer_conflict(
    state: &CoordinatorStateV1,
    reason: WriterTransferConflictV1,
    now: i64,
) -> TransferWriterOutcome {
    TransferWriterOutcome::Conflict {
        reason,
        current_head: state.head.clone(),
        active_lease_expires_at_unix_ms: state
            .active_writer
            .as_ref()
            .filter(|lease| lease.expires_at_unix_ms > now)
            .map(|lease| lease.expires_at_unix_ms),
    }
}

fn checked_expiry(now_unix_ms: i64, ttl: Duration) -> Result<i64, SessionContextCoordinatorError> {
    let ttl_ms =
        i64::try_from(ttl.as_millis()).map_err(|_| SessionContextCoordinatorError::Clock)?;
    now_unix_ms
        .checked_add(ttl_ms)
        .ok_or(SessionContextCoordinatorError::Clock)
}

/// Extends a live authority window without reviving an expired holder.
fn refreshed_live_expiry(
    now_unix_ms: i64,
    ttl: Duration,
    active_expires_at_unix_ms: i64,
    ceiling_expires_at_unix_ms: Option<i64>,
) -> Result<i64, SessionContextCoordinatorError> {
    if active_expires_at_unix_ms <= now_unix_ms
        || ceiling_expires_at_unix_ms.is_some_and(|ceiling| ceiling <= now_unix_ms)
    {
        return Err(SessionContextCoordinatorError::Expired);
    }
    let refreshed = checked_expiry(now_unix_ms, ttl)?;
    Ok(ceiling_expires_at_unix_ms.map_or(refreshed, |ceiling| refreshed.min(ceiling)))
}

/// Atomically removes a writer whose matching in-flight reservation expired.
fn fence_expired_reservation_authority(
    state: &mut CoordinatorStateV1,
    lease: &ConversationWriterLeaseV1,
    now_unix_ms: i64,
) -> bool {
    let reservation_matches_and_expired =
        state
            .active_reservation
            .as_ref()
            .is_some_and(|reservation| {
                reservation.lease_id == lease.lease_id
                    && reservation.writer_epoch == lease.writer_epoch
                    && reservation.expires_at_unix_ms <= now_unix_ms
            });
    let writer_matches = state.active_writer.as_ref().is_some_and(|active| {
        active.lease_id == lease.lease_id && active.writer_epoch == lease.writer_epoch
    });
    if !reservation_matches_and_expired || !writer_matches {
        return false;
    }
    state.active_reservation = None;
    state.active_writer = None;
    true
}

fn validate_lease_request(
    lease: &ConversationWriterLeaseV1,
    key: &SessionKeyV1,
    expected_cursor: &Option<SessionCursorV1>,
    actor: &ActorContextV1,
) -> Result<(), SessionContextCoordinatorError> {
    if &lease.key != key || &lease.expected_cursor != expected_cursor || &lease.actor != actor {
        return Err(SessionContextCoordinatorError::IdempotencyMismatch);
    }
    Ok(())
}

fn validate_reservation_request(
    reservation: &TurnReservationV1,
    lease: &ConversationWriterLeaseV1,
    expected_cursor: &Option<SessionCursorV1>,
) -> Result<(), SessionContextCoordinatorError> {
    if reservation.key != lease.key
        || reservation.lease_id != lease.lease_id
        || reservation.writer_epoch != lease.writer_epoch
        || &reservation.expected_cursor != expected_cursor
    {
        return Err(SessionContextCoordinatorError::IdempotencyMismatch);
    }
    Ok(())
}

fn validate_active_lease(
    state: &CoordinatorStateV1,
    lease: &ConversationWriterLeaseV1,
    now: i64,
) -> Result<(), SessionContextCoordinatorError> {
    if state.writer_epoch != lease.writer_epoch
        || state
            .active_writer
            .as_ref()
            .is_none_or(|active| active.lease_id != lease.lease_id)
        || lease.actor.authority_epochs != state.authority_epochs
    {
        return Err(SessionContextCoordinatorError::Fenced);
    }
    if state
        .active_writer
        .as_ref()
        .is_some_and(|active| active.expires_at_unix_ms <= now)
    {
        return Err(SessionContextCoordinatorError::Expired);
    }
    Ok(())
}

fn validate_active_reservation(
    state: &CoordinatorStateV1,
    reservation: &TurnReservationV1,
    now: i64,
) -> Result<(), SessionContextCoordinatorError> {
    let lease = state
        .active_writer
        .as_ref()
        .ok_or(SessionContextCoordinatorError::Fenced)?;
    if state.writer_epoch != reservation.writer_epoch
        || lease.lease_id != reservation.lease_id
        || state
            .active_reservation
            .as_ref()
            .is_none_or(|active| active.reservation_id != reservation.reservation_id)
    {
        return Err(SessionContextCoordinatorError::Fenced);
    }
    if lease.expires_at_unix_ms <= now
        || state
            .active_reservation
            .as_ref()
            .is_none_or(|active| active.expires_at_unix_ms <= now)
    {
        return Err(SessionContextCoordinatorError::Expired);
    }
    if lease.actor.authority_epochs != state.authority_epochs {
        return Err(SessionContextCoordinatorError::Fenced);
    }
    Ok(())
}

fn validate_delta_advance(
    head: Option<&SessionContextHeadV1>,
    reservation: &TurnReservationV1,
    delta: &CanonicalTurnDeltaV1,
) -> Result<(), SessionContextCoordinatorError> {
    if delta.schema_version != CANONICAL_TURN_DELTA_SCHEMA_VERSION
        || delta.completed_turn != reservation.reserved_turn
    {
        return Err(SessionContextCoordinatorError::Invalid(
            "turn delta does not match its reservation".into(),
        ));
    }
    let (base_journal_seq, base_conversation_seq, base_compaction_generation) =
        head.map_or((0, 0, 0), |head| {
            (
                head.cursor.journal_event_seq,
                head.cursor.conversation_seq,
                head.cursor.compaction_generation,
            )
        });
    let expected_compaction_generation = match delta.mode {
        CanonicalDeltaModeV1::Append => base_compaction_generation,
        CanonicalDeltaModeV1::Replace => base_compaction_generation.saturating_add(1),
    };
    if delta.journal_event_seq <= base_journal_seq
        || delta.conversation_seq != base_conversation_seq.saturating_add(1)
        || delta.compaction_generation != expected_compaction_generation
    {
        return Err(SessionContextCoordinatorError::Invalid(
            "turn delta must advance the reserved base monotonically".into(),
        ));
    }
    Ok(())
}

fn validate_manifest_advance(
    prior: Option<&SessionCursorV1>,
    node: &ContextManifestNodeV1,
) -> Result<(), SessionContextCoordinatorError> {
    let cursor = node.cursor();
    if let Some(prior) = prior {
        if cursor.completed_turn != prior.completed_turn.saturating_add(1)
            || cursor.conversation_seq != prior.conversation_seq.saturating_add(1)
            || cursor.journal_event_seq <= prior.journal_event_seq
            || if node.replaces_history {
                cursor.compaction_generation != prior.compaction_generation.saturating_add(1)
            } else {
                cursor.compaction_generation != prior.compaction_generation
            }
        {
            return Err(SessionContextCoordinatorError::NeedsRepair(
                "manifest cursor sequence is non-monotonic".into(),
            ));
        }
    } else if node.replaces_history {
        if cursor.completed_turn == 0
            || cursor.conversation_seq == 0
            || cursor.compaction_generation == 0
        {
            return Err(SessionContextCoordinatorError::NeedsRepair(
                "replacement manifest cursor is invalid".into(),
            ));
        }
    } else if cursor.completed_turn != 1
        || cursor.conversation_seq != 1
        || cursor.compaction_generation != 0
    {
        return Err(SessionContextCoordinatorError::NeedsRepair(
            "manifest genesis cursor is invalid".into(),
        ));
    }
    Ok(())
}

fn validate_commit_request(
    receipt: &CommitReceiptV1,
    reservation: &TurnReservationV1,
    delta: &CanonicalTurnDeltaV1,
) -> Result<(), SessionContextCoordinatorError> {
    if receipt.reservation_id != reservation.reservation_id
        || receipt.delta_hash != turn_delta_hash(delta)
    {
        return Err(SessionContextCoordinatorError::IdempotencyMismatch);
    }
    Ok(())
}

fn turn_delta_hash(delta: &CanonicalTurnDeltaV1) -> String {
    let mut digest = Sha256::new();
    digest.update(TURN_DELTA_HASH_DOMAIN);
    digest.update(delta.schema_version.to_be_bytes());
    digest.update(delta.completed_turn.to_be_bytes());
    digest.update(delta.journal_event_seq.to_be_bytes());
    digest.update(delta.conversation_seq.to_be_bytes());
    digest.update(delta.compaction_generation.to_be_bytes());
    if delta.mode == CanonicalDeltaModeV1::Replace {
        digest.update(b"replace\0");
    }
    hash_field(
        &mut digest,
        delta.config_version_id.as_deref().unwrap_or_default(),
    );
    digest.update((delta.logical_segments.len() as u64).to_be_bytes());
    for messages in &delta.logical_segments {
        hash_field(&mut digest, &canonical_conversation_root(messages));
        digest.update(canonical_conversation_serialized_len(messages).to_be_bytes());
        digest.update((messages.len() as u64).to_be_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn lease_request_hash(
    key: &SessionKeyV1,
    expected_cursor: Option<&SessionCursorV1>,
    actor: &ActorContextV1,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"astra.acquire-writer-request.v1\0");
    hash_field(&mut digest, &key.isolation_domain);
    hash_field(&mut digest, &key.owner_user_id);
    hash_field(&mut digest, &key.session_id);
    hash_field(&mut digest, &key.branch_id);
    hash_optional_cursor(&mut digest, expected_cursor);
    hash_field(&mut digest, &actor.actor_user_id);
    hash_field(&mut digest, &actor.actor_id);
    hash_field(&mut digest, actor.device_id.as_deref().unwrap_or_default());
    digest.update([actor.actor_kind as u8, actor.surface as u8]);
    digest.update(actor.authority_epochs.authorization_epoch.to_be_bytes());
    digest.update(actor.authority_epochs.device_trust_epoch.to_be_bytes());
    digest.update(actor.authority_epochs.permission_epoch.to_be_bytes());
    format!("{:x}", digest.finalize())
}

fn writer_transfer_request_hash(request: &WriterTransferRequestV1) -> String {
    let mut digest = Sha256::new();
    digest.update(b"astra.transfer-writer-request.v1\0");
    hash_field(&mut digest, &request.handoff_id);
    hash_field(&mut digest, &request.key.isolation_domain);
    hash_field(&mut digest, &request.key.owner_user_id);
    hash_field(&mut digest, &request.key.session_id);
    hash_field(&mut digest, &request.key.branch_id);
    digest.update([match request.mode {
        SessionHandoffModeV1::Graceful => 0,
        SessionHandoffModeV1::Forced => 1,
    }]);
    if let Some(source) = &request.source_lease {
        digest.update([1]);
        hash_field(&mut digest, &source.lease_id);
        digest.update(source.writer_epoch.to_be_bytes());
    } else {
        digest.update([0]);
    }
    match request.expected_writer_epoch {
        Some(epoch) => {
            digest.update([1]);
            digest.update(epoch.to_be_bytes());
        }
        None => digest.update([0]),
    }
    hash_optional_cursor(&mut digest, request.expected_cursor.as_ref());
    hash_field(&mut digest, &request.target_actor.actor_user_id);
    hash_field(&mut digest, &request.target_actor.actor_id);
    hash_field(
        &mut digest,
        request
            .target_actor
            .device_id
            .as_deref()
            .unwrap_or_default(),
    );
    digest.update([
        request.target_actor.actor_kind as u8,
        request.target_actor.surface as u8,
    ]);
    digest.update(
        request
            .target_actor
            .authority_epochs
            .authorization_epoch
            .to_be_bytes(),
    );
    digest.update(
        request
            .target_actor
            .authority_epochs
            .device_trust_epoch
            .to_be_bytes(),
    );
    digest.update(
        request
            .target_actor
            .authority_epochs
            .permission_epoch
            .to_be_bytes(),
    );
    hash_field(
        &mut digest,
        request
            .risk
            .unsynced_suffix_root
            .as_deref()
            .unwrap_or_default(),
    );
    let mut unknown_effects = request.risk.unknown_effect_invocation_ids.clone();
    unknown_effects.sort_unstable();
    for identity in unknown_effects {
        hash_field(&mut digest, &identity);
    }
    hash_field(
        &mut digest,
        request
            .risk
            .forced_authorization_id
            .as_deref()
            .unwrap_or_default(),
    );
    format!("{:x}", digest.finalize())
}

fn reservation_request_hash(
    lease: &ConversationWriterLeaseV1,
    expected_cursor: Option<&SessionCursorV1>,
) -> String {
    reservation_identity_hash(
        &lease.key,
        &lease.lease_id,
        lease.writer_epoch,
        expected_cursor,
    )
}

fn reservation_identity_hash(
    key: &SessionKeyV1,
    lease_id: &str,
    writer_epoch: u64,
    expected_cursor: Option<&SessionCursorV1>,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"astra.reserve-turn-request.v1\0");
    hash_field(&mut digest, &key.isolation_domain);
    hash_field(&mut digest, &key.owner_user_id);
    hash_field(&mut digest, &key.session_id);
    hash_field(&mut digest, &key.branch_id);
    hash_field(&mut digest, lease_id);
    digest.update(writer_epoch.to_be_bytes());
    hash_optional_cursor(&mut digest, expected_cursor);
    format!("{:x}", digest.finalize())
}

fn commit_request_hash(reservation: &TurnReservationV1, delta: &CanonicalTurnDeltaV1) -> String {
    let mut digest = Sha256::new();
    digest.update(b"astra.commit-turn-request.v1\0");
    hash_field(&mut digest, &reservation.key.isolation_domain);
    hash_field(&mut digest, &reservation.key.owner_user_id);
    hash_field(&mut digest, &reservation.key.session_id);
    hash_field(&mut digest, &reservation.key.branch_id);
    hash_field(&mut digest, &reservation.reservation_id);
    hash_field(&mut digest, &turn_delta_hash(delta));
    format!("{:x}", digest.finalize())
}

fn commit_receipt_request_hash(receipt: &CommitReceiptV1) -> String {
    let mut digest = Sha256::new();
    digest.update(b"astra.commit-turn-request.v1\0");
    hash_field(&mut digest, &receipt.reservation.key.isolation_domain);
    hash_field(&mut digest, &receipt.reservation.key.owner_user_id);
    hash_field(&mut digest, &receipt.reservation.key.session_id);
    hash_field(&mut digest, &receipt.reservation.key.branch_id);
    hash_field(&mut digest, &receipt.reservation_id);
    hash_field(&mut digest, &receipt.delta_hash);
    format!("{:x}", digest.finalize())
}

fn hash_optional_cursor(digest: &mut Sha256, cursor: Option<&SessionCursorV1>) {
    let Some(cursor) = cursor else {
        digest.update([0]);
        return;
    };
    digest.update([1]);
    hash_field(digest, &cursor.owner_id);
    hash_field(digest, &cursor.session_id);
    hash_field(digest, &cursor.branch_id);
    digest.update(cursor.completed_turn.to_be_bytes());
    digest.update(cursor.journal_event_seq.to_be_bytes());
    digest.update(cursor.conversation_seq.to_be_bytes());
    hash_field(digest, &cursor.canonical_root_hash);
    digest.update(cursor.projection_schema.to_be_bytes());
    digest.update(cursor.compaction_generation.to_be_bytes());
    hash_field(
        digest,
        cursor.config_version_id.as_deref().unwrap_or_default(),
    );
}

fn hash_receipt(operation: &str, idempotency_key: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(RECEIPT_HASH_DOMAIN);
    hash_field(&mut digest, operation);
    hash_field(&mut digest, idempotency_key);
    format!("{:x}", digest.finalize())
}

fn hash_field(digest: &mut Sha256, value: &str) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value.as_bytes());
}
