//! Durable O(1) session forks over immutable conversation history.
//!
//! Preparing a fork pins one exact parent manifest and records evidence for
//! every durable state dimension. Activating it installs only a shared-prefix
//! pointer plus a fresh child head. No historical segment, manifest node, run,
//! lease, approval, mailbox, or invocation authority is copied.

use std::{sync::Arc, time::Duration};

use astra_core::SharedPool;
use astra_turn_types::{
    ActorContextV1, ForkBasisDimensionV1, ForkDimensionDispositionV1, ForkDimensionEvidenceV1,
    ForkExcludedAuthorityV1, SESSION_FORK_MANIFEST_SCHEMA_VERSION, SessionCursorV1,
    SessionForkActivationV1, SessionForkManifestV1, SessionForkStateV1, SessionKeyV1,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use sqlx::{MySql, Row, Transaction};
use thiserror::Error;
use uuid::Uuid;

use crate::{AcquireWriterOutcome, SessionContextCoordinator, SessionContextCoordinatorError};

const MAX_IDEMPOTENCY_BYTES: usize = 512;
const MAX_REASON_BYTES: usize = 1_024;
const MAX_LIST_LIMIT: u32 = 200;
const MAX_WRITER_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_ABORT_GRACE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Debug, Error)]
pub enum SessionForkCoordinatorError {
    #[error("invalid session fork request: {0}")]
    Invalid(String),
    #[error("session fork was not found")]
    NotFound,
    #[error("session fork conflicts with existing child state")]
    Conflict,
    #[error("session fork idempotency key was reused for a different request")]
    IdempotencyMismatch,
    #[error("session fork writer is held by another actor")]
    WriterConflict,
    #[error("session fork state requires repair: {0}")]
    NeedsRepair(String),
    #[error(transparent)]
    Coordinator(#[from] SessionContextCoordinatorError),
    #[error("session fork database operation {operation} failed: {source}")]
    Database {
        operation: &'static str,
        #[source]
        source: sqlx::Error,
    },
    #[error("session fork JSON for {entity} failed: {source}")]
    Json {
        entity: &'static str,
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrepareSessionForkV1 {
    pub idempotency_key: String,
    pub parent_key: SessionKeyV1,
    pub child_key: SessionKeyV1,
    pub expected_parent_cursor: SessionCursorV1,
    pub dimensions: Vec<ForkDimensionEvidenceV1>,
    pub reason: String,
}

#[derive(Clone)]
pub struct DatabaseSessionForkCoordinator {
    pool: SharedPool,
    context: Arc<dyn SessionContextCoordinator>,
}

impl DatabaseSessionForkCoordinator {
    pub fn new(pool: SharedPool, context: Arc<dyn SessionContextCoordinator>) -> Self {
        Self { pool, context }
    }

    pub async fn prepare(
        &self,
        request: &PrepareSessionForkV1,
    ) -> Result<SessionForkManifestV1, SessionForkCoordinatorError> {
        validate_prepare_request(request)?;
        let request_hash = stable_hash(b"astra.prepare-session-fork.v1\0", request)?;
        let idempotency_hash = identity_hash("prepare", &request.idempotency_key);
        let mut tx = self.begin("begin_prepare_fork").await?;

        if let Some(row) = sqlx::query(
            "SELECT request_hash, manifest_json FROM session_forks
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND parent_session_id = ? AND parent_branch_id = ?
               AND idempotency_hash = ? FOR UPDATE",
        )
        .bind(&request.parent_key.isolation_domain)
        .bind(&request.parent_key.owner_user_id)
        .bind(&request.parent_key.session_id)
        .bind(&request.parent_key.branch_id)
        .bind(&idempotency_hash)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("load_prepare_retry", source))?
        {
            verify_request_hash(&row, &request_hash)?;
            let manifest = decode_json_row(&row, "manifest_json", "fork_manifest")?;
            validate_stored_manifest(&manifest, &request.parent_key)?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_prepare_retry", source))?;
            return Ok(manifest);
        }

        let parent_head = lock_parent_head(&mut tx, request).await?;
        ensure_child_is_empty(&mut tx, &request.child_key).await?;
        let now = database_now_ms(&mut tx).await?;
        let manifest = SessionForkManifestV1 {
            schema_version: SESSION_FORK_MANIFEST_SCHEMA_VERSION,
            fork_id: Uuid::new_v4().to_string(),
            parent_key: request.parent_key.clone(),
            child_key: request.child_key.clone(),
            parent_head,
            dimensions: request.dimensions.clone(),
            excluded_authority: vec![
                ForkExcludedAuthorityV1::Run,
                ForkExcludedAuthorityV1::WriterLease,
                ForkExcludedAuthorityV1::Approval,
                ForkExcludedAuthorityV1::Mailbox,
                ForkExcludedAuthorityV1::Invocation,
            ],
            state: SessionForkStateV1::Prepared,
            created_at_unix_ms: now,
            activated_at_unix_ms: None,
            status_detail: Some(request.reason.clone()),
        };
        manifest
            .validate()
            .map_err(|error| SessionForkCoordinatorError::Invalid(error.to_string()))?;
        insert_fork_child_session(&mut tx, &manifest).await?;

        sqlx::query(
            "INSERT INTO session_forks
             (isolation_domain, owner_user_id, fork_id,
              parent_session_id, parent_branch_id, child_session_id, child_branch_id,
              idempotency_hash, request_hash, state, manifest_json)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'prepared', ?)",
        )
        .bind(&manifest.parent_key.isolation_domain)
        .bind(&manifest.parent_key.owner_user_id)
        .bind(&manifest.fork_id)
        .bind(&manifest.parent_key.session_id)
        .bind(&manifest.parent_key.branch_id)
        .bind(&manifest.child_key.session_id)
        .bind(&manifest.child_key.branch_id)
        .bind(&idempotency_hash)
        .bind(&request_hash)
        .bind(to_json("fork_manifest", &manifest)?)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("insert_prepared_fork", source))?;
        sqlx::query(
            "INSERT INTO conversation_manifest_pins
             (isolation_domain, owner_user_id, pin_id, parent_session_id,
              parent_branch_id, manifest_root, pin_state)
             VALUES (?, ?, ?, ?, ?, ?, 'prepared')",
        )
        .bind(&manifest.parent_key.isolation_domain)
        .bind(&manifest.parent_key.owner_user_id)
        .bind(&manifest.fork_id)
        .bind(&manifest.parent_key.session_id)
        .bind(&manifest.parent_key.branch_id)
        .bind(&manifest.parent_head.latest_manifest_root)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("pin_prepared_fork", source))?;
        insert_fork_event(&mut tx, &manifest, 0, None, "prepared").await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_prepare_fork", source))?;
        Ok(manifest)
    }

    pub async fn activate(
        &self,
        parent_key: &SessionKeyV1,
        fork_id: &str,
        actor: &ActorContextV1,
        writer_ttl: Duration,
    ) -> Result<SessionForkActivationV1, SessionForkCoordinatorError> {
        validate_key_and_id(parent_key, fork_id)?;
        validate_duration(writer_ttl, MAX_WRITER_TTL, "writer TTL")?;
        let stored = self.load(parent_key, fork_id).await?;
        actor
            .validate_for(&stored.child_key)
            .map_err(|error| SessionForkCoordinatorError::Invalid(error.to_string()))?;
        if stored.state == SessionForkStateV1::Aborted {
            return Err(SessionForkCoordinatorError::Conflict);
        }
        let child_head = if stored.state == SessionForkStateV1::Prepared {
            self.context.activate_fork(&stored).await?
        } else {
            self.context
                .load_head(&stored.child_key)
                .await?
                .ok_or_else(|| {
                    SessionForkCoordinatorError::NeedsRepair(
                        "active fork has no child context head".into(),
                    )
                })?
        };
        let writer_idempotency = identity_hash(
            "activate-writer",
            &format!("{}\0{}", stored.fork_id, actor.actor_id),
        );
        let writer_lease = match self
            .context
            .acquire_writer(
                &stored.child_key,
                Some(&child_head.cursor),
                actor,
                writer_ttl,
                &writer_idempotency,
            )
            .await?
        {
            AcquireWriterOutcome::Acquired(lease)
            | AcquireWriterOutcome::AlreadyAcquired(lease) => lease,
            AcquireWriterOutcome::Conflict { .. } => {
                return Err(SessionForkCoordinatorError::WriterConflict);
            }
        };
        let manifest = self.load(parent_key, fork_id).await?;
        if manifest.state != SessionForkStateV1::Active {
            return Err(SessionForkCoordinatorError::NeedsRepair(
                "fork activation did not durably transition to active".into(),
            ));
        }
        let activation = SessionForkActivationV1 {
            manifest,
            child_head,
            writer_lease,
        };
        activation
            .validate()
            .map_err(|error| SessionForkCoordinatorError::NeedsRepair(error.to_string()))?;
        Ok(activation)
    }

    pub async fn abort(
        &self,
        parent_key: &SessionKeyV1,
        fork_id: &str,
        grace: Duration,
        detail: &str,
    ) -> Result<SessionForkManifestV1, SessionForkCoordinatorError> {
        validate_key_and_id(parent_key, fork_id)?;
        validate_duration(grace, MAX_ABORT_GRACE, "retention grace")?;
        validate_text("abort detail", detail, MAX_REASON_BYTES)?;
        let mut tx = self.begin("begin_abort_fork").await?;
        let row = lock_fork(&mut tx, parent_key, fork_id).await?;
        let mut manifest: SessionForkManifestV1 =
            decode_json_row(&row, "manifest_json", "fork_manifest")?;
        validate_stored_manifest(&manifest, parent_key)?;
        if manifest.state == SessionForkStateV1::Aborted {
            tx.commit()
                .await
                .map_err(|source| database_error("commit_abort_retry", source))?;
            return Ok(manifest);
        }
        if manifest.state != SessionForkStateV1::Prepared {
            return Err(SessionForkCoordinatorError::Conflict);
        }
        let now = database_now_ms(&mut tx).await?;
        let grace_expires_at_ms = checked_expiry(now, grace)?;
        manifest.state = SessionForkStateV1::Aborted;
        manifest.status_detail = Some(detail.to_owned());
        manifest
            .validate()
            .map_err(|error| SessionForkCoordinatorError::NeedsRepair(error.to_string()))?;
        sqlx::query(
            "UPDATE session_forks
             SET state = 'aborted', manifest_json = ?, updated_at = NOW(6)
             WHERE isolation_domain = ? AND owner_user_id = ? AND fork_id = ?
               AND state = 'prepared'",
        )
        .bind(to_json("aborted_fork", &manifest)?)
        .bind(&parent_key.isolation_domain)
        .bind(&parent_key.owner_user_id)
        .bind(fork_id)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("abort_fork", source))?;
        sqlx::query(
            "UPDATE conversation_manifest_pins
             SET pin_state = 'grace', grace_expires_at_ms = ?, updated_at = NOW(6)
             WHERE isolation_domain = ? AND owner_user_id = ? AND pin_id = ?
               AND pin_state = 'prepared'",
        )
        .bind(grace_expires_at_ms)
        .bind(&parent_key.isolation_domain)
        .bind(&parent_key.owner_user_id)
        .bind(fork_id)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("grace_aborted_fork_pin", source))?;
        insert_fork_event(&mut tx, &manifest, 1, Some("prepared"), "aborted").await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_abort_fork", source))?;
        Ok(manifest)
    }

    pub async fn load(
        &self,
        parent_key: &SessionKeyV1,
        fork_id: &str,
    ) -> Result<SessionForkManifestV1, SessionForkCoordinatorError> {
        validate_key_and_id(parent_key, fork_id)?;
        let row = sqlx::query(
            "SELECT manifest_json FROM session_forks
             WHERE isolation_domain = ? AND owner_user_id = ? AND fork_id = ?
               AND parent_session_id = ? AND parent_branch_id = ?",
        )
        .bind(&parent_key.isolation_domain)
        .bind(&parent_key.owner_user_id)
        .bind(fork_id)
        .bind(&parent_key.session_id)
        .bind(&parent_key.branch_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| database_error("load_fork", source))?
        .ok_or(SessionForkCoordinatorError::NotFound)?;
        let manifest = decode_json_row(&row, "manifest_json", "fork_manifest")?;
        validate_stored_manifest(&manifest, parent_key)?;
        Ok(manifest)
    }

    pub async fn list(
        &self,
        parent_key: &SessionKeyV1,
        limit: u32,
    ) -> Result<Vec<SessionForkManifestV1>, SessionForkCoordinatorError> {
        parent_key
            .validate()
            .map_err(|error| SessionForkCoordinatorError::Invalid(error.to_string()))?;
        if limit == 0 || limit > MAX_LIST_LIMIT {
            return Err(SessionForkCoordinatorError::Invalid(format!(
                "list limit must be between 1 and {MAX_LIST_LIMIT}"
            )));
        }
        let rows = sqlx::query(
            "SELECT manifest_json FROM session_forks
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND parent_session_id = ? AND parent_branch_id = ?
             ORDER BY created_at DESC, fork_id DESC LIMIT ?",
        )
        .bind(&parent_key.isolation_domain)
        .bind(&parent_key.owner_user_id)
        .bind(&parent_key.session_id)
        .bind(&parent_key.branch_id)
        .bind(i64::from(limit))
        .fetch_all(self.pool.get())
        .await
        .map_err(|source| database_error("list_forks", source))?;
        rows.iter()
            .map(|row| {
                let manifest = decode_json_row(row, "manifest_json", "fork_manifest")?;
                validate_stored_manifest(&manifest, parent_key)?;
                Ok(manifest)
            })
            .collect()
    }

    /// Release only expired grace pins. Immutable objects remain eligible for
    /// a separate reachability GC; request paths never scan or delete history.
    pub async fn release_expired_grace_pins(
        &self,
        limit: u32,
    ) -> Result<u64, SessionForkCoordinatorError> {
        if limit == 0 || limit > 10_000 {
            return Err(SessionForkCoordinatorError::Invalid(
                "cleanup limit must be between 1 and 10000".into(),
            ));
        }
        let mut tx = self.begin("begin_release_fork_pins").await?;
        let now = database_now_ms(&mut tx).await?;
        let result = sqlx::query(
            "DELETE FROM conversation_manifest_pins
             WHERE pin_state = 'grace' AND grace_expires_at_ms <= ?
             ORDER BY grace_expires_at_ms ASC LIMIT ?",
        )
        .bind(now)
        .bind(i64::from(limit))
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("release_expired_fork_pins", source))?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_release_fork_pins", source))?;
        Ok(result.rows_affected())
    }

    /// Collect manifest records whose owning session was deleted after every
    /// fork retention pin was released. Segment payload GC remains a separate
    /// owner-scoped reachability pass, so this bounded job never scans JSON or
    /// performs per-segment refcount fanout.
    pub async fn collect_unpinned_orphan_manifests(
        &self,
        limit: u32,
    ) -> Result<u64, SessionForkCoordinatorError> {
        if limit == 0 || limit > 10_000 {
            return Err(SessionForkCoordinatorError::Invalid(
                "cleanup limit must be between 1 and 10000".into(),
            ));
        }
        let mut tx = self.begin("begin_collect_orphan_manifests").await?;
        let now = database_now_ms(&mut tx).await?;
        let result = sqlx::query(
            "DELETE FROM conversation_manifest_nodes
             WHERE NOT EXISTS (
                       SELECT 1 FROM session_context_heads
                       WHERE session_context_heads.isolation_domain =
                                 conversation_manifest_nodes.isolation_domain
                         AND session_context_heads.owner_user_id =
                                 conversation_manifest_nodes.owner_user_id
                         AND session_context_heads.session_id =
                                 conversation_manifest_nodes.session_id
                         AND session_context_heads.branch_id =
                                 conversation_manifest_nodes.branch_id
                   )
               AND NOT EXISTS (
                       SELECT 1 FROM conversation_manifest_pins
                       WHERE conversation_manifest_pins.isolation_domain =
                                 conversation_manifest_nodes.isolation_domain
                         AND conversation_manifest_pins.owner_user_id =
                                 conversation_manifest_nodes.owner_user_id
                         AND conversation_manifest_pins.parent_session_id =
                                 conversation_manifest_nodes.session_id
                         AND conversation_manifest_pins.parent_branch_id =
                                 conversation_manifest_nodes.branch_id
                         AND (
                             conversation_manifest_pins.pin_state IN ('prepared', 'active')
                             OR (
                                 conversation_manifest_pins.pin_state = 'grace'
                                 AND conversation_manifest_pins.grace_expires_at_ms > ?
                             )
                         )
                   )
             ORDER BY created_at ASC LIMIT ?",
        )
        .bind(now)
        .bind(i64::from(limit))
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("collect_orphan_manifests", source))?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_collect_orphan_manifests", source))?;
        Ok(result.rows_affected())
    }

    /// Remove fork metadata bottom-up after its child session is deleted.
    ///
    /// An ancestor pin is deliberately retained while a live descendant fork
    /// still references that lineage. Repeated bounded passes eventually
    /// release a deleted chain without ever breaking an active descendant.
    pub async fn collect_orphaned_fork_records(
        &self,
        limit: u32,
    ) -> Result<u64, SessionForkCoordinatorError> {
        if limit == 0 || limit > 10_000 {
            return Err(SessionForkCoordinatorError::Invalid(
                "cleanup limit must be between 1 and 10000".into(),
            ));
        }
        let mut tx = self.begin("begin_collect_orphan_forks").await?;
        let rows = sqlx::query(
            "SELECT isolation_domain, owner_user_id, fork_id
             FROM session_forks
             WHERE NOT EXISTS (
                       SELECT 1 FROM agent_sessions
                       WHERE agent_sessions.user_id = session_forks.owner_user_id
                         AND agent_sessions.session_id = session_forks.child_session_id
                   )
               AND NOT EXISTS (
                       SELECT 1 FROM session_forks descendants
                       WHERE descendants.isolation_domain =
                                 session_forks.isolation_domain
                         AND descendants.owner_user_id = session_forks.owner_user_id
                         AND descendants.parent_session_id =
                                 session_forks.child_session_id
                         AND descendants.parent_branch_id =
                                 session_forks.child_branch_id
                         AND descendants.state IN ('prepared', 'active')
                   )
             ORDER BY updated_at ASC, fork_id ASC LIMIT ? FOR UPDATE",
        )
        .bind(i64::from(limit))
        .fetch_all(&mut *tx)
        .await
        .map_err(|source| database_error("load_orphan_forks", source))?;
        for row in &rows {
            let isolation_domain = row
                .try_get::<String, _>("isolation_domain")
                .map_err(|source| database_error("decode_orphan_fork", source))?;
            let owner_user_id = row
                .try_get::<String, _>("owner_user_id")
                .map_err(|source| database_error("decode_orphan_fork", source))?;
            let fork_id = row
                .try_get::<String, _>("fork_id")
                .map_err(|source| database_error("decode_orphan_fork", source))?;
            for (operation, sql) in [
                (
                    "delete_orphan_fork_pin",
                    "DELETE FROM conversation_manifest_pins
                     WHERE isolation_domain = ? AND owner_user_id = ? AND pin_id = ?",
                ),
                (
                    "delete_orphan_fork_events",
                    "DELETE FROM session_fork_events
                     WHERE isolation_domain = ? AND owner_user_id = ? AND fork_id = ?",
                ),
                (
                    "delete_orphan_fork",
                    "DELETE FROM session_forks
                     WHERE isolation_domain = ? AND owner_user_id = ? AND fork_id = ?",
                ),
            ] {
                sqlx::query(sql)
                    .bind(&isolation_domain)
                    .bind(&owner_user_id)
                    .bind(&fork_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|source| database_error(operation, source))?;
            }
        }
        tx.commit()
            .await
            .map_err(|source| database_error("commit_collect_orphan_forks", source))?;
        u64::try_from(rows.len())
            .map_err(|_| SessionForkCoordinatorError::Invalid("cleanup count overflow".into()))
    }

    async fn begin(
        &self,
        operation: &'static str,
    ) -> Result<Transaction<'_, MySql>, SessionForkCoordinatorError> {
        self.pool
            .get()
            .begin()
            .await
            .map_err(|source| database_error(operation, source))
    }
}

