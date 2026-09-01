//! Live dashboard: embedded HTTP + WebSocket server for real-time
//! test progress visualization and run control.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{get, post};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, broadcast};

use crate::report::{CaseRunReport, SuiteReport};
use crate::runner::{RunOutcome, parse_strict_cli_outcome};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct OrchestratePlan {
    models: Vec<String>,
    cases: Vec<String>,
    parallel: u8,
    explanation: String,
}

fn parse_orchestrate_plan(text: &str) -> Result<OrchestratePlan, String> {
    let plan: OrchestratePlan = serde_json::from_str(text.trim())
        .map_err(|error| format!("planner response is not the required JSON plan: {error}"))?;
    if !(1..=8).contains(&plan.parallel) {
        return Err(format!(
            "planner response parallel must be between 1 and 8, got {}",
            plan.parallel
        ));
    }
    if plan.models.iter().any(|model| model.trim().is_empty()) {
        return Err("planner response contains an empty model name".to_string());
    }
    if plan.cases.iter().any(|case| case.trim().is_empty()) {
        return Err("planner response contains an empty case name".to_string());
    }
    if plan.explanation.trim().is_empty() {
        return Err("planner response explanation must not be empty".to_string());
    }
    Ok(plan)
}

fn parse_dashboard_chat_outcome(
    stdout: &str,
    stderr: &str,
    model: &str,
    process_exit: i32,
) -> Result<RunOutcome, String> {
    if stdout.trim().is_empty() {
        return Err(
            "chat subprocess returned empty stdout instead of a typed JSON envelope".into(),
        );
    }
    let mut outcome = parse_strict_cli_outcome(stdout, model)?;
    if outcome.exit_code != process_exit {
        return Err(format!(
            "chat envelope exit_code {} disagrees with process exit {}",
            outcome.exit_code, process_exit
        ));
    }
    outcome.stderr = stderr.to_string();
    Ok(outcome)
}

fn parse_dashboard_model_catalog(
    stdout: &[u8],
) -> Result<astra_services::ModelListPageResponse, String> {
    serde_json::from_slice::<astra_services::ModelListPageResponse>(stdout)
        .map_err(|error| format!("model list returned an invalid catalog envelope: {error}"))
}

async fn load_dashboard_model_catalog(
    admin_bin: &Path,
) -> Result<astra_services::ModelListPageResponse, String> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new(admin_bin)
            .args(["admin", "model", "list"])
            .env("NO_PROXY", "localhost,127.0.0.1")
            .env("no_proxy", "localhost,127.0.0.1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "model list timed out after 30s".to_string())?
    .map_err(|error| format!("model list spawn failed: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "model list exited {}: {}",
            output.status,
            stderr.trim()
        ));
    }
    parse_dashboard_model_catalog(&output.stdout)
}

// ── Event protocol (all events carry run_id for multi-run support) ──

#[derive(Debug, Clone)]
pub enum DashboardEvent {
    RunReset {
        run_id: String,
        sequence: u64,
    },
    SuiteStarted {
        run_id: String,
        total_cases: usize,
        models: Vec<String>,
        started_at: String,
        source: String, // "manual" or "orchestrate"
        sequence: u64,
    },
    CaseStarted {
        run_id: String,
        case_name: String,
        model: String,
        run_index: u32,
        sequence: u64,
    },
    /// A planned case is waiting for an execution permit.  Keeping this
    /// distinct from `case_started` prevents the UI from presenting a
    /// semaphore wait as a hung model call.
    CaseQueued {
        run_id: String,
        case_name: String,
        model: String,
        run_index: u32,
        queue_position: usize,
        sequence: u64,
    },
    /// Periodic liveness evidence for a case that has started.  This is
    /// intentionally a harness observation, not a claim that the model made
    /// semantic progress; the terminal report remains authoritative.
    CaseProgress {
        run_id: String,
        case_name: String,
        model: String,
        run_index: u32,
        phase: String,
        elapsed_ms: u64,
        sequence: u64,
    },
    CaseCompleted {
        run_id: String,
        report: Arc<CaseRunReport>,
        sequence: u64,
    },
    SuiteCompleted {
        run_id: String,
        report: Arc<SuiteReport>,
        sequence: u64,
    },
    SuiteFailed {
        run_id: String,
        error: String,
        sequence: u64,
    },
}

