use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{Row, query};

use astra_core::{ErrorResponse, MatrixOneSettings, SharedPool, error_response, internal_error};

const MAX_SANDBOX_LIST_ROWS: i64 = 200;

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
    pub status: String,
    pub user_id: String,
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
        crate::require_shared_pool(
            self.pool.as_ref(),
            "DatabaseSandboxService",
            &self.matrixone,
        )
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

        self.get_sandbox(request.name, user_id).await
    }

    async fn list_sandboxes(
        &self,
        user_id: String,
        pattern: Option<String>,
    ) -> Result<Vec<SandboxRecord>, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let pat = pattern.unwrap_or_else(|| "%".into());
        let rows = query(
            "SELECT sandbox_name, description, created_by, user_id, status, \
              DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
              FROM infra_sandbox_metadata \
              WHERE user_id = ? AND sandbox_name LIKE ? \
              ORDER BY created_at DESC LIMIT ?",
        )
        .bind(&user_id)
        .bind(&pat)
        .bind(MAX_SANDBOX_LIST_ROWS)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        rows.into_iter().map(sandbox_record_from_row).collect()
    }

    async fn get_sandbox(
        &self,
        name: String,
        user_id: String,
    ) -> Result<SandboxRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let row = query(
            "SELECT sandbox_name, description, created_by, user_id, status, \
              DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
              FROM infra_sandbox_metadata WHERE sandbox_name = ? AND user_id = ?",
        )
        .bind(&name)
        .bind(&user_id)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?;

        let row = row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Sandbox '{}' not found", name),
            )
        })?;
        sandbox_record_from_row(row)
    }

    async fn delete_sandbox(
        &self,
        name: String,
        user_id: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let existing =
            query("SELECT sandbox_name FROM infra_sandbox_metadata WHERE sandbox_name = ? AND user_id = ?")
                .bind(&name)
                .bind(&user_id)
                .fetch_optional(&pool)
                .await
                .map_err(internal_error)?;
        if existing.is_none() {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!("Sandbox '{}' not found", name),
            ));
        }

        query("DELETE FROM infra_sandbox_metadata WHERE sandbox_name = ? AND user_id = ?")
            .bind(&name)
            .bind(&user_id)
            .execute(&pool)
            .await
            .map_err(internal_error)?;
        Ok(())
    }
}

fn sandbox_record_from_row(
    row: sqlx::mysql::MySqlRow,
) -> Result<SandboxRecord, (StatusCode, Json<ErrorResponse>)> {
    let status = required_sandbox_string(&row, "status")?;
    if status != "active" {
        return Err(internal_error(format!(
            "invalid infra_sandbox_metadata.status: {status}"
        )));
    }
    Ok(SandboxRecord {
        sandbox_name: required_sandbox_string(&row, "sandbox_name")?,
        description: required_sandbox_string(&row, "description")?,
        created_by: required_sandbox_string(&row, "created_by")?,
        created_at: required_sandbox_string(&row, "created_at")?,
        status,
        user_id: required_sandbox_string(&row, "user_id")?,
    })
}

fn required_sandbox_string(
    row: &sqlx::mysql::MySqlRow,
    column: &'static str,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    row.try_get::<String, _>(column).map_err(|error| {
        internal_error(format!("invalid infra_sandbox_metadata.{column}: {error}"))
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_record_includes_required_fields() {
        let rec = SandboxRecord {
            sandbox_name: "sb1".into(),
            description: "test".into(),
            created_by: "u1".into(),
            created_at: "now".into(),
            status: "active".into(),
            user_id: "u1".into(),
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert!(json.contains("status"));
        assert!(json.contains("user_id"));
    }

    #[test]
    fn create_sandbox_request_default_description() {
        let req: CreateSandboxRequest = serde_json::from_str(r#"{"name":"test"}"#).unwrap();
        assert_eq!(req.name, "test");
        assert_eq!(req.description, "");
    }

    #[test]
    fn sandbox_record_serde_round_trip() {
        let rec = SandboxRecord {
            sandbox_name: "sb1".into(),
            description: "desc".into(),
            created_by: "u1".into(),
            created_at: "2024-01-01".into(),
            status: "active".into(),
            user_id: "u1".into(),
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: SandboxRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(rec, back);
    }
}
