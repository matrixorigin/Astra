//! Cloud REST proxy for the durable `TaskService` trait.
//!
//! Astra is edge-cloud. The CLI MUST NOT connect to MatrixOne
//! directly — every `TaskService` trait call goes through the
//! server's `POST /agent-jobs:rpc` endpoint so the server owns
//! ownership checks, concurrency, and audit. This struct is the
//! CLI-side `TaskService` implementation that wraps each method
//! into an HTTP request.
//!
//! Used by `cli::session::session_runtime::resolve_task_service` when
//! `cloud_base` is configured. Offline / one-shot CLI falls back
//! to the in-memory `LocalTaskService`.

use astra_services::multi_agent::{
    LeaseClaimResult, NextClaimableLeaseClaimResult, TaskLeaseService, TaskLeaseView,
};
use astra_services::task_orchestrator::{
    LearningStats, TaskCheckpoint, TaskCreateRequest, TaskListItem, TaskOutcome, TaskPlan,
    TaskRecord, TaskService, TaskStatus, TemplateRecommendation,
};
use astra_thin_client::ASTRA_EDGE_ID_HEADER;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
struct JobRpcResponse {
    result: Value,
}

const TASK_HTTP_TIMEOUT_SECS: u64 = 15;

/// HTTP-backed `TaskService`. Stateless wrapper over a base URL and
/// optional bearer token; one client per CLI invocation. Token is
/// captured at construction; long-lived sessions should re-build via
/// `resolve_task_service` when the token rotates (or pass a closure
/// in a future iteration).
pub struct HttpTaskService {
    cloud_base: String,
    token: Option<String>,
}

impl HttpTaskService {
    pub fn new(cloud_base: impl Into<String>, token: Option<String>) -> Self {
        Self {
            cloud_base: cloud_base.into(),
            token,
        }
    }

    /// Send `{method, args}` to `/agent-jobs:rpc` and return the parsed
    /// `result` JSON. All RPC failures map to `Result<_, String>`
    /// matching the `TaskService` trait error type.
    async fn rpc(&self, method: &str, args: Value) -> Result<Value, String> {
        let url = format!("{}/agent-jobs:rpc", self.cloud_base.trim_end_matches('/'));
        let client = reqwest::Client::builder()
            .no_proxy() // astra server is local/intranet; bypass http_proxy env
            .timeout(std::time::Duration::from_secs(TASK_HTTP_TIMEOUT_SECS))
            .build()
            .map_err(|e| format!("http client init: {e}"))?;
        let mut req = client
            .post(&url)
            .json(&json!({ "method": method, "args": args }));
        if let Some(tok) = self.token.as_deref() {
            req = req.bearer_auth(tok);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("network ({method}): {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("cloud {status} ({method}): {body}"));
        }
        let parsed: JobRpcResponse = resp
            .json()
            .await
            .map_err(|e| format!("decode response ({method}): {e}"))?;
        Ok(parsed.result)
    }
}

#[async_trait]
impl TaskService for HttpTaskService {
    async fn create_task(
        &self,
        _user_id: &str,
        session_id: &str,
        req: TaskCreateRequest,
    ) -> Result<String, String> {
        // user_id ignored: server resolves it from the auth header.
        // Passing it would let a client forge tasks for another
        // user; the trait keeps the param for in-memory impls.
        let result = self
            .rpc(
                "create_task",
                json!({
                    "session_id": session_id,
                    "req": req,
                }),
            )
            .await?;
        result
            .get("task_id")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| "create_task: missing task_id in response".to_string())
    }

    async fn get_task(&self, task_id: &str) -> Result<Option<TaskRecord>, String> {
        let result = self.rpc("get_task", json!({ "task_id": task_id })).await?;
        if result.is_null() {
            return Ok(None);
        }
        let task: TaskRecord =
            serde_json::from_value(result).map_err(|e| format!("decode TaskRecord: {e}"))?;
        Ok(Some(task))
    }