impl Serialize for DashboardEvent {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Self::RunReset { run_id, sequence } => {
                let mut m = s.serialize_map(Some(3))?;
                m.serialize_entry("type", "run_reset")?;
                m.serialize_entry("run_id", run_id)?;
                m.serialize_entry("sequence", sequence)?;
                m.end()
            }
            Self::SuiteStarted {
                run_id,
                total_cases,
                models,
                started_at,
                source,
                sequence,
            } => {
                let mut m = s.serialize_map(Some(7))?;
                m.serialize_entry("type", "suite_started")?;
                m.serialize_entry("run_id", run_id)?;
                m.serialize_entry("total_cases", total_cases)?;
                m.serialize_entry("models", models)?;
                m.serialize_entry("started_at", started_at)?;
                m.serialize_entry("source", source)?;
                m.serialize_entry("sequence", sequence)?;
                m.end()
            }
            Self::CaseStarted {
                run_id,
                case_name,
                model,
                run_index,
                sequence,
            } => {
                let mut m = s.serialize_map(Some(6))?;
                m.serialize_entry("type", "case_started")?;
                m.serialize_entry("run_id", run_id)?;
                m.serialize_entry("case_name", case_name)?;
                m.serialize_entry("model", model)?;
                m.serialize_entry("run_index", run_index)?;
                m.serialize_entry("sequence", sequence)?;
                m.end()
            }
            Self::CaseQueued {
                run_id,
                case_name,
                model,
                run_index,
                queue_position,
                sequence,
            } => {
                let mut m = s.serialize_map(Some(7))?;
                m.serialize_entry("type", "case_queued")?;
                m.serialize_entry("run_id", run_id)?;
                m.serialize_entry("case_name", case_name)?;
                m.serialize_entry("model", model)?;
                m.serialize_entry("run_index", run_index)?;
                m.serialize_entry("queue_position", queue_position)?;
                m.serialize_entry("sequence", sequence)?;
                m.end()
            }
            Self::CaseProgress {
                run_id,
                case_name,
                model,
                run_index,
                phase,
                elapsed_ms,
                sequence,
            } => {
                let mut m = s.serialize_map(Some(8))?;
                m.serialize_entry("type", "case_progress")?;
                m.serialize_entry("run_id", run_id)?;
                m.serialize_entry("case_name", case_name)?;
                m.serialize_entry("model", model)?;
                m.serialize_entry("run_index", run_index)?;
                m.serialize_entry("phase", phase)?;
                m.serialize_entry("elapsed_ms", elapsed_ms)?;
                m.serialize_entry("sequence", sequence)?;
                m.end()
            }
            Self::CaseCompleted {
                run_id,
                report,
                sequence,
            } => {
                let mut m = s.serialize_map(Some(4))?;
                m.serialize_entry("type", "case_completed")?;
                m.serialize_entry("run_id", run_id)?;
                m.serialize_entry("report", report.as_ref())?;
                m.serialize_entry("sequence", sequence)?;
                m.end()
            }
            Self::SuiteCompleted {
                run_id,
                report,
                sequence,
            } => {
                let mut m = s.serialize_map(Some(4))?;
                m.serialize_entry("type", "suite_completed")?;
                m.serialize_entry("run_id", run_id)?;
                m.serialize_entry("report", report.as_ref())?;
                m.serialize_entry("sequence", sequence)?;
                m.end()
            }
            Self::SuiteFailed {
                run_id,
                error,
                sequence,
            } => {
                let mut m = s.serialize_map(Some(4))?;
                m.serialize_entry("type", "suite_failed")?;
                m.serialize_entry("run_id", run_id)?;
                m.serialize_entry("error", error)?;
                m.serialize_entry("sequence", sequence)?;
                m.end()
            }
        }
    }
}

// ── Server configuration ────────────────────────────────────────────

#[derive(Clone)]
pub struct DashboardConfig {
    pub suite_dir: PathBuf,
    pub astra_bin: PathBuf,
    pub available_models: Vec<String>,
    pub judger_model: String,
}

#[derive(Clone)]
struct AppState {
    tx: broadcast::Sender<DashboardEvent>,
    config: Arc<DashboardConfig>,
    running: Arc<AtomicBool>,
    cancel_flag: Arc<AtomicBool>,
    snapshot: Arc<Mutex<DashboardSnapshot>>,
    run_counter: Arc<std::sync::atomic::AtomicU32>,
    ws_connections: Arc<AtomicUsize>,
}

#[derive(Debug, Clone, Default)]
struct DashboardSnapshot {
    run_id: Option<String>,
    running: bool,
    report: Option<SuiteReport>,
    error: Option<String>,
    sequence: u64,
    progress: DashboardProgressSnapshot,
}

#[derive(Debug, Clone, Default, Serialize)]
struct DashboardProgressSnapshot {
    total_cases: usize,
    completed_cases: usize,
    queued: BTreeMap<String, DashboardCaseProgress>,
    active: BTreeMap<String, DashboardCaseProgress>,
}

#[derive(Debug, Clone, Serialize)]
struct DashboardCaseProgress {
    case_name: String,
    model: String,
    run_index: u32,
    queue_position: usize,
    phase: String,
    elapsed_ms: u64,
}

fn dashboard_case_key(case_name: &str, model: &str, run_index: u32) -> String {
    format!("{case_name}\u{001f}{model}\u{001f}{run_index}")
}

