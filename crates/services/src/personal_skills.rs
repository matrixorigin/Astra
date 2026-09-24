use std::cmp::max;

use astra_core::SharedPool;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::Row;
use thiserror::Error;
use uuid::Uuid;

use crate::state_projection::{DatabaseStateProjectionStore, StateProjectionError};

pub const MAX_ACTIVE_PERSONAL_SKILLS: usize = 64;

pub const SKILL_MD_NORMALIZE_VERSION: &str = "skill_md_v1";

#[derive(Debug, Error)]
pub enum PersonalSkillError {
    #[error("database operation failed: operation={operation}, entity={entity}, source={source}")]
    Database {
        operation: &'static str,
        entity: String,
        #[source]
        source: sqlx::Error,
    },
    #[error("json serialization failed: operation={operation}, entity={entity}, source={source}")]
    Json {
        operation: &'static str,
        entity: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("state projection failed: operation={operation}, entity={entity}, source={source}")]
    StateProjection {
        operation: &'static str,
        entity: String,
        #[source]
        source: Box<StateProjectionError>,
    },
    #[error("invalid skill version status {status}")]
    InvalidStatus { status: String },
    #[error(
        "skill version not found: owner={owner_user_id}, skill={skill_name}, version_id={version_id}"
    )]
    VersionNotFound {
        owner_user_id: String,
        skill_name: String,
        version_id: String,
    },
    #[error("skill version is not activatable: version_id={version_id}, status={status}")]
    VersionNotActivatable { version_id: String, status: String },
    #[error("session not found or inactive: owner={owner_user_id}, session={session_id}")]
    SessionNotActive {
        owner_user_id: String,
        session_id: String,
    },
    #[error(
        "invalid active personal skill projection: owner={owner_user_id}, session={session_id}, skill={skill_name}"
    )]
    InvalidActiveProjection {
        owner_user_id: String,
        session_id: String,
        skill_name: String,
    },
    #[error("session already has {limit} active personal Skills")]
    ActivationLimitReached { limit: usize },
    #[error(
        "skill activation changed concurrently: skill={skill_name}, expected={expected:?}, actual={actual:?}"
    )]
    ActivationConflict {
        skill_name: String,
        expected: Option<String>,
        actual: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserSkillSourceRecord {
    pub source_id: String,
    pub owner_user_id: String,
    pub skill_name: String,
    pub visibility: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserSkillVersionRecord {
    pub version_id: String,
    pub source_id: String,
    pub owner_user_id: String,
    pub skill_name: String,
    pub version: String,
    pub manifest_json: Value,
    pub content_markdown: String,
    pub content_hash: String,
    pub normalize_version: String,
    pub token_estimate: u32,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivePersonalSkillRecord {
    pub skill_name: String,
    pub version_id: String,
    pub version: String,
    /// Immutable content identity carried into every runtime invocation.
    /// The version label alone is not sufficient because a mutable source
    /// projection or an incorrectly restored checkpoint could otherwise load
    /// different bytes under the same display version.
    pub content_hash: String,
    pub content_markdown: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateUserSkillSource {
    pub skill_name: String,
    pub visibility: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitUserSkillVersion {
    pub version: String,
    pub manifest_json: Value,
    pub content_markdown: String,
    pub status: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivateUserSkillVersion {
    pub session_id: String,
    pub version_id: String,
    #[serde(default)]
    pub expected_active_version_id: Option<String>,
}

#[derive(Clone)]
pub struct DatabasePersonalSkillStore {
    pool: SharedPool,
}

impl DatabasePersonalSkillStore {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    pub async fn create_source(
        &self,
        owner_user_id: &str,
        request: CreateUserSkillSource,
    ) -> Result<UserSkillSourceRecord, PersonalSkillError> {
        let mut conn = self
            .pool
            .get()
            .acquire()
            .await
            .map_err(|error| db_error("create_source", request.skill_name.clone(), error))?;
        Self::create_source_on(&mut conn, owner_user_id, request).await
    }

    pub(crate) async fn create_source_on(
        conn: &mut sqlx::MySqlConnection,
        owner_user_id: &str,
        request: CreateUserSkillSource,
    ) -> Result<UserSkillSourceRecord, PersonalSkillError> {
        if Self::load_source_optional_on(&mut *conn, owner_user_id, &request.skill_name)
            .await?
            .is_some()
        {
            let visibility = request.visibility.unwrap_or_else(|| "private".to_string());
            sqlx::query(
                "UPDATE user_skill_sources SET visibility = ?, updated_at = NOW(6)
                 WHERE owner_user_id = ? AND skill_name = ?",
            )
            .bind(&visibility)
            .bind(owner_user_id)
            .bind(&request.skill_name)
            .execute(&mut *conn)
            .await
            .map_err(|source| PersonalSkillError::Database {
                operation: "update_user_skill_source",
                entity: request.skill_name.clone(),
                source,
            })?;
            return Self::load_source_on(&mut *conn, owner_user_id, &request.skill_name).await;
        }
        let source_id = format!("skill-source-{}", Uuid::new_v4());
        let visibility = request.visibility.unwrap_or_else(|| "private".to_string());
        sqlx::query(
            "INSERT INTO user_skill_sources
             (source_id, owner_user_id, skill_name, visibility, status, created_at, updated_at)
             VALUES (?, ?, ?, ?, 'active', NOW(6), NOW(6))",
        )
        .bind(&source_id)
        .bind(owner_user_id)
        .bind(&request.skill_name)
        .bind(&visibility)
        .execute(&mut *conn)
        .await
        .map_err(|source| PersonalSkillError::Database {
            operation: "create_user_skill_source",
            entity: request.skill_name.clone(),
            source,
        })?;
        Self::load_source_on(&mut *conn, owner_user_id, &request.skill_name).await
    }

    /// Return an existing source without changing its visibility, or create a
    /// private source atomically when it does not exist. Authoring candidates
    /// use this path so generating a draft cannot rewrite an active Skill's
    /// sharing policy, including under a concurrent first write.
    pub async fn ensure_source(
        &self,
        owner_user_id: &str,
        request: CreateUserSkillSource,
    ) -> Result<UserSkillSourceRecord, PersonalSkillError> {
        let mut conn = self
            .pool
            .get()
            .acquire()
            .await
            .map_err(|error| db_error("ensure_source", request.skill_name.clone(), error))?;
        Self::ensure_source_on(&mut conn, owner_user_id, request).await
    }

    pub(crate) async fn ensure_source_on(
        conn: &mut sqlx::MySqlConnection,
        owner_user_id: &str,
        request: CreateUserSkillSource,
    ) -> Result<UserSkillSourceRecord, PersonalSkillError> {
        let source_id = format!("skill-source-{}", Uuid::new_v4());
        let visibility = request.visibility.unwrap_or_else(|| "private".to_string());
        sqlx::query(
            "INSERT INTO user_skill_sources
             (source_id, owner_user_id, skill_name, visibility, status, created_at, updated_at)
             VALUES (?, ?, ?, ?, 'active', NOW(6), NOW(6))
             ON DUPLICATE KEY UPDATE visibility = visibility",
        )
        .bind(&source_id)
        .bind(owner_user_id)
        .bind(&request.skill_name)
        .bind(&visibility)
        .execute(&mut *conn)
        .await
        .map_err(|source| PersonalSkillError::Database {
            operation: "ensure_user_skill_source",
            entity: request.skill_name.clone(),
            source,
        })?;
        Self::load_source_on(&mut *conn, owner_user_id, &request.skill_name).await
    }

    pub async fn submit_version(
        &self,
        owner_user_id: &str,
        skill_name: &str,
        request: SubmitUserSkillVersion,
    ) -> Result<UserSkillVersionRecord, PersonalSkillError> {
        let mut conn = self
            .pool
            .get()
            .acquire()
            .await
            .map_err(|error| db_error("submit_version", skill_name, error))?;
        Self::submit_version_on(&mut conn, owner_user_id, skill_name, request).await
    }

    pub(crate) async fn submit_version_on(
        conn: &mut sqlx::MySqlConnection,
        owner_user_id: &str,
        skill_name: &str,
        request: SubmitUserSkillVersion,
    ) -> Result<UserSkillVersionRecord, PersonalSkillError> {
        let source =
            match Self::load_source_optional_on(&mut *conn, owner_user_id, skill_name).await? {
                Some(source) => source,
                None => {
                    Self::create_source_on(
                        &mut *conn,
                        owner_user_id,
                        CreateUserSkillSource {
                            skill_name: skill_name.to_string(),
                            visibility: Some("private".to_string()),
                        },
                    )
                    .await?
                }
            };
        let status = request.status.unwrap_or_else(|| "draft".to_string());
        validate_version_status(&status)?;
        let canonical = normalize_skill_md(&request.manifest_json, &request.content_markdown);
        let content_hash = sha256_prefixed(&canonical);
        let manifest_json = serde_json::to_string(&request.manifest_json).map_err(|source| {
            PersonalSkillError::Json {
                operation: "serialize_skill_manifest",
                entity: skill_name.to_string(),
                source,
            }
        })?;
        let version_id = format!("skill-version-{}", Uuid::new_v4());
        let token_estimate = estimate_tokens(&canonical);
        sqlx::query(
            "INSERT INTO user_skill_versions
             (version_id, source_id, owner_user_id, skill_name, version, manifest_json,
              content_markdown, content_hash, normalize_version, token_estimate, status,
              created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6))",
        )
        .bind(&version_id)
        .bind(&source.source_id)
        .bind(owner_user_id)
        .bind(skill_name)
        .bind(&request.version)
        .bind(&manifest_json)
        .bind(&request.content_markdown)
        .bind(&content_hash)
        .bind(SKILL_MD_NORMALIZE_VERSION)
        .bind(i64::from(token_estimate))
        .bind(&status)
        .execute(&mut *conn)
        .await
        .map_err(|source| PersonalSkillError::Database {
            operation: "submit_user_skill_version",
            entity: format!("{skill_name}@{}", request.version),
            source,
        })?;
        Self::load_version_by_id_on(&mut *conn, owner_user_id, skill_name, &version_id)
            .await?
            .ok_or_else(|| PersonalSkillError::VersionNotFound {
                owner_user_id: owner_user_id.to_string(),
                skill_name: skill_name.to_string(),
                version_id,
            })
    }

    pub async fn list_sources(
        &self,
        owner_user_id: &str,
        prefix: Option<&str>,
    ) -> Result<Vec<UserSkillSourceRecord>, PersonalSkillError> {
        let rows = if let Some(prefix) = prefix.filter(|p| !p.is_empty()) {
            sqlx::query(
                "SELECT source_id, owner_user_id, skill_name, visibility, status,
                        CAST(created_at AS CHAR) AS created_at, CAST(updated_at AS CHAR) AS updated_at
                 FROM user_skill_sources FORCE INDEX (idx_user_skill_owner_name)
                 WHERE owner_user_id = ? AND skill_name >= ? AND skill_name < ?
                 ORDER BY skill_name ASC LIMIT 100",
            )
            .bind(owner_user_id)
            .bind(prefix)
            .bind(prefix_upper_bound(prefix))
            .fetch_all(self.pool.get())
            .await
        } else {
            sqlx::query(
                "SELECT source_id, owner_user_id, skill_name, visibility, status,
                        CAST(created_at AS CHAR) AS created_at, CAST(updated_at AS CHAR) AS updated_at
                 FROM user_skill_sources FORCE INDEX (idx_user_skill_owner_name)
                 WHERE owner_user_id = ?
                 ORDER BY skill_name ASC LIMIT 100",
            )
            .bind(owner_user_id)
            .fetch_all(self.pool.get())
            .await
        }
        .map_err(|source| PersonalSkillError::Database {
            operation: "list_user_skill_sources",
            entity: owner_user_id.to_string(),
            source,
        })?;
        rows.into_iter()
            .map(|row| source_from_row(row, "list_user_skill_sources", owner_user_id))
            .collect()
    }

    pub async fn list_versions(
        &self,
        owner_user_id: &str,
        skill_name: &str,
    ) -> Result<Vec<UserSkillVersionRecord>, PersonalSkillError> {
        let rows = sqlx::query(
            "SELECT version_id, source_id, owner_user_id, skill_name, version, manifest_json,
                    content_markdown, content_hash, normalize_version, token_estimate, status,
                    CAST(created_at AS CHAR) AS created_at, CAST(updated_at AS CHAR) AS updated_at
             FROM user_skill_versions FORCE INDEX (idx_user_skill_versions_owner_name)
             WHERE owner_user_id = ? AND skill_name = ?
             ORDER BY created_at ASC",
        )
        .bind(owner_user_id)
        .bind(skill_name)
        .fetch_all(self.pool.get())
        .await
        .map_err(|source| PersonalSkillError::Database {
            operation: "list_user_skill_versions",
            entity: skill_name.to_string(),
            source,
        })?;
        rows.into_iter()
            .map(|row| version_from_row(row, "list_user_skill_versions", skill_name))
            .collect()
    }

    /// Load one immutable owner-scoped revision for a trusted execution
    /// adapter. Callers must still validate the returned content hash and
    /// lifecycle status against their frozen experiment.
    pub async fn load_version(
        &self,
        owner_user_id: &str,
        skill_name: &str,
        version_id: &str,
    ) -> Result<Option<UserSkillVersionRecord>, PersonalSkillError> {
        self.load_version_by_id(owner_user_id, skill_name, version_id)
            .await
    }

    /// Load one immutable owner-scoped revision by its source version label.
    /// Authoring retries use this to converge concurrent materialization of the
    /// same content without relying on a random version id.
    pub async fn load_version_by_version(
        &self,
        owner_user_id: &str,
        skill_name: &str,
        version: &str,
    ) -> Result<Option<UserSkillVersionRecord>, PersonalSkillError> {
        let mut conn = self
            .pool
            .get()
            .acquire()
            .await
            .map_err(|error| db_error("load_version_by_version", skill_name, error))?;
        Self::load_version_by_version_on(&mut conn, owner_user_id, skill_name, version).await
    }

    pub(crate) async fn load_version_by_version_on(
        conn: &mut sqlx::MySqlConnection,
        owner_user_id: &str,
        skill_name: &str,
        version: &str,
    ) -> Result<Option<UserSkillVersionRecord>, PersonalSkillError> {
        let row = sqlx::query(
            "SELECT version_id, source_id, owner_user_id, skill_name, version, manifest_json,
                    content_markdown, content_hash, normalize_version, token_estimate, status,
                    CAST(created_at AS CHAR) AS created_at, CAST(updated_at AS CHAR) AS updated_at
             FROM user_skill_versions
             WHERE owner_user_id = ? AND skill_name = ? AND version = ?",
        )
        .bind(owner_user_id)
        .bind(skill_name)
        .bind(version)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|source| PersonalSkillError::Database {
            operation: "load_user_skill_version_by_version",
            entity: format!("{skill_name}@{version}"),
            source,
        })?;
        row.map(|row| version_from_row(row, "load_user_skill_version_by_version", version))
            .transpose()
    }

    /// Activate a version with an explicit compare-and-set expectation.
    ///
    /// `None` means the caller expects no active version for this skill. An
    /// already-applied activation is idempotent only when the caller supplies
    /// that same version as its expected baseline; a stale writer is a
    /// visible conflict rather than an implicit overwrite.
    pub async fn activate_version_with_expected(
        &self,
        owner_user_id: &str,
        session_id: &str,
        skill_name: &str,
        version_id: &str,
        expected_active_version_id: Option<&str>,
    ) -> Result<UserSkillVersionRecord, PersonalSkillError> {
        let version = self
            .load_version_by_id(owner_user_id, skill_name, version_id)
            .await?
            .ok_or_else(|| PersonalSkillError::VersionNotFound {
                owner_user_id: owner_user_id.to_string(),
                skill_name: skill_name.to_string(),
                version_id: version_id.to_string(),
            })?;
        if version.status != "published" {
            return Err(PersonalSkillError::VersionNotActivatable {
                version_id: version_id.to_string(),
                status: version.status,
            });
        }
        let activation = DatabaseStateProjectionStore::new(self.pool.clone())
            .activate_personal_skill_from_ui_with_expected(
                owner_user_id,
                session_id,
                skill_name,
                version_id,
                expected_active_version_id,
            )
            .await;
        match activation {
            Ok(()) => {}
            Err(StateProjectionError::SessionNotActive { .. }) => {
                return Err(PersonalSkillError::SessionNotActive {
                    owner_user_id: owner_user_id.to_string(),
                    session_id: session_id.to_string(),
                });
            }
            Err(StateProjectionError::PersonalSkillVersionUnavailable { .. }) => {
                return Err(PersonalSkillError::VersionNotFound {
                    owner_user_id: owner_user_id.to_string(),
                    skill_name: skill_name.to_string(),
                    version_id: version_id.to_string(),
                });
            }
            Err(StateProjectionError::PersonalSkillVersionNotActivatable { status, .. }) => {
                return Err(PersonalSkillError::VersionNotActivatable {
                    version_id: version_id.to_string(),
                    status,
                });
            }
            Err(StateProjectionError::PersonalSkillActivationLimitReached { limit }) => {
                return Err(PersonalSkillError::ActivationLimitReached { limit });
            }
            Err(StateProjectionError::PersonalSkillActivationConflict {
                skill_name,
                expected,
                actual,
            }) => {
                return Err(PersonalSkillError::ActivationConflict {
                    skill_name,
                    expected,
                    actual,
                });
            }
            Err(source) => {
                return Err(PersonalSkillError::StateProjection {
                    operation: "activate_user_skill_version",
                    entity: version_id.to_string(),
                    source: Box::new(source),
                });
            }
        }
        Ok(version)
    }

    pub async fn load_active_for_session(
        &self,
        owner_user_id: &str,
        session_id: &str,
    ) -> Result<Vec<ActivePersonalSkillRecord>, PersonalSkillError> {
        let rows = sqlx::query(
            "SELECT state.item_key AS skill_name, state.payload_json,
                    versions.version_id, versions.version, versions.content_hash,
                    versions.manifest_json, versions.content_markdown,
                    versions.status AS version_status
             FROM session_state_items state
             JOIN agent_sessions sessions
               ON sessions.user_id = state.user_id AND sessions.session_id = state.session_id
              AND sessions.status = 'active'
             LEFT JOIN user_skill_versions versions
               ON versions.owner_user_id = state.user_id
              AND versions.skill_name = state.item_key
              AND versions.version_id = JSON_UNQUOTE(JSON_EXTRACT(state.payload_json, '$.version_id'))
             WHERE state.user_id = ? AND state.session_id = ?
               AND state.scope = 'session' AND state.category = 'active_skill'
               AND state.status = 'active'
             ORDER BY state.item_key ASC LIMIT ?",
        )
        .bind(owner_user_id)
        .bind(session_id)
        .bind((MAX_ACTIVE_PERSONAL_SKILLS + 1) as u32)
        .fetch_all(self.pool.get())
        .await
        .map_err(|source| PersonalSkillError::Database {
            operation: "load_active_personal_skills",
            entity: session_id.to_string(),
            source,
        })?;
        if rows.len() > MAX_ACTIVE_PERSONAL_SKILLS {
            return Err(PersonalSkillError::InvalidActiveProjection {
                owner_user_id: owner_user_id.to_string(),
                session_id: session_id.to_string(),
                skill_name: "<too-many-active-skills>".to_string(),
            });
        }
        rows.into_iter()
            .map(|row| {
                let skill_name = row_string(
                    &row,
                    "load_active_personal_skills",
                    session_id,
                    "skill_name",
                )?;
                let payload_raw = row_string(
                    &row,
                    "load_active_personal_skills",
                    session_id,
                    "payload_json",
                )?;
                let payload: Value = serde_json::from_str(&payload_raw).map_err(|source| {
                    PersonalSkillError::Json {
                        operation: "deserialize_active_personal_skill",
                        entity: skill_name.clone(),
                        source,
                    }
                })?;
                let projected_name = payload.get("skill_name").and_then(Value::as_str);
                let projected_version = payload.get("version_id").and_then(Value::as_str);
                let projected_hash = payload.get("content_hash").and_then(Value::as_str);
                let version_id =
                    row.try_get::<Option<String>, _>("version_id")
                        .map_err(|source| {
                            db_error("load_active_personal_skills", &skill_name, source)
                        })?;
                let version_status =
                    row.try_get::<Option<String>, _>("version_status")
                        .map_err(|source| {
                            db_error("load_active_personal_skills", &skill_name, source)
                        })?;
                let content_hash =
                    row.try_get::<Option<String>, _>("content_hash")
                        .map_err(|source| {
                            db_error("load_active_personal_skills", &skill_name, source)
                        })?;
                let manifest_raw = row_string(
                    &row,
                    "load_active_personal_skills",
                    session_id,
                    "manifest_json",
                )?;
                let manifest: Value = serde_json::from_str(&manifest_raw).map_err(|source| {
                    PersonalSkillError::Json {
                        operation: "deserialize_active_personal_skill_manifest",
                        entity: skill_name.clone(),
                        source,
                    }
                })?;
                let content_markdown = row_string(
                    &row,
                    "load_active_personal_skills",
                    session_id,
                    "content_markdown",
                )?;
                if projected_name != Some(skill_name.as_str())
                    || projected_version.is_none()
                    || version_id.as_deref() != projected_version
                    || projected_hash.is_none()
                    || content_hash.as_deref() != projected_hash
                    || version_status.as_deref() != Some("published")
                    || content_hash.as_deref()
                        != Some(skill_md_content_hash(&manifest, &content_markdown).as_str())
                {
                    return Err(PersonalSkillError::InvalidActiveProjection {
                        owner_user_id: owner_user_id.to_string(),
                        session_id: session_id.to_string(),
                        skill_name,
                    });
                }
                Ok(ActivePersonalSkillRecord {
                    skill_name,
                    version_id: version_id.expect("validated present"),
                    version: row_string(
                        &row,
                        "load_active_personal_skills",
                        session_id,
                        "version",
                    )?,
                    content_hash: content_hash.expect("validated present"),
                    content_markdown,
                })
            })
            .collect()
    }

    async fn load_source_on(
        conn: &mut sqlx::MySqlConnection,
        owner_user_id: &str,
        skill_name: &str,
    ) -> Result<UserSkillSourceRecord, PersonalSkillError> {
        Self::load_source_optional_on(conn, owner_user_id, skill_name)
            .await?
            .ok_or_else(|| PersonalSkillError::Database {
                operation: "load_user_skill_source",
                entity: skill_name.to_string(),
                source: sqlx::Error::RowNotFound,
            })
    }

    async fn load_source_optional_on(
        conn: &mut sqlx::MySqlConnection,
        owner_user_id: &str,
        skill_name: &str,
    ) -> Result<Option<UserSkillSourceRecord>, PersonalSkillError> {
        let row = sqlx::query(
            "SELECT source_id, owner_user_id, skill_name, visibility, status,
                    CAST(created_at AS CHAR) AS created_at, CAST(updated_at AS CHAR) AS updated_at
             FROM user_skill_sources WHERE owner_user_id = ? AND skill_name = ?",
        )
        .bind(owner_user_id)
        .bind(skill_name)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|source| PersonalSkillError::Database {
            operation: "load_user_skill_source",
            entity: skill_name.to_string(),
            source,
        })?;
        row.map(|row| source_from_row(row, "load_user_skill_source", skill_name))
            .transpose()
    }

    async fn load_version_by_id(
        &self,
        owner_user_id: &str,
        skill_name: &str,
        version_id: &str,
    ) -> Result<Option<UserSkillVersionRecord>, PersonalSkillError> {
        let mut conn = self
            .pool
            .get()
            .acquire()
            .await
            .map_err(|error| db_error("load_version_by_id", skill_name, error))?;
        Self::load_version_by_id_on(&mut conn, owner_user_id, skill_name, version_id).await
    }

    async fn load_version_by_id_on(
        conn: &mut sqlx::MySqlConnection,
        owner_user_id: &str,
        skill_name: &str,
        version_id: &str,
    ) -> Result<Option<UserSkillVersionRecord>, PersonalSkillError> {
        let row = sqlx::query(
            "SELECT version_id, source_id, owner_user_id, skill_name, version, manifest_json,
                    content_markdown, content_hash, normalize_version, token_estimate, status,
                    CAST(created_at AS CHAR) AS created_at, CAST(updated_at AS CHAR) AS updated_at
             FROM user_skill_versions
             WHERE owner_user_id = ? AND skill_name = ? AND version_id = ?",
        )
        .bind(owner_user_id)
        .bind(skill_name)
        .bind(version_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|source| PersonalSkillError::Database {
            operation: "load_user_skill_version",
            entity: version_id.to_string(),
            source,
        })?;
        row.map(|row| version_from_row(row, "load_user_skill_version", version_id))
            .transpose()
    }
}

pub fn normalize_skill_md(manifest_json: &Value, content_markdown: &str) -> String {
    let mut canonical = String::new();
    canonical.push_str(&astra_core::canonical_json_string(manifest_json));
    canonical.push('\n');
    canonical.push_str(&normalize_markdown(content_markdown));
    canonical
}

pub fn skill_md_content_hash(manifest_json: &Value, content_markdown: &str) -> String {
    sha256_prefixed(&normalize_skill_md(manifest_json, content_markdown))
}

fn normalize_markdown(content: &str) -> String {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = Vec::new();
    let mut in_fence = false;
    let mut previous_blank = false;
    for raw_line in normalized.lines() {
        let line = raw_line.trim_end().to_string();
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            previous_blank = false;
            out.push(line);
            continue;
        }
        if !in_fence && line.trim().is_empty() {
            if !previous_blank {
                out.push(String::new());
            }
            previous_blank = true;
            continue;
        }
        previous_blank = false;
        out.push(line);
    }
    while out.last().is_some_and(|line| line.is_empty()) {
        out.pop();
    }
    let mut result = out.join("\n");
    result.push('\n');
    result
}

fn sha256_prefixed(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    format!("sha256:{digest:x}")
}

fn estimate_tokens(content: &str) -> u32 {
    max(1, content.len().div_ceil(4)) as u32
}

fn prefix_upper_bound(prefix: &str) -> String {
    format!("{prefix}\u{10ffff}")
}

fn validate_version_status(status: &str) -> Result<(), PersonalSkillError> {
    match status {
        "draft" | "published" | "superseded" | "quarantined" => Ok(()),
        other => Err(PersonalSkillError::InvalidStatus {
            status: other.to_string(),
        }),
    }
}

fn db_error(
    operation: &'static str,
    entity: impl Into<String>,
    source: sqlx::Error,
) -> PersonalSkillError {
    PersonalSkillError::Database {
        operation,
        entity: entity.into(),
        source,
    }
}

fn invalid_database_value(
    operation: &'static str,
    entity: &str,
    column: &str,
    message: impl Into<String>,
) -> PersonalSkillError {
    db_error(
        operation,
        entity,
        sqlx::Error::Decode(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "personal skill decode column `{column}`: {}",
                message.into()
            ),
        ))),
    )
}

