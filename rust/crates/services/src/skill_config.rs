use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::collections::HashMap;

use crate::auth::FernetTokenEncryptor;
use astra_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait SkillConfigService: Send + Sync {
    async fn validate_config(
        &self,
        user_id: &str,
        skill_name: &str,
        resource_key: Option<&str>,
    ) -> Result<Json<ValidationResponse>, (StatusCode, Json<ErrorResponse>)>;

    async fn get_effective_config(
        &self,
        user_id: &str,
        skill_name: &str,
    ) -> Result<Json<ConfigResponse>, (StatusCode, Json<ErrorResponse>)>;

    async fn set_setting(
        &self,
        user_id: &str,
        skill_name: &str,
        setting_name: &str,
        scope: &str,
        value: serde_json::Value,
        encryptor: &FernetTokenEncryptor,
    ) -> Result<Json<StatusResponse>, (StatusCode, Json<ErrorResponse>)>;

    async fn delete_setting(
        &self,
        user_id: &str,
        skill_name: &str,
        setting_name: &str,
        scope: &str,
    ) -> Result<Json<StatusResponse>, (StatusCode, Json<ErrorResponse>)>;

    async fn list_resources(
        &self,
        user_id: &str,
        skill_name: &str,
    ) -> Result<Json<Vec<ResourceEntry>>, (StatusCode, Json<ErrorResponse>)>;

    async fn bind_resource(
        &self,
        user_id: &str,
        skill_name: &str,
        resource_key: &str,
        bindings: HashMap<String, serde_json::Value>,
        encryptor: &FernetTokenEncryptor,
    ) -> Result<Json<BindResourceResponse>, (StatusCode, Json<ErrorResponse>)>;

    async fn unbind_resource(
        &self,
        user_id: &str,
        skill_name: &str,
        resource_key: &str,
    ) -> Result<Json<UnbindResourceResponse>, (StatusCode, Json<ErrorResponse>)>;
}

// ---------------------------------------------------------------------------
// Request / Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ValidateQuery {
    pub resource: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ValidationResponse {
    pub valid: bool,
    pub errors: Vec<ValidationError>,
}

#[derive(Debug, Serialize)]
pub struct ValidationError {
    pub section: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_key: Option<String>,
    pub error: String,
}

#[derive(Debug, Serialize)]
pub struct ConfigResponse {
    pub settings: HashMap<String, serde_json::Value>,
    pub secrets: HashMap<String, String>,
    pub resources_configured: i64,
}

