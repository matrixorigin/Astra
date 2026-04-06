use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{Row, query};

use astra_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};
use sha2::Digest;

fn is_duplicate_key_error(err: &sqlx::Error) -> bool {
    match err {
        sqlx::Error::Database(db_err) => db_err.code().as_deref() == Some("1062"),
        _ => false,
    }
}

// ── Data types ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct SkillRegisterRequestData {
    pub skill_id: String,
    pub skill_name: String,
    pub skill_version: String,
    pub skill_code: String,
    pub description: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SkillPublishRequestData {
    pub name: String,
    pub version: String,
    pub description: String,
    pub triggers: Option<Vec<String>>,
    pub dependencies: Option<Vec<String>>,
    pub manifest: Option<serde_json::Value>,
    pub category: String,
    pub priority: i32,
    // Phase 3: publisher + trust fields
    pub publisher_id: Option<String>,
    pub trust_tier: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillRecord {
    pub skill_id: String,
    pub skill_name: String,
    pub version: String,
    pub description: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub created_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillListRecord {
    pub skills: Vec<SkillListItem>,
    pub total: i64,
    pub limit: u32,
    pub offset: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillListItem {
    pub skill_id: String,
    pub skill_name: String,
    pub version: String,
    pub description: Option<String>,
    pub status: Option<String>,
    pub source: Option<String>,
    pub category: Option<String>,
    pub created_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillVersionRecord {
    pub version: String,
    pub status: Option<String>,
    pub is_active: Option<i16>,
    pub created_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillInfoRecord {
    pub skill_name: String,
    pub version: String,
    pub description: Option<String>,
    pub source: Option<String>,
    pub status: Option<String>,
    pub created_by: Option<String>,
    pub category: Option<String>,
    pub install_count: i64,
    pub created_at: Option<String>,
    // Phase 3
    pub publisher_id: Option<String>,
    pub trust_tier: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillStatusRecord {
    pub builtin: Vec<SkillListItem>,
    pub marketplace: Vec<SkillListItem>,
    pub user: Vec<SkillListItem>,
    pub platform_total: i64,
    pub user_total: i64,
}

const MAX_SKILL_LIST_ROWS: u32 = 200;
const MAX_SKILL_STATUS_PER_GROUP: u32 = 100;

// ── Trait ─────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait SkillService: Send + Sync {
    async fn register_skill(
        &self,
        user_id: String,
        request: SkillRegisterRequestData,
    ) -> Result<SkillRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn list_skills(
        &self,
        limit: u32,
        offset: u32,
    ) -> Result<SkillListRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn get_skill(
        &self,
        skill_id: String,
        version: Option<String>,
    ) -> Result<SkillRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn get_skill_info(
        &self,
        skill_name: String,
        user_id: String,
    ) -> Result<SkillInfoRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn list_skill_versions(
        &self,
        skill_name: String,
    ) -> Result<Vec<SkillVersionRecord>, (StatusCode, Json<ErrorResponse>)>;

    async fn get_skill_status(
        &self,
        user_id: String,
        per_group: u32,
    ) -> Result<SkillStatusRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn publish_skill(
        &self,
        user_id: String,
        request: SkillPublishRequestData,
    ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)>;

    async fn unpublish_skill(
        &self,
        user_id: String,
        skill_name: String,
    ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)>;
}

// ── Database implementation ──────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct DatabaseSkillService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseSkillService {
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
        if let Some(ref p) = self.pool {
            return Ok(p.get().clone());
        }
        connect_matrixone(&self.matrixone).await
    }
}

#[async_trait]
impl SkillService for DatabaseSkillService {
    async fn register_skill(
        &self,
        user_id: String,
        request: SkillRegisterRequestData,
    ) -> Result<SkillRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let skill_id = if request.skill_id.is_empty() {
            format!("{}@{}", request.skill_name, request.skill_version)
        } else {
            request.skill_id.clone()
        };

        let existing = query("SELECT skill_id FROM skills_registry WHERE skill_id = ?")
            .bind(&skill_id)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?;
        if existing.is_some() {
            return Err(error_response(
                StatusCode::CONFLICT,
                format!("Skill '{}' already exists", skill_id),
            ));
        }

        let metadata_json = request
            .metadata
            .as_ref()
            .map(|m| serde_json::to_string(m).unwrap_or_else(|_| "{}".into()));
        let code_hash = format!("{:x}", sha2::Sha256::digest(request.skill_code.as_bytes()));

        query(
            "INSERT INTO skills_registry \
             (skill_id, skill_name, version, description, skill_definition, code_hash, \
              is_active, status, source, created_by, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, 1, 'active', 'user', ?, NOW(), NOW())",
        )
        .bind(&skill_id)
        .bind(&request.skill_name)
        .bind(&request.skill_version)
        .bind(&request.description)
        .bind(&metadata_json)
        .bind(&code_hash)
        .bind(&user_id)
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        Ok(SkillRecord {
            skill_id,
            skill_name: request.skill_name,
            version: request.skill_version,
            description: request.description,
            metadata: request.metadata,
            created_at: Some(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string()),
        })
    }

    async fn list_skills(
        &self,
        limit: u32,
        offset: u32,
    ) -> Result<SkillListRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let limit = limit.min(MAX_SKILL_LIST_ROWS);

        let count_row = query("SELECT COUNT(*) AS cnt FROM skills_registry WHERE is_active = 1")
            .fetch_one(&pool)
            .await
            .map_err(internal_error)?;
        let total: i64 = count_row.try_get("cnt").unwrap_or(0);

        let rows = query(
            "SELECT skill_id, skill_name, version, description, \
             IFNULL(CAST(skill_definition AS CHAR), '{}') AS definition_json, \
             status, source, category, \
             DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM skills_registry WHERE is_active = 1 \
             ORDER BY created_at DESC LIMIT ? OFFSET ?",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let skills: Vec<SkillListItem> = rows
            .iter()
            .map(|row| SkillListItem {
                skill_id: row.try_get::<String, _>("skill_id").unwrap_or_default(),
                skill_name: row.try_get::<String, _>("skill_name").unwrap_or_default(),
                version: row.try_get::<String, _>("version").unwrap_or_default(),
                description: row.try_get::<String, _>("description").ok(),
                status: row.try_get::<String, _>("status").ok(),
                source: row.try_get::<String, _>("source").ok(),
                category: row.try_get::<String, _>("category").ok(),
                created_at: row.try_get::<String, _>("created_at").ok(),
            })
            .collect();

        Ok(SkillListRecord {
            skills,
            total,
            limit,
            offset,
        })
    }

    async fn get_skill(
        &self,
        skill_id: String,
        _version: Option<String>,
    ) -> Result<SkillRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let row = query(
            "SELECT skill_id, skill_name, version, description, \
             IFNULL(CAST(skill_definition AS CHAR), 'null') AS definition_json, \
             DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM skills_registry WHERE skill_id = ?",
        )
        .bind(&skill_id)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?;

        let row = if row.is_none() {
            let name = skill_id.split('@').next().unwrap_or(&skill_id);
            query(
                "SELECT skill_id, skill_name, version, description, \
                 IFNULL(CAST(skill_definition AS CHAR), 'null') AS definition_json, \
                 DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
                 FROM skills_registry WHERE skill_name = ? AND is_active = 1 \
                 ORDER BY created_at DESC LIMIT 1",
            )
            .bind(name)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?
        } else {
            row
        };

        let row = row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Skill '{}' not found", skill_id),
            )
        })?;
        let def_json: String = row
            .try_get("definition_json")
            .unwrap_or_else(|_| "null".into());

        Ok(SkillRecord {
            skill_id: row.try_get("skill_id").map_err(internal_error)?,
            skill_name: row.try_get("skill_name").map_err(internal_error)?,
            version: row.try_get("version").map_err(internal_error)?,
            description: row.try_get("description").ok(),
            metadata: serde_json::from_str(&def_json).ok(),
            created_at: row.try_get("created_at").ok(),
        })
    }

    async fn get_skill_info(
        &self,
        skill_name: String,
        _user_id: String,
    ) -> Result<SkillInfoRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let row = query(
            "SELECT skill_name, version, description, source, status, created_by, category, \
             publisher_id, trust_tier, \
             DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM skills_registry WHERE skill_name = ? AND is_active = 1 \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(&skill_name)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?;
        let row = row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Skill '{}' not found", skill_name),
            )
        })?;

        let install_count: i64 = query(
            "SELECT COUNT(*) AS cnt FROM skill_installations WHERE skill_name = ? AND status = 'installed'"
        )
        .bind(&skill_name)
        .fetch_one(&pool)
        .await
        .map_or(0, |r| r.try_get("cnt").unwrap_or(0));

        Ok(SkillInfoRecord {
            skill_name: row.try_get("skill_name").map_err(internal_error)?,
            version: row.try_get("version").map_err(internal_error)?,
            description: row.try_get("description").ok(),
            source: row.try_get("source").ok(),
            status: row.try_get("status").ok(),
            created_by: row.try_get("created_by").ok(),
            category: row.try_get("category").ok(),
            install_count,
            created_at: row.try_get("created_at").ok(),
            publisher_id: row.try_get("publisher_id").ok(),
            trust_tier: row.try_get("trust_tier").ok(),
        })
    }

    async fn list_skill_versions(
        &self,
        skill_name: String,
    ) -> Result<Vec<SkillVersionRecord>, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let rows = query(
            "SELECT version, status, is_active, \
             DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM skills_registry WHERE skill_name = ? ORDER BY created_at DESC",
        )
        .bind(&skill_name)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let mut versions = Vec::with_capacity(rows.len());
        for row in rows {
            versions.push(SkillVersionRecord {
                version: row.try_get("version").map_err(internal_error)?,
                status: row.try_get("status").ok(),
                is_active: row.try_get("is_active").ok(),
                created_at: row.try_get("created_at").ok(),
            });
        }
        Ok(versions)
    }

    async fn get_skill_status(
        &self,
        _user_id: String,
        per_group: u32,
    ) -> Result<SkillStatusRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let per_group = per_group.min(MAX_SKILL_STATUS_PER_GROUP);

        let fetch_group = |source: &str| {
            let pool = pool.clone();
            let source = source.to_string();
            async move {
                let rows = query(
                    "SELECT skill_id, skill_name, version, description, status, category, \
                     DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
                     FROM skills_registry WHERE source = ? AND is_active = 1 \
                     ORDER BY skill_name LIMIT ?",
                )
                .bind(&source)
                .bind(per_group)
                .fetch_all(&pool)
                .await
                .unwrap_or_default();

                rows.iter()
                    .map(|row| SkillListItem {
                        skill_id: row.try_get::<String, _>("skill_id").unwrap_or_default(),
                        skill_name: row.try_get::<String, _>("skill_name").unwrap_or_default(),
                        version: row.try_get::<String, _>("version").unwrap_or_default(),
                        description: row.try_get::<String, _>("description").ok(),
                        status: row.try_get::<String, _>("status").ok(),
                        source: Some(source.clone()),
                        category: row.try_get::<String, _>("category").ok(),
                        created_at: row.try_get::<String, _>("created_at").ok(),
                    })
                    .collect::<Vec<_>>()
            }
        };

        let (builtin, marketplace, user) = tokio::join!(
            fetch_group("builtin"),
            fetch_group("marketplace"),
            fetch_group("user"),
        );

        let platform_total = (builtin.len() + marketplace.len()) as i64;
        let user_total = user.len() as i64;

        Ok(SkillStatusRecord {
            builtin,
            marketplace,
            user,
            platform_total,
            user_total,
        })
    }

    async fn publish_skill(
        &self,
        user_id: String,
        request: SkillPublishRequestData,
    ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let existing =
            query("SELECT skill_id FROM skills_registry WHERE skill_name = ? AND source != 'user'")
                .bind(&request.name)
                .fetch_optional(&pool)
                .await
                .map_err(internal_error)?;
        if existing.is_some() {
            return Err(error_response(
                StatusCode::CONFLICT,
                format!(
                    "Skill name '{}' conflicts with a builtin/marketplace skill",
                    request.name
                ),
            ));
        }

        let skill_id = format!("{}@{}", request.name, request.version);
        let triggers_json = request
            .triggers
            .as_ref()
            .map(|t| serde_json::to_string(t).unwrap_or_else(|_| "[]".into()));
        let deps_json = request
            .dependencies
            .as_ref()
            .map(|d| serde_json::to_string(d).unwrap_or_else(|_| "[]".into()));
        let manifest_json = request
            .manifest
            .as_ref()
            .map(|m| serde_json::to_string(m).unwrap_or_else(|_| "{}".into()));

        let insert_result = query(
            "INSERT INTO skills_registry \
             (skill_id, skill_name, version, description, triggers, dependencies, manifest, \
               category, priority, is_active, status, source, is_public, created_by, \
               publisher_id, trust_tier, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 1, 'active', 'user', 1, ?, ?, ?, NOW(), NOW())",
        )
        .bind(&skill_id)
        .bind(&request.name)
        .bind(&request.version)
        .bind(&request.description)
        .bind(&triggers_json)
        .bind(&deps_json)
        .bind(&manifest_json)
        .bind(&request.category)
        .bind(request.priority)
        .bind(&user_id)
        .bind(&request.publisher_id)
        .bind(&request.trust_tier)
        .execute(&pool)
        .await;

        if let Err(error) = insert_result {
            if is_duplicate_key_error(&error) {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    format!("Skill '{}' already exists", skill_id),
                ));
            }
            return Err(internal_error(error));
        }

        Ok(serde_json::json!({
            "skill_id": skill_id,
            "skill_name": request.name,
            "version": request.version,
            "status": "published",
        }))
    }

    async fn unpublish_skill(
        &self,
        user_id: String,
        skill_name: String,
    ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let row = query(
            "SELECT skill_id, created_by FROM skills_registry WHERE skill_name = ? AND source = 'user' AND is_active = 1"
        )
        .bind(&skill_name)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?;
        let row = row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Skill '{}' not found", skill_name),
            )
        })?;

        let created_by: String = row.try_get("created_by").unwrap_or_default();
        if created_by != user_id {
            return Err(error_response(
                StatusCode::FORBIDDEN,
                "Not authorized to unpublish this skill",
            ));
        }

        query(
            "UPDATE skills_registry SET is_active = 0, status = 'unpublished', updated_at = NOW() \
             WHERE skill_name = ? AND source = 'user'",
        )
        .bind(&skill_name)
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        Ok(serde_json::json!({"skill_name": skill_name, "result": "unpublished"}))
    }
}

