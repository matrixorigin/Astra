//! Cloud REST client for `session_todos`.
//!
//! Astra is edge-cloud. The CLI MUST NOT connect to MatrixOne directly
//! — it routes every task mutation through the server's
//! `/sessions/{sid}/todos` endpoints so the server is the single
//! source of truth for ownership, concurrency, and audit.
//!
//! Clients in this module are thin wrappers that mirror the model-
//! facing `task_board` tool action shape (one HTTP call per action).
//! Returns a `Result<String, String>`: on success the rendered output
//! the LLM/UI consumes; on error a stringified failure suitable to
//! return as an `Error: ...` ToolResult.

use serde::{Deserialize, Serialize};
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

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UserTodoEntry {
    session_id: String,
    todo_id: String,
    title: String,
    status: String,
    updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_title: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UserTodosResponse {
    tasks: Vec<UserTodoEntry>,
    total: usize,
}

#[derive(Debug)]
struct TodoReadError {
    health: astra_tools::task_mgmt::TaskStoreHealth,
    message: String,
}

impl TodoReadError {
    fn new(health: astra_tools::task_mgmt::TaskStoreHealth, message: impl Into<String>) -> Self {
        Self {
            health,
            message: message.into(),
        }
    }
}

fn health_for_http_status(
    status: reqwest::StatusCode,
    not_found_health: astra_tools::task_mgmt::TaskStoreHealth,
) -> astra_tools::task_mgmt::TaskStoreHealth {
    use astra_tools::task_mgmt::TaskStoreHealth;
    if matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    ) {
        TaskStoreHealth::AuthenticationRequired
    } else if status == reqwest::StatusCode::NOT_FOUND {
        not_found_health
    } else if status.is_server_error()
        || matches!(
            status,
            reqwest::StatusCode::REQUEST_TIMEOUT | reqwest::StatusCode::TOO_MANY_REQUESTS
        )
    {
        TaskStoreHealth::ServiceUnavailable
    } else {
        TaskStoreHealth::ProtocolMismatch
    }
}

fn format_cloud_error(status: reqwest::StatusCode, body: &str) -> String {
    let detail = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("detail")
                .and_then(|detail| detail.as_str())
                .map(str::to_string)
        })
        .filter(|detail| !detail.trim().is_empty())
        .unwrap_or_else(|| body.to_string());
    if detail.is_empty() {
        format!("cloud {status}")
    } else {
        format!("cloud {status}: {detail}")
    }
}

fn build_request(
    method: reqwest::Method,
    url: &str,
    token: Option<&str>,
) -> Result<reqwest::RequestBuilder, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(TODOS_HTTP_TIMEOUT_SECS))
        .no_proxy()
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
/// `create | update | list | get | stop | adopt | archive`; `args` mirrors the
/// model-emitted `task_board` tool args.
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
    let body = if action == "create" {
        json!({
            "action": action,
            "args": args,
            "idempotency_key": format!("todo-create:{}", uuid::Uuid::new_v4()),
        })
    } else {
        json!({ "action": action, "args": args })
    };

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
                return Err(format_cloud_error(status, &body));
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

/// Internal fork support: copy the parent session task board into an
/// empty child session without migrating the parent tasks. Uses the
/// same server-side TaskManager/MatrixOne write surface as model-facing
/// task actions, but `fork_copy` is intentionally not exposed in the
/// tool schema.
pub async fn copy_todos_for_fork(
    cloud_base: &str,
    token: Option<&str>,
    source_session_id: &str,
    target_session_id: &str,
) -> Result<String, String> {
    execute_todo_action(
        cloud_base,
        token,
        target_session_id,
        "fork_copy",
        &json!({
            "source_session_id": source_session_id,
        }),
    )
    .await
}

/// `GET /users/me/todos?status=...` — cross-session active list for
/// the authenticated user. Returns a JSON-stringified payload
/// formatted as the model-facing `task_board` tool's output convention so
/// the CLI dispatcher can pass it straight through.
pub async fn list_user_todos(
    cloud_base: &str,
    token: Option<&str>,
    status: &str,
) -> Result<String, String> {
    let response = fetch_user_todos(cloud_base, token, status)
        .await
        .map_err(|error| error.message)?;
    let summary_count = response.total;
    let body = serde_json::to_string(&response).map_err(|e| format!("encode response: {e}"))?;
    Ok(format!(
        "Cross-session todos: {summary_count} {status} item(s)\n{body}"
    ))
}

