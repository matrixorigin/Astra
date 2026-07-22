//! Session artifact storage boundaries.
//!
//! - [`LocalSessionArtifactStore`] resolves local filesystem paths for session-scoped
//!   artifacts that still live in a session directory.
//! - [`SessionArtifactJsonStore`] persists remote-visible JSON artifacts (for example
//!   LLM captures and request dumps) without assuming the caller can access server-local
//!   files.

use std::path::{Component, Path, PathBuf};

use crate::db_row::RowExt as SessionArtifactDbRow;
use astra_core::{MatrixOneSettings, SharedPool};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{QueryBuilder, query, query_scalar};
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

    #[error("artifact reference_id must not be empty or exceed 128 bytes: {0:?}")]
    InvalidReferenceId(String),

    #[error("duplicate artifact reference: kind={kind}, reference_id={reference_id}")]
    DuplicateReference {
        kind: &'static str,
        reference_id: String,
    },

    #[error("artifact reference mutation is not supported by this store")]
    ReferenceMutationUnsupported,

    #[error("artifact reference query is not supported by this store")]
    ReferenceQueryUnsupported,

    #[error("mutable artifact projections cannot carry durable references")]
    MutableProjectionReferencesUnsupported,

    #[error(
        "mutable artifact projection id must use the reserved {prefix:?} namespace: {artifact_id:?}"
    )]
    InvalidMutableProjectionId {
        prefix: &'static str,
        artifact_id: String,
    },

    #[error("immutable artifact id uses the reserved mutable projection namespace: {0:?}")]
    ReservedMutableProjectionId(String),

    #[error(
        "mutable artifact projection {artifact_id} cannot replace artifact kind {existing_kind:?} with {requested_kind:?}"
    )]
    MutableProjectionIdentityConflict {
        artifact_id: String,
        existing_kind: String,
        requested_kind: String,
    },

    #[error("stored artifact reference kind is invalid: {0}")]
    InvalidStoredReferenceKind(String),

    #[error("artifact {artifact_id} was not found in session {session_id} for user {user_id}")]
    ArtifactNotFound {
        artifact_id: String,
        session_id: String,
        user_id: String,
    },

    #[error("artifact {artifact_id} cannot acquire a reference while status is {status}")]
    ArtifactNotRetainable { artifact_id: String, status: String },

    /// A relative path under a session directory either escaped the session
    /// root or contained an unsupported component.
    #[error("artifact relative path {reason}: {}", path.display())]
    InvalidRelativePath { path: PathBuf, reason: &'static str },

    /// `serde_json` could not serialize the outbound artifact body.
    #[error("serialize artifact content: {0}")]
    Serialize(#[from] serde_json::Error),

    /// JSON persisted in a database column is malformed.
    #[error("decode artifact {artifact_id} column {column} as JSON: {source}")]
    JsonDecode {
        artifact_id: String,
        column: &'static str,
        #[source]
        source: serde_json::Error,
    },

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

    /// A persisted numeric value cannot be represented by the public contract.
    #[error("invalid artifact {artifact_id} column {column}: value={value}, reason={reason}")]
    InvalidDatabaseValue {
        artifact_id: String,
        column: &'static str,
        value: String,
        reason: &'static str,
    },
}

pub const LOCAL_SESSION_LAYOUT_VERSION: &str = "v1";
pub const LOCAL_SESSION_JOURNAL_FILE_SUFFIX: &str = "jsonl";
pub const MUTABLE_ARTIFACT_PROJECTION_ID_PREFIX: &str = "projection:";
const MAX_ARTIFACT_ID_BYTES: usize = 64;

fn is_mutable_artifact_projection_id(artifact_id: &str) -> bool {
    artifact_id.starts_with(MUTABLE_ARTIFACT_PROJECTION_ID_PREFIX)
}

fn is_valid_mutable_artifact_projection_id(artifact_id: &str) -> bool {
    if artifact_id.len() > MAX_ARTIFACT_ID_BYTES {
        return false;
    }
    artifact_id
        .strip_prefix(MUTABLE_ARTIFACT_PROJECTION_ID_PREFIX)
        .is_some_and(|name| {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                && !name.starts_with('-')
                && !name.ends_with('-')
                && !name.contains("--")
        })
}

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
    /// Durable owners that keep this artifact reachable. These edges are
    /// inserted atomically with the artifact rather than reconstructed from
    /// artifact kinds or payload text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<SessionArtifactReference>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionArtifactReferenceKind {
    InvocationLedger,
    Manifest,
    StateItem,
    Citation,
}

