use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{Row, query};

use astra_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, error_response, internal_error,
    is_duplicate_key_error,
};

use crate::pagination::MAX_API_LIST_LIMIT;

// ── Data types ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct SkillPublishRequestData {
    pub name: String,
    pub version: String,
    pub description: String,
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
    pub next_cursor: Option<SkillListCursor>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillListCursor {
    pub skill_name: String,
    pub version: String,
    pub skill_id: String,
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
enum PublishedSkillType {
    Local,
    Remote,
}

impl PublishedSkillType {
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
const SKILL_LIST_ORDER_SQL: &str = " ORDER BY skill_name ASC, version ASC, skill_id ASC LIMIT ?";
const SKILL_LIST_CURSOR_SQL: &str = "\
    AND (skill_name > ? \
     OR (skill_name = ? AND version > ?) \
     OR (skill_name = ? AND version = ? AND skill_id > ?))";

/// Visibility rule for database-backed skills.
///
/// Astra has two skill worlds:
/// - CLI-local filesystem skills, which are discovered directly from local paths
///   and are intentionally not visible to the web runtime until imported.
/// - Database skills in `skills_registry`, which are the only skills shared by
///   web sessions and remote runtime execution.
///
/// A database skill is visible to a caller when it is active and either public
/// or owned by the authenticated user. Keep this rule centralized so HTTP
/// handlers, runtime skill resolution, and future admin surfaces cannot drift.
const VISIBLE_SKILL_PREDICATE: &str = "is_active = 1 AND (is_public = 1 OR created_by = ?)";

/// Prefer a user's own skill over a public skill with the same logical name.
///
/// The schema currently enforces `(skill_name, version)` uniqueness, but this
/// ordering is still important for "latest by name" lookups: users should not
/// lose their private skill because a newer public skill with the same name was
/// published later.
const USER_OWNED_SKILL_FIRST_ORDER: &str =
    "CASE WHEN created_by = ? THEN 0 ELSE 1 END, created_at DESC";

fn validate_skill_list_limit(limit: u32) -> u32 {
    limit.clamp(1, MAX_API_LIST_LIMIT)
}

fn skill_list_query_limit(limit: u32) -> i64 {
    i64::from(limit) + 1
}

fn validate_skill_list_cursor(
    cursor: &SkillListCursor,
) -> Result<SkillListCursor, (StatusCode, Json<ErrorResponse>)> {
    let skill_name = cursor.skill_name.trim();
    let version = cursor.version.trim();
    let skill_id = cursor.skill_id.trim();
    if skill_name.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid skill list cursor: skill_name is required",
        ));
    }
    if version.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid skill list cursor: version is required",
        ));
    }
    if skill_id.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid skill list cursor: skill_id is required",
        ));
    }
    Ok(SkillListCursor {
        skill_name: skill_name.to_string(),
        version: version.to_string(),
        skill_id: skill_id.to_string(),
    })
}

pub fn skill_list_cursor_from_item(
    item: &SkillListItem,
) -> Result<SkillListCursor, (StatusCode, Json<ErrorResponse>)> {
    validate_skill_list_cursor(&SkillListCursor {
        skill_name: item.skill_name.clone(),
        version: item.version.clone(),
        skill_id: item.skill_id.clone(),
    })
}

// ── Idempotency classifiers ───────────────────────────────────────────────────

/// Row fetched from `skills_registry` when a duplicate key fires during publish.
pub(crate) struct ExistingPublishRow {
    pub skill_name: String,
    pub version: String,
    pub skill_definition: String,
    pub manifest: String,
    pub created_by: Option<String>,
}

/// Decision returned by [`classify_publish_duplicate`].
#[derive(Debug, PartialEq)]
pub(crate) enum PublishDuplicateDecision {
    /// Existing row is identical; return 200 success with original payload.
    IdempotentReplay(serde_json::Value),
    /// Existing row differs; return 409 Conflict.
    Conflict,
}

