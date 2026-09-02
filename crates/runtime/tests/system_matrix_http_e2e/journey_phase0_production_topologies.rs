//! External-process Phase-0 production baseline orchestrator.
//!
//! Unlike the ordinary system matrix, this journey never hosts an Axum router
//! in the test process. Every workload crosses an actual `astra-server`
//! process; CLI and Edge topologies additionally execute the real `astra` and
//! `astra-edge` binaries. Process counters are emitted only by each binary's
//! `ProductionProcessCaptureGuard`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use astra_core::config::{ASTRA_CONFIG_SOURCE_ENV, ASTRA_CONFIG_SOURCE_EXPLICIT_ENV};
use astra_core::history_work_baseline::{
    BASELINE_CAPTURE_SCOPE_ENV, BASELINE_GIT_SHA_ENV, BASELINE_RUN_ID_ENV, PROCESS_CAPTURE_SCHEMA,
    ProductionCaptureScope, ProductionProcessCapture, ProductionProcessRole, ProductionTopology,
    WindowClass, production_process_capture_id, verify_current_build_attestation,
    write_json_atomic,
};
use astra_core::{AppSettings, SharedPool};
use astra_runtime_env::ASTRA_LOCAL_STATE_ROOT_ENV;
use astra_services::models::{PricingData, QuirksData};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{MySqlPool, Row};
use tokio::process::{Child, Command};
use uuid::Uuid;

use super::journey_phase0_production_baseline::{
    AdmissionCounters, AnyResult, ExternalScenarioFacts, LiveTenant, ResolvedOffering,
    StreamAuthority, StreamCapture, assemble_external_scenario, exact_context_window,
    exact_prometheus_counter, invalid, parse_sse_events, seed_structured_history_with_pool,
    window_label,
};

const SERVER_BIN_ENV: &str = "ASTRA_PHASE0_SERVER_BIN";
const CLI_BIN_ENV: &str = "ASTRA_PHASE0_CLI_BIN";
const EDGE_BIN_ENV: &str = "ASTRA_PHASE0_EDGE_BIN";
const SERVER_PORT_ENV: &str = "ASTRA_PHASE0_SERVER_PORT";
const MODELS_FILE_ENV: &str = "ASTRA_PHASE0_MODELS_FILE";
const SOURCE_MODEL_ENV: &str = "ASTRA_PHASE0_SOURCE_MODEL";
const OUTPUT_DIR_ENV: &str = "ASTRA_PHASE0_BASELINE_DIR";
const EXCLUSIVE_ENV: &str = "ASTRA_PHASE0_BASELINE_EXCLUSIVE";
const PROCESS_CAPTURE_ENV: &str = "ASTRA_HISTORY_WORK_BASELINE_FRAGMENT";
const TRACE_ENV: &str = "ASTRA_HISTORY_WORK_TRACE";
const CLI_STREAM_JSON_SCHEMA: &str = "astra.cli.stream_json.v1";
const STREAM_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const PROCESS_STOP_TIMEOUT: Duration = Duration::from_secs(60);
const READY_TIMEOUT: Duration = Duration::from_secs(60);

const ADMISSION_ATTEMPTS_METRIC: &str = "astra_run_admission_attempts_total";
const ADMISSION_WAIT_MS_METRIC: &str = "astra_run_admission_wait_ms_total";
const ADMISSION_UNITS_METRIC: &str = "astra_run_admission_weight_units_total";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MeasuredPhase {
    Cold,
    WarmEligible,
}

impl MeasuredPhase {
    const fn label(self) -> &'static str {
        match self {
            Self::Cold => "cold",
            Self::WarmEligible => "warm_eligible",
        }
    }

    const fn capture_scope(
        self,
        topology: ProductionTopology,
        window: WindowClass,
    ) -> ProductionCaptureScope {
        match self {
            Self::Cold => ProductionCaptureScope::cold(topology, window),
            Self::WarmEligible => ProductionCaptureScope::warm_eligible(topology, window),
        }
    }
}

struct RunnerConfig {
    server_bin: PathBuf,
    cli_bin: PathBuf,
    edge_bin: PathBuf,
    models_file: PathBuf,
    source_model_name: String,
    output_dir: PathBuf,
    server_port: u16,
    server_origin: String,
    baseline_run_id: String,
    git_sha: String,
}

impl RunnerConfig {
    fn from_env() -> AnyResult<Self> {
        require_exact_env(EXCLUSIVE_ENV, "1")?;
        require_exact_env(TRACE_ENV, "1")?;
        require_exact_env(ASTRA_CONFIG_SOURCE_ENV, ASTRA_CONFIG_SOURCE_EXPLICIT_ENV)?;
        let workspace_root = std::env::current_dir()?.canonicalize()?;
        let server_bin = required_existing_file(SERVER_BIN_ENV)?;
        let cli_bin = required_existing_file(CLI_BIN_ENV)?;
        let edge_bin = required_existing_file(EDGE_BIN_ENV)?;
        require_binary_name(&server_bin, "astra-server")?;
        require_binary_name(&cli_bin, "astra")?;
        require_binary_name(&edge_bin, "astra-edge")?;
        let models_file = required_existing_file(MODELS_FILE_ENV)?;
        let source_model_name = required_nonempty_env(SOURCE_MODEL_ENV)?;
        let output_dir = PathBuf::from(required_nonempty_env(OUTPUT_DIR_ENV)?).canonicalize()?;
        if !output_dir.is_dir() {
            return Err(invalid(format!(
                "{OUTPUT_DIR_ENV} is not a directory: {}",
                output_dir.display()
            )));
        }
        if output_dir.starts_with(&workspace_root) {
            return Err(invalid(format!(
                "{OUTPUT_DIR_ENV} must be outside the git workspace so baseline fragments do not dirty provenance"
            )));
        }
        if output_dir
            .ancestors()
            .any(|ancestor| ancestor.join(".git").exists())
        {
            return Err(invalid(format!(
                "{OUTPUT_DIR_ENV} must not be nested in any git worktree because isolated child discovery would observe that repository"
            )));
        }
        let server_port = required_nonempty_env(SERVER_PORT_ENV)?
            .parse::<u16>()
            .map_err(|error| invalid(format!("{SERVER_PORT_ENV} is invalid: {error}")))?;
        if server_port == 0 {
            return Err(invalid(format!("{SERVER_PORT_ENV} cannot be zero")));
        }
        let baseline_run_id = required_nonempty_env(BASELINE_RUN_ID_ENV)?;
        validate_run_id(&baseline_run_id)?;
        let git_sha = required_nonempty_env(BASELINE_GIT_SHA_ENV)?;
        validate_git_sha(&workspace_root, &git_sha)?;
        Ok(Self {
            server_bin,
            cli_bin,
            edge_bin,
            models_file,
            source_model_name,
            output_dir,
            server_port,
            server_origin: format!("http://127.0.0.1:{server_port}"),
            baseline_run_id,
            git_sha,
        })
    }

    fn capture_path(
        &self,
        topology: ProductionTopology,
        window: WindowClass,
        suffix: &str,
    ) -> PathBuf {
        self.output_dir.join(format!(
            "{}_{}.{}.production_process_capture.json",
            topology_label(topology),
            window_label(window),
            suffix
        ))
    }

    fn scenario_path(&self, topology: ProductionTopology, window: WindowClass) -> PathBuf {
        self.output_dir.join(format!(
            "{}_{}.production_scenario.json",
            topology_label(topology),
            window_label(window)
        ))
    }
}

struct SourceModel {
    provider: String,
    api_key: String,
    base_url: Option<String>,
    description: Option<String>,
    max_completion_tokens: Option<i32>,
    input_modalities: Vec<String>,
    output_modalities: Vec<String>,
    supported_parameters: Vec<String>,
    pricing: PricingData,
    architecture: Option<String>,
    tags: Vec<String>,
    quirks: QuirksData,
}

#[derive(Deserialize)]
struct SourceModelWire {
    name: String,
    provider: String,
    api_key: String,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    max_completion_tokens: Option<i32>,
    #[serde(default = "default_text_modality")]
    input_modalities: Vec<String>,
    #[serde(default = "default_text_modality")]
    output_modalities: Vec<String>,
    #[serde(default)]
    supported_parameters: Vec<String>,
    #[serde(default)]
    pricing: PricingData,
    #[serde(default)]
    architecture: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    quirks: QuirksData,
}

fn default_text_modality() -> Vec<String> {
    vec!["text".to_string()]
}

fn load_source_model(path: &Path, exact_name: &str) -> AnyResult<SourceModel> {
    let text = fs::read_to_string(path)?;
    let document: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text)?;
    let entries = document
        .as_sequence()
        .or_else(|| {
            document
                .get("models")
                .and_then(serde_yaml_ng::Value::as_sequence)
        })
        .ok_or_else(|| invalid("models file must be a sequence or contain a `models` sequence"))?;
    let mut exact = entries.iter().filter(|entry| {
        entry.get("name").and_then(serde_yaml_ng::Value::as_str) == Some(exact_name)
    });
    let entry = exact
        .next()
        .ok_or_else(|| invalid(format!("source model `{exact_name}` was not found")))?;
    if exact.next().is_some() {
        return Err(invalid(format!(
            "source model `{exact_name}` is duplicated in the models file"
        )));
    }
    let wire: SourceModelWire = serde_yaml_ng::from_value(entry.clone())?;
    if wire.name != exact_name {
        return Err(invalid(
            "exact source-model selection changed during decode",
        ));
    }
    if wire.provider.trim().is_empty()
        || wire.provider.eq_ignore_ascii_case("mock")
        || wire.api_key.trim().is_empty()
    {
        return Err(invalid(
            "source model must have a real non-mock provider and non-empty credential",
        ));
    }
    let mut quirks = wire.quirks;
    if quirks.wire_model_name.is_none() {
        quirks.wire_model_name = Some(wire.name.clone());
    }
    Ok(SourceModel {
        provider: wire.provider,
        api_key: wire.api_key,
        base_url: wire.base_url,
        description: wire.description,
        max_completion_tokens: wire.max_completion_tokens,
        input_modalities: wire.input_modalities,
        output_modalities: wire.output_modalities,
        supported_parameters: wire.supported_parameters,
        pricing: wire.pricing,
        architecture: wire.architecture,
        tags: wire.tags,
        quirks,
    })
}

