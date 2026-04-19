use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{Row, query};
use uuid::Uuid;

use astra_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmTrustedDomainRecord {
    pub domain_id: String,
    pub domain_host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain_port: Option<u16>,
    pub is_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LlmTrustedDomainUpsertRequestData {
    pub domain_host: String,
    pub domain_port: Option<u16>,
    pub is_enabled: bool,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmTrustedDomainDeleteResponse {
    pub status: String,
}

#[derive(Debug, Deserialize)]
pub struct LlmTrustedDomainUpsertRequest {
    pub domain_host: String,
    #[serde(default)]
    pub domain_port: Option<u16>,
    #[serde(default = "default_enabled")]
    pub is_enabled: bool,
    #[serde(default)]
    pub description: Option<String>,
}

fn default_enabled() -> bool {
    true
}

#[async_trait]
pub trait LlmTrustedDomainService: Send + Sync {
    async fn list_trusted_domains(
        &self,
    ) -> Result<Vec<LlmTrustedDomainRecord>, (StatusCode, Json<ErrorResponse>)>;

    async fn upsert_trusted_domain(
        &self,
        updated_by: Option<&str>,
        request: LlmTrustedDomainUpsertRequestData,
    ) -> Result<LlmTrustedDomainRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn delete_trusted_domain(
        &self,
        domain_id: &str,
    ) -> Result<LlmTrustedDomainDeleteResponse, (StatusCode, Json<ErrorResponse>)>;
}

#[derive(Clone, Debug)]
pub struct DatabaseLlmTrustedDomainService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseLlmTrustedDomainService {
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
        if let Some(ref pool) = self.pool {
            return Ok(pool.get().clone());
        }
        connect_matrixone(&self.matrixone).await
    }

    fn normalize_domain_host(raw: &str) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "domain_host must not be empty",
            ));
        }
        if trimmed.contains("://") {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "domain_host must be host only (without URL scheme)",
            ));
        }
        let parsed = reqwest::Url::parse(&format!("http://{trimmed}")).map_err(|error| {
            error_response(
                StatusCode::BAD_REQUEST,
                format!("domain_host is invalid: {error}"),
            )
        })?;
        if parsed.username() != ""
            || parsed.password().is_some()
            || parsed.path() != "/"
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || parsed.port().is_some()
        {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "domain_host must be host only (without path/query/port)",
            ));
        }
        let Some(host) = parsed.host_str() else {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "domain_host must include a host",
            ));
        };
        Ok(host.to_ascii_lowercase())
    }

    fn map_row(row: &sqlx::mysql::MySqlRow) -> Result<LlmTrustedDomainRecord, sqlx::Error> {
        let domain_port_raw: Option<i64> = row.try_get("domain_port")?;
        let domain_port = if let Some(raw) = domain_port_raw {
            if (1..=65_535).contains(&raw) {
                Some(raw as u16)
            } else {
                None
            }
        } else {
            None
        };
        let is_enabled: i16 = row.try_get("is_enabled")?;
        Ok(LlmTrustedDomainRecord {
            domain_id: row.try_get("domain_id")?,
            domain_host: row.try_get("domain_host")?,
            domain_port,
            is_enabled: is_enabled == 1,
            description: row.try_get("description")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }

    async fn fetch_by_id(
        &self,
        pool: &sqlx::Pool<sqlx::MySql>,
        domain_id: &str,
    ) -> Result<LlmTrustedDomainRecord, (StatusCode, Json<ErrorResponse>)> {
        let row = query(
            "SELECT domain_id, domain_host, domain_port, is_enabled, description, \
                    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS created_at, \
                    DATE_FORMAT(updated_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS updated_at \
             FROM runtime_llm_trusted_domains \
             WHERE domain_id = ?",
        )
        .bind(domain_id)
        .fetch_optional(pool)
        .await
        .map_err(internal_error)?;
        let row = row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("trusted domain '{domain_id}' not found"),
            )
        })?;
        Self::map_row(&row).map_err(internal_error)
    }
}

