use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sqlx::{Row, query};
use std::sync::LazyLock;

use astra_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};

// ── SafeIdent validation ─────────────────────────────────────────────────────

pub static SAFE_IDENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z_][a-zA-Z0-9_.]{0,127}$").expect("valid regex"));

pub fn validate_ident(name: &str, label: &str) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if !SAFE_IDENT_RE.is_match(name) {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("Invalid {label}: must match [a-zA-Z_][a-zA-Z0-9_.]{{0,127}}"),
        ));
    }
    Ok(())
}

// ── Data types ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct CreateBranchData {
    pub name: String,
    pub source: String,
    pub snapshot: Option<String>,
    pub is_database: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DiffData {
    pub target: String,
    pub source: String,
    pub target_snapshot: Option<String>,
    pub source_snapshot: Option<String>,
    pub output: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MergeData {
    pub source: String,
    pub target: String,
    pub on_conflict: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DeleteBranchData {
    pub name: String,
    pub is_database: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CostEstimateData {
    pub operation: String,
    pub model: String,
    pub session_count: Option<i64>,
    pub budget_remaining: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CreateBranchResponse {
    pub name: String,
    pub source: String,
    pub snapshot: String,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DiffResponse {
    pub rows: Vec<serde_json::Value>,
    pub count: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MergeResponse {
    pub status: String,
    pub source: String,
    pub target: String,
    pub rows_affected: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StatusResponse {
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CostEstimateResponse {
    pub operation: String,
    pub model: String,
    pub estimated_tokens: i64,
    pub estimated_cost: f64,
    pub exceeds_budget: bool,
    pub alternatives: Vec<String>,
}

// ── Trait ─────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait BranchService: Send + Sync {
    async fn create_branch(
        &self,
        user_id: String,
        request: CreateBranchData,
    ) -> Result<CreateBranchResponse, (StatusCode, Json<ErrorResponse>)>;

    async fn diff_branch(
        &self,
        user_id: String,
        request: DiffData,
    ) -> Result<DiffResponse, (StatusCode, Json<ErrorResponse>)>;

    async fn merge_branch(
        &self,
        user_id: String,
        request: MergeData,
    ) -> Result<MergeResponse, (StatusCode, Json<ErrorResponse>)>;

    async fn delete_branch(
        &self,
        user_id: String,
        request: DeleteBranchData,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)>;

    async fn estimate_cost(
        &self,
        request: CostEstimateData,
    ) -> Result<CostEstimateResponse, (StatusCode, Json<ErrorResponse>)>;
}

// ── Database implementation ──────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct DatabaseBranchService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseBranchService {
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
impl BranchService for DatabaseBranchService {
    async fn create_branch(
        &self,
        _user_id: String,
        request: CreateBranchData,
    ) -> Result<CreateBranchResponse, (StatusCode, Json<ErrorResponse>)> {
        validate_ident(&request.name, "branch name")?;
        validate_ident(&request.source, "source")?;

        let pool = self.get_pool().await.map_err(internal_error)?;

        let snapshot_name = request
            .snapshot
            .unwrap_or_else(|| format!("{}__snap", request.name));

        let sql = crate::snapshot_sql::create_snapshot_for_db_sql(
            &snapshot_name,
            &self.matrixone.database,
        );
        query(&sql).execute(&pool).await.map_err(internal_error)?;

        Ok(CreateBranchResponse {
            name: request.name,
            source: request.source,
            snapshot: snapshot_name,
            created_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        })
    }

    async fn diff_branch(
        &self,
        _user_id: String,
        request: DiffData,
    ) -> Result<DiffResponse, (StatusCode, Json<ErrorResponse>)> {
        validate_ident(&request.target, "target")?;
        validate_ident(&request.source, "source")?;

        let pool = self.get_pool().await.map_err(internal_error)?;

        let is_count = request.output.as_deref() == Some("count");

        if is_count {
            let sql = format!(
                "SELECT COUNT(*) AS cnt FROM mo_diff('{}', '{}')",
                request.source, request.target
            );
            let row = query(&sql)
                .fetch_optional(&pool)
                .await
                .map_err(internal_error)?;

            let count: i64 = row.map(|r| r.try_get("cnt").unwrap_or(0)).unwrap_or(0);

            Ok(DiffResponse {
                rows: Vec::new(),
                count,
            })
        } else {
            let sql = format!(
                "SELECT * FROM mo_diff('{}', '{}')",
                request.source, request.target
            );
            let rows = query(&sql).fetch_all(&pool).await.map_err(internal_error)?;

            let count = rows.len() as i64;
            let values: Vec<serde_json::Value> =
                rows.iter().map(|_row| serde_json::json!({})).collect();

            Ok(DiffResponse {
                rows: values,
                count,
            })
        }
    }

    async fn merge_branch(
        &self,
        _user_id: String,
        request: MergeData,
    ) -> Result<MergeResponse, (StatusCode, Json<ErrorResponse>)> {
        validate_ident(&request.source, "source")?;
        validate_ident(&request.target, "target")?;

        let on_conflict = request.on_conflict.as_deref().unwrap_or("error");
        if !["skip", "accept", "error"].contains(&on_conflict) {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "on_conflict must be 'skip', 'accept', or 'error'",
            ));
        }

        let pool = self.get_pool().await.map_err(internal_error)?;

        let account = crate::snapshot_sql::resolve_account_name(&pool)
            .await
            .map_err(internal_error)?;
        let sql = crate::snapshot_sql::restore_snapshot_db_sql(
            &request.source,
            &account,
            &self.matrixone.database,
        );
        let result = query(&sql).execute(&pool).await.map_err(internal_error)?;

        Ok(MergeResponse {
            status: "merged".into(),
            source: request.source,
            target: request.target,
            rows_affected: result.rows_affected() as i64,
        })
    }

    async fn delete_branch(
        &self,
        _user_id: String,
        request: DeleteBranchData,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)> {
        validate_ident(&request.name, "branch name")?;

        let pool = self.get_pool().await.map_err(internal_error)?;

        let sql = format!("DROP SNAPSHOT IF EXISTS `{}`", request.name);
        query(&sql).execute(&pool).await.map_err(internal_error)?;

        Ok(StatusResponse {
            status: "deleted".into(),
        })
    }

    async fn estimate_cost(
        &self,
        request: CostEstimateData,
    ) -> Result<CostEstimateResponse, (StatusCode, Json<ErrorResponse>)> {
        let session_count = request.session_count.unwrap_or(1);
        let budget_remaining = request.budget_remaining.unwrap_or(10.0);

        let estimated_tokens: i64 = session_count * 1000;
        let estimated_cost: f64 = estimated_tokens as f64 * 0.00001;
        let exceeds_budget = estimated_cost > budget_remaining;

        let alternatives = if exceeds_budget {
            vec!["Use a smaller model".into(), "Reduce session count".into()]
        } else {
            Vec::new()
        };

        Ok(CostEstimateResponse {
            operation: request.operation,
            model: request.model,
            estimated_tokens,
            estimated_cost,
            exceeds_budget,
            alternatives,
        })
    }
}

// ── Noop implementation ──────────────────────────────────────────────────────

pub struct UnconfiguredBranchService;

#[async_trait]
impl BranchService for UnconfiguredBranchService {
    async fn create_branch(
        &self,
        _: String,
        _: CreateBranchData,
    ) -> Result<CreateBranchResponse, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("branch service not configured"))
    }
    async fn diff_branch(
        &self,
        _: String,
        _: DiffData,
    ) -> Result<DiffResponse, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("branch service not configured"))
    }
    async fn merge_branch(
        &self,
        _: String,
        _: MergeData,
    ) -> Result<MergeResponse, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("branch service not configured"))
    }
    async fn delete_branch(
        &self,
        _: String,
        _: DeleteBranchData,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("branch service not configured"))
    }
    async fn estimate_cost(
        &self,
        _: CostEstimateData,
    ) -> Result<CostEstimateResponse, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("branch service not configured"))
    }
}