struct NetworkClient {
    http: Client,
    origin: String,
}

impl NetworkClient {
    fn new(origin: String) -> AnyResult<Self> {
        let http = Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        Ok(Self { http, origin })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.origin, path)
    }

    async fn wait_ready(&self, server: &mut ManagedProcess) -> AnyResult<()> {
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        loop {
            if let Some(status) = server.child.try_wait()? {
                return Err(invalid(format!(
                    "astra-server exited before readiness with {status}; logs={}",
                    server.log_paths
                )));
            }
            if let Ok(response) = self.http.get(self.url("/health")).send().await
                && response.status().is_success()
            {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(invalid(format!(
                    "astra-server did not become ready within {READY_TIMEOUT:?}; logs={}",
                    server.log_paths
                )));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn post_json(
        &self,
        path: &str,
        auth: Option<&str>,
        payload: &Value,
        expected: StatusCode,
    ) -> AnyResult<Value> {
        let mut request = self.http.post(self.url(path)).json(payload);
        if let Some(auth) = auth {
            request = request.header("authorization", auth);
        }
        let response = request.send().await?;
        let status = response.status();
        let body = response.bytes().await?;
        if status != expected {
            return Err(invalid(format!(
                "POST {path} returned {status}, expected {expected}"
            )));
        }
        if body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&body)
            .map_err(|error| invalid(format!("POST {path} returned invalid JSON: {error}")))
    }

    async fn get_json(&self, path: &str, auth: &str) -> AnyResult<Value> {
        let response = self
            .http
            .get(self.url(path))
            .header("authorization", auth)
            .send()
            .await?;
        let status = response.status();
        let body = response.bytes().await?;
        if status != StatusCode::OK {
            return Err(invalid(format!("GET {path} returned {status}")));
        }
        serde_json::from_slice(&body)
            .map_err(|error| invalid(format!("GET {path} returned invalid JSON: {error}")))
    }

    async fn stream(&self, tenant: &LiveTenant, payload: &Value) -> AnyResult<StreamCapture> {
        let mut payload = payload.clone();
        if payload.get("session_id").and_then(Value::as_str) != Some(offering_session_placeholder())
        {
            return Err(invalid(
                "production stream payload lost its explicit session placeholder",
            ));
        }
        payload["session_id"] = Value::String(tenant.session_id.clone());
        let response = tokio::time::timeout(
            STREAM_TIMEOUT,
            self.http
                .post(self.url("/chat/stream"))
                .header("authorization", &tenant.auth_header)
                .header("accept", "text/event-stream")
                .json(&payload)
                .send(),
        )
        .await
        .map_err(|_| invalid("POST /chat/stream timed out before response headers"))??;
        let status = response.status();
        let body = tokio::time::timeout(STREAM_TIMEOUT, response.bytes())
            .await
            .map_err(|_| invalid("POST /chat/stream timed out while reading SSE"))??;
        if !status.is_success() {
            return Err(invalid(format!("POST /chat/stream returned {status}")));
        }
        let text = std::str::from_utf8(&body)
            .map_err(|error| invalid(format!("chat SSE is not UTF-8: {error}")))?;
        let events = parse_sse_events(text)?;
        reject_typed_error(&events, "/chat/stream")?;
        let session_info = events
            .iter()
            .find(|event| event.get("type").and_then(Value::as_str) == Some("session_info"))
            .ok_or_else(|| invalid("chat SSE omitted typed session_info"))?;
        let session_id = required_json_string(session_info, "session_id")?;
        let run_id = required_json_string(session_info, "run_id")?;
        if session_id != tenant.session_id {
            return Err(invalid(
                "chat SSE session_id differs from the requested session",
            ));
        }
        let mut terminals = events
            .iter()
            .filter(|event| event.get("type").and_then(Value::as_str) == Some("run_finished"));
        let terminal = terminals
            .next()
            .ok_or_else(|| invalid("chat SSE omitted typed run_finished"))?;
        if terminals.next().is_some()
            || required_json_string(terminal, "run_id")? != run_id
            || required_json_string(terminal, "status")? != "completed"
        {
            return Err(invalid(
                "chat SSE did not end in one exact completed run terminal",
            ));
        }
        Ok(StreamCapture {
            session_id,
            authority: StreamAuthority::DurableRun { run_id },
            events,
        })
    }

    async fn admission_counters(&self) -> AnyResult<AdmissionCounters> {
        let response = self.http.get(self.url("/metrics")).send().await?;
        if response.status() != StatusCode::OK {
            return Err(invalid(format!(
                "GET /metrics returned {}",
                response.status()
            )));
        }
        let text = response.text().await?;
        Ok(AdmissionCounters {
            attempts: exact_prometheus_counter(&text, ADMISSION_ATTEMPTS_METRIC)?,
            wait_ms: exact_prometheus_counter(&text, ADMISSION_WAIT_MS_METRIC)?,
            units: exact_prometheus_counter(&text, ADMISSION_UNITS_METRIC)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProcessLogPaths {
    stdout: PathBuf,
    stderr: PathBuf,
}

impl fmt::Display for ProcessLogPaths {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "stdout={} stderr={}",
            self.stdout.display(),
            self.stderr.display()
        )
    }
}

struct ProcessLogs {
    paths: ProcessLogPaths,
    stdout: Stdio,
    stderr: Stdio,
}

struct ManagedProcess {
    role: ProductionProcessRole,
    expected_capture_scope: ProductionCaptureScope,
    child: Child,
    capture_path: PathBuf,
    log_paths: ProcessLogPaths,
    local_state_root: Option<tempfile::TempDir>,
}

impl ManagedProcess {
    fn finish_capture(&mut self) -> AnyResult<ProductionProcessCapture> {
        let capture = read_capture(&self.capture_path, self.role, self.expected_capture_scope)?;
        if let Some(local_state_root) = self.local_state_root.take() {
            local_state_root.close()?;
        }
        Ok(capture)
    }

    async fn graceful_stop(mut self) -> AnyResult<ProductionProcessCapture> {
        let pid = self
            .child
            .id()
            .ok_or_else(|| invalid(format!("{:?} process has no pid", self.role)))?;
        kill(
            Pid::from_raw(i32::try_from(pid).map_err(|_| invalid("child pid exceeds i32"))?),
            Signal::SIGTERM,
        )?;
        let status = tokio::time::timeout(PROCESS_STOP_TIMEOUT, self.child.wait())
            .await
            .map_err(|_| {
                invalid(format!(
                    "{:?} process did not stop within {:?}; logs={}",
                    self.role, PROCESS_STOP_TIMEOUT, self.log_paths
                ))
            })??;
        if !status.success() {
            return Err(invalid(format!(
                "{:?} process exited with {status}; logs={}",
                self.role, self.log_paths
            )));
        }
        self.finish_capture()
    }

    async fn wait_for_exit(mut self) -> AnyResult<ProductionProcessCapture> {
        let status = tokio::time::timeout(PROCESS_STOP_TIMEOUT, self.child.wait())
            .await
            .map_err(|_| {
                invalid(format!(
                    "{:?} process did not exit after server closing; logs={}",
                    self.role, self.log_paths
                ))
            })??;
        if !status.success() {
            return Err(invalid(format!(
                "{:?} process exited with {status}; logs={}",
                self.role, self.log_paths
            )));
        }
        self.finish_capture()
    }
}

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

struct RegisteredUser {
    user_id: String,
    username: String,
    password: String,
    access_token: String,
    refresh_token: String,
}

pub async fn run_external_production_topologies() {
    if let Err(error) = run_external_production_topologies_inner(None).await {
        panic!("Phase-0 external production topology baseline failed closed: {error}");
    }
}

pub async fn run_external_edge_server_m1() {
    if let Err(error) = run_external_production_topologies_inner(Some((
        ProductionTopology::EdgeServer,
        WindowClass::M1,
    )))
    .await
    {
        panic!("Phase-0 Edge+Server × 1M diagnostic failed closed: {error}");
    }
}

async fn run_external_production_topologies_inner(
    exact_scenario: Option<(ProductionTopology, WindowClass)>,
) -> AnyResult<()> {
    let config = RunnerConfig::from_env()?;
    verify_current_build_attestation(&config.git_sha, &config.baseline_run_id)?;
    let source = load_source_model(&config.models_file, &config.source_model_name)?;
    let settings = AppSettings::from_explicit_env()?;
    astra_services::ensure_core_schema(&settings.matrixone, &settings.database_bootstrap_catalog)
        .await?;
    let database_url = settings.matrixone.database_url_with_password();
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(8)
        .connect(&database_url)
        .await?;
    let shared_pool = SharedPool::new(&settings.matrixone).await?;
    let network = NetworkClient::new(config.server_origin.clone())?;

    let setup_capture_path = config
        .output_dir
        .join("setup.server.production_process_capture.json");
    let setup_log_path = config.output_dir.join("setup.server.log");
    let mut setup_server = spawn_server(
        &config,
        ProductionCaptureScope::Setup,
        setup_capture_path.clone(),
        setup_log_path,
    )?;
    network.wait_ready(&mut setup_server).await?;

    let mut primary = register_user(&network, "primary").await?;
    grant_admin(&pool, &primary.user_id).await?;
    primary = login_user(&network, primary).await?;
    let offerings = create_window_offerings(&network, &pool, &primary, &config, &source).await?;
    let credentials_dir = tempfile::Builder::new()
        .prefix(".phase0-credentials-")
        .tempdir_in(&config.output_dir)?;
    write_credentials(credentials_dir.path(), &primary)?;

    let setup_capture = setup_server.graceful_stop().await?;
    assert_capture_run_id(&setup_capture, &config.baseline_run_id)?;

    let scenarios = exact_scenario.map_or_else(
        || {
            ProductionTopology::ALL
                .into_iter()
                .flat_map(|topology| {
                    WindowClass::ALL
                        .into_iter()
                        .map(move |window| (topology, window))
                })
                .collect::<Vec<_>>()
        },
        |scenario| vec![scenario],
    );
    for (topology, window) in scenarios {
        let offering = offerings
            .get(&window)
            .cloned()
            .ok_or_else(|| invalid(format!("missing resolved offering for {window:?}")))?;
        run_external_scenario(ExternalScenario {
            config: &config,
            network: &network,
            pool: &pool,
            shared_pool: &shared_pool,
            primary_user: &mut primary,
            credentials_dir: credentials_dir.path(),
            topology,
            offering,
        })
        .await?;
    }

    pool.close().await;
    Ok(())
}

struct ExternalScenario<'a> {
    config: &'a RunnerConfig,
    network: &'a NetworkClient,
    pool: &'a MySqlPool,
    shared_pool: &'a SharedPool,
    primary_user: &'a mut RegisteredUser,
    credentials_dir: &'a Path,
    topology: ProductionTopology,
    offering: ResolvedOffering,
}

async fn run_external_scenario(scenario: ExternalScenario<'_>) -> AnyResult<()> {
    let ExternalScenario {
        config,
        network,
        pool,
        shared_pool,
        primary_user,
        credentials_dir,
        topology,
        offering,
    } = scenario;
    let window = offering.window_class;
    let scenario_path = config.scenario_path(topology, window);
    require_absent(&scenario_path)?;
    let server_capture_path = config.capture_path(topology, window, "server");
    let server_log_path = config.output_dir.join(format!(
        "{}_{}.server.log",
        topology_label(topology),
        window_label(window)
    ));
    let mut server = spawn_server(
        config,
        ProductionCaptureScope::service(topology, window),
        server_capture_path,
        server_log_path,
    )?;
    network.wait_ready(&mut server).await?;

    *primary_user = login_user(
        network,
        RegisteredUser {
            user_id: primary_user.user_id.clone(),
            username: primary_user.username.clone(),
            password: primary_user.password.clone(),
            access_token: String::new(),
            refresh_token: primary_user.refresh_token.clone(),
        },
    )
    .await?;
    write_credentials(credentials_dir, primary_user)?;

    let primary = create_session(
        network,
        primary_user,
        topology,
        window,
        &offering,
        "primary",
    )
    .await?;
    seed_structured_history_with_pool(shared_pool, &primary, &offering).await?;
    let secondary_user = register_user(network, "fairness").await?;
    let secondary = create_session(
        network,
        &secondary_user,
        topology,
        window,
        &offering,
        "fairness",
    )
    .await?;

    let cli_workspace = if topology == ProductionTopology::CliServer {
        Some(
            tempfile::Builder::new()
                .prefix(&format!(
                    ".phase0-{}_{}.cli-workspace-",
                    topology_label(topology),
                    window_label(window)
                ))
                .tempdir_in(&config.output_dir)?,
        )
    } else {
        None
    };
    let edge_workspace = if topology == ProductionTopology::EdgeServer {
        Some(
            tempfile::Builder::new()
                .prefix(&format!(
                    ".phase0-{}_{}.edge-workspace-",
                    topology_label(topology),
                    window_label(window)
                ))
                .tempdir_in(&config.output_dir)?,
        )
    } else {
        None
    };
    let edge_id = format!(
        "phase0-{}-{}-{}",
        &config.baseline_run_id[..12],
        topology_label(topology),
        window_label(window)
    );
    let marker_name = "phase0-edge-marker.json";
    let mut edge = if let Some(edge_workspace) = edge_workspace.as_ref() {
        write_json_atomic(
            &edge_workspace.path().join(marker_name),
            &json!({
                "schema": "astra.phase0.edge_marker.v1",
                "baseline_run_id": config.baseline_run_id,
                "window_class": window_label(window),
            }),
        )?;
        let edge_capture_path = config.capture_path(topology, window, "edge");
        let edge_log_path = config.output_dir.join(format!(
            "{}_{}.edge.log",
            topology_label(topology),
            window_label(window)
        ));
        let mut process = spawn_edge(
            config,
            credentials_dir,
            edge_workspace.path(),
            &edge_id,
            ProductionCaptureScope::service(topology, window),
            edge_capture_path,
            edge_log_path,
        )?;
        wait_for_edge(network, &primary.auth_header, &edge_id, &mut process).await?;
        Some(process)
    } else {
        None
    };

    let primary_before = network.admission_counters().await?;
    let mut cli_captures = Vec::new();
    let (cold, warm_eligible) = if topology == ProductionTopology::CliServer {
        let cli_workspace = cli_workspace
            .as_ref()
            .ok_or_else(|| invalid("CLI topology is missing its stable workspace"))?;
        let (cold, cold_capture) = run_cli_turn(
            config,
            credentials_dir,
            cli_workspace.path(),
            &primary,
            &offering,
            topology,
            MeasuredPhase::Cold,
        )
        .await?;
        cli_captures.push(cold_capture);
        let (warm, warm_capture) = run_cli_turn(
            config,
            credentials_dir,
            cli_workspace.path(),
            &primary,
            &offering,
            topology,
            MeasuredPhase::WarmEligible,
        )
        .await?;
        cli_captures.push(warm_capture);
        (cold, warm)
    } else {
        let cold = network
            .stream(
                &primary,
                &stream_payload(
                    topology,
                    &offering,
                    "cold",
                    edge_workspace.as_ref().map(|edge_workspace| {
                        (edge_id.as_str(), edge_workspace.path(), marker_name)
                    }),
                ),
            )
            .await?;
        let warm = network
            .stream(
                &primary,
                &stream_payload(
                    topology,
                    &offering,
                    "warm_eligible",
                    edge_workspace.as_ref().map(|edge_workspace| {
                        (edge_id.as_str(), edge_workspace.path(), marker_name)
                    }),
                ),
            )
            .await?;
        if topology == ProductionTopology::EdgeServer {
            validate_edge_scenario_execution([&cold.events, &warm.events], &edge_id, marker_name)?;
        }
        (cold, warm)
    };
    let primary_fairness_control_requests = if topology == ProductionTopology::CliServer {
        let control = network
            .stream(
                &primary,
                &stream_payload(
                    ProductionTopology::ServerOnly,
                    &offering,
                    "primary_fairness_control",
                    None,
                ),
            )
            .await?;
        control.admitted_durable_requests()
    } else {
        0
    };
    let primary_after = network.admission_counters().await?;

    let secondary_before = network.admission_counters().await?;
    let secondary_stream = network
        .stream(
            &secondary,
            &stream_payload(ProductionTopology::ServerOnly, &offering, "fairness", None),
        )
        .await?;
    let secondary_after = network.admission_counters().await?;

    let server_capture = server.graceful_stop().await?;
    let mut captures = vec![server_capture];
    captures.append(&mut cli_captures);
    if let Some(edge) = edge.take() {
        captures.push(edge.wait_for_exit().await?);
    }
    for capture in &captures {
        assert_capture_run_id(capture, &config.baseline_run_id)?;
    }
    let scenario = assemble_external_scenario(
        pool,
        ExternalScenarioFacts {
            baseline_run_id: config.baseline_run_id.clone(),
            topology,
            offering,
            primary,
            secondary,
            cold,
            warm_eligible,
            secondary_stream,
            primary_before,
            primary_after,
            secondary_before,
            secondary_after,
            primary_fairness_control_requests,
            process_captures: captures,
        },
    )
    .await?;
    write_json_atomic(&scenario_path, &scenario)?;
    if let Some(cli_workspace) = cli_workspace {
        cli_workspace.close()?;
    }
    if let Some(edge_workspace) = edge_workspace {
        edge_workspace.close()?;
    }
    Ok(())
}

fn spawn_server(
    config: &RunnerConfig,
    capture_scope: ProductionCaptureScope,
    capture_path: PathBuf,
    log_path: PathBuf,
) -> AnyResult<ManagedProcess> {
    require_absent(&capture_path)?;
    let ProcessLogs {
        paths: log_paths,
        stdout,
        stderr,
    } = process_logs(&log_path)?;
    let local_state_root = create_process_local_state_root(&config.output_dir)?;
    let capture_scope_json = serde_json::to_string(&capture_scope)?;
    let mut command = Command::new(&config.server_bin);
    command
        .current_dir(local_state_root.path())
        .env("ASTRA_API_HOST", "127.0.0.1")
        .env("ASTRA_API_PORT", config.server_port.to_string())
        .env(ASTRA_LOCAL_STATE_ROOT_ENV, local_state_root.path())
        .env(TRACE_ENV, "1")
        .env(PROCESS_CAPTURE_ENV, &capture_path)
        .env(BASELINE_RUN_ID_ENV, &config.baseline_run_id)
        .env(BASELINE_GIT_SHA_ENV, &config.git_sha)
        .env(BASELINE_CAPTURE_SCOPE_ENV, capture_scope_json)
        .env(ASTRA_CONFIG_SOURCE_ENV, ASTRA_CONFIG_SOURCE_EXPLICIT_ENV)
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        // Keep the child hermetic even when dotenv discovery finds an
        // unrelated ancestor file. dotenvy preserves explicitly-set values.
        .env("ASTRA_OUTPUT_STYLE", "default")
        .env(
            "ASTRA_PROMPT_OVERRIDES_DIR",
            local_state_root.path().join("prompts"),
        )
        .env("ASTRA_TEST_PROMPT_CACHE_DISABLED", "0")
        .env("ASTRA_TEST_E2E_SECRET", "")
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .kill_on_drop(true);
    let child = command.spawn()?;
    Ok(ManagedProcess {
        role: ProductionProcessRole::Server,
        expected_capture_scope: capture_scope,
        child,
        capture_path,
        log_paths,
        local_state_root: Some(local_state_root),
    })
}

fn spawn_edge(
    config: &RunnerConfig,
    credentials_dir: &Path,
    workspace: &Path,
    edge_id: &str,
    capture_scope: ProductionCaptureScope,
    capture_path: PathBuf,
    log_path: PathBuf,
) -> AnyResult<ManagedProcess> {
    require_absent(&capture_path)?;
    let ProcessLogs {
        paths: log_paths,
        stdout,
        stderr,
    } = process_logs(&log_path)?;
    let local_state_root = create_process_local_state_root(&config.output_dir)?;
    let capture_scope_json = serde_json::to_string(&capture_scope)?;
    let mut command = Command::new(&config.edge_bin);
    command
        .current_dir(workspace)
        .arg("--server-url")
        .arg(&config.server_origin)
        .arg("--profile")
        .arg("phase0-baseline")
        .arg("--workspace-dir")
        .arg(workspace)
        .arg("--edge-id")
        .arg(edge_id)
        .arg("--reconnect=false")
        .env("ASTRA_CLI_CREDENTIALS_DIR", credentials_dir)
        .env(ASTRA_LOCAL_STATE_ROOT_ENV, local_state_root.path())
        .env(TRACE_ENV, "1")
        .env(PROCESS_CAPTURE_ENV, &capture_path)
        .env(BASELINE_RUN_ID_ENV, &config.baseline_run_id)
        .env(BASELINE_GIT_SHA_ENV, &config.git_sha)
        .env(BASELINE_CAPTURE_SCOPE_ENV, capture_scope_json)
        .env(ASTRA_CONFIG_SOURCE_ENV, ASTRA_CONFIG_SOURCE_EXPLICIT_ENV)
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .env("ASTRA_OUTPUT_STYLE", "default")
        .env(
            "ASTRA_PROMPT_OVERRIDES_DIR",
            local_state_root.path().join("prompts"),
        )
        .env("ASTRA_TEST_PROMPT_CACHE_DISABLED", "0")
        .env("ASTRA_TEST_E2E_SECRET", "")
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .kill_on_drop(true);
    let child = command.spawn()?;
    Ok(ManagedProcess {
        role: ProductionProcessRole::Edge,
        expected_capture_scope: capture_scope,
        child,
        capture_path,
        log_paths,
        local_state_root: Some(local_state_root),
    })
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum CliStreamRecord {
    ExchangeStarted {
        schema: String,
        execution_id: String,
        durable: bool,
        session_id: Option<String>,
        session_turn: u32,
        turn_chain_id: String,
        user_query_event_id: String,
        exchange_id: String,
        request_ordinal: u32,
        round_index: u32,
    },
    SseEvent {
        schema: String,
        execution_id: String,
        durable: bool,
        session_id: Option<String>,
        session_turn: u32,
        turn_chain_id: String,
        user_query_event_id: String,
        exchange_id: String,
        request_ordinal: u32,
        round_index: u32,
        event_seq: u64,
        event: Value,
    },
    ExchangeFinished {
        schema: String,
        execution_id: String,
        durable: bool,
        session_id: Option<String>,
        session_turn: u32,
        turn_chain_id: String,
        user_query_event_id: String,
        exchange_id: String,
        request_ordinal: u32,
        round_index: u32,
        event_count: u64,
        server_run_id: String,
        stream_complete: bool,
        usage: Option<Value>,
        context_manifest_trace: Option<Value>,
        compactions: Vec<Value>,
        error: Option<Value>,
    },
    Result {
        schema: String,
        execution_id: String,
        durable: bool,
        session_id: Option<String>,
        session_turn: u32,
        turn_chain_id: String,
        user_query_event_id: String,
        result: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliStreamIdentity {
    execution_id: String,
    session_id: String,
    session_turn: u32,
    turn_chain_id: String,
    user_query_event_id: String,
}

#[derive(Debug)]
struct CliExchangeState {
    request_ordinal: u32,
    round_index: u32,
    event_count: u64,
    saw_session_info: bool,
    finished: bool,
}

impl CliStreamRecord {
    fn identity(&self, requested_session_id: &str) -> AnyResult<CliStreamIdentity> {
        let (
            schema,
            execution_id,
            durable,
            session_id,
            session_turn,
            turn_chain_id,
            user_query_event_id,
        ) = match self {
            Self::ExchangeStarted {
                schema,
                execution_id,
                durable,
                session_id,
                session_turn,
                turn_chain_id,
                user_query_event_id,
                ..
            }
            | Self::SseEvent {
                schema,
                execution_id,
                durable,
                session_id,
                session_turn,
                turn_chain_id,
                user_query_event_id,
                ..
            }
            | Self::ExchangeFinished {
                schema,
                execution_id,
                durable,
                session_id,
                session_turn,
                turn_chain_id,
                user_query_event_id,
                ..
            }
            | Self::Result {
                schema,
                execution_id,
                durable,
                session_id,
                session_turn,
                turn_chain_id,
                user_query_event_id,
                ..
            } => (
                schema,
                execution_id,
                durable,
                session_id,
                session_turn,
                turn_chain_id,
                user_query_event_id,
            ),
        };
        if schema != CLI_STREAM_JSON_SCHEMA {
            return Err(invalid(format!(
                "CLI stream-json schema {schema} != {CLI_STREAM_JSON_SCHEMA}"
            )));
        }
        if *durable {
            return Err(invalid(
                "CLI stream-json falsely claims durable run authority",
            ));
        }
        let session_id = session_id
            .as_deref()
            .ok_or_else(|| invalid("CLI stream-json omitted its known session_id"))?;
        if session_id != requested_session_id {
            return Err(invalid(format!(
                "CLI stream-json session {session_id} != requested {requested_session_id}"
            )));
        }
        if *session_turn == 0 {
            return Err(invalid("CLI stream-json session_turn is zero"));
        }
        let execution_id = required_bounded_string(execution_id, "CLI execution_id", 64)?;
        let turn_chain_id = required_bounded_string(turn_chain_id, "CLI turn_chain_id", 64)?;
        if execution_id != turn_chain_id {
            return Err(invalid(
                "CLI execution_id and turn_chain_id are not one stable bridge identity",
            ));
        }
        Ok(CliStreamIdentity {
            execution_id,
            session_id: session_id.to_string(),
            session_turn: *session_turn,
            turn_chain_id,
            user_query_event_id: required_bounded_string(
                user_query_event_id,
                "CLI user_query_event_id",
                64,
            )?,
        })
    }
}

fn parse_cli_stream_json(
    stdout: &[u8],
    requested_session_id: &str,
    phase_label: &str,
) -> AnyResult<StreamCapture> {
    let text = std::str::from_utf8(stdout).map_err(|error| {
        invalid(format!(
            "astra CLI {phase_label} stdout is not UTF-8: {error}"
        ))
    })?;
    if text.is_empty() {
        return Err(invalid(format!(
            "astra CLI {phase_label} emitted no stream-json records"
        )));
    }
    let mut identity = None;
    let mut exchanges = BTreeMap::<String, CliExchangeState>::new();
    let mut next_request_ordinal = 1_u32;
    let mut events = Vec::new();
    let mut result_seen = false;
    for (line_index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            return Err(invalid(format!(
                "astra CLI {phase_label} emitted a blank JSONL record at line {}",
                line_index + 1
            )));
        }
        if result_seen {
            return Err(invalid(format!(
                "astra CLI {phase_label} emitted data after its terminal result"
            )));
        }
        let record: CliStreamRecord = serde_json::from_str(line).map_err(|error| {
            invalid(format!(
                "astra CLI {phase_label} emitted invalid stream-json at line {}: {error}",
                line_index + 1
            ))
        })?;
        let record_identity = record.identity(requested_session_id)?;
        if let Some(expected) = identity.as_ref() {
            if expected != &record_identity {
                return Err(invalid(format!(
                    "astra CLI {phase_label} changed stream correlation at line {}",
                    line_index + 1
                )));
            }
        } else {
            identity = Some(record_identity.clone());
        }
        match record {
            CliStreamRecord::ExchangeStarted {
                exchange_id,
                request_ordinal,
                round_index,
                ..
            } => {
                let exchange_id = required_bounded_string(&exchange_id, "CLI exchange_id", 64)?;
                if request_ordinal != next_request_ordinal {
                    return Err(invalid(format!(
                        "CLI exchange ordinal {request_ordinal} != expected {next_request_ordinal}"
                    )));
                }
                next_request_ordinal = next_request_ordinal
                    .checked_add(1)
                    .ok_or_else(|| invalid("CLI request ordinal overflowed"))?;
                if exchanges
                    .insert(
                        exchange_id,
                        CliExchangeState {
                            request_ordinal,
                            round_index,
                            event_count: 0,
                            saw_session_info: false,
                            finished: false,
                        },
                    )
                    .is_some()
                {
                    return Err(invalid("CLI stream-json reused an exchange_id"));
                }
            }
            CliStreamRecord::SseEvent {
                exchange_id,
                request_ordinal,
                round_index,
                event_seq,
                event,
                ..
            } => {
                let exchange = exchanges.get_mut(&exchange_id).ok_or_else(|| {
                    invalid(format!(
                        "CLI SSE event references unstarted exchange {exchange_id}"
                    ))
                })?;
                if exchange.finished {
                    return Err(invalid(format!(
                        "CLI SSE event followed terminal exchange {exchange_id}"
                    )));
                }
                if request_ordinal != exchange.request_ordinal
                    || round_index != exchange.round_index
                {
                    return Err(invalid(format!(
                        "CLI exchange {exchange_id} changed ordinal/round coordinates"
                    )));
                }
                let expected_event_seq = exchange
                    .event_count
                    .checked_add(1)
                    .ok_or_else(|| invalid("CLI SSE event sequence overflowed"))?;
                if event_seq != expected_event_seq {
                    return Err(invalid(format!(
                        "CLI exchange {exchange_id} event_seq {event_seq} != {expected_event_seq}"
                    )));
                }
                let event_type = event.get("type").and_then(Value::as_str).ok_or_else(|| {
                    invalid(format!(
                        "CLI exchange {exchange_id} emitted an untyped SSE event"
                    ))
                })?;
                required_bounded_string(event_type, "CLI SSE event type", 128)?;
                if event_type == "session_info" {
                    let session_id = required_json_string(&event, "session_id")?;
                    let server_run_id = required_json_string(&event, "run_id")?;
                    let turn_chain_id = required_json_string(&event, "turn_chain_id")?;
                    if session_id != record_identity.session_id
                        || server_run_id != record_identity.turn_chain_id
                        || turn_chain_id != record_identity.turn_chain_id
                        || event.get("durable").and_then(Value::as_bool) != Some(false)
                    {
                        return Err(invalid(format!(
                            "CLI exchange {exchange_id} session_info disagrees with stable transport correlation"
                        )));
                    }
                    exchange.saw_session_info = true;
                }
                exchange.event_count = event_seq;
                events.push(event);
            }
            CliStreamRecord::ExchangeFinished {
                exchange_id,
                request_ordinal,
                round_index,
                event_count,
                server_run_id,
                stream_complete,
                usage,
                context_manifest_trace,
                compactions,
                error,
                ..
            } => {
                let exchange = exchanges.get_mut(&exchange_id).ok_or_else(|| {
                    invalid(format!(
                        "CLI finish references unstarted exchange {exchange_id}"
                    ))
                })?;
                if exchange.finished {
                    return Err(invalid(format!(
                        "CLI exchange {exchange_id} finished more than once"
                    )));
                }
                if request_ordinal != exchange.request_ordinal
                    || round_index != exchange.round_index
                    || event_count != exchange.event_count
                {
                    return Err(invalid(format!(
                        "CLI exchange {exchange_id} terminal counters changed"
                    )));
                }
                if !stream_complete || error.is_some() {
                    return Err(invalid(format!(
                        "CLI exchange {exchange_id} did not reach a clean [DONE] terminal"
                    )));
                }
                required_bounded_string(&server_run_id, "CLI server_run_id", 64)?;
                if server_run_id != record_identity.execution_id
                    || server_run_id != record_identity.turn_chain_id
                {
                    return Err(invalid(format!(
                        "CLI exchange {exchange_id} server_run_id disagrees with turn_chain_id"
                    )));
                }
                if usage.as_ref().is_some_and(|value| !value.is_object())
                    || context_manifest_trace
                        .as_ref()
                        .is_some_and(|value| !value.is_object())
                    || compactions.iter().any(|value| !value.is_object())
                {
                    return Err(invalid(format!(
                        "CLI exchange {exchange_id} terminal typed evidence is malformed"
                    )));
                }
                if !exchange.saw_session_info {
                    return Err(invalid(format!(
                        "CLI exchange {exchange_id} omitted typed session_info"
                    )));
                }
                exchange.finished = true;
            }
            CliStreamRecord::Result { result, .. } => {
                if exchanges.is_empty() || exchanges.values().any(|exchange| !exchange.finished) {
                    return Err(invalid(
                        "CLI terminal result preceded a complete exchange set",
                    ));
                }
                if !result.is_object()
                    || result.get("success").and_then(Value::as_bool) != Some(true)
                {
                    return Err(invalid(format!(
                        "astra CLI {phase_label} terminal result is not successful"
                    )));
                }
                if result.get("run_id").is_some() {
                    return Err(invalid(
                        "CLI terminal result retained ambiguous legacy run_id",
                    ));
                }
                result_seen = true;
            }
        }
    }
    if !result_seen {
        return Err(invalid(format!(
            "astra CLI {phase_label} omitted its terminal result record"
        )));
    }
    reject_typed_error(&events, "CLI stream-json")?;
    let identity = identity.ok_or_else(|| invalid("CLI stream-json has no identity"))?;
    let exchange_count =
        u32::try_from(exchanges.len()).map_err(|_| invalid("CLI exchange count exceeds u32"))?;
    Ok(StreamCapture {
        session_id: identity.session_id,
        authority: StreamAuthority::CliSessionBridge {
            execution_id: identity.execution_id,
            session_turn: identity.session_turn,
            turn_chain_id: identity.turn_chain_id,
            user_query_event_id: identity.user_query_event_id,
            exchange_count,
        },
        events,
    })
}

async fn run_cli_turn(
    config: &RunnerConfig,
    credentials_dir: &Path,
    workspace: &Path,
    tenant: &LiveTenant,
    offering: &ResolvedOffering,
    topology: ProductionTopology,
    phase: MeasuredPhase,
) -> AnyResult<(StreamCapture, ProductionProcessCapture)> {
    let phase_label = phase.label();
    let suffix = format!("cli-{phase_label}");
    let capture_path = config.capture_path(topology, offering.window_class, &suffix);
    require_absent(&capture_path)?;
    let local_state_root = create_process_local_state_root(&config.output_dir)?;
    let capture_scope =
        serde_json::to_string(&phase.capture_scope(topology, offering.window_class))?;
    let log_path = config.output_dir.join(format!(
        "{}_{}.{}.log",
        topology_label(topology),
        window_label(offering.window_class),
        suffix
    ));
    let stream_path = config.output_dir.join(format!(
        "{}_{}.{}.stream-json.jsonl",
        topology_label(topology),
        window_label(offering.window_class),
        suffix
    ));
    require_absent(&stream_path)?;
    let log = create_new_file(&log_path)?;
    let prompt = serde_json::to_string(&json!({
        "schema": "astra.phase0.cli_turn.v1",
        "phase": phase_label,
        "window_class": window_label(offering.window_class),
        "response_contract": {
            "kind": "concise_text",
            "tool_calls": "not_required",
        }
    }))?;
    let mut command = Command::new(&config.cli_bin);
    command
        .current_dir(workspace)
        .arg("--api-url")
        .arg(&config.server_origin)
        .arg("--profile")
        .arg("phase0-baseline")
        .arg("--model")
        .arg(&offering.model_name)
        .arg("--print")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--session-id")
        .arg(&tenant.session_id)
        .arg("--bare")
        .arg("--no-instructions")
        .arg("--yes")
        .arg(prompt)
        .env("ASTRA_CLI_CREDENTIALS_DIR", credentials_dir)
        .env(ASTRA_LOCAL_STATE_ROOT_ENV, local_state_root.path())
        .env(TRACE_ENV, "1")
        .env(PROCESS_CAPTURE_ENV, &capture_path)
        .env(BASELINE_RUN_ID_ENV, &config.baseline_run_id)
        .env(BASELINE_GIT_SHA_ENV, &config.git_sha)
        .env(BASELINE_CAPTURE_SCOPE_ENV, capture_scope)
        .env(ASTRA_CONFIG_SOURCE_ENV, ASTRA_CONFIG_SOURCE_EXPLICIT_ENV)
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .env("ASTRA_OUTPUT_STYLE", "default")
        .env(
            "ASTRA_PROMPT_OVERRIDES_DIR",
            local_state_root.path().join("prompts"),
        )
        .env("ASTRA_TEST_PROMPT_CACHE_DISABLED", "0")
        .env("ASTRA_TEST_E2E_SECRET", "")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log))
        .kill_on_drop(true);
    let output = tokio::time::timeout(STREAM_TIMEOUT, command.output())
        .await
        .map_err(|_| {
            invalid(format!(
                "astra CLI {phase_label} timed out; stderr={}; stream={}",
                log_path.display(),
                stream_path.display()
            ))
        })??;
    write_private_file(&stream_path, &output.stdout)?;
    if !output.status.success() {
        return Err(invalid(format!(
            "astra CLI {phase_label} exited with {}; stderr={}; stream={}",
            output.status,
            log_path.display(),
            stream_path.display()
        )));
    }
    let stream = parse_cli_stream_json(&output.stdout, &tenant.session_id, phase_label)?;
    let capture = read_capture(
        &capture_path,
        ProductionProcessRole::Cli,
        phase.capture_scope(topology, offering.window_class),
    )?;
    local_state_root.close()?;
    Ok((stream, capture))
}

async fn wait_for_edge(
    network: &NetworkClient,
    auth_header: &str,
    edge_id: &str,
    process: &mut ManagedProcess,
) -> AnyResult<()> {
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    loop {
        if let Some(status) = process.child.try_wait()? {
            return Err(invalid(format!(
                "astra-edge exited before registration with {status}; logs={}",
                process.log_paths
            )));
        }
        if let Ok(body) = network.get_json("/edges/status", auth_header).await
            && body
                .get("edges")
                .and_then(Value::as_array)
                .is_some_and(|edges| {
                    edges.iter().any(|edge| {
                        edge.get("edge_agent_id").and_then(Value::as_str) == Some(edge_id)
                    })
                })
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(invalid(format!(
                "astra-edge {edge_id} did not register within {READY_TIMEOUT:?}"
            )));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn register_user(network: &NetworkClient, role: &str) -> AnyResult<RegisteredUser> {
    let suffix = Uuid::new_v4().simple().to_string();
    let username = format!("phase0_external_{role}_{suffix}");
    let password = "Phase0-external-baseline-9".to_string();
    let body = network
        .post_json(
            "/auth/register",
            None,
            &json!({
                "username": username,
                "email": format!("phase0_external_{role}_{suffix}@e2e.test"),
                "password": password,
                "display_name": format!("Phase-0 external {role}"),
            }),
            StatusCode::CREATED,
        )
        .await?;
    Ok(RegisteredUser {
        user_id: required_json_string(&body, "user_id")?,
        username,
        password,
        access_token: required_json_string(&body, "access_token")?,
        refresh_token: required_json_string(&body, "refresh_token")?,
    })
}

async fn login_user(
    network: &NetworkClient,
    previous: RegisteredUser,
) -> AnyResult<RegisteredUser> {
    let body = network
        .post_json(
            "/auth/login",
            None,
            &json!({
                "username": previous.username,
                "password": previous.password,
            }),
            StatusCode::OK,
        )
        .await?;
    let response_user_id = body
        .get("user_id")
        .and_then(Value::as_str)
        .unwrap_or(previous.user_id.as_str());
    if response_user_id != previous.user_id {
        return Err(invalid("login returned a different stable user_id"));
    }
    Ok(RegisteredUser {
        user_id: previous.user_id,
        username: previous.username,
        password: previous.password,
        access_token: required_json_string(&body, "access_token")?,
        refresh_token: required_json_string(&body, "refresh_token")?,
    })
}

async fn grant_admin(pool: &MySqlPool, user_id: &str) -> AnyResult<()> {
    sqlx::query(
        "INSERT IGNORE INTO auth_user_roles (user_id, role_id) \
         SELECT ?, role_id FROM auth_roles WHERE role_name = 'astra_admin' LIMIT 1",
    )
    .bind(user_id)
    .execute(pool)
    .await?;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM auth_user_roles ur \
         INNER JOIN auth_roles r ON r.role_id = ur.role_id \
         WHERE ur.user_id = ? AND r.role_name = 'astra_admin'",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    if count != 1 {
        return Err(invalid(format!(
            "expected exactly one astra_admin binding, found {count}"
        )));
    }
    Ok(())
}

async fn create_window_offerings(
    network: &NetworkClient,
    pool: &MySqlPool,
    admin: &RegisteredUser,
    config: &RunnerConfig,
    source: &SourceModel,
) -> AnyResult<BTreeMap<WindowClass, ResolvedOffering>> {
    let auth = format!("Bearer {}", admin.access_token);
    let mut resolved = BTreeMap::new();
    let mut ids = BTreeSet::new();
    for window in WindowClass::ALL {
        let context_window = exact_context_window(window);
        let local_name = format!(
            "phase0-{}-{}",
            &config.baseline_run_id[..12],
            window_label(window)
        );
        let context_window_i32 = i32::try_from(context_window)
            .map_err(|_| invalid("context window exceeds i32 API contract"))?;
        let body = network
            .post_json(
                "/models",
                Some(&auth),
                &json!({
                    "name": local_name,
                    "provider": source.provider,
                    "api_key": source.api_key,
                    "base_url": source.base_url,
                    "description": source.description.as_deref().unwrap_or(
                        "Phase-0 production history-work baseline Offering"
                    ),
                    "context_window": context_window_i32,
                    "max_completion_tokens": source.max_completion_tokens,
                    "input_modalities": source.input_modalities,
                    "output_modalities": source.output_modalities,
                    "supported_parameters": source.supported_parameters,
                    "pricing": source.pricing,
                    "architecture": source.architecture,
                    "tags": source.tags,
                    "quirks": source.quirks,
                }),
                StatusCode::CREATED,
            )
            .await?;
        let offering_id = required_json_string(&body, "model_id")?;
        if !ids.insert(offering_id.clone()) {
            return Err(invalid("model creation returned a duplicate Offering ID"));
        }
        let row = sqlx::query(
            "SELECT model_name, provider, context_window, is_active \
             FROM infra_llm_models WHERE model_id = ? LIMIT 1",
        )
        .bind(&offering_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| invalid("created Offering is absent from infra_llm_models"))?;
        let db_name: String = row.try_get("model_name")?;
        let db_provider: String = row.try_get("provider")?;
        let db_window: i32 = row.try_get("context_window")?;
        let is_active: i16 = row.try_get("is_active")?;
        if db_name != local_name
            || db_provider != source.provider
            || db_window != context_window_i32
            || is_active != 1
        {
            return Err(invalid(format!(
                "created Offering DB evidence disagrees for {}",
                window_label(window)
            )));
        }
        resolved.insert(
            window,
            ResolvedOffering {
                window_class: window,
                offering_id,
                model_name: local_name,
                provider: source.provider.clone(),
                context_window_tokens: context_window,
            },
        );
    }
    Ok(resolved)
}

async fn create_session(
    network: &NetworkClient,
    user: &RegisteredUser,
    topology: ProductionTopology,
    window: WindowClass,
    offering: &ResolvedOffering,
    role: &str,
) -> AnyResult<LiveTenant> {
    let auth_header = format!("Bearer {}", user.access_token);
    let body = network
        .post_json(
            "/sessions",
            Some(&auth_header),
            &json!({
                "title": format!(
                    "Phase-0 external {} {} {role}",
                    topology_label(topology),
                    window_label(window)
                ),
                "metadata": {
                    "suite": "phase0_external_production_baseline",
                    "topology": topology_label(topology),
                    "window_class": window_label(window),
                    "offering_id": offering.offering_id,
                    "tenant_role": role,
                }
            }),
            StatusCode::CREATED,
        )
        .await?;
    Ok(LiveTenant {
        user_id: user.user_id.clone(),
        auth_header,
        session_id: required_json_string(&body, "session_id")?,
    })
}

fn write_credentials(directory: &Path, user: &RegisteredUser) -> AnyResult<()> {
    fs::create_dir_all(directory)?;
    let path = directory.join("credentials.json");
    let bytes = serde_json::to_vec_pretty(&json!({
        "current_profile": "phase0-baseline",
        "profiles": {
            "phase0-baseline": {
                "username": user.username,
                "account_id": user.user_id,
                "access_token": user.access_token,
                "refresh_token": user.refresh_token,
            }
        }
    }))?;
    let temp = directory.join(format!("credentials.{}.tmp", Uuid::new_v4().simple()));
    write_private_file(&temp, &bytes)?;
    fs::rename(&temp, &path)?;
    Ok(())
}

fn stream_payload(
    topology: ProductionTopology,
    offering: &ResolvedOffering,
    phase: &str,
    edge: Option<(&str, &Path, &str)>,
) -> Value {
    let task = match edge {
        Some((edge_id, _, marker_name)) => json!({
            "schema": "astra.phase0.edge_task.v1",
            "operation": {
                "tool": "read_file",
                "arguments": {
                    "path": marker_name,
                }
            },
            "completion": {
                "requires_successful_tool_result": true,
                "response_kind": "concise_text",
            },
            "executor_id": edge_id,
            "phase": phase,
        }),
        None => json!({
            "schema": "astra.phase0.server_task.v1",
            "operation": "respond",
            "response_kind": "concise_text",
            "phase": phase,
        }),
    };
    let mut payload = json!({
        "message": serde_json::to_string(&task).expect("JSON value serializes"),
        "session_id": offering_session_placeholder(),
        "model_selection": {
            "offering_id": offering.offering_id,
        },
        "context": {
            "phase0_production_baseline": {
                "schema": "astra.phase0.external_request.v1",
                "topology": topology_label(topology),
                "phase": phase,
                "window_class": window_label(offering.window_class),
            }
        },
        "execution_budget": {
            "initial_turns": 4,
            "hard_turn_limit": 4,
        },
        "interaction_mode": "non_interactive",
    });
    // `NetworkClient::stream` owns the tenant identity and replaces this
    // explicit placeholder before serialization.
    if let Some((edge_id, workspace, _)) = edge {
        let workspace = workspace.to_string_lossy().to_string();
        payload["runtime_system_prompt"] = Value::String(
            "Consume the structured task object. Execute its declared operation through the admitted tool, then return concise text based on the typed tool result."
                .to_string(),
        );
        payload["allow_tools"] = json!(["read_file"]);
        payload["edge_executor_id"] = Value::String(edge_id.to_string());
        payload["workspace_binding"] = json!({
            "kind": "edge_workspace",
            "display_name": "Phase-0 edge workspace",
            "root": workspace,
            "authority": "read_write",
        });
        payload["executor_binding"] = json!({
            "kind": "edge_agent",
            "executor_id": edge_id,
            "display_name": "Phase-0 astra-edge",
            "transport": "edge_ws",
            "status": "online",
        });
    }
    payload
}

fn offering_session_placeholder() -> &'static str {
    "__ASTRA_PHASE0_TENANT_SESSION__"
}

fn validate_edge_scenario_execution(
    streams: [&[Value]; 2],
    edge_id: &str,
    marker_name: &str,
) -> AnyResult<()> {
    let completed_streams = streams.into_iter().try_fold(0_u8, |count, events| {
        validate_edge_execution_stream(events, edge_id, marker_name).map(|executed| {
            count
                .checked_add(u8::from(executed))
                .expect("two streams fit in u8")
        })
    })?;
    if completed_streams == 0 {
        return Err(invalid(
            "edge workload did not execute its typed read_file operation in either measured stream",
        ));
    }
    Ok(())
}

fn validate_edge_execution_stream(
    events: &[Value],
    edge_id: &str,
    marker_name: &str,
) -> AnyResult<bool> {
    let started = events
        .iter()
        .filter(|event| event.get("type").and_then(Value::as_str) == Some("tool_transport_started"))
        .collect::<Vec<_>>();
    if started.is_empty() {
        let dangling_edge_lifecycle = events.iter().any(|event| {
            (event.get("type").and_then(Value::as_str) == Some("tool_routing_decision")
                && event.get("route").and_then(Value::as_str) == Some("edge_bound"))
                || (matches!(
                    event.get("type").and_then(Value::as_str),
                    Some("tool_transport_completed" | "tool_transport_failed")
                ) && event
                    .pointer("/executor/executor_id")
                    .and_then(Value::as_str)
                    == Some(edge_id))
                || (event.get("type").and_then(Value::as_str) == Some("tool_call_end")
                    && event.get("transport").and_then(Value::as_str) == Some("edge_ledger"))
        });
        if dangling_edge_lifecycle {
            return Err(invalid(
                "edge stream emitted a typed lifecycle without its transport start",
            ));
        }
        return Ok(false);
    }
    if started.len() != 1 {
        return Err(invalid(format!(
            "one measured edge stream emitted {} typed read_file transport starts",
            started.len()
        )));
    }
    if started[0].get("tool").and_then(Value::as_str) != Some("read_file")
        || started[0]
            .pointer("/arguments/path")
            .and_then(Value::as_str)
            != Some(marker_name)
        || started[0].get("transport").and_then(Value::as_str) != Some("edge_ws")
        || started[0]
            .pointer("/executor/executor_id")
            .and_then(Value::as_str)
            != Some(edge_id)
    {
        return Err(invalid(
            "typed edge read_file start did not target the pinned workspace operation",
        ));
    }
    let call_id = required_json_string(started[0], "call_id")?;
    let routing = events
        .iter()
        .filter(|event| {
            event.get("type").and_then(Value::as_str) == Some("tool_routing_decision")
                && event.get("call_id").and_then(Value::as_str) == Some(call_id.as_str())
                && event.get("tool").and_then(Value::as_str) == Some("read_file")
                && event.get("route").and_then(Value::as_str) == Some("edge_bound")
        })
        .count();
    if routing != 1 {
        return Err(invalid(
            "typed tool routing did not select the unique edge_bound route",
        ));
    }
    let completed = events
        .iter()
        .filter(|event| {
            event.get("type").and_then(Value::as_str) == Some("tool_transport_completed")
                && event.get("call_id").and_then(Value::as_str) == Some(call_id.as_str())
        })
        .collect::<Vec<_>>();
    if completed.len() != 1
        || completed[0].get("success").and_then(Value::as_bool) != Some(true)
        || completed[0].get("transport").and_then(Value::as_str) != Some("edge_ledger")
        || completed[0]
            .pointer("/executor/executor_id")
            .and_then(Value::as_str)
            != Some(edge_id)
        || completed[0]
            .pointer("/executor/transport")
            .and_then(Value::as_str)
            != Some("edge_ledger")
    {
        return Err(invalid(
            "typed edge tool transport did not complete successfully on the pinned executor",
        ));
    }
    let terminal = events.iter().filter(|event| {
        event.get("type").and_then(Value::as_str) == Some("tool_call_end")
            && event.get("call_id").and_then(Value::as_str) == Some(call_id.as_str())
            && event.get("success").and_then(Value::as_bool) == Some(true)
            && event.get("transport").and_then(Value::as_str) == Some("edge_ledger")
            && event
                .pointer("/executor/executor_id")
                .and_then(Value::as_str)
                == Some(edge_id)
    });
    if terminal.count() != 1 {
        return Err(invalid(
            "edge workload omitted the unique typed edge tool_call_end",
        ));
    }
    Ok(true)
}

fn read_capture(
    path: &Path,
    expected_role: ProductionProcessRole,
    expected_scope: ProductionCaptureScope,
) -> AnyResult<ProductionProcessCapture> {
    let bytes = fs::read(path)?;
    let capture: ProductionProcessCapture = serde_json::from_slice(&bytes)?;
    if capture.schema != PROCESS_CAPTURE_SCHEMA
        || capture.role != expected_role
        || capture.executable_name != expected_role.expected_executable_name()
        || capture.scope != expected_scope
        || capture.build_git_dirty
        || capture.capture_id
            != production_process_capture_id(&capture.baseline_run_id, capture.role, capture.scope)
    {
        return Err(invalid(format!(
            "invalid {:?} process capture at {}",
            expected_role,
            path.display()
        )));
    }
    if capture.finished_at_unix_seconds < capture.started_at_unix_seconds
        || capture.pid == 0
        || capture.executable_sha256.len() != 64
        || !capture
            .executable_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(invalid(format!(
            "malformed {:?} process capture at {}",
            expected_role,
            path.display()
        )));
    }
    if capture
        .sites
        .iter()
        .any(|site| site.queue_current_bytes_change != 0 || site.accounting_errors != 0)
    {
        return Err(invalid(format!(
            "{:?} process capture has leaked queue bytes or accounting errors",
            expected_role
        )));
    }
    Ok(capture)
}

fn assert_capture_run_id(
    capture: &ProductionProcessCapture,
    expected_run_id: &str,
) -> AnyResult<()> {
    if capture.baseline_run_id != expected_run_id {
        return Err(invalid(format!(
            "{:?} capture has a foreign baseline run id",
            capture.role
        )));
    }
    let expected_git_sha = required_nonempty_env(BASELINE_GIT_SHA_ENV)?;
    if capture.git_sha != expected_git_sha {
        return Err(invalid(format!(
            "{:?} capture has a foreign git SHA",
            capture.role
        )));
    }
    Ok(())
}

fn reject_typed_error(events: &[Value], source: &str) -> AnyResult<()> {
    if events
        .iter()
        .any(|event| event.get("type").and_then(Value::as_str) == Some("error"))
    {
        return Err(invalid(format!("{source} emitted a typed error event")));
    }
    Ok(())
}

fn required_json_string(value: &Value, field: &str) -> AnyResult<String> {
    let value = value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("JSON field `{field}` is missing or not a string")))?;
    required_nonempty_string(value, &format!("JSON field `{field}`"))
}

