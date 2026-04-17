use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{Row, query};

use astra_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, connect_matrixone, error_response,
    internal_error, is_duplicate_key_error,
};

use crate::pagination::clamp_api_list_pagination;
use sha2::Digest;

// ── Data types ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct SkillRegisterRequestData {
    pub skill_id: String,
    pub skill_name: String,
    pub skill_version: String,
    pub skill_code: String,
    pub skill_type: String,
    pub remote_url: Option<String>,
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
    pub skill_type: String,
    pub remote_url: Option<String>,
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

const MAX_SKILL_STATUS_PER_GROUP: u32 = 100;
const SKILL_TYPE_LOCAL: &str = "local";
const SKILL_TYPE_REMOTE: &str = "remote";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegisteredSkillType {
    Local,
    Remote,
}

impl RegisteredSkillType {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | SKILL_TYPE_LOCAL => Some(Self::Local),
            SKILL_TYPE_REMOTE => Some(Self::Remote),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Local => SKILL_TYPE_LOCAL,
            Self::Remote => SKILL_TYPE_REMOTE,
        }
    }
}

fn validate_remote_url(raw_url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(raw_url)
        .map_err(|err| format!("invalid remote_url '{raw_url}': {err}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "remote_url must use http:// or https:// (found '{other}')"
            ));
        }
    }
    if parsed.host_str().is_none() {
        return Err("remote_url must include a valid host".to_string());
    }
    Ok(())
}

fn strip_reserved_skill_definition_keys(map: &mut serde_json::Map<String, serde_json::Value>) {
    // `skill_type` / `remote_url` are controlled by dedicated API fields.
    map.remove("skill_type");
    map.remove("remote_url");
}