fn apply_dashboard_event_to_snapshot(snapshot: &mut DashboardSnapshot, event: &DashboardEvent) {
    match event {
        DashboardEvent::RunReset { run_id, sequence } => {
            snapshot.run_id = Some(run_id.clone());
            // Admission has succeeded by the time RunReset is published;
            // keep reconnects in the running phase while the suite is being
            // assembled and before SuiteStarted arrives.
            snapshot.running = true;
            snapshot.report = None;
            snapshot.error = None;
            snapshot.progress = DashboardProgressSnapshot::default();
            snapshot.sequence = *sequence;
        }
        DashboardEvent::SuiteStarted {
            run_id,
            total_cases,
            sequence,
            ..
        } => {
            snapshot.run_id = Some(run_id.clone());
            snapshot.running = true;
            snapshot.report = None;
            snapshot.error = None;
            snapshot.progress = DashboardProgressSnapshot {
                total_cases: *total_cases,
                ..Default::default()
            };
            snapshot.sequence = *sequence;
        }
        DashboardEvent::CaseQueued {
            run_id,
            case_name,
            model,
            run_index,
            queue_position,
            sequence,
        } => {
            snapshot.run_id = Some(run_id.clone());
            let key = dashboard_case_key(case_name, model, *run_index);
            let progress = DashboardCaseProgress {
                case_name: case_name.clone(),
                model: model.clone(),
                run_index: *run_index,
                queue_position: *queue_position,
                phase: "queued".into(),
                elapsed_ms: 0,
            };
            snapshot.progress.active.remove(&key);
            snapshot.progress.queued.insert(key, progress);
            snapshot.sequence = *sequence;
        }
        DashboardEvent::CaseStarted {
            run_id,
            case_name,
            model,
            run_index,
            sequence,
        } => {
            snapshot.run_id = Some(run_id.clone());
            let key = dashboard_case_key(case_name, model, *run_index);
            let queue_position = snapshot
                .progress
                .queued
                .remove(&key)
                .map(|progress| progress.queue_position)
                .unwrap_or_default();
            snapshot.progress.active.insert(
                key,
                DashboardCaseProgress {
                    case_name: case_name.clone(),
                    model: model.clone(),
                    run_index: *run_index,
                    queue_position,
                    phase: "executing".into(),
                    elapsed_ms: 0,
                },
            );
            snapshot.sequence = *sequence;
        }
        DashboardEvent::CaseProgress {
            run_id,
            case_name,
            model,
            run_index,
            phase,
            elapsed_ms,
            sequence,
        } => {
            snapshot.run_id = Some(run_id.clone());
            let key = dashboard_case_key(case_name, model, *run_index);
            if let Some(progress) = snapshot.progress.active.get_mut(&key) {
                progress.phase = phase.clone();
                progress.elapsed_ms = *elapsed_ms;
            }
            snapshot.sequence = *sequence;
        }
        DashboardEvent::CaseCompleted {
            run_id,
            report,
            sequence,
        } => {
            snapshot.run_id = Some(run_id.clone());
            let key = dashboard_case_key(&report.case_name, &report.model, report.run_index);
            snapshot.progress.queued.remove(&key);
            snapshot.progress.active.remove(&key);
            snapshot.progress.completed_cases = snapshot.progress.completed_cases.saturating_add(1);
            snapshot.sequence = *sequence;
        }
        DashboardEvent::SuiteCompleted {
            run_id,
            report,
            sequence,
        } => {
            snapshot.run_id = Some(run_id.clone());
            snapshot.running = false;
            snapshot.report = Some(report.as_ref().clone());
            snapshot.error = None;
            snapshot.progress = DashboardProgressSnapshot::default();
            snapshot.sequence = *sequence;
        }
        DashboardEvent::SuiteFailed {
            run_id,
            error,
            sequence,
        } => {
            snapshot.run_id = Some(run_id.clone());
            snapshot.running = false;
            snapshot.report = None;
            snapshot.error = Some(error.clone());
            snapshot.progress = DashboardProgressSnapshot::default();
            snapshot.sequence = *sequence;
        }
    }
}

static DASHBOARD_EVENT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn next_dashboard_event_sequence() -> u64 {
    DASHBOARD_EVENT_SEQUENCE.fetch_add(1, Ordering::AcqRel) + 1
}

fn snapshot_json(snapshot: &DashboardSnapshot) -> serde_json::Value {
    let phase = if snapshot.running {
        "running"
    } else if snapshot.error.is_some() {
        "failed"
    } else if snapshot.report.is_some() {
        "completed"
    } else {
        "idle"
    };
    serde_json::json!({
        "type": "init",
        "phase": phase,
        "running": snapshot.running,
        "run_id": snapshot.run_id,
        "sequence": snapshot.sequence,
        "progress": snapshot.progress,
        "report": snapshot.report,
        "error": snapshot.error,
    })
}

/// Maximum number of concurrent WebSocket connections.
const MAX_WS_CONNECTIONS: usize = 100;

pub struct DashboardServer {
    tx: broadcast::Sender<DashboardEvent>,
    config: Arc<DashboardConfig>,
}

impl DashboardServer {
    pub fn new(config: DashboardConfig) -> Self {
        let (tx, _) = broadcast::channel(512);
        Self {
            tx,
            config: Arc::new(config),
        }
    }

    pub fn sender(&self) -> broadcast::Sender<DashboardEvent> {
        self.tx.clone()
    }

    pub async fn start(self, port: u16) -> anyhow::Result<()> {
        let state = AppState {
            tx: self.tx,
            config: self.config,
            running: Arc::new(AtomicBool::new(false)),
            cancel_flag: Arc::new(AtomicBool::new(false)),
            snapshot: Arc::new(Mutex::new(DashboardSnapshot::default())),
            run_counter: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            ws_connections: Arc::new(AtomicUsize::new(0)),
        };
        // Keep reconnect snapshots aligned with the same ordered event
        // protocol sent to browsers.  This avoids a refresh reducing a live
        // run to the ambiguous boolean "running=true".
        let mut snapshot_rx = state.tx.subscribe();
        let snapshot_for_events = state.snapshot.clone();
        tokio::spawn(async move {
            loop {
                match snapshot_rx.recv().await {
                    Ok(event) => {
                        let mut snapshot = snapshot_for_events.lock().await;
                        apply_dashboard_event_to_snapshot(&mut snapshot, &event);
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // A lagged subscriber cannot safely reconstruct the
                        // missing lifecycle prefix. The terminal snapshot or
                        // the next run reset remains authoritative; keep the
                        // last known state rather than inventing progress.
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        let app = Router::new()
            .route("/", get(index_handler))
            .route("/ws", get(ws_handler))
            .route("/api/config", get(config_handler))
            .route("/api/models", get(models_handler))
            .route("/api/cases", get(cases_handler))
            .route("/api/run", post(run_handler))
            .route("/api/cancel", post(cancel_handler))
            .route("/api/login", post(login_handler))
            .route("/api/chat", post(chat_handler))
            .route("/api/orchestrate", post(orchestrate_handler))
            .route("/api/report", get(report_handler))
            .route("/api/analyze", post(analyze_handler))
            .route("/api/eval", get(eval_handler))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
        eprintln!("[astra-test] dashboard: http://localhost:{port}");
        axum::serve(listener, app).await?;
        Ok(())
    }
}

// ── Handlers ────────────────────────────────────────────────────────

async fn index_handler() -> Html<&'static str> {
    Html(include_str!("../dashboard.html"))
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    let current = state.ws_connections.load(Ordering::Relaxed);
    if current >= MAX_WS_CONNECTIONS {
        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    ws.on_upgrade(move |socket| handle_ws(socket, state))
        .into_response()
}

async fn handle_ws(mut socket: WebSocket, state: AppState) {
    state.ws_connections.fetch_add(1, Ordering::Relaxed);
    // Ensure the counter is decremented when this function exits for any reason.
    struct WsGuard(Arc<AtomicUsize>);
    impl Drop for WsGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::Relaxed);
        }
    }
    let _ws_guard = WsGuard(state.ws_connections.clone());

    // Subscribe before reading the snapshot. Events that occur while the
    // snapshot is being sent remain queued and are delivered after init;
    // a terminal event can therefore never fall into a snapshot gap.
    let mut rx = state.tx.subscribe();
    let snapshot = state.snapshot.lock().await.clone();
    let init = snapshot_json(&snapshot);
    let _ = socket.send(Message::Text(init.to_string().into())).await;
    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(event) => {
                        let json = serde_json::to_string(&event).unwrap_or_default();
                        if socket.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        let snapshot = state.snapshot.lock().await.clone();
                        let mut msg = snapshot_json(&snapshot);
                        msg["resync_reason"] = serde_json::json!("lagged");
                        msg["missed"] = serde_json::json!(n);
                        let _ = socket.send(Message::Text(msg.to_string().into())).await;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(_)) => {}
                    _ => break,
                }
            }
        }
    }
}