fn validate_prepare_request(
    request: &PrepareSessionForkV1,
) -> Result<(), SessionForkCoordinatorError> {
    request
        .parent_key
        .validate()
        .map_err(|error| SessionForkCoordinatorError::Invalid(error.to_string()))?;
    request
        .child_key
        .validate()
        .map_err(|error| SessionForkCoordinatorError::Invalid(error.to_string()))?;
    validate_text(
        "idempotency key",
        &request.idempotency_key,
        MAX_IDEMPOTENCY_BYTES,
    )?;
    validate_text("reason", &request.reason, MAX_REASON_BYTES)?;
    if request.parent_key.isolation_domain != request.child_key.isolation_domain
        || request.parent_key.owner_user_id != request.child_key.owner_user_id
        || request.parent_key == request.child_key
        || request.parent_key.session_id.len() > 64
        || request.child_key.session_id.len() > 64
        || !request
            .parent_key
            .validates_cursor(&request.expected_parent_cursor)
    {
        return Err(SessionForkCoordinatorError::Invalid(
            "parent, child, and expected cursor coordinates do not match".into(),
        ));
    }
    let conversation = request
        .dimensions
        .iter()
        .find(|evidence| evidence.dimension == ForkBasisDimensionV1::Conversation)
        .ok_or_else(|| {
            SessionForkCoordinatorError::Invalid("conversation fork evidence is missing".into())
        })?;
    if conversation.disposition != ForkDimensionDispositionV1::SharedPrefix
        || conversation.source_cursor.as_ref() != Some(&request.expected_parent_cursor)
        || conversation.evidence_digest.as_deref()
            != Some(request.expected_parent_cursor.canonical_root_hash.as_str())
    {
        return Err(SessionForkCoordinatorError::Invalid(
            "conversation evidence must bind the exact shared parent prefix".into(),
        ));
    }
    for evidence in &request.dimensions {
        if evidence
            .source_cursor
            .as_ref()
            .is_some_and(|cursor| cursor != &request.expected_parent_cursor)
            || (evidence.disposition != ForkDimensionDispositionV1::Gap
                && evidence.evidence_digest.is_none())
        {
            return Err(SessionForkCoordinatorError::Invalid(
                "fork dimension evidence is not bound to the expected parent cursor".into(),
            ));
        }
    }
    Ok(())
}

