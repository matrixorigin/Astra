//! Explicit CLI-only canonical journal publish/import.
//!
//! Payload staging never mutates authority. Publishing advances the Server
//! head only from owner-scoped, signed-sync-outbox events already acknowledged
//! by the Server.

use std::{collections::HashMap, sync::Arc, time::Duration};

use astra_core::SharedPool;
use astra_turn_types::{
    ActorContextV1, CANONICAL_TURN_DELTA_SCHEMA_VERSION, CanonicalTurnDeltaV1,
    ContextManifestNodeV1, ConversationCommitV1, ConversationDeltaV1, ConversationSegmentV1,
    ConversationWriterLeaseV1, CoordinatorMutationV1, SessionContextHeadV1, SessionCursorV1,
    SessionKeyV1, canonical_conversation_root, validate_canonical_tool_pairing,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use sqlx::{MySql, QueryBuilder, Row, mysql::MySqlRow};
use thiserror::Error;

use crate::{
    AcquireWriterOutcome, ReserveTurnOutcome, SessionContextCoordinator,
    SessionContextCoordinatorError,
    session_journal::JournalEvent,
    sync_outbox::{sync_outbox_canonical_payload_hash, sync_outbox_stable_event_id},
};

const MAX_PUBLISH_EVENTS: usize = 256;
const MAX_SEGMENTS_PER_EVENT: usize = 256;
const MAX_WRITER_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublishJournalItemV1 {
    pub event_id: String,
    pub payload_hash: String,
    pub segment_hashes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublishSessionRequestV1 {
    pub idempotency_key: String,
    pub key: SessionKeyV1,
    pub actor: ActorContextV1,
    /// A controller lease obtained from attach, fork, handoff, or a prior
    /// publish. When present it must still be the exact active lease.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer_lease: Option<ConversationWriterLeaseV1>,
    pub items: Vec<PublishJournalItemV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublishSessionOutcomeV1 {
    /// The exact local journal cursor acknowledged by this import.
    pub acknowledged_local_cursor: SessionCursorV1,
    /// The Server's segmented-projection cursor for the same content.
    pub server_cursor: SessionCursorV1,
    pub published_events: u32,
    pub idempotent_events: u32,
    pub writer_lease: ConversationWriterLeaseV1,
}

#[derive(Debug, Error)]
pub enum SessionPublishError {
    #[error("invalid session publish request: {0}")]
    Invalid(String),
    #[error("one or more staged segments are missing")]
    MissingSegment,
    #[error("journal event is missing or was not ingested by the signed sync outbox")]
    UnacknowledgedJournal,
    #[error(
        "local root {local_root} cannot fast-forward Server root {server_root:?}; fork is required"
    )]
    ForkRequired {
        local_root: String,
        server_root: Option<String>,
    },
    #[error("canonical Server writer or head changed during publish")]
    Conflict,
    #[error("publish receipt requires repair: {0}")]
    NeedsRepair(String),
    #[error(transparent)]
    Coordinator(#[from] SessionContextCoordinatorError),
    #[error("publish database operation {operation} failed: {source}")]
    Database {
        operation: &'static str,
        #[source]
        source: sqlx::Error,
    },
    #[error("publish JSON for {entity} failed: {source}")]
    Json {
        entity: &'static str,
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Clone)]
pub struct DatabaseSessionPublishService {
    pool: SharedPool,
    coordinator: Arc<dyn SessionContextCoordinator>,
}

impl DatabaseSessionPublishService {
    pub fn new(pool: SharedPool, coordinator: Arc<dyn SessionContextCoordinator>) -> Self {
        Self { pool, coordinator }
    }

    /// Idempotently stage content-addressed payloads. This never installs a
    /// manifest or changes a writer epoch.
    pub async fn store_segments(
        &self,
        key: &SessionKeyV1,
        segments: &[ConversationSegmentV1],
    ) -> Result<(), SessionPublishError> {
        self.coordinator.store_segments(key, segments).await?;
        Ok(())
    }

    pub async fn publish(
        &self,
        request: &PublishSessionRequestV1,
        writer_ttl: Duration,
    ) -> Result<PublishSessionOutcomeV1, SessionPublishError> {
        validate_request(request, writer_ttl)?;
        let actual_head = self.coordinator.load_head(&request.key).await?;
        self.recover_committed_intent(&request.key, actual_head.as_ref())
            .await?;
        let anchor = self
            .load_anchor(
                &request.key,
                actual_head
                    .as_ref()
                    .map(|head| head.latest_manifest_root.as_str()),
            )
            .await?;
        let rows = self.load_journal_rows(request).await?;
        let verified = verify_journal_items(request, rows)?;
        let start = suffix_start(&verified, anchor.as_ref())?;

        let mut local_messages = match actual_head.as_ref() {
            Some(head) => self.coordinator.materialize(head).await?.messages,
            None => Vec::new(),
        };
        let mut local_root = canonical_conversation_root(&local_messages);
        if let Some(anchor) = &anchor {
            if anchor.local_cursor.canonical_root_hash != local_root {
                return Err(SessionPublishError::NeedsRepair(
                    "local/Server root mapping does not match materialized content".into(),
                ));
            }
            local_root.clone_from(&anchor.local_cursor.canonical_root_hash);
        } else if actual_head.is_some() {
            return Err(fork_required(
                verified
                    .get(start)
                    .or_else(|| verified.last())
                    .map(|item| item.commit.base_root_hash.clone())
                    .unwrap_or_default(),
                actual_head.as_ref(),
            ));
        }

        let mut simulated_head = actual_head.clone();
        let mut plans = Vec::with_capacity(verified.len().saturating_sub(start));
        for item in verified.iter().skip(start) {
            if item.commit.base_root_hash != local_root {
                return Err(fork_required(
                    item.commit.base_root_hash.clone(),
                    simulated_head.as_ref(),
                ));
            }
            let segments = self
                .coordinator
                .load_segments(&request.key, &item.request.segment_hashes)
                .await
                .map_err(|error| match error {
                    SessionContextCoordinatorError::SegmentNotFound => {
                        SessionPublishError::MissingSegment
                    }
                    other => other.into(),
                })?;
            let appended = segments
                .iter()
                .flat_map(|segment| segment.messages.iter().cloned())
                .collect::<Vec<_>>();
            match &item.commit.delta {
                ConversationDeltaV1::Append { messages } if messages == &appended => {}
                ConversationDeltaV1::Append { .. } => {
                    return Err(SessionPublishError::Invalid(
                        "staged segments do not equal the ordered journal delta".into(),
                    ));
                }
                // A replacement requires the replacement-manifest work in
                // Phase 6. Treating it as append would silently corrupt head
                // consistency, so it is an explicit child-lineage case.
                ConversationDeltaV1::Replace { .. } => {
                    return Err(fork_required(
                        item.commit.cursor.canonical_root_hash.clone(),
                        simulated_head.as_ref(),
                    ));
                }
            }
            local_messages.extend(appended);
            validate_canonical_tool_pairing(&local_messages)
                .map_err(|error| SessionPublishError::Invalid(error.to_string()))?;
            local_root = canonical_conversation_root(&local_messages);
            if item.commit.cursor.canonical_root_hash != local_root {
                return Err(SessionPublishError::Invalid(
                    "journal cursor root does not match its materialized deltas".into(),
                ));
            }

            let base = simulated_head.as_ref().map(|head| &head.cursor);
            let delta = CanonicalTurnDeltaV1 {
                schema_version: CANONICAL_TURN_DELTA_SCHEMA_VERSION,
                completed_turn: base.map_or(1, |cursor| cursor.completed_turn.saturating_add(1)),
                journal_event_seq: base
                    .map_or(1, |cursor| cursor.journal_event_seq.saturating_add(1)),
                conversation_seq: base
                    .map_or(1, |cursor| cursor.conversation_seq.saturating_add(1)),
                compaction_generation: base.map_or(0, |cursor| cursor.compaction_generation),
                config_version_id: item.commit.cursor.config_version_id.clone(),
                logical_segments: segments
                    .iter()
                    .map(|segment| segment.messages.clone())
                    .collect(),
            };
            let node = ContextManifestNodeV1::new(
                request.key.clone(),
                simulated_head
                    .as_ref()
                    .map(|head| head.latest_manifest_root.clone()),
                delta.completed_turn,
                delta.journal_event_seq,
                delta.conversation_seq,
                delta.compaction_generation,
                delta.config_version_id.clone(),
                segments
                    .iter()
                    .map(ConversationSegmentV1::reference)
                    .collect(),
            )
            .map_err(|error| SessionPublishError::Invalid(error.to_string()))?;
            let server_cursor = node.cursor();
            plans.push(PublishPlan {
                item: item.clone(),
                delta,
                server_base_manifest_root: simulated_head
                    .as_ref()
                    .map(|head| head.latest_manifest_root.clone()),
                server_cursor: server_cursor.clone(),
            });
            simulated_head = Some(simulated_next_head(
                &request.key,
                simulated_head.as_ref(),
                &segments,
                server_cursor,
            )?);
        }

        let expected_cursor = actual_head.as_ref().map(|head| &head.cursor);
        let active = self.coordinator.load_active_writer(&request.key).await?;
        let lease = match reusable_publish_lease(
            request.writer_lease.as_ref(),
            active.as_ref(),
            &request.actor,
        )? {
            Some(lease) => lease,
            None => {
                let acquire_idempotency_key = format!("publish:{}", request.idempotency_key);
                match self
                    .coordinator
                    .acquire_writer(
                        &request.key,
                        expected_cursor,
                        &request.actor,
                        writer_ttl,
                        &acquire_idempotency_key,
                    )
                    .await?
                {
                    AcquireWriterOutcome::Acquired(lease)
                    | AcquireWriterOutcome::AlreadyAcquired(lease) => lease,
                    AcquireWriterOutcome::Conflict { .. } => {
                        return Err(SessionPublishError::Conflict);
                    }
                }
            }
        };

        let mut current_cursor = expected_cursor.cloned();
        let mut published_events = 0_u32;
        for plan in &plans {
            self.prepare_receipt(&request.key, plan).await?;
            let reservation = match self
                .coordinator
                .reserve_turn(
                    &lease,
                    current_cursor.as_ref(),
                    writer_ttl,
                    &format!("publish:{}:reserve", plan.item.request.event_id),
                )
                .await?
            {
                ReserveTurnOutcome::Reserved(reservation)
                | ReserveTurnOutcome::AlreadyReserved(reservation) => reservation,
                ReserveTurnOutcome::Conflict { .. } => return Err(SessionPublishError::Conflict),
            };
            let committed = match self
                .coordinator
                .commit_turn(
                    &reservation,
                    plan.delta.clone(),
                    &format!("publish:{}:commit", plan.item.request.event_id),
                )
                .await?
            {
                CoordinatorMutationV1::Applied { cursor }
                | CoordinatorMutationV1::AlreadyApplied { cursor } => cursor,
                CoordinatorMutationV1::Conflict { .. } => {
                    return Err(SessionPublishError::Conflict);
                }
                CoordinatorMutationV1::NeedsRepair { reason, .. } => {
                    return Err(SessionPublishError::NeedsRepair(reason));
                }
            };
            if committed != plan.server_cursor {
                return Err(SessionPublishError::NeedsRepair(
                    "commit differs from the durable prepared publish intent".into(),
                ));
            }
            self.mark_receipt_committed(&request.key, &plan.item.request.event_id)
                .await?;
            current_cursor = Some(committed);
            published_events = published_events.saturating_add(1);
        }

        let server_cursor = current_cursor.ok_or_else(|| {
            SessionPublishError::NeedsRepair("publish completed without a Server head".into())
        })?;
        Ok(PublishSessionOutcomeV1 {
            acknowledged_local_cursor: verified
                .last()
                .expect("validated publish is non-empty")
                .commit
                .cursor
                .clone(),
            server_cursor,
            published_events,
            idempotent_events: u32::try_from(start).unwrap_or(u32::MAX),
            writer_lease: lease,
        })
    }

    async fn load_journal_rows(
        &self,
        request: &PublishSessionRequestV1,
    ) -> Result<HashMap<String, MySqlRow>, SessionPublishError> {
        let mut query = QueryBuilder::<MySql>::new(
            "SELECT event_id, content,
                    IFNULL(CAST(`metadata` AS CHAR), '{}') AS metadata_json
             FROM agent_events WHERE user_id = ",
        );
        query.push_bind(&request.key.owner_user_id);
        query.push(" AND session_id = ");
        query.push_bind(&request.key.session_id);
        query.push(" AND event_id IN (");
        {
            let mut ids = query.separated(", ");
            for item in &request.items {
                ids.push_bind(&item.event_id);
            }
        }
        query.push(")");
        let rows = query
            .build()
            .fetch_all(self.pool.get())
            .await
            .map_err(|source| database_error("load_journal_events", source))?;
        let mut result = HashMap::with_capacity(rows.len());
        for row in rows {
            let id = row
                .try_get::<String, _>("event_id")
                .map_err(|source| database_error("decode_event_id", source))?;
            result.insert(id, row);
        }
        Ok(result)
    }

    async fn recover_committed_intent(
        &self,
        key: &SessionKeyV1,
        head: Option<&SessionContextHeadV1>,
    ) -> Result<(), SessionPublishError> {
        let Some(head) = head else {
            return Ok(());
        };
        sqlx::query(
            "UPDATE session_publish_receipts
             SET publish_state = 'committed', updated_at = NOW(6)
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?
               AND server_manifest_root = ? AND publish_state = 'prepared'",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .bind(&head.latest_manifest_root)
        .execute(self.pool.get())
        .await
        .map_err(|source| database_error("recover_publish_intent", source))?;
        Ok(())
    }

    async fn load_anchor(
        &self,
        key: &SessionKeyV1,
        server_root: Option<&str>,
    ) -> Result<Option<PublishAnchor>, SessionPublishError> {
        let Some(server_root) = server_root else {
            return Ok(None);
        };
        sqlx::query(
            "SELECT local_cursor_json, server_cursor_json
             FROM session_publish_receipts
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?
               AND server_manifest_root = ? AND publish_state = 'committed'
             LIMIT 1",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .bind(server_root)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| database_error("load_publish_anchor", source))?
        .map(|row| {
            Ok(PublishAnchor {
                local_cursor: decode_json(&row, "local_cursor_json", "local_cursor")?,
                server_cursor: decode_json(&row, "server_cursor_json", "server_cursor")?,
            })
        })
        .transpose()
    }

    async fn prepare_receipt(
        &self,
        key: &SessionKeyV1,
        plan: &PublishPlan,
    ) -> Result<(), SessionPublishError> {
        let request_hash = plan_hash(plan)?;
        let result = sqlx::query(
            "INSERT IGNORE INTO session_publish_receipts
             (isolation_domain, owner_user_id, session_id, branch_id,
              local_event_id, payload_hash, request_hash, local_base_root,
              local_cursor_root, local_cursor_json, server_base_manifest_root,
              server_manifest_root, server_cursor_json, segment_hashes_json, publish_state)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'prepared')",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .bind(&plan.item.request.event_id)
        .bind(&plan.item.request.payload_hash)
        .bind(&request_hash)
        .bind(&plan.item.commit.base_root_hash)
        .bind(&plan.item.commit.cursor.canonical_root_hash)
        .bind(encode_json("local_cursor", &plan.item.commit.cursor)?)
        .bind(&plan.server_base_manifest_root)
        .bind(&plan.server_cursor.canonical_root_hash)
        .bind(encode_json("server_cursor", &plan.server_cursor)?)
        .bind(encode_json(
            "segment_hashes",
            &plan.item.request.segment_hashes,
        )?)
        .execute(self.pool.get())
        .await
        .map_err(|source| database_error("prepare_publish_receipt", source))?;
        if result.rows_affected() == 0 {
            let row = sqlx::query(
                "SELECT request_hash FROM session_publish_receipts
                 WHERE isolation_domain = ? AND owner_user_id = ?
                   AND session_id = ? AND branch_id = ? AND local_event_id = ?",
            )
            .bind(&key.isolation_domain)
            .bind(&key.owner_user_id)
            .bind(&key.session_id)
            .bind(&key.branch_id)
            .bind(&plan.item.request.event_id)
            .fetch_one(self.pool.get())
            .await
            .map_err(|source| database_error("verify_publish_receipt", source))?;
            let stored = row
                .try_get::<String, _>("request_hash")
                .map_err(|source| database_error("decode_publish_hash", source))?;
            if stored != request_hash {
                return Err(SessionPublishError::Invalid(
                    "journal identity was reused with different publish evidence".into(),
                ));
            }
        }
        Ok(())
    }

    async fn mark_receipt_committed(
        &self,
        key: &SessionKeyV1,
        event_id: &str,
    ) -> Result<(), SessionPublishError> {
        let result = sqlx::query(
            "UPDATE session_publish_receipts
             SET publish_state = 'committed', updated_at = NOW(6)
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ? AND local_event_id = ?
               AND publish_state IN ('prepared', 'committed')",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .bind(event_id)
        .execute(self.pool.get())
        .await
        .map_err(|source| database_error("commit_publish_receipt", source))?;
        if result.rows_affected() == 0 {
            return Err(SessionPublishError::NeedsRepair(
                "prepared publish receipt disappeared".into(),
            ));
        }
        Ok(())
    }
}

fn reusable_publish_lease(
    presented: Option<&ConversationWriterLeaseV1>,
    active: Option<&ConversationWriterLeaseV1>,
    actor: &ActorContextV1,
) -> Result<Option<ConversationWriterLeaseV1>, SessionPublishError> {
    match (presented, active) {
        (Some(presented), Some(active)) if presented == active && &active.actor == actor => {
            Ok(Some(active.clone()))
        }
        (Some(_), _) => Err(SessionPublishError::Conflict),
        (None, Some(active)) if &active.actor == actor => Ok(Some(active.clone())),
        (None, _) => Ok(None),
    }
}

#[derive(Clone)]
struct VerifiedItem {
    request: PublishJournalItemV1,
    commit: ConversationCommitV1,
}

struct PublishPlan {
    item: VerifiedItem,
    delta: CanonicalTurnDeltaV1,
    server_base_manifest_root: Option<String>,
    server_cursor: SessionCursorV1,
}

struct PublishAnchor {
    local_cursor: SessionCursorV1,
    server_cursor: SessionCursorV1,
}

fn verify_journal_items(
    request: &PublishSessionRequestV1,
    mut rows: HashMap<String, MySqlRow>,
) -> Result<Vec<VerifiedItem>, SessionPublishError> {
    let mut verified = Vec::with_capacity(request.items.len());
    for item in &request.items {
        let row = rows
            .remove(&item.event_id)
            .ok_or(SessionPublishError::UnacknowledgedJournal)?;
        let content = row
            .try_get::<String, _>("content")
            .map_err(|source| database_error("decode_journal_content", source))?;
        let metadata = row
            .try_get::<String, _>("metadata_json")
            .map_err(|source| database_error("decode_journal_metadata", source))?;
        let metadata: serde_json::Value =
            serde_json::from_str(&metadata).map_err(|source| SessionPublishError::Json {
                entity: "journal_metadata",
                source,
            })?;
        let stored_hash = metadata
            .pointer("/sync_outbox/payload_hash")
            .and_then(serde_json::Value::as_str)
            .ok_or(SessionPublishError::UnacknowledgedJournal)?;
        let payload: serde_json::Value =
            serde_json::from_str(&content).map_err(|source| SessionPublishError::Json {
                entity: "journal_event",
                source,
            })?;
        let computed_hash = sync_outbox_canonical_payload_hash(&payload);
        let event: JournalEvent =
            serde_json::from_value(payload).map_err(|source| SessionPublishError::Json {
                entity: "journal_event",
                source,
            })?;
        let stable_id = sync_outbox_stable_event_id(&event, &computed_hash)
            .map_err(|error| SessionPublishError::Invalid(error.to_string()))?;
        if stored_hash != computed_hash
            || item.payload_hash != computed_hash
            || stable_id != item.event_id
            || event.session_id.as_deref() != Some(request.key.session_id.as_str())
        {
            return Err(SessionPublishError::UnacknowledgedJournal);
        }
        let commit = event.conversation_commit.ok_or_else(|| {
            SessionPublishError::Invalid(
                "published journal event has no canonical conversation commit".into(),
            )
        })?;
        if commit.schema_version != astra_turn_types::CONVERSATION_COMMIT_SCHEMA_VERSION
            || commit.cursor.schema_version != astra_turn_types::SESSION_CURSOR_SCHEMA_VERSION
            || commit.cursor.projection_schema
                != astra_turn_types::CONVERSATION_PROJECTION_SCHEMA_VERSION
            || !request.key.validates_cursor(&commit.cursor)
        {
            return Err(SessionPublishError::Invalid(
                "journal commit schema or owner-scoped cursor is invalid".into(),
            ));
        }
        verified.push(VerifiedItem {
            request: item.clone(),
            commit,
        });
    }
    Ok(verified)
}

fn suffix_start(
    items: &[VerifiedItem],
    anchor: Option<&PublishAnchor>,
) -> Result<usize, SessionPublishError> {
    let base = anchor
        .map(|anchor| anchor.local_cursor.canonical_root_hash.clone())
        .unwrap_or_else(|| canonical_conversation_root(&[]));
    if items
        .last()
        .is_some_and(|item| item.commit.cursor.canonical_root_hash == base)
    {
        return Ok(items.len());
    }
    items
        .iter()
        .position(|item| item.commit.base_root_hash == base)
        .ok_or_else(|| SessionPublishError::ForkRequired {
            local_root: items
                .first()
                .map(|item| item.commit.base_root_hash.clone())
                .unwrap_or_default(),
            server_root: anchor.map(|anchor| anchor.server_cursor.canonical_root_hash.clone()),
        })
}

fn validate_request(
    request: &PublishSessionRequestV1,
    writer_ttl: Duration,
) -> Result<(), SessionPublishError> {
    request
        .key
        .validate()
        .map_err(|error| SessionPublishError::Invalid(error.to_string()))?;
    request
        .actor
        .validate_for(&request.key)
        .map_err(|error| SessionPublishError::Invalid(error.to_string()))?;
    if request.writer_lease.as_ref().is_some_and(|lease| {
        lease.schema_version != astra_turn_types::SESSION_COORDINATION_SCHEMA_VERSION
            || lease.key != request.key
            || lease.actor != request.actor
            || lease.lease_id.is_empty()
            || lease.lease_id.len() > 512
    }) {
        return Err(SessionPublishError::Invalid(
            "presented writer lease does not match the publish actor and branch".into(),
        ));
    }
    if request.idempotency_key.is_empty()
        || request.idempotency_key.len() > 384
        || request.idempotency_key.chars().any(char::is_control)
        || writer_ttl.is_zero()
        || writer_ttl > MAX_WRITER_TTL
        || request.items.is_empty()
        || request.items.len() > MAX_PUBLISH_EVENTS
    {
        return Err(SessionPublishError::Invalid(
            "publish identity, TTL, or event batch is invalid".into(),
        ));
    }
    let mut ids = std::collections::HashSet::with_capacity(request.items.len());
    for item in &request.items {
        if item.event_id.is_empty()
            || item.event_id.len() > 128
            || item.event_id.chars().any(char::is_control)
            || item.payload_hash.is_empty()
            || item.payload_hash.len() > 80
            || item.segment_hashes.is_empty()
            || item.segment_hashes.len() > MAX_SEGMENTS_PER_EVENT
            || !ids.insert(item.event_id.as_str())
        {
            return Err(SessionPublishError::Invalid(
                "journal identity, payload hash, or segment batch is invalid".into(),
            ));
        }
    }
    Ok(())
}

fn simulated_next_head(
    key: &SessionKeyV1,
    base: Option<&SessionContextHeadV1>,
    segments: &[ConversationSegmentV1],
    cursor: SessionCursorV1,
) -> Result<SessionContextHeadV1, SessionPublishError> {
    let bytes = segments.iter().try_fold(
        base.map_or(0, |head| head.total_canonical_bytes),
        |total, segment| total.checked_add(segment.canonical_bytes),
    );
    let messages = segments.iter().try_fold(
        base.map_or(0, |head| head.total_message_count),
        |total, segment| total.checked_add(u64::from(segment.message_count)),
    );
    Ok(SessionContextHeadV1 {
        schema_version: astra_turn_types::SESSION_COORDINATION_SCHEMA_VERSION,
        key: key.clone(),
        latest_manifest_root: cursor.canonical_root_hash.clone(),
        cursor,
        total_canonical_bytes: bytes
            .ok_or_else(|| SessionPublishError::Invalid("canonical byte total overflow".into()))?,
        total_message_count: messages
            .ok_or_else(|| SessionPublishError::Invalid("message total overflow".into()))?,
        writer_epoch: base.map_or(0, |head| head.writer_epoch),
    })
}

fn fork_required(local_root: String, head: Option<&SessionContextHeadV1>) -> SessionPublishError {
    SessionPublishError::ForkRequired {
        local_root,
        server_root: head.map(|head| head.cursor.canonical_root_hash.clone()),
    }
}

fn plan_hash(plan: &PublishPlan) -> Result<String, SessionPublishError> {
    let encoded = serde_json::to_vec(&(
        &plan.item.request,
        &plan.item.commit,
        &plan.server_base_manifest_root,
        &plan.server_cursor,
    ))
    .map_err(|source| SessionPublishError::Json {
        entity: "publish_hash",
        source,
    })?;
    let mut hash = Sha256::new();
    hash.update(b"astra.session-publish-item.v1\0");
    hash.update(encoded);
    Ok(format!("{:x}", hash.finalize()))
}

fn decode_json<T: DeserializeOwned>(
    row: &MySqlRow,
    column: &'static str,
    entity: &'static str,
) -> Result<T, SessionPublishError> {
    let json = row
        .try_get::<String, _>(column)
        .map_err(|source| database_error("decode_publish_json", source))?;
    serde_json::from_str(&json).map_err(|source| SessionPublishError::Json { entity, source })
}

fn encode_json(
    entity: &'static str,
    value: &impl Serialize,
) -> Result<String, SessionPublishError> {
    serde_json::to_string(value).map_err(|source| SessionPublishError::Json { entity, source })
}

fn database_error(operation: &'static str, source: sqlx::Error) -> SessionPublishError {
    SessionPublishError::Database { operation, source }
}

#[cfg(test)]
mod tests {
    use crate::{
        DatabaseEventService, DatabaseSessionContextCoordinator, EventCreateRequestData,
        EventIngestionSource, EventService,
    };
    use astra_turn_types::{
        ActorKindV1, AuthorityEpochsV1, CONVERSATION_COMMIT_SCHEMA_VERSION,
        CONVERSATION_PROJECTION_SCHEMA_VERSION, ConversationDeltaV1,
        SEGMENTED_CONVERSATION_PROJECTION_SCHEMA_VERSION, SESSION_CURSOR_SCHEMA_VERSION,
        SessionSurfaceV1,
    };
    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    static PUBLISH_DB: tokio::sync::OnceCell<SharedPool> = tokio::sync::OnceCell::const_new();

    fn item(base: &str, messages: Vec<serde_json::Value>, sequence: u64) -> VerifiedItem {
        VerifiedItem {
            request: PublishJournalItemV1 {
                event_id: format!("event-{sequence}"),
                payload_hash: format!("hash-{sequence}"),
                segment_hashes: vec![format!("{sequence:064x}")],
            },
            commit: ConversationCommitV1 {
                schema_version: CONVERSATION_COMMIT_SCHEMA_VERSION,
                base_root_hash: base.into(),
                cursor: SessionCursorV1 {
                    schema_version: SESSION_CURSOR_SCHEMA_VERSION,
                    owner_id: "owner-a".into(),
                    session_id: "session-a".into(),
                    branch_id: "main".into(),
                    completed_turn: sequence as u32,
                    journal_event_seq: sequence,
                    conversation_seq: sequence,
                    canonical_root_hash: canonical_conversation_root(&messages),
                    projection_schema: CONVERSATION_PROJECTION_SCHEMA_VERSION,
                    compaction_generation: 0,
                    config_version_id: None,
                },
                delta: ConversationDeltaV1::Append { messages },
            },
        }
    }

    #[test]
    fn exact_root_selects_suffix_and_equal_head_is_idempotent() {
        let first = item(
            &canonical_conversation_root(&[]),
            vec![json!({"role":"user","content":"same"})],
            1,
        );
        let second = item(
            &first.commit.cursor.canonical_root_hash,
            vec![json!({"role":"assistant","content":"same"})],
            2,
        );
        let anchor = PublishAnchor {
            local_cursor: first.commit.cursor.clone(),
            server_cursor: SessionCursorV1 {
                projection_schema: SEGMENTED_CONVERSATION_PROJECTION_SCHEMA_VERSION,
                canonical_root_hash: "a".repeat(64),
                ..first.commit.cursor.clone()
            },
        };
        assert_eq!(
            suffix_start(&[first.clone(), second.clone()], Some(&anchor)).unwrap(),
            1
        );
        let equal = PublishAnchor {
            local_cursor: second.commit.cursor.clone(),
            server_cursor: anchor.server_cursor,
        };
        assert_eq!(suffix_start(&[first, second], Some(&equal)).unwrap(), 2);
    }

    fn controller_lease(actor: ActorContextV1, lease_id: &str) -> ConversationWriterLeaseV1 {
        ConversationWriterLeaseV1 {
            schema_version: astra_turn_types::SESSION_COORDINATION_SCHEMA_VERSION,
            key: SessionKeyV1::owner_session("server", "owner-a", "session-a", "main"),
            lease_id: lease_id.into(),
            writer_epoch: 3,
            actor,
            expected_cursor: None,
            acquired_at_unix_ms: 1_000,
            expires_at_unix_ms: 60_000,
            idempotency_key: "original-operation".into(),
        }
    }

    #[test]
    fn controller_lease_is_reused_across_distinct_publish_operations() {
        let actor = ActorContextV1::owner_user(
            "owner-a",
            "device-a",
            ActorKindV1::Cli,
            SessionSurfaceV1::Cli,
            Some("device-a".into()),
            AuthorityEpochsV1::default(),
        );
        let active = controller_lease(actor.clone(), "lease-current");

        assert_eq!(
            reusable_publish_lease(None, Some(&active), &actor).unwrap(),
            Some(active)
        );
    }

    #[test]
    fn stale_presented_lease_fails_closed_after_handoff() {
        let actor = ActorContextV1::owner_user(
            "owner-a",
            "device-a",
            ActorKindV1::Cli,
            SessionSurfaceV1::Cli,
            Some("device-a".into()),
            AuthorityEpochsV1::default(),
        );
        let stale = controller_lease(actor.clone(), "lease-before-handoff");
        let mut active = controller_lease(actor.clone(), "lease-after-handoff");
        active.writer_epoch += 1;

        assert!(matches!(
            reusable_publish_lease(Some(&stale), Some(&active), &actor),
            Err(SessionPublishError::Conflict)
        ));
    }

    async fn setup_publish_db_it() -> (SharedPool, astra_core::MatrixOneSettings) {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
        );
        let settings = astra_core::MatrixOneSettings::from_env();
        let pool = PUBLISH_DB
            .get_or_init(|| async {
                let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
                    .unwrap_or_else(|_| "mysql".to_owned());
                crate::storage::ensure_core_schema(&settings, &catalog)
                    .await
                    .expect("ensure core schema");
                SharedPool::new(&settings).await.expect("shared pool")
            })
            .await
            .clone();
        (pool, settings)
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn signed_journal_publish_is_resumable_and_owner_isolated() {
        let (pool, settings) = setup_publish_db_it().await;
        let suffix = Uuid::new_v4();
        let owner = format!("publish-owner-{suffix}");
        let session = format!("publish-session-{suffix}");
        let key = SessionKeyV1::owner_session("server", &owner, &session, "main");
        let coordinator: Arc<dyn SessionContextCoordinator> =
            Arc::new(DatabaseSessionContextCoordinator::new(pool.clone()));
        let service = DatabaseSessionPublishService::new(pool.clone(), coordinator.clone());
        let messages = vec![
            json!({"role":"user","content":"inspect"}),
            json!({"role":"assistant","tool_calls":[
                {"id":"call-a","type":"function","function":{"name":"inspect","arguments":"{}"}}
            ]}),
            json!({"role":"tool","tool_call_id":"call-a","content":"ok"}),
            json!({"role":"assistant","content":"done"}),
        ];
        let commit = ConversationCommitV1 {
            schema_version: CONVERSATION_COMMIT_SCHEMA_VERSION,
            base_root_hash: canonical_conversation_root(&[]),
            cursor: SessionCursorV1 {
                schema_version: SESSION_CURSOR_SCHEMA_VERSION,
                owner_id: owner.clone(),
                session_id: session.clone(),
                branch_id: "main".into(),
                completed_turn: 1,
                journal_event_seq: 1,
                conversation_seq: 1,
                canonical_root_hash: canonical_conversation_root(&messages),
                projection_schema: CONVERSATION_PROJECTION_SCHEMA_VERSION,
                compaction_generation: 0,
                config_version_id: None,
            },
            delta: ConversationDeltaV1::Append {
                messages: messages.clone(),
            },
        };
        let event = crate::session_journal::JournalEvent::turn(
            Some(&session),
            1,
            Some("test-model"),
            "inspect",
            "done",
            1,
            10,
            5,
            20,
        )
        .with_conversation_commit(commit.clone());
        let payload = serde_json::to_value(&event).unwrap();
        let payload_hash = sync_outbox_canonical_payload_hash(&payload);
        let event_id = sync_outbox_stable_event_id(&event, &payload_hash).unwrap();
        let event_type = serde_json::to_value(&event.event_type)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned();
        DatabaseEventService::new(settings)
            .with_pool(pool.clone())
            .create_event(
                owner.clone(),
                EventCreateRequestData {
                    ingestion_source: EventIngestionSource::SyncOutbox,
                    event_id: Some(event_id.clone()),
                    session_id: session.clone(),
                    event_type,
                    content: serde_json::to_string(&event).unwrap(),
                    agent_id: None,
                    agent_version: None,
                    parent_event_id: None,
                    parent_event_ids: None,
                    causal_chain_id: None,
                    metadata: Some(json!({"sync_outbox":{"payload_hash":payload_hash}})),
                },
            )
            .await
            .expect("server acknowledges signed-outbox-shaped event");

        let segment = ConversationSegmentV1::new(&key, messages).unwrap();
        service
            .store_segments(&key, std::slice::from_ref(&segment))
            .await
            .expect("stage segment");
        assert!(coordinator.load_head(&key).await.unwrap().is_none());
        let request = PublishSessionRequestV1 {
            idempotency_key: "publish-lineage".into(),
            key: key.clone(),
            actor: ActorContextV1::owner_user(
                &owner,
                "device-a",
                ActorKindV1::Cli,
                SessionSurfaceV1::Cli,
                Some("device-a".into()),
                AuthorityEpochsV1::default(),
            ),
            writer_lease: None,
            items: vec![PublishJournalItemV1 {
                event_id: event_id.clone(),
                payload_hash: payload_hash.clone(),
                segment_hashes: vec![segment.segment_hash.clone()],
            }],
        };
        let first = service
            .publish(&request, Duration::from_secs(60))
            .await
            .expect("publish");
        assert_eq!(first.published_events, 1);
        assert_eq!(first.acknowledged_local_cursor, commit.cursor);
        assert_ne!(
            first.acknowledged_local_cursor.projection_schema,
            first.server_cursor.projection_schema
        );
        let retry = service
            .publish(&request, Duration::from_secs(60))
            .await
            .expect("exact retry");
        assert_eq!(retry.published_events, 0);
        assert_eq!(retry.idempotent_events, 1);
        assert_eq!(retry.server_cursor, first.server_cursor);
        assert_eq!(retry.writer_lease, first.writer_lease);

        let other_key =
            SessionKeyV1::owner_session("server", format!("{owner}-other"), &session, "main");
        assert!(matches!(
            service
                .store_segments(&other_key, std::slice::from_ref(&segment))
                .await,
            Err(SessionPublishError::Coordinator(
                SessionContextCoordinatorError::Invalid(_)
            ))
        ));

        coordinator
            .release_writer(&first.writer_lease)
            .await
            .expect("release writer");
        for table in [
            "session_publish_receipts",
            "session_context_authority_events",
            "session_context_operation_receipts",
            "conversation_manifest_nodes",
            "session_context_heads",
        ] {
            sqlx::query(&format!(
                "DELETE FROM {table} WHERE isolation_domain = ? AND owner_user_id = ? AND session_id = ?"
            ))
            .bind(&key.isolation_domain)
            .bind(&key.owner_user_id)
            .bind(&key.session_id)
            .execute(pool.get())
            .await
            .unwrap_or_else(|error| panic!("cleanup {table}: {error}"));
        }
        sqlx::query(
            "DELETE FROM conversation_segments WHERE isolation_domain = ? AND owner_user_id = ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .execute(pool.get())
        .await
        .expect("cleanup segments");
        sqlx::query("DELETE FROM agent_events WHERE user_id = ? AND session_id = ?")
            .bind(&owner)
            .bind(&session)
            .execute(pool.get())
            .await
            .expect("cleanup events");
        sqlx::query("DELETE FROM agent_sessions WHERE user_id = ? AND session_id = ?")
            .bind(&owner)
            .bind(&session)
            .execute(pool.get())
            .await
            .expect("cleanup session");
    }
}
