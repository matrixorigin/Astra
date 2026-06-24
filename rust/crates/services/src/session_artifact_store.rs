//! Session artifact storage boundaries.
//!
//! - [`LocalSessionArtifactStore`] resolves local filesystem paths for session-scoped
//!   artifacts that still live in a session directory.
//! - [`SessionArtifactJsonStore`] persists remote-visible JSON artifacts (for example
//!   LLM captures and request dumps) without assuming the caller can access server-local
//!   files.

use std::path::{Component, Path, PathBuf};

use astra_core::{MatrixOneSettings, SharedPool};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Row, mysql::MySqlRow, query};
use uuid::Uuid;

/// Structured error type for [`SessionArtifactJsonStore`] operations. Replaces
/// the previous `Result<_, String>` to preserve sqlx / serde context and to
/// encode validation / overflow failures as distinct variants callers can
/// match on.
#[derive(Debug, thiserror::Error)]
pub enum SessionArtifactStoreError {
    /// Session id failed [`crate::session_journal::validate_session_id`]. The
    /// attached string echoes the validator's reason so existing tests that
    /// matched on the old stringified error still see the substring.
    #[error("invalid session_id: {0}")]
    InvalidSessionId(String),

    /// `artifact_id` was empty or otherwise unusable as a lookup key.
    #[error("artifact_id must not be empty: {0:?}")]
    InvalidArtifactId(String),