async fn fetch_user_todos(
    cloud_base: &str,
    token: Option<&str>,
    status: &str,
) -> Result<UserTodosResponse, TodoReadError> {
    let url = format!(
        "{}/users/me/todos?status={}",
        cloud_base.trim_end_matches('/'),
        status
    );
    let resp = build_request(reqwest::Method::GET, &url, token)
        .map_err(|error| {
            TodoReadError::new(
                astra_tools::task_mgmt::TaskStoreHealth::TransportUnavailable,
                error,
            )
        })?
        .send()
        .await
        .map_err(|e| {
            TodoReadError::new(
                astra_tools::task_mgmt::TaskStoreHealth::TransportUnavailable,
                format!("network: {e}"),
            )
        })?;
    let status_code = resp.status();
    if !status_code.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(TodoReadError::new(
            health_for_http_status(
                status_code,
                astra_tools::task_mgmt::TaskStoreHealth::ProtocolMismatch,
            ),
            format_cloud_error(status_code, &body),
        ));
    }
    resp.json().await.map_err(|e| {
        TodoReadError::new(
            astra_tools::task_mgmt::TaskStoreHealth::ProtocolMismatch,
            format!("decode response: {e}"),
        )
    })
}

async fn load_open_todo_summaries(
    cloud_base: &str,
    token: Option<&str>,
    limit: usize,
) -> Result<Vec<(String, Vec<astra_tools::task_mgmt::OpenTaskSummary>)>, TodoReadError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let response = fetch_user_todos(cloud_base, token, "active").await?;
    let mut grouped: Vec<(String, Vec<astra_tools::task_mgmt::OpenTaskSummary>)> = Vec::new();
    for entry in response.tasks.into_iter().take(limit) {
        let status = astra_tools::task_mgmt::SessionTaskStatusKind::from_status_str(&entry.status);
        if !status.is_open_work() {
            return Err(TodoReadError::new(
                astra_tools::task_mgmt::TaskStoreHealth::ProtocolMismatch,
                format!(
                    "cloud active todo '{}' has non-open status '{}'",
                    entry.todo_id, entry.status
                ),
            ));
        }
        let summary = astra_tools::task_mgmt::OpenTaskSummary {
            id: entry.todo_id,
            title: entry.title,
            status,
            updated_at: entry.updated_at,
        };
        if let Some((_, tasks)) = grouped
            .iter_mut()
            .find(|(session_id, _)| session_id == &entry.session_id)
        {
            tasks.push(summary);
        } else {
            grouped.push((entry.session_id, vec![summary]));
        }
    }
    Ok(grouped)
}

/// `GET /sessions/{sid}/todos` — full list, used by the task board
/// observer to render the dashboard without per-action round-trips.
pub async fn load_todos(
    cloud_base: &str,
    token: Option<&str>,
    session_id: &str,
) -> Result<Vec<astra_tools::task_mgmt::SessionTask>, String> {
    load_todos_read(cloud_base, token, session_id)
        .await
        .map_err(|error| error.message)
}

async fn load_todos_read(
    cloud_base: &str,
    token: Option<&str>,
    session_id: &str,
) -> Result<Vec<astra_tools::task_mgmt::SessionTask>, TodoReadError> {
    let url = format!(
        "{}/sessions/{}/todos",
        cloud_base.trim_end_matches('/'),
        session_id
    );
    let resp = build_request(reqwest::Method::GET, &url, token)
        .map_err(|error| {
            TodoReadError::new(
                astra_tools::task_mgmt::TaskStoreHealth::TransportUnavailable,
                error,
            )
        })?
        .send()
        .await
        .map_err(|e| {
            TodoReadError::new(
                astra_tools::task_mgmt::TaskStoreHealth::TransportUnavailable,
                format!("network: {e}"),
            )
        })?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(TodoReadError::new(
            health_for_http_status(
                status,
                astra_tools::task_mgmt::TaskStoreHealth::SessionUnavailable,
            ),
            format_cloud_error(status, &body),
        ));
    }
    let parsed: LoadTodosResponse = resp.json().await.map_err(|e| {
        TodoReadError::new(
            astra_tools::task_mgmt::TaskStoreHealth::ProtocolMismatch,
            format!("decode response: {e}"),
        )
    })?;
    Ok(parsed.tasks)
}

