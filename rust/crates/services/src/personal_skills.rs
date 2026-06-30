use std::cmp::max;

use astra_core::SharedPool;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::Row;
use thiserror::Error;
use uuid::Uuid;

use crate::state_projection::{DatabaseStateProjectionStore, StateProjectionError};

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
    #[error("skill version is quarantined: version_id={version_id}")]
    VersionQuarantined { version_id: String },
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserSkillEvaluationRecord {
    pub evaluation_id: String,
    pub owner_user_id: String,
    pub source_id: String,
    pub version_id: String,
    pub run_id: Option<String>,
    pub hits: u64,
    pub suspects: u64,
    pub false_positives: u64,
    pub payload_json: Value,
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateUserSkillSource {
    pub skill_name: String,
    pub visibility: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubmitUserSkillVersion {
    pub version: String,
    pub manifest_json: Value,
    pub content_markdown: String,
    pub status: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActivateUserSkillVersion {
    pub session_id: String,
    pub version_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstallUserSkill {
    pub version_id: Option<String>,
    pub scope: Option<String>,
    pub session_id: Option<String>,
    pub workspace_id: Option<String>,
    pub auto_activate_on_topic_match: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordUserSkillEvaluation {
    pub source_id: String,
    pub version_id: String,
    pub run_id: Option<String>,
    pub hits: u64,
    pub suspects: u64,
    pub false_positives: u64,
    pub payload_json: Option<Value>,
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
        if self
            .load_source_optional(owner_user_id, &request.skill_name)
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
            .execute(self.pool.get())
            .await
            .map_err(|source| PersonalSkillError::Database {
                operation: "update_user_skill_source",
                entity: request.skill_name.clone(),
                source,
            })?;
            return self.load_source(owner_user_id, &request.skill_name).await;
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
        .execute(self.pool.get())
        .await
        .map_err(|source| PersonalSkillError::Database {
            operation: "create_user_skill_source",
            entity: request.skill_name.clone(),
            source,
        })?;
        self.load_source(owner_user_id, &request.skill_name).await
    }

    pub async fn submit_version(
        &self,
        owner_user_id: &str,
        skill_name: &str,
        request: SubmitUserSkillVersion,
    ) -> Result<UserSkillVersionRecord, PersonalSkillError> {
        let source = match self.load_source_optional(owner_user_id, skill_name).await? {
            Some(source) => source,
            None => {
                self.create_source(
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
        .execute(self.pool.get())
        .await
        .map_err(|source| PersonalSkillError::Database {
            operation: "submit_user_skill_version",
            entity: format!("{skill_name}@{}", request.version),
            source,
        })?;
        self.load_version_by_id(owner_user_id, skill_name, &version_id)
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

    pub async fn activate_version(
        &self,
        owner_user_id: &str,
        session_id: &str,
        skill_name: &str,
        version_id: &str,
    ) -> Result<UserSkillVersionRecord, PersonalSkillError> {
        let version = self
            .load_version_by_id(owner_user_id, skill_name, version_id)
            .await?
            .ok_or_else(|| PersonalSkillError::VersionNotFound {
                owner_user_id: owner_user_id.to_string(),
                skill_name: skill_name.to_string(),
                version_id: version_id.to_string(),
            })?;
        if version.status == "quarantined" {
            return Err(PersonalSkillError::VersionQuarantined {
                version_id: version_id.to_string(),
            });
        }
        DatabaseStateProjectionStore::new(self.pool.clone())
            .activate_personal_skill_from_ui(owner_user_id, session_id, skill_name, version_id)
            .await
            .map_err(|source| PersonalSkillError::StateProjection {
                operation: "activate_user_skill_version",
                entity: version_id.to_string(),
                source: Box::new(source),
            })?;
        Ok(version)
    }

    pub async fn install_skill(
        &self,
        owner_user_id: &str,
        skill_name: &str,
        request: InstallUserSkill,
    ) -> Result<(), PersonalSkillError> {
        let version = if let Some(version_id) = request.version_id.as_deref() {
            self.load_version_by_id(owner_user_id, skill_name, version_id)
                .await?
                .ok_or_else(|| PersonalSkillError::VersionNotFound {
                    owner_user_id: owner_user_id.to_string(),
                    skill_name: skill_name.to_string(),
                    version_id: version_id.to_string(),
                })?
                .version
        } else {
            self.latest_published_version(owner_user_id, skill_name)
                .await?
                .unwrap_or_else(|| "draft".to_string())
        };
        let installation_id = Uuid::new_v4().to_string();
        let scope = request.scope.unwrap_or_else(|| "user".to_string());
        let auto_activate = if request.auto_activate_on_topic_match.unwrap_or(false) {
            1_i64
        } else {
            0_i64
        };
        let existing = sqlx::query(
            "SELECT installation_id FROM skill_installations
             WHERE user_id = ? AND skill_name = ? LIMIT 1",
        )
        .bind(owner_user_id)
        .bind(skill_name)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| PersonalSkillError::Database {
            operation: "load_existing_skill_installation",
            entity: skill_name.to_string(),
            source,
        })?;
        if let Some(row) = existing {
            let existing_id = row_string(
                &row,
                "load_existing_skill_installation",
                skill_name,
                "installation_id",
            )?;
            sqlx::query(
                "UPDATE skill_installations
                 SET skill_version = ?, status = 'installed', scope = ?, session_id = ?,
                     workspace_id = ?, auto_activate_on_topic_match = ?, updated_at = NOW(6)
                 WHERE installation_id = ?",
            )
            .bind(&version)
            .bind(&scope)
            .bind(&request.session_id)
            .bind(&request.workspace_id)
            .bind(auto_activate)
            .bind(&existing_id)
            .execute(self.pool.get())
            .await
            .map_err(|source| PersonalSkillError::Database {
                operation: "update_user_skill_installation",
                entity: skill_name.to_string(),
                source,
            })?;
        } else {
            sqlx::query(
                "INSERT INTO skill_installations
                 (installation_id, user_id, skill_name, skill_version, status, scope, session_id,
                  workspace_id, auto_activate_on_topic_match, installed_at, updated_at)
                 VALUES (?, ?, ?, ?, 'installed', ?, ?, ?, ?, NOW(6), NOW(6))",
            )
            .bind(&installation_id)
            .bind(owner_user_id)
            .bind(skill_name)
            .bind(&version)
            .bind(&scope)
            .bind(&request.session_id)
            .bind(&request.workspace_id)
            .bind(auto_activate)
            .execute(self.pool.get())
            .await
            .map_err(|source| PersonalSkillError::Database {
                operation: "insert_user_skill_installation",
                entity: skill_name.to_string(),
                source,
            })?;
        }
        Ok(())
    }

    pub async fn record_evaluation(
        &self,
        owner_user_id: &str,
        skill_name: &str,
        request: RecordUserSkillEvaluation,
    ) -> Result<UserSkillEvaluationRecord, PersonalSkillError> {
        let evaluation_id = format!("skill-eval-{}", Uuid::new_v4());
        let payload = request.payload_json.unwrap_or(Value::Null);
        let payload_json =
            serde_json::to_string(&payload).map_err(|source| PersonalSkillError::Json {
                operation: "serialize_skill_evaluation",
                entity: request.version_id.clone(),
                source,
            })?;
        let result = sqlx::query(
            "INSERT INTO user_skill_evaluations
             (evaluation_id, owner_user_id, source_id, version_id, run_id, hits, suspects,
              false_positives, payload_json, created_at)
             SELECT ?, sources.owner_user_id, versions.source_id, versions.version_id,
                    ?, ?, ?, ?, ?, NOW(6)
             FROM user_skill_sources sources
             JOIN user_skill_versions versions
               ON versions.source_id = sources.source_id
             WHERE sources.owner_user_id = ?
               AND sources.skill_name = ?
               AND sources.source_id = ?
               AND versions.version_id = ?
             LIMIT 1",
        )
        .bind(&evaluation_id)
        .bind(&request.run_id)
        .bind(request.hits as i64)
        .bind(request.suspects as i64)
        .bind(request.false_positives as i64)
        .bind(&payload_json)
        .bind(owner_user_id)
        .bind(skill_name)
        .bind(&request.source_id)
        .bind(&request.version_id)
        .execute(self.pool.get())
        .await
        .map_err(|source| PersonalSkillError::Database {
            operation: "record_user_skill_evaluation",
            entity: request.version_id.clone(),
            source,
        })?;
        if result.rows_affected() == 0 {
            return Err(PersonalSkillError::VersionNotFound {
                owner_user_id: owner_user_id.to_string(),
                skill_name: skill_name.to_string(),
                version_id: request.version_id,
            });
        }
        let row = sqlx::query(
            "SELECT evaluation_id, owner_user_id, source_id, version_id, run_id, hits, suspects,
                    false_positives, payload_json, CAST(created_at AS CHAR) AS created_at
             FROM user_skill_evaluations WHERE owner_user_id = ? AND evaluation_id = ?",
        )
        .bind(owner_user_id)
        .bind(&evaluation_id)
        .fetch_one(self.pool.get())
        .await
        .map_err(|source| PersonalSkillError::Database {
            operation: "load_user_skill_evaluation",
            entity: evaluation_id.clone(),
            source,
        })?;
        evaluation_from_row(row, "load_user_skill_evaluation", &evaluation_id)
    }

    pub async fn auto_activate_candidates(
        &self,
        owner_user_id: &str,
    ) -> Result<Vec<String>, PersonalSkillError> {
        let rows = sqlx::query(
            "SELECT skill_name FROM skill_installations FORCE INDEX (idx_si_auto_activate)
             WHERE user_id = ? AND auto_activate_on_topic_match = 1 AND status = 'installed'
             ORDER BY updated_at DESC LIMIT 32",
        )
        .bind(owner_user_id)
        .fetch_all(self.pool.get())
        .await
        .map_err(|source| PersonalSkillError::Database {
            operation: "load_auto_activate_skill_candidates",
            entity: owner_user_id.to_string(),
            source,
        })?;
        rows.into_iter()
            .map(|row| {
                row_string(
                    &row,
                    "load_auto_activate_skill_candidates",
                    owner_user_id,
                    "skill_name",
                )
            })
            .collect()
    }

    async fn load_source(
        &self,
        owner_user_id: &str,
        skill_name: &str,
    ) -> Result<UserSkillSourceRecord, PersonalSkillError> {
        self.load_source_optional(owner_user_id, skill_name)
            .await?
            .ok_or_else(|| PersonalSkillError::Database {
                operation: "load_user_skill_source",
                entity: skill_name.to_string(),
                source: sqlx::Error::RowNotFound,
            })
    }

    async fn load_source_optional(
        &self,
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
        .fetch_optional(self.pool.get())
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
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| PersonalSkillError::Database {
            operation: "load_user_skill_version",
            entity: version_id.to_string(),
            source,
        })?;
        row.map(|row| version_from_row(row, "load_user_skill_version", version_id))
            .transpose()
    }

    async fn latest_published_version(
        &self,
        owner_user_id: &str,
        skill_name: &str,
    ) -> Result<Option<String>, PersonalSkillError> {
        let row = sqlx::query(
            "SELECT version FROM user_skill_versions FORCE INDEX (idx_user_skill_versions_owner_name)
             WHERE owner_user_id = ? AND skill_name = ? AND status = 'published'
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(owner_user_id)
        .bind(skill_name)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| PersonalSkillError::Database {
            operation: "load_latest_published_user_skill_version",
            entity: skill_name.to_string(),
            source,
        })?;
        row.map(|row| {
            row_string(
                &row,
                "load_latest_published_user_skill_version",
                skill_name,
                "version",
            )
        })
        .transpose()
    }
}

pub fn normalize_skill_md(manifest_json: &Value, content_markdown: &str) -> String {
    let mut canonical = String::new();
    canonical.push_str(&canonical_json(manifest_json));
    canonical.push('\n');
    canonical.push_str(&normalize_markdown(content_markdown));
    canonical
}

pub fn skill_md_content_hash(manifest_json: &Value, content_markdown: &str) -> String {
    sha256_prefixed(&normalize_skill_md(manifest_json, content_markdown))
}

fn canonical_json(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(v) => v.to_string(),
        Value::Number(v) => v.to_string(),
        Value::String(v) => serde_json::to_string(v).unwrap_or_else(|_| "\"\"".to_string()),
        Value::Array(values) => {
            let parts = values.iter().map(canonical_json).collect::<Vec<_>>();
            format!("[{}]", parts.join(","))
        }
        Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            let parts = keys
                .into_iter()
                .map(|key| {
                    let key_json =
                        serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string());
                    format!("{key_json}:{}", canonical_json(&map[key]))
                })
                .collect::<Vec<_>>();
            format!("{{{}}}", parts.join(","))
        }
    }
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

fn row_optional_string(
    row: &sqlx::mysql::MySqlRow,
    operation: &'static str,
    entity: &str,
    column: &'static str,
) -> Result<Option<String>, PersonalSkillError> {
    row.try_get::<Option<String>, _>(column)
        .map_err(|source| db_error(operation, entity, source))
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

fn evaluation_from_row(
    row: sqlx::mysql::MySqlRow,
    operation: &'static str,
    entity: &str,
) -> Result<UserSkillEvaluationRecord, PersonalSkillError> {
    let evaluation_id = row_string(&row, operation, entity, "evaluation_id")?;
    let payload_raw = row_string(&row, operation, entity, "payload_json")?;
    let payload_json =
        serde_json::from_str(&payload_raw).map_err(|source| PersonalSkillError::Json {
            operation: "deserialize_skill_evaluation",
            entity: evaluation_id.clone(),
            source,
        })?;
    Ok(UserSkillEvaluationRecord {
        evaluation_id,
        owner_user_id: row_string(&row, operation, entity, "owner_user_id")?,
        source_id: row_string(&row, operation, entity, "source_id")?,
        version_id: row_string(&row, operation, entity, "version_id")?,
        run_id: row_optional_string(&row, operation, entity, "run_id")?,
        hits: row_non_negative_i64(&row, operation, entity, "hits")? as u64,
        suspects: row_non_negative_i64(&row, operation, entity, "suspects")? as u64,
        false_positives: row_non_negative_i64(&row, operation, entity, "false_positives")? as u64,
        payload_json,
        created_at: row_string(&row, operation, entity, "created_at")?,
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
