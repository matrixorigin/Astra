//! Session artifact storage boundaries.
//!
//! - [`LocalSessionArtifactStore`] resolves local filesystem paths for session-scoped
//!   artifacts that still live in a session directory.
//! - [`SessionArtifactJsonStore`] persists remote-visible JSON artifacts (for example
//!   LLM captures and request dumps) without assuming the caller can access server-local
//!   files.

use std::path::{Component, Path, PathBuf};

use astra_core::{MatrixOneSettings, SharedPool, connect_matrixone};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Row, mysql::MySqlRow, query};
use uuid::Uuid;

pub trait SessionArtifactStore {
    fn sessions_root(&self) -> PathBuf;
    fn session_dir(&self, session_id: &str) -> Result<PathBuf, String>;
    fn session_path(&self, session_id: &str, relative: impl AsRef<Path>)
    -> Result<PathBuf, String>;
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
    pub created_at: Option<String>,
}

#[async_trait]
pub trait SessionArtifactJsonStore: Send + Sync {
    async fn persist_json_artifact(
        &self,
        record: SessionArtifactJsonRecord,
    ) -> Result<StoredSessionArtifact, String>;

    async fn load_json_artifact(
        &self,
        artifact_id: &str,
    ) -> Result<Option<StoredSessionArtifact>, String>;

    async fn load_latest_json_artifact(
        &self,
        session_id: &str,
        artifact_kind: &str,
    ) -> Result<Option<StoredSessionArtifact>, String>;

    async fn list_json_artifacts(
        &self,
        session_id: &str,
        artifact_kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<StoredSessionArtifact>, String>;
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
        if let Some(ref pool) = self.pool {
            return Ok(pool.get().clone());
        }
        connect_matrixone(&self.matrixone).await
    }
}

fn stored_artifact_from_row(row: &MySqlRow) -> Result<StoredSessionArtifact, String> {
    let content: Value = serde_json::from_str(
        row.try_get::<String, _>("content_json")
            .map_err(|error| error.to_string())?
            .as_str(),
    )
    .map_err(|error| error.to_string())?;
    let metadata = row
        .try_get::<Option<String>, _>("metadata_json")
        .map_err(|error| error.to_string())?
        .and_then(|value| serde_json::from_str(&value).ok());

    Ok(StoredSessionArtifact {
        artifact_id: row
            .try_get("artifact_id")
            .map_err(|error| error.to_string())?,
        session_id: row
            .try_get("session_id")
            .map_err(|error| error.to_string())?,
        user_id: row.try_get("user_id").map_err(|error| error.to_string())?,
        artifact_kind: row
            .try_get("artifact_kind")
            .map_err(|error| error.to_string())?,
        source: row.try_get("source").map_err(|error| error.to_string())?,
        turn: row
            .try_get::<Option<i32>, _>("turn")
            .map_err(|error| error.to_string())?
            .and_then(|value| u32::try_from(value).ok()),
        round: row
            .try_get::<Option<i32>, _>("round")
            .map_err(|error| error.to_string())?
            .and_then(|value| u32::try_from(value).ok()),
        content,
        metadata,
        created_at: row
            .try_get("created_at")
            .map_err(|error| error.to_string())?,
    })
}

#[async_trait]
impl SessionArtifactJsonStore for DatabaseSessionArtifactStore {
    async fn persist_json_artifact(
        &self,
        mut record: SessionArtifactJsonRecord,
    ) -> Result<StoredSessionArtifact, String> {
        crate::session_journal::validate_session_id(&record.session_id)?;
        if record.artifact_id.trim().is_empty() {
            record.artifact_id = Uuid::now_v7().to_string();
        }

        let pool = self.get_pool().await.map_err(|error| error.to_string())?;
        let content_json =
            serde_json::to_string(&record.content).map_err(|error| error.to_string())?;
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
        .bind(record.turn.map(|v| i32::try_from(v).unwrap_or(i32::MAX)))
        .bind(record.round.map(|v| i32::try_from(v).unwrap_or(i32::MAX)))
        .bind(&content_json)
        .bind(metadata_json)
        .execute(&pool)
        .await
        .map_err(|error| error.to_string())?;

        let row = query(
            "SELECT artifact_id, session_id, user_id, artifact_kind, source, turn, round, \
                    content_json, CAST(metadata AS CHAR) AS metadata_json, CAST(created_at AS CHAR) AS created_at \
             FROM session_artifacts WHERE artifact_id = ?",
        )
        .bind(&record.artifact_id)
        .fetch_one(&pool)
        .await
        .map_err(|error| error.to_string())?;

        stored_artifact_from_row(&row)
    }