async fn insert_fork_child_session(
    tx: &mut Transaction<'_, MySql>,
    manifest: &SessionForkManifestV1,
) -> Result<(), SessionForkCoordinatorError> {
    let metadata = serde_json::json!({
        "fork_id": manifest.fork_id,
        "fork_parent_session_id": manifest.parent_key.session_id,
        "fork_parent_branch_id": manifest.parent_key.branch_id,
        "fork_parent_manifest_root": manifest.parent_head.latest_manifest_root,
    });
    let result = sqlx::query(
        "INSERT IGNORE INTO agent_sessions
         (session_id, user_id, agent_id, title, status, event_count, metadata,
          project_id, project_retention_policy, config_version_id,
          created_at, updated_at, last_active_at)
         SELECT ?, user_id, agent_id, CONCAT(COALESCE(title, 'Session'), ' (fork)'),
                'active', 0, ?, project_id, project_retention_policy, config_version_id,
                NOW(6), NOW(6), NOW(6)
         FROM agent_sessions
         WHERE user_id = ? AND session_id = ?",
    )
    .bind(&manifest.child_key.session_id)
    .bind(to_json("fork_session_metadata", &metadata)?)
    .bind(&manifest.parent_key.owner_user_id)
    .bind(&manifest.parent_key.session_id)
    .execute(&mut **tx)
    .await
    .map_err(|source| database_error("insert_fork_child_session", source))?;
    if result.rows_affected() != 1 {
        return Err(SessionForkCoordinatorError::Conflict);
    }
    Ok(())
}