/// `list_skills` row projection — excludes `skill_definition` (large JSON); use `get_skill` for body.
const SKILL_REGISTRY_LIST_SELECT: &str = "\
    skill_id, skill_name, version, description, \
    status, source, category, \
    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at";

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

        let skill_type = RegisteredSkillType::parse(&request.skill_type).ok_or_else(|| {
            error_response(
                StatusCode::BAD_REQUEST,
                format!(
                    "invalid skill_type '{}'; expected '{}' or '{}'",
                    request.skill_type, SKILL_TYPE_LOCAL, SKILL_TYPE_REMOTE
                ),
            )
        })?;
        let remote_url = request
            .remote_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_string);
        match skill_type {
            RegisteredSkillType::Local => {
                if request.skill_code.trim().is_empty() {
                    return Err(error_response(
                        StatusCode::BAD_REQUEST,
                        "local skills require non-empty skill_code",
                    ));
                }
                if remote_url.is_some() {
                    return Err(error_response(
                        StatusCode::BAD_REQUEST,
                        "local skills must not provide remote_url",
                    ));
                }
            }
            RegisteredSkillType::Remote => {
                let Some(url) = remote_url.as_deref() else {
                    return Err(error_response(
                        StatusCode::BAD_REQUEST,
                        "remote skills require remote_url",
                    ));
                };
                if !request.skill_code.trim().is_empty() {
                    return Err(error_response(
                        StatusCode::BAD_REQUEST,
                        "remote skills must not provide skill_code",
                    ));
                }
                validate_remote_url(url)
                    .map_err(|msg| error_response(StatusCode::BAD_REQUEST, msg))?;
            }
        }

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

        let mut skill_definition = match request.metadata.clone() {
            Some(serde_json::Value::Object(mut map)) => {
                strip_reserved_skill_definition_keys(&mut map);
                map
            }
            Some(value) => {
                let mut map = serde_json::Map::new();
                map.insert("metadata".to_string(), value);
                map
            }
            None => serde_json::Map::new(),
        };
        skill_definition.insert(
            "skill_type".to_string(),
            serde_json::Value::String(skill_type.as_str().to_string()),
        );
        if !request.skill_code.trim().is_empty() {
            skill_definition.insert(
                "instructions".to_string(),
                serde_json::Value::String(request.skill_code.clone()),
            );
        }
        if let Some(url) = remote_url.clone() {
            skill_definition.insert("remote_url".to_string(), serde_json::Value::String(url));
        }
        let definition_value = serde_json::Value::Object(skill_definition);
        let definition_json =
            serde_json::to_string(&definition_value).unwrap_or_else(|_| "{}".to_string());
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
        .bind(&definition_json)
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
            metadata: Some(definition_value),
            created_at: Some(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string()),
        })
    }

    async fn list_skills(
        &self,
        limit: u32,
        offset: u32,
    ) -> Result<SkillListRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let (limit, offset) = clamp_api_list_pagination(limit, offset);

        let count_row = query("SELECT COUNT(*) AS cnt FROM skills_registry WHERE is_active = 1")
            .fetch_one(&pool)
            .await
            .map_err(internal_error)?;
        let total: i64 = count_row.try_get("cnt").unwrap_or(0);

        let list_sql = format!(
            "SELECT {SKILL_REGISTRY_LIST_SELECT} FROM skills_registry WHERE is_active = 1 \
             ORDER BY created_at DESC LIMIT ? OFFSET ?",
        );
        let rows = query(&list_sql)
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
        version: Option<String>,
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
            if let Some(version) = version.as_deref() {
                query(
                    "SELECT skill_id, skill_name, version, description, \
                     IFNULL(CAST(skill_definition AS CHAR), 'null') AS definition_json, \
                     DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
                     FROM skills_registry WHERE skill_name = ? AND version = ? AND is_active = 1 \
                     ORDER BY created_at DESC LIMIT 1",
                )
                .bind(name)
                .bind(version)
                .fetch_optional(&pool)
                .await
                .map_err(internal_error)?
            } else {
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
            }
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

        let skill_type = RegisteredSkillType::parse(&request.skill_type).ok_or_else(|| {
            error_response(
                StatusCode::BAD_REQUEST,
                format!(
                    "invalid skill_type '{}'; expected '{}' or '{}'",
                    request.skill_type, SKILL_TYPE_LOCAL, SKILL_TYPE_REMOTE
                ),
            )
        })?;
        let remote_url = request
            .remote_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_string);
        match skill_type {
            RegisteredSkillType::Local => {
                if remote_url.is_some() {
                    return Err(error_response(
                        StatusCode::BAD_REQUEST,
                        "local skills must not provide remote_url",
                    ));
                }
            }
            RegisteredSkillType::Remote => {
                let Some(url) = remote_url.as_deref() else {
                    return Err(error_response(
                        StatusCode::BAD_REQUEST,
                        "remote skills require remote_url",
                    ));
                };
                validate_remote_url(url)
                    .map_err(|msg| error_response(StatusCode::BAD_REQUEST, msg))?;
            }
        }

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
        let mut manifest_map = match request.manifest.clone() {
            Some(serde_json::Value::Object(mut map)) => {
                strip_reserved_skill_definition_keys(&mut map);
                map
            }
            Some(_) => {
                return Err(error_response(
                    StatusCode::BAD_REQUEST,
                    "manifest must be a JSON object",
                ));
            }
            None => serde_json::Map::new(),
        };
        manifest_map.insert(
            "skill_type".to_string(),
            serde_json::Value::String(skill_type.as_str().to_string()),
        );
        if let Some(url) = remote_url.clone() {
            manifest_map.insert("remote_url".to_string(), serde_json::Value::String(url));
        }
        if !request.category.trim().is_empty() {
            manifest_map.insert(
                "category".to_string(),
                serde_json::Value::String(request.category.clone()),
            );
        }
        manifest_map.insert("priority".to_string(), serde_json::json!(request.priority));
        if let Some(triggers) = request.triggers.clone() {
            manifest_map.insert("triggers".to_string(), serde_json::json!(triggers));
        }
        if let Some(dependencies) = request.dependencies.clone() {
            manifest_map.insert("dependencies".to_string(), serde_json::json!(dependencies));
        }
        let skill_definition_json =
            serde_json::to_string(&serde_json::Value::Object(manifest_map.clone()))
                .unwrap_or_else(|_| "{}".into());
        let manifest_json = serde_json::to_string(&serde_json::Value::Object(manifest_map))
            .unwrap_or_else(|_| "{}".into());

        let insert_result = query(
            "INSERT INTO skills_registry \
             (skill_id, skill_name, version, description, skill_definition, triggers, dependencies, manifest, \
               category, priority, is_active, status, source, is_public, created_by, \
               publisher_id, trust_tier, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, 'active', 'user', 1, ?, ?, ?, NOW(), NOW())",
        )
        .bind(&skill_id)
        .bind(&request.name)
        .bind(&request.version)
        .bind(&request.description)
        .bind(&skill_definition_json)
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
    #[serde(default)]
    pub skill_id: String,
    pub skill_name: String,
    pub skill_version: String,
    #[serde(default)]
    pub skill_code: String,
    #[serde(default = "default_skill_type")]
    pub skill_type: String,
    #[serde(default)]
    pub remote_url: Option<String>,
    pub description: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