async fn config_handler(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "suite_dir": state.config.suite_dir.display().to_string(),
        "available_models": state.config.available_models,
        "judger_model": state.config.judger_model,
    }))
}

async fn models_handler(State(state): State<AppState>) -> Json<serde_json::Value> {
    match load_dashboard_model_catalog(&state.config.astra_bin).await {
        Ok(page) => Json(serde_json::json!({
            "models": page.items,
            "total": page.total,
            "limit": page.limit,
            "catalog_revision": page.catalog_revision,
        })),
        Err(error) => Json(serde_json::json!({"models": [], "error": error})),
    }
}

async fn cases_handler(State(state): State<AppState>) -> Json<serde_json::Value> {
    match crate::case::Case::load_dir(&state.config.suite_dir) {
        Ok(cases) => {
            let items: Vec<serde_json::Value> = cases
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "name": c.name,
                        "description": c.description,
                        "capability": c.capability.as_ref().map(|cap| cap.to_string()),
                        "difficulty": c.difficulty,
                        "weight": c.weight,
                        "timeout_seconds": c.timeout_seconds,
                        "has_steps": !c.steps.is_empty(),
                        "criteria_count": c.criteria.len(),
                        "has_own_models": c.models.is_some(),
                    })
                })
                .collect();
            Json(serde_json::json!({"cases": items}))
        }
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

async fn report_handler(State(state): State<AppState>) -> Json<serde_json::Value> {
    let snapshot = state.snapshot.lock().await;
    Json(serde_json::json!({
        "run_id": snapshot.run_id,
        "running": snapshot.running,
        "error": snapshot.error,
        "report": snapshot.report,
    }))
}

#[derive(Debug, Deserialize)]
struct RunRequest {
    models: Vec<String>,
    #[serde(default)]
    cases: Vec<String>,
    #[serde(default)]
    filter: Option<String>,
    #[serde(default = "default_parallel")]
    parallel: usize,
    #[serde(default)]
    no_judger: bool,
    #[serde(default)]
    judger_model: Option<String>,
    /// Request origin tag ("manual" / "orchestrate"). Deserialized
    /// for API completeness but not consumed by `execute_run`.
    #[serde(default)]
    #[allow(dead_code)]
    source: Option<String>,
}

fn default_parallel() -> usize {
    // Match the CLI's safe deterministic default. Parallel execution is an
    // explicit opt-in because cases may share a workspace or external quota.
    1
}

async fn run_handler(State(state): State<AppState>, Json(mut req): Json<RunRequest>) -> Response {
    let cases = match crate::case::Case::load_dir(&state.config.suite_dir) {
        Ok(cases) => cases,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("cannot load cases: {error}"),
                })),
            )
                .into_response();
        }
    };
    let selected_cases = if !req.cases.is_empty() {
        cases
            .iter()
            .filter(|case| req.cases.iter().any(|name| name == &case.name))
            .count()
    } else if let Some(filter) = req.filter.as_deref() {
        cases
            .iter()
            .filter(|case| crate::case::matches_filter(&case.name, filter))
            .count()
    } else {
        cases.len()
    };
    if selected_cases == 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "No cases matched the selection",
            })),
        )
            .into_response();
    }
    // H3: reject early when no models are specified and no cases have their own models.
    if req.models.is_empty() {
        let cases_have_models = cases.iter().any(|c| c.models.is_some());
        if !cases_have_models {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "No models specified"})),
            )
                .into_response();
        }
    }

    // Resolve the execution identity before admitting an asynchronous run.
    // An invalid owner/profile is a request failure, never a later empty
    // suite result.
    if let Err(error) = crate::runner::resolve_runner_profile_owner(None) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": format!("cannot resolve runner profile: {error}"),
            })),
        )
            .into_response();
    }

    // M7: clamp parallel to a sane range.
    req.parallel = req.parallel.clamp(1, 32);

    // Atomically swap false→true. If it was already true, a run is in progress.
    if state
        .running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "A run is already in progress"})),
        )
            .into_response();
    }

    state.cancel_flag.store(false, Ordering::SeqCst);

    let run_num = state.run_counter.fetch_add(1, Ordering::SeqCst);
    let run_id = format!("run-{run_num}");
    let sequence = next_dashboard_event_sequence();

    {
        let mut snapshot = state.snapshot.lock().await;
        snapshot.run_id = Some(run_id.clone());
        snapshot.running = true;
        snapshot.report = None;
        snapshot.error = None;
        snapshot.sequence = sequence;
    }

    let _ = state.tx.send(DashboardEvent::RunReset {
        run_id: run_id.clone(),
        sequence,
    });

    let config = state.config.clone();
    let tx = state.tx.clone();
    let running_flag = state.running.clone();
    let cancel_flag = state.cancel_flag.clone();
    let snapshot = state.snapshot.clone();
    let rid = run_id.clone();

    tokio::spawn(async move {
        // M3: ensure running_flag is always reset, even if the body panics.
        struct RunGuard(Arc<AtomicBool>);
        impl Drop for RunGuard {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _guard = RunGuard(running_flag);

        let result =
            std::panic::AssertUnwindSafe(execute_run(config, tx.clone(), req, cancel_flag, &rid))
                .catch_unwind()
                .await;
        match result {
            Ok(Ok(report)) => {
                let sequence = next_dashboard_event_sequence();
                {
                    let mut current = snapshot.lock().await;
                    current.running = false;
                    current.report = Some(report.clone());
                    current.error = None;
                    current.sequence = sequence;
                }
                let _ = tx.send(DashboardEvent::SuiteCompleted {
                    run_id: rid,
                    report: Arc::new(report),
                    sequence,
                });
            }
            Ok(Err(e)) => {
                publish_dashboard_failure(&snapshot, &tx, &rid, e.to_string()).await;
            }
            Err(_) => {
                publish_dashboard_failure(
                    &snapshot,
                    &tx,
                    &rid,
                    "dashboard run panicked before producing a terminal report".into(),
                )
                .await;
            }
        }
        // The RunGuard drop releases admission only after the terminal
        // snapshot and event are committed.
    });

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"status": "started", "run_id": run_id})),
    )
        .into_response()
}