async fn lock_parent_head(
    tx: &mut Transaction<'_, MySql>,
    request: &PrepareSessionForkV1,
) -> Result<astra_turn_types::SessionContextHeadV1, SessionForkCoordinatorError> {
    let row = sqlx::query(
        "SELECT head_json FROM session_context_heads
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ? FOR UPDATE",
    )
    .bind(&request.parent_key.isolation_domain)
    .bind(&request.parent_key.owner_user_id)
    .bind(&request.parent_key.session_id)
    .bind(&request.parent_key.branch_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| database_error("lock_fork_parent_head", source))?
    .ok_or(SessionForkCoordinatorError::NotFound)?;
    let current_head: astra_turn_types::SessionContextHeadV1 =
        decode_json_row(&row, "head_json", "parent_head")?;
    if current_head.key != request.parent_key {
        return Err(SessionForkCoordinatorError::NeedsRepair(
            "parent context head escaped its owner scope".into(),
        ));
    }
    if current_head.cursor == request.expected_parent_cursor {
        return Ok(current_head);
    }
    let retained = sqlx::query(
        "SELECT manifest_json, total_canonical_bytes, total_message_count
         FROM conversation_manifest_nodes
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ? AND manifest_root = ?
           AND reachable = 1 FOR UPDATE",
    )
    .bind(&request.parent_key.isolation_domain)
    .bind(&request.parent_key.owner_user_id)
    .bind(&request.parent_key.session_id)
    .bind(&request.parent_key.branch_id)
    .bind(&request.expected_parent_cursor.canonical_root_hash)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| database_error("lock_retained_fork_parent", source))?
    .ok_or(SessionForkCoordinatorError::Conflict)?;
    let node: astra_turn_types::ContextManifestNodeV1 =
        decode_json_row(&retained, "manifest_json", "retained_parent_manifest")?;
    node.validate()
        .map_err(|error| SessionForkCoordinatorError::NeedsRepair(error.to_string()))?;
    if node.key != request.parent_key || node.cursor() != request.expected_parent_cursor {
        return Err(SessionForkCoordinatorError::Conflict);
    }
    let total_canonical_bytes =
        database_u64(&retained, "total_canonical_bytes", "retained parent bytes")?;
    let total_message_count =
        database_u64(&retained, "total_message_count", "retained parent messages")?;
    Ok(astra_turn_types::SessionContextHeadV1 {
        schema_version: astra_turn_types::SESSION_COORDINATION_SCHEMA_VERSION,
        key: request.parent_key.clone(),
        cursor: request.expected_parent_cursor.clone(),
        latest_manifest_root: request.expected_parent_cursor.canonical_root_hash.clone(),
        total_canonical_bytes,
        total_message_count,
        writer_epoch: current_head.writer_epoch,
    })
}

