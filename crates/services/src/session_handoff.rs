//! Durable cross-placement session attachment and handoff coordination.
//!
//! Attachments are readers. A handoff becomes a writer only through the
//! canonical [`SessionContextCoordinator`] fence. The durable `Fencing`
//! transition is written before that fence, so a crash can retry the same
//! idempotent transfer without ever reporting authority that was not installed.

use std::{sync::Arc, time::Duration};

use astra_core::SharedPool;
use astra_turn_types::{
    ActorContextV1, ConversationWriterLeaseV1, HandoffOperationWatermarksV1, HandoffRiskEvidenceV1,
    ManifestDeltaV1, SESSION_ATTACHMENT_SCHEMA_VERSION, SESSION_HANDOFF_SCHEMA_VERSION,
    SessionAttachmentModeV1, SessionAttachmentV1, SessionContextHeadV1, SessionCursorV1,
    SessionHandoffModeV1, SessionHandoffRecordV1, SessionHandoffStateV1, SessionKeyV1,
    SessionPlacementV1, WorkspaceHandoffEvidenceV1,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use sqlx::{MySql, Row, Transaction};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    SessionContextCoordinator, SessionContextCoordinatorError, TransferWriterOutcome,
    WriterTransferRequestV1,
};

const MAX_ATTACHMENT_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_HANDOFF_DEADLINE: Duration = Duration::from_secs(60 * 60);
const MAX_IDEMPOTENCY_BYTES: usize = 512;

