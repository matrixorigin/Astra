use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{Row, query};

use mo_agent_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};

// ── Data types ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct SandboxCreateRequestData {
    pub name: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SandboxRecord {
    pub sandbox_name: String,
    pub description: String,
    pub created_by: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SandboxListResponse {
    pub sandboxes: Vec<SandboxRecord>,
    pub total: usize,
}

// ── Trait ─────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait SandboxService: Send + Sync {
    async fn create_sandbox(
        &self,
        user_id: String,
        request: SandboxCreateRequestData,
    ) -> Result<SandboxRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn list_sandboxes(
        &self,
        user_id: String,
        pattern: Option<String>,
    ) -> Result<Vec<SandboxRecord>, (StatusCode, Json<ErrorResponse>)>;

    async fn get_sandbox(
        &self,
        name: String,
        user_id: String,
    ) -> Result<SandboxRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn delete_sandbox(
        &self,
        name: String,
        user_id: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)>;
}

// ── Database implementation ──────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct DatabaseSandboxService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseSandboxService {
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
impl SandboxService for DatabaseSandboxService {
    async fn create_sandbox(
        &self,
        user_id: String,
        request: SandboxCreateRequestData,
    ) -> Result<SandboxRecord, (StatusCode, Json<ErrorResponse>)> {
        if request.name.trim().is_empty() {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "Sandbox name cannot be empty",
            ));
        }

        let pool = self.get_pool().await.map_err(internal_error)?;

        let existing =
            query("SELECT sandbox_name FROM infra_sandbox_metadata WHERE sandbox_name = ?")
                .bind(&request.name)
                .fetch_optional(&pool)
                .await
                .map_err(internal_error)?;
        if existing.is_some() {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                format!("Sandbox '{}' already exists", request.name),
            ));
        }

        query(
            "INSERT INTO infra_sandbox_metadata \
             (sandbox_name, user_id, description, created_by, created_at, updated_at, status) \
             VALUES (?, ?, ?, ?, NOW(), NOW(), 'active')",
        )
        .bind(&request.name)
        .bind(&user_id)
        .bind(&request.description)
        .bind(&user_id)
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        Ok(SandboxRecord {
            sandbox_name: request.name,
            description: request.description,
            created_by: user_id.clone(),
            created_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
            status: Some("active".into()),
            user_id: Some(user_id),
        })
    }

    async fn list_sandboxes(
        &self,
        _user_id: String,
        pattern: Option<String>,
    ) -> Result<Vec<SandboxRecord>, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let pat = pattern.unwrap_or_else(|| "%".into());
        let rows = query(
            "SELECT sandbox_name, IFNULL(description, '') AS description, \
             IFNULL(created_by, '') AS created_by, user_id, status, \
             DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM infra_sandbox_metadata WHERE sandbox_name LIKE ? ORDER BY created_at DESC",
        )
        .bind(&pat)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let mut sandboxes = Vec::with_capacity(rows.len());
        for row in rows {
            sandboxes.push(SandboxRecord {
                sandbox_name: row.try_get("sandbox_name").map_err(internal_error)?,
                description: row.try_get("description").unwrap_or_default(),
                created_by: row.try_get("created_by").unwrap_or_default(),
                created_at: row.try_get("created_at").unwrap_or_default(),
                status: row.try_get("status").ok(),
                user_id: row.try_get("user_id").ok(),
            });
        }
        Ok(sandboxes)
    }

    async fn get_sandbox(
        &self,
        name: String,
        _user_id: String,
    ) -> Result<SandboxRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let row = query(
            "SELECT sandbox_name, IFNULL(description, '') AS description, \
             IFNULL(created_by, '') AS created_by, user_id, status, \
             DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM infra_sandbox_metadata WHERE sandbox_name = ?",
        )
        .bind(&name)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?;

        let row = row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Sandbox '{}' not found", name),
            )
        })?;
        Ok(SandboxRecord {
            sandbox_name: row.try_get("sandbox_name").map_err(internal_error)?,
            description: row.try_get("description").unwrap_or_default(),
            created_by: row.try_get("created_by").unwrap_or_default(),
            created_at: row.try_get("created_at").unwrap_or_default(),
            status: row.try_get("status").ok(),
            user_id: row.try_get("user_id").ok(),
        })
    }

    async fn delete_sandbox(
        &self,
        name: String,
        _user_id: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let existing =
            query("SELECT sandbox_name FROM infra_sandbox_metadata WHERE sandbox_name = ?")
                .bind(&name)
                .fetch_optional(&pool)
                .await
                .map_err(internal_error)?;
        if existing.is_none() {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!("Sandbox '{}' not found", name),
            ));
        }

        query("DELETE FROM infra_sandbox_metadata WHERE sandbox_name = ?")
            .bind(&name)
            .execute(&pool)
            .await
            .map_err(internal_error)?;
        Ok(())
    }
}

// ── Noop implementation ──────────────────────────────────────────────────────

pub struct UnconfiguredSandboxService;

#[async_trait]
impl SandboxService for UnconfiguredSandboxService {
    async fn create_sandbox(
        &self,
        _: String,
        _: SandboxCreateRequestData,
    ) -> Result<SandboxRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("sandbox service not configured"))
    }
    async fn list_sandboxes(
        &self,
        _: String,
        _: Option<String>,
    ) -> Result<Vec<SandboxRecord>, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("sandbox service not configured"))
    }
    async fn get_sandbox(
        &self,
        _: String,
        _: String,
    ) -> Result<SandboxRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("sandbox service not configured"))
    }
    async fn delete_sandbox(
        &self,
        _: String,
        _: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("sandbox service not configured"))
    }
}

// ── HTTP types ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateSandboxRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Deserialize)]
pub struct SandboxListQuery {
    pub pattern: Option<String>,
}