/// Classify whether a duplicate `publish_skill` call is an idempotent retry or a real conflict.
///
/// Compares `skill_definition` and `manifest` of the stored row against what was being inserted,
/// using structural JSON equality because MatrixOne — like many JSON-typed columns — does not
/// guarantee that read-back text preserves on-write key ordering or whitespace.
pub(crate) fn classify_publish_duplicate(
    existing: Option<ExistingPublishRow>,
    request_user_id: &str,
    skill_id: &str,
    new_definition_json: &str,
    new_manifest_json: &str,
) -> PublishDuplicateDecision {
    let Some(row) = existing else {
        return PublishDuplicateDecision::Conflict;
    };
    if row.created_by.as_deref() != Some(request_user_id) {
        return PublishDuplicateDecision::Conflict;
    }
    let same_def = json_structurally_equal(&row.skill_definition, new_definition_json);
    let same_manifest = json_structurally_equal(&row.manifest, new_manifest_json);
    if same_def && same_manifest {
        PublishDuplicateDecision::IdempotentReplay(serde_json::json!({
            "skill_id": skill_id,
            "skill_name": row.skill_name,
            "version": row.version,
            "status": "published",
        }))
    } else {
        PublishDuplicateDecision::Conflict
    }
}

/// Compare two JSON strings for **structural** equality.
///
/// Returns `true` iff both strings parse to the same `serde_json::Value` tree.
/// Falls back to byte-equality when either side fails to parse — that way
/// non-JSON columns (or schema-evolved free-form text) still behave like a
/// strict `==` comparison instead of silently collapsing to "equal" on a
/// parser error.
pub(crate) fn json_structurally_equal(a: &str, b: &str) -> bool {
    match (
        serde_json::from_str::<serde_json::Value>(a),
        serde_json::from_str::<serde_json::Value>(b),
    ) {
        (Ok(va), Ok(vb)) => va == vb,
        _ => a == b,
    }
}

// ── Trait ─────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait SkillService: Send + Sync {
    async fn list_skills(
        &self,
        user_id: String,
        limit: u32,
        cursor: Option<SkillListCursor>,
    ) -> Result<SkillListRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn get_skill(
        &self,
        user_id: String,
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
        user_id: String,
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
        crate::require_shared_pool(self.pool.as_ref(), "DatabaseSkillService", &self.matrixone)
    }
}

#[async_trait]
impl SkillService for DatabaseSkillService {
    async fn list_skills(
        &self,
        user_id: String,
        limit: u32,
        cursor: Option<SkillListCursor>,
    ) -> Result<SkillListRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let limit = validate_skill_list_limit(limit);

        let count_sql =
            format!("SELECT COUNT(*) AS cnt FROM skills_registry WHERE {VISIBLE_SKILL_PREDICATE}",);
        let count_row = query(&count_sql)
            .bind(&user_id)
            .fetch_one(&pool)
            .await
            .map_err(internal_error)?;
        let total: i64 = count_row.try_get("cnt").map_err(internal_error)?;

        let list_sql = if cursor.is_some() {
            format!(
                "SELECT {SKILL_REGISTRY_LIST_SELECT} FROM skills_registry \
                 WHERE {VISIBLE_SKILL_PREDICATE} {SKILL_LIST_CURSOR_SQL}{SKILL_LIST_ORDER_SQL}",
            )
        } else {
            format!(
                "SELECT {SKILL_REGISTRY_LIST_SELECT} FROM skills_registry \
                 WHERE {VISIBLE_SKILL_PREDICATE}{SKILL_LIST_ORDER_SQL}",
            )
        };
        let mut list_query = query(&list_sql).bind(&user_id);
        if let Some(cursor) = &cursor {
            let cursor = validate_skill_list_cursor(cursor)?;
            list_query = list_query
                .bind(cursor.skill_name.clone())
                .bind(cursor.skill_name.clone())
                .bind(cursor.version.clone())
                .bind(cursor.skill_name)
                .bind(cursor.version)
                .bind(cursor.skill_id);
        }
        let rows = list_query
            .bind(skill_list_query_limit(limit))
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;

