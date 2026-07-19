use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{Row, query};
use uuid::Uuid;

use crate::auth::FernetTokenEncryptor;
use astra_core::{ErrorResponse, MatrixOneSettings, SharedPool, error_response, internal_error};

// ── Data types ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InstallationResponse {
    pub installation_id: String,
    pub skill_name: String,
    pub skill_version: String,
    pub status: String,
    pub installed_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InstalledListResponse {
    pub installations: Vec<InstallationResponse>,
    pub total: Option<i64>,
    pub limit: i64,
    pub next_cursor: Option<InstalledListCursor>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledListCursor {
    pub installed_at: String,
    pub installation_id: String,
}

const MAX_INSTALLED_LIST_ROWS: i64 = 200;
const INSTALLED_LIST_SELECT: &str = "\
    installation_id, skill_name, skill_version, status, \
    DATE_FORMAT(installed_at, '%Y-%m-%dT%H:%i:%s.%f') AS installed_at";

fn validate_installed_list_limit(limit: i64) -> i64 {
    limit.clamp(1, MAX_INSTALLED_LIST_ROWS)
}

fn installed_list_query_limit(limit: i64) -> i64 {
    limit + 1
}

fn installed_list_cursor_db_installed_at(
    cursor: &InstalledListCursor,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let installed_at = cursor.installed_at.trim();
    if installed_at.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid installed list cursor: installed_at is required",
        ));
    }
    let db_installed_at = installed_at.replace('T', " ");
    if db_installed_at.len() != "YYYY-MM-DD HH:MM:SS.ffffff".len()
        || db_installed_at.as_bytes().get(10) != Some(&b' ')
        || db_installed_at.as_bytes().get(19) != Some(&b'.')
        || chrono::NaiveDateTime::parse_from_str(&db_installed_at, "%Y-%m-%d %H:%M:%S%.6f").is_err()
    {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("invalid installed list cursor timestamp: {installed_at}"),
        ));
    }
    Ok(db_installed_at)
}

fn installed_list_cursor_installation_id(
    cursor: &InstalledListCursor,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let installation_id = cursor.installation_id.trim();
    if installation_id.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid installed list cursor: installation_id is required",
        ));
    }
    Ok(installation_id.to_string())
}

fn installed_list_cursor_from_installation(
    installation: &InstallationResponse,
) -> Result<InstalledListCursor, (StatusCode, Json<ErrorResponse>)> {
    if installation.installed_at.trim().is_empty() {
        return Err(internal_error(format!(
            "invalid skill_installations cursor: installation_id={}, column=installed_at, value is empty",
            installation.installation_id
        )));
    }
    if installation.installation_id.trim().is_empty() {
        return Err(internal_error(
            "invalid skill_installations cursor: column=installation_id, value is empty",
        ));
    }
    Ok(InstalledListCursor {
        installed_at: installation.installed_at.clone(),
        installation_id: installation.installation_id.clone(),
    })
}

fn required_installation_string(
    row: &sqlx::mysql::MySqlRow,
    column: &'static str,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let value: String = row.try_get(column).map_err(internal_error)?;
    if value.trim().is_empty() {
        return Err(internal_error(format!(
            "invalid skill_installations.{column}: value is empty"
        )));
    }
    Ok(value)
}