#[derive(Debug, Error)]
pub enum SessionHandoffError {
    #[error("invalid handoff request: {0}")]
    Invalid(String),
    #[error("session handoff or attachment was not found")]
    NotFound,
    #[error("another handoff is active: {active_handoff_id}")]
    ActiveHandoffConflict { active_handoff_id: String },
    #[error(
        "handoff state conflict: expected {expected_state:?}/{expected_seq}, observed {observed_state:?}/{observed_seq}"
    )]
    StateConflict {
        expected_state: SessionHandoffStateV1,
        expected_seq: u64,
        observed_state: SessionHandoffStateV1,
        observed_seq: u64,
    },
    #[error("handoff idempotency key was reused for a different request")]
    IdempotencyMismatch,
    #[error("handoff deadline expired")]
    DeadlineExpired,
    #[error("session attachment expired")]
    AttachmentExpired,
    #[error(
        "local manifest {observed_manifest_root} diverges from Server head; fork is required (quarantine {quarantine_id})"
    )]
    ForkRequired {
        quarantine_id: String,
        observed_manifest_root: String,
        current_manifest_root: Option<String>,
    },
    #[error("handoff state requires repair: {0}")]
    NeedsRepair(String),
    #[error(transparent)]
    Coordinator(#[from] SessionContextCoordinatorError),
    #[error("handoff database operation {operation} failed: {source}")]
    Database {
        operation: &'static str,
        #[source]
        source: sqlx::Error,
    },
    #[error("handoff JSON for {entity} failed: {source}")]
    Json {
        entity: &'static str,
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachSessionRequestV1 {
    pub idempotency_key: String,
    pub key: SessionKeyV1,
    pub actor: ActorContextV1,
    pub placement: SessionPlacementV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_manifest_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceHandoffEvidenceV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachSessionOutcomeV1 {
    pub attachment: SessionAttachmentV1,
    pub delta: ManifestDeltaV1,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RequestSessionHandoffV1 {
    pub idempotency_key: String,
    pub key: SessionKeyV1,
    pub mode: SessionHandoffModeV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_attachment_id: Option<String>,
    pub to_attachment_id: String,
    pub from_placement: SessionPlacementV1,
    pub to_placement: SessionPlacementV1,
    pub target_actor: ActorContextV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_cursor: Option<SessionCursorV1>,
    pub authority_epochs: astra_turn_types::AuthorityEpochsV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceHandoffEvidenceV1>,
    #[serde(default)]
    pub watermarks: HandoffOperationWatermarksV1,
    #[serde(default)]
    pub risk: HandoffRiskEvidenceV1,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HandoffTransitionPatchV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_cursor: Option<SessionCursorV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_writer_epoch: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceHandoffEvidenceV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watermarks: Option<HandoffOperationWatermarksV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<HandoffRiskEvidenceV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransitionSessionHandoffV1 {
    pub idempotency_key: String,
    pub key: SessionKeyV1,
    pub handoff_id: String,
    pub expected_state: SessionHandoffStateV1,
    pub expected_transition_seq: u64,
    pub next_state: SessionHandoffStateV1,
    #[serde(default)]
    pub patch: HandoffTransitionPatchV1,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FenceSessionWriterOutcomeV1 {
    pub handoff: SessionHandoffRecordV1,
    pub lease: Option<ConversationWriterLeaseV1>,
    pub transfer: Option<TransferWriterOutcome>,
}

#[derive(Clone)]
pub struct DatabaseSessionHandoffService {
    pool: SharedPool,
    coordinator: Arc<dyn SessionContextCoordinator>,
}

impl DatabaseSessionHandoffService {
    pub fn new(pool: SharedPool, coordinator: Arc<dyn SessionContextCoordinator>) -> Self {
        Self { pool, coordinator }
    }

    pub async fn list_handoff_events(
        &self,
        key: &SessionKeyV1,
        limit: u32,
    ) -> Result<Vec<SessionHandoffEventV1>, SessionHandoffError> {
        key.validate()
            .map_err(|error| SessionHandoffError::Invalid(error.to_string()))?;
        let rows = sqlx::query(
            "SELECT event_json, created_at
             FROM session_handoff_events
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?
             ORDER BY created_at DESC, handoff_id DESC, transition_seq DESC
             LIMIT ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .bind(i64::from(limit.clamp(1, 500)))
        .fetch_all(self.pool.get())
        .await
        .map_err(|source| database_error("list_handoff_events", source))?;
        rows.into_iter()
            .map(|row| {
                let transition: HandoffTransitionEventV1 =
                    decode_json_row(&row, "event_json", "handoff event")?;
                Ok(SessionHandoffEventV1 {
                    from_state: transition.from_state,
                    record: transition.record,
                    created_at: row
                        .try_get("created_at")
                        .map_err(|source| database_error("decode_handoff_event_time", source))?,
                })
            })
            .collect()
    }

    pub async fn attach_read_only(
        &self,
        request: &AttachSessionRequestV1,
        ttl: Duration,
    ) -> Result<AttachSessionOutcomeV1, SessionHandoffError> {
        validate_attach_request(request)?;
        validate_duration(ttl, MAX_ATTACHMENT_TTL, "attachment TTL")?;
        let request_hash = stable_hash(b"astra.attach-session.v1\0", request)?;
        let idempotency_hash = identity_hash("attach", &request.idempotency_key);
        let delta = match self
            .coordinator
            .load_manifest_delta(&request.key, request.after_manifest_root.as_deref())
            .await
        {
            Ok(delta) => delta,
            Err(SessionContextCoordinatorError::DivergentManifest) => {
                let observed_manifest_root = request
                    .after_manifest_root
                    .as_deref()
                    .expect("divergence requires an observed manifest root");
                let current_head = self.coordinator.load_head(&request.key).await?;
                let quarantine_id = quarantine_divergent_attachment(
                    self.pool.get(),
                    &request.key,
                    observed_manifest_root,
                    current_head.as_ref(),
                    &idempotency_hash,
                    &request_hash,
                )
                .await?;
                return Err(SessionHandoffError::ForkRequired {
                    quarantine_id,
                    observed_manifest_root: observed_manifest_root.to_owned(),
                    current_manifest_root: current_head.map(|head| head.latest_manifest_root),
                });
            }
            Err(error) => return Err(error.into()),
        };
        let mut tx = self.begin("begin_attach").await?;
        let now = database_now_ms(&mut tx).await?;

        if let Some(row) = sqlx::query(
            "SELECT request_hash, attachment_json
             FROM session_attachments
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ? AND idempotency_hash = ?
             FOR UPDATE",
        )
        .bind(&request.key.isolation_domain)
        .bind(&request.key.owner_user_id)
        .bind(&request.key.session_id)
        .bind(&request.key.branch_id)
        .bind(&idempotency_hash)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("load_attach_retry", source))?
        {
            verify_request_hash(&row, &request_hash)?;
            let mut attachment: SessionAttachmentV1 =
                decode_json_row(&row, "attachment_json", "attachment")?;
            validate_stored_attachment(&attachment, &request.key)?;
            if attachment.expires_at_unix_ms <= now {
                return Err(SessionHandoffError::AttachmentExpired);
            }
            install_observation(&mut attachment, &delta);
            sqlx::query(
                "UPDATE session_attachments
                 SET observed_manifest_root = ?, attachment_json = ?, updated_at = NOW(6)
                 WHERE isolation_domain = ? AND owner_user_id = ?
                   AND session_id = ? AND branch_id = ? AND attachment_id = ?",
            )
            .bind(attachment.observed_manifest_root.as_deref())
            .bind(to_json("attachment", &attachment)?)
            .bind(&request.key.isolation_domain)
            .bind(&request.key.owner_user_id)
            .bind(&request.key.session_id)
            .bind(&request.key.branch_id)
            .bind(&attachment.attachment_id)
            .execute(&mut *tx)
            .await
            .map_err(|source| database_error("refresh_attach_observation", source))?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_attach_retry", source))?;
            return Ok(AttachSessionOutcomeV1 { attachment, delta });
        }

        ensure_slot(&mut tx, &request.key).await?;
        let next_epoch = lock_and_increment_attachment_epoch(&mut tx, &request.key).await?;
        let expires_at_unix_ms = checked_expiry(now, ttl)?;
        let mut attachment = SessionAttachmentV1 {
            schema_version: SESSION_ATTACHMENT_SCHEMA_VERSION,
            attachment_id: Uuid::new_v4().to_string(),
            attachment_epoch: next_epoch,
            key: request.key.clone(),
            actor: request.actor.clone(),
            mode: SessionAttachmentModeV1::ReadOnly,
            placement: request.placement,
            observed_cursor: None,
            observed_manifest_root: None,
            workspace: request.workspace.clone(),
            attached_at_unix_ms: now,
            expires_at_unix_ms,
        };
        install_observation(&mut attachment, &delta);
        attachment
            .validate()
            .map_err(|error| SessionHandoffError::Invalid(error.to_string()))?;
        sqlx::query(
            "INSERT INTO session_attachments
             (isolation_domain, owner_user_id, session_id, branch_id,
              attachment_id, attachment_epoch, idempotency_hash, request_hash,
              actor_id, mode, placement, observed_manifest_root,
              attachment_json, expires_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&request.key.isolation_domain)
        .bind(&request.key.owner_user_id)
        .bind(&request.key.session_id)
        .bind(&request.key.branch_id)
        .bind(&attachment.attachment_id)
        .bind(i64_from_u64(
            "attachment epoch",
            attachment.attachment_epoch,
        )?)
        .bind(idempotency_hash)
        .bind(request_hash)
        .bind(&attachment.actor.actor_id)
        .bind(attachment_mode(attachment.mode))
        .bind(placement_name(attachment.placement))
        .bind(attachment.observed_manifest_root.as_deref())
        .bind(to_json("attachment", &attachment)?)
        .bind(attachment.expires_at_unix_ms)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("insert_attachment", source))?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_attach", source))?;
        Ok(AttachSessionOutcomeV1 { attachment, delta })
    }

    pub async fn request_handoff(
        &self,
        request: &RequestSessionHandoffV1,
        deadline: Duration,
    ) -> Result<SessionHandoffRecordV1, SessionHandoffError> {
        validate_handoff_request(request)?;
        validate_duration(deadline, MAX_HANDOFF_DEADLINE, "handoff deadline")?;
        let request_hash = stable_hash(b"astra.request-session-handoff.v1\0", request)?;
        let idempotency_hash = identity_hash("handoff", &request.idempotency_key);
        let mut tx = self.begin("begin_request_handoff").await?;
        let now = database_now_ms(&mut tx).await?;
        ensure_slot(&mut tx, &request.key).await?;
        let active_handoff_id = lock_active_handoff(&mut tx, &request.key).await?;

        if let Some(row) = sqlx::query(
            "SELECT request_hash, record_json FROM session_handoffs
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ? AND idempotency_hash = ?
             FOR UPDATE",
        )
        .bind(&request.key.isolation_domain)
        .bind(&request.key.owner_user_id)
        .bind(&request.key.session_id)
        .bind(&request.key.branch_id)
        .bind(&idempotency_hash)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("load_handoff_retry", source))?
        {
            verify_request_hash(&row, &request_hash)?;
            let record: SessionHandoffRecordV1 = decode_json_row(&row, "record_json", "handoff")?;
            validate_stored_handoff(&record, &request.key)?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_handoff_retry", source))?;
            return Ok(record);
        }
        if let Some(active_handoff_id) = active_handoff_id {
            return Err(SessionHandoffError::ActiveHandoffConflict { active_handoff_id });
        }

        let target = lock_attachment(&mut tx, &request.key, &request.to_attachment_id).await?;
        if target.expires_at_unix_ms <= now
            || target.actor != request.target_actor
            || target.placement != request.to_placement
        {
            return Err(SessionHandoffError::Invalid(
                "target attachment is expired or does not match the target actor/placement".into(),
            ));
        }
        let mut workspace_mismatch = target.workspace != request.workspace;
        if let Some(source_id) = &request.from_attachment_id {
            let source = lock_attachment(&mut tx, &request.key, source_id).await?;
            if source.expires_at_unix_ms <= now || source.placement != request.from_placement {
                return Err(SessionHandoffError::Invalid(
                    "source attachment is expired or placement-mismatched".into(),
                ));
            }
            workspace_mismatch |= source.workspace != request.workspace;
        }

        let handoff_id = Uuid::new_v4().to_string();
        let record = SessionHandoffRecordV1 {
            schema_version: SESSION_HANDOFF_SCHEMA_VERSION,
            handoff_id: handoff_id.clone(),
            idempotency_key: request.idempotency_key.clone(),
            key: request.key.clone(),
            state: if workspace_mismatch {
                SessionHandoffStateV1::Blocked
            } else {
                SessionHandoffStateV1::Requested
            },
            mode: request.mode,
            from_attachment_id: request.from_attachment_id.clone(),
            to_attachment_id: Some(request.to_attachment_id.clone()),
            from_placement: request.from_placement,
            to_placement: request.to_placement,
            target_actor: request.target_actor.clone(),
            base_cursor: request.base_cursor.clone(),
            target_writer_epoch: None,
            authority_epochs: request.authority_epochs,
            workspace: request.workspace.clone(),
            watermarks: request.watermarks.clone(),
            risk: request.risk.clone(),
            reason: request.reason.clone(),
            status_detail: workspace_mismatch.then(|| "workspace_evidence_mismatch".to_owned()),
            blocked_from: workspace_mismatch.then_some(SessionHandoffStateV1::Validating),
            deadline_unix_ms: checked_expiry(now, deadline)?,
            created_at_unix_ms: now,
            updated_at_unix_ms: now,
            transition_seq: 1,
        };
        record
            .validate()
            .map_err(|error| SessionHandoffError::Invalid(error.to_string()))?;
        insert_handoff(&mut tx, &record, &idempotency_hash, &request_hash).await?;
        insert_transition_event(&mut tx, None, &record, &request_hash).await?;
        sqlx::query(
            "UPDATE session_handoff_slots
             SET active_handoff_id = ?, updated_at = NOW(6)
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?",
        )
        .bind(&handoff_id)
        .bind(&request.key.isolation_domain)
        .bind(&request.key.owner_user_id)
        .bind(&request.key.session_id)
        .bind(&request.key.branch_id)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("activate_handoff_slot", source))?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_request_handoff", source))?;
        Ok(record)
    }

    pub async fn load_handoff(
        &self,
        key: &SessionKeyV1,
        handoff_id: &str,
    ) -> Result<SessionHandoffRecordV1, SessionHandoffError> {
        validate_key_and_id(key, handoff_id)?;
        let row = sqlx::query(
            "SELECT record_json FROM session_handoffs
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ? AND handoff_id = ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .bind(handoff_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| database_error("load_handoff", source))?
        .ok_or(SessionHandoffError::NotFound)?;
        let record = decode_json_row(&row, "record_json", "handoff")?;
        validate_stored_handoff(&record, key)?;
        Ok(record)
    }

    pub async fn load_attachment(
        &self,
        key: &SessionKeyV1,
        attachment_id: &str,
    ) -> Result<SessionAttachmentV1, SessionHandoffError> {
        validate_key_and_id(key, attachment_id)?;
        let row = sqlx::query(
            "SELECT attachment_json FROM session_attachments
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ? AND attachment_id = ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .bind(attachment_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| database_error("load_attachment", source))?
        .ok_or(SessionHandoffError::NotFound)?;
        let attachment = decode_json_row(&row, "attachment_json", "attachment")?;
        validate_stored_attachment(&attachment, key)?;
        Ok(attachment)
    }

    pub async fn transition_handoff(
        &self,
        request: &TransitionSessionHandoffV1,
    ) -> Result<SessionHandoffRecordV1, SessionHandoffError> {
        self.transition_handoff_internal(request, false).await
    }

    async fn transition_handoff_internal(
        &self,
        request: &TransitionSessionHandoffV1,
        authority_transition: bool,
    ) -> Result<SessionHandoffRecordV1, SessionHandoffError> {
        validate_transition_request(request)?;
        if !authority_transition
            && matches!(
                request.next_state,
                SessionHandoffStateV1::Fenced | SessionHandoffStateV1::Active
            )
        {
            return Err(SessionHandoffError::Invalid(
                "Fenced and Active are installed only by authority-bearing service operations"
                    .into(),
            ));
        }
        let request_hash = stable_hash(b"astra.transition-session-handoff.v1\0", request)?;
        let next_sequence = request
            .expected_transition_seq
            .checked_add(1)
            .ok_or_else(|| SessionHandoffError::Invalid("transition sequence overflow".into()))?;
        let mut tx = self.begin("begin_transition_handoff").await?;
        let now = database_now_ms(&mut tx).await?;

        if let Some(row) = sqlx::query(
            "SELECT request_hash, event_json FROM session_handoff_events
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?
               AND handoff_id = ? AND transition_seq = ?",
        )
        .bind(&request.key.isolation_domain)
        .bind(&request.key.owner_user_id)
        .bind(&request.key.session_id)
        .bind(&request.key.branch_id)
        .bind(&request.handoff_id)
        .bind(i64_from_u64("transition sequence", next_sequence)?)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("load_transition_retry", source))?
        {
            verify_request_hash(&row, &request_hash)?;
            let event: HandoffTransitionEventV1 =
                decode_json_row(&row, "event_json", "handoff_transition")?;
            validate_stored_handoff(&event.record, &request.key)?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_transition_retry", source))?;
            return Ok(event.record);
        }

        let row = sqlx::query(
            "SELECT record_json FROM session_handoffs
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ? AND handoff_id = ?
             FOR UPDATE",
        )
        .bind(&request.key.isolation_domain)
        .bind(&request.key.owner_user_id)
        .bind(&request.key.session_id)
        .bind(&request.key.branch_id)
        .bind(&request.handoff_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("lock_handoff", source))?
        .ok_or(SessionHandoffError::NotFound)?;
        let mut record: SessionHandoffRecordV1 = decode_json_row(&row, "record_json", "handoff")?;
        validate_stored_handoff(&record, &request.key)?;
        if now > record.deadline_unix_ms
            && !matches!(
                request.next_state,
                SessionHandoffStateV1::Blocked | SessionHandoffStateV1::Aborted
            )
        {
            return Err(SessionHandoffError::DeadlineExpired);
        }
        if record.state != request.expected_state
            || record.transition_seq != request.expected_transition_seq
        {
            return Err(SessionHandoffError::StateConflict {
                expected_state: request.expected_state,
                expected_seq: request.expected_transition_seq,
                observed_state: record.state,
                observed_seq: record.transition_seq,
            });
        }
        let from = record.state;
        apply_patch(&mut record, &request.patch);
        if request.next_state == SessionHandoffStateV1::Checkpointed
            && (record.watermarks.checkpoint_id.is_none()
                || record.watermarks.pending_invocation_count != 0
                || record.watermarks.pending_approval_count != 0
                || record.watermarks.pending_outbox_count != 0)
        {
            return Err(SessionHandoffError::Invalid(
                "checkpoint requires a durable checkpoint identity and zero pending operation watermarks"
                    .into(),
            ));
        }
        record
            .transition(request.expected_state, request.next_state, now)
            .map_err(|error| SessionHandoffError::Invalid(error.to_string()))?;
        record
            .validate()
            .map_err(|error| SessionHandoffError::Invalid(error.to_string()))?;
        update_handoff(&mut tx, &record).await?;
        insert_transition_event(&mut tx, Some(from), &record, &request_hash).await?;
        if record.state.is_terminal() {
            clear_active_slot(&mut tx, &record.key, &record.handoff_id).await?;
        }
        tx.commit()
            .await
            .map_err(|source| database_error("commit_transition_handoff", source))?;
        Ok(record)
    }

    /// Atomically promote the hydrated target attachment to controller,
    /// demote the source attachment, publish `Active`, and release the
    /// one-handoff slot.
    pub async fn activate_handoff(
        &self,
        key: &SessionKeyV1,
        handoff_id: &str,
        expected_transition_seq: u64,
        idempotency_key: &str,
    ) -> Result<SessionHandoffRecordV1, SessionHandoffError> {
        validate_key_and_id(key, handoff_id)?;
        validate_identity("idempotency key", idempotency_key, MAX_IDEMPOTENCY_BYTES)?;
        let request = TransitionSessionHandoffV1 {
            idempotency_key: idempotency_key.to_owned(),
            key: key.clone(),
            handoff_id: handoff_id.to_owned(),
            expected_state: SessionHandoffStateV1::Hydrating,
            expected_transition_seq,
            next_state: SessionHandoffStateV1::Active,
            patch: HandoffTransitionPatchV1 {
                status_detail: Some("target_controller_active".into()),
                ..HandoffTransitionPatchV1::default()
            },
        };
        let request_hash = stable_hash(b"astra.activate-session-handoff.v1\0", &request)?;
        let next_sequence = expected_transition_seq
            .checked_add(1)
            .ok_or_else(|| SessionHandoffError::Invalid("transition sequence overflow".into()))?;
        let mut tx = self.begin("begin_activate_handoff").await?;
        let now = database_now_ms(&mut tx).await?;

        if let Some(row) = sqlx::query(
            "SELECT request_hash, event_json FROM session_handoff_events
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?
               AND handoff_id = ? AND transition_seq = ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .bind(handoff_id)
        .bind(i64_from_u64("transition sequence", next_sequence)?)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("load_activation_retry", source))?
        {
            verify_request_hash(&row, &request_hash)?;
            let event: HandoffTransitionEventV1 =
                decode_json_row(&row, "event_json", "handoff_transition")?;
            validate_stored_handoff(&event.record, key)?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_activation_retry", source))?;
            return Ok(event.record);
        }

        let row = sqlx::query(
            "SELECT record_json FROM session_handoffs
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ? AND handoff_id = ?
             FOR UPDATE",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .bind(handoff_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("lock_activation_handoff", source))?
        .ok_or(SessionHandoffError::NotFound)?;
        let mut record: SessionHandoffRecordV1 = decode_json_row(&row, "record_json", "handoff")?;
        validate_stored_handoff(&record, key)?;
        if now > record.deadline_unix_ms {
            return Err(SessionHandoffError::DeadlineExpired);
        }
        if record.state != SessionHandoffStateV1::Hydrating
            || record.transition_seq != expected_transition_seq
        {
            return Err(SessionHandoffError::StateConflict {
                expected_state: SessionHandoffStateV1::Hydrating,
                expected_seq: expected_transition_seq,
                observed_state: record.state,
                observed_seq: record.transition_seq,
            });
        }
        if record.target_writer_epoch.is_none() {
            return Err(SessionHandoffError::Invalid(
                "cannot activate before canonical writer fencing".into(),
            ));
        }
        let target_id = record.to_attachment_id.as_deref().ok_or_else(|| {
            SessionHandoffError::NeedsRepair("handoff has no target attachment".into())
        })?;
        let mut target = lock_attachment(&mut tx, key, target_id).await?;
        if target.expires_at_unix_ms <= now
            || target.actor != record.target_actor
            || target.workspace != record.workspace
        {
            return Err(SessionHandoffError::Invalid(
                "target attachment expired or workspace/actor evidence changed".into(),
            ));
        }
        if let Some(source_id) = &record.from_attachment_id {
            let mut source = lock_attachment(&mut tx, key, source_id).await?;
            source.mode = SessionAttachmentModeV1::ReadOnly;
            update_attachment_mode(&mut tx, &source).await?;
        }
        target.mode = SessionAttachmentModeV1::Controller;
        update_attachment_mode(&mut tx, &target).await?;

        let from = record.state;
        apply_patch(&mut record, &request.patch);
        record
            .transition(from, SessionHandoffStateV1::Active, now)
            .map_err(|error| SessionHandoffError::Invalid(error.to_string()))?;
        record
            .validate()
            .map_err(|error| SessionHandoffError::Invalid(error.to_string()))?;
        update_handoff(&mut tx, &record).await?;
        insert_transition_event(&mut tx, Some(from), &record, &request_hash).await?;
        clear_active_slot(&mut tx, key, handoff_id).await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_activate_handoff", source))?;
        Ok(record)
    }

    /// Persist fence intent, atomically transfer canonical writer authority,
    /// then persist the observed fence. A retry while `Fencing` replays the
    /// same writer-transfer idempotency key.
    pub async fn fence_writer(
        &self,
        key: &SessionKeyV1,
        handoff_id: &str,
        source_lease: Option<ConversationWriterLeaseV1>,
        ttl: Duration,
        transition_idempotency_key: &str,
    ) -> Result<FenceSessionWriterOutcomeV1, SessionHandoffError> {
        validate_duration(ttl, MAX_ATTACHMENT_TTL, "writer lease TTL")?;
        validate_identity(
            "transition idempotency key",
            transition_idempotency_key,
            512,
        )?;
        let mut record = self.load_handoff(key, handoff_id).await?;
        if record.state == SessionHandoffStateV1::Fenced
            || record.state == SessionHandoffStateV1::Hydrating
            || record.state == SessionHandoffStateV1::Active
        {
            return Ok(FenceSessionWriterOutcomeV1 {
                handoff: record,
                lease: None,
                transfer: None,
            });
        }
        if record.state != SessionHandoffStateV1::Fencing {
            record = self
                .transition_handoff(&TransitionSessionHandoffV1 {
                    idempotency_key: format!("{transition_idempotency_key}:intent"),
                    key: key.clone(),
                    handoff_id: handoff_id.to_owned(),
                    expected_state: record.state,
                    expected_transition_seq: record.transition_seq,
                    next_state: SessionHandoffStateV1::Fencing,
                    patch: HandoffTransitionPatchV1::default(),
                })
                .await?;
        }
        let transfer_request = WriterTransferRequestV1 {
            handoff_id: record.handoff_id.clone(),
            idempotency_key: format!("handoff:{}:writer-transfer", record.handoff_id),
            key: record.key.clone(),
            mode: record.mode,
            source_lease,
            expected_cursor: record.base_cursor.clone(),
            target_actor: record.target_actor.clone(),
            risk: record.risk.clone(),
        };
        let transfer = self
            .coordinator
            .transfer_writer(&transfer_request, ttl)
            .await?;
        let lease = match &transfer {
            TransferWriterOutcome::Transferred(lease)
            | TransferWriterOutcome::AlreadyTransferred(lease) => Some(lease.clone()),
            TransferWriterOutcome::Conflict { reason, .. } => {
                let blocked = self
                    .transition_handoff(&TransitionSessionHandoffV1 {
                        idempotency_key: format!("{transition_idempotency_key}:blocked"),
                        key: key.clone(),
                        handoff_id: handoff_id.to_owned(),
                        expected_state: SessionHandoffStateV1::Fencing,
                        expected_transition_seq: record.transition_seq,
                        next_state: SessionHandoffStateV1::Blocked,
                        patch: HandoffTransitionPatchV1 {
                            status_detail: Some(
                                format!("writer_transfer_{reason:?}").to_lowercase(),
                            ),
                            ..HandoffTransitionPatchV1::default()
                        },
                    })
                    .await?;
                return Ok(FenceSessionWriterOutcomeV1 {
                    handoff: blocked,
                    lease: None,
                    transfer: Some(transfer),
                });
            }
        };
        let lease_ref = lease.as_ref().expect("successful transfer has lease");
        let fenced = self
            .transition_handoff_internal(
                &TransitionSessionHandoffV1 {
                    idempotency_key: format!("{transition_idempotency_key}:fenced"),
                    key: key.clone(),
                    handoff_id: handoff_id.to_owned(),
                    expected_state: SessionHandoffStateV1::Fencing,
                    expected_transition_seq: record.transition_seq,
                    next_state: SessionHandoffStateV1::Fenced,
                    patch: HandoffTransitionPatchV1 {
                        target_writer_epoch: Some(lease_ref.writer_epoch),
                        status_detail: Some("canonical_writer_fenced".into()),
                        ..HandoffTransitionPatchV1::default()
                    },
                },
                true,
            )
            .await?;
        Ok(FenceSessionWriterOutcomeV1 {
            handoff: fenced,
            lease,
            transfer: Some(transfer),
        })
    }

    async fn begin(
        &self,
        operation: &'static str,
    ) -> Result<Transaction<'_, MySql>, SessionHandoffError> {
        self.pool
            .get()
            .begin()
            .await
            .map_err(|source| database_error(operation, source))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HandoffTransitionEventV1 {
    from_state: Option<SessionHandoffStateV1>,
    record: SessionHandoffRecordV1,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionHandoffEventV1 {
    pub from_state: Option<SessionHandoffStateV1>,
    pub record: SessionHandoffRecordV1,
    pub created_at: chrono::NaiveDateTime,
}

fn validate_attach_request(request: &AttachSessionRequestV1) -> Result<(), SessionHandoffError> {
    request
        .key
        .validate()
        .map_err(|error| SessionHandoffError::Invalid(error.to_string()))?;
    request
        .actor
        .validate_for(&request.key)
        .map_err(|error| SessionHandoffError::Invalid(error.to_string()))?;
    validate_identity(
        "idempotency key",
        &request.idempotency_key,
        MAX_IDEMPOTENCY_BYTES,
    )?;
    if let Some(root) = &request.after_manifest_root {
        validate_digest("after manifest root", root)?;
    }
    if let Some(workspace) = &request.workspace {
        workspace
            .validate()
            .map_err(|error| SessionHandoffError::Invalid(error.to_string()))?;
    }
    Ok(())
}

fn validate_handoff_request(request: &RequestSessionHandoffV1) -> Result<(), SessionHandoffError> {
    request
        .key
        .validate()
        .map_err(|error| SessionHandoffError::Invalid(error.to_string()))?;
    request
        .target_actor
        .validate_for(&request.key)
        .map_err(|error| SessionHandoffError::Invalid(error.to_string()))?;
    if request.target_actor.authority_epochs != request.authority_epochs {
        return Err(SessionHandoffError::Invalid(
            "target actor authority epochs do not match the request".into(),
        ));
    }
    validate_identity(
        "idempotency key",
        &request.idempotency_key,
        MAX_IDEMPOTENCY_BYTES,
    )?;
    validate_identity("target attachment", &request.to_attachment_id, 128)?;
    validate_identity("reason", &request.reason, 1_024)?;
    if request.mode == SessionHandoffModeV1::Graceful && request.from_attachment_id.is_none() {
        return Err(SessionHandoffError::Invalid(
            "graceful handoff requires a source attachment".into(),
        ));
    }
    if let Some(id) = &request.from_attachment_id {
        validate_identity("source attachment", id, 128)?;
    }
    if request
        .base_cursor
        .as_ref()
        .is_some_and(|cursor| !request.key.validates_cursor(cursor))
    {
        return Err(SessionHandoffError::Invalid(
            "base cursor does not match SessionKey".into(),
        ));
    }
    request
        .risk
        .validate()
        .map_err(|error| SessionHandoffError::Invalid(error.to_string()))?;
    if request.mode == SessionHandoffModeV1::Forced && !request.risk.permits_forced_fence() {
        return Err(SessionHandoffError::Invalid(
            "forced handoff requires verified authorization".into(),
        ));
    }
    if request.mode == SessionHandoffModeV1::Graceful
        && request.risk != HandoffRiskEvidenceV1::default()
    {
        return Err(SessionHandoffError::Invalid(
            "graceful handoff cannot carry forced-takeover risk evidence".into(),
        ));
    }
    if let Some(workspace) = &request.workspace {
        workspace
            .validate()
            .map_err(|error| SessionHandoffError::Invalid(error.to_string()))?;
    }
    Ok(())
}

fn validate_transition_request(
    request: &TransitionSessionHandoffV1,
) -> Result<(), SessionHandoffError> {
    validate_key_and_id(&request.key, &request.handoff_id)?;
    validate_identity(
        "idempotency key",
        &request.idempotency_key,
        MAX_IDEMPOTENCY_BYTES,
    )?;
    if request.expected_transition_seq == 0 {
        return Err(SessionHandoffError::Invalid(
            "expected transition sequence must be positive".into(),
        ));
    }
    if request
        .patch
        .base_cursor
        .as_ref()
        .is_some_and(|cursor| !request.key.validates_cursor(cursor))
    {
        return Err(SessionHandoffError::Invalid(
            "patched cursor does not match SessionKey".into(),
        ));
    }
    Ok(())
}

fn validate_key_and_id(key: &SessionKeyV1, handoff_id: &str) -> Result<(), SessionHandoffError> {
    key.validate()
        .map_err(|error| SessionHandoffError::Invalid(error.to_string()))?;
    validate_identity("handoff id", handoff_id, 128)
}

fn validate_stored_attachment(
    attachment: &SessionAttachmentV1,
    key: &SessionKeyV1,
) -> Result<(), SessionHandoffError> {
    attachment
        .validate()
        .map_err(|error| SessionHandoffError::NeedsRepair(error.to_string()))?;
    if &attachment.key != key {
        return Err(SessionHandoffError::NeedsRepair(
            "stored attachment SessionKey mismatch".into(),
        ));
    }
    Ok(())
}

fn validate_stored_handoff(
    record: &SessionHandoffRecordV1,
    key: &SessionKeyV1,
) -> Result<(), SessionHandoffError> {
    record
        .validate()
        .map_err(|error| SessionHandoffError::NeedsRepair(error.to_string()))?;
    if &record.key != key {
        return Err(SessionHandoffError::NeedsRepair(
            "stored handoff SessionKey mismatch".into(),
        ));
    }
    Ok(())
}

fn validate_identity(field: &str, value: &str, maximum: usize) -> Result<(), SessionHandoffError> {
    if value.is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        return Err(SessionHandoffError::Invalid(format!(
            "{field} must be non-empty and at most {maximum} bytes"
        )));
    }
    Ok(())
}

fn validate_digest(field: &str, value: &str) -> Result<(), SessionHandoffError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(SessionHandoffError::Invalid(format!(
            "{field} must be a lowercase SHA-256 digest"
        )));
    }
    Ok(())
}

fn validate_duration(
    value: Duration,
    maximum: Duration,
    field: &str,
) -> Result<(), SessionHandoffError> {
    if value.is_zero() || value > maximum {
        return Err(SessionHandoffError::Invalid(format!(
            "{field} must be between 1 ms and {} seconds",
            maximum.as_secs()
        )));
    }
    Ok(())
}

fn install_observation(attachment: &mut SessionAttachmentV1, delta: &ManifestDeltaV1) {
    attachment.observed_cursor = delta.head.as_ref().map(|head| head.cursor.clone());
    attachment.observed_manifest_root = delta
        .head
        .as_ref()
        .map(|head| head.latest_manifest_root.clone());
}

fn apply_patch(record: &mut SessionHandoffRecordV1, patch: &HandoffTransitionPatchV1) {
    if let Some(cursor) = &patch.base_cursor {
        record.base_cursor = Some(cursor.clone());
    }
    if let Some(epoch) = patch.target_writer_epoch {
        record.target_writer_epoch = Some(epoch);
    }
    if let Some(workspace) = &patch.workspace {
        record.workspace = Some(workspace.clone());
    }
    if let Some(watermarks) = &patch.watermarks {
        record.watermarks = watermarks.clone();
    }
    if let Some(risk) = &patch.risk {
        record.risk = risk.clone();
    }
    if let Some(detail) = &patch.status_detail {
        record.status_detail = Some(detail.clone());
    }
}

async fn quarantine_divergent_attachment(
    pool: &sqlx::Pool<MySql>,
    key: &SessionKeyV1,
    observed_manifest_root: &str,
    current_head: Option<&SessionContextHeadV1>,
    idempotency_hash: &str,
    request_hash: &str,
) -> Result<String, SessionHandoffError> {
    let candidate_id = Uuid::new_v4().to_string();
    let mut tx = pool
        .begin()
        .await
        .map_err(|source| database_error("begin_attachment_quarantine", source))?;
    sqlx::query(
        "INSERT IGNORE INTO session_attachment_quarantines
         (isolation_domain, owner_user_id, session_id, branch_id, quarantine_id,
          idempotency_hash, request_hash, observed_manifest_root,
          current_manifest_root, reason)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'divergent_manifest')",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .bind(&candidate_id)
    .bind(idempotency_hash)
    .bind(request_hash)
    .bind(observed_manifest_root)
    .bind(current_head.map(|head| head.latest_manifest_root.as_str()))
    .execute(&mut *tx)
    .await
    .map_err(|source| database_error("insert_attachment_quarantine", source))?;
    let row = sqlx::query(
        "SELECT quarantine_id, request_hash
         FROM session_attachment_quarantines
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ? AND idempotency_hash = ?
         FOR UPDATE",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .bind(idempotency_hash)
    .fetch_one(&mut *tx)
    .await
    .map_err(|source| database_error("load_attachment_quarantine", source))?;
    verify_request_hash(&row, request_hash)?;
    let quarantine_id = row
        .try_get("quarantine_id")
        .map_err(|source| database_error("decode_attachment_quarantine", source))?;
    tx.commit()
        .await
        .map_err(|source| database_error("commit_attachment_quarantine", source))?;
    Ok(quarantine_id)
}

async fn ensure_slot(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
) -> Result<(), SessionHandoffError> {
    sqlx::query(
        "INSERT IGNORE INTO session_handoff_slots
         (isolation_domain, owner_user_id, session_id, branch_id)
         VALUES (?, ?, ?, ?)",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .execute(&mut **tx)
    .await
    .map_err(|source| database_error("ensure_handoff_slot", source))?;
    Ok(())
}

async fn lock_active_handoff(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
) -> Result<Option<String>, SessionHandoffError> {
    sqlx::query(
        "SELECT active_handoff_id FROM session_handoff_slots
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ?
         FOR UPDATE",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|source| database_error("lock_handoff_slot", source))?
    .try_get("active_handoff_id")
    .map_err(|source| database_error("decode_active_handoff", source))
}

async fn lock_and_increment_attachment_epoch(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
) -> Result<u64, SessionHandoffError> {
    let row = sqlx::query(
        "SELECT next_attachment_epoch FROM session_handoff_slots
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ?
         FOR UPDATE",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|source| database_error("lock_attachment_epoch", source))?;
    let current: i64 = row
        .try_get("next_attachment_epoch")
        .map_err(|source| database_error("decode_attachment_epoch", source))?;
    let next = u64::try_from(current)
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| SessionHandoffError::NeedsRepair("attachment epoch overflow".into()))?;
    sqlx::query(
        "UPDATE session_handoff_slots
         SET next_attachment_epoch = ?, updated_at = NOW(6)
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ?",
    )
    .bind(i64_from_u64("attachment epoch", next)?)
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .execute(&mut **tx)
    .await
    .map_err(|source| database_error("increment_attachment_epoch", source))?;
    Ok(next)
}

async fn lock_attachment(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
    attachment_id: &str,
) -> Result<SessionAttachmentV1, SessionHandoffError> {
    let row = sqlx::query(
        "SELECT attachment_json FROM session_attachments
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ? AND attachment_id = ?
         FOR UPDATE",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .bind(attachment_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| database_error("lock_attachment", source))?
    .ok_or(SessionHandoffError::NotFound)?;
    let attachment = decode_json_row(&row, "attachment_json", "attachment")?;
    validate_stored_attachment(&attachment, key)?;
    Ok(attachment)
}

async fn update_attachment_mode(
    tx: &mut Transaction<'_, MySql>,
    attachment: &SessionAttachmentV1,
) -> Result<(), SessionHandoffError> {
    attachment
        .validate()
        .map_err(|error| SessionHandoffError::Invalid(error.to_string()))?;
    let result = sqlx::query(
        "UPDATE session_attachments
         SET mode = ?, attachment_json = ?, updated_at = NOW(6)
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ? AND attachment_id = ?",
    )
    .bind(attachment_mode(attachment.mode))
    .bind(to_json("attachment", attachment)?)
    .bind(&attachment.key.isolation_domain)
    .bind(&attachment.key.owner_user_id)
    .bind(&attachment.key.session_id)
    .bind(&attachment.key.branch_id)
    .bind(&attachment.attachment_id)
    .execute(&mut **tx)
    .await
    .map_err(|source| database_error("update_attachment_mode", source))?;
    if result.rows_affected() != 1 {
        return Err(SessionHandoffError::NeedsRepair(
            "attachment disappeared during activation".into(),
        ));
    }
    Ok(())
}

async fn insert_handoff(
    tx: &mut Transaction<'_, MySql>,
    record: &SessionHandoffRecordV1,
    idempotency_hash: &str,
    request_hash: &str,
) -> Result<(), SessionHandoffError> {
    sqlx::query(
        "INSERT INTO session_handoffs
         (isolation_domain, owner_user_id, session_id, branch_id, handoff_id,
          idempotency_hash, request_hash, state, mode, transition_seq,
          deadline_ms, record_json)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&record.key.isolation_domain)
    .bind(&record.key.owner_user_id)
    .bind(&record.key.session_id)
    .bind(&record.key.branch_id)
    .bind(&record.handoff_id)
    .bind(idempotency_hash)
    .bind(request_hash)
    .bind(state_name(record.state))
    .bind(mode_name(record.mode))
    .bind(i64_from_u64("transition sequence", record.transition_seq)?)
    .bind(record.deadline_unix_ms)
    .bind(to_json("handoff", record)?)
    .execute(&mut **tx)
    .await
    .map_err(|source| database_error("insert_handoff", source))?;
    Ok(())
}

async fn update_handoff(
    tx: &mut Transaction<'_, MySql>,
    record: &SessionHandoffRecordV1,
) -> Result<(), SessionHandoffError> {
    let result = sqlx::query(
        "UPDATE session_handoffs
         SET state = ?, transition_seq = ?, deadline_ms = ?,
             record_json = ?, updated_at = NOW(6)
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ? AND handoff_id = ?",
    )
    .bind(state_name(record.state))
    .bind(i64_from_u64("transition sequence", record.transition_seq)?)
    .bind(record.deadline_unix_ms)
    .bind(to_json("handoff", record)?)
    .bind(&record.key.isolation_domain)
    .bind(&record.key.owner_user_id)
    .bind(&record.key.session_id)
    .bind(&record.key.branch_id)
    .bind(&record.handoff_id)
    .execute(&mut **tx)
    .await
    .map_err(|source| database_error("update_handoff", source))?;
    if result.rows_affected() != 1 {
        return Err(SessionHandoffError::NeedsRepair(
            "handoff row disappeared during transition".into(),
        ));
    }
    Ok(())
}

async fn insert_transition_event(
    tx: &mut Transaction<'_, MySql>,
    from_state: Option<SessionHandoffStateV1>,
    record: &SessionHandoffRecordV1,
    request_hash: &str,
) -> Result<(), SessionHandoffError> {
    let event = HandoffTransitionEventV1 {
        from_state,
        record: record.clone(),
    };
    sqlx::query(
        "INSERT INTO session_handoff_events
         (isolation_domain, owner_user_id, session_id, branch_id, handoff_id,
          transition_seq, request_hash, from_state, to_state, event_json)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&record.key.isolation_domain)
    .bind(&record.key.owner_user_id)
    .bind(&record.key.session_id)
    .bind(&record.key.branch_id)
    .bind(&record.handoff_id)
    .bind(i64_from_u64("transition sequence", record.transition_seq)?)
    .bind(request_hash)
    .bind(from_state.map(state_name))
    .bind(state_name(record.state))
    .bind(to_json("handoff_transition", &event)?)
    .execute(&mut **tx)
    .await
    .map_err(|source| database_error("insert_handoff_event", source))?;
    Ok(())
}

async fn clear_active_slot(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
    handoff_id: &str,
) -> Result<(), SessionHandoffError> {
    sqlx::query(
        "UPDATE session_handoff_slots
         SET active_handoff_id = NULL, updated_at = NOW(6)
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ? AND active_handoff_id = ?",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .bind(handoff_id)
    .execute(&mut **tx)
    .await
    .map_err(|source| database_error("clear_handoff_slot", source))?;
    Ok(())
}

async fn database_now_ms(tx: &mut Transaction<'_, MySql>) -> Result<i64, SessionHandoffError> {
    crate::db_row::database_now_unix_ms(tx)
        .await
        .map_err(|source| database_error("load_handoff_time", source))
}

fn checked_expiry(now: i64, duration: Duration) -> Result<i64, SessionHandoffError> {
    let millis = i64::try_from(duration.as_millis())
        .map_err(|_| SessionHandoffError::Invalid("duration exceeds i64".into()))?;
    now.checked_add(millis)
        .ok_or_else(|| SessionHandoffError::Invalid("deadline overflow".into()))
}

fn stable_hash<T: Serialize>(domain: &[u8], value: &T) -> Result<String, SessionHandoffError> {
    let mut digest = Sha256::new();
    digest.update(domain);
    let bytes = serde_json::to_vec(value).map_err(|source| SessionHandoffError::Json {
        entity: "request_hash",
        source,
    })?;
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
    Ok(format!("{:x}", digest.finalize()))
}

fn identity_hash(operation: &str, value: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"astra.session-handoff-idempotency.v1\0");
    digest.update((operation.len() as u64).to_be_bytes());
    digest.update(operation.as_bytes());
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value.as_bytes());
    format!("{:x}", digest.finalize())
}

fn verify_request_hash(
    row: &sqlx::mysql::MySqlRow,
    expected: &str,
) -> Result<(), SessionHandoffError> {
    let stored: String = row
        .try_get("request_hash")
        .map_err(|source| database_error("decode_request_hash", source))?;
    if stored != expected {
        return Err(SessionHandoffError::IdempotencyMismatch);
    }
    Ok(())
}

fn decode_json_row<T: DeserializeOwned>(
    row: &sqlx::mysql::MySqlRow,
    column: &'static str,
    entity: &'static str,
) -> Result<T, SessionHandoffError> {
    let json: String = row
        .try_get(column)
        .map_err(|source| database_error("decode_handoff_row", source))?;
    serde_json::from_str(&json).map_err(|source| SessionHandoffError::Json { entity, source })
}

fn to_json<T: Serialize>(entity: &'static str, value: &T) -> Result<String, SessionHandoffError> {
    serde_json::to_string(value).map_err(|source| SessionHandoffError::Json { entity, source })
}

fn i64_from_u64(field: &str, value: u64) -> Result<i64, SessionHandoffError> {
    i64::try_from(value)
        .map_err(|_| SessionHandoffError::Invalid(format!("{field} exceeds BIGINT")))
}

fn state_name(state: SessionHandoffStateV1) -> &'static str {
    match state {
        SessionHandoffStateV1::Requested => "requested",
        SessionHandoffStateV1::Validating => "validating",
        SessionHandoffStateV1::Draining => "draining",
        SessionHandoffStateV1::Checkpointed => "checkpointed",
        SessionHandoffStateV1::Fencing => "fencing",
        SessionHandoffStateV1::Fenced => "fenced",
        SessionHandoffStateV1::Hydrating => "hydrating",
        SessionHandoffStateV1::Active => "active",
        SessionHandoffStateV1::Blocked => "blocked",
        SessionHandoffStateV1::Aborted => "aborted",
        SessionHandoffStateV1::NeedsReconciliation => "needs_reconciliation",
    }
}

fn mode_name(mode: SessionHandoffModeV1) -> &'static str {
    match mode {
        SessionHandoffModeV1::Graceful => "graceful",
        SessionHandoffModeV1::Forced => "forced",
    }
}

fn attachment_mode(mode: SessionAttachmentModeV1) -> &'static str {
    match mode {
        SessionAttachmentModeV1::ReadOnly => "read_only",
        SessionAttachmentModeV1::Controller => "controller",
    }
}

fn placement_name(placement: SessionPlacementV1) -> &'static str {
    match placement {
        SessionPlacementV1::Server => "server",
        SessionPlacementV1::Cli => "cli",
        SessionPlacementV1::Edge => "edge",
    }
}

fn database_error(operation: &'static str, source: sqlx::Error) -> SessionHandoffError {
    SessionHandoffError::Database { operation, source }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AcquireWriterOutcome, DatabaseSessionContextCoordinator, ReserveTurnOutcome,
        SessionContextCoordinator,
    };
    use astra_turn_types::{
        ActorKindV1, AuthorityEpochsV1, CANONICAL_TURN_DELTA_SCHEMA_VERSION, CanonicalTurnDeltaV1,
        CoordinatorMutationV1, SessionSurfaceV1,
    };
    use serde_json::json;

    static HANDOFF_DB: tokio::sync::OnceCell<SharedPool> = tokio::sync::OnceCell::const_new();

    async fn setup_handoff_db_it() -> SharedPool {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
        );
        let _ = dotenvy::dotenv();
        HANDOFF_DB
            .get_or_init(|| async {
                let settings = astra_core::MatrixOneSettings::from_env();
                let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
                    .unwrap_or_else(|_| "mysql".to_owned());
                crate::storage::ensure_core_schema(&settings, &catalog)
                    .await
                    .expect("ensure core schema");
                SharedPool::new(&settings).await.expect("shared pool")
            })
            .await
            .clone()
    }

    fn actor(owner: &str, identity: &str) -> ActorContextV1 {
        ActorContextV1::owner_user(
            owner,
            identity,
            ActorKindV1::Cli,
            SessionSurfaceV1::Cli,
            Some(format!("device-{identity}")),
            AuthorityEpochsV1::default(),
        )
    }

    fn acquired(outcome: AcquireWriterOutcome) -> ConversationWriterLeaseV1 {
        match outcome {
            AcquireWriterOutcome::Acquired(lease)
            | AcquireWriterOutcome::AlreadyAcquired(lease) => lease,
            other => panic!("unexpected acquire outcome {other:?}"),
        }
    }

    fn reserved(outcome: ReserveTurnOutcome) -> astra_turn_types::TurnReservationV1 {
        match outcome {
            ReserveTurnOutcome::Reserved(reservation)
            | ReserveTurnOutcome::AlreadyReserved(reservation) => reservation,
            other => panic!("unexpected reservation outcome {other:?}"),
        }
    }

    async fn cleanup(pool: &SharedPool, key: &SessionKeyV1) {
        for table in [
            "session_attachment_quarantines",
            "session_handoff_events",
            "session_handoffs",
            "session_attachments",
            "session_handoff_slots",
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
            "DELETE FROM conversation_segments
             WHERE isolation_domain = ? AND owner_user_id = ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .execute(pool.get())
        .await
        .expect("cleanup segments");
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn graceful_handoff_is_durable_idempotent_and_fences_old_writer() {
        let pool = setup_handoff_db_it().await;
        let suffix = Uuid::new_v4();
        let key = SessionKeyV1::owner_session(
            "handoff-it",
            format!("owner-{suffix}"),
            format!("session-{suffix}"),
            "main",
        );
        cleanup(&pool, &key).await;
        let authority_reader = DatabaseSessionContextCoordinator::new(pool.clone());
        let coordinator: Arc<dyn SessionContextCoordinator> = Arc::new(authority_reader.clone());
        let service = DatabaseSessionHandoffService::new(pool.clone(), coordinator.clone());
        let source_actor = actor(&key.owner_user_id, "source");
        let target_actor = actor(&key.owner_user_id, "target");
        let source_lease = acquired(
            coordinator
                .acquire_writer(
                    &key,
                    None,
                    &source_actor,
                    Duration::from_secs(60),
                    "source-acquire",
                )
                .await
                .expect("acquire source"),
        );
        let reservation = reserved(
            coordinator
                .reserve_turn(&source_lease, None, Duration::from_secs(30), "source-turn")
                .await
                .expect("reserve source turn"),
        );
        let cursor = match coordinator
            .commit_turn(
                &reservation,
                CanonicalTurnDeltaV1 {
                    schema_version: CANONICAL_TURN_DELTA_SCHEMA_VERSION,
                    completed_turn: 1,
                    journal_event_seq: 1,
                    conversation_seq: 1,
                    compaction_generation: 0,
                    config_version_id: None,
                    mode: astra_turn_types::CanonicalDeltaModeV1::Append,
                    logical_segments: vec![vec![
                        json!({"role": "user", "content": "question"}),
                        json!({"role": "assistant", "content": "answer"}),
                    ]],
                },
                "source-commit",
            )
            .await
            .expect("commit source turn")
        {
            CoordinatorMutationV1::Applied { cursor } => cursor,
            other => panic!("unexpected commit outcome {other:?}"),
        };
        let quarantine_id = match service
            .attach_read_only(
                &AttachSessionRequestV1 {
                    idempotency_key: "divergent-attachment".into(),
                    key: key.clone(),
                    actor: actor(&key.owner_user_id, "divergent"),
                    placement: SessionPlacementV1::Cli,
                    after_manifest_root: Some("0".repeat(64)),
                    workspace: None,
                },
                Duration::from_secs(60),
            )
            .await
            .expect_err("divergent local root must not attach")
        {
            SessionHandoffError::ForkRequired { quarantine_id, .. } => quarantine_id,
            other => panic!("unexpected divergent attach error {other:?}"),
        };
        let quarantine_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM session_attachment_quarantines
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ? AND quarantine_id = ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .bind(quarantine_id)
        .fetch_one(pool.get())
        .await
        .expect("count quarantine");
        assert_eq!(quarantine_count, 1);
        let source_attachment = service
            .attach_read_only(
                &AttachSessionRequestV1 {
                    idempotency_key: "source-attachment".into(),
                    key: key.clone(),
                    actor: source_actor,
                    placement: SessionPlacementV1::Cli,
                    after_manifest_root: Some(cursor.canonical_root_hash.clone()),
                    workspace: None,
                },
                Duration::from_secs(60),
            )
            .await
            .expect("attach source")
            .attachment;
        let target_attachment = service
            .attach_read_only(
                &AttachSessionRequestV1 {
                    idempotency_key: "target-attachment".into(),
                    key: key.clone(),
                    actor: target_actor.clone(),
                    placement: SessionPlacementV1::Cli,
                    after_manifest_root: Some(cursor.canonical_root_hash.clone()),
                    workspace: None,
                },
                Duration::from_secs(60),
            )
            .await
            .expect("attach target")
            .attachment;
        let mut handoff = service
            .request_handoff(
                &RequestSessionHandoffV1 {
                    idempotency_key: "handoff-request".into(),
                    key: key.clone(),
                    mode: SessionHandoffModeV1::Graceful,
                    from_attachment_id: Some(source_attachment.attachment_id.clone()),
                    to_attachment_id: target_attachment.attachment_id.clone(),
                    from_placement: SessionPlacementV1::Cli,
                    to_placement: SessionPlacementV1::Cli,
                    target_actor,
                    base_cursor: Some(cursor.clone()),
                    authority_epochs: AuthorityEpochsV1::default(),
                    workspace: None,
                    watermarks: HandoffOperationWatermarksV1::default(),
                    risk: HandoffRiskEvidenceV1::default(),
                    reason: "move to replacement CLI".into(),
                },
                Duration::from_secs(60),
            )
            .await
            .expect("request handoff");
        for (state, idempotency) in [
            (SessionHandoffStateV1::Validating, "validate"),
            (SessionHandoffStateV1::Draining, "drain"),
        ] {
            handoff = service
                .transition_handoff(&TransitionSessionHandoffV1 {
                    idempotency_key: idempotency.into(),
                    key: key.clone(),
                    handoff_id: handoff.handoff_id.clone(),
                    expected_state: handoff.state,
                    expected_transition_seq: handoff.transition_seq,
                    next_state: state,
                    patch: HandoffTransitionPatchV1::default(),
                })
                .await
                .expect("advance handoff");
        }
        handoff = service
            .transition_handoff(&TransitionSessionHandoffV1 {
                idempotency_key: "checkpoint".into(),
                key: key.clone(),
                handoff_id: handoff.handoff_id.clone(),
                expected_state: handoff.state,
                expected_transition_seq: handoff.transition_seq,
                next_state: SessionHandoffStateV1::Checkpointed,
                patch: HandoffTransitionPatchV1 {
                    watermarks: Some(HandoffOperationWatermarksV1 {
                        checkpoint_id: Some("checkpoint-1".into()),
                        effect_cursor: Some("effect-1".into()),
                        ..HandoffOperationWatermarksV1::default()
                    }),
                    ..HandoffTransitionPatchV1::default()
                },
            })
            .await
            .expect("checkpoint handoff");
        let fenced = service
            .fence_writer(
                &key,
                &handoff.handoff_id,
                Some(source_lease.clone()),
                Duration::from_secs(60),
                "fence",
            )
            .await
            .expect("fence writer");
        let target_lease = fenced.lease.expect("new target lease");
        handoff = service
            .transition_handoff(&TransitionSessionHandoffV1 {
                idempotency_key: "hydrate".into(),
                key: key.clone(),
                handoff_id: handoff.handoff_id.clone(),
                expected_state: SessionHandoffStateV1::Fenced,
                expected_transition_seq: fenced.handoff.transition_seq,
                next_state: SessionHandoffStateV1::Hydrating,
                patch: HandoffTransitionPatchV1::default(),
            })
            .await
            .expect("hydrate target");
        handoff = service
            .activate_handoff(
                &key,
                &handoff.handoff_id,
                handoff.transition_seq,
                "activate",
            )
            .await
            .expect("activate target");
        assert_eq!(handoff.state, SessionHandoffStateV1::Active);
        assert_eq!(handoff.target_writer_epoch, Some(target_lease.writer_epoch));
        assert_eq!(
            service
                .load_attachment(&key, &source_attachment.attachment_id)
                .await
                .expect("load source attachment")
                .mode,
            SessionAttachmentModeV1::ReadOnly
        );
        assert_eq!(
            service
                .load_attachment(&key, &target_attachment.attachment_id)
                .await
                .expect("load target attachment")
                .mode,
            SessionAttachmentModeV1::Controller
        );
        assert!(matches!(
            coordinator
                .reserve_turn(
                    &source_lease,
                    Some(&cursor),
                    Duration::from_secs(10),
                    "stale-source-turn",
                )
                .await,
            Err(SessionContextCoordinatorError::Fenced)
        ));
        assert!(matches!(
            coordinator
                .reserve_turn(
                    &target_lease,
                    Some(&cursor),
                    Duration::from_secs(10),
                    "target-turn",
                )
                .await
                .expect("target reserve"),
            ReserveTurnOutcome::Reserved(_)
        ));
        let event_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM session_handoff_events
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ? AND handoff_id = ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .bind(&handoff.handoff_id)
        .fetch_one(pool.get())
        .await
        .expect("count handoff events");
        assert_eq!(event_count, 8);
        let handoff_events = service
            .list_handoff_events(&key, 100)
            .await
            .expect("list typed handoff events");
        assert_eq!(handoff_events.len(), 8);
        assert_eq!(
            handoff_events.first().map(|event| event.record.state),
            Some(SessionHandoffStateV1::Active)
        );
        let authority_events = authority_reader
            .list_authority_events(&key, 100)
            .await
            .expect("list typed authority events");
        assert!(
            authority_events
                .iter()
                .any(|event| event.outcome == "stale_fenced"),
            "the rejected old-writer reservation must remain causally queryable"
        );
        cleanup(&pool, &key).await;
    }
}