        let mut skills: Vec<SkillListItem> = rows
            .iter()
            .map(|row| -> Result<SkillListItem, _> {
                Ok(SkillListItem {
                    skill_id: row
                        .try_get::<String, _>("skill_id")
                        .map_err(internal_error)?,
                    skill_name: row
                        .try_get::<String, _>("skill_name")
                        .map_err(internal_error)?,
                    version: row
                        .try_get::<String, _>("version")
                        .map_err(internal_error)?,
                    description: row.try_get::<String, _>("description").ok(),
                    status: row.try_get::<String, _>("status").ok(),
                    source: row.try_get::<String, _>("source").ok(),
                    category: row.try_get::<String, _>("category").ok(),
                    created_at: row.try_get::<String, _>("created_at").ok(),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = skills.len() > limit as usize;
        if has_more {
            skills.truncate(limit as usize);
        }
        let next_cursor = if has_more {
            skills.last().map(skill_list_cursor_from_item).transpose()?
        } else {
            None
        };

        Ok(SkillListRecord {
            skills,
            total,
            limit,
            next_cursor,
        })
    }

    async fn get_skill(
        &self,
        user_id: String,
        skill_id: String,
        version: Option<String>,
    ) -> Result<SkillRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let by_id_sql = format!(
            "SELECT skill_id, skill_name, version, description, \
             IFNULL(CAST(skill_definition AS CHAR), 'null') AS definition_json, \
             DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM skills_registry WHERE skill_id = ? AND {VISIBLE_SKILL_PREDICATE}",
        );
        let row = query(&by_id_sql)
            .bind(&skill_id)
            .bind(&user_id)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?;

        let row = if row.is_none() {
            let name = skill_id.split('@').next().unwrap_or(&skill_id);
            if let Some(version) = version.as_deref() {
                let by_name_version_sql = format!(
                    "SELECT skill_id, skill_name, version, description, \
                     IFNULL(CAST(skill_definition AS CHAR), 'null') AS definition_json, \
                     DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
                     FROM skills_registry WHERE skill_name = ? AND version = ? AND {VISIBLE_SKILL_PREDICATE} \
                     ORDER BY {USER_OWNED_SKILL_FIRST_ORDER} LIMIT 1",
                );
                query(&by_name_version_sql)
                    .bind(name)
                    .bind(version)
                    .bind(&user_id)
                    .bind(&user_id)
                    .fetch_optional(&pool)
                    .await
                    .map_err(internal_error)?
            } else {
                let by_name_latest_sql = format!(
                    "SELECT skill_id, skill_name, version, description, \
                     IFNULL(CAST(skill_definition AS CHAR), 'null') AS definition_json, \
                     DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
                     FROM skills_registry WHERE skill_name = ? AND {VISIBLE_SKILL_PREDICATE} \
                     ORDER BY {USER_OWNED_SKILL_FIRST_ORDER} LIMIT 1",
                );
                query(&by_name_latest_sql)
                    .bind(name)
                    .bind(&user_id)
                    .bind(&user_id)
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
        let def_json: String = row.try_get("definition_json").map_err(internal_error)?;

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
        user_id: String,
    ) -> Result<SkillInfoRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let info_sql = format!(
            "SELECT skill_name, version, description, source, status, created_by, category, \
             publisher_id, trust_tier, \
             DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM skills_registry WHERE skill_name = ? AND {VISIBLE_SKILL_PREDICATE} \
             ORDER BY {USER_OWNED_SKILL_FIRST_ORDER} LIMIT 1",
        );
        let row = query(&info_sql)
            .bind(&skill_name)
            .bind(&user_id)
            .bind(&user_id)
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
        user_id: String,
        skill_name: String,
    ) -> Result<Vec<SkillVersionRecord>, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let versions_sql = format!(
            "SELECT version, status, is_active, \
             DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM skills_registry WHERE skill_name = ? AND {VISIBLE_SKILL_PREDICATE} \
             ORDER BY {USER_OWNED_SKILL_FIRST_ORDER}",
        );
        let rows = query(&versions_sql)
            .bind(&skill_name)
            .bind(&user_id)
            .bind(&user_id)
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
        user_id: String,
        per_group: u32,
    ) -> Result<SkillStatusRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let per_group = per_group.min(MAX_SKILL_STATUS_PER_GROUP);

        let fetch_group = |source: &str| {
            let pool = pool.clone();
            let user_id = user_id.clone();
            let source = source.to_string();
            async move {
                let group_sql = format!(
                    "SELECT skill_id, skill_name, version, description, status, category, \
                     DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
                     FROM skills_registry WHERE source = ? AND {VISIBLE_SKILL_PREDICATE} \
                     ORDER BY skill_name LIMIT ?",
                );
                let rows = query(&group_sql)
                    .bind(&source)
                    .bind(&user_id)
                    .bind(per_group)
                    .fetch_all(&pool)
                    .await?;

                let items = rows
                    .iter()
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
                    .collect::<Vec<_>>();
                Ok::<_, sqlx::Error>(items)
            }
        };

        let (builtin, marketplace, user) = tokio::try_join!(
            fetch_group("builtin"),
            fetch_group("marketplace"),
            fetch_group("user"),
        )
        .map_err(internal_error)?;

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

        let skill_type = PublishedSkillType::parse(&request.skill_type).ok_or_else(|| {
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
            PublishedSkillType::Local => {
                if remote_url.is_some() {
                    return Err(error_response(
                        StatusCode::BAD_REQUEST,
                        "local skills must not provide remote_url",
                    ));
                }
            }
            PublishedSkillType::Remote => {
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
        let deps_json = request
            .dependencies
            .as_ref()
            .map(|d| serde_json::to_string(d).expect("dependency serialization should not fail"));
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
        if let Some(dependencies) = request.dependencies.clone() {
            manifest_map.insert("dependencies".to_string(), serde_json::json!(dependencies));
        }
        let skill_definition_json =
            serde_json::to_string(&serde_json::Value::Object(manifest_map.clone()))
                .expect("serde_json Value::Object serialization is infallible");
        let manifest_json = serde_json::to_string(&serde_json::Value::Object(manifest_map))
            .expect("serde_json Value::Object serialization is infallible");

        let insert_result = query(
            "INSERT INTO skills_registry \
             (skill_id, skill_name, version, description, skill_definition, dependencies, manifest, \
                category, priority, is_active, status, source, is_public, created_by, \
                publisher_id, trust_tier, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 1, 'active', 'user', 1, ?, ?, ?, NOW(), NOW())",
        )
        .bind(&skill_id)
        .bind(&request.name)
        .bind(&request.version)
        .bind(&request.description)
        .bind(&skill_definition_json)
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
                // Re-query to determine whether this is an idempotent retry or a real conflict.
                let dup_row = query(
                    "SELECT skill_name, version, \
                     IFNULL(CAST(skill_definition AS CHAR), '') AS skill_definition, \
                     IFNULL(CAST(manifest AS CHAR), '') AS manifest, \
                     created_by \
                     FROM skills_registry WHERE skill_id = ?",
                )
                .bind(&skill_id)
                .fetch_optional(&pool)
                .await
                .ok()
                .flatten()
                .map(|row| ExistingPublishRow {
                    skill_name: row.try_get::<String, _>("skill_name").unwrap_or_default(),
                    version: row.try_get::<String, _>("version").unwrap_or_default(),
                    skill_definition: row
                        .try_get::<String, _>("skill_definition")
                        .unwrap_or_default(),
                    manifest: row.try_get::<String, _>("manifest").unwrap_or_default(),
                    created_by: row.try_get::<String, _>("created_by").ok(),
                });

                match classify_publish_duplicate(
                    dup_row,
                    &user_id,
                    &skill_id,
                    &skill_definition_json,
                    &manifest_json,
                ) {
                    PublishDuplicateDecision::IdempotentReplay(value) => {
                        return Ok(value);
                    }
                    PublishDuplicateDecision::Conflict => {
                        return Err(error_response(
                            StatusCode::CONFLICT,
                            format!("Skill '{}' already exists", skill_id),
                        ));
                    }
                }
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

        let created_by: String = row.try_get("created_by").map_err(internal_error)?;
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
    async fn list_skills(
        &self,
        _: String,
        _: u32,
        _: Option<SkillListCursor>,
    ) -> Result<SkillListRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("skill service not configured"))
    }
    async fn get_skill(
        &self,
        _: String,
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

fn default_skill_type() -> String {
    SKILL_TYPE_LOCAL.to_string()
}

#[derive(Deserialize)]
pub struct PublishSkillRequest {
    pub name: String,
    pub version: String,
    pub description: String,
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
    pub after_skill_name: Option<String>,
    pub after_version: Option<String>,
    pub after_skill_id: Option<String>,
}

impl SkillListQuery {
    pub fn cursor(&self) -> Result<Option<SkillListCursor>, &'static str> {
        match (
            &self.after_skill_name,
            &self.after_version,
            &self.after_skill_id,
        ) {
            (None, None, None) => Ok(None),
            (Some(skill_name), Some(version), Some(skill_id)) => Ok(Some(SkillListCursor {
                skill_name: skill_name.clone(),
                version: version.clone(),
                skill_id: skill_id.clone(),
            })),
            _ => Err(
                "skill list cursor requires after_skill_name, after_version, and after_skill_id",
            ),
        }
    }
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
    fn published_skill_type_parse() {
        assert_eq!(
            PublishedSkillType::parse("local"),
            Some(PublishedSkillType::Local)
        );
        assert_eq!(
            PublishedSkillType::parse(""),
            Some(PublishedSkillType::Local)
        );
        assert_eq!(
            PublishedSkillType::parse("remote"),
            Some(PublishedSkillType::Remote)
        );
        assert!(PublishedSkillType::parse("invalid").is_none());
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
        assert_eq!(q.cursor().unwrap(), None);
    }

    #[test]
    fn skill_list_query_requires_complete_cursor() {
        let q: SkillListQuery =
            serde_json::from_str(r#"{"after_skill_name":"alpha","after_version":"1.0"}"#).unwrap();
        assert_eq!(
            q.cursor().unwrap_err(),
            "skill list cursor requires after_skill_name, after_version, and after_skill_id"
        );
    }

    #[test]
    fn skill_list_limit_has_hard_cap_and_minimum() {
        assert_eq!(validate_skill_list_limit(0), 1);
        assert_eq!(validate_skill_list_limit(10), 10);
        assert_eq!(validate_skill_list_limit(u32::MAX), MAX_API_LIST_LIMIT);
    }

    #[test]
    fn skill_list_cursor_rejects_empty_fields() {
        let ok = validate_skill_list_cursor(&SkillListCursor {
            skill_name: " alpha ".to_string(),
            version: " 1.0 ".to_string(),
            skill_id: " alpha@1.0 ".to_string(),
        })
        .unwrap();
        assert_eq!(ok.skill_name, "alpha");
        assert_eq!(ok.version, "1.0");
        assert_eq!(ok.skill_id, "alpha@1.0");

        let err = validate_skill_list_cursor(&SkillListCursor {
            skill_name: "alpha".to_string(),
            version: "".to_string(),
            skill_id: "alpha@1.0".to_string(),
        })
        .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn skill_list_sql_contract_uses_seek_cursor_not_offset() {
        assert!(
            !SKILL_LIST_ORDER_SQL
                .to_ascii_uppercase()
                .contains(" OFFSET ")
        );
        assert!(
            !SKILL_LIST_CURSOR_SQL
                .to_ascii_uppercase()
                .contains(" OFFSET ")
        );
        assert!(SKILL_LIST_ORDER_SQL.contains("skill_name ASC"));
        assert!(SKILL_LIST_CURSOR_SQL.contains("skill_id > ?"));
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

    // ── classify_publish_duplicate unit tests ─────────────────────────────────

    fn make_existing_publish_row(def: &str, manifest: &str) -> ExistingPublishRow {
        ExistingPublishRow {
            skill_name: "pub-skill".to_string(),
            version: "2.0.0".to_string(),
            skill_definition: def.to_string(),
            manifest: manifest.to_string(),
            created_by: Some("owner-1".to_string()),
        }
    }

    #[test]
    fn classify_publish_duplicate_none_returns_conflict() {
        let decision =
            classify_publish_duplicate(None, "owner-1", "pub-skill@2.0.0", r#"{}"#, r#"{}"#);
        assert_eq!(decision, PublishDuplicateDecision::Conflict);
    }

    #[test]
    fn classify_publish_duplicate_identical_returns_idempotent_replay() {
        let def = r#"{"skill_type":"local","priority":5}"#;
        let manifest = r#"{"skill_type":"local","priority":5}"#;
        let row = make_existing_publish_row(def, manifest);
        let decision =
            classify_publish_duplicate(Some(row), "owner-1", "pub-skill@2.0.0", def, manifest);
        assert!(
            matches!(decision, PublishDuplicateDecision::IdempotentReplay(_)),
            "identical payload must return IdempotentReplay, got {decision:?}"
        );
        if let PublishDuplicateDecision::IdempotentReplay(value) = decision {
            assert_eq!(value["skill_id"], "pub-skill@2.0.0");
            assert_eq!(value["status"], "published");
        }
    }

    #[test]
    fn classify_publish_duplicate_different_definition_returns_conflict() {
        let row = make_existing_publish_row(r#"{"priority":5}"#, r#"{"priority":5}"#);
        let decision = classify_publish_duplicate(
            Some(row),
            "owner-1",
            "pub-skill@2.0.0",
            r#"{"priority":10}"#,
            r#"{"priority":5}"#,
        );
        assert_eq!(decision, PublishDuplicateDecision::Conflict);
    }

    #[test]
    fn classify_publish_duplicate_different_manifest_returns_conflict() {
        let row = make_existing_publish_row(r#"{"priority":5}"#, r#"{"priority":5}"#);
        let decision = classify_publish_duplicate(
            Some(row),
            "owner-1",
            "pub-skill@2.0.0",
            r#"{"priority":5}"#,
            r#"{"priority":10}"#,
        );
        assert_eq!(decision, PublishDuplicateDecision::Conflict);
    }

    #[test]
    fn classify_publish_duplicate_same_payload_different_owner_conflicts() {
        let def = r#"{"skill_type":"local","priority":5}"#;
        let manifest = r#"{"skill_type":"local","priority":5}"#;
        let row = make_existing_publish_row(def, manifest);
        let decision =
            classify_publish_duplicate(Some(row), "other-user", "pub-skill@2.0.0", def, manifest);
        assert_eq!(decision, PublishDuplicateDecision::Conflict);
    }

    /// Regression: MatrixOne can reorder JSON object keys on persist/read-back.
    /// Publish retries compare structurally so identical retries remain idempotent.
    /// Caught by
    /// `it_publish_skill_idempotent_retry_returns_200`.
    #[test]
    fn classify_publish_duplicate_reordered_keys_returns_idempotent_replay() {
        let new_def = r#"{"skill_type":"local","priority":5}"#;
        let stored_def = r#"{"priority":5,"skill_type":"local"}"#;
        let new_manifest = r#"{"a":1,"b":2}"#;
        let stored_manifest = r#"{"b":2,"a":1}"#;
        let row = make_existing_publish_row(stored_def, stored_manifest);
        let decision = classify_publish_duplicate(
            Some(row),
            "owner-1",
            "pub-skill@2.0.0",
            new_def,
            new_manifest,
        );
        assert!(
            matches!(decision, PublishDuplicateDecision::IdempotentReplay(_)),
            "reordered-keys retry must be idempotent replay, got {decision:?}"
        );
    }

    #[test]
    fn json_structurally_equal_ignores_key_order_and_whitespace() {
        assert!(json_structurally_equal(
            r#"{"a":1,"b":2}"#,
            r#"{ "b": 2, "a": 1 }"#
        ));
        assert!(json_structurally_equal(r#"[1,2,3]"#, r#"[ 1, 2, 3 ]"#));
        assert!(!json_structurally_equal(r#"{"a":1}"#, r#"{"a":2}"#));
        // Non-JSON falls back to byte equality, not silent "equal".
        assert!(json_structurally_equal("not-json", "not-json"));
        assert!(!json_structurally_equal("not-json-a", "not-json-b"));
    }
}