fn installation_response_from_row(
    row: sqlx::mysql::MySqlRow,
) -> Result<InstallationResponse, (StatusCode, Json<ErrorResponse>)> {
    Ok(InstallationResponse {
        installation_id: required_installation_string(&row, "installation_id")?,
        skill_name: required_installation_string(&row, "skill_name")?,
        skill_version: required_installation_string(&row, "skill_version")?,
        status: required_installation_string(&row, "status")?,
        installed_at: required_installation_string(&row, "installed_at")?,
    })
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatusResponse {
    pub status: String,
}

// ── Internal request data ────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct InstallRequestData {
    pub skill_name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CredentialRequestData {
    pub skill_name: String,
    pub credential_name: String,
    pub value: String,
}

// ── Trait ─────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait MarketplaceService: Send + Sync {
    async fn install_skill(
        &self,
        user_id: String,
        request: InstallRequestData,
    ) -> Result<InstallationResponse, (StatusCode, Json<ErrorResponse>)>;

    async fn uninstall_skill(
        &self,
        user_id: String,
        request: InstallRequestData,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)>;

    async fn upgrade_skill(
        &self,
        user_id: String,
        request: InstallRequestData,
    ) -> Result<InstallationResponse, (StatusCode, Json<ErrorResponse>)>;

    async fn rollback_skill(
        &self,
        user_id: String,
        request: InstallRequestData,
    ) -> Result<InstallationResponse, (StatusCode, Json<ErrorResponse>)>;

    async fn list_installed(
        &self,
        user_id: String,
        limit: i64,
        cursor: Option<InstalledListCursor>,
    ) -> Result<InstalledListResponse, (StatusCode, Json<ErrorResponse>)>;

    async fn save_credential(
        &self,
        user_id: String,
        request: CredentialRequestData,
        encryptor: &FernetTokenEncryptor,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)>;

    async fn delete_credential(
        &self,
        user_id: String,
        skill_name: String,
        credential_name: String,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)>;

    async fn publish_skill(
        &self,
        user_id: String,
        skill_name: String,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)>;

    async fn deprecate_skill(
        &self,
        user_id: String,
        skill_name: String,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)>;
}

// ── Database implementation ──────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct DatabaseMarketplaceService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseMarketplaceService {
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
            "DatabaseMarketplaceService",
            &self.matrixone,
        )
    }
}

#[async_trait]
impl MarketplaceService for DatabaseMarketplaceService {
    async fn install_skill(
        &self,
        user_id: String,
        request: InstallRequestData,
    ) -> Result<InstallationResponse, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let skill_row = query(
            "SELECT skill_name, version FROM skills_registry WHERE skill_name = ? AND is_active = 1"
        )
        .bind(&request.skill_name)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?;