async fn publish_dashboard_failure(
    snapshot: &Arc<Mutex<DashboardSnapshot>>,
    tx: &broadcast::Sender<DashboardEvent>,
    run_id: &str,
    error: String,
) {
    eprintln!("[astra-test] dashboard run error: {error}");
    let sequence = next_dashboard_event_sequence();
    {
        let mut current = snapshot.lock().await;
        current.running = false;
        current.report = None;
        current.error = Some(error.clone());
        current.sequence = sequence;
    }
    let _ = tx.send(DashboardEvent::SuiteFailed {
        run_id: run_id.to_string(),
        error,
        sequence,
    });
}

async fn cancel_handler(State(state): State<AppState>) -> Json<serde_json::Value> {
    if state.running.load(Ordering::Acquire) {
        state.cancel_flag.store(true, Ordering::SeqCst);
        Json(serde_json::json!({"status": "cancelling"}))
    } else {
        Json(serde_json::json!({"error": "No run in progress"}))
    }
}

/// Structured capability evaluation with numeric scores.
async fn eval_handler(State(state): State<AppState>) -> Json<serde_json::Value> {
    let report = state.snapshot.lock().await.report.clone();
    match report.as_ref() {
        Some(r) => {
            let eval = crate::eval::evaluate(r);
            Json(serde_json::json!({"eval": eval}))
        }
        None => Json(serde_json::json!({"error": "No run results to evaluate"})),
    }
}

/// Run the 5-dimension LLM analysis on the latest report.
#[derive(Debug, Deserialize)]
struct AnalyzeRequest {
    #[serde(default)]
    model: Option<String>,
}

async fn analyze_handler(
    State(state): State<AppState>,
    Json(req): Json<AnalyzeRequest>,
) -> Json<serde_json::Value> {
    let report = state.snapshot.lock().await.report.clone();
    let Some(r) = report else {
        return Json(serde_json::json!({"error": "No run results to analyze"}));
    };

    let model = req.model.as_deref().unwrap_or("claude-sonnet-4-6");
    match crate::summarizer::summarize(&state.config.astra_bin, None, model, &r, 300).await {
        Ok(text) => Json(serde_json::json!({"analysis": text})),
        Err(e) => Json(serde_json::json!({"error": e})),
    }
}

#[derive(Debug, Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

