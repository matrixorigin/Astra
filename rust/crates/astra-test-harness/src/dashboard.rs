//! Live dashboard: embedded HTTP + WebSocket server for real-time
//! test progress visualization and run control.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::{Html, IntoResponse, Json};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, broadcast};

use crate::report::{CaseRunReport, SuiteReport};

// ── Event protocol (all events carry run_id for multi-run support) ──

#[derive(Debug, Clone)]
pub enum DashboardEvent {
    RunReset {
        run_id: String,
    },
    SuiteStarted {
        run_id: String,
        total_cases: usize,
        models: Vec<String>,
        started_at: String,
        source: String, // "manual" or "orchestrate"
    },
    CaseStarted {
        run_id: String,
        case_name: String,
        model: String,
        run_index: u32,
    },
    CaseCompleted {
        run_id: String,
        report: Arc<CaseRunReport>,
    },
    SuiteCompleted {
        run_id: String,
        report: Arc<SuiteReport>,
    },
}

impl Serialize for DashboardEvent {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Self::RunReset { run_id } => {
                let mut m = s.serialize_map(Some(2))?;
                m.serialize_entry("type", "run_reset")?;
                m.serialize_entry("run_id", run_id)?;
                m.end()
            }
            Self::SuiteStarted {
                run_id,
                total_cases,
                models,
                started_at,
                source,
            } => {
                let mut m = s.serialize_map(Some(6))?;
                m.serialize_entry("type", "suite_started")?;
                m.serialize_entry("run_id", run_id)?;
                m.serialize_entry("total_cases", total_cases)?;
                m.serialize_entry("models", models)?;
                m.serialize_entry("started_at", started_at)?;
                m.serialize_entry("source", source)?;
                m.end()
            }
            Self::CaseStarted {
                run_id,
                case_name,
                model,
                run_index,
            } => {
                let mut m = s.serialize_map(Some(5))?;
                m.serialize_entry("type", "case_started")?;
                m.serialize_entry("run_id", run_id)?;
                m.serialize_entry("case_name", case_name)?;
                m.serialize_entry("model", model)?;
                m.serialize_entry("run_index", run_index)?;
                m.end()
            }
            Self::CaseCompleted { run_id, report } => {
                let mut m = s.serialize_map(Some(3))?;
                m.serialize_entry("type", "case_completed")?;
                m.serialize_entry("run_id", run_id)?;
                m.serialize_entry("report", report.as_ref())?;
                m.end()
            }
            Self::SuiteCompleted { run_id, report } => {
                let mut m = s.serialize_map(Some(3))?;
                m.serialize_entry("type", "suite_completed")?;
                m.serialize_entry("run_id", run_id)?;
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
    cancel_flag: Arc<AtomicBool>,
    last_report: Arc<Mutex<Option<SuiteReport>>>,
    run_counter: Arc<std::sync::atomic::AtomicU32>,
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
            cancel_flag: Arc::new(AtomicBool::new(false)),
            last_report: Arc::new(Mutex::new(None)),
            run_counter: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        };
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
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: WebSocket, state: AppState) {
    // Send current state to late-joining clients.
    let running = *state.running.lock().await;
    let init = serde_json::json!({
        "type": "init",
        "running": running,
        "has_report": state.last_report.lock().await.is_some(),
    });
    let _ = socket.send(Message::Text(init.to_string().into())).await;

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

async fn report_handler(State(state): State<AppState>) -> Json<serde_json::Value> {
    let report = state.last_report.lock().await;
    match &*report {
        Some(r) => Json(serde_json::json!({"report": r})),
        None => Json(serde_json::json!({"report": null})),
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
    #[serde(default)]
    source: Option<String>,
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

    state.cancel_flag.store(false, Ordering::SeqCst);

    let run_num = state.run_counter.fetch_add(1, Ordering::SeqCst);
    let run_id = format!("run-{run_num}");
    let source = req.source.clone().unwrap_or_else(|| "manual".into());

    let _ = state.tx.send(DashboardEvent::RunReset {
        run_id: run_id.clone(),
    });

    let config = state.config.clone();
    let tx = state.tx.clone();
    let running_flag = state.running.clone();
    let cancel_flag = state.cancel_flag.clone();
    let last_report = state.last_report.clone();
    let rid = run_id.clone();

    tokio::spawn(async move {
        let result = execute_run(config, tx.clone(), req, cancel_flag, &rid, &source).await;
        match result {
            Ok(report) => {
                *last_report.lock().await = Some(report);
            }
            Err(e) => {
                eprintln!("[astra-test] dashboard run error: {e}");
                let _ = tx.send(DashboardEvent::SuiteCompleted {
                    run_id: rid,
                    report: Arc::new(SuiteReport::default()),
                });
            }
        }
        *running_flag.lock().await = false;
    });

    Json(serde_json::json!({"status": "started", "run_id": run_id}))
}

async fn cancel_handler(State(state): State<AppState>) -> Json<serde_json::Value> {
    if *state.running.lock().await {
        state.cancel_flag.store(true, Ordering::SeqCst);
        Json(serde_json::json!({"status": "cancelling"}))
    } else {
        Json(serde_json::json!({"error": "No run in progress"}))
    }
}

/// Structured capability evaluation with numeric scores.
async fn eval_handler(State(state): State<AppState>) -> Json<serde_json::Value> {
    let report = state.last_report.lock().await;
    match &*report {
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
    let report = state.last_report.lock().await;
    let Some(ref r) = *report else {
        return Json(serde_json::json!({"error": "No run results to analyze"}));
    };

    let model = req.model.as_deref().unwrap_or("claude-sonnet-4-6");
    match crate::summarizer::summarize(&state.config.astra_bin, model, r, 300).await {
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
    let admin_bin = state.config.astra_bin.with_file_name("astra-admin");
    let output = tokio::process::Command::new(&admin_bin)
        .args([
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
}

async fn chat_handler(
    State(state): State<AppState>,
    Json(req): Json<ChatRequest>,
) -> Json<serde_json::Value> {
    use tokio::process::Command;

    let model = req.model.as_deref().unwrap_or("claude-sonnet-4-6");
    eprintln!(
        "[astra-test] chat: model={model} session_id={:?} msg_len={}",
        req.session_id.as_deref().unwrap_or("(new)"),
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
        let report = state.last_report.lock().await;
        if let Some(ref r) = *report {
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
                        if run.passed { "PASS" } else { "FAIL" },
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
                "\n\nLatest run results ({} total, {} passed, {} failed, wall={}ms):\n{}",
                r.total(),
                r.passed(),
                r.failed(),
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

    let admin_bin = state.config.astra_bin.with_file_name("astra-admin");
    let models_list = tokio::process::Command::new(&admin_bin)
        .args(["model", "list"])
        .env("NO_PROXY", "localhost,127.0.0.1")
        .env("no_proxy", "localhost,127.0.0.1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .ok()
        .and_then(|o| {
            serde_json::from_str::<Vec<serde_json::Value>>(&String::from_utf8_lossy(&o.stdout)).ok()
        })
        .map(|ms| {
            ms.iter()
                .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();

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
    let output = tokio::process::Command::new(&state.config.astra_bin)
        .args(["chat", "-m", &prompt, "--model", model, "--json", "-y"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await;

    match output {
        Ok(o) => {
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

            let text = serde_json::from_str::<serde_json::Value>(stdout.trim())
                .ok()
                .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(str::to_string))
                .unwrap_or_else(|| stdout.trim().to_string());

            if text.is_empty() {
                return Json(serde_json::json!({
                    "error": format!("astra returned empty response (exit={})", exit_code)
                }));
            }

            let plan_text = text
                .trim()
                .trim_start_matches("```json")
                .trim_start_matches("```")
                .trim_end_matches("```")
                .trim();

            if let Ok(plan) = serde_json::from_str::<serde_json::Value>(plan_text) {
                return Json(serde_json::json!({"plan": plan}));
            }

            if let Some(start) = text.find('{')
                && let Some(end) = text.rfind('}')
            {
                let json_slice = &text[start..=end];
                if let Ok(plan) = serde_json::from_str::<serde_json::Value>(json_slice) {
                    return Json(serde_json::json!({"plan": plan}));
                }
            }

            Json(serde_json::json!({"chat_fallback": text}))
        }
        Err(e) => Json(serde_json::json!({"error": format!("orchestrate: {e}")})),
    }
}

/// Execute a full test run with cancel support.
async fn execute_run(
    config: Arc<DashboardConfig>,
    tx: broadcast::Sender<DashboardEvent>,
    req: RunRequest,
    cancel_flag: Arc<AtomicBool>,
    run_id: &str,
    _source: &str,
) -> anyhow::Result<SuiteReport> {
    use crate::case::Case;
    use crate::digest::AstraCliDigestCollector;
    use crate::exec::AstraCliExecutor;
    use crate::judger::{AstraCliJudger, JudgerConfig};
    use crate::runner::RunnerConfig;
    use crate::suite::{DiskSessionLoader, SessionCaptureMode, SuiteConfig, SuiteRunner};

    let mut cases = Case::load_dir(&config.suite_dir)?;

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