#[derive(Debug, Deserialize)]
pub struct SetSettingRequest {
    pub value: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct ScopeQuery {
    #[serde(default = "default_scope")]
    pub scope: String,
}

fn default_scope() -> String {
    "user".to_string()
}

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct ResourceEntry {
    pub resource_key: String,
    pub resource_type: String,
}

#[derive(Debug, Deserialize)]
pub struct BindResourceRequest {
    pub bindings: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct BindResourceResponse {
    pub status: String,
    pub resource_key: String,
}

#[derive(Debug, Serialize)]
pub struct UnbindResourceResponse {
    pub status: String,
    pub count: u64,
}

// ---------------------------------------------------------------------------
// Database implementation
// ---------------------------------------------------------------------------

pub struct DatabaseSkillConfigService {
    settings: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseSkillConfigService {
    pub fn new(settings: MatrixOneSettings) -> Self {
        Self {
            settings,
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
        connect_matrixone(&self.settings).await
    }

    fn scope_id(user_id: &str, scope: &str) -> Option<String> {
        match scope {
            "global" => None,
            _ => Some(user_id.to_string()),
        }
    }
}

#[async_trait]
impl SkillConfigService for DatabaseSkillConfigService {
    async fn validate_config(
        &self,
        user_id: &str,
        skill_name: &str,
        _resource_key: Option<&str>,
    ) -> Result<Json<ValidationResponse>, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let exists = sqlx::query("SELECT 1 FROM skills_registry WHERE skill_name = ? LIMIT 1")
            .bind(skill_name)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?
            .is_some();

        let mut errors = Vec::new();
        if !exists {
            errors.push(ValidationError {
                section: "settings".to_string(),
                name: skill_name.to_string(),
                resource_key: None,
                error: format!("Skill '{}' not found in registry", skill_name),
            });
        }

        let setting_rows = sqlx::query(
            "SELECT setting_name, setting_value, is_secret FROM skill_settings \
             WHERE skill_name = ? AND (scope_type = 'global' OR (scope_type = 'user' AND scope_id = ?)) \
             ORDER BY CASE scope_type WHEN 'user' THEN 1 WHEN 'tenant' THEN 2 WHEN 'global' THEN 3 END",
        )
        .bind(skill_name)
        .bind(user_id)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        for row in &setting_rows {
            let name: String = row.try_get("setting_name").unwrap_or_default();
            let is_secret: i16 = row.try_get("is_secret").unwrap_or(0);
            let value: String = row.try_get("setting_value").unwrap_or_default();
            if is_secret == 1 && value.is_empty() {
                errors.push(ValidationError {
                    section: "secrets".to_string(),
                    name,
                    resource_key: None,
                    error: "Secret value is empty".to_string(),
                });
            }
        }

        Ok(Json(ValidationResponse {
            valid: errors.is_empty(),
            errors,
        }))
    }

    async fn get_effective_config(
        &self,
        user_id: &str,
        skill_name: &str,
    ) -> Result<Json<ConfigResponse>, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let rows = sqlx::query(
            "SELECT setting_name, setting_value, is_secret, scope_type FROM skill_settings \
             WHERE skill_name = ? AND (scope_type = 'global' OR (scope_type = 'user' AND scope_id = ?)) \
             ORDER BY CASE scope_type WHEN 'user' THEN 1 WHEN 'tenant' THEN 2 WHEN 'global' THEN 3 END",
        )
        .bind(skill_name)
        .bind(user_id)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let mut settings: HashMap<String, serde_json::Value> = HashMap::new();
        let mut secrets: HashMap<String, String> = HashMap::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        for row in &rows {
            let name: String = row.try_get("setting_name").unwrap_or_default();
            if seen.contains(&name) {
                continue;
            }
            seen.insert(name.clone());

            let is_secret: i16 = row.try_get("is_secret").unwrap_or(0);
            let value: String = row.try_get("setting_value").unwrap_or_default();

            if is_secret == 1 {
                secrets.insert(name, "***".to_string());
            } else {
                settings.insert(name, serde_json::Value::String(value));
            }
        }

        let count_row = sqlx::query(
            "SELECT COUNT(DISTINCT resource_key) as cnt FROM skill_resource_bindings \
             WHERE user_id = ? AND skill_name = ?",
        )
        .bind(user_id)
        .bind(skill_name)
        .fetch_one(&pool)
        .await
        .map_err(internal_error)?;
        let resources_configured: i64 = count_row.try_get("cnt").unwrap_or(0);

        Ok(Json(ConfigResponse {
            settings,
            secrets,
            resources_configured,
        }))
    }

    async fn set_setting(
        &self,
        user_id: &str,
        skill_name: &str,
        setting_name: &str,
        scope: &str,
        value: serde_json::Value,
        encryptor: &FernetTokenEncryptor,
    ) -> Result<Json<StatusResponse>, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let scope_id = Self::scope_id(user_id, scope);
        let value_str = match &value {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };

        let is_secret = self
            .check_is_secret(&pool, skill_name, setting_name)
            .await
            .unwrap_or(false);

        let stored_value = if is_secret {
            encryptor.encrypt(&value_str).map_err(|e| {
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Encryption failed: {}", e),
                )
            })?
        } else {
            value_str
        };
        let is_secret_flag: i16 = if is_secret { 1 } else { 0 };