        let skill_row = skill_row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Skill '{}' not found in registry", request.skill_name),
            )
        })?;
        let version: String = skill_row.try_get("version").map_err(internal_error)?;

        let installation_id = Uuid::new_v4().to_string();

        query(
            "INSERT INTO skill_installations \
             (installation_id, user_id, skill_name, skill_version, status, installed_at, updated_at) \
             VALUES (?, ?, ?, ?, 'installed', NOW(), NOW())"
        )
        .bind(&installation_id)
        .bind(&user_id)
        .bind(&request.skill_name)
        .bind(&version)
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        let row = query(&format!(
            "SELECT {INSTALLED_LIST_SELECT} FROM skill_installations WHERE installation_id = ?"
        ))
        .bind(&installation_id)
        .fetch_one(&pool)
        .await
        .map_err(internal_error)?;
        installation_response_from_row(row)
    }

    async fn uninstall_skill(
        &self,
        user_id: String,
        request: InstallRequestData,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        query("DELETE FROM skill_installations WHERE user_id = ? AND skill_name = ?")
            .bind(&user_id)
            .bind(&request.skill_name)
            .execute(&pool)
            .await
            .map_err(internal_error)?;

        Ok(StatusResponse {
            status: "uninstalled".into(),
        })
    }

    async fn upgrade_skill(
        &self,
        user_id: String,
        request: InstallRequestData,
    ) -> Result<InstallationResponse, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let current = query(
            "SELECT installation_id, skill_version FROM skill_installations \
             WHERE user_id = ? AND skill_name = ?",
        )
        .bind(&user_id)
        .bind(&request.skill_name)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?;

        let current =
            current.ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Skill not installed"))?;
        let installation_id: String = current.try_get("installation_id").map_err(internal_error)?;
        let old_version: String = current.try_get("skill_version").map_err(internal_error)?;

        let latest =
            query("SELECT version FROM skills_registry WHERE skill_name = ? AND is_active = 1")
                .bind(&request.skill_name)
                .fetch_optional(&pool)
                .await
                .map_err(internal_error)?;

        let latest =
            latest.ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Skill not found"))?;
        let new_version: String = latest.try_get("version").map_err(internal_error)?;

        query(
            "UPDATE skill_installations SET skill_version = ?, previous_version = ?, \
             status = 'upgraded', updated_at = NOW() \
             WHERE installation_id = ?",
        )
        .bind(&new_version)
        .bind(&old_version)
        .bind(&installation_id)
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        let row = query(&format!(
            "SELECT {INSTALLED_LIST_SELECT} FROM skill_installations WHERE installation_id = ?"
        ))
        .bind(&installation_id)
        .fetch_one(&pool)
        .await
        .map_err(internal_error)?;
        installation_response_from_row(row)
    }

    async fn rollback_skill(
        &self,
        user_id: String,
        request: InstallRequestData,
    ) -> Result<InstallationResponse, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let current = query(
            "SELECT installation_id, skill_version, previous_version FROM skill_installations \
             WHERE user_id = ? AND skill_name = ?",
        )
        .bind(&user_id)
        .bind(&request.skill_name)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?;

        let current =
            current.ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Skill not installed"))?;
        let installation_id: String = current.try_get("installation_id").map_err(internal_error)?;
        let previous_version: Option<String> = current
            .try_get("previous_version")
            .map_err(internal_error)?;
        let previous_version = previous_version.ok_or_else(|| {
            error_response(
                StatusCode::BAD_REQUEST,
                "No previous version to rollback to",
            )
        })?;

        query(
            "UPDATE skill_installations SET skill_version = ?, previous_version = NULL, \
             status = 'rolled_back', updated_at = NOW() \
             WHERE installation_id = ?",
        )
        .bind(&previous_version)
        .bind(&installation_id)
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        let row = query(&format!(
            "SELECT {INSTALLED_LIST_SELECT} FROM skill_installations WHERE installation_id = ?"
        ))
        .bind(&installation_id)
        .fetch_one(&pool)
        .await
        .map_err(internal_error)?;
        installation_response_from_row(row)
    }

    async fn list_installed(
        &self,
        user_id: String,
        limit: i64,
        cursor: Option<InstalledListCursor>,
    ) -> Result<InstalledListResponse, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let limit = validate_installed_list_limit(limit);

        let list_sql = if cursor.is_some() {
            format!(
                "SELECT {INSTALLED_LIST_SELECT} FROM skill_installations WHERE user_id = ? \
                 AND (installed_at < ? OR (installed_at = ? AND installation_id < ?)) \
                 ORDER BY installed_at DESC, installation_id DESC LIMIT ?"
            )
        } else {
            format!(
                "SELECT {INSTALLED_LIST_SELECT} FROM skill_installations WHERE user_id = ? \
                 ORDER BY installed_at DESC, installation_id DESC LIMIT ?"
            )
        };
        let mut list_query = query(&list_sql).bind(&user_id);
        if let Some(cursor) = &cursor {
            let installed_at = installed_list_cursor_db_installed_at(cursor)?;
            let installation_id = installed_list_cursor_installation_id(cursor)?;
            list_query = list_query
                .bind(installed_at.clone())
                .bind(installed_at)
                .bind(installation_id);
        }
        let rows = list_query
            .bind(installed_list_query_limit(limit))
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;

        let mut installations = Vec::with_capacity(rows.len());
        for row in rows {
            installations.push(installation_response_from_row(row)?);
        }
        let has_more = installations.len() > limit as usize;
        if has_more {
            installations.truncate(limit as usize);
        }
        let next_cursor = if has_more {
            installations
                .last()
                .map(installed_list_cursor_from_installation)
                .transpose()?
        } else {
            None
        };

        Ok(InstalledListResponse {
            installations,
            total: None,
            limit,
            next_cursor,
        })
    }

    async fn save_credential(
        &self,
        user_id: String,
        request: CredentialRequestData,
        encryptor: &FernetTokenEncryptor,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)> {
        let encrypted = encryptor
            .encrypt(&request.value)
            .map_err(|e| internal_error(format!("encryption failed: {}", e)))?;

        let pool = self.get_pool().await.map_err(internal_error)?;
        let credential_id = Uuid::new_v4().to_string();

        query(
            "INSERT INTO skill_user_credentials \
             (credential_id, user_id, skill_name, credential_name, value_encrypted, created_at) \
             VALUES (?, ?, ?, ?, ?, NOW())",
        )
        .bind(&credential_id)
        .bind(&user_id)
        .bind(&request.skill_name)
        .bind(&request.credential_name)
        .bind(&encrypted)
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        Ok(StatusResponse {
            status: "saved".into(),
        })
    }

    async fn delete_credential(
        &self,
        user_id: String,
        skill_name: String,
        credential_name: String,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        query(
            "DELETE FROM skill_user_credentials \
             WHERE user_id = ? AND skill_name = ? AND credential_name = ?",
        )
        .bind(&user_id)
        .bind(&skill_name)
        .bind(&credential_name)
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        Ok(StatusResponse {
            status: "deleted".into(),
        })
    }

    async fn publish_skill(
        &self,
        _user_id: String,
        skill_name: String,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        query(
            "UPDATE skills_registry SET status = 'published', is_active = 1 WHERE skill_name = ?",
        )
        .bind(&skill_name)
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        Ok(StatusResponse {
            status: "published".into(),
        })
    }

    async fn deprecate_skill(
        &self,
        _user_id: String,
        skill_name: String,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        query(
            "UPDATE skills_registry SET status = 'deprecated', is_active = 0 WHERE skill_name = ?",
        )
        .bind(&skill_name)
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        Ok(StatusResponse {
            status: "deprecated".into(),
        })
    }
}

