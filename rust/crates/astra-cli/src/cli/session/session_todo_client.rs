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

use astra_tools::task_mgmt::{SessionTask, TaskMutation, TaskStore};
use async_trait::async_trait;
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
//  3. `task` tool stuck in T2 deferred so the model never invoked it
//     even when the user asked for multi-step work.
//
// Each test wires a fresh wiremock server playing the role of
// astra-server's `/sessions/{sid}/todos*` endpoints, builds the real
// HttpTaskStore + TaskBoardObserver, and asserts the observable
// behaviour (mutations land within an SLA, completed tasks age out).
// No MatrixOne, no axum, no SSE.
// ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod wiring_e2e {
    use super::{HttpTaskStore, execute_todo_action};
    use crate::lock_recovery::LockRecovery;
    use crate::tui::task_board_observer::{COMPLETED_TASK_TTL, TaskBoardObserver};
    use astra_tools::task_mgmt::{SessionTask, TaskStore};
    use serde_json::{Value, json};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};
    use wiremock::matchers::method;
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
                            .get("status")
                            .or_else(|| args.get("new_status"))
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

    /// REGRESSION: `route_task_action` POSTs to the cloud on a `task.create`,
    /// fires the broadcast, and the observer picks the row up within the
    /// dirty/FAST_POLL window (≤ 200ms even on slow CI). This is the
    /// "dashboard appears mid-turn" SLA.
    #[tokio::test]
    async fn task_create_lands_in_observer_within_sla() {
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

    /// REGRESSION: completed tasks used to linger forever — only "all
    /// completed" triggered a board-wide hide. With per-task TTL, a row
    /// completed for longer than COMPLETED_TASK_TTL drops out of the
    /// render snapshot but the truth snapshot keeps it for counts/audit.
    /// We don't actually wait 30s; we force `completed_at` into the past
    /// and assert the partition.
    #[tokio::test]
    async fn completed_task_ages_out_of_render_snapshot_via_http_path() {
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
            &json!({ "task_id": "task-1", "status": "completed" }),
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

        // Fresh: the row stays in the render snapshot so the user sees
        // the ✔ land.
        assert_eq!(
            observer.snapshot_for_render().tasks.len(),
            1,
            "fresh completion must still render"
        );

        // Force the TTL into the past — equivalent to 31s having passed.
        observer
            .testing_force_completed_at_past("task-1", COMPLETED_TASK_TTL + Duration::from_secs(1));
        assert!(
            observer.snapshot_for_render().tasks.is_empty(),
            "completed row past TTL must drop from render snapshot"
        );
        // Truth snapshot still carries the row — counts (`/task list`,
        // header chip) reflect the full set.
        assert_eq!(observer.snapshot().tasks.len(), 1);
    }
}