        let existing = sqlx::query(
            "SELECT skill_id FROM skill_settings \
             WHERE skill_name = ? AND setting_name = ? AND scope_type = ? \
             AND (scope_id = ? OR (scope_id IS NULL AND ? IS NULL))",
        )
        .bind(skill_name)
        .bind(setting_name)
        .bind(scope)
        .bind(&scope_id)
        .bind(&scope_id)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?;

        if existing.is_some() {
            sqlx::query(
                "UPDATE skill_settings SET setting_value = ?, is_secret = ?, updated_by = ? \
                 WHERE skill_name = ? AND setting_name = ? AND scope_type = ? \
                 AND (scope_id = ? OR (scope_id IS NULL AND ? IS NULL))",
            )
            .bind(&stored_value)
            .bind(is_secret_flag)
            .bind(user_id)
            .bind(skill_name)
            .bind(setting_name)
            .bind(scope)
            .bind(&scope_id)
            .bind(&scope_id)
            .execute(&pool)
            .await
            .map_err(internal_error)?;
        } else {
            let id = uuid::Uuid::new_v4().to_string();
            sqlx::query(
                "INSERT INTO skill_settings (skill_id, skill_name, setting_name, setting_value, is_secret, scope_type, scope_id, updated_by) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(skill_name)
            .bind(setting_name)
            .bind(&stored_value)
            .bind(is_secret_flag)
            .bind(scope)
            .bind(&scope_id)
            .bind(user_id)
            .execute(&pool)
            .await
            .map_err(internal_error)?;
        }

        Ok(Json(StatusResponse {
            status: "ok".to_string(),
        }))
    }

    async fn delete_setting(
        &self,
        user_id: &str,
        skill_name: &str,
        setting_name: &str,
        scope: &str,
    ) -> Result<Json<StatusResponse>, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let scope_id = Self::scope_id(user_id, scope);