// ── Noop implementation ──────────────────────────────────────────────────────

pub struct UnconfiguredMarketplaceService;

#[async_trait]
impl MarketplaceService for UnconfiguredMarketplaceService {
    async fn install_skill(
        &self,
        _: String,
        _: InstallRequestData,
    ) -> Result<InstallationResponse, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("marketplace service not configured"))
    }
    async fn uninstall_skill(
        &self,
        _: String,
        _: InstallRequestData,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("marketplace service not configured"))
    }
    async fn upgrade_skill(
        &self,
        _: String,
        _: InstallRequestData,
    ) -> Result<InstallationResponse, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("marketplace service not configured"))
    }
    async fn rollback_skill(
        &self,
        _: String,
        _: InstallRequestData,
    ) -> Result<InstallationResponse, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("marketplace service not configured"))
    }
    async fn list_installed(
        &self,
        _: String,
        _: i64,
        _: Option<InstalledListCursor>,
    ) -> Result<InstalledListResponse, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("marketplace service not configured"))
    }
    async fn save_credential(
        &self,
        _: String,
        _: CredentialRequestData,
        _: &FernetTokenEncryptor,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("marketplace service not configured"))
    }
    async fn delete_credential(
        &self,
        _: String,
        _: String,
        _: String,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("marketplace service not configured"))
    }
    async fn publish_skill(
        &self,
        _: String,
        _: String,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("marketplace service not configured"))
    }
    async fn deprecate_skill(
        &self,
        _: String,
        _: String,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("marketplace service not configured"))
    }
}

// ── HTTP types ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct InstallRequest {
    pub skill_name: String,
}

#[derive(Deserialize)]
pub struct CredentialRequest {
    pub skill_name: String,
    pub credential_name: String,
    pub value: String,
}