fn row_string(
    row: &sqlx::mysql::MySqlRow,
    operation: &'static str,
    entity: &str,
    column: &'static str,
) -> Result<String, PersonalSkillError> {
    let value = row
        .try_get::<String, _>(column)
        .map_err(|source| db_error(operation, entity, source))?;
    if value.trim().is_empty() {
        return Err(invalid_database_value(
            operation,
            entity,
            column,
            "must not be empty",
        ));
    }
    Ok(value)
}

fn row_non_negative_i64(
    row: &sqlx::mysql::MySqlRow,
    operation: &'static str,
    entity: &str,
    column: &'static str,
) -> Result<i64, PersonalSkillError> {
    let value = row
        .try_get::<i64, _>(column)
        .map_err(|source| db_error(operation, entity, source))?;
    if value < 0 {
        return Err(invalid_database_value(
            operation,
            entity,
            column,
            format!("expected non-negative integer, got {value}"),
        ));
    }
    Ok(value)
}

fn source_from_row(
    row: sqlx::mysql::MySqlRow,
    operation: &'static str,
    entity: &str,
) -> Result<UserSkillSourceRecord, PersonalSkillError> {
    Ok(UserSkillSourceRecord {
        source_id: row_string(&row, operation, entity, "source_id")?,
        owner_user_id: row_string(&row, operation, entity, "owner_user_id")?,
        skill_name: row_string(&row, operation, entity, "skill_name")?,
        visibility: row_string(&row, operation, entity, "visibility")?,
        status: row_string(&row, operation, entity, "status")?,
        created_at: row_string(&row, operation, entity, "created_at")?,
        updated_at: row_string(&row, operation, entity, "updated_at")?,
    })
}