async fn ensure_child_is_empty(
    tx: &mut Transaction<'_, MySql>,
    child_key: &SessionKeyV1,
) -> Result<(), SessionForkCoordinatorError> {
    let context_row = sqlx::query(
        "SELECT head_json, fork_base_json, active_writer_json, active_reservation_json
         FROM session_context_heads
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ? FOR UPDATE",
    )
    .bind(&child_key.isolation_domain)
    .bind(&child_key.owner_user_id)
    .bind(&child_key.session_id)
    .bind(&child_key.branch_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| database_error("lock_fork_child_head", source))?;
    if context_row.is_some_and(|row| {
        [
            "head_json",
            "fork_base_json",
            "active_writer_json",
            "active_reservation_json",
        ]
        .into_iter()
        .any(|column| {
            row.try_get::<Option<String>, _>(column)
                .ok()
                .flatten()
                .is_some()
        })
    }) {
        return Err(SessionForkCoordinatorError::Conflict);
    }
    let existing = sqlx::query(
        "SELECT fork_id FROM session_forks
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND child_session_id = ? AND child_branch_id = ? FOR UPDATE",
    )
    .bind(&child_key.isolation_domain)
    .bind(&child_key.owner_user_id)
    .bind(&child_key.session_id)
    .bind(&child_key.branch_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| database_error("lock_existing_child_fork", source))?;
    if existing.is_some() {
        return Err(SessionForkCoordinatorError::Conflict);
    }
    Ok(())
}