#[derive(Deserialize)]
pub struct DeleteCredentialQuery {
    pub skill_name: String,
    pub credential_name: String,
}

#[derive(Deserialize)]
pub struct ListInstalledQuery {
    #[serde(default = "default_limit")]
    pub limit: i64,
    pub after_installed_at: Option<String>,
    pub after_installation_id: Option<String>,
}

fn default_limit() -> i64 {
    50
}

impl ListInstalledQuery {
    pub fn cursor(&self) -> Result<Option<InstalledListCursor>, (StatusCode, Json<ErrorResponse>)> {
        match (&self.after_installed_at, &self.after_installation_id) {
            (None, None) => Ok(None),
            (Some(installed_at), Some(installation_id)) => Ok(Some(InstalledListCursor {
                installed_at: installed_at.clone(),
                installation_id: installation_id.clone(),
            })),
            _ => Err(error_response(
                StatusCode::BAD_REQUEST,
                "installed list cursor requires both after_installed_at and after_installation_id",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_installed_query_default_limit() {
        let q: ListInstalledQuery = serde_json::from_str("{}").unwrap();
        assert_eq!(q.limit, 50);
        assert_eq!(q.cursor().unwrap(), None);
    }

    #[test]
    fn list_installed_query_explicit_limit_overrides() {
        let q: ListInstalledQuery = serde_json::from_str(r#"{"limit": 10}"#).unwrap();
        assert_eq!(q.limit, 10);
    }

    #[test]
    fn list_installed_query_rejects_null_limit() {
        assert!(serde_json::from_str::<ListInstalledQuery>(r#"{"limit": null}"#).is_err());
    }

    #[test]
    fn list_installed_query_requires_complete_cursor() {
        let q: ListInstalledQuery =
            serde_json::from_str(r#"{"after_installed_at":"2026-04-01T10:00:00.123456"}"#).unwrap();
        assert_eq!(q.cursor().unwrap_err().0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn installed_list_limit_has_hard_cap_and_minimum() {
        assert_eq!(validate_installed_list_limit(0), 1);
        assert_eq!(validate_installed_list_limit(10), 10);
        assert_eq!(
            validate_installed_list_limit(i64::MAX),
            MAX_INSTALLED_LIST_ROWS
        );
        assert_eq!(installed_list_query_limit(MAX_INSTALLED_LIST_ROWS), 201);
    }

    #[test]
    fn installed_list_cursor_rejects_invalid_inputs() {
        let cursor = InstalledListCursor {
            installed_at: "2026-04-01T10:00:00.123456".to_string(),
            installation_id: "inst-1".to_string(),
        };
        assert_eq!(
            installed_list_cursor_db_installed_at(&cursor).unwrap(),
            "2026-04-01 10:00:00.123456"
        );
        assert_eq!(
            installed_list_cursor_installation_id(&cursor).unwrap(),
            "inst-1".to_string()
        );

        let invalid_time = InstalledListCursor {
            installed_at: "2026-04-01T10:00:00".to_string(),
            installation_id: "inst-1".to_string(),
        };
        assert_eq!(
            installed_list_cursor_db_installed_at(&invalid_time)
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );

        let missing_id = InstalledListCursor {
            installed_at: "2026-04-01T10:00:00.123456".to_string(),
            installation_id: "  ".to_string(),
        };
        assert_eq!(
            installed_list_cursor_installation_id(&missing_id)
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn installed_list_sql_contract_uses_seek_cursor_not_offset() {
        let sql = format!(
            "SELECT {INSTALLED_LIST_SELECT} FROM skill_installations WHERE user_id = ? \
             AND (installed_at < ? OR (installed_at = ? AND installation_id < ?)) \
             ORDER BY installed_at DESC, installation_id DESC LIMIT ?"
        );
        assert!(!sql.to_ascii_uppercase().contains(" OFFSET "));
        assert!(sql.contains("installed_at < ?"));
        assert!(sql.contains("installation_id < ?"));
    }
}