fn required_nonempty_string(value: &str, field: &str) -> AnyResult<String> {
    if value.is_empty() || value != value.trim() || value.chars().any(char::is_control) {
        return Err(invalid(format!(
            "{field} is not an exact non-empty identity"
        )));
    }
    Ok(value.to_string())
}

fn required_bounded_string(value: &str, field: &str, max_bytes: usize) -> AnyResult<String> {
    let value = required_nonempty_string(value, field)?;
    if value.len() > max_bytes {
        return Err(invalid(format!("{field} exceeds {max_bytes} bytes")));
    }
    Ok(value)
}

fn process_logs(stdout_path: &Path) -> AnyResult<ProcessLogs> {
    let stderr_path = stdout_path.with_extension("stderr.log");
    let stdout = create_new_file(stdout_path)?;
    let stderr = match create_new_file(&stderr_path) {
        Ok(stderr) => stderr,
        Err(error) => {
            drop(stdout);
            if let Err(cleanup_error) = fs::remove_file(stdout_path) {
                return Err(invalid(format!(
                    "failed to create stderr log {}: {error}; also failed to roll back stdout log {}: {cleanup_error}",
                    stderr_path.display(),
                    stdout_path.display()
                )));
            }
            return Err(error);
        }
    };
    Ok(ProcessLogs {
        paths: ProcessLogPaths {
            stdout: stdout_path.to_path_buf(),
            stderr: stderr_path,
        },
        stdout: Stdio::from(stdout),
        stderr: Stdio::from(stderr),
    })
}

