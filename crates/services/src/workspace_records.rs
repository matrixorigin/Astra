use std::collections::HashMap;
use std::sync::Arc;

use astra_core::SharedPool;
use astra_runtime_env::{CleanupReason, WorkspaceRecord, WorkspaceSource, validate_workspace_id};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::{Row, mysql::MySqlRow};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceRecordEntry {
    pub owner_id: String,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub record: WorkspaceRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceCleanupDebtEntry {
    pub debt_id: String,
    pub owner_id: String,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub workspace_id: String,
    pub reason: CleanupReason,
    pub message: String,
    pub attempts: u32,
    pub record: WorkspaceRecord,
    /// When the debt was first recorded (UTC RFC3339).
    /// Populated by DB queries; defaults to empty string for in-memory/test usage.
    #[serde(default)]
    pub created_at: String,
}

impl WorkspaceCleanupDebtEntry {
    pub fn new(
        owner_id: impl Into<String>,
        session_id: Option<String>,
        run_id: Option<String>,
        record: WorkspaceRecord,
        reason: CleanupReason,
        message: impl Into<String>,
    ) -> Self {
        Self {
            debt_id: format!("cleanup-debt-{}", Uuid::now_v7()),
            owner_id: owner_id.into(),
            session_id,
            run_id,
            workspace_id: record.workspace_id.clone(),
            reason,
            message: message.into(),
            attempts: 0,
            record,
            created_at: String::new(),
        }
    }
}

impl WorkspaceRecordEntry {
    pub fn new(
        owner_id: impl Into<String>,
        session_id: Option<String>,
        run_id: Option<String>,
        record: WorkspaceRecord,
    ) -> Self {
        Self {
            owner_id: owner_id.into(),
            session_id,
            run_id,
            record,
        }
    }

    pub fn workspace_id(&self) -> &str {
        &self.record.workspace_id
    }
}

#[derive(Debug, Error)]
pub enum WorkspaceRecordStoreError {
    #[error("workspace owner id must not be empty")]
    InvalidOwnerId,
    #[error("session id must not be empty when provided")]
    InvalidSessionId,
    #[error("run id must not be empty when provided")]
    InvalidRunId,
    #[error("invalid workspace id: {0}")]
    InvalidWorkspaceId(String),
    #[error("workspace '{workspace_id}' is already owned by another principal")]
    WorkspaceOwnerConflict { workspace_id: String },
    #[error("workspace source '{source_key}' is already claimed by another workspace")]
    SourceOwnerConflict { source_key: String },
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("workspace record store unavailable: {0}")]
    Unavailable(String),
}

#[derive(Debug, Error)]
pub enum WorkspaceCleanupDebtStoreError {
    #[error("workspace cleanup debt owner id must not be empty")]
    InvalidOwnerId,
    #[error("session id must not be empty when provided")]
    InvalidSessionId,
    #[error("run id must not be empty when provided")]
    InvalidRunId,
    #[error("invalid workspace id: {0}")]
    InvalidWorkspaceId(String),
    #[error("workspace cleanup debt id must not be empty")]
    InvalidDebtId,
    #[error("workspace cleanup debt '{debt_id}' is already owned by another principal")]
    CleanupDebtOwnerConflict { debt_id: String },
    #[error("workspace cleanup debt message must not be empty")]
    InvalidMessage,
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("workspace cleanup debt store unavailable: {0}")]
    Unavailable(String),
}

#[async_trait]
pub trait WorkspaceRecordStore: Send + Sync {
    async fn upsert_workspace_record(
        &self,
        entry: WorkspaceRecordEntry,
    ) -> Result<(), WorkspaceRecordStoreError>;

    async fn load_workspace_record(
        &self,
        owner_id: &str,
        workspace_id: &str,
    ) -> Result<Option<WorkspaceRecordEntry>, WorkspaceRecordStoreError>;

    async fn list_workspace_records(
        &self,
        owner_id: &str,
        limit: u32,
    ) -> Result<Vec<WorkspaceRecordEntry>, WorkspaceRecordStoreError>;

    async fn delete_workspace_record(
        &self,
        owner_id: &str,
        workspace_id: &str,
    ) -> Result<bool, WorkspaceRecordStoreError>;
}

#[async_trait]
pub trait WorkspaceCleanupDebtStore: Send + Sync {
    async fn record_cleanup_debt(
        &self,
        entry: WorkspaceCleanupDebtEntry,
    ) -> Result<(), WorkspaceCleanupDebtStoreError>;

    async fn list_cleanup_debts(
        &self,
        owner_id: &str,
        limit: u32,
    ) -> Result<Vec<WorkspaceCleanupDebtEntry>, WorkspaceCleanupDebtStoreError>;

    async fn resolve_cleanup_debt(
        &self,
        owner_id: &str,
        debt_id: &str,
    ) -> Result<bool, WorkspaceCleanupDebtStoreError>;

    /// List all unresolved debts across all owners (for background retry).
    async fn list_all_unresolved_debts(
        &self,
    ) -> Result<Vec<WorkspaceCleanupDebtEntry>, WorkspaceCleanupDebtStoreError>;

    /// Increment the attempts counter for a debt entry.
    async fn increment_debt_attempts(
        &self,
        debt_id: &str,
    ) -> Result<(), WorkspaceCleanupDebtStoreError>;
}

pub trait WorkspaceStateStore: WorkspaceRecordStore + WorkspaceCleanupDebtStore {}

impl<T> WorkspaceStateStore for T where T: WorkspaceRecordStore + WorkspaceCleanupDebtStore {}

#[derive(Clone, Default)]
pub struct InMemoryWorkspaceRecordStore {
    records: Arc<tokio::sync::RwLock<HashMap<String, WorkspaceRecordEntry>>>,
    cleanup_debts: Arc<tokio::sync::RwLock<HashMap<String, WorkspaceCleanupDebtEntry>>>,
}

impl InMemoryWorkspaceRecordStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl WorkspaceRecordStore for InMemoryWorkspaceRecordStore {
    async fn upsert_workspace_record(
        &self,
        entry: WorkspaceRecordEntry,
    ) -> Result<(), WorkspaceRecordStoreError> {
        validate_entry(&entry)?;
        let mut records = self.records.write().await;
        if let Some(existing) = records.get(&entry.record.workspace_id)
            && existing.owner_id != entry.owner_id
        {
            return Err(WorkspaceRecordStoreError::WorkspaceOwnerConflict {
                workspace_id: entry.record.workspace_id,
            });
        }
        if let Some(source_key) = workspace_source_key(&entry.record)
            && records.values().any(|existing| {
                existing.record.workspace_id != entry.record.workspace_id
                    && workspace_source_key(&existing.record).as_deref() == Some(&source_key)
            })
        {
            return Err(WorkspaceRecordStoreError::SourceOwnerConflict { source_key });
        }
        records.insert(entry.record.workspace_id.clone(), entry);
        Ok(())
    }

    async fn load_workspace_record(
        &self,
        owner_id: &str,
        workspace_id: &str,
    ) -> Result<Option<WorkspaceRecordEntry>, WorkspaceRecordStoreError> {
        validate_owner_id(owner_id)?;
        validate_workspace_id(workspace_id)
            .map_err(|error| WorkspaceRecordStoreError::InvalidWorkspaceId(error.to_string()))?;
        Ok(self
            .records
            .read()
            .await
            .get(workspace_id)
            .filter(|entry| entry.owner_id == owner_id)
            .cloned())
    }

    async fn list_workspace_records(
        &self,
        owner_id: &str,
        limit: u32,
    ) -> Result<Vec<WorkspaceRecordEntry>, WorkspaceRecordStoreError> {
        validate_owner_id(owner_id)?;
        let mut records = self
            .records
            .read()
            .await
            .values()
            .filter(|entry| entry.owner_id == owner_id)
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by(|left, right| left.record.workspace_id.cmp(&right.record.workspace_id));
        records.truncate(limit.max(1) as usize);
        Ok(records)
    }