// ─── HttpTaskStore ─────────────────────────────────────────────────

use astra_tools::task_mgmt::{
    OpenTaskSummary, SessionTask, TaskMutation, TaskStore, TaskStoreHealth,
};
use async_trait::async_trait;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

fn task_store_health_code(health: TaskStoreHealth) -> u8 {
    match health {
        TaskStoreHealth::Unknown => 0,
        TaskStoreHealth::Ready => 1,
        TaskStoreHealth::AuthenticationRequired => 2,
        TaskStoreHealth::SessionUnavailable => 3,
        TaskStoreHealth::ServiceUnavailable => 4,
        TaskStoreHealth::TransportUnavailable => 5,
        TaskStoreHealth::ProtocolMismatch => 6,
    }
}

fn task_store_health_from_code(code: u8) -> TaskStoreHealth {
    match code {
        1 => TaskStoreHealth::Ready,
        2 => TaskStoreHealth::AuthenticationRequired,
        3 => TaskStoreHealth::SessionUnavailable,
        4 => TaskStoreHealth::ServiceUnavailable,
        5 => TaskStoreHealth::TransportUnavailable,
        6 => TaskStoreHealth::ProtocolMismatch,
        _ => TaskStoreHealth::Unknown,
    }
}

/// A read-only `TaskStore` backed by the server's REST API. The
/// observer polls `load()` → `GET /sessions/{sid}/todos`; mutations
/// are signalled via a broadcast channel that `route_task_action`
/// fires after every successful write so the observer refetches
/// immediately (≤50ms dirty window).
pub struct HttpTaskStore {
    cloud_base: String,
    credentials: TaskStoreCredentials,
    notify_tx: tokio::sync::broadcast::Sender<String>,
    health: AtomicU8,
}

/// The task board is a long-lived TUI dependency. Its credentials must not be
/// a startup snapshot: login and token refresh can happen after the observer
/// has been created. Tests and one-shot callers may still deliberately supply
/// a fixed token, so make that distinction explicit instead of silently
/// treating a stale startup token as current authentication.
enum TaskStoreCredentials {
    Fixed(Option<String>),
    Profile(Option<String>),
}

impl TaskStoreCredentials {
    fn access_token(&self) -> Option<String> {
        match self {
            Self::Fixed(token) => token.clone(),
            Self::Profile(profile) => {
                crate::cli::session::session_runtime::current_access_token(profile.as_deref())
            }
        }
    }
}

impl HttpTaskStore {
    pub fn new(
        cloud_base: impl Into<String>,
        token: Option<String>,
    ) -> (Arc<Self>, tokio::sync::broadcast::Sender<String>) {
        let (tx, _) = tokio::sync::broadcast::channel(32);
        let store = Arc::new(Self {
            cloud_base: cloud_base.into(),
            credentials: TaskStoreCredentials::Fixed(token),
            notify_tx: tx.clone(),
            health: AtomicU8::new(task_store_health_code(TaskStoreHealth::Unknown)),
        });
        (store, tx)
    }

    /// Build a store for the interactive CLI. The access token is resolved at
    /// request time so an existing task board converges after `/login` or a
    /// silent refresh without a TUI restart.
    pub fn for_profile(
        cloud_base: impl Into<String>,
        profile: Option<&str>,
    ) -> (Arc<Self>, tokio::sync::broadcast::Sender<String>) {
        let (tx, _) = tokio::sync::broadcast::channel(32);
        let store = Arc::new(Self {
            cloud_base: cloud_base.into(),
            credentials: TaskStoreCredentials::Profile(profile.map(str::to_owned)),
            notify_tx: tx.clone(),
            health: AtomicU8::new(task_store_health_code(TaskStoreHealth::Unknown)),
        });
        (store, tx)
    }
}

#[async_trait]
impl TaskStore for HttpTaskStore {
    fn health_snapshot(&self) -> TaskStoreHealth {
        task_store_health_from_code(self.health.load(Ordering::Acquire))
    }