// ── Noop implementation ──────────────────────────────────────────────────────

pub struct UnconfiguredSkillService;

#[async_trait]
impl SkillService for UnconfiguredSkillService {
    async fn register_skill(
        &self,
        _: String,
        _: SkillRegisterRequestData,
    ) -> Result<SkillRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("skill service not configured"))
    }
    async fn list_skills(
        &self,
        _: u32,
        _: u32,
    ) -> Result<SkillListRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("skill service not configured"))
    }
    async fn get_skill(
        &self,
        _: String,
        _: Option<String>,
    ) -> Result<SkillRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("skill service not configured"))
    }
    async fn get_skill_info(
        &self,
        _: String,
        _: String,
    ) -> Result<SkillInfoRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("skill service not configured"))
    }
    async fn list_skill_versions(
        &self,
        _: String,
    ) -> Result<Vec<SkillVersionRecord>, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("skill service not configured"))
    }
    async fn get_skill_status(
        &self,
        _: String,
        _: u32,
    ) -> Result<SkillStatusRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("skill service not configured"))
    }
    async fn publish_skill(
        &self,
        _: String,
        _: SkillPublishRequestData,
    ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("skill service not configured"))
    }
    async fn unpublish_skill(
        &self,
        _: String,
        _: String,
    ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("skill service not configured"))
    }
}