    async fn list_recent_tasks(
        &self,
        _user_id: &str,
        status_filter: Option<TaskStatus>,
    ) -> Result<Vec<TaskListItem>, String> {
        let status_str = status_filter
            .map(|s| serde_json::to_value(s).unwrap_or_default())
            .and_then(|v| v.as_str().map(String::from));
        let mut args = json!({});
        if let Some(s) = status_str {
            args["status_filter"] = json!(s);
        }
        let result = self.rpc("list_recent_tasks", args).await?;
        let tasks: Vec<TaskListItem> =
            serde_json::from_value(result).map_err(|e| format!("decode Vec<TaskListItem>: {e}"))?;
        Ok(tasks)
    }

    async fn list_recent_tasks_for_session(
        &self,
        _user_id: &str,
        session_id: &str,
        status_filter: Option<TaskStatus>,
    ) -> Result<Vec<TaskListItem>, String> {
        let status_str = status_filter
            .map(|s| serde_json::to_value(s).unwrap_or_default())
            .and_then(|v| v.as_str().map(String::from));
        let mut args = json!({ "session_id": session_id });
        if let Some(s) = status_str {
            args["status_filter"] = json!(s);
        }
        let result = self.rpc("list_recent_tasks_for_session", args).await?;
        let tasks: Vec<TaskListItem> =
            serde_json::from_value(result).map_err(|e| format!("decode Vec<TaskListItem>: {e}"))?;
        Ok(tasks)
    }