    async fn load(&self, session_id: &str) -> Result<Vec<SessionTask>, String> {
        let token = self.credentials.access_token();
        match load_todos_read(&self.cloud_base, token.as_deref(), session_id).await {
            Ok(tasks) => {
                self.health.store(
                    task_store_health_code(TaskStoreHealth::Ready),
                    Ordering::Release,
                );
                Ok(tasks)
            }
            Err(error) => {
                self.health
                    .store(task_store_health_code(error.health), Ordering::Release);
                Err(error.message)
            }
        }
    }

    async fn load_open_task_summaries(
        &self,
        limit: usize,
    ) -> Result<Vec<(String, Vec<OpenTaskSummary>)>, String> {
        let token = self.credentials.access_token();
        match load_open_todo_summaries(&self.cloud_base, token.as_deref(), limit).await {
            Ok(summaries) => {
                self.health.store(
                    task_store_health_code(TaskStoreHealth::Ready),
                    Ordering::Release,
                );
                Ok(summaries)
            }
            Err(error) => {
                self.health
                    .store(task_store_health_code(error.health), Ordering::Release);
                Err(error.message)
            }
        }
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

    async fn set_next_task_id(&self, _session_id: &str, _next: u32) -> Result<(), String> {
        Err("HttpTaskStore: id counter is server-side".into())
    }

    fn subscribe(&self) -> Option<tokio::sync::broadcast::Receiver<String>> {
        Some(self.notify_tx.subscribe())
    }
}

// ───────────────────────────────────────────────────────────────────────
// Wiring E2E tests
//
// These tests pin the assembly boundary that unit tests can't reach:
// HttpTaskStore (this file) + TaskBoardObserver + the broadcast plumbing
// the executor uses to signal mid-turn writes. They've caught three
// distinct regressions during the dashboard rewrite:
//
//  1. resolve_task_store reading ASTRA_API_URL while the executor used
//     api.api_origin() — two cloud_base sources of truth → observer
//     polled an empty in-memory store while the executor wrote to cloud.
//  2. TaskBoardObserver constructed with empty session_id silently
//     filtered every broadcast for a brand-new session (the server
//     allocates the SID mid-turn).
//  3. `task_board` tool stuck in T2 deferred so the model never invoked it
//     even when the user asked for multi-step work.
//
// Each test wires a fresh wiremock server playing the role of
// astra-server's `/sessions/{sid}/todos*` endpoints, builds the real
// HttpTaskStore + TaskBoardObserver, and asserts the observable
// behaviour (mutations land within an SLA and terminal tasks remain
// inspectable through the canonical projection).
// No MatrixOne, no axum, no SSE.
// ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod wiring_e2e {
    use super::{
        HttpTaskStore, copy_todos_for_fork, execute_todo_action, health_for_http_status,
        list_user_todos,
    };
    use crate::cli::cli_config::cli_utils::{CredentialsFile, Profile, save_credentials};
    use crate::lock_recovery::LockRecovery;
    use crate::test_utils::ProcessEnvGuard;
    use crate::tests::isolate_credentials;
    use crate::tui::task_board_observer::TaskBoardObserver;
    use astra_tools::task_mgmt::{SessionTask, TaskStore, TaskStoreHealth};
    use serde_json::{Value, json};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    /// Build a stub server that:
    /// - GET  /sessions/{sid}/todos          → returns an evolving task list
    /// - POST /sessions/{sid}/todos:execute  → bumps the list (create/update)
    ///
    /// State is shared via Arc<Mutex<Vec<SessionTask>>>.
    async fn spawn_mock_server() -> (MockServer, Arc<std::sync::Mutex<Vec<SessionTask>>>) {
        let server = MockServer::start().await;
        let state: Arc<std::sync::Mutex<Vec<SessionTask>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let counter = Arc::new(AtomicU64::new(0));

        // GET /sessions/.../todos — return whatever's in `state`.
        let state_get = state.clone();
        Mock::given(method("GET"))
            .and(wiremock::matchers::path_regex(r"^/sessions/[^/]+/todos$"))
            .respond_with(move |_req: &Request| {
                let tasks = state_get.lock_recover().clone();
                ResponseTemplate::new(200).set_body_json(json!({ "tasks": tasks }))
            })
            .mount(&server)
            .await;

        // POST /sessions/.../todos:execute — create / update / list.
        let state_exec = state.clone();
        let counter_exec = counter.clone();
        Mock::given(method("POST"))
            .and(wiremock::matchers::path_regex(
                r"^/sessions/[^/]+/todos:execute$",
            ))
            .respond_with(move |req: &Request| {
                let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
                let action = body.get("action").and_then(|v| v.as_str()).unwrap_or("");
                let args = body.get("args").cloned().unwrap_or(Value::Null);
                let mut tasks = state_exec.lock_recover();
                let output = match action {
                    "create" => {
                        let next = counter_exec.fetch_add(1, Ordering::SeqCst) + 1;
                        let id = format!("task-{next}");
                        let title = args
                            .get("title")
                            .and_then(|v| v.as_str())
                            .unwrap_or("untitled")
                            .to_string();
                        tasks.push(SessionTask {
                            archived_at: None,
                            id: id.clone(),
                            title: title.clone(),
                            description: None,
                            status: "pending".into(),
                            subtasks: vec![],
                            created_at: "now".into(),
                            updated_at: "now".into(),
                            active_form: None,
                            owner: None,
                            metadata: None,
                            blocks: vec![],
                            blocked_by: vec![],
                        });
                        format!("Task #{id} created: {title}")
                    }
                    "update" => {
                        let id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
                        let new_status = args
                            .get("new_status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("pending");
                        if let Some(task) = tasks.iter_mut().find(|t| t.id == id) {
                            task.status =
                                astra_tools::task_mgmt::SessionTaskStatusKind::from(new_status);
                        }
                        format!("Task #{id} updated to {new_status}")
                    }
                    other => format!("Error: unsupported action {other}"),
                };
                ResponseTemplate::new(200).set_body_json(json!({ "output": output }))
            })
            .mount(&server)
            .await;

        (server, state)
    }

    /// Wait for `cond` to hold while pumping `pump()` between polls.
    /// Returns the elapsed time on success; panics on timeout so the
    /// test fails with a useful message via `expect`.
    async fn wait_until<F: Fn() -> bool>(
        cond: F,
        timeout_ms: u64,
        pump: impl Fn(),
    ) -> Result<Duration, ()> {
        let start = Instant::now();
        let deadline = start + Duration::from_millis(timeout_ms);
        while !cond() && Instant::now() < deadline {
            pump();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if cond() { Ok(start.elapsed()) } else { Err(()) }
    }

    /// REGRESSION: `route_task_action` POSTs to the cloud on a `task_board.create`,
    /// fires the broadcast, and the observer picks the row up within the
    /// dirty/FAST_POLL window (≤ 200ms even on slow CI). This is the
    /// "dashboard appears mid-turn" SLA.
    #[tokio::test]
    async fn task_action_create_lands_in_observer_within_sla() {
        let (server, _state) = spawn_mock_server().await;
        let (store, notify_tx) = HttpTaskStore::new(server.uri(), None);
        let store_dyn: Arc<dyn TaskStore> = store.clone();
        let observer = TaskBoardObserver::new(store_dyn, "sess-sla");

        // Simulate what `route_task_action` does after a successful POST:
        // (1) push the row server-side via the same HTTP path the executor
        // would hit, (2) broadcast the session_id so the observer picks
        // it up immediately.
        let sid = "sess-sla";
        let started = Instant::now();
        let resp = execute_todo_action(
            &server.uri(),
            None,
            sid,
            "create",
            &json!({ "title": "first task" }),
        )
        .await
        .expect("create should succeed against the mock");
        assert!(resp.contains("first task"));
        let _ = notify_tx.send(sid.to_string());

        let elapsed = wait_until(
            || !observer.snapshot().tasks.is_empty(),
            300,
            || observer.maybe_refresh(),
        )
        .await
        .expect("task must surface in observer within SLA window");
        let total_ms = started.elapsed().as_millis();
        assert!(
            elapsed < Duration::from_millis(300),
            "observer should pick up create within SLA (took {elapsed:?}, total {total_ms}ms)"
        );
        let snap = observer.snapshot();
        assert_eq!(snap.tasks.len(), 1);
        assert_eq!(snap.tasks[0].title, "first task");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn profile_backed_store_uses_credentials_written_after_startup() {
        let _credentials = isolate_credentials();
        let _access_token = ProcessEnvGuard::remove("ASTRA_ACCESS_TOKEN");
        let mut credentials = CredentialsFile {
            current_profile: Some("task-board".to_string()),
            ..Default::default()
        };
        credentials.profiles.insert(
            "task-board".to_string(),
            Profile {
                access_token: Some("token-at-startup".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&credentials).expect("write initial credentials");

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/sessions/session-a/todos"))
            .and(header("authorization", "Bearer token-after-refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "tasks": [] })))
            .mount(&server)
            .await;
        let (store, _notify_tx) = HttpTaskStore::for_profile(server.uri(), Some("task-board"));

        credentials
            .profiles
            .get_mut("task-board")
            .expect("profile must exist")
            .access_token = Some("token-after-refresh".to_string());
        save_credentials(&credentials).expect("write refreshed credentials");

        let tasks = store
            .load("session-a")
            .await
            .expect("task board must use the refreshed token");
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn list_user_todos_summary_names_requested_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/users/me/todos"))
            .and(query_param("status", "completed"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tasks": [],
                "total": 2
            })))
            .mount(&server)
            .await;

        let output = list_user_todos(&server.uri(), None, "completed")
            .await
            .expect("mock list_user_todos");
        assert!(
            output.starts_with("Cross-session todos: 2 completed item(s)"),
            "summary should name the requested status, not always active: {output}"
        );
    }

    #[test]
    fn task_read_http_statuses_map_to_structured_recovery_classes() {
        assert_eq!(
            health_for_http_status(
                reqwest::StatusCode::UNAUTHORIZED,
                TaskStoreHealth::ProtocolMismatch
            ),
            TaskStoreHealth::AuthenticationRequired
        );
        assert_eq!(
            health_for_http_status(
                reqwest::StatusCode::TOO_MANY_REQUESTS,
                TaskStoreHealth::ProtocolMismatch
            ),
            TaskStoreHealth::ServiceUnavailable
        );
        assert_eq!(
            health_for_http_status(
                reqwest::StatusCode::NOT_FOUND,
                TaskStoreHealth::SessionUnavailable
            ),
            TaskStoreHealth::SessionUnavailable
        );
        assert_eq!(
            health_for_http_status(
                reqwest::StatusCode::NOT_FOUND,
                TaskStoreHealth::ProtocolMismatch
            ),
            TaskStoreHealth::ProtocolMismatch
        );
        assert_eq!(
            health_for_http_status(
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                TaskStoreHealth::ProtocolMismatch
            ),
            TaskStoreHealth::ServiceUnavailable
        );
        assert_eq!(
            health_for_http_status(
                reqwest::StatusCode::BAD_REQUEST,
                TaskStoreHealth::SessionUnavailable
            ),
            TaskStoreHealth::ProtocolMismatch
        );
    }

    #[tokio::test]
    async fn http_store_cross_session_summaries_reach_the_tui_without_fabricated_tasks() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/users/me/todos"))
            .and(query_param("status", "active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tasks": [
                    {
                        "session_id": "session-b",
                        "todo_id": "task-b1",
                        "title": "newest remote work",
                        "status": "in_progress",
                        "updated_at": "2026-07-11T12:03:00Z",
                        "session_started_at": "2026-07-11T12:00:00Z",
                        "session_title": "Remote B"
                    },
                    {
                        "session_id": "session-a",
                        "todo_id": "task-a1",
                        "title": "queued remote work",
                        "status": "pending",
                        "updated_at": "2026-07-11T12:02:00Z"
                    },
                    {
                        "session_id": "session-b",
                        "todo_id": "task-b2",
                        "title": "paused remote work",
                        "status": "paused",
                        "updated_at": "2026-07-11T12:01:00Z"
                    }
                ],
                "total": 3
            })))
            .mount(&server)
            .await;

        let (store, _notify_tx) = HttpTaskStore::new(server.uri(), None);
        let bounded = store
            .load_open_task_summaries(2)
            .await
            .expect("typed cross-session read");
        assert_eq!(
            bounded.iter().map(|(_, tasks)| tasks.len()).sum::<usize>(),
            2
        );
        assert_eq!(bounded[0].0, "session-b");
        assert_eq!(bounded[0].1[0].id, "task-b1");
        assert_eq!(bounded[0].1[0].updated_at, "2026-07-11T12:03:00Z");
        assert_eq!(bounded[1].0, "session-a");

        let observer = TaskBoardObserver::new(store as Arc<dyn TaskStore>, "session-a");
        observer.toggle_view_mode();
        wait_until(
            || {
                observer
                    .multi_snapshot()
                    .per_session
                    .iter()
                    .map(|(_, tasks)| tasks.len())
                    .sum::<usize>()
                    == 3
            },
            500,
            || observer.maybe_refresh(),
        )
        .await
        .expect("HTTP cross-session summaries must reach the all-sessions TUI lane");
        let snapshot = observer.multi_snapshot();
        assert_eq!(snapshot.per_session.len(), 2);
        assert_eq!(snapshot.per_session[0].0, "session-b");
        assert_eq!(snapshot.per_session[0].1[1].title, "paused remote work");
    }

    #[tokio::test]
    async fn http_store_rejects_non_open_rows_from_the_active_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/users/me/todos"))
            .and(query_param("status", "active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tasks": [{
                    "session_id": "session-a",
                    "todo_id": "task-done",
                    "title": "should not be active",
                    "status": "completed",
                    "updated_at": "2026-07-11T12:00:00Z"
                }],
                "total": 1
            })))
            .mount(&server)
            .await;

