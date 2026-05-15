//! Cloud REST client for `session_todos`.
//!
//! Astra is edge-cloud. The CLI MUST NOT connect to MatrixOne directly
//! — it routes every task mutation through the server's
//! `/sessions/{sid}/todos` endpoints so the server is the single
//! source of truth for ownership, concurrency, and audit.
//!
//! Clients in this module are thin wrappers that mirror the model-
//! facing `task` tool action shape (one HTTP call per action).
//! Returns a `Result<String, String>`: on success the rendered output
//! the LLM/UI consumes; on error a stringified failure suitable to
//! return as an `Error: ...` ToolResult.

use serde::Deserialize;
use serde_json::{Value, json};

const TODOS_HTTP_TIMEOUT_SECS: u64 = 15;

#[derive(Deserialize)]
struct ExecuteTodoResponse {
    output: String,
}

#[derive(Deserialize)]
struct LoadTodosResponse {
    tasks: Vec<astra_tools::task_mgmt::SessionTask>,
}

fn build_request(
    method: reqwest::Method,
    url: &str,
    token: Option<&str>,
) -> Result<reqwest::RequestBuilder, String> {
    let client = reqwest::Client::builder()
        .no_proxy() // astra server is local/intranet; bypass http_proxy env
        .timeout(std::time::Duration::from_secs(TODOS_HTTP_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("http client init: {e}"))?;
    let mut req = client.request(method, url);
    if let Some(tok) = token {
        req = req.bearer_auth(tok);
    }
    Ok(req)
}

/// `POST /sessions/{sid}/todos:execute` — server-side TaskManager
/// runs the action against MO and returns the same string output the
/// in-memory manager would produce. Action is one of
/// `create | update | list | get | stop`; `args` mirrors the
/// model-emitted `task` tool args.
///
/// U-15: transient cloud failures (5xx response, connection drop)
/// retry once with 200ms backoff. Client errors (4xx) propagate
/// immediately — retrying a 400 just yields the same 400.
pub async fn execute_todo_action(
    cloud_base: &str,
    token: Option<&str>,
    session_id: &str,
    action: &str,
    args: &Value,
) -> Result<String, String> {
    let url = format!(
        "{}/sessions/{}/todos:execute",
        cloud_base.trim_end_matches('/'),
        session_id
    );
    let body = json!({ "action": action, "args": args });

    for attempt in 0..2 {
        let resp = build_request(reqwest::Method::POST, &url, token)?
            .json(&body)
            .send()
            .await;
        match resp {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    let parsed: ExecuteTodoResponse = resp
                        .json()
                        .await
                        .map_err(|e| format!("decode response: {e}"))?;
                    return Ok(parsed.output);
                }
                // Retry once on 5xx; 4xx surfaces immediately.
                if status.is_server_error() && attempt == 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    continue;
                }
                let body = resp.text().await.unwrap_or_default();
                return Err(format!("cloud {status}: {body}"));
            }
            Err(e) => {
                // Connection-failed counts as transient. Retry once;
                // second failure surfaces.
                if attempt == 0 && (e.is_connect() || e.is_timeout()) {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    continue;
                }
                return Err(format!("network: {e}"));
            }
        }
    }
    // Loop always returns inside; this branch is unreachable but the
    // compiler can't prove it without a labeled break.
    Err("execute_todo_action: retry loop exhausted".to_string())
}

/// `GET /users/me/todos?status=...` — cross-session active list for
/// the authenticated user. Returns a JSON-stringified payload
/// formatted as the model-facing `task` tool's output convention so
/// the CLI dispatcher can pass it straight through.
pub async fn list_user_todos(
    cloud_base: &str,
    token: Option<&str>,
    status: &str,
) -> Result<String, String> {
    let url = format!(
        "{}/users/me/todos?status={}",
        cloud_base.trim_end_matches('/'),
        status
    );
    let resp = build_request(reqwest::Method::GET, &url, token)?
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;
    let status_code = resp.status();
    if !status_code.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("cloud {status_code}: {body}"));
    }
    let body = resp.text().await.map_err(|e| format!("read body: {e}"))?;
    let summary_count = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("total").and_then(|t| t.as_u64()))
        .unwrap_or(0);
    Ok(format!(
        "Cross-session todos: {summary_count} active item(s)\n{body}"
    ))
}

/// `GET /sessions/{sid}/todos` — full list, used by the task board
/// observer to render the dashboard without per-action round-trips.
pub async fn load_todos(
    cloud_base: &str,
    token: Option<&str>,
    session_id: &str,
) -> Result<Vec<astra_tools::task_mgmt::SessionTask>, String> {
    let url = format!(
        "{}/sessions/{}/todos",
        cloud_base.trim_end_matches('/'),
        session_id
    );
    let resp = build_request(reqwest::Method::GET, &url, token)?
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("cloud {status}: {body}"));
    }
    let parsed: LoadTodosResponse = resp
        .json()
        .await
        .map_err(|e| format!("decode response: {e}"))?;
    Ok(parsed.tasks)
}

// ─── HttpTaskStore ─────────────────────────────────────────────────

use async_trait::async_trait;
use astra_tools::task_mgmt::{SessionTask, TaskStore, TaskMutation};
use std::sync::Arc;

/// A read-only `TaskStore` backed by the server's REST API. The
/// observer polls `load()` → `GET /sessions/{sid}/todos`; mutations
/// are signalled via a broadcast channel that `route_task_action`
/// fires after every successful write so the observer refetches
/// immediately (≤50ms dirty window).
pub struct HttpTaskStore {
    cloud_base: String,
    token: Option<String>,
    notify_tx: tokio::sync::broadcast::Sender<String>,
}

impl HttpTaskStore {
    pub fn new(
        cloud_base: impl Into<String>,
        token: Option<String>,
    ) -> (Arc<Self>, tokio::sync::broadcast::Sender<String>) {
        let (tx, _) = tokio::sync::broadcast::channel(32);
        let store = Arc::new(Self {
            cloud_base: cloud_base.into(),
            token,
            notify_tx: tx.clone(),
        });
        (store, tx)
    }
}

#[async_trait]
impl TaskStore for HttpTaskStore {
    async fn load(&self, session_id: &str) -> Result<Vec<SessionTask>, String> {
        load_todos(&self.cloud_base, self.token.as_deref(), session_id).await
    }

    async fn save(&self, _session_id: &str, _tasks: Vec<SessionTask>) -> Result<(), String> {
        Err("HttpTaskStore is read-only; mutations go through route_task_action".into())
    }

    async fn mutate(&self, _session_id: &str, _mutation: TaskMutation) -> Result<String, String> {
        Err("HttpTaskStore is read-only; mutations go through route_task_action".into())
    }

    async fn next_task_id(&self, _session_id: &str) -> Result<u32, String> {
        Err("HttpTaskStore: id allocation is server-side".into())
    }

    async fn peek_next_task_id(&self, _session_id: &str) -> Result<u32, String> {
        Err("HttpTaskStore: id allocation is server-side".into())
    }

    fn subscribe(&self) -> Option<tokio::sync::broadcast::Receiver<String>> {
        Some(self.notify_tx.subscribe())
    }
}
