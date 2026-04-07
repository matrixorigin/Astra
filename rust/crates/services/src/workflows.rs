use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{Row, query};

use astra_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};

// ── Data types ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkflowDefRecord {
    pub workflow_id: String,
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub definition: serde_json::Value,
    pub is_active: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkflowListItem {
    pub workflow_id: String,
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub is_active: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkflowRunRecord {
    pub run_id: String,
    pub workflow_id: String,
    pub agent_run_id: Option<String>,
    pub status: String,
    pub waiting_for: Option<String>,
    pub current_step_idx: i32,
    pub step_results: serde_json::Value,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowResolveData {
    pub result: serde_json::Value,
}

// ── Trait ─────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait WorkflowService: Send + Sync {
    async fn list_workflows(
        &self,
    ) -> Result<Vec<WorkflowListItem>, (StatusCode, Json<ErrorResponse>)>;

    async fn get_workflow(
        &self,
        workflow_id: String,
    ) -> Result<WorkflowDefRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn get_workflow_run(
        &self,
        run_id: String,
    ) -> Result<WorkflowRunRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn resolve_workflow_wait(
        &self,
        run_id: String,
        result: serde_json::Value,
    ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)>;
}

// ── Database implementation ──────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct DatabaseWorkflowService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseWorkflowService {
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

    fn workflow_record_from_row(
        row: sqlx::mysql::MySqlRow,
    ) -> Result<WorkflowDefRecord, (StatusCode, Json<ErrorResponse>)> {
        let def_json: String = row
            .try_get("definition_json")
            .unwrap_or_else(|_| "{}".into());
        Ok(WorkflowDefRecord {
            workflow_id: row.try_get("workflow_id").map_err(internal_error)?,
            name: row.try_get("name").map_err(internal_error)?,
            version: row.try_get("version").map_err(internal_error)?,
            description: row.try_get("description").ok(),
            definition: serde_json::from_str(&def_json).unwrap_or(serde_json::json!({})),
            is_active: row.try_get::<i16, _>("is_active").unwrap_or(1) != 0,
        })
    }
}

const WORKFLOW_DETAIL_SELECT_COLS: &str = "\
    workflow_id, name, version, description, \
    IFNULL(CAST(definition AS CHAR), '{}') AS definition_json, is_active";
const WORKFLOW_LIST_SELECT_COLS: &str = "\
    workflow_id, name, version, description, is_active";
const MAX_WORKFLOW_LIST_ROWS: i64 = 200;

#[async_trait]
impl WorkflowService for DatabaseWorkflowService {
    async fn list_workflows(
        &self,
    ) -> Result<Vec<WorkflowListItem>, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let sql = format!(
            "SELECT {} FROM wf_definitions WHERE is_active = 1 ORDER BY name LIMIT ?",
            WORKFLOW_LIST_SELECT_COLS
        );
        let rows = query(&sql)
            .bind(MAX_WORKFLOW_LIST_ROWS)
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;