impl SessionArtifactReferenceKind {
    fn wire_name(self) -> &'static str {
        match self {
            Self::InvocationLedger => "invocation_ledger",
            Self::Manifest => "manifest",
            Self::StateItem => "state_item",
            Self::Citation => "citation",
        }
    }

    fn from_wire_name(value: &str) -> Result<Self, SessionArtifactStoreError> {
        match value {
            "invocation_ledger" => Ok(Self::InvocationLedger),
            "manifest" => Ok(Self::Manifest),
            "state_item" => Ok(Self::StateItem),
            "citation" => Ok(Self::Citation),
            other => Err(SessionArtifactStoreError::InvalidStoredReferenceKind(
                other.to_string(),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionArtifactReference {
    pub kind: SessionArtifactReferenceKind,
    pub reference_id: String,
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
    pub referenced_by_durable_count: u32,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionArtifactListCursor {
    pub created_at: String,
    pub artifact_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionArtifactListPage {
    pub artifacts: Vec<StoredSessionArtifact>,
    pub limit: usize,
    pub next_cursor: Option<SessionArtifactListCursor>,
}

#[async_trait]
pub trait SessionArtifactJsonStore: Send + Sync {
    async fn persist_json_artifact(
        &self,
        record: SessionArtifactJsonRecord,
    ) -> Result<StoredSessionArtifact, SessionArtifactStoreError>;

    /// Creates or replaces a mutable, stable-identity projection. Unlike
    /// immutable artifacts, a projection must provide an ID in the reserved
    /// `projection:` namespace and may not carry durable references.
    async fn upsert_json_artifact_projection(
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
        cursor: Option<SessionArtifactListCursor>,
    ) -> Result<SessionArtifactListPage, SessionArtifactStoreError>;

    /// Adds an owner-scoped durable reachability edge. Returns `true` only
    /// when a new edge was inserted; replaying the same retain is successful
    /// and returns `false`.
    async fn retain_json_artifact_reference(
        &self,
        _user_id: &str,
        _session_id: &str,
        _artifact_id: &str,
        _reference: &SessionArtifactReference,
    ) -> Result<bool, SessionArtifactStoreError> {
        Err(SessionArtifactStoreError::ReferenceMutationUnsupported)
    }

    /// Removes an owner-scoped durable reachability edge. Returns `true` only
    /// when an existing edge was removed; replaying a release is successful
    /// and returns `false`.
    async fn release_json_artifact_reference(
        &self,
        _user_id: &str,
        _session_id: &str,
        _artifact_id: &str,
        _reference: &SessionArtifactReference,
    ) -> Result<bool, SessionArtifactStoreError> {
        Err(SessionArtifactStoreError::ReferenceMutationUnsupported)
    }

    /// Lists the exact durable owners keeping one artifact reachable.
    async fn list_json_artifact_references(
        &self,
        _user_id: &str,
        _session_id: &str,
        _artifact_id: &str,
        _limit: usize,
    ) -> Result<Vec<SessionArtifactReference>, SessionArtifactStoreError> {
        Err(SessionArtifactStoreError::ReferenceQueryUnsupported)
    }

    /// Reverse lookup for reconciliation and introspection. The result is a
    /// bounded list of artifact IDs owned by the exact reference.
    async fn list_json_artifacts_for_reference(
        &self,
        _user_id: &str,
        _session_id: &str,
        _reference: &SessionArtifactReference,
        _limit: usize,
    ) -> Result<Vec<String>, SessionArtifactStoreError> {
        Err(SessionArtifactStoreError::ReferenceQueryUnsupported)
    }
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

fn validate_artifact_list_limit(limit: usize) -> usize {
    limit.clamp(1, 100)
}

fn validate_artifact_references(
    references: &[SessionArtifactReference],
) -> Result<(), SessionArtifactStoreError> {
    let mut seen = std::collections::HashSet::with_capacity(references.len());
    for reference in references {
        let reference_id = reference.reference_id.trim();
        if reference_id.is_empty() || reference_id.len() > 128 {
            return Err(SessionArtifactStoreError::InvalidReferenceId(
                reference.reference_id.clone(),
            ));
        }
        if !seen.insert((reference.kind.wire_name(), reference_id)) {
            return Err(SessionArtifactStoreError::DuplicateReference {
                kind: reference.kind.wire_name(),
                reference_id: reference_id.to_string(),
            });
        }
    }
    Ok(())
}

fn artifact_list_query_limit(limit: usize) -> i64 {
    limit as i64 + 1
}

fn artifact_list_cursor_db_created_at(
    cursor: &SessionArtifactListCursor,
) -> Result<String, SessionArtifactStoreError> {
    let created_at = cursor.created_at.trim();
    if created_at.is_empty() {
        return Err(SessionArtifactStoreError::InvalidDatabaseValue {
            artifact_id: cursor.artifact_id.clone(),
            column: "created_at",
            value: cursor.created_at.clone(),
            reason: "cursor timestamp must not be empty",
        });
    }
    let db_created_at = created_at.replace('T', " ");
    if db_created_at.len() != "YYYY-MM-DD HH:MM:SS.ffffff".len()
        || db_created_at.as_bytes().get(10) != Some(&b' ')
        || db_created_at.as_bytes().get(19) != Some(&b'.')
        || chrono::NaiveDateTime::parse_from_str(&db_created_at, "%Y-%m-%d %H:%M:%S%.6f").is_err()
    {
        return Err(SessionArtifactStoreError::InvalidDatabaseValue {
            artifact_id: cursor.artifact_id.clone(),
            column: "created_at",
            value: cursor.created_at.clone(),
            reason: "cursor timestamp must use YYYY-MM-DDTHH:MM:SS.ffffff",
        });
    }
    Ok(db_created_at)
}

fn artifact_list_cursor_artifact_id(
    cursor: &SessionArtifactListCursor,
) -> Result<String, SessionArtifactStoreError> {
    let artifact_id = cursor.artifact_id.trim();
    if artifact_id.is_empty() {
        return Err(SessionArtifactStoreError::InvalidArtifactId(
            cursor.artifact_id.clone(),
        ));
    }
    Ok(artifact_id.to_string())
}

fn artifact_list_cursor_from_record(
    artifact: &StoredSessionArtifact,
) -> Result<SessionArtifactListCursor, SessionArtifactStoreError> {
    let created_at = artifact.created_at.as_deref().ok_or_else(|| {
        SessionArtifactStoreError::InvalidDatabaseValue {
            artifact_id: artifact.artifact_id.clone(),
            column: "created_at",
            value: "NULL".to_string(),
            reason: "list cursor requires created_at",
        }
    })?;
    if created_at.trim().is_empty() {
        return Err(SessionArtifactStoreError::InvalidDatabaseValue {
            artifact_id: artifact.artifact_id.clone(),
            column: "created_at",
            value: created_at.to_string(),
            reason: "list cursor requires non-empty created_at",
        });
    }
    if artifact.artifact_id.trim().is_empty() {
        return Err(SessionArtifactStoreError::InvalidArtifactId(
            artifact.artifact_id.clone(),
        ));
    }
    Ok(SessionArtifactListCursor {
        created_at: created_at.to_string(),
        artifact_id: artifact.artifact_id.clone(),
    })
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

fn artifact_row_string(
    row: &impl SessionArtifactDbRow,
    column: &'static str,
) -> Result<String, SessionArtifactStoreError> {
    row.string_column(column)
        .map_err(SessionArtifactStoreError::Database)
}

fn artifact_row_optional_string(
    row: &impl SessionArtifactDbRow,
    column: &'static str,
) -> Result<Option<String>, SessionArtifactStoreError> {
    row.optional_string_column(column)
        .map_err(SessionArtifactStoreError::Database)
}

fn artifact_row_optional_u32(
    row: &impl SessionArtifactDbRow,
    artifact_id: &str,
    column: &'static str,
) -> Result<Option<u32>, SessionArtifactStoreError> {
    let value = row
        .optional_i32_column(column)
        .map_err(SessionArtifactStoreError::Database)?;
    value
        .map(|value| {
            u32::try_from(value).map_err(|_| SessionArtifactStoreError::InvalidDatabaseValue {
                artifact_id: artifact_id.to_string(),
                column,
                value: value.to_string(),
                reason: "expected non-negative i32",
            })
        })
        .transpose()
}

fn artifact_row_u32(
    row: &impl SessionArtifactDbRow,
    artifact_id: &str,
    column: &'static str,
) -> Result<u32, SessionArtifactStoreError> {
    let value = row
        .i64_column(column)
        .map_err(SessionArtifactStoreError::Database)?;
    u32::try_from(value).map_err(|_| SessionArtifactStoreError::InvalidDatabaseValue {
        artifact_id: artifact_id.to_string(),
        column,
        value: value.to_string(),
        reason: "expected u32 range",
    })
}

fn artifact_row_json(
    raw: &str,
    artifact_id: &str,
    column: &'static str,
) -> Result<Value, SessionArtifactStoreError> {
    serde_json::from_str(raw).map_err(|source| SessionArtifactStoreError::JsonDecode {
        artifact_id: artifact_id.to_string(),
        column,
        source,
    })
}

fn stored_artifact_from_row(
    row: &impl SessionArtifactDbRow,
) -> Result<StoredSessionArtifact, SessionArtifactStoreError> {
    let artifact_id = artifact_row_string(row, "artifact_id")?;
    let content_raw = artifact_row_string(row, "content_json")?;
    let content = artifact_row_json(&content_raw, &artifact_id, "content_json")?;
    let metadata = artifact_row_optional_string(row, "metadata_json")?
        .map(|raw| artifact_row_json(&raw, &artifact_id, "metadata_json"))
        .transpose()?;

    Ok(StoredSessionArtifact {
        session_id: artifact_row_string(row, "session_id")?,
        user_id: artifact_row_string(row, "user_id")?,
        artifact_kind: artifact_row_string(row, "artifact_kind")?,
        source: artifact_row_optional_string(row, "source")?,
        turn: artifact_row_optional_u32(row, &artifact_id, "turn")?,
        round: artifact_row_optional_u32(row, &artifact_id, "round")?,
        content,
        metadata,
        artifact_id: artifact_id.clone(),
        retention_policy: artifact_row_optional_string(row, "retention_policy")?,
        retention_until: artifact_row_optional_string(row, "retention_until")?,
        status: artifact_row_optional_string(row, "status")?,
        referenced_by_manifest_count: artifact_row_u32(
            row,
            &artifact_id,
            "referenced_by_manifest_count",
        )?,
        referenced_by_state_items_count: artifact_row_u32(
            row,
            &artifact_id,
            "referenced_by_state_items_count",
        )?,
        referenced_by_citation_count: artifact_row_u32(
            row,
            &artifact_id,
            "referenced_by_citation_count",
        )?,
        referenced_by_durable_count: artifact_row_u32(
            row,
            &artifact_id,
            "referenced_by_durable_count",
        )?,
        created_at: artifact_row_optional_string(row, "created_at")?,
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
                 referenced_by_citation_count, \
                 (SELECT COUNT(*) FROM session_artifact_references refs \
                  WHERE refs.user_id = session_artifacts.user_id \
                    AND refs.session_id = session_artifacts.session_id \
                    AND refs.artifact_id = session_artifacts.artifact_id) \
                    AS referenced_by_durable_count, \
                 CAST(created_at AS CHAR) AS created_at \
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

pub(crate) async fn load_json_artifact_from_pool(
    pool: &sqlx::Pool<sqlx::MySql>,
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
    let row = query(
        "SELECT artifact_id, session_id, user_id, artifact_kind, source, turn, round, \
                content_json, CAST(metadata AS CHAR) AS metadata_json, retention_policy, \
                CAST(retention_until AS CHAR) AS retention_until, status, \
                referenced_by_manifest_count, referenced_by_state_items_count, \
                referenced_by_citation_count, \
                (SELECT COUNT(*) FROM session_artifact_references refs \
                 WHERE refs.user_id = session_artifacts.user_id \
                   AND refs.session_id = session_artifacts.session_id \
                   AND refs.artifact_id = session_artifacts.artifact_id) \
                   AS referenced_by_durable_count, \
                CAST(created_at AS CHAR) AS created_at \
         FROM session_artifacts \
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(artifact_id)
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
        } else if is_mutable_artifact_projection_id(&record.artifact_id) {
            return Err(SessionArtifactStoreError::ReservedMutableProjectionId(
                record.artifact_id,
            ));
        }
        validate_artifact_references(&record.references)?;

        let pool = self.get_pool().await?;
        self.require_owned_session(&pool, &record.user_id, &record.session_id)
            .await?;
        let content_json = serde_json::to_string(&record.content)?;
        let metadata_json = record
            .metadata
            .as_ref()
            .map(|metadata| metadata.to_string());
        let retention_until = (!record.references.is_empty())
            .then(|| (chrono::Utc::now() + chrono::Duration::days(30)).naive_utc());
        let mut tx = pool.begin().await?;
        query(
            "INSERT INTO session_artifacts \
             (artifact_id, session_id, user_id, artifact_kind, source, turn, round, content_json, metadata, retention_until, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP(6))",
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
        .bind(retention_until)
        .execute(&mut *tx)
        .await?;

        for reference in &record.references {
            query(
                "INSERT INTO session_artifact_references \
                 (user_id, session_id, artifact_id, reference_kind, reference_id, created_at) \
                 VALUES (?, ?, ?, ?, ?, CURRENT_TIMESTAMP(6))",
            )
            .bind(&record.user_id)
            .bind(&record.session_id)
            .bind(&record.artifact_id)
            .bind(reference.kind.wire_name())
            .bind(reference.reference_id.trim())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;

        let row = query(
            "SELECT artifact_id, session_id, user_id, artifact_kind, source, turn, round, \
                    content_json, CAST(metadata AS CHAR) AS metadata_json, retention_policy, \
                    CAST(retention_until AS CHAR) AS retention_until, status, \
                    referenced_by_manifest_count, referenced_by_state_items_count, \
                    referenced_by_citation_count, \
                    (SELECT COUNT(*) FROM session_artifact_references refs \
                     WHERE refs.user_id = session_artifacts.user_id \
                       AND refs.session_id = session_artifacts.session_id \
                       AND refs.artifact_id = session_artifacts.artifact_id) \
                       AS referenced_by_durable_count, \
                    CAST(created_at AS CHAR) AS created_at \
             FROM session_artifacts \
             WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
        )
        .bind(&record.user_id)
        .bind(&record.session_id)
        .bind(&record.artifact_id)
        .fetch_one(&pool)
        .await?;

        stored_artifact_from_row(&row)
    }

    async fn upsert_json_artifact_projection(
        &self,
        record: SessionArtifactJsonRecord,
    ) -> Result<StoredSessionArtifact, SessionArtifactStoreError> {
        validate_session_id(&record.session_id)?;
        if record.artifact_id.trim().is_empty() {
            return Err(SessionArtifactStoreError::InvalidArtifactId(
                record.artifact_id,
            ));
        }
        if !is_valid_mutable_artifact_projection_id(&record.artifact_id) {
            return Err(SessionArtifactStoreError::InvalidMutableProjectionId {
                prefix: MUTABLE_ARTIFACT_PROJECTION_ID_PREFIX,
                artifact_id: record.artifact_id,
            });
        }
        if !record.references.is_empty() {
            return Err(SessionArtifactStoreError::MutableProjectionReferencesUnsupported);
        }

        let pool = self.get_pool().await?;
        self.require_owned_session(&pool, &record.user_id, &record.session_id)
            .await?;
        let content_json = serde_json::to_string(&record.content)?;
        let metadata_json = record.metadata.as_ref().map(serde_json::Value::to_string);
        let turn = encode_counter(record.turn, SessionArtifactStoreError::TurnOverflow)?;
        let round = encode_counter(record.round, SessionArtifactStoreError::RoundOverflow)?;
        let mut tx = pool.begin().await?;
        // Claim the composite identity before inspecting it. The duplicate-key
        // branch assigns a non-key column to itself because MatrixOne rejects
        // assignments to primary/unique columns even when the value is
        // unchanged. Do not use INSERT IGNORE here: it can suppress unrelated
        // data errors. Concurrent first writers still serialize on the unique
        // insert before the row lock below.
        query(
            "INSERT INTO session_artifacts \
             (artifact_id, session_id, user_id, artifact_kind, source, turn, round, \
              content_json, metadata, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP(6), CURRENT_TIMESTAMP(6)) \
             ON DUPLICATE KEY UPDATE updated_at = updated_at",
        )
        .bind(&record.artifact_id)
        .bind(&record.session_id)
        .bind(&record.user_id)
        .bind(&record.artifact_kind)
        .bind(record.source.as_deref())
        .bind(turn)
        .bind(round)
        .bind(&content_json)
        .bind(&metadata_json)
        .execute(&mut *tx)
        .await?;

        let existing = query(
            "SELECT artifact_kind, referenced_by_manifest_count, \
                    referenced_by_state_items_count, referenced_by_citation_count \
             FROM session_artifacts \
             WHERE user_id = ? AND session_id = ? AND artifact_id = ? FOR UPDATE",
        )
        .bind(&record.user_id)
        .bind(&record.session_id)
        .bind(&record.artifact_id)
        .fetch_one(&mut *tx)
        .await?;
        let existing_kind = existing.string_column("artifact_kind")?;
        if existing_kind != record.artifact_kind {
            return Err(
                SessionArtifactStoreError::MutableProjectionIdentityConflict {
                    artifact_id: record.artifact_id,
                    existing_kind,
                    requested_kind: record.artifact_kind,
                },
            );
        }
        let counter_references = [
            "referenced_by_manifest_count",
            "referenced_by_state_items_count",
            "referenced_by_citation_count",
        ]
        .into_iter()
        .try_fold(0_i64, |total, column| {
            existing.i64_column(column).map(|value| total + value)
        })?;
        let durable_references: i64 = query_scalar(
            "SELECT COUNT(*) FROM session_artifact_references \
             WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
        )
        .bind(&record.user_id)
        .bind(&record.session_id)
        .bind(&record.artifact_id)
        .fetch_one(&mut *tx)
        .await?;
        if counter_references != 0 || durable_references != 0 {
            return Err(SessionArtifactStoreError::MutableProjectionReferencesUnsupported);
        }

        query(
            "UPDATE session_artifacts \
             SET source = ?, turn = ?, round = ?, content_json = ?, metadata = ?, \
                 status = 'active', updated_at = CURRENT_TIMESTAMP(6) \
             WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
        )
        .bind(record.source.as_deref())
        .bind(turn)
        .bind(round)
        .bind(content_json)
        .bind(metadata_json)
        .bind(&record.user_id)
        .bind(&record.session_id)
        .bind(&record.artifact_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        self.load_json_artifact(&record.user_id, &record.session_id, &record.artifact_id)
            .await?
            .ok_or(SessionArtifactStoreError::ArtifactNotFound {
                artifact_id: record.artifact_id,
                session_id: record.session_id,
                user_id: record.user_id,
            })
    }

    async fn retain_json_artifact_reference(
        &self,
        user_id: &str,
        session_id: &str,
        artifact_id: &str,
        reference: &SessionArtifactReference,
    ) -> Result<bool, SessionArtifactStoreError> {
        validate_session_id(session_id)?;
        if artifact_id.trim().is_empty() {
            return Err(SessionArtifactStoreError::InvalidArtifactId(
                artifact_id.to_string(),
            ));
        }
        if is_mutable_artifact_projection_id(artifact_id) {
            return Err(SessionArtifactStoreError::MutableProjectionReferencesUnsupported);
        }
        validate_artifact_references(std::slice::from_ref(reference))?;

        let pool = self.get_pool().await?;
        self.require_owned_session(&pool, user_id, session_id)
            .await?;
        let mut tx = pool.begin().await?;
        let row = query(
            "SELECT status FROM session_artifacts \
             WHERE user_id = ? AND session_id = ? AND artifact_id = ? FOR UPDATE",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(artifact_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Err(SessionArtifactStoreError::ArtifactNotFound {
                artifact_id: artifact_id.to_string(),
                session_id: session_id.to_string(),
                user_id: user_id.to_string(),
            });
        };
        let status = row.string_column("status")?;
        if status != "active" {
            return Err(SessionArtifactStoreError::ArtifactNotRetainable {
                artifact_id: artifact_id.to_string(),
                status,
            });
        }

        let inserted = query(
            "INSERT IGNORE INTO session_artifact_references \
             (user_id, session_id, artifact_id, reference_kind, reference_id, created_at) \
             VALUES (?, ?, ?, ?, ?, CURRENT_TIMESTAMP(6))",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(artifact_id)
        .bind(reference.kind.wire_name())
        .bind(reference.reference_id.trim())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        let retention_until = (chrono::Utc::now() + chrono::Duration::days(30)).naive_utc();
        query(
            "UPDATE session_artifacts SET retention_until = ? \
             WHERE user_id = ? AND session_id = ? AND artifact_id = ? \
               AND (retention_until IS NULL OR retention_until < ?)",
        )
        .bind(retention_until)
        .bind(user_id)
        .bind(session_id)
        .bind(artifact_id)
        .bind(retention_until)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(inserted)
    }

    async fn release_json_artifact_reference(
        &self,
        user_id: &str,
        session_id: &str,
        artifact_id: &str,
        reference: &SessionArtifactReference,
    ) -> Result<bool, SessionArtifactStoreError> {
        validate_session_id(session_id)?;
        if artifact_id.trim().is_empty() {
            return Err(SessionArtifactStoreError::InvalidArtifactId(
                artifact_id.to_string(),
            ));
        }
        validate_artifact_references(std::slice::from_ref(reference))?;

        let pool = self.get_pool().await?;
        self.require_owned_session(&pool, user_id, session_id)
            .await?;
        let deleted = query(
            "DELETE FROM session_artifact_references \
             WHERE user_id = ? AND session_id = ? AND artifact_id = ? \
               AND reference_kind = ? AND reference_id = ?",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(artifact_id)
        .bind(reference.kind.wire_name())
        .bind(reference.reference_id.trim())
        .execute(&pool)
        .await?
        .rows_affected()
            > 0;
        Ok(deleted)
    }

    async fn list_json_artifact_references(
        &self,
        user_id: &str,
        session_id: &str,
        artifact_id: &str,
        limit: usize,
    ) -> Result<Vec<SessionArtifactReference>, SessionArtifactStoreError> {
        validate_session_id(session_id)?;
        if artifact_id.trim().is_empty() {
            return Err(SessionArtifactStoreError::InvalidArtifactId(
                artifact_id.to_string(),
            ));
        }
        let pool = self.get_pool().await?;
        self.require_owned_session(&pool, user_id, session_id)
            .await?;
        let exists: Option<i8> = sqlx::query_scalar(
            "SELECT 1 FROM session_artifacts
             WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(artifact_id)
        .fetch_optional(&pool)
        .await?;
        if exists.is_none() {
            return Err(SessionArtifactStoreError::ArtifactNotFound {
                artifact_id: artifact_id.to_string(),
                session_id: session_id.to_string(),
                user_id: user_id.to_string(),
            });
        }
        let rows = query(
            "SELECT reference_kind, reference_id
             FROM session_artifact_references
             WHERE user_id = ? AND session_id = ? AND artifact_id = ?
             ORDER BY reference_kind, reference_id LIMIT ?",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(artifact_id)
        .bind(validate_artifact_list_limit(limit) as i64)
        .fetch_all(&pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(SessionArtifactReference {
                    kind: SessionArtifactReferenceKind::from_wire_name(
                        &row.string_column("reference_kind")?,
                    )?,
                    reference_id: row.string_column("reference_id")?,
                })
            })
            .collect()
    }

    async fn list_json_artifacts_for_reference(
        &self,
        user_id: &str,
        session_id: &str,
        reference: &SessionArtifactReference,
        limit: usize,
    ) -> Result<Vec<String>, SessionArtifactStoreError> {
        validate_session_id(session_id)?;
        validate_artifact_references(std::slice::from_ref(reference))?;
        let pool = self.get_pool().await?;
        self.require_owned_session(&pool, user_id, session_id)
            .await?;
        let rows = query(
            "SELECT artifact_id FROM session_artifact_references
             WHERE user_id = ? AND session_id = ?
               AND reference_kind = ? AND reference_id = ?
             ORDER BY artifact_id LIMIT ?",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(reference.kind.wire_name())
        .bind(reference.reference_id.trim())
        .bind(validate_artifact_list_limit(limit) as i64)
        .fetch_all(&pool)
        .await?;
        rows.iter()
            .map(|row| row.string_column("artifact_id").map_err(Into::into))
            .collect()
    }

    async fn load_json_artifact(
        &self,
        user_id: &str,
        session_id: &str,
        artifact_id: &str,
    ) -> Result<Option<StoredSessionArtifact>, SessionArtifactStoreError> {
        let pool = self.get_pool().await?;
        load_json_artifact_from_pool(&pool, user_id, session_id, artifact_id).await
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
        cursor: Option<SessionArtifactListCursor>,
    ) -> Result<SessionArtifactListPage, SessionArtifactStoreError> {
        validate_session_id(session_id)?;
        let pool = self.get_pool().await?;
        let capped_limit = validate_artifact_list_limit(limit);

        let mut qb = QueryBuilder::<sqlx::MySql>::new(
            "SELECT artifact_id, session_id, user_id, artifact_kind, source, turn, round, \
                    content_json, CAST(metadata AS CHAR) AS metadata_json, retention_policy, \
                    CAST(retention_until AS CHAR) AS retention_until, status, \
                    referenced_by_manifest_count, referenced_by_state_items_count, \
                    referenced_by_citation_count, \
                    (SELECT COUNT(*) FROM session_artifact_references refs \
                     WHERE refs.user_id = session_artifacts.user_id \
                       AND refs.session_id = session_artifacts.session_id \
                       AND refs.artifact_id = session_artifacts.artifact_id) \
                       AS referenced_by_durable_count, \
                    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%f') AS created_at \
             FROM session_artifacts \
             WHERE user_id = ",
        );
        qb.push_bind(user_id);
        qb.push(" AND session_id = ");
        qb.push_bind(session_id);
        if let Some(kind) = artifact_kind {
            qb.push(" AND artifact_kind = ");
            qb.push_bind(kind);
        }
        if let Some(cursor) = &cursor {
            let created_at = artifact_list_cursor_db_created_at(cursor)?;
            let artifact_id = artifact_list_cursor_artifact_id(cursor)?;
            qb.push(" AND (created_at < ");
            qb.push_bind(created_at.clone());
            qb.push(" OR (created_at = ");
            qb.push_bind(created_at);
            qb.push(" AND artifact_id < ");
            qb.push_bind(artifact_id);
            qb.push("))");
        }
        qb.push(" ORDER BY created_at DESC, artifact_id DESC LIMIT ");
        qb.push_bind(artifact_list_query_limit(capped_limit));

        let rows = qb.build().fetch_all(&pool).await?;
        let mut artifacts = rows
            .iter()
            .map(stored_artifact_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = artifacts.len() > capped_limit;
        if has_more {
            artifacts.truncate(capped_limit);
        }
        let next_cursor = if has_more {
            artifacts
                .last()
                .map(artifact_list_cursor_from_record)
                .transpose()?
        } else {
            None
        };

        Ok(SessionArtifactListPage {
            artifacts,
            limit: capped_limit,
            next_cursor,
        })
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

    #[derive(Clone)]
    struct FakeArtifactRow {
        failed_column: Option<&'static str>,
        content_json: String,
        metadata_json: Option<String>,
        optional_i32_overrides: Vec<(&'static str, Option<i32>)>,
        i64_overrides: Vec<(&'static str, i64)>,
    }

    impl FakeArtifactRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                content_json: serde_json::json!({"ok": true}).to_string(),
                metadata_json: Some(serde_json::json!({"model": "gpt-5.4"}).to_string()),
                optional_i32_overrides: Vec::new(),
                i64_overrides: Vec::new(),
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }

        fn with_content_json(content_json: impl Into<String>) -> Self {
            Self {
                content_json: content_json.into(),
                ..Self::complete()
            }
        }

        fn with_metadata_json(metadata_json: Option<String>) -> Self {
            Self {
                metadata_json,
                ..Self::complete()
            }
        }

        fn with_optional_i32(column: &'static str, value: Option<i32>) -> Self {
            Self {
                optional_i32_overrides: vec![(column, value)],
                ..Self::complete()
            }
        }

        fn with_i64(column: &'static str, value: i64) -> Self {
            Self {
                i64_overrides: vec![(column, value)],
                ..Self::complete()
            }
        }

        fn fail_if_needed(&self, column: &str) -> Result<(), sqlx::Error> {
            if self.failed_column == Some(column) {
                Err(sqlx::Error::ColumnNotFound(column.to_string()))
            } else {
                Ok(())
            }
        }
    }

    impl SessionArtifactDbRow for FakeArtifactRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "artifact_id" => "artifact-1".to_string(),
                "session_id" => "session-1".to_string(),
                "user_id" => "user-1".to_string(),
                "artifact_kind" => "llm_capture".to_string(),
                "content_json" => self.content_json.clone(),
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            })
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "source" => Some("server_loop_host".to_string()),
                "metadata_json" => self.metadata_json.clone(),
                "retention_policy" => Some("default".to_string()),
                "retention_until" => Some("2026-06-26 12:00:00".to_string()),
                "status" => Some("active".to_string()),
                "created_at" => Some("2026-06-26 10:00:00".to_string()),
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            })
        }

        fn optional_i32_column(&self, column: &str) -> Result<Option<i32>, sqlx::Error> {
            self.fail_if_needed(column)?;
            if let Some((_, value)) = self
                .optional_i32_overrides
                .iter()
                .find(|(candidate, _)| *candidate == column)
            {
                return Ok(*value);
            }
            Ok(match column {
                "turn" => Some(4),
                "round" => Some(2),
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            })
        }

        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            self.fail_if_needed(column)?;
            if let Some((_, value)) = self
                .i64_overrides
                .iter()
                .find(|(candidate, _)| *candidate == column)
            {
                return Ok(*value);
            }
            Ok(match column {
                "referenced_by_manifest_count" => 1,
                "referenced_by_state_items_count" => 2,
                "referenced_by_citation_count" => 3,
                "referenced_by_durable_count" => 4,
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            })
        }
    }

    fn assert_database_error_mentions(
        result: Result<impl std::fmt::Debug, SessionArtifactStoreError>,
        column: &str,
    ) {
        let err = result.expect_err("decode should fail");
        match err {
            SessionArtifactStoreError::Database(source) => {
                assert!(
                    source.to_string().contains(column),
                    "database error should contain `{column}`, got `{source}`"
                );
            }
            other => panic!("expected database error, got {other:?}"),
        }
    }

    fn assert_json_decode_column(
        result: Result<impl std::fmt::Debug, SessionArtifactStoreError>,
        column: &'static str,
    ) {
        let err = result.expect_err("decode should fail");
        assert!(
            matches!(err, SessionArtifactStoreError::JsonDecode { column: actual, .. } if actual == column),
            "expected JsonDecode for {column}, got {err:?}"
        );
    }

    fn assert_invalid_database_column(
        result: Result<impl std::fmt::Debug, SessionArtifactStoreError>,
        column: &'static str,
    ) {
        let err = result.expect_err("decode should fail");
        assert!(
            matches!(err, SessionArtifactStoreError::InvalidDatabaseValue { column: actual, .. } if actual == column),
            "expected InvalidDatabaseValue for {column}, got {err:?}"
        );
    }

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
            references: Vec::new(),
        };
        let value = serde_json::to_value(&record).unwrap();
        assert_eq!(value["artifact_kind"], "llm_capture");
        assert_eq!(value["source"], "server_loop_host");
        assert_eq!(value["metadata"]["model"], "gpt-5.4");
    }

    fn projection_record(artifact_id: &str) -> SessionArtifactJsonRecord {
        SessionArtifactJsonRecord {
            artifact_id: artifact_id.to_string(),
            session_id: "sess-123".into(),
            user_id: "user-1".into(),
            artifact_kind: "projection_test".into(),
            source: Some("test".into()),
            turn: None,
            round: None,
            content: serde_json::json!({"version": 1}),
            metadata: None,
            references: Vec::new(),
        }
    }

    #[test]
    fn mutable_projection_id_validation_matches_storage_and_namespace_contract() {
        let max_name =
            "a".repeat(MAX_ARTIFACT_ID_BYTES - MUTABLE_ARTIFACT_PROJECTION_ID_PREFIX.len());
        assert!(is_valid_mutable_artifact_projection_id(&format!(
            "{MUTABLE_ARTIFACT_PROJECTION_ID_PREFIX}{max_name}"
        )));
        assert!(!is_valid_mutable_artifact_projection_id(&format!(
            "{MUTABLE_ARTIFACT_PROJECTION_ID_PREFIX}{max_name}a"
        )));

        for invalid_id in [
            "projection:",
            "projection:../escape",
            "projection:a/b",
            "projection:UPPER",
            "projection:-leading",
            "projection:trailing-",
            "projection:two--parts",
        ] {
            assert!(
                !is_valid_mutable_artifact_projection_id(invalid_id),
                "projection identity must be canonical: {invalid_id}"
            );
        }
    }

    #[tokio::test]
    async fn mutable_projection_namespace_cannot_be_used_as_an_immutable_artifact() {
        let store = DatabaseSessionArtifactStore::new(MatrixOneSettings::default());
        let error = store
            .persist_json_artifact(projection_record("projection:test"))
            .await
            .expect_err("immutable writes must not claim projection identities");
        assert!(matches!(
            error,
            SessionArtifactStoreError::ReservedMutableProjectionId(_)
        ));
    }

    #[tokio::test]
    async fn mutable_projection_requires_reserved_identity_and_rejects_references() {
        let store = DatabaseSessionArtifactStore::new(MatrixOneSettings::default());
        let error = store
            .upsert_json_artifact_projection(projection_record("ordinary-id"))
            .await
            .expect_err("projection identity must be explicit before database access");
        assert!(matches!(
            error,
            SessionArtifactStoreError::InvalidMutableProjectionId { .. }
        ));

        let error = store
            .upsert_json_artifact_projection(projection_record("projection:  "))
            .await
            .expect_err("projection identity must name a concrete projection");
        assert!(matches!(
            error,
            SessionArtifactStoreError::InvalidMutableProjectionId { .. }
        ));

        let mut referenced = projection_record("projection:test");
        referenced.references.push(SessionArtifactReference {
            kind: SessionArtifactReferenceKind::Manifest,
            reference_id: "manifest-1".into(),
        });
        let error = store
            .upsert_json_artifact_projection(referenced)
            .await
            .expect_err("mutable projections cannot arrive with durable references");
        assert!(matches!(
            error,
            SessionArtifactStoreError::MutableProjectionReferencesUnsupported
        ));
    }

    #[tokio::test]
    async fn mutable_projection_cannot_acquire_a_reference_after_creation() {
        let store = DatabaseSessionArtifactStore::new(MatrixOneSettings::default());
        let error = store
            .retain_json_artifact_reference(
                "user-1",
                "sess-123",
                "projection:test",
                &SessionArtifactReference {
                    kind: SessionArtifactReferenceKind::Manifest,
                    reference_id: "manifest-1".into(),
                },
            )
            .await
            .expect_err("projection retain must fail before database access");
        assert!(matches!(
            error,
            SessionArtifactStoreError::MutableProjectionReferencesUnsupported
        ));
    }

    #[test]
    fn artifact_reference_contract_rejects_empty_oversized_and_duplicate_owners() {
        for reference_id in [String::new(), "x".repeat(129)] {
            let error = validate_artifact_references(&[SessionArtifactReference {
                kind: SessionArtifactReferenceKind::InvocationLedger,
                reference_id,
            }])
            .unwrap_err();
            assert!(matches!(
                error,
                SessionArtifactStoreError::InvalidReferenceId(_)
            ));
        }

        let reference = SessionArtifactReference {
            kind: SessionArtifactReferenceKind::InvocationLedger,
            reference_id: "sha256:invocation".to_string(),
        };
        let error = validate_artifact_references(&[reference.clone(), reference]).unwrap_err();
        assert!(matches!(
            error,
            SessionArtifactStoreError::DuplicateReference {
                kind: "invocation_ledger",
                ..
            }
        ));
    }

    #[test]
    fn stored_artifact_row_decode_preserves_values_and_fails_loudly() {
        let artifact =
            stored_artifact_from_row(&FakeArtifactRow::complete()).expect("artifact row decodes");
        assert_eq!(artifact.artifact_id, "artifact-1");
        assert_eq!(artifact.session_id, "session-1");
        assert_eq!(artifact.user_id, "user-1");
        assert_eq!(artifact.artifact_kind, "llm_capture");
        assert_eq!(artifact.source.as_deref(), Some("server_loop_host"));
        assert_eq!(artifact.turn, Some(4));
        assert_eq!(artifact.round, Some(2));
        assert_eq!(artifact.content["ok"], true);
        assert_eq!(artifact.metadata.as_ref().unwrap()["model"], "gpt-5.4");
        assert_eq!(artifact.retention_policy.as_deref(), Some("default"));
        assert_eq!(
            artifact.retention_until.as_deref(),
            Some("2026-06-26 12:00:00")
        );
        assert_eq!(artifact.status.as_deref(), Some("active"));
        assert_eq!(artifact.referenced_by_manifest_count, 1);
        assert_eq!(artifact.referenced_by_state_items_count, 2);
        assert_eq!(artifact.referenced_by_citation_count, 3);
        assert_eq!(artifact.referenced_by_durable_count, 4);
        assert_eq!(artifact.created_at.as_deref(), Some("2026-06-26 10:00:00"));

        for column in [
            "artifact_id",
            "session_id",
            "user_id",
            "artifact_kind",
            "source",
            "turn",
            "round",
            "content_json",
            "metadata_json",
            "retention_policy",
            "retention_until",
            "status",
            "referenced_by_manifest_count",
            "referenced_by_state_items_count",
            "referenced_by_citation_count",
            "referenced_by_durable_count",
            "created_at",
        ] {
            assert_database_error_mentions(
                stored_artifact_from_row(&FakeArtifactRow::fail_on(column)),
                column,
            );
        }
    }

    #[test]
    fn stored_artifact_row_decode_rejects_bad_json_and_invalid_counters() {
        assert_json_decode_column(
            stored_artifact_from_row(&FakeArtifactRow::with_content_json("{not-json")),
            "content_json",
        );
        assert_json_decode_column(
            stored_artifact_from_row(&FakeArtifactRow::with_metadata_json(Some(
                "{not-json".to_string(),
            ))),
            "metadata_json",
        );
        let no_metadata = stored_artifact_from_row(&FakeArtifactRow::with_metadata_json(None))
            .expect("null metadata decodes");
        assert_eq!(no_metadata.metadata, None);

        assert_invalid_database_column(
            stored_artifact_from_row(&FakeArtifactRow::with_optional_i32("turn", Some(-1))),
            "turn",
        );
        assert_invalid_database_column(
            stored_artifact_from_row(&FakeArtifactRow::with_optional_i32("round", Some(-1))),
            "round",
        );
        assert_invalid_database_column(
            stored_artifact_from_row(&FakeArtifactRow::with_i64(
                "referenced_by_manifest_count",
                -1,
            )),
            "referenced_by_manifest_count",
        );
        assert_invalid_database_column(
            stored_artifact_from_row(&FakeArtifactRow::with_i64(
                "referenced_by_state_items_count",
                i64::from(u32::MAX) + 1,
            )),
            "referenced_by_state_items_count",
        );
        assert_invalid_database_column(
            stored_artifact_from_row(&FakeArtifactRow::with_i64(
                "referenced_by_citation_count",
                -1,
            )),
            "referenced_by_citation_count",
        );
        assert_invalid_database_column(
            stored_artifact_from_row(&FakeArtifactRow::with_i64(
                "referenced_by_durable_count",
                -1,
            )),
            "referenced_by_durable_count",
        );
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

    #[test]
    fn artifact_list_cursor_validates_timestamp_and_id() {
        let cursor = SessionArtifactListCursor {
            created_at: "2026-10-01T12:34:56.123456".to_string(),
            artifact_id: "artifact-1".to_string(),
        };
        assert_eq!(
            artifact_list_cursor_db_created_at(&cursor).unwrap(),
            "2026-10-01 12:34:56.123456"
        );
        assert_eq!(
            artifact_list_cursor_artifact_id(&cursor).unwrap(),
            "artifact-1"
        );

        let invalid_time = SessionArtifactListCursor {
            created_at: "2026-10-01T12:34:56".to_string(),
            artifact_id: "artifact-1".to_string(),
        };
        assert!(artifact_list_cursor_db_created_at(&invalid_time).is_err());

        let missing_id = SessionArtifactListCursor {
            created_at: "2026-10-01T12:34:56.123456".to_string(),
            artifact_id: "  ".to_string(),
        };
        assert!(artifact_list_cursor_artifact_id(&missing_id).is_err());
    }

    #[test]
    fn artifact_list_limit_is_bounded_and_fetches_one_extra_row() {
        assert_eq!(validate_artifact_list_limit(0), 1);
        assert_eq!(validate_artifact_list_limit(20), 20);
        assert_eq!(validate_artifact_list_limit(usize::MAX), 100);
        assert_eq!(artifact_list_query_limit(100), 101);
    }
}