fn version_from_row(
    row: sqlx::mysql::MySqlRow,
    operation: &'static str,
    entity: &str,
) -> Result<UserSkillVersionRecord, PersonalSkillError> {
    let version_id = row_string(&row, operation, entity, "version_id")?;
    let manifest_raw = row_string(&row, operation, entity, "manifest_json")?;
    let manifest_json =
        serde_json::from_str(&manifest_raw).map_err(|source| PersonalSkillError::Json {
            operation: "deserialize_skill_manifest",
            entity: version_id.clone(),
            source,
        })?;
    Ok(UserSkillVersionRecord {
        version_id,
        source_id: row_string(&row, operation, entity, "source_id")?,
        owner_user_id: row_string(&row, operation, entity, "owner_user_id")?,
        skill_name: row_string(&row, operation, entity, "skill_name")?,
        version: row_string(&row, operation, entity, "version")?,
        manifest_json,
        content_markdown: row_string(&row, operation, entity, "content_markdown")?,
        content_hash: row_string(&row, operation, entity, "content_hash")?,
        normalize_version: row_string(&row, operation, entity, "normalize_version")?,
        token_estimate: row_non_negative_i64(&row, operation, entity, "token_estimate")? as u32,
        status: row_string(&row, operation, entity, "status")?,
        created_at: row_string(&row, operation, entity, "created_at")?,
        updated_at: row_string(&row, operation, entity, "updated_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_status_validator_accepts_only_lifecycle_states() {
        for status in ["draft", "published", "superseded", "quarantined"] {
            validate_version_status(status).expect("valid version status");
        }

        let error = validate_version_status("archived").expect_err("unknown status");
        assert!(matches!(
            error,
            PersonalSkillError::InvalidStatus { status } if status == "archived"
        ));
    }
}