async fn lock_fork(
    tx: &mut Transaction<'_, MySql>,
    parent_key: &SessionKeyV1,
    fork_id: &str,
) -> Result<sqlx::mysql::MySqlRow, SessionForkCoordinatorError> {
    sqlx::query(
        "SELECT manifest_json FROM session_forks
         WHERE isolation_domain = ? AND owner_user_id = ? AND fork_id = ?
           AND parent_session_id = ? AND parent_branch_id = ? FOR UPDATE",
    )
    .bind(&parent_key.isolation_domain)
    .bind(&parent_key.owner_user_id)
    .bind(fork_id)
    .bind(&parent_key.session_id)
    .bind(&parent_key.branch_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| database_error("lock_fork", source))?
    .ok_or(SessionForkCoordinatorError::NotFound)
}

async fn insert_fork_event(
    tx: &mut Transaction<'_, MySql>,
    manifest: &SessionForkManifestV1,
    transition_seq: u64,
    from_state: Option<&str>,
    to_state: &str,
) -> Result<(), SessionForkCoordinatorError> {
    sqlx::query(
        "INSERT IGNORE INTO session_fork_events
         (isolation_domain, owner_user_id, fork_id, transition_seq,
          parent_session_id, child_session_id, from_state, to_state, event_json)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&manifest.parent_key.isolation_domain)
    .bind(&manifest.parent_key.owner_user_id)
    .bind(&manifest.fork_id)
    .bind(i64::try_from(transition_seq).map_err(|_| {
        SessionForkCoordinatorError::Invalid("fork transition sequence exceeds BIGINT".into())
    })?)
    .bind(&manifest.parent_key.session_id)
    .bind(&manifest.child_key.session_id)
    .bind(from_state)
    .bind(to_state)
    .bind(to_json("fork_event", manifest)?)
    .execute(&mut **tx)
    .await
    .map_err(|source| database_error("insert_fork_event", source))?;
    Ok(())
}

fn validate_key_and_id(
    key: &SessionKeyV1,
    fork_id: &str,
) -> Result<(), SessionForkCoordinatorError> {
    key.validate()
        .map_err(|error| SessionForkCoordinatorError::Invalid(error.to_string()))?;
    validate_text("fork id", fork_id, 128)
}

fn validate_stored_manifest(
    manifest: &SessionForkManifestV1,
    parent_key: &SessionKeyV1,
) -> Result<(), SessionForkCoordinatorError> {
    manifest
        .validate()
        .map_err(|error| SessionForkCoordinatorError::NeedsRepair(error.to_string()))?;
    if &manifest.parent_key != parent_key {
        return Err(SessionForkCoordinatorError::NeedsRepair(
            "stored fork parent key does not match lookup scope".into(),
        ));
    }
    Ok(())
}

fn validate_text(
    field: &str,
    value: &str,
    maximum: usize,
) -> Result<(), SessionForkCoordinatorError> {
    if value.trim().is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        Err(SessionForkCoordinatorError::Invalid(format!(
            "{field} is invalid"
        )))
    } else {
        Ok(())
    }
}

fn validate_duration(
    duration: Duration,
    maximum: Duration,
    field: &str,
) -> Result<(), SessionForkCoordinatorError> {
    if duration.is_zero() || duration > maximum {
        Err(SessionForkCoordinatorError::Invalid(format!(
            "{field} is outside the supported range"
        )))
    } else {
        Ok(())
    }
}

fn checked_expiry(now: i64, duration: Duration) -> Result<i64, SessionForkCoordinatorError> {
    let millis = i64::try_from(duration.as_millis())
        .map_err(|_| SessionForkCoordinatorError::Invalid("duration exceeds i64".into()))?;
    now.checked_add(millis)
        .ok_or_else(|| SessionForkCoordinatorError::Invalid("deadline overflow".into()))
}

async fn database_now_ms(
    tx: &mut Transaction<'_, MySql>,
) -> Result<i64, SessionForkCoordinatorError> {
    crate::db_row::database_now_unix_ms(tx)
        .await
        .map_err(|source| database_error("load_fork_time", source))
}

fn stable_hash<T: Serialize>(
    domain: &[u8],
    value: &T,
) -> Result<String, SessionForkCoordinatorError> {
    let mut digest = Sha256::new();
    digest.update(domain);
    let bytes = serde_json::to_vec(value).map_err(|source| SessionForkCoordinatorError::Json {
        entity: "request_hash",
        source,
    })?;
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
    Ok(format!("{:x}", digest.finalize()))
}

fn identity_hash(operation: &str, value: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"astra.session-fork-idempotency.v1\0");
    digest.update((operation.len() as u64).to_be_bytes());
    digest.update(operation.as_bytes());
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value.as_bytes());
    format!("{:x}", digest.finalize())
}

