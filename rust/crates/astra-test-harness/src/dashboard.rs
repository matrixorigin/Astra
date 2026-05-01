//! Live dashboard: embedded HTTP + WebSocket server for real-time
//! test progress visualization and run control.
//!
//! `--live-dashboard [PORT]` starts an axum server at `GET /` that
//! serves a single-page control console. The console can:
//! - List available cases and models via REST API
//! - Trigger runs with custom configuration via `POST /api/run`
//! - Stream real-time results over `GET /ws`

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::{Html, IntoResponse, Json};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, broadcast};

use crate::report::{CaseRunReport, SuiteReport};

// ── Event protocol ──────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum DashboardEvent {
    SuiteStarted {
        total_cases: usize,
        models: Vec<String>,
        started_at: String,
    },
    CaseStarted {
        case_name: String,
        model: String,
        run_index: u32,
    },
    CaseCompleted {
        report: Arc<CaseRunReport>,
    },
    SuiteCompleted {
        report: Arc<SuiteReport>,
    },
}

impl Serialize for DashboardEvent {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Self::SuiteStarted {
                total_cases,
                models,
                started_at,
            } => {
                let mut m = s.serialize_map(Some(4))?;
                m.serialize_entry("type", "suite_started")?;
                m.serialize_entry("total_cases", total_cases)?;
                m.serialize_entry("models", models)?;
                m.serialize_entry("started_at", started_at)?;
                m.end()
            }
            Self::CaseStarted {
                case_name,
                model,
                run_index,
            } => {
                let mut m = s.serialize_map(Some(4))?;
                m.serialize_entry("type", "case_started")?;
                m.serialize_entry("case_name", case_name)?;
                m.serialize_entry("model", model)?;
                m.serialize_entry("run_index", run_index)?;
                m.end()
            }
            Self::CaseCompleted { report } => {
                let mut m = s.serialize_map(Some(2))?;
                m.serialize_entry("type", "case_completed")?;
                m.serialize_entry("report", report.as_ref())?;
                m.end()
            }
            Self::SuiteCompleted { report } => {
                let mut m = s.serialize_map(Some(2))?;
                m.serialize_entry("type", "suite_completed")?;
                m.serialize_entry("report", report.as_ref())?;
                m.end()
            }
        }
    }
}

// ── Server configuration ────────────────────────────────────────────

/// Immutable config the dashboard needs to discover cases and run them.
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
    running: Arc<Mutex<bool>>,
}

// ── Server ──────────────────────────────────────────────────────────

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
            running: Arc::new(Mutex::new(false)),
        };
        let app = Router::new()
            .route("/", get(index_handler))
            .route("/ws", get(ws_handler))
            .route("/api/config", get(config_handler))
            .route("/api/cases", get(cases_handler))
            .route("/api/run", post(run_handler))
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
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: WebSocket, state: AppState) {
    let mut rx = state.tx.subscribe();
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
                        let msg = serde_json::json!({"type": "lagged", "missed": n});
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

/// Return available configuration for the UI.
async fn config_handler(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "suite_dir": state.config.suite_dir.display().to_string(),
        "available_models": state.config.available_models,
        "judger_model": state.config.judger_model,
    }))
}

/// Return the list of available case names (from the suite directory).
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
                    })
                })
                .collect();
            Json(serde_json::json!({"cases": items}))
        }
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

/// Run request from the UI.
#[derive(Debug, Deserialize)]
struct RunRequest {
    models: Vec<String>,
    #[serde(default)]
    filter: Option<String>,
    #[serde(default = "default_parallel")]
    parallel: usize,
    #[serde(default)]
    no_judger: bool,
    #[serde(default)]
    judger_model: Option<String>,
}

fn default_parallel() -> usize {
    2
}

/// Trigger a test run from the dashboard UI. Non-blocking: spawns
/// the run on a background task and streams results over WS.
async fn run_handler(
    State(state): State<AppState>,
    Json(req): Json<RunRequest>,
) -> Json<serde_json::Value> {
    {
        let mut running = state.running.lock().await;
        if *running {
            return Json(serde_json::json!({"error": "A run is already in progress"}));
        }
        *running = true;
    }

    let config = state.config.clone();
    let tx = state.tx.clone();
    let running_flag = state.running.clone();

    tokio::spawn(async move {
        let result = execute_run(config, tx.clone(), req).await;
        if let Err(e) = result {
            eprintln!("[astra-test] dashboard run error: {e}");
            let _ = tx.send(DashboardEvent::SuiteCompleted {
                report: Arc::new(SuiteReport::default()),
            });
        }
        *running_flag.lock().await = false;
    });

    Json(serde_json::json!({"status": "started"}))
}

/// Execute a full test run (called on a background task).
async fn execute_run(
    config: Arc<DashboardConfig>,
    tx: broadcast::Sender<DashboardEvent>,
    req: RunRequest,
) -> anyhow::Result<()> {
    use crate::case::Case;
    use crate::digest::AstraCliDigestCollector;
    use crate::exec::AstraCliExecutor;
    use crate::judger::{AstraCliJudger, JudgerConfig};
    use crate::runner::RunnerConfig;
    use crate::suite::{DiskSessionLoader, SessionCaptureMode, SuiteConfig, SuiteRunner};

    let mut cases = Case::load_dir(&config.suite_dir)?;
    if let Some(ref pattern) = req.filter {
        let re_str = format!(
            "^{}$",
            regex::escape(pattern)
                .replace(r"\*", ".*")
                .replace(r"\?", ".")
        );
        if let Ok(re) = regex::Regex::new(&re_str) {
            cases.retain(|c| re.is_match(&c.name));
        }
    }

    let fallback_models = req.models.clone();
    let mut runner_cfg =
        RunnerConfig::new(config.astra_bin.clone()).with_fallback_models(fallback_models);
    runner_cfg.working_dir = None;

    let judger_model = req.judger_model.as_deref().unwrap_or(&config.judger_model);
    let judger_cfg = JudgerConfig::new(config.astra_bin.clone(), judger_model);
    let judger = AstraCliJudger::new(judger_cfg);
    let executor = AstraCliExecutor::new(runner_cfg.clone());
    let session_loader = DiskSessionLoader;
    let digest = AstraCliDigestCollector::new(config.astra_bin.clone());

    let suite_cfg = SuiteConfig {
        parallel: req.parallel.max(1),
        circuit_breaker_threshold: 3,
        retry_on_429: false,
        runs: 1,
    };

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
    };

    runner.run_all(&cases).await;
    Ok(())
}