    async fn delete_workspace_record(
        &self,
        owner_id: &str,
        workspace_id: &str,
    ) -> Result<bool, WorkspaceRecordStoreError> {
        validate_owner_id(owner_id)?;
        validate_workspace_id(workspace_id)
            .map_err(|error| WorkspaceRecordStoreError::InvalidWorkspaceId(error.to_string()))?;
        let mut records = self.records.write().await;
        if records
            .get(workspace_id)
            .is_some_and(|entry| entry.owner_id == owner_id)
        {
            records.remove(workspace_id);
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

#[async_trait]
impl WorkspaceCleanupDebtStore for InMemoryWorkspaceRecordStore {
    async fn record_cleanup_debt(
        &self,
        entry: WorkspaceCleanupDebtEntry,
    ) -> Result<(), WorkspaceCleanupDebtStoreError> {
        validate_cleanup_debt_entry(&entry)?;
        let mut debts = self.cleanup_debts.write().await;
        if let Some(existing) = debts.get(&entry.debt_id)
            && existing.owner_id != entry.owner_id
        {
            return Err(WorkspaceCleanupDebtStoreError::CleanupDebtOwnerConflict {
                debt_id: entry.debt_id,
            });
        }
        debts.insert(entry.debt_id.clone(), entry);
        Ok(())
    }

    async fn list_cleanup_debts(
        &self,
        owner_id: &str,
        limit: u32,
    ) -> Result<Vec<WorkspaceCleanupDebtEntry>, WorkspaceCleanupDebtStoreError> {
        validate_cleanup_owner_id(owner_id)?;
        let mut debts = self
            .cleanup_debts
            .read()
            .await
            .values()
            .filter(|entry| entry.owner_id == owner_id)
            .cloned()
            .collect::<Vec<_>>();
        debts.sort_by(|left, right| left.debt_id.cmp(&right.debt_id));
        debts.truncate(limit.max(1) as usize);
        Ok(debts)
    }

    async fn resolve_cleanup_debt(
        &self,
        owner_id: &str,
        debt_id: &str,
    ) -> Result<bool, WorkspaceCleanupDebtStoreError> {
        validate_cleanup_owner_id(owner_id)?;
        validate_debt_id(debt_id)?;
        let mut debts = self.cleanup_debts.write().await;
        if debts
            .get(debt_id)
            .is_some_and(|entry| entry.owner_id == owner_id)
        {
            debts.remove(debt_id);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn list_all_unresolved_debts(
        &self,
    ) -> Result<Vec<WorkspaceCleanupDebtEntry>, WorkspaceCleanupDebtStoreError> {
        let debts = self.cleanup_debts.read().await;
        let mut all_debts = debts.values().cloned().collect::<Vec<_>>();
        all_debts.sort_by(|left, right| left.created_at.cmp(&right.created_at));
        Ok(all_debts)
    }

    async fn increment_debt_attempts(
        &self,
        debt_id: &str,
    ) -> Result<(), WorkspaceCleanupDebtStoreError> {
        validate_debt_id(debt_id)?;
        let mut debts = self.cleanup_debts.write().await;
        if let Some(entry) = debts.get_mut(debt_id) {
            entry.attempts += 1;
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct DatabaseWorkspaceRecordStore {
    pool: SharedPool,
}

pub(crate) const WORKSPACE_RECORDS_CREATE_SQL: &str = r#"
            CREATE TABLE IF NOT EXISTS workspace_records (
                workspace_id        VARCHAR(255) PRIMARY KEY,
                owner_id            VARCHAR(255) NOT NULL,
                session_id          VARCHAR(255) NULL,
                run_id              VARCHAR(255) NULL,
                kind                VARCHAR(64)  NOT NULL,
                authority           VARCHAR(64)  NOT NULL,
                persistence         VARCHAR(64)  NOT NULL,
                root_or_volume_ref  LONGTEXT     NOT NULL,
                source_json         LONGTEXT     NOT NULL,
                revision            VARCHAR(255) NOT NULL,
                display_name        VARCHAR(255) NOT NULL,
                source_key          VARCHAR(512) NULL,
                record_json         LONGTEXT     NOT NULL,
                created_at          DATETIME(6)  NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
                updated_at          DATETIME(6)  NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
                UNIQUE INDEX ux_source_key (source_key)
            )
            "#;

pub(crate) const WORKSPACE_CLEANUP_DEBTS_CREATE_SQL: &str = r#"
            CREATE TABLE IF NOT EXISTS workspace_cleanup_debts (
                debt_id            VARCHAR(255) PRIMARY KEY,
                owner_id           VARCHAR(255) NOT NULL,
                session_id         VARCHAR(255) NULL,
                run_id             VARCHAR(255) NULL,
                workspace_id       VARCHAR(255) NOT NULL,
                reason             VARCHAR(64)  NOT NULL,
                message            LONGTEXT     NOT NULL,
                attempts           INT          NOT NULL DEFAULT 0,
                record_json        LONGTEXT     NOT NULL,
                created_at         DATETIME(6)  NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
                updated_at         DATETIME(6)  NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
                resolved_at        DATETIME(6)  NULL
            )
            "#;

impl DatabaseWorkspaceRecordStore {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }
}

pub(crate) async fn verify_workspace_record_tables(
    pool: &sqlx::Pool<sqlx::MySql>,
) -> Result<(), sqlx::Error> {
    verify_workspace_records_source_key_contract(pool).await
}

async fn verify_workspace_records_source_key_contract(
    pool: &sqlx::Pool<sqlx::MySql>,
) -> Result<(), sqlx::Error> {
    let column_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'workspace_records' \
           AND COLUMN_NAME = 'source_key' AND IS_NULLABLE = 'YES' \
           AND UPPER(DATA_TYPE) = 'VARCHAR' AND CHARACTER_MAXIMUM_LENGTH >= 512",
    )
    .fetch_one(pool)
    .await?;
    if column_count != 1 {
        return Err(sqlx::Error::Protocol(
            "workspace_records.source_key must be nullable VARCHAR(512) in the canonical schema"
                .to_string(),
        ));
    }
    let index_columns: Vec<String> = sqlx::query_scalar(
        "SELECT COLUMN_NAME FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'workspace_records' \
           AND INDEX_NAME = 'ux_source_key' AND NON_UNIQUE = 0 \
         ORDER BY SEQ_IN_INDEX",
    )
    .fetch_all(pool)
    .await?;
    if index_columns != ["source_key"] {
        return Err(sqlx::Error::Protocol(
            "workspace_records.ux_source_key must uniquely index only source_key".to_string(),
        ));
    }
    Ok(())
}

#[async_trait]
impl WorkspaceRecordStore for DatabaseWorkspaceRecordStore {
    async fn upsert_workspace_record(
        &self,
        entry: WorkspaceRecordEntry,
    ) -> Result<(), WorkspaceRecordStoreError> {
        validate_entry(&entry)?;
        let record_json = serde_json::to_string(&entry.record)?;
        let source_json = serde_json::to_string(&entry.record.source)?;
        let kind = serde_json_string(&entry.record.kind)?;
        let authority = serde_json_string(&entry.record.authority)?;
        let persistence = serde_json_string(&entry.record.persistence)?;
        let source_key = workspace_source_key(&entry.record);

        // Optimistic INSERT first — 1 round-trip for the common case (new record).
        // Falls through to validation + UPDATE only on duplicate key.
        match sqlx::query(
            "INSERT INTO workspace_records \
             (workspace_id, owner_id, session_id, run_id, kind, authority, persistence, \
              root_or_volume_ref, source_json, revision, display_name, source_key, record_json, \
              created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6))",
        )
        .bind(&entry.record.workspace_id)
        .bind(&entry.owner_id)
        .bind(entry.session_id.as_deref())
        .bind(entry.run_id.as_deref())
        .bind(&kind)
        .bind(&authority)
        .bind(&persistence)
        .bind(&entry.record.root_or_volume_ref)
        .bind(&source_json)
        .bind(&entry.record.revision)
        .bind(&entry.record.display_name)
        .bind(source_key.as_deref())
        .bind(&record_json)
        .execute(self.pool.get())
        .await
        {
            Ok(_) => return Ok(()),
            Err(error) => {
                if !astra_core::is_duplicate_key_error(&error) {
                    return Err(error.into());
                }
                // Duplicate key — fall through to validate ownership, then UPDATE.
            }
        }

        // Existing record found — validate ownership.
        let existing: Option<(String,)> =
            sqlx::query_as("SELECT owner_id FROM workspace_records WHERE workspace_id = ?")
                .bind(&entry.record.workspace_id)
                .fetch_optional(self.pool.get())
                .await?;

        if let Some((existing_owner,)) = existing
            && existing_owner != entry.owner_id
        {
            return Err(WorkspaceRecordStoreError::WorkspaceOwnerConflict {
                workspace_id: entry.record.workspace_id,
            });
        }

        // Check source_key conflict before UPDATE so production behavior
        // matches the in-memory store and returns a domain error instead of a
        // raw duplicate-key database error.
        if let Some(source_key) = source_key.as_ref() {
            let conflict: Option<(String,)> = sqlx::query_as(
                "SELECT owner_id FROM workspace_records \
                 WHERE source_key = ? AND workspace_id <> ? LIMIT 1",
            )
            .bind(source_key)
            .bind(&entry.record.workspace_id)
            .fetch_optional(self.pool.get())
            .await?;
            if conflict.is_some() {
                return Err(WorkspaceRecordStoreError::SourceOwnerConflict {
                    source_key: source_key.clone(),
                });
            }
        }

        // Guard the UPDATE with `owner_id = ?` so that a concurrent owner change
        // between the SELECT above and this UPDATE cannot silently clobber the
        // new owner. If `rows_affected == 0`, either the row was deleted or
        // another owner now holds it — treat both as a conflict.
        let result = sqlx::query(
            "UPDATE workspace_records \
             SET owner_id = ?, session_id = ?, run_id = ?, kind = ?, authority = ?, \
                 persistence = ?, root_or_volume_ref = ?, source_json = ?, revision = ?, \
                 display_name = ?, source_key = ?, record_json = ?, updated_at = NOW(6) \
             WHERE workspace_id = ? AND owner_id = ?",
        )
        .bind(&entry.owner_id)
        .bind(entry.session_id.as_deref())
        .bind(entry.run_id.as_deref())
        .bind(&kind)
        .bind(&authority)
        .bind(&persistence)
        .bind(&entry.record.root_or_volume_ref)
        .bind(&source_json)
        .bind(&entry.record.revision)
        .bind(&entry.record.display_name)
        .bind(source_key.as_deref())
        .bind(&record_json)
        .bind(&entry.record.workspace_id)
        .bind(&entry.owner_id)
        .execute(self.pool.get())
        .await?;
        if result.rows_affected() == 0 {
            // Row vanished or was re-owned concurrently — surface as conflict so
            // callers do not assume the upsert succeeded.
            return Err(WorkspaceRecordStoreError::WorkspaceOwnerConflict {
                workspace_id: entry.record.workspace_id,
            });
        }
        Ok(())
    }

    async fn load_workspace_record(
        &self,
        owner_id: &str,
        workspace_id: &str,
    ) -> Result<Option<WorkspaceRecordEntry>, WorkspaceRecordStoreError> {
        validate_owner_id(owner_id)?;
        validate_workspace_id(workspace_id)
            .map_err(|error| WorkspaceRecordStoreError::InvalidWorkspaceId(error.to_string()))?;
        let row = sqlx::query(
            "SELECT owner_id, session_id, run_id, CAST(record_json AS CHAR) AS record_json \
             FROM workspace_records WHERE workspace_id = ? AND owner_id = ?",
        )
        .bind(workspace_id)
        .bind(owner_id)
        .fetch_optional(self.pool.get())
        .await?;
        row.map(workspace_entry_from_row).transpose()
    }

    async fn list_workspace_records(
        &self,
        owner_id: &str,
        limit: u32,
    ) -> Result<Vec<WorkspaceRecordEntry>, WorkspaceRecordStoreError> {
        validate_owner_id(owner_id)?;
        let rows = sqlx::query(
            "SELECT owner_id, session_id, run_id, CAST(record_json AS CHAR) AS record_json \
             FROM workspace_records WHERE owner_id = ? ORDER BY updated_at DESC, workspace_id ASC \
             LIMIT ?",
        )
        .bind(owner_id)
        .bind(i64::from(limit.max(1)))
        .fetch_all(self.pool.get())
        .await?;
        rows.into_iter().map(workspace_entry_from_row).collect()
    }

    async fn delete_workspace_record(
        &self,
        owner_id: &str,
        workspace_id: &str,
    ) -> Result<bool, WorkspaceRecordStoreError> {
        validate_owner_id(owner_id)?;
        validate_workspace_id(workspace_id)
            .map_err(|error| WorkspaceRecordStoreError::InvalidWorkspaceId(error.to_string()))?;
        let result =
            sqlx::query("DELETE FROM workspace_records WHERE workspace_id = ? AND owner_id = ?")
                .bind(workspace_id)
                .bind(owner_id)
                .execute(self.pool.get())
                .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[async_trait]
impl WorkspaceCleanupDebtStore for DatabaseWorkspaceRecordStore {
    async fn record_cleanup_debt(
        &self,
        entry: WorkspaceCleanupDebtEntry,
    ) -> Result<(), WorkspaceCleanupDebtStoreError> {
        validate_cleanup_debt_entry(&entry)?;
        let reason = serde_json_string_cleanup(&entry.reason)?;
        let record_json = serde_json::to_string(&entry.record)?;
        match sqlx::query(
            "INSERT INTO workspace_cleanup_debts \
             (debt_id, owner_id, session_id, run_id, workspace_id, reason, message, attempts, \
              record_json, created_at, updated_at, resolved_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6), NULL)",
        )
        .bind(&entry.debt_id)
        .bind(&entry.owner_id)
        .bind(entry.session_id.as_deref())
        .bind(entry.run_id.as_deref())
        .bind(&entry.workspace_id)
        .bind(&reason)
        .bind(&entry.message)
        .bind(i64::from(entry.attempts))
        .bind(&record_json)
        .execute(self.pool.get())
        .await
        {
            Ok(_) => return Ok(()),
            Err(error) => {
                if !astra_core::is_duplicate_key_error(&error) {
                    return Err(error.into());
                }
            }
        }

        let existing: Option<(String,)> =
            sqlx::query_as("SELECT owner_id FROM workspace_cleanup_debts WHERE debt_id = ?")
                .bind(&entry.debt_id)
                .fetch_optional(self.pool.get())
                .await?;
        if let Some((existing_owner,)) = existing
            && existing_owner != entry.owner_id
        {
            return Err(WorkspaceCleanupDebtStoreError::CleanupDebtOwnerConflict {
                debt_id: entry.debt_id,
            });
        }

        let update_result = sqlx::query(
            "UPDATE workspace_cleanup_debts \
             SET session_id = ?, run_id = ?, workspace_id = ?, reason = ?, \
                 message = IF(resolved_at IS NULL, ?, message), \
                 attempts = IF(resolved_at IS NULL, GREATEST(attempts, ?), attempts), \
                 record_json = IF(resolved_at IS NULL, ?, record_json), \
                 updated_at = NOW(6), resolved_at = resolved_at \
             WHERE debt_id = ? AND owner_id = ?",
        )
        .bind(entry.session_id.as_deref())
        .bind(entry.run_id.as_deref())
        .bind(&entry.workspace_id)
        .bind(reason)
        .bind(&entry.message)
        .bind(i64::from(entry.attempts))
        .bind(&record_json)
        .bind(&entry.debt_id)
        .bind(&entry.owner_id)
        .execute(self.pool.get())
        .await?;
        if update_result.rows_affected() == 0 {
            return Err(WorkspaceCleanupDebtStoreError::CleanupDebtOwnerConflict {
                debt_id: entry.debt_id,
            });
        }
        Ok(())
    }

    async fn list_cleanup_debts(
        &self,
        owner_id: &str,
        limit: u32,
    ) -> Result<Vec<WorkspaceCleanupDebtEntry>, WorkspaceCleanupDebtStoreError> {
        validate_cleanup_owner_id(owner_id)?;
        let rows = sqlx::query(
            "SELECT debt_id, owner_id, session_id, run_id, workspace_id, reason, message, \
                    attempts, CAST(record_json AS CHAR) AS record_json, \
                    CAST(created_at AS CHAR) AS created_at \
             FROM workspace_cleanup_debts \
             WHERE owner_id = ? AND resolved_at IS NULL \
             ORDER BY created_at DESC, debt_id ASC LIMIT ?",
        )
        .bind(owner_id)
        .bind(i64::from(limit.max(1)))
        .fetch_all(self.pool.get())
        .await?;
        rows.into_iter().map(cleanup_debt_from_row).collect()
    }

    async fn resolve_cleanup_debt(
        &self,
        owner_id: &str,
        debt_id: &str,
    ) -> Result<bool, WorkspaceCleanupDebtStoreError> {
        validate_cleanup_owner_id(owner_id)?;
        validate_debt_id(debt_id)?;
        let result = sqlx::query(
            "UPDATE workspace_cleanup_debts \
             SET resolved_at = NOW(6), updated_at = NOW(6) \
             WHERE owner_id = ? AND debt_id = ? AND resolved_at IS NULL",
        )
        .bind(owner_id)
        .bind(debt_id)
        .execute(self.pool.get())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn list_all_unresolved_debts(
        &self,
    ) -> Result<Vec<WorkspaceCleanupDebtEntry>, WorkspaceCleanupDebtStoreError> {
        let rows = sqlx::query(
            "SELECT debt_id, owner_id, session_id, run_id, workspace_id, reason, message, \
                    attempts, CAST(record_json AS CHAR) AS record_json, \
                    CAST(created_at AS CHAR) AS created_at \
             FROM workspace_cleanup_debts \
             WHERE resolved_at IS NULL \
             ORDER BY created_at ASC",
        )
        .fetch_all(self.pool.get())
        .await?;
        rows.into_iter().map(cleanup_debt_from_row).collect()
    }

    async fn increment_debt_attempts(
        &self,
        debt_id: &str,
    ) -> Result<(), WorkspaceCleanupDebtStoreError> {
        validate_debt_id(debt_id)?;
        sqlx::query(
            "UPDATE workspace_cleanup_debts \
             SET attempts = attempts + 1, updated_at = NOW(6) \
             WHERE debt_id = ? AND resolved_at IS NULL",
        )
        .bind(debt_id)
        .execute(self.pool.get())
        .await?;
        Ok(())
    }
}

fn validate_entry(entry: &WorkspaceRecordEntry) -> Result<(), WorkspaceRecordStoreError> {
    validate_owner_id(&entry.owner_id)?;
    validate_workspace_id(&entry.record.workspace_id)
        .map_err(|error| WorkspaceRecordStoreError::InvalidWorkspaceId(error.to_string()))?;
    if entry
        .session_id
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err(WorkspaceRecordStoreError::InvalidSessionId);
    }
    if entry
        .run_id
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err(WorkspaceRecordStoreError::InvalidRunId);
    }
    Ok(())
}

fn validate_cleanup_debt_entry(
    entry: &WorkspaceCleanupDebtEntry,
) -> Result<(), WorkspaceCleanupDebtStoreError> {
    validate_cleanup_owner_id(&entry.owner_id)?;
    validate_debt_id(&entry.debt_id)?;
    validate_workspace_id(&entry.workspace_id)
        .map_err(|error| WorkspaceCleanupDebtStoreError::InvalidWorkspaceId(error.to_string()))?;
    validate_workspace_id(&entry.record.workspace_id)
        .map_err(|error| WorkspaceCleanupDebtStoreError::InvalidWorkspaceId(error.to_string()))?;
    if entry
        .session_id
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err(WorkspaceCleanupDebtStoreError::InvalidSessionId);
    }
    if entry
        .run_id
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err(WorkspaceCleanupDebtStoreError::InvalidRunId);
    }
    if entry.message.trim().is_empty() {
        return Err(WorkspaceCleanupDebtStoreError::InvalidMessage);
    }
    Ok(())
}

fn validate_cleanup_owner_id(owner_id: &str) -> Result<(), WorkspaceCleanupDebtStoreError> {
    if owner_id.trim().is_empty() {
        return Err(WorkspaceCleanupDebtStoreError::InvalidOwnerId);
    }
    Ok(())
}

fn validate_debt_id(debt_id: &str) -> Result<(), WorkspaceCleanupDebtStoreError> {
    if debt_id.trim().is_empty() {
        return Err(WorkspaceCleanupDebtStoreError::InvalidDebtId);
    }
    Ok(())
}

fn validate_owner_id(owner_id: &str) -> Result<(), WorkspaceRecordStoreError> {
    let trimmed = owner_id.trim();
    if trimmed.is_empty() || trimmed.len() > 255 {
        return Err(WorkspaceRecordStoreError::InvalidOwnerId);
    }
    Ok(())
}

fn serde_json_string<T: Serialize>(value: &T) -> Result<String, WorkspaceRecordStoreError> {
    let value = serde_json::to_value(value)?;
    Ok(value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string()))
}

fn serde_json_string_cleanup<T: Serialize>(
    value: &T,
) -> Result<String, WorkspaceCleanupDebtStoreError> {
    let value = serde_json::to_value(value)?;
    Ok(value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string()))
}

fn workspace_source_key(record: &WorkspaceRecord) -> Option<String> {
    match &record.source {
        WorkspaceSource::PersistentVolume { volume_id } => {
            Some(format!("persistent_volume:{volume_id}"))
        }
        WorkspaceSource::UploadedSnapshot { artifact_id } => {
            Some(format!("uploaded_snapshot:{artifact_id}"))
        }
        WorkspaceSource::Template { template_id } => Some(format!("template:{template_id}")),
        WorkspaceSource::DatasetBundle { dataset_id } => {
            Some(format!("dataset_bundle:{dataset_id}"))
        }
        WorkspaceSource::ArtifactBundle { artifact_id } => {
            Some(format!("artifact_bundle:{artifact_id}"))
        }
        WorkspaceSource::ProviderManaged {
            provider,
            reference,
        } => Some(format!(
            "provider_managed:{}:{}:{}:{}",
            provider.len(),
            provider,
            reference.len(),
            reference
        )),
        WorkspaceSource::None
        | WorkspaceSource::LocalPath { .. }
        | WorkspaceSource::EdgePath { .. }
        | WorkspaceSource::ServerSandbox { .. }
        | WorkspaceSource::GitCheckout { .. }
        | WorkspaceSource::Scratch => None,
    }
}

fn workspace_entry_from_row(
    row: MySqlRow,
) -> Result<WorkspaceRecordEntry, WorkspaceRecordStoreError> {
    let record_json: String = row.try_get("record_json")?;
    let record: WorkspaceRecord = serde_json::from_str(&record_json)?;
    Ok(WorkspaceRecordEntry {
        owner_id: row.try_get("owner_id")?,
        session_id: row.try_get("session_id")?,
        run_id: row.try_get("run_id")?,
        record,
    })
}

fn cleanup_debt_from_row(
    row: MySqlRow,
) -> Result<WorkspaceCleanupDebtEntry, WorkspaceCleanupDebtStoreError> {
    let record_json: String = row.try_get("record_json")?;
    let record: WorkspaceRecord = serde_json::from_str(&record_json)?;
    let reason_string: String = row.try_get("reason")?;
    let reason: CleanupReason = serde_json::from_value(serde_json::Value::String(reason_string))?;
    let attempts: i64 = row.try_get("attempts")?;
    // A negative `attempts` is corruption (schema is INT NOT NULL DEFAULT 0).
    // Previously masked with u32::MAX, hiding the bug and inflating retry
    // counters. Surface it so the caller can fail loudly.
    let attempts = u32::try_from(attempts).map_err(|_| {
        WorkspaceCleanupDebtStoreError::Database(sqlx::Error::Decode(
            format!("cleanup_debt attempts is negative/corrupt: {attempts}").into(),
        ))
    })?;
    // `created_at` drives the debt processing order. The previous
    // `unwrap_or_default()` substituted the empty String silently and
    // reordered debts ahead of everything else, masking corruption.
    // Propagate the error instead.
    let created_at: String = row.try_get("created_at")?;
    Ok(WorkspaceCleanupDebtEntry {
        debt_id: row.try_get("debt_id")?,
        owner_id: row.try_get("owner_id")?,
        session_id: row.try_get("session_id")?,
        run_id: row.try_get("run_id")?,
        workspace_id: row.try_get("workspace_id")?,
        reason,
        message: row.try_get("message")?,
        attempts,
        record,
        created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_runtime_env::{
        WorkspaceAuthority, WorkspaceBindingKind, WorkspaceOwnerScope, WorkspacePersistence,
        WorkspaceSource,
    };

    fn record(workspace_id: &str) -> WorkspaceRecord {
        WorkspaceRecord {
            workspace_id: workspace_id.to_string(),
            owner_scope: WorkspaceOwnerScope::Tenant,
            kind: WorkspaceBindingKind::CloudWorkspace,
            authority: WorkspaceAuthority::ReadWrite,
            root_or_volume_ref: "/workspace/volume-1".to_string(),
            source: WorkspaceSource::PersistentVolume {
                volume_id: "volume-1".to_string(),
            },
            persistence: WorkspacePersistence::Persistent,
            revision: "rev-1".to_string(),
            display_name: "Team workspace".to_string(),
        }
    }

    fn snapshot_record(workspace_id: &str, artifact_id: &str) -> WorkspaceRecord {
        WorkspaceRecord {
            workspace_id: workspace_id.to_string(),
            owner_scope: WorkspaceOwnerScope::Tenant,
            kind: WorkspaceBindingKind::CloudWorkspace,
            authority: WorkspaceAuthority::ReadOnly,
            root_or_volume_ref: format!("/workspace/snapshots/{artifact_id}"),
            source: WorkspaceSource::UploadedSnapshot {
                artifact_id: artifact_id.to_string(),
            },
            persistence: WorkspacePersistence::ImmutableSnapshot,
            revision: "rev-1".to_string(),
            display_name: "Uploaded snapshot".to_string(),
        }
    }

    fn materialized_record(workspace_id: &str, source: WorkspaceSource) -> WorkspaceRecord {
        WorkspaceRecord {
            workspace_id: workspace_id.to_string(),
            owner_scope: WorkspaceOwnerScope::Tenant,
            kind: WorkspaceBindingKind::CloudWorkspace,
            authority: WorkspaceAuthority::ReadOnly,
            root_or_volume_ref: format!("/workspace/materialized/{workspace_id}"),
            source,
            persistence: WorkspacePersistence::ImmutableSnapshot,
            revision: "rev-1".to_string(),
            display_name: "Materialized workspace".to_string(),
        }
    }

    #[test]
    fn workspace_record_owner_id_accepts_non_uuid_principal() {
        validate_owner_id("test-user").expect("business user ids are valid principals");
        validate_owner_id("tenant:alpha").expect("tenant-scoped principals are valid");
    }

    #[test]
    fn workspace_record_owner_id_rejects_empty_and_too_long_values() {
        assert!(matches!(
            validate_owner_id("   "),
            Err(WorkspaceRecordStoreError::InvalidOwnerId)
        ));
        let too_long = "x".repeat(256);
        assert!(matches!(
            validate_owner_id(&too_long),
            Err(WorkspaceRecordStoreError::InvalidOwnerId)
        ));
    }

    #[tokio::test]
    async fn in_memory_store_round_trips_workspace_record_for_owner() {
        let store = InMemoryWorkspaceRecordStore::new();
        store
            .upsert_workspace_record(WorkspaceRecordEntry::new(
                "00000000-0000-0000-0000-000000000001",
                Some("session-1".to_string()),
                Some("run-1".to_string()),
                record("workspace-1"),
            ))
            .await
            .expect("store workspace record");

        let loaded = store
            .load_workspace_record("00000000-0000-0000-0000-000000000001", "workspace-1")
            .await
            .expect("load workspace record")
            .expect("record");

        assert_eq!(loaded.owner_id, "00000000-0000-0000-0000-000000000001");
        assert_eq!(loaded.session_id.as_deref(), Some("session-1"));
        assert_eq!(loaded.run_id.as_deref(), Some("run-1"));
        assert_eq!(loaded.record.workspace_id, "workspace-1");
        assert_eq!(
            loaded.record.source,
            WorkspaceSource::PersistentVolume {
                volume_id: "volume-1".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn in_memory_store_deletes_workspace_record_for_owner_only() {
        let store = InMemoryWorkspaceRecordStore::new();
        store
            .upsert_workspace_record(WorkspaceRecordEntry::new(
                "00000000-0000-0000-0000-000000000001",
                Some("session-1".to_string()),
                Some("run-1".to_string()),
                record("workspace-delete-owner"),
            ))
            .await
            .expect("store workspace record");

        assert!(
            !store
                .delete_workspace_record(
                    "00000000-0000-0000-0000-000000000002",
                    "workspace-delete-owner"
                )
                .await
                .expect("foreign delete is a no-op")
        );
        assert!(
            store
                .load_workspace_record(
                    "00000000-0000-0000-0000-000000000001",
                    "workspace-delete-owner"
                )
                .await
                .expect("load after foreign delete")
                .is_some(),
            "foreign owner must not delete workspace record"
        );

        assert!(
            store
                .delete_workspace_record(
                    "00000000-0000-0000-0000-000000000001",
                    "workspace-delete-owner"
                )
                .await
                .expect("owner delete")
        );
        assert!(
            store
                .load_workspace_record(
                    "00000000-0000-0000-0000-000000000001",
                    "workspace-delete-owner"
                )
                .await
                .expect("load after owner delete")
                .is_none(),
            "owner delete must remove workspace record"
        );
    }

    #[tokio::test]
    async fn in_memory_store_enforces_owner_visibility() {
        let store = InMemoryWorkspaceRecordStore::new();
        store
            .upsert_workspace_record(WorkspaceRecordEntry::new(
                "00000000-0000-0000-0000-000000000001",
                None,
                None,
                record("workspace-1"),
            ))
            .await
            .expect("store workspace record");

        assert!(
            store
                .load_workspace_record("00000000-0000-0000-0000-000000000002", "workspace-1")
                .await
                .expect("load workspace record")
                .is_none()
        );
        assert!(
            store
                .list_workspace_records("00000000-0000-0000-0000-000000000002", 10)
                .await
                .expect("list workspace records")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn store_rejects_invalid_workspace_id() {
        let store = InMemoryWorkspaceRecordStore::new();
        let error = store
            .upsert_workspace_record(WorkspaceRecordEntry::new(
                "00000000-0000-0000-0000-000000000001",
                None,
                None,
                record("../bad"),
            ))
            .await
            .expect_err("invalid workspace id");

        assert!(matches!(
            error,
            WorkspaceRecordStoreError::InvalidWorkspaceId(_)
        ));
    }

    #[tokio::test]
    async fn store_rejects_cross_owner_workspace_id_takeover() {
        let store = InMemoryWorkspaceRecordStore::new();
        store
            .upsert_workspace_record(WorkspaceRecordEntry::new(
                "00000000-0000-0000-0000-000000000001",
                None,
                None,
                record("workspace-1"),
            ))
            .await
            .expect("store workspace record");

        let error = store
            .upsert_workspace_record(WorkspaceRecordEntry::new(
                "00000000-0000-0000-0000-000000000002",
                None,
                None,
                record("workspace-1"),
            ))
            .await
            .expect_err("owner takeover must fail");

        assert!(matches!(
            error,
            WorkspaceRecordStoreError::WorkspaceOwnerConflict { .. }
        ));
    }

    #[tokio::test]
    async fn store_rejects_cross_owner_persistent_volume_claim() {
        let store = InMemoryWorkspaceRecordStore::new();
        store
            .upsert_workspace_record(WorkspaceRecordEntry::new(
                "00000000-0000-0000-0000-000000000001",
                None,
                None,
                record("workspace-1"),
            ))
            .await
            .expect("store workspace record");
        let mut second = record("workspace-2");
        second.root_or_volume_ref = "/workspace/volume-1-copy".to_string();

        let error = store
            .upsert_workspace_record(WorkspaceRecordEntry::new(
                "00000000-0000-0000-0000-000000000002",
                None,
                None,
                second,
            ))
            .await
            .expect_err("cross-owner source claim must fail");

        assert!(matches!(
            error,
            WorkspaceRecordStoreError::SourceOwnerConflict { .. }
        ));
    }

    #[tokio::test]
    async fn store_rejects_cross_owner_uploaded_snapshot_claim() {
        let store = InMemoryWorkspaceRecordStore::new();
        store
            .upsert_workspace_record(WorkspaceRecordEntry::new(
                "00000000-0000-0000-0000-000000000001",
                None,
                None,
                snapshot_record("snapshot-1", "artifact-1"),
            ))
            .await
            .expect("store snapshot record");

        let error = store
            .upsert_workspace_record(WorkspaceRecordEntry::new(
                "00000000-0000-0000-0000-000000000002",
                None,
                None,
                snapshot_record("snapshot-2", "artifact-1"),
            ))
            .await
            .expect_err("cross-owner artifact claim must fail");

        assert!(matches!(
            error,
            WorkspaceRecordStoreError::SourceOwnerConflict { .. }
        ));
    }

    #[tokio::test]
    async fn store_rejects_cross_owner_materialized_source_claims() {
        let cases = [
            WorkspaceSource::Template {
                template_id: "template-1".to_string(),
            },
            WorkspaceSource::DatasetBundle {
                dataset_id: "dataset-1".to_string(),
            },
            WorkspaceSource::ArtifactBundle {
                artifact_id: "artifact-1".to_string(),
            },
        ];

        for source in cases {
            let store = InMemoryWorkspaceRecordStore::new();
            store
                .upsert_workspace_record(WorkspaceRecordEntry::new(
                    "00000000-0000-0000-0000-000000000001",
                    None,
                    None,
                    materialized_record("workspace-1", source.clone()),
                ))
                .await
                .expect("store materialized source record");

            let error = store
                .upsert_workspace_record(WorkspaceRecordEntry::new(
                    "00000000-0000-0000-0000-000000000002",
                    None,
                    None,
                    materialized_record("workspace-2", source),
                ))
                .await
                .expect_err("cross-owner materialized source claim must fail");

            assert!(matches!(
                error,
                WorkspaceRecordStoreError::SourceOwnerConflict { .. }
            ));
        }
    }

    #[tokio::test]
    async fn store_rejects_cross_owner_provider_managed_source_claim() {
        let store = InMemoryWorkspaceRecordStore::new();
        let source = WorkspaceSource::ProviderManaged {
            provider: "openshell".to_string(),
            reference: "sandbox-template-1".to_string(),
        };
        store
            .upsert_workspace_record(WorkspaceRecordEntry::new(
                "00000000-0000-0000-0000-000000000001",
                None,
                None,
                materialized_record("workspace-1", source.clone()),
            ))
            .await
            .expect("store provider-managed source record");

        let error = store
            .upsert_workspace_record(WorkspaceRecordEntry::new(
                "00000000-0000-0000-0000-000000000002",
                None,
                None,
                materialized_record("workspace-2", source),
            ))
            .await
            .expect_err("cross-owner provider-managed source claim must fail");

        assert!(matches!(
            error,
            WorkspaceRecordStoreError::SourceOwnerConflict { .. }
        ));
    }

    #[tokio::test]
    async fn store_rejects_same_owner_source_key_reuse() {
        let store = InMemoryWorkspaceRecordStore::new();
        store
            .upsert_workspace_record(WorkspaceRecordEntry::new(
                "00000000-0000-0000-0000-000000000001",
                Some("session-1".to_string()),
                Some("run-1".to_string()),
                record("workspace-1"),
            ))
            .await
            .expect("store workspace record");

        store
            .upsert_workspace_record(WorkspaceRecordEntry::new(
                "00000000-0000-0000-0000-000000000001",
                Some("session-2".to_string()),
                Some("run-2".to_string()),
                record("workspace-2"),
            ))
            .await
            .expect_err("source key cannot be reused by another workspace");

        let records = store
            .list_workspace_records("00000000-0000-0000-0000-000000000001", 10)
            .await
            .expect("list workspace records");
        assert_eq!(records.len(), 1);
    }

    #[tokio::test]
    async fn store_rejects_update_to_source_claimed_by_another_owner() {
        let store = InMemoryWorkspaceRecordStore::new();
        store
            .upsert_workspace_record(WorkspaceRecordEntry::new(
                "00000000-0000-0000-0000-000000000001",
                None,
                None,
                snapshot_record("snapshot-1", "artifact-1"),
            ))
            .await
            .expect("store first snapshot");
        store
            .upsert_workspace_record(WorkspaceRecordEntry::new(
                "00000000-0000-0000-0000-000000000002",
                None,
                None,
                snapshot_record("snapshot-2", "artifact-2"),
            ))
            .await
            .expect("store second snapshot");

        let error = store
            .upsert_workspace_record(WorkspaceRecordEntry::new(
                "00000000-0000-0000-0000-000000000002",
                None,
                None,
                snapshot_record("snapshot-2", "artifact-1"),
            ))
            .await
            .expect_err("update to cross-owner source claim must fail");

        assert!(matches!(
            error,
            WorkspaceRecordStoreError::SourceOwnerConflict { .. }
        ));
    }

    #[tokio::test]
    async fn cleanup_debt_store_round_trips_and_resolves_owner_debt() {
        let store = InMemoryWorkspaceRecordStore::new();
        let debt = WorkspaceCleanupDebtEntry::new(
            "00000000-0000-0000-0000-000000000001",
            Some("session-1".to_string()),
            Some("run-1".to_string()),
            record("workspace-1"),
            CleanupReason::Failed,
            "remove_dir failed",
        );
        let debt_id = debt.debt_id.clone();

        store
            .record_cleanup_debt(debt)
            .await
            .expect("record cleanup debt");

        let debts = store
            .list_cleanup_debts("00000000-0000-0000-0000-000000000001", 10)
            .await
            .expect("list cleanup debts");
        assert_eq!(debts.len(), 1);
        assert_eq!(debts[0].debt_id, debt_id);
        assert_eq!(debts[0].workspace_id, "workspace-1");
        assert_eq!(debts[0].reason, CleanupReason::Failed);

        assert!(
            store
                .resolve_cleanup_debt("00000000-0000-0000-0000-000000000001", &debt_id)
                .await
                .expect("resolve cleanup debt")
        );
        assert!(
            store
                .list_cleanup_debts("00000000-0000-0000-0000-000000000001", 10)
                .await
                .expect("list cleanup debts")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn cleanup_debt_store_enforces_owner_visibility() {
        let store = InMemoryWorkspaceRecordStore::new();
        let debt = WorkspaceCleanupDebtEntry::new(
            "00000000-0000-0000-0000-000000000001",
            None,
            None,
            record("workspace-1"),
            CleanupReason::LeaseExpired,
            "lease cleanup failed",
        );
        let debt_id = debt.debt_id.clone();
        store
            .record_cleanup_debt(debt)
            .await
            .expect("record cleanup debt");

        assert!(
            store
                .list_cleanup_debts("00000000-0000-0000-0000-000000000002", 10)
                .await
                .expect("list cleanup debts")
                .is_empty()
        );
        assert!(
            !store
                .resolve_cleanup_debt("00000000-0000-0000-0000-000000000002", &debt_id)
                .await
                .expect("cross-owner resolve should not fail but should not resolve")
        );
        assert_eq!(
            store
                .list_cleanup_debts("00000000-0000-0000-0000-000000000001", 10)
                .await
                .expect("owner debts")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn cleanup_debt_store_rejects_cross_owner_debt_id_takeover() {
        let store = InMemoryWorkspaceRecordStore::new();
        let debt = WorkspaceCleanupDebtEntry::new(
            "00000000-0000-0000-0000-000000000001",
            None,
            None,
            record("workspace-1"),
            CleanupReason::Failed,
            "first owner debt",
        );
        let mut takeover = WorkspaceCleanupDebtEntry::new(
            "00000000-0000-0000-0000-000000000002",
            None,
            None,
            record("workspace-2"),
            CleanupReason::Failed,
            "takeover debt",
        );
        takeover.debt_id = debt.debt_id.clone();

        store
            .record_cleanup_debt(debt)
            .await
            .expect("record first debt");
        let error = store
            .record_cleanup_debt(takeover)
            .await
            .expect_err("cross-owner debt id takeover must fail");

        assert!(matches!(
            error,
            WorkspaceCleanupDebtStoreError::CleanupDebtOwnerConflict { .. }
        ));
        let debts = store
            .list_cleanup_debts("00000000-0000-0000-0000-000000000001", 10)
            .await
            .expect("list first owner debts");
        assert_eq!(debts.len(), 1);
        assert_eq!(debts[0].message, "first owner debt");
    }

    #[tokio::test]
    async fn cleanup_debt_store_rejects_invalid_debt() {
        let store = InMemoryWorkspaceRecordStore::new();
        let mut debt = WorkspaceCleanupDebtEntry::new(
            "00000000-0000-0000-0000-000000000001",
            None,
            None,
            record("workspace-1"),
            CleanupReason::Failed,
            "cleanup failed",
        );
        debt.message = "   ".to_string();

        let error = store
            .record_cleanup_debt(debt)
            .await
            .expect_err("empty cleanup debt message must fail");

        assert!(matches!(
            error,
            WorkspaceCleanupDebtStoreError::InvalidMessage
        ));
    }
}
