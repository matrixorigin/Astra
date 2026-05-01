//! Live dashboard: embedded HTTP + WebSocket server for real-time
//! test progress visualization and run control.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::{Html, IntoResponse, Json};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, broadcast};
use tokio_util::sync::CancellationToken;

use crate::report::{CaseRunReport, SuiteReport};

// ── Event protocol ──────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum DashboardEvent {
    RunReset,
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
            Self::RunReset => {
                let mut m = s.serialize_map(Some(1))?;
                m.serialize_entry("type", "run_reset")?;
                m.end()
            }
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
    cancel_token: Arc<Mutex<Option<CancellationToken>>>,
}

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
            cancel_token: Arc::new(Mutex::new(None)),
        };
        let app = Router::new()
            .route("/", get(index_handler))
            .route("/ws", get(ws_handler))
            .route("/api/config", get(config_handler))
            .route("/api/models", get(models_handler))
            .route("/api/cases", get(cases_handler))
            .route("/api/run", post(run_handler))
            .route("/api/cancel", post(cancel_handler))
            .route("/api/chat", post(chat_handler))
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

async fn config_handler(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "suite_dir": state.config.suite_dir.display().to_string(),
        "available_models": state.config.available_models,
        "judger_model": state.config.judger_model,
    }))
}

/// List all registered models by calling `astra-admin model list`.
async fn models_handler(State(state): State<AppState>) -> Json<serde_json::Value> {
    let admin_bin = state.config.astra_bin.with_file_name("astra-admin");
    let output = tokio::process::Command::new(&admin_bin)
        .args(["model", "list"])
        .env("NO_PROXY", "localhost,127.0.0.1")
        .env("no_proxy", "localhost,127.0.0.1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await;
    match output {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            if let Ok(models) = serde_json::from_str::<serde_json::Value>(stdout.trim()) {
                Json(serde_json::json!({"models": models}))
            } else {
                Json(serde_json::json!({"models": [], "error": "parse failed"}))
            }
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            Json(serde_json::json!({"models": [], "error": stderr.trim()}))
        }
        Err(e) => Json(serde_json::json!({"models": [], "error": e.to_string()})),
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
}

fn default_parallel() -> usize {
    2
}

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

    let token = CancellationToken::new();
    *state.cancel_token.lock().await = Some(token.clone());

    // Reset frontend state before starting a new run.
    let _ = state.tx.send(DashboardEvent::RunReset);

    let config = state.config.clone();
    let tx = state.tx.clone();
    let running_flag = state.running.clone();
    let cancel_token_slot = state.cancel_token.clone();

    tokio::spawn(async move {
        let result = execute_run(config, tx.clone(), req, token).await;
        if let Err(e) = result {
            eprintln!("[astra-test] dashboard run error: {e}");
            let _ = tx.send(DashboardEvent::SuiteCompleted {
                report: Arc::new(SuiteReport::default()),
            });
        }
        *running_flag.lock().await = false;
        *cancel_token_slot.lock().await = None;
    });

    Json(serde_json::json!({"status": "started"}))
}

async fn cancel_handler(State(state): State<AppState>) -> Json<serde_json::Value> {
    let token = state.cancel_token.lock().await;
    if let Some(ref t) = *token {
        t.cancel();
        Json(serde_json::json!({"status": "cancelled"}))
    } else {
        Json(serde_json::json!({"error": "No run in progress"}))
    }
}

#[derive(Debug, Deserialize)]
struct ChatRequest {
    message: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
}

async fn chat_handler(
    State(state): State<AppState>,
    Json(req): Json<ChatRequest>,
) -> Json<serde_json::Value> {
    use tokio::process::Command;

    let model = req.model.as_deref().unwrap_or("claude-sonnet-4-6");
    let mut cmd = Command::new(&state.config.astra_bin);
    cmd.arg("chat")
        .arg("-m")
        .arg(&req.message)
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

    let output = match tokio::time::timeout(std::time::Duration::from_secs(120), cmd.output()).await
    {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Json(serde_json::json!({"error": format!("spawn: {e}")})),
        Err(_) => return Json(serde_json::json!({"error": "chat timed out after 120s"})),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code().unwrap_or(-1);

    if exit_code == 3 {
        return Json(serde_json::json!({
            "error": "Authentication failed — credentials may have expired. Run: astra-admin login",
            "exit_code": 3,
        }));
    }

    if let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout.trim()) {
        Json(v)
    } else {
        Json(serde_json::json!({
            "text": stdout.trim().to_string(),
            "stderr": stderr.trim().to_string(),
            "exit_code": exit_code,
        }))
    }
}

/// Execute a full test run.
async fn execute_run(
    config: Arc<DashboardConfig>,
    tx: broadcast::Sender<DashboardEvent>,
    req: RunRequest,
    _cancel: CancellationToken,
) -> anyhow::Result<()> {
    use crate::case::Case;
    use crate::digest::AstraCliDigestCollector;
    use crate::exec::AstraCliExecutor;
    use crate::judger::{AstraCliJudger, JudgerConfig};
    use crate::runner::RunnerConfig;
    use crate::suite::{DiskSessionLoader, SessionCaptureMode, SuiteConfig, SuiteRunner};

    let mut cases = Case::load_dir(&config.suite_dir)?;

    // Filter by explicit case names, then glob.
    if !req.cases.is_empty() {
        let selected: std::collections::HashSet<&str> =
            req.cases.iter().map(|s| s.as_str()).collect();
        cases.retain(|c| selected.contains(c.name.as_str()));
    } else if let Some(ref pattern) = req.filter {
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

    if cases.is_empty() {
        anyhow::bail!("no cases matched the selection");
    }

    // Override case-level models with user's selection so cases
    // with hardcoded `models:` don't run unexpected models.
    if !req.models.is_empty() {
        for case in &mut cases {
            case.models = Some(req.models.clone());
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