        let result = sqlx::query(
            "DELETE FROM skill_settings \
             WHERE skill_name = ? AND setting_name = ? AND scope_type = ? \
             AND (scope_id = ? OR (scope_id IS NULL AND ? IS NULL))",
        )
        .bind(skill_name)
        .bind(setting_name)
        .bind(scope)
        .bind(&scope_id)
        .bind(&scope_id)
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        if result.rows_affected() == 0 {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!(
                    "Setting '{}' not found for skill '{}' at scope '{}'",
                    setting_name, skill_name, scope
                ),
            ));
        }

        Ok(Json(StatusResponse {
            status: "deleted".to_string(),
        }))
    }

    async fn list_resources(
        &self,
        user_id: &str,
        skill_name: &str,
    ) -> Result<Json<Vec<ResourceEntry>>, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let rows = sqlx::query(
            "SELECT DISTINCT resource_key, resource_type FROM skill_resource_bindings \
             WHERE user_id = ? AND skill_name = ?",
        )
        .bind(user_id)
        .bind(skill_name)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let entries: Vec<ResourceEntry> = rows
            .iter()
            .map(|r| ResourceEntry {
                resource_key: r.try_get("resource_key").unwrap_or_default(),
                resource_type: r.try_get("resource_type").unwrap_or_default(),
            })
            .collect();

        Ok(Json(entries))
    }

    async fn bind_resource(
        &self,
        user_id: &str,
        skill_name: &str,
        resource_key: &str,
        bindings: HashMap<String, serde_json::Value>,
        encryptor: &FernetTokenEncryptor,
    ) -> Result<Json<BindResourceResponse>, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let type_row = sqlx::query(
            "SELECT resource_type FROM skill_resource_bindings \
             WHERE user_id = ? AND skill_name = ? AND resource_key = ? LIMIT 1",
        )
        .bind(user_id)
        .bind(skill_name)
        .bind(resource_key)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?;

        let resource_type: String = type_row
            .map(|r| r.try_get("resource_type").unwrap_or_default())
            .unwrap_or_else(|| "generic".to_string());

        for (binding_name, value) in &bindings {
            let value_str = match value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };

            let is_secret = self
                .check_binding_is_secret(&pool, skill_name, resource_key, binding_name)
                .await
                .unwrap_or(false);

            let stored_value = if is_secret {
                encryptor.encrypt(&value_str).map_err(|e| {
                    error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Encryption failed: {}", e),
                    )
                })?
            } else {
                value_str
            };
            let is_secret_flag: i16 = if is_secret { 1 } else { 0 };

            let existing = sqlx::query(
                "SELECT binding_id FROM skill_resource_bindings \
                 WHERE user_id = ? AND skill_name = ? AND resource_key = ? AND binding_name = ?",
            )
            .bind(user_id)
            .bind(skill_name)
            .bind(resource_key)
            .bind(binding_name)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?;

            if existing.is_some() {
                sqlx::query(
                    "UPDATE skill_resource_bindings SET binding_value = ?, is_secret = ?, updated_by = ? \
                     WHERE user_id = ? AND skill_name = ? AND resource_key = ? AND binding_name = ?",
                )
                .bind(&stored_value)
                .bind(is_secret_flag)
                .bind(user_id)
                .bind(user_id)
                .bind(skill_name)
                .bind(resource_key)
                .bind(binding_name)
                .execute(&pool)
                .await
                .map_err(internal_error)?;
            } else {
                let id = uuid::Uuid::new_v4().to_string();
                sqlx::query(
                    "INSERT INTO skill_resource_bindings \
                     (binding_id, user_id, skill_name, resource_type, resource_key, binding_name, binding_value, is_secret, updated_by) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&id)
                .bind(user_id)
                .bind(skill_name)
                .bind(&resource_type)
                .bind(resource_key)
                .bind(binding_name)
                .bind(&stored_value)
                .bind(is_secret_flag)
                .bind(user_id)
                .execute(&pool)
                .await
                .map_err(internal_error)?;
            }
        }

        Ok(Json(BindResourceResponse {
            status: "ok".to_string(),
            resource_key: resource_key.to_string(),
        }))
    }

    async fn unbind_resource(
        &self,
        user_id: &str,
        skill_name: &str,
        resource_key: &str,
    ) -> Result<Json<UnbindResourceResponse>, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let result = sqlx::query(
            "DELETE FROM skill_resource_bindings \
             WHERE user_id = ? AND skill_name = ? AND resource_key = ?",
        )
        .bind(user_id)
        .bind(skill_name)
        .bind(resource_key)
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        if result.rows_affected() == 0 {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!(
                    "No resource bindings found for key '{}' in skill '{}'",
                    resource_key, skill_name
                ),
            ));
        }

        Ok(Json(UnbindResourceResponse {
            status: "deleted".to_string(),
            count: result.rows_affected(),
        }))
    }
}