fn create_process_local_state_root(output_dir: &Path) -> AnyResult<tempfile::TempDir> {
    let root = tempfile::Builder::new()
        .prefix(".phase0-local-state-")
        .tempdir_in(output_dir)?;
    if !root.path().is_absolute() || !root.path().starts_with(output_dir) {
        return Err(invalid(format!(
            "process local-state root escaped the absolute baseline output directory: {}",
            root.path().display()
        )));
    }
    Ok(root)
}

fn create_new_file(path: &Path) -> AnyResult<File> {
    if let Some(parent) = path.parent()
        && !parent.is_dir()
    {
        return Err(invalid(format!(
            "output parent does not exist: {}",
            parent.display()
        )));
    }
    Ok(OpenOptions::new().write(true).create_new(true).open(path)?)
}

fn write_private_file(path: &Path, bytes: &[u8]) -> AnyResult<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    use std::io::Write;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn require_absent(path: &Path) -> AnyResult<()> {
    if path.exists() {
        return Err(invalid(format!(
            "refusing to overwrite existing baseline output: {}",
            path.display()
        )));
    }
    Ok(())
}

fn required_existing_file(name: &str) -> AnyResult<PathBuf> {
    let path = PathBuf::from(required_nonempty_env(name)?).canonicalize()?;
    if !path.is_file() {
        return Err(invalid(format!("{name} is not a file: {}", path.display())));
    }
    Ok(path)
}