#[async_trait]
impl LlmTrustedDomainService for DatabaseLlmTrustedDomainService {
    async fn list_trusted_domains(
        &self,
    ) -> Result<Vec<LlmTrustedDomainRecord>, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let rows = query(
            "SELECT domain_id, domain_host, domain_port, is_enabled, description, \
                    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS created_at, \
                    DATE_FORMAT(updated_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS updated_at \
             FROM runtime_llm_trusted_domains \
             ORDER BY domain_host ASC, domain_port ASC, domain_id ASC",
        )
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;
        rows.iter()
            .map(Self::map_row)
            .collect::<Result<Vec<_>, _>>()
            .map_err(internal_error)
    }

    async fn upsert_trusted_domain(
        &self,
        updated_by: Option<&str>,
        request: LlmTrustedDomainUpsertRequestData,
    ) -> Result<LlmTrustedDomainRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let domain_host = Self::normalize_domain_host(&request.domain_host)?;
        if request.domain_port == Some(0) {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "domain_port must be between 1 and 65535 when provided",
            ));
        }
        let domain_port = request.domain_port.map(i64::from);
        let description = request
            .description
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let is_enabled: i16 = if request.is_enabled { 1 } else { 0 };
        let updated_by = updated_by
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(String::from);

        let existing = query(
            "SELECT domain_id \
             FROM runtime_llm_trusted_domains \
             WHERE domain_host = ? AND domain_port <=> ? \
             LIMIT 1",
        )
        .bind(&domain_host)
        .bind(domain_port)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?;

        let domain_id = if let Some(row) = existing {
            let domain_id: String = row.try_get("domain_id").map_err(internal_error)?;
            query(
                "UPDATE runtime_llm_trusted_domains \
                 SET is_enabled = ?, description = ?, updated_by = ? \
                 WHERE domain_id = ?",
            )
            .bind(is_enabled)
            .bind(&description)
            .bind(&updated_by)
            .bind(&domain_id)
            .execute(&pool)
            .await
            .map_err(internal_error)?;
            domain_id
        } else {
            let domain_id = Uuid::new_v4().to_string();
            query(
                "INSERT INTO runtime_llm_trusted_domains \
                 (domain_id, domain_host, domain_port, is_enabled, description, created_by, updated_by) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&domain_id)
            .bind(&domain_host)
            .bind(domain_port)
            .bind(is_enabled)
            .bind(&description)
            .bind(&updated_by)
            .bind(&updated_by)
            .execute(&pool)
            .await
            .map_err(internal_error)?;
            domain_id
        };

        self.fetch_by_id(&pool, &domain_id).await
    }

    async fn delete_trusted_domain(
        &self,
        domain_id: &str,
    ) -> Result<LlmTrustedDomainDeleteResponse, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let domain_id = domain_id.trim();
        if domain_id.is_empty() {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "domain_id must not be empty",
            ));
        }
        let result = query("DELETE FROM runtime_llm_trusted_domains WHERE domain_id = ?")
            .bind(domain_id)
            .execute(&pool)
            .await
            .map_err(internal_error)?;
        if result.rows_affected() == 0 {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!("trusted domain '{domain_id}' not found"),
            ));
        }
        Ok(LlmTrustedDomainDeleteResponse {
            status: "deleted".to_string(),
        })
    }
}

pub struct UnconfiguredLlmTrustedDomainService;

#[async_trait]
impl LlmTrustedDomainService for UnconfiguredLlmTrustedDomainService {
    async fn list_trusted_domains(
        &self,
    ) -> Result<Vec<LlmTrustedDomainRecord>, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("llm trusted domain service not configured"))
    }

    async fn upsert_trusted_domain(
        &self,
        _: Option<&str>,
        _: LlmTrustedDomainUpsertRequestData,
    ) -> Result<LlmTrustedDomainRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("llm trusted domain service not configured"))
    }

    async fn delete_trusted_domain(
        &self,
        _: &str,
    ) -> Result<LlmTrustedDomainDeleteResponse, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("llm trusted domain service not configured"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_request_defaults_enabled() {
        let req: LlmTrustedDomainUpsertRequest =
            serde_json::from_str(r#"{"domain_host":"catalog"}"#).expect("request should parse");
        assert!(req.is_enabled);
        assert_eq!(req.domain_port, None);
    }
}