        let mut workflows = Vec::with_capacity(rows.len());
        for row in rows {
            workflows.push(WorkflowListItem {
                workflow_id: row.try_get("workflow_id").map_err(internal_error)?,
                name: row.try_get("name").map_err(internal_error)?,
                version: row.try_get("version").map_err(internal_error)?,
                description: row.try_get("description").ok(),
                is_active: row.try_get::<i16, _>("is_active").unwrap_or(1) != 0,
            });
        }
        Ok(workflows)
    }

    async fn get_workflow(
        &self,
        workflow_id: String,
    ) -> Result<WorkflowDefRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let sql = format!(
            "SELECT {} FROM wf_definitions WHERE workflow_id = ? LIMIT 1",
            WORKFLOW_DETAIL_SELECT_COLS
        );
        let row = query(&sql)
            .bind(&workflow_id)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?;
        let row = row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Workflow {} not found", workflow_id),
            )
        })?;
        Self::workflow_record_from_row(row)
    }

    async fn get_workflow_run(
        &self,
        run_id: String,
    ) -> Result<WorkflowRunRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let row = query(
            "SELECT run_id, workflow_id, agent_run_id, status, waiting_for, \
             IFNULL(current_step_idx, 0) AS current_step_idx, \
             IFNULL(CAST(step_results AS CHAR), '{}') AS step_results_json, error \
             FROM wf_runs WHERE run_id = ?",
        )
        .bind(&run_id)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?;

        let row =
            row.ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Workflow run not found"))?;
        let sr_json: String = row
            .try_get("step_results_json")
            .unwrap_or_else(|_| "{}".into());
        Ok(WorkflowRunRecord {
            run_id: row.try_get("run_id").map_err(internal_error)?,
            workflow_id: row.try_get("workflow_id").map_err(internal_error)?,
            agent_run_id: row.try_get("agent_run_id").ok(),
            status: row.try_get("status").map_err(internal_error)?,
            waiting_for: row.try_get("waiting_for").ok(),
            current_step_idx: row.try_get("current_step_idx").unwrap_or(0),
            step_results: serde_json::from_str(&sr_json).unwrap_or(serde_json::json!({})),
            error: row.try_get("error").ok(),
        })
    }

    async fn resolve_workflow_wait(
        &self,
        run_id: String,
        _result: serde_json::Value,
    ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let row = query("SELECT status, waiting_for FROM wf_runs WHERE run_id = ?")
            .bind(&run_id)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?;
        let row = row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                "Workflow run not found or not waiting",
            )
        })?;

        let status: String = row.try_get("status").map_err(internal_error)?;
        if status != "waiting" {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                "Workflow run not found or not waiting",
            ));
        }
        let waiting_for: Option<String> = row.try_get("waiting_for").ok();
        if waiting_for.is_none() {
            return Err(error_response(StatusCode::BAD_REQUEST, "No wait handle"));
        }

        query("UPDATE wf_runs SET status = 'running', waiting_for = NULL, updated_at = NOW() WHERE run_id = ?")
            .bind(&run_id)
            .execute(&pool)
            .await
            .map_err(internal_error)?;

        Ok(serde_json::json!({"run_id": run_id, "status": "resumed"}))
    }
}

// ── Noop implementation ──────────────────────────────────────────────────────

pub struct UnconfiguredWorkflowService;

#[async_trait]
impl WorkflowService for UnconfiguredWorkflowService {
    async fn list_workflows(
        &self,
    ) -> Result<Vec<WorkflowListItem>, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("workflow service not configured"))
    }
    async fn get_workflow(
        &self,
        _: String,
    ) -> Result<WorkflowDefRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("workflow service not configured"))
    }
    async fn get_workflow_run(
        &self,
        _: String,
    ) -> Result<WorkflowRunRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("workflow service not configured"))
    }
    async fn resolve_workflow_wait(
        &self,
        _: String,
        _: serde_json::Value,
    ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("workflow service not configured"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_def_record_round_trip() {
        let rec = WorkflowDefRecord {
            workflow_id: "w1".into(),
            name: "test-wf".into(),
            version: "1.0.0".into(),
            description: Some("test workflow".into()),
            definition: serde_json::json!({"steps": []}),
            is_active: true,
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: WorkflowDefRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(rec, back);
    }

    #[test]
    fn workflow_run_record_round_trip() {
        let rec = WorkflowRunRecord {
            run_id: "r1".into(),
            workflow_id: "w1".into(),
            agent_run_id: None,
            status: "running".into(),
            waiting_for: Some("approval".into()),
            current_step_idx: 2,
            step_results: serde_json::json!([]),
            error: None,
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: WorkflowRunRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(rec, back);
    }

    #[test]
    fn workflow_list_item_none_description() {
        let item = WorkflowListItem {
            workflow_id: "w1".into(),
            name: "wf".into(),
            version: "1.0".into(),
            description: None,
            is_active: false,
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains(r#""description":null"#));
        let back: WorkflowListItem = serde_json::from_str(&json).unwrap();
        assert_eq!(item, back);
    }
}