fn required_nonempty_env(name: &str) -> AnyResult<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid(format!("{name} must be set to a non-empty value")))
}

fn require_exact_env(name: &str, expected: &str) -> AnyResult<()> {
    let actual = std::env::var(name).unwrap_or_default();
    if actual != expected {
        return Err(invalid(format!("{name} must be exactly `{expected}`")));
    }
    Ok(())
}

fn require_binary_name(path: &Path, expected: &str) -> AnyResult<()> {
    let actual = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.strip_suffix(".exe").unwrap_or(name))
        .ok_or_else(|| {
            invalid(format!(
                "binary path is not valid UTF-8: {}",
                path.display()
            ))
        })?;
    if actual != expected {
        return Err(invalid(format!(
            "expected binary `{expected}`, got `{actual}`"
        )));
    }
    Ok(())
}

fn validate_run_id(run_id: &str) -> AnyResult<()> {
    if run_id.len() != 64 || !run_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid(format!(
            "{BASELINE_RUN_ID_ENV} must be a 64-character hexadecimal value"
        )));
    }
    Ok(())
}

fn validate_git_sha(workspace_root: &Path, configured: &str) -> AnyResult<()> {
    if !matches!(configured.len(), 40 | 64)
        || !configured.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(invalid(format!(
            "{BASELINE_GIT_SHA_ENV} must be a full hexadecimal Git object id"
        )));
    }
    let head = std::process::Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(workspace_root)
        .output()?;
    if !head.status.success() {
        return Err(invalid("git rev-parse HEAD failed"));
    }
    let actual = std::str::from_utf8(&head.stdout)?.trim();
    if actual != configured {
        return Err(invalid(format!(
            "{BASELINE_GIT_SHA_ENV} does not match the checked-out HEAD"
        )));
    }
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=all"])
        .current_dir(workspace_root)
        .output()?;
    if !status.status.success() {
        return Err(invalid("git status failed"));
    }
    if !status.stdout.is_empty() {
        return Err(invalid(
            "production baseline requires a completely clean git worktree",
        ));
    }
    Ok(())
}