// ── HTTP types ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RegisterSkillRequest {
    pub skill_id: String,
    pub skill_name: String,
    pub skill_version: String,
    pub skill_code: String,
    pub description: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub struct PublishSkillRequest {
    pub name: String,
    pub version: String,
    pub description: String,
    pub triggers: Option<Vec<String>>,
    pub dependencies: Option<Vec<String>>,
    pub manifest: Option<serde_json::Value>,
    #[serde(default = "default_category")]
    pub category: String,
    #[serde(default = "default_priority")]
    pub priority: i32,
}

fn default_category() -> String {
    "user".to_string()
}
fn default_priority() -> i32 {
    5
}

#[derive(Deserialize)]
pub struct SkillListQuery {
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
}

fn default_limit() -> u32 {
    50
}

#[derive(Deserialize)]
pub struct SkillGetQuery {
    pub version: Option<String>,
}

#[derive(Deserialize)]
pub struct SkillStatusQuery {
    #[serde(default = "default_per_group")]
    pub per_group: u32,
}

fn default_per_group() -> u32 {
    50
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_count_query_uses_status_not_is_active() {
        let sql = "SELECT COUNT(*) AS cnt FROM skill_installations WHERE skill_name = ? AND status = 'installed'";
        assert!(
            !sql.contains("is_active"),
            "skill_installations has no is_active column; use status = 'installed'"
        );
        assert!(sql.contains("status = 'installed'"));
    }

    #[test]
    fn publish_request_defaults() {
        let json = r#"{"name":"test","version":"1.0","description":"desc"}"#;
        let req: PublishSkillRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.category, "user");
        assert_eq!(req.priority, 5);
        assert!(req.triggers.is_none());
        assert!(req.manifest.is_none());
    }

    #[test]
    fn skill_list_query_defaults() {
        let q: SkillListQuery = serde_json::from_str("{}").unwrap();
        assert_eq!(q.limit, 50);
        assert_eq!(q.offset, 0);
    }

    #[test]
    fn skill_status_query_default() {
        let q: SkillStatusQuery = serde_json::from_str("{}").unwrap();
        assert_eq!(q.per_group, 50);
    }

    #[test]
    fn skill_get_query_no_version() {
        let q: SkillGetQuery = serde_json::from_str("{}").unwrap();
        assert!(q.version.is_none());
    }
}