fn verify_request_hash(
    row: &sqlx::mysql::MySqlRow,
    expected: &str,
) -> Result<(), SessionForkCoordinatorError> {
    let stored: String = row
        .try_get("request_hash")
        .map_err(|source| database_error("decode_request_hash", source))?;
    if stored != expected {
        return Err(SessionForkCoordinatorError::IdempotencyMismatch);
    }
    Ok(())
}

fn decode_json_row<T: DeserializeOwned>(
    row: &sqlx::mysql::MySqlRow,
    column: &'static str,
    entity: &'static str,
) -> Result<T, SessionForkCoordinatorError> {
    let json: Option<String> = row
        .try_get(column)
        .map_err(|source| database_error("decode_fork_row", source))?;
    let json = json.ok_or_else(|| {
        SessionForkCoordinatorError::NeedsRepair(format!("{entity} JSON is missing"))
    })?;
    serde_json::from_str(&json)
        .map_err(|source| SessionForkCoordinatorError::Json { entity, source })
}

fn to_json<T: Serialize>(
    entity: &'static str,
    value: &T,
) -> Result<String, SessionForkCoordinatorError> {
    serde_json::to_string(value)
        .map_err(|source| SessionForkCoordinatorError::Json { entity, source })
}

fn database_u64(
    row: &sqlx::mysql::MySqlRow,
    column: &'static str,
    field: &str,
) -> Result<u64, SessionForkCoordinatorError> {
    let value = row
        .try_get::<i64, _>(column)
        .map_err(|source| database_error("decode_fork_integer", source))?;
    u64::try_from(value)
        .map_err(|_| SessionForkCoordinatorError::NeedsRepair(format!("{field} is negative")))
}