const fn topology_label(topology: ProductionTopology) -> &'static str {
    match topology {
        ProductionTopology::CliServer => "cli_server",
        ProductionTopology::ServerOnly => "server_only",
        ProductionTopology::EdgeServer => "edge_server",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli_stream_record(record_type: &str) -> serde_json::Map<String, Value> {
        serde_json::Map::from_iter([
            ("schema".to_string(), json!(CLI_STREAM_JSON_SCHEMA)),
            ("execution_id".to_string(), json!("run-execution")),
            ("durable".to_string(), json!(false)),
            ("session_id".to_string(), json!("session-1")),
            ("session_turn".to_string(), json!(3)),
            ("turn_chain_id".to_string(), json!("run-execution")),
            ("user_query_event_id".to_string(), json!("query-event")),
            ("type".to_string(), json!(record_type)),
        ])
    }

    fn cli_exchange_records(
        exchange_id: &str,
        request_ordinal: u32,
        round_index: u32,
    ) -> Vec<Value> {
        let mut started = cli_stream_record("exchange_started");
        started.insert("exchange_id".to_string(), json!(exchange_id));
        started.insert("request_ordinal".to_string(), json!(request_ordinal));
        started.insert("round_index".to_string(), json!(round_index));

        let mut event = cli_stream_record("sse_event");
        event.insert("exchange_id".to_string(), json!(exchange_id));
        event.insert("request_ordinal".to_string(), json!(request_ordinal));
        event.insert("round_index".to_string(), json!(round_index));
        event.insert("event_seq".to_string(), json!(1));
        event.insert(
            "event".to_string(),
            json!({
                "type": "session_info",
                "session_id": "session-1",
                "run_id": "run-execution",
                "turn_chain_id": "run-execution",
                "durable": false,
            }),
        );

        let mut finished = cli_stream_record("exchange_finished");
        finished.insert("exchange_id".to_string(), json!(exchange_id));
        finished.insert("request_ordinal".to_string(), json!(request_ordinal));
        finished.insert("round_index".to_string(), json!(round_index));
        finished.insert("event_count".to_string(), json!(1));
        finished.insert("server_run_id".to_string(), json!("run-execution"));
        finished.insert("stream_complete".to_string(), json!(true));
        finished.insert("usage".to_string(), Value::Null);
        finished.insert("context_manifest_trace".to_string(), Value::Null);
        finished.insert("compactions".to_string(), json!([]));
        finished.insert("error".to_string(), Value::Null);
        vec![
            Value::Object(started),
            Value::Object(event),
            Value::Object(finished),
        ]
    }

    fn serialize_cli_records(records: &[Value]) -> String {
        records
            .iter()
            .map(|record| serde_json::to_string(record).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn offering() -> ResolvedOffering {
        ResolvedOffering {
            window_class: WindowClass::M1,
            offering_id: "offering-1".to_string(),
            model_name: "phase0-model".to_string(),
            provider: "openai".to_string(),
            context_window_tokens: 1_000_000,
        }
    }

    #[test]
    fn source_model_selection_is_exact_and_keeps_wire_identity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("models.yaml");
        fs::write(
            &path,
            r#"
- name: deepseek-v4-flash-preview
  provider: openai
  api_key: preview-secret
- name: deepseek-v4-flash
  provider: openai
  api_key: production-secret
  base_url: https://provider.invalid/v1
"#,
        )
        .unwrap();

        let source = load_source_model(&path, "deepseek-v4-flash").unwrap();

        assert_eq!(source.provider, "openai");
        assert_eq!(
            source.quirks.wire_model_name.as_deref(),
            Some("deepseek-v4-flash")
        );
    }

    #[test]
    fn child_local_state_roots_are_unique_and_removed_on_success() {
        let output = tempfile::tempdir().unwrap();
        let first = create_process_local_state_root(output.path()).unwrap();
        let first_path = first.path().to_path_buf();
        let second = create_process_local_state_root(output.path()).unwrap();
        let second_path = second.path().to_path_buf();

        assert_ne!(first_path, second_path);
        assert!(first_path.starts_with(output.path()));
        assert!(second_path.starts_with(output.path()));
        first.close().unwrap();
        second.close().unwrap();
        assert!(!first_path.exists());
        assert!(!second_path.exists());
    }

    #[test]
    fn child_local_state_root_is_removed_when_work_returns_an_error() {
        fn fail_after_allocating(output: &Path, observed: &mut PathBuf) -> AnyResult<()> {
            let root = create_process_local_state_root(output)?;
            *observed = root.path().to_path_buf();
            Err(invalid("expected test failure"))
        }

        let output = tempfile::tempdir().unwrap();
        let mut observed = PathBuf::new();
        assert!(fail_after_allocating(output.path(), &mut observed).is_err());
        assert!(!observed.exists());
    }

    #[test]
    fn process_logs_expose_distinct_typed_stdout_and_stderr_paths() {
        let output = tempfile::tempdir().unwrap();
        let stdout_path = output.path().join("server.log");
        let logs = process_logs(&stdout_path).unwrap();

        assert_eq!(logs.paths.stdout, stdout_path);
        assert_eq!(logs.paths.stderr, output.path().join("server.stderr.log"));
        assert_ne!(logs.paths.stdout, logs.paths.stderr);
        assert!(logs.paths.stdout.is_file());
        assert!(logs.paths.stderr.is_file());
        assert_eq!(
            logs.paths.to_string(),
            format!(
                "stdout={} stderr={}",
                logs.paths.stdout.display(),
                logs.paths.stderr.display()
            )
        );
    }

    #[test]
    fn process_logs_roll_back_stdout_when_stderr_creation_fails() {
        let output = tempfile::tempdir().unwrap();
        let stdout_path = output.path().join("edge.log");
        let stderr_path = output.path().join("edge.stderr.log");
        fs::write(&stderr_path, b"preexisting").unwrap();

        assert!(process_logs(&stdout_path).is_err());
        assert!(
            !stdout_path.exists(),
            "partial stdout allocation must be rolled back"
        );
        assert_eq!(fs::read(stderr_path).unwrap(), b"preexisting");
    }

    #[test]
    fn edge_payload_carries_explicit_workspace_and_executor_bindings() {
        let payload = stream_payload(
            ProductionTopology::EdgeServer,
            &offering(),
            "cold",
            Some(("edge-1", Path::new("/tmp/phase0-edge"), "marker.json")),
        );

        assert_eq!(payload["allow_tools"], json!(["read_file"]));
        assert_eq!(payload["edge_executor_id"], "edge-1");
        assert_eq!(payload["workspace_binding"]["kind"], "edge_workspace");
        assert_eq!(payload["workspace_binding"]["authority"], "read_write");
        assert_eq!(payload["executor_binding"]["kind"], "edge_agent");
        assert_eq!(payload["executor_binding"]["transport"], "edge_ws");
        assert_eq!(
            payload["session_id"],
            Value::String(offering_session_placeholder().to_string())
        );
    }

    #[test]
    fn edge_validation_requires_one_typed_execution_across_measured_streams() {
        let events = vec![
            json!({
                "type": "tool_routing_decision",
                "call_id": "call-1",
                "tool": "read_file",
                "route": "edge_bound",
            }),
            json!({
                "type": "tool_transport_started",
                "call_id": "call-1",
                "tool": "read_file",
                "arguments": {"path": "marker.json"},
                "transport": "edge_ws",
                "executor": {"kind": "edge_agent", "executor_id": "edge-1", "transport": "edge_ws"},
            }),
            json!({
                "type": "tool_transport_completed",
                "call_id": "call-1",
                "tool": "read_file",
                "success": true,
                "transport": "edge_ledger",
                "executor": {"kind": "edge_agent", "executor_id": "edge-1", "transport": "edge_ledger"},
            }),
            json!({
                "type": "tool_call_end",
                "call_id": "call-1",
                "tool": "read_file",
                "success": true,
                "transport": "edge_ledger",
                "executor": {"kind": "edge_agent", "executor_id": "edge-1", "transport": "edge_ledger"},
            }),
        ];

        assert!(validate_edge_execution_stream(&events, "edge-1", "marker.json").unwrap());
        assert!(!validate_edge_execution_stream(&[], "edge-1", "marker.json").unwrap());
        validate_edge_scenario_execution([&[], &events], "edge-1", "marker.json").unwrap();
        assert!(validate_edge_scenario_execution([&[], &[]], "edge-1", "marker.json").is_err());

        let mut wrong_executor = events;
        wrong_executor[2]["executor"]["executor_id"] = Value::String("edge-2".to_string());
        assert!(validate_edge_execution_stream(&wrong_executor, "edge-1", "marker.json").is_err());

        let mut non_durable_delivery = wrong_executor;
        non_durable_delivery[2]["executor"]["executor_id"] = Value::String("edge-1".to_string());
        non_durable_delivery[2]["transport"] = Value::String("edge_ws".to_string());
        assert!(
            validate_edge_execution_stream(&non_durable_delivery, "edge-1", "marker.json").is_err()
        );
    }

    #[test]
    fn cli_stream_json_preserves_every_completed_exchange() {
        let mut records = cli_exchange_records("exchange-1", 1, 0);
        records.extend(cli_exchange_records("exchange-2", 2, 1));
        let mut result = cli_stream_record("result");
        result.insert("result".to_string(), json!({"success": true}));
        records.push(Value::Object(result));
        let stdout = serialize_cli_records(&records);

        let capture = parse_cli_stream_json(stdout.as_bytes(), "session-1", "fixture").unwrap();
        assert_eq!(capture.events.len(), 2);
        assert!(matches!(
            capture.authority,
            StreamAuthority::CliSessionBridge {
                exchange_count: 2,
                ..
            }
        ));

        records[1]["event_seq"] = json!(2);
        let malformed = serialize_cli_records(&records);
        assert!(parse_cli_stream_json(malformed.as_bytes(), "session-1", "fixture").is_err());
    }

    #[test]
    fn cli_stream_json_rejects_non_exact_identity_and_event_type_boundaries() {
        let mut records = cli_exchange_records("exchange-1", 1, 0);
        let mut result = cli_stream_record("result");
        result.insert("result".to_string(), json!({"success": true}));
        records.push(Value::Object(result));

        let mut padded_identity = records.clone();
        for record in &mut padded_identity {
            record["execution_id"] = json!(" run-execution");
            record["turn_chain_id"] = json!(" run-execution");
        }
        padded_identity[1]["event"]["run_id"] = json!(" run-execution");
        padded_identity[1]["event"]["turn_chain_id"] = json!(" run-execution");
        padded_identity[2]["server_run_id"] = json!(" run-execution");
        let padded = serialize_cli_records(&padded_identity);
        assert!(parse_cli_stream_json(padded.as_bytes(), "session-1", "fixture").is_err());

        let mut control_type = records;
        let mut control_event = control_type[1].clone();
        control_event["event_seq"] = json!(2);
        control_event["event"] = json!({"type": "usage\n"});
        control_type.insert(2, control_event);
        control_type[3]["event_count"] = json!(2);
        let controlled = serialize_cli_records(&control_type);
        assert!(parse_cli_stream_json(controlled.as_bytes(), "session-1", "fixture").is_err());
    }
}