// ── HTTP types ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateBranchRequest {
    pub name: String,
    pub source: String,
    pub snapshot: Option<String>,
    pub is_database: Option<bool>,
}

#[derive(Deserialize)]
pub struct DiffRequest {
    pub target: String,
    pub source: String,
    pub target_snapshot: Option<String>,
    pub source_snapshot: Option<String>,
    pub output: Option<String>,
}

#[derive(Deserialize)]
pub struct MergeRequest {
    pub source: String,
    pub target: String,
    pub on_conflict: Option<String>,
}

#[derive(Deserialize)]
pub struct DeleteBranchRequest {
    pub name: String,
    pub is_database: Option<bool>,
}

#[derive(Deserialize)]
pub struct CostEstimateRequest {
    pub operation: String,
    pub model: String,
    pub session_count: Option<i64>,
    pub budget_remaining: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_ident_valid_letter_start() {
        assert!(validate_ident("myBranch", "branch").is_ok());
    }

    #[test]
    fn validate_ident_valid_underscore_start() {
        assert!(validate_ident("_private", "branch").is_ok());
    }

    #[test]
    fn validate_ident_valid_with_dots_digits() {
        assert!(validate_ident("v1.2.3_beta", "version").is_ok());
    }

    #[test]
    fn validate_ident_empty() {
        assert!(validate_ident("", "branch").is_err());
    }

    #[test]
    fn validate_ident_starts_with_digit() {
        assert!(validate_ident("1abc", "branch").is_err());
    }

    #[test]
    fn validate_ident_special_chars() {
        assert!(validate_ident("my-branch", "branch").is_err());
        assert!(validate_ident("my@branch", "branch").is_err());
        assert!(validate_ident("my branch", "branch").is_err());
    }

    #[test]
    fn validate_ident_too_long() {
        let long = format!("a{}", "b".repeat(128)); // 129 chars
        assert!(validate_ident(&long, "branch").is_err());
    }

    #[test]
    fn validate_ident_max_length() {
        let max = format!("a{}", "b".repeat(127)); // 128 chars
        assert!(validate_ident(&max, "branch").is_ok());
    }

    #[test]
    fn validate_ident_single_char() {
        assert!(validate_ident("a", "branch").is_ok());
        assert!(validate_ident("_", "branch").is_ok());
    }
}