fn default_skill_type() -> String {
    SKILL_TYPE_LOCAL.to_string()
}

#[derive(Deserialize)]
pub struct PublishSkillRequest {
    pub name: String,
    pub version: String,
    pub description: String,
    pub triggers: Option<Vec<String>>,
    pub dependencies: Option<Vec<String>>,
    pub manifest: Option<serde_json::Value>,
    #[serde(default = "default_skill_type")]
    pub skill_type: String,
    #[serde(default)]
    pub remote_url: Option<String>,
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
        assert_eq!(req.skill_type, "local");
        assert!(req.remote_url.is_none());
        assert!(req.triggers.is_none());
        assert!(req.manifest.is_none());
    }

    #[test]
    fn publish_request_remote_mode_fields() {
        let json = r#"{
            "name":"remote-test",
            "version":"1.0",
            "description":"desc",
            "skill_type":"remote",
            "remote_url":"https://example.com/skills/run"
        }"#;
        let req: PublishSkillRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.skill_type, "remote");
        assert_eq!(
            req.remote_url.as_deref(),
            Some("https://example.com/skills/run")
        );
    }

    #[test]
    fn register_request_defaults_local_mode() {
        let json = r#"{"skill_name":"s","skill_version":"1.0.0"}"#;
        let req: RegisterSkillRequest = serde_json::from_str(json).unwrap();
        assert!(req.skill_id.is_empty());
        assert!(req.skill_code.is_empty());
        assert_eq!(req.skill_type, "local");
        assert!(req.remote_url.is_none());
    }

    #[test]
    fn register_request_remote_mode_fields() {
        let json = r#"{
            "skill_name":"s",
            "skill_version":"1.0.0",
            "skill_type":"remote",
            "remote_url":"http://127.0.0.1:8080/skills/run"
        }"#;
        let req: RegisterSkillRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.skill_type, "remote");
        assert_eq!(
            req.remote_url.as_deref(),
            Some("http://127.0.0.1:8080/skills/run")
        );
    }

    #[test]
    fn registered_skill_type_parse() {
        assert_eq!(
            RegisteredSkillType::parse("local"),
            Some(RegisteredSkillType::Local)
        );
        assert_eq!(
            RegisteredSkillType::parse(""),
            Some(RegisteredSkillType::Local)
        );
        assert_eq!(
            RegisteredSkillType::parse("remote"),
            Some(RegisteredSkillType::Remote)
        );
        assert!(RegisteredSkillType::parse("invalid").is_none());
    }

    #[test]
    fn validate_remote_url_accepts_http_https_with_host() {
        assert!(validate_remote_url("https://example.com/skills/run").is_ok());
        assert!(validate_remote_url("http://127.0.0.1:8080/execute").is_ok());
    }

    #[test]
    fn validate_remote_url_rejects_invalid_or_unsupported_urls() {
        assert!(validate_remote_url("http://").is_err());
        assert!(validate_remote_url("https://").is_err());
        assert!(validate_remote_url("ftp://example.com/execute").is_err());
    }

    #[test]
    fn strip_reserved_skill_definition_keys_removes_remote_url_and_skill_type() {
        let mut map = serde_json::Map::new();
        map.insert(
            "remote_url".to_string(),
            serde_json::Value::String("https://evil.example/exec".to_string()),
        );
        map.insert(
            "skill_type".to_string(),
            serde_json::Value::String("remote".to_string()),
        );
        map.insert("display_name".to_string(), serde_json::json!("safe"));

        strip_reserved_skill_definition_keys(&mut map);

        assert!(!map.contains_key("remote_url"));
        assert!(!map.contains_key("skill_type"));
        assert_eq!(
            map.get("display_name"),
            Some(&serde_json::Value::String("safe".to_string()))
        );
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

    #[test]
    fn skill_registry_list_select_omits_definition_blob() {
        assert!(
            !SKILL_REGISTRY_LIST_SELECT.contains("skill_definition"),
            "list_skills must not read skill_definition (use get_skill for full record)"
        );
    }
}