async fn login_handler(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Json<serde_json::Value> {
    let admin_bin = &state.config.astra_bin;
    let output = tokio::process::Command::new(admin_bin)
        .args([
            "admin",
            "login",
            "--username",
            &req.username,
            "--password",
            &req.password,
        ])
        .env("NO_PROXY", "localhost,127.0.0.1")
        .env("no_proxy", "localhost,127.0.0.1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await;
    match output {
        Ok(o) if o.status.success() => Json(serde_json::json!({
            "status": "ok",
            "username": req.username,
            "message": "Logged in successfully"
        })),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            let detail = if stderr.contains("Invalid") {
                "Invalid username or password"
            } else {
                "Login failed"
            };
            Json(serde_json::json!({"error": detail}))
        }
        Err(e) => Json(serde_json::json!({"error": format!("spawn: {e}")})),
    }
}

#[derive(Debug, Deserialize)]
struct ChatRequest {
    message: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    run_id: Option<String>,
}

async fn chat_handler(
    State(state): State<AppState>,
    Json(req): Json<ChatRequest>,
) -> Json<serde_json::Value> {
    use tokio::process::Command;

    let model = req.model.as_deref().unwrap_or("claude-sonnet-4-6");
    eprintln!(
        "[astra-test] chat: model={model} session_id={:?} run_id={:?} msg_len={}",
        req.session_id.as_deref().unwrap_or("(new)"),
        req.run_id.as_deref().unwrap_or("-"),
        req.message.len()
    );

    let cases_summary = crate::case::Case::load_dir(&state.config.suite_dir)
        .map(|cs| {
            cs.iter()
                .map(|c| {
                    format!(
                        "- {} (cap={}, d={}, steps={})",
                        c.name,
                        c.capability
                            .as_ref()
                            .map(|cap| cap.to_string())
                            .unwrap_or("-".into()),
                        c.difficulty.unwrap_or(0),
                        c.steps.len()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    let report_ctx = {
        let report = state.snapshot.lock().await.report.clone();
        if let Some(ref r) = report {
            let runs: Vec<String> = r
                .runs
                .iter()
                .map(|run| {
                    let score = run
                        .criteria
                        .iter()
                        .find_map(|c| c.score)
                        .map(|s| format!(" score={s:.2}"))
                        .unwrap_or_default();
                    format!(
                        "  {} × {}: {} | exit={} tok={} dur={}ms turns={}{score}{}",
                        run.case_name,
                        crate::report::normalize_model_display(&run.model),
                        match run.status {
                            crate::report::CaseRunStatus::Passed => "PASS",
                            crate::report::CaseRunStatus::Failed => "FAIL",
                            crate::report::CaseRunStatus::Cancelled => "CANCELLED",
                            crate::report::CaseRunStatus::Unavailable => "UNAVAILABLE",
                        },
                        run.outcome.exit_code,
                        run.outcome.prompt_tokens + run.outcome.completion_tokens,
                        run.outcome.duration_ms,
                        run.outcome.turn_rounds,
                        run.failure_class
                            .as_ref()
                            .map(|c| format!(" class={c}"))
                            .unwrap_or_default()
                    )
                })
                .collect();
            format!(
                "\n\nLatest run results ({} total, {} passed, {} failed, {} cancelled, {} unavailable, wall={}ms):\n{}",
                r.total(),
                r.passed(),
                r.failed(),
                r.cancelled(),
                r.unavailable(),
                r.wall_time_ms,
                runs.join("\n")
            )
        } else {
            String::new()
        }
    };

    let system_ctx = format!(
        "You are an expert test engineer analyzing astra-test-harness results.\n\
         Available test cases:\n{cases_summary}\n\n\
         Suite directory: {}{report_ctx}\n\n\
         When the user asks about test results, failures, or comparisons, \
         give specific, actionable analysis citing case names and metrics. \
         Use markdown formatting for readability.\n\n\
         If the user asks to run tests, compare models, or create a test workflow, \
         respond with a concrete plan and tell them you can execute it. \
         Include the exact models and case names in your suggestion.",
        state.config.suite_dir.display()
    );

    let full_message = if req.session_id.is_none() {
        format!("[System context]\n{system_ctx}\n\n[User]\n{}", req.message)
    } else {
        req.message.clone()
    };

    let mut cmd = Command::new(&state.config.astra_bin);
    cmd.arg("chat")
        .arg("-m")
        .arg(&full_message)
        .arg("--model")
        .arg(model)
        .arg("--json")
        .arg("-y");
    if let Some(ref sid) = req.session_id {
        cmd.arg("--session-id").arg(sid);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let output = match tokio::time::timeout(std::time::Duration::from_secs(300), cmd.output()).await
    {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Json(serde_json::json!({"error": format!("spawn: {e}")})),
        Err(_) => {
            return Json(serde_json::json!({
                "error": "Chat timed out after 5 minutes. Try a shorter question or a faster model."
            }));
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code().unwrap_or(-1);

    if exit_code == 3 {
        return Json(serde_json::json!({
            "error": "Authentication failed — credentials may have expired. Click Login to refresh.",
            "exit_code": 3,
        }));
    }

    if !output.status.success() {
        return Json(serde_json::json!({
            "error": format!("astra chat exited {}: {}", output.status, stderr.trim()),
            "exit_code": exit_code,
        }));
    }

    match parse_dashboard_chat_outcome(&stdout, &stderr, model, exit_code) {
        Ok(outcome) => Json(serde_json::to_value(outcome).unwrap_or_else(
            |error| serde_json::json!({"error": format!("serialize chat outcome: {error}")}),
        )),
        Err(error) => Json(serde_json::json!({"error": error})),
    }
}

#[derive(Debug, Deserialize)]
struct OrchestrateRequest {
    instruction: String,
    #[serde(default)]
    model: Option<String>,
}

async fn orchestrate_handler(
    State(state): State<AppState>,
    Json(req): Json<OrchestrateRequest>,
) -> Json<serde_json::Value> {
    let cases = crate::case::Case::load_dir(&state.config.suite_dir)
        .map(|cs| {
            cs.iter()
                .map(|c| {
                    format!(
                        "{}|{}|{}",
                        c.name,
                        c.capability
                            .as_ref()
                            .map(|cap| cap.to_string())
                            .unwrap_or("-".into()),
                        c.difficulty.unwrap_or(0)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    let admin_bin = &state.config.astra_bin;
    let model_catalog = match load_dashboard_model_catalog(admin_bin).await {
        Ok(page) => page,
        Err(error) => return Json(serde_json::json!({"error": error})),
    };
    let models_list = model_catalog
        .items
        .iter()
        .map(|model| model.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    let prompt = format!(
        "You are a test harness planner. Given an instruction, output ONLY a JSON object with:\n\
         - \"models\": array of model name strings to test\n\
         - \"cases\": array of case name strings to run\n\
         - \"parallel\": number (1-8)\n\
         - \"explanation\": one sentence explaining your selection\n\n\
         Available models: {models_list}\n\n\
         Available cases (name|capability|difficulty):\n{cases}\n\n\
         Instruction: {}\n\n\
         Output ONLY the JSON, no markdown fences.",
        req.instruction
    );

    let model = req.model.as_deref().unwrap_or("claude-sonnet-4-6");
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(300),
        tokio::process::Command::new(&state.config.astra_bin)
            .args(["chat", "-m", &prompt, "--model", model, "--json", "-y"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await;

    match output {
        Ok(Ok(o)) => {
            let exit_code = o.status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&o.stdout);
            let stderr = String::from_utf8_lossy(&o.stderr);

            if exit_code == 3 {
                return Json(serde_json::json!({
                    "error": "Authentication failed — Click Login to refresh."
                }));
            }
            if !o.status.success() {
                return Json(serde_json::json!({
                    "error": format!("astra exited {} — {}", exit_code, stderr.chars().take(200).collect::<String>())
                }));
            }

            let outcome = match parse_dashboard_chat_outcome(&stdout, &stderr, model, exit_code) {
                Ok(outcome) => outcome,
                Err(error) => return Json(serde_json::json!({"error": error})),
            };
            match parse_orchestrate_plan(&outcome.text) {
                Ok(plan) => Json(serde_json::json!({"plan": plan})),
                Err(error) => Json(serde_json::json!({"error": error})),
            }
        }
        Ok(Err(e)) => Json(serde_json::json!({"error": format!("orchestrate: {e}")})),
        Err(_) => Json(serde_json::json!({
            "error": "Orchestrate timed out after 5 minutes. Try a simpler instruction or a faster model."
        })),
    }
}

/// Execute a full test run with cancel support.
async fn execute_run(
    config: Arc<DashboardConfig>,
    tx: broadcast::Sender<DashboardEvent>,
    req: RunRequest,
    cancel_flag: Arc<AtomicBool>,
    run_id: &str,
) -> anyhow::Result<SuiteReport> {
    use crate::case::Case;
    use crate::digest::AstraCliDigestCollector;
    use crate::exec::AstraCliExecutor;
    use crate::judger::{AstraCliJudger, JudgerConfig};
    use crate::runner::{RunnerConfig, resolve_runner_profile_owner};
    use crate::suite::{ScopedDiskSessionLoader, SessionCaptureMode, SuiteConfig, SuiteRunner};

    let mut cases = Case::load_dir(&config.suite_dir)?;

    if !req.cases.is_empty() {
        let selected: std::collections::HashSet<&str> =
            req.cases.iter().map(|s| s.as_str()).collect();
        cases.retain(|c| selected.contains(c.name.as_str()));
    } else if let Some(ref pattern) = req.filter {
        cases.retain(|c| crate::case::matches_filter(&c.name, pattern));
    }

    if cases.is_empty() {
        anyhow::bail!("no cases matched the selection");
    }

    if !req.models.is_empty() {
        for case in &mut cases {
            // Only override models for cases that don't specify their own.
            // Cases with explicit models: [...] are model-specific tests
            // (e.g. fork_prefix_provider_mismatch requires Anthropic parent).
            if case.models.is_none() {
                case.models = Some(req.models.clone());
            }
        }
    }

    let fallback_models = req.models.clone();
    let mut runner_cfg = RunnerConfig::new(config.astra_bin.clone())
        .with_fallback_models(fallback_models)
        .with_required_session_subsystem_health();
    runner_cfg.working_dir = None;
    let runner_identity = resolve_runner_profile_owner(None).map_err(anyhow::Error::msg)?;
    astra_services::configure_local_owner_scope(runner_identity.local_owner_scope.clone());
    runner_cfg.profile = Some(runner_identity.profile_name);
    runner_cfg.artifact_owner_scopes = runner_identity.artifact_owner_scopes.clone();

    let judger_model = req.judger_model.as_deref().unwrap_or(&config.judger_model);
    let judger_cfg = JudgerConfig::new(config.astra_bin.clone(), judger_model);
    let judger = AstraCliJudger::new(judger_cfg);
    let executor = AstraCliExecutor::new(runner_cfg.clone());
    let session_loader = ScopedDiskSessionLoader::new(runner_identity.artifact_owner_scopes);
    let digest = AstraCliDigestCollector::new(config.astra_bin.clone())
        .with_profile(runner_cfg.profile.clone());

    let suite_cfg = SuiteConfig {
        parallel: req.parallel.max(1),
        circuit_breaker_threshold: 3,
        retry_on_429: false,
        runs: 1,
    };

    // SuiteStarted is emitted by runner.run_all() — don't duplicate here.
    let runner = SuiteRunner {
        executor: &executor,
        judger: &judger,
        session_loader: &session_loader,
        digest_collector: Some(&digest),
        runner_cfg,
        no_judger: req.no_judger,
        session_mode: SessionCaptureMode::OnDebugLog,
        suite_cfg,
        dashboard_tx: Some(tx),
        run_id: run_id.to_string(),
        cancel_flag: Some(cancel_flag),
    };

    let report = runner.run_all(&cases).await;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_chat_payload() -> String {
        serde_json::json!({
            "trace_id": null,
            "request_id": null,
            "run_id": "run-1",
            "text": "typed response",
            "final_state": "completed",
            "interruption_kind": null,
            "tool_result_class_counts": {},
            "prompt_tokens": 7,
            "fresh_prompt_tokens": 7,
            "cache": {"hit": false, "read_tokens": 0, "creation_tokens": 0},
            "completion_tokens": 3,
            "llm_rounds": 1,
            "exit_code": 0,
            "tool_calls_count": 0,
            "tools_used": [],
            "persistence_error": null,
            "success": true,
            "error_kind": null,
            "session_id": "00000000-0000-4000-8000-000000000001"
        })
        .to_string()
    }

    #[test]
    fn dashboard_chat_requires_typed_success_envelope() {
        let outcome = parse_dashboard_chat_outcome(&valid_chat_payload(), "m", "model", 0)
            .expect("typed chat envelope");
        assert_eq!(outcome.text, "typed response");
        assert!(parse_dashboard_chat_outcome("{}", "", "model", 0).is_err());
        assert!(parse_dashboard_chat_outcome("raw response", "", "model", 0).is_err());
        assert!(parse_dashboard_chat_outcome(&valid_chat_payload(), "", "model", 1).is_err());
    }

    #[test]
    fn dashboard_model_catalog_requires_production_page_shape() {
        let page = serde_json::json!({
            "items": [],
            "next_cursor": null,
            "limit": 200,
            "total": 0,
            "catalog_revision": "sha256:test"
        });
        let parsed = parse_dashboard_model_catalog(page.to_string().as_bytes())
            .expect("production model page envelope");
        assert_eq!(parsed.total, 0);
        assert!(parse_dashboard_model_catalog(br"[]").is_err());
        assert!(parse_dashboard_model_catalog(br#"{"items":{}}"#).is_err());
    }

    #[test]
    fn dashboard_planner_rejects_fallback_text_and_invalid_plan_shape() {
        let valid = r#"{"models":["model"],"cases":[],"parallel":2,"explanation":"run it"}"#;
        let plan = parse_orchestrate_plan(valid).expect("typed plan");
        assert_eq!(plan.parallel, 2);
        assert!(parse_orchestrate_plan("prefix {\"models\":[]} suffix").is_err());
        assert!(
            parse_orchestrate_plan(
                r#"{"models":"model","cases":null,"parallel":99,"explanation":"run it"}"#
            )
            .is_err()
        );
        assert!(
            parse_orchestrate_plan(
                r#"{"models":["model"],"cases":[],"parallel":2,"explanation":""}"#
            )
            .is_err()
        );
    }

    #[test]
    fn dashboard_run_default_matches_cli_safe_serial_execution() {
        let request: RunRequest = serde_json::from_value(serde_json::json!({
            "models": ["model"],
            "cases": []
        }))
        .expect("dashboard request should deserialize");
        assert_eq!(request.parallel, 1);
    }

    #[test]
    fn dashboard_failure_is_an_explicit_terminal_event() {
        let json = serde_json::to_value(DashboardEvent::SuiteFailed {
            run_id: "run-7".into(),
            error: "no cases matched the selection".into(),
            sequence: 9,
        })
        .expect("serialize failure event");
        assert_eq!(json["type"], "suite_failed");
        assert_eq!(json["run_id"], "run-7");
        assert_eq!(json["error"], "no cases matched the selection");
        assert_eq!(json["sequence"], 9);
    }

    #[test]
    fn dashboard_queue_and_progress_events_are_typed_and_distinct() {
        let queued = serde_json::to_value(DashboardEvent::CaseQueued {
            run_id: "run-8".into(),
            case_name: "slow-case".into(),
            model: "model".into(),
            run_index: 0,
            queue_position: 3,
            sequence: 10,
        })
        .expect("serialize queue event");
        assert_eq!(queued["type"], "case_queued");
        assert_eq!(queued["queue_position"], 3);

        let progress = serde_json::to_value(DashboardEvent::CaseProgress {
            run_id: "run-8".into(),
            case_name: "slow-case".into(),
            model: "model".into(),
            run_index: 0,
            phase: "executing".into(),
            elapsed_ms: 5000,
            sequence: 11,
        })
        .expect("serialize progress event");
        assert_eq!(progress["type"], "case_progress");
        assert_eq!(progress["phase"], "executing");
        assert_eq!(progress["elapsed_ms"], 5000);
        assert_ne!(queued["type"], progress["type"]);
    }

    #[test]
    fn dashboard_snapshot_keeps_live_queue_after_reconnect() {
        let mut snapshot = DashboardSnapshot::default();
        let started = DashboardEvent::SuiteStarted {
            run_id: "run-reconnect".into(),
            total_cases: 2,
            models: vec!["m".into()],
            started_at: "2026-08-23T00:00:00Z".into(),
            source: "suite".into(),
            sequence: 1,
        };
        apply_dashboard_event_to_snapshot(&mut snapshot, &started);
        apply_dashboard_event_to_snapshot(
            &mut snapshot,
            &DashboardEvent::CaseQueued {
                run_id: "run-reconnect".into(),
                case_name: "slow".into(),
                model: "m".into(),
                run_index: 0,
                queue_position: 2,
                sequence: 2,
            },
        );
        apply_dashboard_event_to_snapshot(
            &mut snapshot,
            &DashboardEvent::CaseProgress {
                run_id: "run-reconnect".into(),
                case_name: "active".into(),
                model: "m".into(),
                run_index: 0,
                phase: "executing".into(),
                elapsed_ms: 5000,
                sequence: 3,
            },
        );

        apply_dashboard_event_to_snapshot(
            &mut snapshot,
            &DashboardEvent::CaseStarted {
                run_id: "run-reconnect".into(),
                case_name: "active".into(),
                model: "m".into(),
                run_index: 0,
                sequence: 4,
            },
        );
        apply_dashboard_event_to_snapshot(
            &mut snapshot,
            &DashboardEvent::CaseProgress {
                run_id: "run-reconnect".into(),
                case_name: "active".into(),
                model: "m".into(),
                run_index: 0,
                phase: "executing".into(),
                elapsed_ms: 5000,
                sequence: 5,
            },
        );

        let json = snapshot_json(&snapshot);
        assert_eq!(json["phase"], "running");
        assert_eq!(json["progress"]["total_cases"], 2);
        assert_eq!(json["progress"]["queued"].as_object().unwrap().len(), 1);
        assert_eq!(json["progress"]["active"].as_object().unwrap().len(), 1);
        assert_eq!(
            json["progress"]["active"]
                .as_object()
                .unwrap()
                .values()
                .next()
                .unwrap()["elapsed_ms"],
            5000
        );
    }
}