    /// A relative path under a session directory either escaped the session
    /// root or contained an unsupported component.
    #[error("artifact relative path {reason}: {}", path.display())]
    InvalidRelativePath { path: PathBuf, reason: &'static str },

    /// `serde_json` could not serialize the outbound artifact body.
    #[error("serialize artifact content: {0}")]
    Serialize(#[from] serde_json::Error),

    /// `sqlx` returned an error from the underlying database.
    #[error("database: {0}")]
    Database(#[from] sqlx::Error),

    /// The `turn` counter exceeded `i32::MAX` and cannot be persisted to the
    /// `session_artifacts.turn INT` column without data loss.
    #[error("turn {0} exceeds i32::MAX and cannot be persisted")]
    TurnOverflow(u32),

    /// The `round` counter exceeded `i32::MAX`.
    #[error("round {0} exceeds i32::MAX and cannot be persisted")]
    RoundOverflow(u32),

    /// A write attempted to attach an artifact to a session the user does not own.
    #[error("session {session_id} is not owned by user {user_id}")]
    SessionNotOwned { session_id: String, user_id: String },
}

pub const LOCAL_SESSION_LAYOUT_VERSION: &str = "v1";
pub const LOCAL_SESSION_JOURNAL_FILE_SUFFIX: &str = "jsonl";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerScopeKind {
    User,
}

impl OwnerScopeKind {
    fn directory_segment(self) -> &'static str {
        match self {
            OwnerScopeKind::User => "users",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerScope {
    kind: OwnerScopeKind,
    id: String,
}

impl OwnerScope {
    pub fn new(kind: OwnerScopeKind, id: impl Into<String>) -> Result<Self, String> {
        let id = id.into();
        let trimmed = id.trim();
        if trimmed.is_empty() {
            return Err("owner id must not be empty".to_string());
        }
        Ok(Self {
            kind,
            id: trimmed.to_string(),
        })
    }

    pub fn user(user_id: impl Into<String>) -> Result<Self, String> {
        Self::new(OwnerScopeKind::User, user_id)
    }

    pub fn local_user() -> Self {
        Self::user(local_owner_user_id()).expect("local owner user id is non-empty")
    }

    pub fn kind(&self) -> OwnerScopeKind {
        self.kind
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    fn directory_segment(&self) -> &'static str {
        self.kind.directory_segment()
    }

    fn storage_key(&self) -> String {
        format!("b64-{}", URL_SAFE_NO_PAD.encode(self.id.as_bytes()))
    }
}

pub fn local_owner_user_id() -> String {
    std::env::var("ASTRA_CLI_USER_ID")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "local".to_string())
}

pub trait SessionArtifactStore {
    fn sessions_root(&self) -> PathBuf;
    fn owner_sessions_root(&self, owner_scope: &OwnerScope) -> Result<PathBuf, String>;
    fn session_dir_for_owner(
        &self,
        owner_scope: &OwnerScope,
        session_id: &str,
    ) -> Result<PathBuf, String>;
    fn session_dir(&self, session_id: &str) -> Result<PathBuf, String>;
    fn session_path_for_owner(
        &self,
        owner_scope: &OwnerScope,
        session_id: &str,
        relative: impl AsRef<Path>,
    ) -> Result<PathBuf, String>;
    fn session_path(&self, session_id: &str, relative: impl AsRef<Path>)
    -> Result<PathBuf, String>;
    fn journal_path_for_owner(
        &self,
        owner_scope: &OwnerScope,
        session_id: &str,
    ) -> Result<PathBuf, String>;
    fn journal_path(&self, session_id: &str) -> Result<PathBuf, String>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LocalSessionArtifactStore;

pub fn local_session_artifact_store() -> LocalSessionArtifactStore {
    LocalSessionArtifactStore
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionArtifactJsonRecord {
    pub artifact_id: String,
    pub session_id: String,
    pub user_id: String,
    pub artifact_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<u32>,
    pub content: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredSessionArtifact {
    pub artifact_id: String,
    pub session_id: String,
    pub user_id: String,
    pub artifact_kind: String,
    pub source: Option<String>,
    pub turn: Option<u32>,
    pub round: Option<u32>,
    pub content: Value,
    pub metadata: Option<Value>,
    pub retention_policy: Option<String>,
    pub retention_until: Option<String>,
    pub status: Option<String>,
    pub referenced_by_manifest_count: u32,
    pub referenced_by_state_items_count: u32,
    pub referenced_by_citation_count: u32,
    pub created_at: Option<String>,
}

#[async_trait]
pub trait SessionArtifactJsonStore: Send + Sync {
    async fn persist_json_artifact(
        &self,
        record: SessionArtifactJsonRecord,
    ) -> Result<StoredSessionArtifact, SessionArtifactStoreError>;

    async fn load_json_artifact(
        &self,
        user_id: &str,
        session_id: &str,
        artifact_id: &str,
    ) -> Result<Option<StoredSessionArtifact>, SessionArtifactStoreError>;

    async fn load_latest_json_artifact(
        &self,
        user_id: &str,
        session_id: &str,
        artifact_kind: &str,
    ) -> Result<Option<StoredSessionArtifact>, SessionArtifactStoreError>;

    async fn list_json_artifacts(
        &self,
        user_id: &str,
        session_id: &str,
        artifact_kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<StoredSessionArtifact>, SessionArtifactStoreError>;
}

#[derive(Clone, Debug)]
pub struct DatabaseSessionArtifactStore {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseSessionArtifactStore {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
        }
    }

    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        crate::require_shared_pool(
            self.pool.as_ref(),
            "DatabaseSessionArtifactStore",
            &self.matrixone,
        )
    }

    async fn require_owned_session(
        &self,
        pool: &sqlx::Pool<sqlx::MySql>,
        user_id: &str,
        session_id: &str,
    ) -> Result<(), SessionArtifactStoreError> {
        if crate::storage::agent_session_exists_for_user(pool, session_id, user_id).await? {
            return Ok(());
        }
        Err(SessionArtifactStoreError::SessionNotOwned {
            session_id: session_id.to_string(),
            user_id: user_id.to_string(),
        })
    }
}

fn validate_session_id(session_id: &str) -> Result<(), SessionArtifactStoreError> {
    crate::session_journal::validate_session_id(session_id)
        .map_err(SessionArtifactStoreError::InvalidSessionId)
}

/// Convert a `u32` logical counter (`turn` / `round`) to an `i32` column
/// value. `turn`/`round` feed WHERE clauses and ORDER BY in
/// `session_artifacts`; silent saturation (as elsewhere in the codebase for
/// array indices) would corrupt ordering and produce collisions, so callers
/// get a structured overflow error instead.
fn encode_counter(
    value: Option<u32>,
    make_overflow: fn(u32) -> SessionArtifactStoreError,
) -> Result<Option<i32>, SessionArtifactStoreError> {
    match value {
        None => Ok(None),
        Some(v) => i32::try_from(v).map(Some).map_err(|_| make_overflow(v)),
    }
}

fn stored_artifact_from_row(
    row: &MySqlRow,
) -> Result<StoredSessionArtifact, SessionArtifactStoreError> {
    let artifact_id: String = row.try_get("artifact_id")?;
    let content_raw: String = row.try_get("content_json")?;
    let content: Value = serde_json::from_str(&content_raw)?;
    let metadata = match row.try_get::<Option<String>, _>("metadata_json")? {
        None => None,
        Some(raw) => match serde_json::from_str::<Value>(&raw) {
            Ok(value) => Some(value),
            Err(error) => {
                tracing::warn!(
                    target: "astra_services::session_artifact_store",
                    %artifact_id,
                    error = %error,
                    "failed to parse metadata_json; dropping to keep read path resilient"
                );
                None
            }
        },
    };

    Ok(StoredSessionArtifact {
        artifact_id,
        session_id: row.try_get("session_id")?,
        user_id: row.try_get("user_id")?,
        artifact_kind: row.try_get("artifact_kind")?,
        source: row.try_get("source")?,
        turn: row
            .try_get::<Option<i32>, _>("turn")?
            .and_then(|value| u32::try_from(value).ok()),
        round: row
            .try_get::<Option<i32>, _>("round")?
            .and_then(|value| u32::try_from(value).ok()),
        content,
        metadata,
        retention_policy: row.try_get("retention_policy").ok(),
        retention_until: row.try_get("retention_until").ok(),
        status: row.try_get("status").ok(),
        referenced_by_manifest_count: row
            .try_get::<i64, _>("referenced_by_manifest_count")
            .unwrap_or(0)
            .max(0) as u32,
        referenced_by_state_items_count: row
            .try_get::<i64, _>("referenced_by_state_items_count")
            .unwrap_or(0)
            .max(0) as u32,
        referenced_by_citation_count: row
            .try_get::<i64, _>("referenced_by_citation_count")
            .unwrap_or(0)
            .max(0) as u32,
        created_at: row.try_get("created_at")?,
    })
}

pub(crate) async fn load_latest_json_artifact_from_pool(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    session_id: &str,
    artifact_kind: &str,
) -> Result<Option<StoredSessionArtifact>, SessionArtifactStoreError> {
    validate_session_id(session_id)?;
    let row = query(
        "SELECT artifact_id, session_id, user_id, artifact_kind, source, turn, round, \
                 content_json, CAST(metadata AS CHAR) AS metadata_json, retention_policy, \
                 CAST(retention_until AS CHAR) AS retention_until, status, \
                 referenced_by_manifest_count, referenced_by_state_items_count, \
                 referenced_by_citation_count, CAST(created_at AS CHAR) AS created_at \
          FROM session_artifacts \
          WHERE user_id = ? AND session_id = ? AND artifact_kind = ? \
          ORDER BY created_at DESC, artifact_id DESC LIMIT 1",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(artifact_kind)
    .fetch_optional(pool)
    .await?;

    row.as_ref().map(stored_artifact_from_row).transpose()
}

#[async_trait]
impl SessionArtifactJsonStore for DatabaseSessionArtifactStore {
    async fn persist_json_artifact(
        &self,
        mut record: SessionArtifactJsonRecord,
    ) -> Result<StoredSessionArtifact, SessionArtifactStoreError> {
        validate_session_id(&record.session_id)?;
        if record.artifact_id.trim().is_empty() {
            record.artifact_id = Uuid::now_v7().to_string();
        }

        let pool = self.get_pool().await?;
        self.require_owned_session(&pool, &record.user_id, &record.session_id)
            .await?;
        let content_json = serde_json::to_string(&record.content)?;
        let metadata_json = record
            .metadata
            .as_ref()
            .map(|metadata| metadata.to_string());
        query(
            "INSERT INTO session_artifacts \
             (artifact_id, session_id, user_id, artifact_kind, source, turn, round, content_json, metadata, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())",
        )
        .bind(&record.artifact_id)
        .bind(&record.session_id)
        .bind(&record.user_id)
        .bind(&record.artifact_kind)
        .bind(record.source.as_deref())
        .bind(encode_counter(
            record.turn,
            SessionArtifactStoreError::TurnOverflow,
        )?)
        .bind(encode_counter(
            record.round,
            SessionArtifactStoreError::RoundOverflow,
        )?)
        .bind(&content_json)
        .bind(metadata_json)
        .execute(&pool)
        .await?;

        let row = query(
            "SELECT artifact_id, session_id, user_id, artifact_kind, source, turn, round, \
                    content_json, CAST(metadata AS CHAR) AS metadata_json, retention_policy, \
                    CAST(retention_until AS CHAR) AS retention_until, status, \
                    referenced_by_manifest_count, referenced_by_state_items_count, \
                    referenced_by_citation_count, CAST(created_at AS CHAR) AS created_at \
             FROM session_artifacts \
             WHERE artifact_id = ? AND session_id = ? AND user_id = ?",
        )
        .bind(&record.artifact_id)
        .bind(&record.session_id)
        .bind(&record.user_id)
        .fetch_one(&pool)
        .await?;

        stored_artifact_from_row(&row)
    }

    async fn load_json_artifact(
        &self,
        user_id: &str,
        session_id: &str,
        artifact_id: &str,
    ) -> Result<Option<StoredSessionArtifact>, SessionArtifactStoreError> {
        validate_session_id(session_id)?;
        if artifact_id.trim().is_empty() {
            return Err(SessionArtifactStoreError::InvalidArtifactId(
                artifact_id.to_string(),
            ));
        }
        let pool = self.get_pool().await?;
        let row = query(
            "SELECT artifact_id, session_id, user_id, artifact_kind, source, turn, round, \
                    content_json, CAST(metadata AS CHAR) AS metadata_json, retention_policy, \
                    CAST(retention_until AS CHAR) AS retention_until, status, \
                    referenced_by_manifest_count, referenced_by_state_items_count, \
                    referenced_by_citation_count, CAST(created_at AS CHAR) AS created_at \
             FROM session_artifacts \
             WHERE artifact_id = ? AND session_id = ? AND user_id = ?",
        )
        .bind(artifact_id)
        .bind(session_id)
        .bind(user_id)
        .fetch_optional(&pool)
        .await?;

        row.as_ref().map(stored_artifact_from_row).transpose()
    }

    async fn load_latest_json_artifact(
        &self,
        user_id: &str,
        session_id: &str,
        artifact_kind: &str,
    ) -> Result<Option<StoredSessionArtifact>, SessionArtifactStoreError> {
        let pool = self.get_pool().await?;
        load_latest_json_artifact_from_pool(&pool, user_id, session_id, artifact_kind).await
    }

    async fn list_json_artifacts(
        &self,
        user_id: &str,
        session_id: &str,
        artifact_kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<StoredSessionArtifact>, SessionArtifactStoreError> {
        validate_session_id(session_id)?;
        let pool = self.get_pool().await?;
        let capped_limit = limit.clamp(1, 100) as i64;
        let rows = if let Some(kind) = artifact_kind {
            query(
                "SELECT artifact_id, session_id, user_id, artifact_kind, source, turn, round, \
                        content_json, CAST(metadata AS CHAR) AS metadata_json, retention_policy, \
                        CAST(retention_until AS CHAR) AS retention_until, status, \
                        referenced_by_manifest_count, referenced_by_state_items_count, \
                        referenced_by_citation_count, CAST(created_at AS CHAR) AS created_at \
                 FROM session_artifacts \
                 WHERE user_id = ? AND session_id = ? AND artifact_kind = ? \
                 ORDER BY created_at DESC, artifact_id DESC LIMIT ?",
            )
            .bind(user_id)
            .bind(session_id)
            .bind(kind)
            .bind(capped_limit)
            .fetch_all(&pool)
            .await?
        } else {
            query(
                "SELECT artifact_id, session_id, user_id, artifact_kind, source, turn, round, \
                        content_json, CAST(metadata AS CHAR) AS metadata_json, retention_policy, \
                        CAST(retention_until AS CHAR) AS retention_until, status, \
                        referenced_by_manifest_count, referenced_by_state_items_count, \
                        referenced_by_citation_count, CAST(created_at AS CHAR) AS created_at \
                 FROM session_artifacts \
                 WHERE user_id = ? AND session_id = ? \
                 ORDER BY created_at DESC, artifact_id DESC LIMIT ?",
            )
            .bind(user_id)
            .bind(session_id)
            .bind(capped_limit)
            .fetch_all(&pool)
            .await?
        };

        rows.iter().map(stored_artifact_from_row).collect()
    }
}

fn validate_relative_path(relative: &Path) -> Result<(), String> {
    if relative.as_os_str().is_empty() {
        return Ok(());
    }
    if relative.is_absolute() {
        return Err(format!(
            "artifact relative path must not be absolute: {}",
            relative.display()
        ));
    }
    for component in relative.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err(format!(
                    "artifact relative path must not escape session directory: {}",
                    relative.display()
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "artifact relative path contains unsupported component: {}",
                    relative.display()
                ));
            }
        }
    }
    Ok(())
}

impl SessionArtifactStore for LocalSessionArtifactStore {
    fn sessions_root(&self) -> PathBuf {
        crate::session_journal::local_sessions_dir()
    }

    fn owner_sessions_root(&self, owner_scope: &OwnerScope) -> Result<PathBuf, String> {
        Ok(self
            .sessions_root()
            .join(LOCAL_SESSION_LAYOUT_VERSION)
            .join(owner_scope.directory_segment())
            .join(owner_scope.storage_key())
            .join("sessions"))
    }

    fn session_dir_for_owner(
        &self,
        owner_scope: &OwnerScope,
        session_id: &str,
    ) -> Result<PathBuf, String> {
        crate::session_journal::validate_session_id(session_id)?;
        Ok(self.owner_sessions_root(owner_scope)?.join(session_id))
    }

    fn session_dir(&self, session_id: &str) -> Result<PathBuf, String> {
        self.session_dir_for_owner(&OwnerScope::local_user(), session_id)
    }

    fn session_path_for_owner(
        &self,
        owner_scope: &OwnerScope,
        session_id: &str,
        relative: impl AsRef<Path>,
    ) -> Result<PathBuf, String> {
        let relative = relative.as_ref();
        validate_relative_path(relative)?;
        Ok(self
            .session_dir_for_owner(owner_scope, session_id)?
            .join(relative))
    }

    fn session_path(
        &self,
        session_id: &str,
        relative: impl AsRef<Path>,
    ) -> Result<PathBuf, String> {
        self.session_path_for_owner(&OwnerScope::local_user(), session_id, relative)
    }

    fn journal_path_for_owner(
        &self,
        owner_scope: &OwnerScope,
        session_id: &str,
    ) -> Result<PathBuf, String> {
        crate::session_journal::validate_session_id(session_id)?;
        Ok(self
            .owner_sessions_root(owner_scope)?
            .join(format!("{session_id}.{LOCAL_SESSION_JOURNAL_FILE_SUFFIX}")))
    }

    fn journal_path(&self, session_id: &str) -> Result<PathBuf, String> {
        self.journal_path_for_owner(&OwnerScope::local_user(), session_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_journal::JournalDirGuard;

    #[test]
    fn local_store_resolves_session_paths_under_override_root() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let store = local_session_artifact_store();
        let owner_sessions_root = store
            .owner_sessions_root(&OwnerScope::local_user())
            .expect("owner sessions root");
        assert!(
            owner_sessions_root
                .strip_prefix(temp.path())
                .unwrap()
                .starts_with(Path::new(LOCAL_SESSION_LAYOUT_VERSION).join("users")),
            "local session artifacts support user-owned layout only: {}",
            owner_sessions_root.display()
        );
        let session_dir = store.session_dir("sess-123").unwrap();
        assert_eq!(session_dir, owner_sessions_root.join("sess-123"));
        let artifact_path = store
            .session_path("sess-123", "step_checkpoints/000001-heavy.json")
            .unwrap();
        assert_eq!(
            artifact_path,
            owner_sessions_root
                .join("sess-123")
                .join("step_checkpoints/000001-heavy.json")
        );
        assert_eq!(
            store.journal_path("sess-123").unwrap(),
            store
                .owner_sessions_root(&OwnerScope::local_user())
                .unwrap()
                .join("sess-123.jsonl")
        );
    }

    #[test]
    fn local_store_rejects_parent_relative_paths() {
        let store = local_session_artifact_store();
        let err = store.session_path("sess-123", "../escape").unwrap_err();
        assert!(err.contains("must not escape"), "{err}");
    }

    #[test]
    fn artifact_record_round_trips_metadata() {
        let record = SessionArtifactJsonRecord {
            artifact_id: String::new(),
            session_id: "sess-123".into(),
            user_id: "user-1".into(),
            artifact_kind: "llm_capture".into(),
            source: Some("server_loop_host".into()),
            turn: Some(4),
            round: Some(2),
            content: serde_json::json!({"request":{"messages":1},"response":{"finish_reason":"stop"}}),
            metadata: Some(serde_json::json!({"model":"gpt-5.4"})),
        };
        let value = serde_json::to_value(&record).unwrap();
        assert_eq!(value["artifact_kind"], "llm_capture");
        assert_eq!(value["source"], "server_loop_host");
        assert_eq!(value["metadata"]["model"], "gpt-5.4");
    }

    #[test]
    fn invalid_relative_path_display_preserves_substring() {
        let err = SessionArtifactStoreError::InvalidRelativePath {
            path: PathBuf::from("../x"),
            reason: "must not escape session directory",
        };
        let rendered = format!("{err}");
        assert!(
            rendered.contains("must not escape"),
            "InvalidRelativePath display should cite the reason, got: {rendered}"
        );
    }

    #[test]
    fn invalid_artifact_id_display_preserves_substring() {
        let err = SessionArtifactStoreError::InvalidArtifactId(String::new());
        let rendered = format!("{err}");
        assert!(
            rendered.contains("artifact_id must not be empty"),
            "InvalidArtifactId display should explain the failure, got: {rendered}"
        );
    }

    #[test]
    fn error_is_send_sync_and_static() {
        fn assert_bounds<T: Send + Sync + 'static>() {}
        assert_bounds::<SessionArtifactStoreError>();
    }

    #[test]
    fn encode_counter_errors_on_u32_overflow() {
        let overflow = u32::MAX;
        let err = encode_counter(Some(overflow), SessionArtifactStoreError::TurnOverflow)
            .expect_err("u32::MAX must not silently clamp");
        match err {
            SessionArtifactStoreError::TurnOverflow(value) => assert_eq!(value, overflow),
            other => panic!("expected TurnOverflow, got: {other:?}"),
        }
    }

    #[test]
    fn encode_counter_round_trips_small_values() {
        assert_eq!(
            encode_counter(Some(42_u32), SessionArtifactStoreError::TurnOverflow).unwrap(),
            Some(42_i32)
        );
        assert_eq!(
            encode_counter(None, SessionArtifactStoreError::TurnOverflow).unwrap(),
            None
        );
        assert_eq!(
            encode_counter(
                Some(i32::MAX as u32),
                SessionArtifactStoreError::TurnOverflow
            )
            .unwrap(),
            Some(i32::MAX)
        );
    }
}