fn database_error(operation: &'static str, source: sqlx::Error) -> SessionForkCoordinatorError {
    SessionForkCoordinatorError::Database { operation, source }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, OnceLock};

    use astra_turn_types::{
        ActorKindV1, AuthorityEpochsV1, CANONICAL_TURN_DELTA_SCHEMA_VERSION, CanonicalTurnDeltaV1,
        CoordinatorMutationV1, SessionSurfaceV1,
    };
    use serde_json::json;
    use tokio::sync::OnceCell;

    use super::*;
    use crate::{DatabaseSessionContextCoordinator, ReserveTurnOutcome, SessionContextCoordinator};

    static FORK_DB: OnceLock<OnceCell<SharedPool>> = OnceLock::new();

    async fn setup_fork_db_it() -> SharedPool {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
        );
        FORK_DB
            .get_or_init(OnceCell::new)
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

    fn dimensions(cursor: &SessionCursorV1) -> Vec<ForkDimensionEvidenceV1> {
        [
            ForkBasisDimensionV1::Conversation,
            ForkBasisDimensionV1::TaskBoard,
            ForkBasisDimensionV1::Checkpoint,
            ForkBasisDimensionV1::Workspace,
            ForkBasisDimensionV1::Artifacts,
        ]
        .into_iter()
        .map(|dimension| {
            let conversation = dimension == ForkBasisDimensionV1::Conversation;
            ForkDimensionEvidenceV1 {
                dimension,
                disposition: if conversation {
                    ForkDimensionDispositionV1::SharedPrefix
                } else {
                    ForkDimensionDispositionV1::Gap
                },
                source_cursor: conversation.then(|| cursor.clone()),
                evidence_digest: conversation.then(|| cursor.canonical_root_hash.clone()),
                detail: (!conversation).then(|| "not present in the fixture".into()),
            }
        })
        .collect()
    }

    fn delta(turn: u32) -> CanonicalTurnDeltaV1 {
        CanonicalTurnDeltaV1 {
            schema_version: CANONICAL_TURN_DELTA_SCHEMA_VERSION,
            completed_turn: turn,
            journal_event_seq: u64::from(turn),
            conversation_seq: u64::from(turn),
            compaction_generation: 0,
            config_version_id: None,
            logical_segments: vec![vec![
                json!({"role":"user","content":format!("question-{turn}")}),
                json!({"role":"assistant","content":format!("answer-{turn}")}),
            ]],
        }
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn durable_fork_is_atomic_owner_isolated_and_does_not_copy_history() {
        let pool = setup_fork_db_it().await;
        let suffix = Uuid::new_v4().simple().to_string();
        let owner = format!("fork-owner-{suffix}");
        let parent_key =
            SessionKeyV1::owner_session("fork-it", &owner, format!("parent-{suffix}"), "main");
        let child_key =
            SessionKeyV1::owner_session("fork-it", &owner, format!("child-{suffix}"), "main");
        sqlx::query(
            "INSERT INTO agent_sessions
             (session_id, user_id, title, status, event_count, metadata)
             VALUES (?, ?, 'fork integration parent', 'active', 0, '{}')",
        )
        .bind(&parent_key.session_id)
        .bind(&owner)
        .execute(pool.get())
        .await
        .expect("insert parent session");

        let context: Arc<dyn SessionContextCoordinator> =
            Arc::new(DatabaseSessionContextCoordinator::new(pool.clone()));
        let service = DatabaseSessionForkCoordinator::new(pool.clone(), context.clone());
        let parent_actor = actor(&owner, "parent");
        let parent_lease = match context
            .acquire_writer(
                &parent_key,
                None,
                &parent_actor,
                Duration::from_secs(60),
                "parent-writer",
            )
            .await
            .expect("acquire parent")
        {
            AcquireWriterOutcome::Acquired(lease) => lease,
            other => panic!("unexpected parent acquire {other:?}"),
        };
        let mut cursor = None;
        let mut retained_fork_cursor = None;
        for turn in 1..=3 {
            let reservation = match context
                .reserve_turn(
                    &parent_lease,
                    cursor.as_ref(),
                    Duration::from_secs(30),
                    &format!("parent-reserve-{turn}"),
                )
                .await
                .expect("reserve parent")
            {
                ReserveTurnOutcome::Reserved(reservation) => reservation,
                other => panic!("unexpected parent reservation {other:?}"),
            };
            cursor = Some(
                match context
                    .commit_turn(&reservation, delta(turn), &format!("parent-commit-{turn}"))
                    .await
                    .expect("commit parent")
                {
                    CoordinatorMutationV1::Applied { cursor } => cursor,
                    other => panic!("unexpected parent commit {other:?}"),
                },
            );
            if turn == 2 {
                retained_fork_cursor = cursor.clone();
            }
        }
        let current_parent_cursor = cursor.expect("parent cursor");
        let fork_cursor = retained_fork_cursor.expect("retained fork cursor");
        let request = PrepareSessionForkV1 {
            idempotency_key: "prepare-exact".into(),
            parent_key: parent_key.clone(),
            child_key: child_key.clone(),
            expected_parent_cursor: fork_cursor.clone(),
            dimensions: dimensions(&fork_cursor),
            reason: "integration fork".into(),
        };
        let prepared = service.prepare(&request).await.expect("prepare fork");
        assert_eq!(
            service.prepare(&request).await.expect("retry prepare"),
            prepared
        );
        assert!(
            context
                .load_head(&child_key)
                .await
                .expect("load prepared child")
                .is_none(),
            "a prepared/partial fork must not expose an activatable child head"
        );
        let child_manifest_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversation_manifest_nodes
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?",
        )
        .bind(&child_key.isolation_domain)
        .bind(&child_key.owner_user_id)
        .bind(&child_key.session_id)
        .bind(&child_key.branch_id)
        .fetch_one(pool.get())
        .await
        .expect("count prepared child manifests");
        assert_eq!(child_manifest_count, 0);

        let activation = service
            .activate(
                &parent_key,
                &prepared.fork_id,
                &actor(&owner, "child"),
                Duration::from_secs(60),
            )
            .await
            .expect("activate fork");
        assert_eq!(activation.manifest.state, SessionForkStateV1::Active);
        let child_manifest_count_after: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversation_manifest_nodes
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?",
        )
        .bind(&child_key.isolation_domain)
        .bind(&child_key.owner_user_id)
        .bind(&child_key.session_id)
        .bind(&child_key.branch_id)
        .fetch_one(pool.get())
        .await
        .expect("count active child manifests");
        assert_eq!(
            child_manifest_count_after, 0,
            "activation must be O(1) metadata, not an O(history) copy"
        );
        let cold = context
            .load_manifest_delta(&child_key, None)
            .await
            .expect("load child prefix");
        assert_eq!(cold.shared_prefix, Some(prepared.shared_prefix()));
        assert!(cold.missing_nodes.is_empty());
        let pin_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversation_manifest_pins
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND pin_id = ? AND pin_state = 'active'",
        )
        .bind(&parent_key.isolation_domain)
        .bind(&owner)
        .bind(&prepared.fork_id)
        .fetch_one(pool.get())
        .await
        .expect("count active retention pin");
        assert_eq!(pin_count, 1);

        let child_reservation = match context
            .reserve_turn(
                &activation.writer_lease,
                Some(&activation.child_head.cursor),
                Duration::from_secs(30),
                "child-reserve-3",
            )
            .await
            .expect("reserve child")
        {
            ReserveTurnOutcome::Reserved(reservation) => reservation,
            other => panic!("unexpected child reservation {other:?}"),
        };
        let child_cursor = match context
            .commit_turn(&child_reservation, delta(3), "child-commit-3")
            .await
            .expect("commit child")
        {
            CoordinatorMutationV1::Applied { cursor } => cursor,
            other => panic!("unexpected child commit {other:?}"),
        };
        let parent_reservation = match context
            .reserve_turn(
                &parent_lease,
                Some(&current_parent_cursor),
                Duration::from_secs(30),
                "parent-reserve-4",
            )
            .await
            .expect("reserve parent after fork")
        {
            ReserveTurnOutcome::Reserved(reservation) => reservation,
            other => panic!("unexpected parent reservation after fork {other:?}"),
        };
        let parent_cursor = match context
            .commit_turn(&parent_reservation, delta(4), "parent-commit-4")
            .await
            .expect("commit parent after fork")
        {
            CoordinatorMutationV1::Applied { cursor } => cursor,
            other => panic!("unexpected parent commit after fork {other:?}"),
        };
        assert_ne!(
            child_cursor.canonical_root_hash,
            parent_cursor.canonical_root_hash
        );
        let child_materialized = context
            .materialize(
                &context
                    .load_head(&child_key)
                    .await
                    .expect("load child head")
                    .expect("child head"),
            )
            .await
            .expect("materialize child");
        let parent_materialized = context
            .materialize(
                &context
                    .load_head(&parent_key)
                    .await
                    .expect("load parent head")
                    .expect("parent head"),
            )
            .await
            .expect("materialize parent");
        assert_eq!(child_materialized.messages.len(), 6);
        assert_eq!(parent_materialized.messages.len(), 8);

        let other_owner_key = SessionKeyV1::owner_session(
            "fork-it",
            format!("other-{suffix}"),
            &parent_key.session_id,
            "main",
        );
        assert!(matches!(
            service.load(&other_owner_key, &prepared.fork_id).await,
            Err(SessionForkCoordinatorError::NotFound)
        ));
    }
}