    async fn search_tasks(
        &self,
        _user_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<TaskListItem>, String> {
        let result = self
            .rpc(
                "search_tasks",
                json!({
                    "query": query,
                    "limit": limit,
                }),
            )
            .await?;
        let tasks: Vec<TaskListItem> =
            serde_json::from_value(result).map_err(|e| format!("decode Vec<TaskListItem>: {e}"))?;
        Ok(tasks)
    }

    async fn list_claimable_tasks_for_worker(
        &self,
        _user_id: &str,
        limit: usize,
    ) -> Result<Vec<TaskListItem>, String> {
        let result = self
            .rpc("list_claimable_tasks_for_worker", json!({ "limit": limit }))
            .await?;
        let tasks: Vec<TaskListItem> =
            serde_json::from_value(result).map_err(|e| format!("decode Vec<TaskListItem>: {e}"))?;
        Ok(tasks)
    }

    async fn update_status(&self, task_id: &str, status: TaskStatus) -> Result<(), String> {
        let status_str = serde_json::to_value(status)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
        self.rpc(
            "update_status",
            json!({ "task_id": task_id, "status": status_str }),
        )
        .await?;
        Ok(())
    }

    async fn update_progress(
        &self,
        task_id: &str,
        progress_pct: u32,
        items_done: u32,
        items_total: u32,
    ) -> Result<(), String> {
        self.rpc(
            "update_progress",
            json!({
                "task_id": task_id,
                "progress_pct": progress_pct,
                "items_done": items_done,
                "items_total": items_total,
            }),
        )
        .await?;
        Ok(())
    }

    async fn save_checkpoint(
        &self,
        task_id: &str,
        checkpoint: &TaskCheckpoint,
    ) -> Result<(), String> {
        self.rpc(
            "save_checkpoint",
            json!({ "task_id": task_id, "checkpoint": checkpoint }),
        )
        .await?;
        Ok(())
    }

    async fn update_plan(&self, task_id: &str, plan: &TaskPlan) -> Result<(), String> {
        self.rpc("update_plan", json!({ "task_id": task_id, "plan": plan }))
            .await?;
        Ok(())
    }

    async fn fail_task(&self, task_id: &str, error: &str) -> Result<(), String> {
        self.rpc("fail_task", json!({ "task_id": task_id, "error": error }))
            .await?;
        Ok(())
    }

    async fn complete_task(&self, task_id: &str) -> Result<(), String> {
        self.rpc("complete_task", json!({ "task_id": task_id }))
            .await?;
        Ok(())
    }

    async fn complete_task_with_outcome(
        &self,
        task_id: &str,
        outcome: TaskOutcome,
    ) -> Result<(), String> {
        self.rpc(
            "complete_task_with_outcome",
            json!({ "task_id": task_id, "outcome": outcome }),
        )
        .await?;
        Ok(())
    }

    async fn complete_plan_run(
        &self,
        task_id: &str,
        progress_pct: u32,
        items_done: u32,
        items_total: u32,
        outcome: TaskOutcome,
    ) -> Result<(), String> {
        self.rpc(
            "complete_plan_run",
            json!({
                "task_id": task_id,
                "progress_pct": progress_pct,
                "items_done": items_done,
                "items_total": items_total,
                "outcome": outcome,
            }),
        )
        .await?;
        Ok(())
    }

    async fn record_feedback(
        &self,
        task_id: &str,
        rating: u8,
        outcome: TaskOutcome,
        completion_time_sec: Option<i32>,
    ) -> Result<(), String> {
        self.rpc(
            "record_feedback",
            json!({
                "task_id": task_id,
                "rating": rating,
                "outcome": outcome,
                "completion_time_sec": completion_time_sec,
            }),
        )
        .await?;
        Ok(())
    }

    async fn increment_replan_count(&self, task_id: &str) -> Result<(), String> {
        self.rpc("increment_replan_count", json!({ "task_id": task_id }))
            .await?;
        Ok(())
    }

    async fn extract_template(
        &self,
        task_id: &str,
        goal_pattern: &str,
    ) -> Result<Option<String>, String> {
        let result = self
            .rpc(
                "extract_template",
                json!({ "task_id": task_id, "goal_pattern": goal_pattern }),
            )
            .await?;
        Ok(result
            .get("template_id")
            .and_then(|v| v.as_str())
            .map(String::from))
    }

    async fn recommend_templates(
        &self,
        _user_id: &str,
        goal: &str,
        project_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<TemplateRecommendation>, String> {
        let mut args = json!({ "goal": goal, "limit": limit });
        if let Some(pt) = project_type {
            args["project_type"] = json!(pt);
        }
        let result = self.rpc("recommend_templates", args).await?;
        let recs: Vec<TemplateRecommendation> = serde_json::from_value(result)
            .map_err(|e| format!("decode Vec<TemplateRecommendation>: {e}"))?;
        Ok(recs)
    }

    async fn get_learning_stats(
        &self,
        _user_id: &str,
        goal_pattern: &str,
    ) -> Result<LearningStats, String> {
        let result = self
            .rpc(
                "get_learning_stats",
                json!({ "goal_pattern": goal_pattern }),
            )
            .await?;
        let stats: LearningStats =
            serde_json::from_value(result).map_err(|e| format!("decode LearningStats: {e}"))?;
        Ok(stats)
    }

    async fn record_template_usage(&self, template_id: &str) -> Result<(), String> {
        self.rpc(
            "record_template_usage",
            json!({ "template_id": template_id }),
        )
        .await?;
        Ok(())
    }
}

// ── Job lease HTTP client ─────────────────────────────────────────

/// HTTP-backed `TaskLeaseService`. Mirrors the four trait methods to
/// the existing `/agent-jobs/{task_id}/lease/*` endpoints. Same auth and
/// timeout policy as `HttpTaskService`.
pub struct HttpTaskLeaseService {
    cloud_base: String,
    token: Option<String>,
}

impl HttpTaskLeaseService {
    pub fn new(cloud_base: impl Into<String>, token: Option<String>) -> Self {
        Self {
            cloud_base: cloud_base.into(),
            token,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.cloud_base.trim_end_matches('/'), path)
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value, String> {
        self.post_with_edge_id(path, body, None).await
    }

    async fn post_with_edge_id(
        &self,
        path: &str,
        body: Value,
        edge_id: Option<&str>,
    ) -> Result<Value, String> {
        let client = reqwest::Client::builder()
            .no_proxy() // astra server is local/intranet; bypass http_proxy env
            .timeout(std::time::Duration::from_secs(TASK_HTTP_TIMEOUT_SECS))
            .build()
            .map_err(|e| format!("http client init: {e}"))?;
        let mut req = client.post(self.url(path)).json(&body);
        if let Some(edge_id) = edge_id.filter(|edge_id| !edge_id.trim().is_empty()) {
            req = req.header(ASTRA_EDGE_ID_HEADER, edge_id);
        }
        if let Some(tok) = self.token.as_deref() {
            req = req.bearer_auth(tok);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("network ({path}): {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("cloud {status} ({path}): {body}"));
        }
        resp.json::<Value>()
            .await
            .map_err(|e| format!("decode response ({path}): {e}"))
    }

    async fn get(&self, path: &str) -> Result<Value, String> {
        let client = reqwest::Client::builder()
            .no_proxy() // astra server is local/intranet; bypass http_proxy env
            .timeout(std::time::Duration::from_secs(TASK_HTTP_TIMEOUT_SECS))
            .build()
            .map_err(|e| format!("http client init: {e}"))?;
        let mut req = client.get(self.url(path));
        if let Some(tok) = self.token.as_deref() {
            req = req.bearer_auth(tok);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("network ({path}): {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("cloud {status} ({path}): {body}"));
        }
        resp.json::<Value>()
            .await
            .map_err(|e| format!("decode response ({path}): {e}"))
    }
}

#[async_trait]
impl TaskLeaseService for HttpTaskLeaseService {
    async fn claim_next_claimable_lease(
        &self,
        _user_id: &str,
        agent_id: &str,
        edge_id: &str,
        ttl_sec: i64,
    ) -> Result<NextClaimableLeaseClaimResult, String> {
        let result = self
            .post_with_edge_id(
                "/agent-jobs/lease/claim-next",
                json!({
                    "edge_agent_id": agent_id,
                    "ttl_sec": ttl_sec,
                    "edge_id": edge_id,
                }),
                Some(edge_id),
            )
            .await?;
        serde_json::from_value(result)
            .map_err(|e| format!("decode NextClaimableLeaseClaimResult: {e}"))
    }

    async fn try_claim_lease(
        &self,
        _user_id: &str,
        task_id: &str,
        agent_id: &str,
        edge_id: &str,
        ttl_sec: i64,
    ) -> Result<LeaseClaimResult, String> {
        // user_id resolved server-side from auth header.
        let result = self
            .post_with_edge_id(
                &format!("/agent-jobs/{task_id}/lease/claim"),
                json!({
                    "edge_agent_id": agent_id,
                    "ttl_sec": ttl_sec,
                    "edge_id": edge_id,
                }),
                Some(edge_id),
            )
            .await?;
        serde_json::from_value(result).map_err(|e| format!("decode LeaseClaimResult: {e}"))
    }

    async fn release_lease(
        &self,
        _user_id: &str,
        task_id: &str,
        agent_id: &str,
    ) -> Result<bool, String> {
        let result = self
            .post(
                &format!("/agent-jobs/{task_id}/lease/release"),
                json!({ "edge_agent_id": agent_id }),
            )
            .await?;
        Ok(result
            .get("released")
            .and_then(|v| v.as_bool())
            .unwrap_or(false))
    }

    async fn get_lease(
        &self,
        _user_id: &str,
        task_id: &str,
    ) -> Result<Option<TaskLeaseView>, String> {
        let result = self.get(&format!("/agent-jobs/{task_id}/lease")).await?;
        if result.is_null() {
            return Ok(None);
        }
        serde_json::from_value(result)
            .map(Some)
            .map_err(|e| format!("decode TaskLeaseView: {e}"))
    }

    async fn renew_lease(
        &self,
        _user_id: &str,
        task_id: &str,
        agent_id: &str,
        edge_id: &str,
        ttl_sec: i64,
    ) -> Result<Option<TaskLeaseView>, String> {
        let result = self
            .post_with_edge_id(
                &format!("/agent-jobs/{task_id}/lease/renew"),
                json!({
                    "edge_agent_id": agent_id,
                    "ttl_sec": ttl_sec,
                    "edge_id": edge_id,
                }),
                Some(edge_id),
            )
            .await?;
        if result.is_null() {
            return Ok(None);
        }
        serde_json::from_value(result)
            .map(Some)
            .map_err(|e| format!("decode TaskLeaseView: {e}"))
    }
}