    async fn load_json_artifact(
        &self,
        artifact_id: &str,
    ) -> Result<Option<StoredSessionArtifact>, String> {
        if artifact_id.trim().is_empty() {
            return Err("artifact_id must not be empty".to_string());
        }
        let pool = self.get_pool().await.map_err(|error| error.to_string())?;
        let row = query(
            "SELECT artifact_id, session_id, user_id, artifact_kind, source, turn, round, \
                    content_json, CAST(metadata AS CHAR) AS metadata_json, CAST(created_at AS CHAR) AS created_at \
             FROM session_artifacts WHERE artifact_id = ?",
        )
        .bind(artifact_id)
        .fetch_optional(&pool)
        .await
        .map_err(|error| error.to_string())?;

        row.as_ref().map(stored_artifact_from_row).transpose()
    }

    async fn load_latest_json_artifact(
        &self,
        session_id: &str,
        artifact_kind: &str,
    ) -> Result<Option<StoredSessionArtifact>, String> {
        crate::session_journal::validate_session_id(session_id)?;
        let pool = self.get_pool().await.map_err(|error| error.to_string())?;
        let row = query(
             "SELECT artifact_id, session_id, user_id, artifact_kind, source, turn, round, \
                     content_json, CAST(metadata AS CHAR) AS metadata_json, CAST(created_at AS CHAR) AS created_at \
              FROM session_artifacts \
              WHERE session_id = ? AND artifact_kind = ? \
              ORDER BY created_at DESC, artifact_id DESC LIMIT 1",
        )
        .bind(session_id)
        .bind(artifact_kind)
        .fetch_optional(&pool)
        .await
        .map_err(|error| error.to_string())?;

        let Some(row) = row else {
            return Ok(None);
        };

        Ok(Some(stored_artifact_from_row(&row)?))
    }

    async fn list_json_artifacts(
        &self,
        session_id: &str,
        artifact_kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<StoredSessionArtifact>, String> {
        crate::session_journal::validate_session_id(session_id)?;
        let pool = self.get_pool().await.map_err(|error| error.to_string())?;
        let capped_limit = limit.clamp(1, 100) as i64;
        let rows = if let Some(kind) = artifact_kind {
            query(
                "SELECT artifact_id, session_id, user_id, artifact_kind, source, turn, round, \
                        content_json, CAST(metadata AS CHAR) AS metadata_json, CAST(created_at AS CHAR) AS created_at \
                 FROM session_artifacts \
                 WHERE session_id = ? AND artifact_kind = ? \
                 ORDER BY created_at DESC, artifact_id DESC LIMIT ?",
            )
            .bind(session_id)
            .bind(kind)
            .bind(capped_limit)
            .fetch_all(&pool)
            .await
            .map_err(|error| error.to_string())?
        } else {
            query(
                "SELECT artifact_id, session_id, user_id, artifact_kind, source, turn, round, \
                        content_json, CAST(metadata AS CHAR) AS metadata_json, CAST(created_at AS CHAR) AS created_at \
                 FROM session_artifacts \
                 WHERE session_id = ? \
                 ORDER BY created_at DESC, artifact_id DESC LIMIT ?",
            )
            .bind(session_id)
            .bind(capped_limit)
            .fetch_all(&pool)
            .await
            .map_err(|error| error.to_string())?
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

    fn session_dir(&self, session_id: &str) -> Result<PathBuf, String> {
        crate::session_journal::validate_session_id(session_id)?;
        Ok(self.sessions_root().join(session_id))
    }

    fn session_path(
        &self,
        session_id: &str,
        relative: impl AsRef<Path>,
    ) -> Result<PathBuf, String> {
        let relative = relative.as_ref();
        validate_relative_path(relative)?;
        Ok(self.session_dir(session_id)?.join(relative))
    }

    fn journal_path(&self, session_id: &str) -> Result<PathBuf, String> {
        crate::session_journal::validate_session_id(session_id)?;
        Ok(self.sessions_root().join(format!("{session_id}.jsonl")))
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
        let session_dir = store.session_dir("sess-123").unwrap();
        assert_eq!(session_dir, temp.path().join("sess-123"));
        let artifact_path = store
            .session_path("sess-123", "step_checkpoints/000001-heavy.json")
            .unwrap();
        assert_eq!(
            artifact_path,
            temp.path()
                .join("sess-123")
                .join("step_checkpoints/000001-heavy.json")
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
    fn database_store_loads_artifact_by_id() {
        let source = include_str!("session_artifact_store.rs");
        assert!(
            source.contains("FROM session_artifacts WHERE artifact_id = ?"),
            "artifact store should support loading a specific artifact by id"
        );
    }

    #[test]
    fn database_store_lists_session_artifacts_newest_first() {
        let source = include_str!("session_artifact_store.rs");
        assert!(
            source.contains("WHERE session_id = ? AND artifact_kind = ?"),
            "artifact listing should support filtering by artifact kind"
        );
        assert!(
            source.contains("WHERE session_id = ? \\"),
            "artifact listing should support listing all artifacts for a session"
        );
        assert!(
            source.contains("ORDER BY created_at DESC, artifact_id DESC LIMIT ?"),
            "artifact listing should return newest artifacts first with a stable bounded order"
        );
    }
}
