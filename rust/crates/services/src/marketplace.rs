use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{Row, query};
use uuid::Uuid;

use crate::auth::FernetTokenEncryptor;
use astra_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};

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
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
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
        offset: i64,
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
        if let Some(ref p) = self.pool {
            return Ok(p.get().clone());
        }
        connect_matrixone(&self.matrixone).await
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
        let version: String = skill_row
            .try_get("version")
            .unwrap_or_else(|_| "1.0.0".into());

        let installation_id = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();

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

        Ok(InstallationResponse {
            installation_id,
            skill_name: request.skill_name,
            skill_version: version,
            status: "installed".into(),
            installed_at: now,
        })
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
        let old_version: String = current
            .try_get("skill_version")
            .unwrap_or_else(|_| "1.0.0".into());

        let latest =
            query("SELECT version FROM skills_registry WHERE skill_name = ? AND is_active = 1")
                .bind(&request.skill_name)
                .fetch_optional(&pool)
                .await
                .map_err(internal_error)?;

        let new_version: String = latest
            .and_then(|r| r.try_get("version").ok())
            .unwrap_or_else(|| old_version.clone());

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

        Ok(InstallationResponse {
            installation_id,
            skill_name: request.skill_name,
            skill_version: new_version,
            status: "upgraded".into(),
            installed_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        })
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
        let previous_version: Option<String> = current.try_get("previous_version").ok();
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

        Ok(InstallationResponse {
            installation_id,
            skill_name: request.skill_name,
            skill_version: previous_version,
            status: "rolled_back".into(),
            installed_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        })
    }

    async fn list_installed(
        &self,
        user_id: String,
        limit: i64,
        offset: i64,
    ) -> Result<InstalledListResponse, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let count_row = query("SELECT COUNT(*) AS cnt FROM skill_installations WHERE user_id = ?")
            .bind(&user_id)
            .fetch_one(&pool)
            .await
            .map_err(internal_error)?;
        let total: i64 = count_row.try_get("cnt").unwrap_or(0);

        let rows = query(
            "SELECT installation_id, skill_name, skill_version, status, \
             DATE_FORMAT(installed_at, '%Y-%m-%dT%H:%i:%s') AS installed_at \
             FROM skill_installations WHERE user_id = ? ORDER BY installed_at DESC LIMIT ? OFFSET ?"
        )
        .bind(&user_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let mut installations = Vec::with_capacity(rows.len());
        for row in rows {
            installations.push(InstallationResponse {
                installation_id: row.try_get("installation_id").map_err(internal_error)?,
                skill_name: row.try_get("skill_name").map_err(internal_error)?,
                skill_version: row.try_get("skill_version").unwrap_or_default(),
                status: row.try_get("status").unwrap_or_default(),
                installed_at: row.try_get("installed_at").unwrap_or_default(),
            });
        }

        Ok(InstalledListResponse {
            installations,
            total,
            limit,
            offset,
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
        _: i64,
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
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

fn default_limit() -> Option<i64> {
    Some(50)
}
