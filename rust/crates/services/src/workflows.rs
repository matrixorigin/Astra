use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};

use astra_core::{ErrorResponse, internal_error};

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

pub struct UnconfiguredWorkflowService;

#[async_trait]
impl WorkflowService for UnconfiguredWorkflowService {
    async fn list_workflows(
        &self,
    ) -> Result<Vec<WorkflowListItem>, (StatusCode, Json<ErrorResponse>)> {
        Ok(Vec::new())
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
            name: "workflow".into(),
            version: "1".into(),
            description: None,
            is_active: true,
        };
        let json = serde_json::to_string(&item).unwrap();
        let back: WorkflowListItem = serde_json::from_str(&json).unwrap();
        assert_eq!(item, back);
    }
}