impl DatabaseSkillConfigService {
    async fn check_is_secret(
        &self,
        pool: &sqlx::MySqlPool,
        skill_name: &str,
        setting_name: &str,
    ) -> Result<bool, sqlx::Error> {
        let row = sqlx::query(
            "SELECT IFNULL(CAST(manifest AS CHAR), '{}') as manifest \
             FROM skills_registry WHERE skill_name = ? AND is_active = 1 LIMIT 1",
        )
        .bind(skill_name)
        .fetch_optional(pool)
        .await?;

        if let Some(row) = row {
            let manifest_str: String = row.try_get("manifest").unwrap_or_default();
            if let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&manifest_str)
                && let Some(secrets) = manifest.get("secrets")
                && let Some(arr) = secrets.as_array()
            {
                for s in arr {
                    if let Some(name) = s.get("name").and_then(|n| n.as_str())
                        && name == setting_name
                    {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    async fn check_binding_is_secret(
        &self,
        pool: &sqlx::MySqlPool,
        skill_name: &str,
        _resource_key: &str,
        binding_name: &str,
    ) -> Result<bool, sqlx::Error> {
        let row = sqlx::query(
            "SELECT IFNULL(CAST(manifest AS CHAR), '{}') as manifest \
             FROM skills_registry WHERE skill_name = ? AND is_active = 1 LIMIT 1",
        )
        .bind(skill_name)
        .fetch_optional(pool)
        .await?;

        if let Some(row) = row {
            let manifest_str: String = row.try_get("manifest").unwrap_or_default();
            if let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&manifest_str)
                && let Some(resources) = manifest.get("resources")
                && let Some(arr) = resources.as_array()
            {
                for r in arr {
                    if let Some(bindings) = r.get("bindings").and_then(|b| b.as_array()) {
                        for b in bindings {
                            let name = b.get("name").and_then(|n| n.as_str());
                            let btype = b.get("type").and_then(|t| t.as_str());
                            if name == Some(binding_name) && btype == Some("secret") {
                                return Ok(true);
                            }
                        }
                    }
                }
            }
        }
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// Unconfigured (noop) implementation
// ---------------------------------------------------------------------------

pub struct UnconfiguredSkillConfigService;

#[async_trait]
impl SkillConfigService for UnconfiguredSkillConfigService {
    async fn validate_config(
        &self,
        _user_id: &str,
        _skill_name: &str,
        _resource_key: Option<&str>,
    ) -> Result<Json<ValidationResponse>, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Skill config service not configured",
        ))
    }

    async fn get_effective_config(
        &self,
        _user_id: &str,
        _skill_name: &str,
    ) -> Result<Json<ConfigResponse>, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Skill config service not configured",
        ))
    }

    async fn set_setting(
        &self,
        _user_id: &str,
        _skill_name: &str,
        _setting_name: &str,
        _scope: &str,
        _value: serde_json::Value,
        _encryptor: &FernetTokenEncryptor,
    ) -> Result<Json<StatusResponse>, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Skill config service not configured",
        ))
    }

    async fn delete_setting(
        &self,
        _user_id: &str,
        _skill_name: &str,
        _setting_name: &str,
        _scope: &str,
    ) -> Result<Json<StatusResponse>, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Skill config service not configured",
        ))
    }

    async fn list_resources(
        &self,
        _user_id: &str,
        _skill_name: &str,
    ) -> Result<Json<Vec<ResourceEntry>>, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Skill config service not configured",
        ))
    }

    async fn bind_resource(
        &self,
        _user_id: &str,
        _skill_name: &str,
        _resource_key: &str,
        _bindings: HashMap<String, serde_json::Value>,
        _encryptor: &FernetTokenEncryptor,
    ) -> Result<Json<BindResourceResponse>, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Skill config service not configured",
        ))
    }

    async fn unbind_resource(
        &self,
        _user_id: &str,
        _skill_name: &str,
        _resource_key: &str,
    ) -> Result<Json<UnbindResourceResponse>, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Skill config service not configured",
        ))
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── scope_id ────────────────────────────────────────────────────────

    #[test]
    fn scope_id_global_returns_none() {
        assert_eq!(
            DatabaseSkillConfigService::scope_id("user1", "global"),
            None
        );
    }

    #[test]
    fn scope_id_user_returns_user_id() {
        assert_eq!(
            DatabaseSkillConfigService::scope_id("user1", "user"),
            Some("user1".to_string())
        );
    }

    #[test]
    fn scope_id_unknown_scope_returns_user_id() {
        // Any scope that isn't "global" returns the user_id
        assert_eq!(
            DatabaseSkillConfigService::scope_id("u", "tenant"),
            Some("u".to_string())
        );
        assert_eq!(
            DatabaseSkillConfigService::scope_id("u", ""),
            Some("u".to_string())
        );
    }

    // ── default_scope ───────────────────────────────────────────────────

    #[test]
    fn default_scope_is_user() {
        assert_eq!(default_scope(), "user");
    }

    // ── UnconfiguredSkillConfigService ───────────────────────────────────

    fn assert_unavailable(result: Result<impl std::fmt::Debug, (StatusCode, Json<ErrorResponse>)>) {
        let (status, _) = result.unwrap_err();
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn unconfigured_validate_config() {
        let svc = UnconfiguredSkillConfigService;
        assert_unavailable(svc.validate_config("u", "s", None).await);
    }

    #[tokio::test]
    async fn unconfigured_get_effective_config() {
        let svc = UnconfiguredSkillConfigService;
        assert_unavailable(svc.get_effective_config("u", "s").await);
    }

    #[tokio::test]
    async fn unconfigured_delete_setting() {
        let svc = UnconfiguredSkillConfigService;
        assert_unavailable(svc.delete_setting("u", "s", "k", "user").await);
    }

    #[tokio::test]
    async fn unconfigured_list_resources() {
        let svc = UnconfiguredSkillConfigService;
        assert_unavailable(svc.list_resources("u", "s").await);
    }

    #[tokio::test]
    async fn unconfigured_unbind_resource() {
        let svc = UnconfiguredSkillConfigService;
        assert_unavailable(svc.unbind_resource("u", "s", "r").await);
    }

    // ── Type serialization ──────────────────────────────────────────────

    #[test]
    fn validation_response_serialize() {
        let resp = ValidationResponse {
            valid: false,
            errors: vec![ValidationError {
                section: "settings".into(),
                name: "api_key".into(),
                resource_key: Some("my-db".into()),
                error: "Missing".into(),
            }],
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["valid"], false);
        assert_eq!(json["errors"][0]["resource_key"], "my-db");
    }

    #[test]
    fn validation_response_no_resource_key() {
        let resp = ValidationResponse {
            valid: true,
            errors: vec![ValidationError {
                section: "secrets".into(),
                name: "token".into(),
                resource_key: None,
                error: "Empty".into(),
            }],
        };
        let json = serde_json::to_value(&resp).unwrap();
        // resource_key: None → should be skipped
        assert!(json["errors"][0].get("resource_key").is_none());
    }

    #[test]
    fn config_response_serialize() {
        let mut settings = HashMap::new();
        settings.insert("timeout".into(), serde_json::json!(30));
        let resp = ConfigResponse {
            settings,
            secrets: HashMap::from([("api_key".into(), "***".into())]),
            resources_configured: 2,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["settings"]["timeout"], 30);
        assert_eq!(json["secrets"]["api_key"], "***");
        assert_eq!(json["resources_configured"], 2);
    }

    #[test]
    fn scope_query_default() {
        let q: ScopeQuery = serde_json::from_str("{}").unwrap();
        assert_eq!(q.scope, "user");
    }

    #[test]
    fn scope_query_custom() {
        let q: ScopeQuery = serde_json::from_str(r#"{"scope":"global"}"#).unwrap();
        assert_eq!(q.scope, "global");
    }

    #[test]
    fn bind_resource_request_deserialize() {
        let json = r#"{"bindings":{"host":"localhost","port":5432}}"#;
        let req: BindResourceRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.bindings.len(), 2);
        assert_eq!(req.bindings["host"], "localhost");
    }

    #[test]
    fn resource_entry_serialize() {
        let entry = ResourceEntry {
            resource_key: "my-db".into(),
            resource_type: "postgres".into(),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["resource_key"], "my-db");
        assert_eq!(json["resource_type"], "postgres");
    }

    #[test]
    fn status_response_serialize() {
        let resp = StatusResponse {
            status: "ok".into(),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["status"], "ok");
    }

    #[test]
    fn unbind_response_serialize() {
        let resp = UnbindResourceResponse {
            status: "deleted".into(),
            count: 3,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["count"], 3);
    }

    #[test]
    fn bind_response_serialize() {
        let resp = BindResourceResponse {
            status: "ok".into(),
            resource_key: "my-db".into(),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["resource_key"], "my-db");
    }

    #[test]
    fn validate_query_deserialize() {
        let q: ValidateQuery = serde_json::from_str(r#"{"resource":"my-db"}"#).unwrap();
        assert_eq!(q.resource, Some("my-db".into()));

        let q: ValidateQuery = serde_json::from_str("{}").unwrap();
        assert!(q.resource.is_none());
    }
}