        let (store, _notify_tx) = HttpTaskStore::new(server.uri(), None);
        let error = store
            .load_open_task_summaries(200)
            .await
            .expect_err("active endpoint contract violations must fail closed");
        assert!(
            error.contains("task-done") && error.contains("completed"),
            "{error}"
        );
        assert_eq!(store.health_snapshot(), TaskStoreHealth::ProtocolMismatch);
    }

    #[tokio::test]
    async fn http_store_service_failure_reaches_observer_as_structured_health() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/sessions/session-a/todos"))
            .respond_with(ResponseTemplate::new(503).set_body_json(json!({
                "detail": "session_todos storage unavailable"
            })))
            .mount(&server)
            .await;

        let (store, _notify_tx) = HttpTaskStore::new(server.uri(), None);
        let observer = TaskBoardObserver::new(store.clone() as Arc<dyn TaskStore>, "session-a");
        observer.maybe_refresh();
        wait_until(
            || {
                observer.truth_state()
                    == crate::tui::task_board_observer::TaskBoardTruthState::Unavailable
            },
            500,
            || observer.maybe_refresh(),
        )
        .await
        .expect("failed storage read must reach observer truth");

        assert_eq!(store.health_snapshot(), TaskStoreHealth::ServiceUnavailable);
        assert_eq!(
            observer.active_projection().store_health(),
            TaskStoreHealth::ServiceUnavailable
        );
    }

    #[tokio::test]
    async fn copy_todos_for_fork_posts_internal_action_to_child_session() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/sessions/child-session/todos:execute"))
            .respond_with(|req: &Request| {
                let body: Value = serde_json::from_slice(&req.body).expect("json body");
                assert_eq!(body["action"], "fork_copy", "{body}");
                assert_eq!(body["args"]["source_session_id"], "parent-session", "{body}");
                ResponseTemplate::new(200).set_body_json(json!({
                    "output": "Fork task board copied: 1 task(s)\n{\"success\":true,\"status\":\"copied\",\"count\":1}"
                }))
            })
            .mount(&server)
            .await;

        let output = copy_todos_for_fork(&server.uri(), None, "parent-session", "child-session")
            .await
            .expect("mock fork copy");
        assert!(output.contains("\"status\":\"copied\""), "{output}");
    }

    #[tokio::test]
    async fn list_user_todos_surfaces_cloud_error_detail_without_json_noise() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/users/me/todos"))
            .and(query_param("status", "cancelledd"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "detail": "invalid status 'cancelledd' (valid: pending|in_progress|paused|completed|failed|cancelled)"
            })))
            .mount(&server)
            .await;

        let err = list_user_todos(&server.uri(), None, "cancelledd")
            .await
            .expect_err("invalid status should surface as a cloud error");
        assert!(
            err.contains("cloud 400 Bad Request: invalid status 'cancelledd'"),
            "{err}"
        );
        assert!(
            !err.contains("\"detail\""),
            "CLI-facing error should show the detail, not raw JSON: {err}"
        );
    }

    #[tokio::test]
    async fn execute_todo_action_surfaces_cloud_error_detail_without_json_noise() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wiremock::matchers::path_regex(
                r"^/sessions/[^/]+/todos:execute$",
            ))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "detail": "field 'status_filter' must be a string"
            })))
            .mount(&server)
            .await;

        let err = execute_todo_action(
            &server.uri(),
            None,
            "sess-bad-filter",
            "list",
            &json!({"status_filter": true}),
        )
        .await
        .expect_err("invalid task list args should surface as a cloud error");
        assert!(
            err.contains("cloud 400 Bad Request: field 'status_filter' must be a string"),
            "{err}"
        );
        assert!(
            !err.contains("\"detail\""),
            "CLI-facing error should show the detail, not raw JSON: {err}"
        );
    }

    /// REGRESSION: TUI starts with an empty `session_id` (server allocates
    /// it mid-turn). Without the adoption fix the observer's broadcast
    /// receiver dropped every event because `changed_sid != ""`. This
    /// test pins the self-heal: an empty observer must adopt the first
    /// non-empty broadcast SID and start fetching against it.
    #[tokio::test]
    async fn empty_observer_self_heals_on_first_broadcast() {
        let (server, _state) = spawn_mock_server().await;
        let (store, notify_tx) = HttpTaskStore::new(server.uri(), None);
        let store_dyn: Arc<dyn TaskStore> = store.clone();
        // Observer constructed BEFORE the server allocates a SID — same
        // shape as `run_tui_session` building it from `state.session_id =
        // None.unwrap_or_default() = ""`.
        let observer = TaskBoardObserver::new(store_dyn, "");

        let sid = "sess-allocated-mid-turn";
        let _ = execute_todo_action(
            &server.uri(),
            None,
            sid,
            "create",
            &json!({ "title": "post-adoption" }),
        )
        .await
        .expect("mock POST");
        let _ = notify_tx.send(sid.to_string());

        wait_until(
            || !observer.snapshot().tasks.is_empty(),
            500,
            || observer.maybe_refresh(),
        )
        .await
        .expect("observer must adopt the broadcast SID");
        assert_eq!(
            observer.snapshot().tasks[0].title,
            "post-adoption",
            "observer must read against the adopted SID, not its constructor sid"
        );
    }

    /// Completion is durable history, not a transient toast: the real HTTP
    /// store path must keep a terminal row inspectable in the same canonical
    /// task-board projection that reported its completion.
    #[tokio::test]
    async fn completed_task_remains_renderable_via_http_path() {
        let (server, _state) = spawn_mock_server().await;
        let (store, notify_tx) = HttpTaskStore::new(server.uri(), None);
        let store_dyn: Arc<dyn TaskStore> = store.clone();
        let observer = TaskBoardObserver::new(store_dyn, "sess-ttl");

        let sid = "sess-ttl";
        let _ = execute_todo_action(
            &server.uri(),
            None,
            sid,
            "create",
            &json!({ "title": "shipping work" }),
        )
        .await
        .unwrap();
        let _ = notify_tx.send(sid.to_string());
        wait_until(
            || observer.snapshot().tasks.len() == 1,
            500,
            || observer.maybe_refresh(),
        )
        .await
        .expect("task must surface");

        let _ = execute_todo_action(
            &server.uri(),
            None,
            sid,
            "update",
            &json!({ "task_id": "task-1", "new_status": "in_progress" }),
        )
        .await
        .unwrap();
        let _ = execute_todo_action(
            &server.uri(),
            None,
            sid,
            "update",
            &json!({ "task_id": "task-1", "new_status": "completed" }),
        )
        .await
        .unwrap();
        let _ = notify_tx.send(sid.to_string());
        wait_until(
            || {
                observer
                    .snapshot()
                    .tasks
                    .iter()
                    .any(|t| t.status.is_completed())
            },
            500,
            || observer.maybe_refresh(),
        )
        .await
        .expect("completion must propagate");

        assert!(
            observer
                .snapshot_for_render()
                .tasks
                .iter()
                .any(|task| task.id == "task-1" && task.status.is_completed()),
            "completed row must remain visible until a deliberate archive/delete"
        );
        assert_eq!(observer.snapshot().tasks.len(), 1);
    }
}
