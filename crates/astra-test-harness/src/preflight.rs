//! Pre-flight checks before running any test cases.
//!
//! Validates that the astra binary exists, the server is healthy,
//! and auth + model connectivity works. Fails fast with actionable
//! error messages so users don't waste time on doomed runs.

use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::runner::parse_strict_cli_outcome;
use crate::session_identity::delete_server_session;

/// Errors surfaced by pre-flight checks.
#[derive(Debug, Error)]
pub enum PreflightError {
    #[error("astra binary not found at path")]
    BinaryNotFound,
    #[error("astra binary exists but is not executable")]
    BinaryNotExecutable,
    #[error("build identity verification failed: {detail}")]
    BuildIdentity { detail: String },
    #[error("server unreachable: {detail}")]
    ServerUnreachable { detail: String },
    #[error("server is reachable but not ready: {detail}")]
    ServerUnready { detail: String },
    #[error("authentication failed: {detail}")]
    AuthFailed { detail: String },
    #[error("model `{model}` unavailable: {detail}")]
    ModelUnavailable { model: String, detail: String },
    #[error("could not create an isolated preflight workspace: {detail}")]
    ProbeWorkspaceUnavailable { detail: String },
}

const OWNER_READINESS_PROBE_USER: &str = "astra-owner-readiness-probe";
const OWNER_READINESS_PROBE_QUERY: &str = "__astra_owner_auth_readiness_probe__";

fn stderr_indicates_cli_auth_failure(stderr: &str) -> bool {
    stderr.contains("Could not validate credentials")
        || stderr.contains("Unable to obtain a valid access token")
        || stderr.contains("Session expired")
        || stderr.contains("try /login")
        || stderr.contains("401 Unauthorized")
        || stderr.contains("status 401")
}

fn stderr_indicates_model_inactive(stderr: &str, model: &str) -> bool {
    stderr.contains(&format!("Model '{model}' is inactive"))
        || stderr.contains("is inactive (connectivity failed or disabled)")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerReadiness {
    degraded: bool,
    unavailable_components: Vec<String>,
    interaction_api_major: String,
    build_git_sha: String,
    build_git_dirty: Option<bool>,
}

fn parse_server_readiness(stdout: &[u8]) -> Result<ServerReadiness, String> {
    if stdout.len() > 65536 {
        return Err("health response exceeds its bounded contract".to_string());
    }
    let value: serde_json::Value = serde_json::from_slice(stdout)
        .map_err(|error| format!("health response is not valid JSON: {error}"))?;
    let status = value
        .get("status")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("health response omitted status: {value}"))?;
    if !matches!(status, "healthy" | "degraded") {
        return Err(format!("server status is {status}: {value}"));
    }
    if value.get("database").and_then(serde_json::Value::as_str) != Some("connected") {
        return Err(format!("server database is not connected: {value}"));
    }
    let interaction_api_major = value
        .get("interaction_api_major")
        .and_then(serde_json::Value::as_str)
        .filter(|major| !major.trim().is_empty())
        .ok_or_else(|| format!("health response omitted interaction_api_major: {value}"))?;
    if interaction_api_major != astra_server_types::AGENT_INTERACTION_API_MAJOR {
        return Err(format!(
            "unsupported interaction_api_major={interaction_api_major}; expected {}: {value}",
            astra_server_types::AGENT_INTERACTION_API_MAJOR,
        ));
    }
    let build_git_sha = value
        .get("build_git_sha")
        .and_then(serde_json::Value::as_str)
        .filter(|sha| sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| format!("health response omitted a valid build_git_sha: {value}"))?;
    let unavailable_components = value
        .as_object()
        .into_iter()
        .flat_map(|object| object.iter())
        .filter_map(|(name, state)| (state.as_str() == Some("unavailable")).then_some(name.clone()))
        .collect();
    Ok(ServerReadiness {
        degraded: status == "degraded",
        unavailable_components,
        interaction_api_major: interaction_api_major.to_string(),
        build_git_sha: build_git_sha.to_string(),
        build_git_dirty: value
            .get("build_git_dirty")
            .and_then(serde_json::Value::as_bool),
    })
}

fn validate_build_identity(
    component: &str,
    expected: &str,
    actual: Option<&str>,
    dirty: Option<bool>,
) -> Result<(), PreflightError> {
    let invalid = |detail| PreflightError::BuildIdentity { detail };
    if expected.len() != 40 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid(
            "ASTRA_EXPECTED_BUILD_GIT_SHA must be a full commit SHA".into(),
        ));
    }
    if actual != Some(expected) || dirty != Some(false) {
        return Err(invalid(format!(
            "{component}: expected clean build {expected}, got revision {actual:?}, dirty {dirty:?}"
        )));
    }
    Ok(())
}

async fn check_client_build(astra_bin: &Path, expected: &str) -> Result<(), PreflightError> {
    let invalid = |detail| PreflightError::BuildIdentity { detail };
    let (status, stdout, stderr) =
        capture_readiness_probe(astra_bin, "--build-info-json", None, 4096)
            .await
            .map_err(invalid)?;
    if !status.success() {
        return Err(invalid(format!(
            "CLI build identity probe exited {status}: {stderr}"
        )));
    }
    let identity: serde_json::Value = serde_json::from_slice(&stdout)
        .map_err(|error| invalid(format!("CLI build identity is invalid JSON: {error}")))?;
    if identity.get("schema").and_then(serde_json::Value::as_str)
        != Some(astra_core::build_info::BUILD_INFO_SCHEMA)
    {
        return Err(invalid("CLI build identity schema is unsupported".into()));
    }
    for field in ["target", "profile"] {
        if identity
            .get(field)
            .and_then(serde_json::Value::as_str)
            .is_none_or(|value| value.trim().is_empty() || value == "unknown")
        {
            return Err(invalid(format!("CLI build identity omitted valid {field}")));
        }
    }
    validate_build_identity(
        "CLI",
        expected,
        identity.get("git_sha").and_then(serde_json::Value::as_str),
        identity
            .get("git_dirty")
            .and_then(serde_json::Value::as_bool),
    )
}

/// Health and artifact probes must finish before any paid model work. Bound
/// their retained output while reading, and kill the child when a deadline wins.
async fn capture_readiness_probe(
    astra_bin: &Path,
    argument: &str,
    profile: Option<&str>,
    max_bytes: usize,
) -> Result<(ExitStatus, Vec<u8>, String), String> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut child = astra_command(astra_bin, profile)
            .arg(argument)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("{argument} probe spawn failed: {error}"))?;
        let mut stdout_reader = child
            .stdout
            .take()
            .expect("piped probe stdout")
            .take(max_bytes as u64 + 1);
        let mut stderr_reader = child.stderr.take().expect("piped probe stderr").take(4097);
        let read_stdout = async {
            let mut bytes = Vec::new();
            stdout_reader
                .read_to_end(&mut bytes)
                .await
                .map_err(|error| format!("{argument} stdout read failed: {error}"))?;
            if bytes.len() > max_bytes {
                return Err(format!("{argument} stdout exceeds {max_bytes} bytes"));
            }
            Ok(bytes)
        };
        let read_stderr = async {
            let mut bytes = Vec::new();
            stderr_reader
                .read_to_end(&mut bytes)
                .await
                .map_err(|error| format!("{argument} stderr read failed: {error}"))?;
            if bytes.len() > 4096 {
                return Err(format!("{argument} stderr exceeds 4096 bytes"));
            }
            Ok(bytes)
        };
        let (stdout, stderr) = tokio::try_join!(read_stdout, read_stderr)?;
        let stderr = safe_probe_diagnostic(&String::from_utf8_lossy(&stderr));
        let status = child
            .wait()
            .await
            .map_err(|error| format!("{argument} probe wait failed: {error}"))?;
        Ok((status, stdout, stderr))
    })
    .await
    .map_err(|_| format!("{argument} probe timed out"))?
}

fn safe_probe_diagnostic(stderr: &str) -> String {
    let (redacted, _) = astra_turn_core::safety_middleware::redact_credentials_in_text(stderr);
    // The general tool-output redactor intentionally uses length thresholds.
    // Readiness errors need no secret-bearing line, even for a short password.
    static SENSITIVE_LINE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let sensitive = SENSITIVE_LINE.get_or_init(|| {
        regex::Regex::new(r"(?i)password|passwd|secret|token|api.?key|auth|bearer")
            .expect("probe diagnostic credential labels")
    });
    redacted
        .lines()
        .map(|line| {
            if sensitive.is_match(line) {
                "[credential-bearing diagnostic omitted]"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn validate_health_probe(stdout: &[u8], exit_code: Option<i32>) -> Result<ServerReadiness, String> {
    // `astra health` uses ApiError (3) for any non-healthy body. Only a
    // validated optional degradation is usable by this smoke harness.
    if !matches!(exit_code, Some(0 | 3)) {
        return Err(format!("health process failed: {exit_code:?}"));
    }
    let readiness = parse_server_readiness(stdout)?;
    if exit_code == Some(3) && !readiness.degraded {
        return Err("health process exit disagrees with healthy response".to_string());
    }
    Ok(readiness)
}

/// Validate the complete terminal evidence emitted by a successful model
/// probe.  A process that exits successfully is not sufficient evidence: the
/// typed envelope must agree with the process status and must itself describe
/// a successful run.  Keeping this at the preflight boundary prevents both
/// the initial probe and the auto-register retry from accepting a typed
/// failure envelope printed by a process that exits 0.
fn validate_successful_model_probe(
    stdout: &[u8],
    model: &str,
    process_exit: i32,
) -> Result<(), String> {
    let outcome = parse_strict_cli_outcome(&String::from_utf8_lossy(stdout), model)?;
    if outcome.exit_code != process_exit {
        return Err(format!(
            "terminal evidence exit_code {} disagrees with process exit {}",
            outcome.exit_code, process_exit
        ));
    }
    if process_exit != 0 {
        return Err(format!(
            "model probe reported failure (exit_code={})",
            outcome.exit_code
        ));
    }
    Ok(())
}

/// Revision verification binds the suite to the preflight routing context.
/// Case-local overrides are not visible to that probe (including follow-ups,
/// which inherit these fields), so reject them before any live work.
pub fn validate_revision_bound_cases(cases: &[crate::case::Case]) -> Result<(), PreflightError> {
    for case in cases {
        for key in case.cli_env.keys() {
            if matches!(
                key.to_ascii_uppercase().as_str(),
                "ASTRA_API_URL"
                    | "ASTRA_CONFIG_SOURCE"
                    | "ASTRA_PROFILE"
                    | "ASTRA_CLI_CREDENTIALS_DIR"
                    | "ASTRA_LOCAL_STATE_ROOT"
                    | "ASTRA_ACCESS_TOKEN"
                    | "MOI_AUTH_DIR"
                    | "HOME"
                    | "USERPROFILE"
                    | "HOMEDRIVE"
                    | "HOMEPATH"
                    | "XDG_CONFIG_HOME"
                    | "XDG_STATE_HOME"
                    | "APPDATA"
                    | "LOCALAPPDATA"
                    | "HTTP_PROXY"
                    | "HTTPS_PROXY"
                    | "ALL_PROXY"
                    | "NO_PROXY"
            ) {
                return Err(PreflightError::BuildIdentity {
                    detail: format!(
                        "case {:?}: cli_env key {key} can change the verified execution target; configure routing before preflight instead",
                        case.name
                    ),
                });
            }
        }
        for argument in &case.extra_cli_args {
            let flag = argument.split('=').next().unwrap_or(argument);
            if matches!(flag, "--api-url" | "--profile") {
                return Err(PreflightError::BuildIdentity {
                    detail: format!(
                        "case {:?}: extra_cli_args flag {flag} can change the verified execution target; configure routing before preflight instead",
                        case.name
                    ),
                });
            }
        }
    }
    Ok(())
}

/// Run all pre-flight checks in order. Validates binary, server, and
/// every model in the matrix (not just the first).
pub async fn run_preflight(
    astra_bin: &Path,
    models: &[String],
    requested_profile: Option<&str>,
    require_memoria: bool,
) -> Result<Option<String>, PreflightError> {
    // The server and local dependency scripts both load `.env`; do the same
    // for the harness so an owner-auth probe cannot be skipped merely because
    // the caller did not export the local development variables in its shell.
    if !astra_core::config::explicit_env_config_requested().map_err(|error| {
        PreflightError::BuildIdentity {
            detail: error.to_string(),
        }
    })? {
        dotenvy::dotenv().ok();
    }
    let expected_build = std::env::var("ASTRA_EXPECTED_BUILD_GIT_SHA")
        .map(Some)
        .or_else(|error| match error {
            std::env::VarError::NotPresent => Ok(None),
            _ => Err(PreflightError::BuildIdentity {
                detail: "ASTRA_EXPECTED_BUILD_GIT_SHA must be valid UTF-8".into(),
            }),
        })?;
    // Execution later uses an explicit legacy profile for owner-scoped
    // capture. Bind that same route before health, rather than probing a
    // native MOI login and only switching profiles after paid model work.
    let credentials = astra_credentials::CredentialStore::new()
        .load()
        .map_err(|error| PreflightError::AuthFailed {
            detail: error.to_string(),
        })?;
    let effective_profile = astra_credentials::CredentialStore::resolve_profile_name(
        requested_profile,
        credentials.current_profile.as_deref(),
    );
    run_preflight_with_build(
        astra_bin,
        models,
        Some(&effective_profile),
        requested_profile.unwrap_or("harness-auto"),
        require_memoria,
        expected_build.as_deref(),
        astra_core::build_info::current(),
    )
    .await
}

async fn run_preflight_with_build(
    astra_bin: &Path,
    models: &[String],
    requested_profile: Option<&str>,
    auto_profile: &str,
    require_memoria: bool,
    expected_build: Option<&str>,
    harness: astra_core::build_info::BuildInfo,
) -> Result<Option<String>, PreflightError> {
    // Probe workspaces change CWD; resolve the executable once beforehand.
    let astra_bin = canonical_binary_path(astra_bin)?;
    let expected_build = expected_build.map(|value| value.trim().to_ascii_lowercase());
    if let Some(expected) = expected_build.as_deref() {
        validate_build_identity(
            "harness",
            expected,
            Some(harness.git_sha),
            Some(harness.git_dirty),
        )?;
        check_client_build(&astra_bin, expected).await?;
    }
    check_execution_server(
        &astra_bin,
        requested_profile,
        expected_build.as_deref(),
        require_memoria,
    )
    .await?;
    if expected_build.is_none() {
        eprintln!(
            "[astra-test] preflight: deployment smoke check; build revision equality not verified"
        );
    }
    // A model probe is a real `astra chat` invocation. Running it from the
    // caller's checkout can therefore acquire that checkout's physical
    // workspace claim, even with `--no-resume`. This made a healthy harness
    // fail merely because an interactive TUI happened to be open in the same
    // directory. Probe from an empty, disposable directory so readiness is
    // about the server/model contract and never mutates or contends with a
    // user's workspace.
    let probe_workspace =
        tempfile::tempdir().map_err(|error| PreflightError::ProbeWorkspaceUnavailable {
            detail: error.to_string(),
        })?;
    let mut effective_profile = requested_profile.map(str::to_string);
    for model in models {
        effective_profile = check_model(
            &astra_bin,
            model,
            effective_profile.as_deref(),
            probe_workspace.path(),
            auto_profile,
            expected_build.as_deref(),
            require_memoria,
        )
        .await?;
    }
    Ok(effective_profile)
}

fn check_binary(astra_bin: &Path) -> Result<(), PreflightError> {
    if !astra_bin.exists() {
        return Err(PreflightError::BinaryNotFound);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(astra_bin).map_err(|_| PreflightError::BinaryNotFound)?;
        if meta.permissions().mode() & 0o111 == 0 {
            return Err(PreflightError::BinaryNotExecutable);
        }
    }
    Ok(())
}

fn canonical_binary_path(astra_bin: &Path) -> Result<std::path::PathBuf, PreflightError> {
    let canonical = std::fs::canonicalize(astra_bin).map_err(|_| PreflightError::BinaryNotFound)?;
    check_binary(&canonical)?;
    Ok(canonical)
}

async fn check_execution_server(
    astra_bin: &Path,
    profile: Option<&str>,
    expected_build: Option<&str>,
    require_memoria: bool,
) -> Result<(), PreflightError> {
    let readiness = check_server(astra_bin, profile).await?;
    if let Some(expected) = expected_build {
        validate_build_identity(
            "Server",
            expected,
            Some(&readiness.build_git_sha),
            readiness.build_git_dirty,
        )?;
        eprintln!("[astra-test] preflight: execution Server matches clean build {expected}");
    }
    if require_memoria {
        check_memoria_readiness(&readiness).await?;
    }
    Ok(())
}

async fn check_server(
    astra_bin: &Path,
    profile: Option<&str>,
) -> Result<ServerReadiness, PreflightError> {
    let (status, stdout, stderr) = capture_readiness_probe(astra_bin, "health", profile, 65536)
        .await
        .map_err(|detail| PreflightError::ServerUnreachable { detail })?;

    let readiness = validate_health_probe(&stdout, status.code()).map_err(|detail| {
        PreflightError::ServerUnready {
            detail: format!("{detail}; stderr: {stderr}"),
        }
    })?;
    eprintln!(
        "[astra-test] preflight: Server contract={} build={}",
        readiness.interaction_api_major,
        &readiness.build_git_sha[..12],
    );
    if readiness.degraded {
        eprintln!(
            "[astra-test] preflight: core Server is ready; degraded components: {}",
            if readiness.unavailable_components.is_empty() {
                "unspecified".to_string()
            } else {
                readiness.unavailable_components.join(", ")
            }
        );
    }
    Ok(readiness)
}

/// Validate the storage/authentication contract used by self-hosted memory.
///
/// The ordinary Memoria health endpoint authenticates as an administrator
/// (`Bearer`) and therefore cannot detect an older backend that accepts health
/// checks but rejects Astra's required `Memoria-Owner` requests.  Probe a
/// nonexistent query through the exact owner-scoped endpoint instead. The
/// request does not execute an explicit memory write, is bounded, and the
/// master key is never included in an error string.
async fn check_memoria_readiness(server_readiness: &ServerReadiness) -> Result<(), PreflightError> {
    if server_readiness
        .unavailable_components
        .iter()
        .any(|component| component == "memoria")
    {
        return Err(PreflightError::ServerUnready {
            detail: "Memoria is unavailable according to the server health response; \
                     fix the dependency before running memory-dependent cases"
                .to_string(),
        });
    }

    // Hosted/browser-login deployments use a scoped user credential and must
    // not be forced through the local master-key fallback. The explicit local
    // flag is the contract that enables this probe.
    if std::env::var("MEMORIA_SELF_HOSTED_MASTER_ACCESS").as_deref() != Ok("1")
        || std::env::var("MEMORIA_WEB_URL")
            .ok()
            .is_some_and(|url| !url.trim().is_empty())
    {
        return Ok(());
    }

    let master_key = std::env::var("MEMORIA_MASTER_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| PreflightError::ServerUnready {
            detail: "MEMORIA_MASTER_KEY is required for self-hosted Memoria-Owner readiness"
                .to_string(),
        })?;
    let base_url = std::env::var("MEMORIA_BASE_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| {
            format!(
                "http://127.0.0.1:{}",
                std::env::var("MEMORIA_PORT").unwrap_or_else(|_| "8100".to_string())
            )
        });
    probe_memoria_owner(&base_url, &master_key)
        .await
        .map_err(|detail| PreflightError::ServerUnready { detail })?;
    eprintln!("[astra-test] preflight: Memoria owner-authenticated storage is ready");
    Ok(())
}

fn validate_memoria_retrieve_body(body: &str) -> Result<(), String> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|error| format!("Memoria owner-auth readiness returned invalid JSON: {error}"))?;
    let valid = match &value {
        serde_json::Value::Array(_) => true,
        serde_json::Value::Object(object) => {
            !object.contains_key("error")
                && ["memories", "results"]
                    .iter()
                    .any(|key| object.get(*key).is_some_and(serde_json::Value::is_array))
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err("Memoria owner-auth readiness returned a non-retrieve response".to_string())
    }
}

async fn probe_memoria_owner(base_url: &str, master_key: &str) -> Result<(), String> {
    const MAX_RESPONSE_BYTES: usize = 64 * 1024;
    let url = format!("{}/v1/memories/retrieve", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|error| format!("failed to build Memoria readiness client: {error}"))?;
    let response = client
        .post(url)
        .header("Authorization", format!("Memoria-Owner {master_key}"))
        .header("X-User-Id", OWNER_READINESS_PROBE_USER)
        .json(&serde_json::json!({
            "query": OWNER_READINESS_PROBE_QUERY,
            "top_k": 1,
        }))
        .send()
        .await
        .map_err(|error| format!("Memoria owner-auth readiness request failed: {error}"))?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(
            "Memoria owner authentication returned HTTP 401; use a Memoria 0.5.2+ image with matching MEMORIA_MASTER_KEY"
                .to_string(),
        );
    }
    if status == reqwest::StatusCode::FORBIDDEN {
        return Err(
            "Memoria owner authentication returned HTTP 403; check MEMORIA_SELF_HOSTED_MASTER_ACCESS=1"
                .to_string(),
        );
    }
    if !status.is_success() {
        return Err(format!(
            "Memoria owner-auth readiness returned HTTP {status}"
        ));
    }
    use futures::StreamExt;
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            format!("Memoria owner-auth readiness response could not be read: {error}")
        })?;
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err("Memoria owner-auth readiness response exceeds 64 KiB".to_string());
        }
        body.extend_from_slice(&chunk);
    }
    validate_memoria_retrieve_body(std::str::from_utf8(&body).map_err(|error| {
        format!("Memoria owner-auth readiness returned non-UTF-8 JSON: {error}")
    })?)?;
    Ok(())
}

fn astra_command(astra_bin: &Path, profile: Option<&str>) -> Command {
    let mut command = Command::new(astra_bin);
    if let Some(profile) = profile {
        command.arg("--profile").arg(profile);
    }
    command
}

async fn release_model_probe_session(
    astra_bin: &Path,
    profile: Option<&str>,
    session_id: Option<&str>,
) -> Result<(), String> {
    let Some(session_id) = session_id else {
        return Ok(());
    };
    delete_server_session(astra_bin, profile, session_id).await
}

async fn check_model(
    astra_bin: &Path,
    model: &str,
    profile: Option<&str>,
    probe_workspace: &Path,
    auto_profile: &str,
    expected_build: Option<&str>,
    require_memoria: bool,
) -> Result<Option<String>, PreflightError> {
    let mut command = astra_command(astra_bin, profile);
    command.args([
        "chat",
        "-m",
        "ping",
        "--no-resume",
        "--model",
        model,
        "--json",
        "-y",
    ]);
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        command.current_dir(probe_workspace).output(),
    )
    .await;

    let output = match result {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Err(PreflightError::ModelUnavailable {
                model: model.to_string(),
                detail: format!("spawn failed: {e}"),
            });
        }
        Err(_) => {
            return Err(PreflightError::ModelUnavailable {
                model: model.to_string(),
                detail: "timed out after 30s".to_string(),
            });
        }
    };

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr_indicates_model_inactive(&stderr, model) {
        return Err(PreflightError::ModelUnavailable {
            model: model.to_string(),
            detail: stderr.trim().to_string(),
        });
    }

    if !output.status.success() && stderr_indicates_cli_auth_failure(&stderr) {
        // Try auto-login in an isolated profile and retry. The CLI owns its
        // credential store; the harness must never parse tokens and write that
        // file through a second implementation.
        if profile != Some(auto_profile) {
            // Switching auth mode can switch Server. Validate before even
            // registering an account at the new target.
            check_execution_server(
                astra_bin,
                Some(auto_profile),
                expected_build,
                require_memoria,
            )
            .await?;
        }
        eprintln!(
            "[astra-test] preflight: auth failed, attempting auto-register in profile `{auto_profile}`..."
        );
        match try_auto_register(astra_bin, auto_profile, probe_workspace).await {
            Ok(()) => {
                check_execution_server(
                    astra_bin,
                    Some(auto_profile),
                    expected_build,
                    require_memoria,
                )
                .await?;
                // Retry the model check after registration.
                let mut retry_command = astra_command(astra_bin, Some(auto_profile));
                retry_command.args([
                    "chat",
                    "-m",
                    "ping",
                    "--no-resume",
                    "--model",
                    model,
                    "--json",
                    "-y",
                ]);
                let retry = tokio::time::timeout(
                    Duration::from_secs(30),
                    retry_command.current_dir(probe_workspace).output(),
                )
                .await;
                match retry {
                    Ok(Ok(o)) => {
                        if o.status.success() {
                            match validate_successful_model_probe(
                                &o.stdout,
                                model,
                                o.status.code().unwrap_or(-1),
                            ) {
                                Ok(_) => {
                                    let outcome = parse_strict_cli_outcome(
                                        &String::from_utf8_lossy(&o.stdout),
                                        model,
                                    )
                                    .expect("successful model probe was already validated");
                                    if let Err(error) = release_model_probe_session(
                                        astra_bin,
                                        Some(auto_profile),
                                        outcome.session_id.as_deref(),
                                    )
                                    .await
                                    {
                                        return Err(PreflightError::ModelUnavailable {
                                            model: model.to_string(),
                                            detail: format!(
                                                "model probe session cleanup failed: {error}"
                                            ),
                                        });
                                    }
                                    eprintln!(
                                        "[astra-test] preflight: profile `{auto_profile}` authenticated, model `{model}` OK"
                                    );
                                    return Ok(Some(auto_profile.to_string()));
                                }
                                Err(error) => {
                                    return Err(PreflightError::ModelUnavailable {
                                        model: model.to_string(),
                                        detail: format!(
                                            "profile `{auto_profile}` authenticated, but model probe returned invalid terminal evidence: {error}"
                                        ),
                                    });
                                }
                            }
                        }
                        let retry_stderr = String::from_utf8_lossy(&o.stderr);
                        return Err(PreflightError::ModelUnavailable {
                            model: model.to_string(),
                            detail: format!(
                                "profile `{auto_profile}` authenticated, but model probe exited {}: {}",
                                o.status.code().unwrap_or(-1),
                                retry_stderr.trim()
                            ),
                        });
                    }
                    Ok(Err(error)) => {
                        return Err(PreflightError::ModelUnavailable {
                            model: model.to_string(),
                            detail: format!("retry spawn failed: {error}"),
                        });
                    }
                    Err(_) => {
                        return Err(PreflightError::ModelUnavailable {
                            model: model.to_string(),
                            detail: "retry timed out after 30s".to_string(),
                        });
                    }
                }
            }
            Err(detail) => {
                return Err(PreflightError::AuthFailed {
                    detail: format!(
                        "profile `{auto_profile}` is invalid and isolated auto-register/login failed: {detail}. If this database already has an administrator, log in with `astra --profile {auto_profile} admin login`"
                    ),
                });
            }
        }
    }

    if !output.status.success() {
        return Err(PreflightError::ModelUnavailable {
            model: model.to_string(),
            detail: format!(
                "exit {}: {}",
                output.status.code().unwrap_or(-1),
                stderr.trim()
            ),
        });
    }

    if let Err(error) =
        validate_successful_model_probe(&output.stdout, model, output.status.code().unwrap_or(-1))
    {
        return Err(PreflightError::ModelUnavailable {
            model: model.to_string(),
            detail: format!("model probe returned invalid terminal evidence: {error}"),
        });
    }

    let outcome = parse_strict_cli_outcome(&String::from_utf8_lossy(&output.stdout), model)
        .expect("successful model probe was already validated");
    if let Err(error) =
        release_model_probe_session(astra_bin, profile, outcome.session_id.as_deref()).await
    {
        return Err(PreflightError::ModelUnavailable {
            model: model.to_string(),
            detail: format!("model probe session cleanup failed: {error}"),
        });
    }

    eprintln!("[astra-test] preflight: model `{model}` responded OK");
    Ok(profile.map(str::to_string))
}

/// Try to register a test user via `astra admin` and login via astra CLI.
/// The CLI is the only owner of credential persistence; a successful login
/// means the requested profile is ready for every subsequent subprocess.
async fn try_auto_register(
    astra_bin: &Path,
    profile: &str,
    probe_workspace: &Path,
) -> Result<(), String> {
    if !astra_bin.exists() {
        return Err("astra binary disappeared before registration".to_string());
    }

    // Register (may fail if user already exists — that's fine).
    let mut register_command = astra_command(astra_bin, Some(profile));
    let register_out = register_command
        .args([
            "admin",
            "register",
            "--username",
            "harness-auto",
            "--password",
            "harness-auto-pw",
        ])
        .current_dir(probe_workspace)
        .output()
        .await;

    // Login to get fresh tokens.
    let mut login_command = astra_command(astra_bin, Some(profile));
    let login_out = login_command
        .args([
            "admin",
            "login",
            "--username",
            "harness-auto",
            "--password",
            "harness-auto-pw",
        ])
        .current_dir(probe_workspace)
        .output()
        .await;

    match login_out {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            let register_detail = match register_out {
                Ok(registered) => format!(
                    "exit {} ({})",
                    registered.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&registered.stderr).trim()
                ),
                Err(error) => format!("spawn failed: {error}"),
            };
            let login_detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Err(format!(
                "register={register_detail}; login={} ({login_detail})",
                output.status.code().unwrap_or(-1)
            ))
        }
        Err(error) => Err(format!("failed to spawn login: {error}")),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[tokio::test]
    async fn public_preflight_resolves_profile_before_health_without_reusing_it_for_auto_auth() {
        const CHILD: &str = "ASTRA_TEST_PREFLIGHT_PROFILE_CHILD";
        if let Ok(directory) = std::env::var(CHILD) {
            let requested = std::env::var("ASTRA_TEST_REQUESTED_PROFILE").ok();
            let error = super::run_preflight(
                &std::path::Path::new(&directory).join("astra"),
                &["test-model".into()],
                requested.as_deref(),
                false,
            )
            .await
            .unwrap_err();
            assert!(
                matches!(error, super::PreflightError::AuthFailed { .. }),
                "{error}"
            );
            return;
        }
        for (requested, environment, current, expected) in [
            (
                Some("requested"),
                Some("environment"),
                Some("current"),
                "requested",
            ),
            (None, Some("environment"), Some("current"), "environment"),
            (None, None, Some("current"), "current"),
            (None, None, None, "default"),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let calls = directory.path().join("calls");
            std::fs::write(
                directory.path().join("credentials.json"),
                serde_json::json!({"current_profile": current, "profiles": {}}).to_string(),
            )
            .unwrap();
            crate::test_support::write_executable_shim(&directory.path().join("astra"), format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{calls}'
[ "$1" = --profile ] || exit 98
shift 2
case "$1" in
health) printf '%s' '{{"status":"healthy","database":"connected","interaction_api_major":"3","build_git_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","build_git_dirty":false}}' ;;
chat) printf '401 Unauthorized' >&2; exit 3 ;;
admin) exit 1 ;;
*) exit 99 ;;
esac
"#, calls=calls.display(),
            )).unwrap();
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.args(["--exact", "preflight::tests::public_preflight_resolves_profile_before_health_without_reusing_it_for_auto_auth", "--test-threads=1"])
                .env(CHILD, directory.path())
                .env("ASTRA_CLI_CREDENTIALS_DIR", directory.path())
                .env("ASTRA_CONFIG_SOURCE", "explicit-env")
                .env_remove("ASTRA_EXPECTED_BUILD_GIT_SHA")
                .env_remove("ASTRA_TEST_REQUESTED_PROFILE")
                .env_remove("ASTRA_PROFILE");
            if let Some(value) = requested {
                command.env("ASTRA_TEST_REQUESTED_PROFILE", value);
            }
            if let Some(value) = environment {
                command.env("ASTRA_PROFILE", value);
            }
            let output = command.output().unwrap();
            assert!(output.status.success(), "{output:?}");
            let log = std::fs::read_to_string(calls).unwrap();
            assert!(
                log.starts_with(&format!(
                    "--profile {expected} health\n--profile {expected} chat "
                )),
                "{log}"
            );
            let auto = requested.unwrap_or("harness-auto");
            assert!(
                log.contains(&format!("--profile {auto} admin register ")),
                "{log}"
            );
        }
    }

    #[test]
    fn revision_bound_cases_reject_routing_overrides_without_disclosing_values() {
        for key in [
            "ASTRA_API_URL",
            "ASTRA_CONFIG_SOURCE",
            "ASTRA_PROFILE",
            "ASTRA_CLI_CREDENTIALS_DIR",
            "ASTRA_LOCAL_STATE_ROOT",
            "ASTRA_ACCESS_TOKEN",
            "MOI_AUTH_DIR",
            "HOME",
            "USERPROFILE",
            "HOMEDRIVE",
            "HOMEPATH",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "APPDATA",
            "LOCALAPPDATA",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
        ] {
            for value in ["", "private-endpoint-or-credential"] {
                let mut case =
                    serde_yaml_ng::from_str::<crate::case::Case>("name: routing\nprompt: hello\n")
                        .unwrap();
                case.cli_env.insert(key.into(), value.into());
                let error = super::validate_revision_bound_cases(&[case])
                    .unwrap_err()
                    .to_string();
                assert!(error.contains(key), "{error}");
                assert!(!error.contains("private-endpoint-or-credential"), "{error}");
            }
        }
        for flag in ["--api-url", "--profile"] {
            for args in [
                vec![flag.to_string(), "private-endpoint-or-credential".into()],
                vec![format!("{flag}=private-endpoint-or-credential")],
            ] {
                let mut case =
                    serde_yaml_ng::from_str::<crate::case::Case>("name: routing\nprompt: hello\n")
                        .unwrap();
                case.extra_cli_args = args;
                let error = super::validate_revision_bound_cases(&[case])
                    .unwrap_err()
                    .to_string();
                assert!(error.contains(flag), "{error}");
                assert!(!error.contains("private-endpoint-or-credential"), "{error}");
            }
        }
        let case = serde_yaml_ng::from_str::<crate::case::Case>(
            "name: tracing\nprompt: hello\ncli_env: {ASTRA_TRACE: verbose, NO_COLOR: '1'}\nextra_cli_args: [--explain]\nsteps:\n  - prompt: continue\n",
        ).unwrap();
        super::validate_revision_bound_cases(&[case]).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn revision_probe_checks_explicit_execution_profile_not_native_endpoint() {
        const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("astra");
        let calls = dir.path().join("calls");
        let identity = serde_json::json!({
            "schema": astra_core::build_info::BUILD_INFO_SCHEMA,
            "git_sha": SHA, "git_dirty": false, "target": "test", "profile": "test"
        });
        for execution_sha in ["bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", SHA] {
            // No explicit profile models native MOI routing to A; local
            // selects the independent execution endpoint (B in the first run).
            crate::test_support::write_executable_shim(&bin, format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{calls}'
if [ "$1" = --build-info-json ]; then
  [ "$#" -eq 1 ] || exit 98
  printf '%s' '{identity}'
  exit 0
fi
revision='{SHA}'
if [ "$1" = --profile ]; then
  [ "$2" = local ] || exit 97
  revision='{execution_sha}'
  shift 2
fi
case "$1" in
health) printf '{{"status":"healthy","database":"connected","interaction_api_major":"3","build_git_sha":"%s","build_git_dirty":false}}' "$revision" ;;
chat) printf '{{}}' ;;
*) exit 99 ;;
esac
"#, calls=calls.display(),
            )).unwrap();
            let error = super::run_preflight_with_build(
                &bin,
                &["test-model".into()],
                Some("local"),
                "local",
                false,
                Some(SHA),
                astra_core::build_info::BuildInfo {
                    git_sha: SHA,
                    git_dirty: false,
                    ..astra_core::build_info::current()
                },
            )
            .await
            .unwrap_err();
            let log = std::fs::read_to_string(&calls).unwrap();
            assert!(
                log.starts_with("--build-info-json\n--profile local health\n"),
                "{log}"
            );
            if execution_sha != SHA {
                assert!(
                    matches!(error, super::PreflightError::BuildIdentity { .. }),
                    "{error}"
                );
                assert!(
                    !log.contains("chat"),
                    "mismatch must prevent paid work: {log}"
                );
            } else {
                // Invalid model evidence is intentional: reaching chat on the
                // verified profile proves routing without live model work.
                assert!(
                    matches!(error, super::PreflightError::ModelUnavailable { .. }),
                    "{error}"
                );
                assert!(log.contains("--profile local chat "), "{log}");
            }
            std::fs::remove_file(&calls).unwrap();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_auto_registration_keeps_retry_cleanup_and_returned_profile_bound() {
        const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        const SESSION: &str = "550e8400-e29b-41d4-a716-446655440000";
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("astra");
        let calls = dir.path().join("calls");
        let registered = dir.path().join("registered");
        let outcome = serde_json::json!({
            "trace_id": null, "request_id": null, "run_id": "run-1", "session_id": SESSION,
            "text": "pong", "final_state": "completed", "interruption_kind": null,
            "tool_result_class_counts": {}, "prompt_tokens": 0, "fresh_prompt_tokens": 0,
            "cache": {"hit": false, "read_tokens": 0, "creation_tokens": 0},
            "completion_tokens": 0, "llm_rounds": 0, "tool_calls_count": 0, "tools_used": [],
            "persistence_error": null, "exit_code": 0, "success": true, "error_kind": null
        });
        crate::test_support::write_executable_shim(&bin, format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{calls}'
[ "$1" = --profile ] || exit 98
shift 2
case "$1" in
health) printf '%s' '{{"status":"healthy","database":"connected","interaction_api_major":"3","build_git_sha":"{SHA}","build_git_dirty":false}}' ;;
chat)
  if [ ! -f '{registered}' ]; then printf 'Error: Unable to obtain a valid access token; run astra login and retry.' >&2; exit 3; fi
  printf '%s' '{outcome}'
  ;;
admin) if [ "$2" = login ]; then touch '{registered}'; fi ;;
session) printf '%s' '{{"session_id":"{SESSION}","status":"cancelled","execution_settled":true}}' ;;
*) exit 99 ;;
esac
"#, calls=calls.display(), registered=registered.display(),
        )).unwrap();
        let profile = super::check_model(
            &bin,
            "test-model",
            Some("user-current"),
            dir.path(),
            "harness-auto",
            Some(SHA),
            false,
        )
        .await
        .unwrap();
        assert_eq!(profile.as_deref(), Some("harness-auto"));
        let log = std::fs::read_to_string(calls).unwrap();
        let lines: Vec<_> = log.lines().collect();
        assert!(
            lines[0].starts_with("--profile user-current chat "),
            "{log}"
        );
        assert!(
            lines[1..]
                .iter()
                .all(|line| line.starts_with("--profile harness-auto ")),
            "{log}"
        );
        assert_eq!(log.matches(" health").count(), 2, "{log}");
        assert_eq!(log.matches(" chat ").count(), 2, "{log}");
        assert!(log.contains(&format!("session cancel {SESSION}")), "{log}");
        assert!(log.contains(&format!("session delete {SESSION}")), "{log}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn auto_profile_is_verified_before_registration_and_before_model_retry() {
        const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("astra");
        let calls = dir.path().join("calls");
        let registered = dir.path().join("registered");
        for (fail_after_registration, missing_memoria) in
            [(false, false), (true, false), (true, true)]
        {
            crate::test_support::write_executable_shim(&bin, format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{calls}'
[ "$1" = --profile ] || exit 98
profile="$2"
shift 2
case "$1" in
chat) printf '401 Unauthorized' >&2; exit 3 ;;
health)
  revision='bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'
  status=healthy
  memoria=connected
  if [ '{fail_after_registration}' = true ] && [ ! -f '{registered}' ]; then revision='{SHA}'; fi
  if [ '{missing_memoria}' = true ]; then revision='{SHA}'; status=degraded; memoria=unavailable; fi
  printf '{{"status":"%s","database":"connected","memoria":"%s","interaction_api_major":"3","build_git_sha":"%s","build_git_dirty":false}}' "$status" "$memoria" "$revision"
  ;;
admin)
  [ "$profile" = harness-auto ] || exit 97
  if [ "$2" = login ]; then touch '{registered}'; fi
  ;;
*) exit 99 ;;
esac
"#, calls=calls.display(), registered=registered.display(),
            )).unwrap();
            let error = super::check_model(
                &bin,
                "test-model",
                Some(if missing_memoria {
                    "harness-auto"
                } else {
                    "user-current"
                }),
                dir.path(),
                "harness-auto",
                Some(SHA),
                missing_memoria,
            )
            .await
            .unwrap_err();
            if missing_memoria {
                assert!(
                    matches!(error, super::PreflightError::ServerUnready { .. }),
                    "{error}"
                );
            } else {
                assert!(
                    matches!(error, super::PreflightError::BuildIdentity { .. }),
                    "{error}"
                );
            }
            let log = std::fs::read_to_string(&calls).unwrap();
            assert_eq!(
                log.matches(" chat ").count(),
                1,
                "no retry on unverified target: {log}"
            );
            assert!(log.contains("--profile harness-auto health"), "{log}");
            assert_eq!(
                log.contains(" admin register "),
                fail_after_registration,
                "{log}"
            );
            assert_eq!(
                log.matches(" health").count(),
                if fail_after_registration && !missing_memoria {
                    2
                } else {
                    1
                },
                "{log}"
            );
            std::fs::remove_file(&calls).unwrap();
            if registered.exists() {
                std::fs::remove_file(&registered).unwrap();
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn identity_gate_precedes_health_model_and_auth_probes() {
        const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("astra");
        let calls = dir.path().join("calls");
        let identity = serde_json::json!({
            "schema": astra_core::build_info::BUILD_INFO_SCHEMA,
            "git_sha": SHA, "git_dirty": false, "target": "test", "profile": "test"
        })
        .to_string();
        let health = serde_json::json!({
            "status": "healthy", "database": "connected", "memoria": "connected",
            "interaction_api_major": "3", "build_git_sha": SHA, "build_git_dirty": true
        })
        .to_string();
        for (metadata, expected_calls) in [
            ("not-json", "--build-info-json\n"),
            (identity.as_str(), "--build-info-json\nhealth\n"),
        ] {
            crate::test_support::write_executable_shim(&bin, format!(
                "#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{}'\ncase \"$1\" in\n--build-info-json) printf '%s' '{}' ;;\nhealth) printf '%s' '{}' ;;\n*) exit 99 ;;\nesac\n",
                calls.display(), metadata, health,
            )).unwrap();
            let harness = astra_core::build_info::BuildInfo {
                git_sha: SHA,
                git_dirty: false,
                ..astra_core::build_info::current()
            };
            let error = super::run_preflight_with_build(
                &bin,
                &["test-model".into()],
                None,
                "harness-auto",
                false,
                Some(" AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n"),
                harness,
            )
            .await
            .unwrap_err();
            assert!(
                matches!(error, super::PreflightError::BuildIdentity { .. }),
                "{error}"
            );
            assert_eq!(std::fs::read_to_string(&calls).unwrap(), expected_calls);
            std::fs::remove_file(&calls).unwrap();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn readiness_probe_preserves_redacted_failure_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("astra");
        crate::test_support::write_executable_shim(
            &bin,
            "#!/bin/sh\nprintf 'configuration failed\\npassword = hunter2\\napi_key = test-secret-value-123456\\nauth = abc123\\nBearer xyz789\\n' >&2\nexit 2\n",
        )
        .unwrap();
        let error = super::check_server(&bin, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("configuration failed"), "{error}");
        assert!(!error.contains("test-secret-value-123456"), "{error}");
        assert!(!error.contains("hunter2"), "{error}");
        assert!(!error.contains("abc123"), "{error}");
        assert!(!error.contains("xyz789"), "{error}");
        let error = super::check_client_build(&bin, &"a".repeat(40))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("configuration failed"), "{error}");
        assert!(!error.contains("test-secret-value-123456"), "{error}");
        assert!(!error.contains("hunter2"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn readiness_probe_bounds_both_output_streams() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("astra");
        for (script, expected) in [
            (
                "#!/bin/sh\nwhile :; do printf '0123456789'; done\n",
                "stdout exceeds 16 bytes",
            ),
            (
                "#!/bin/sh\nwhile :; do printf '0123456789' >&2; done\n",
                "stderr exceeds 4096 bytes",
            ),
        ] {
            crate::test_support::write_executable_shim(&bin, script).unwrap();
            let error = super::capture_readiness_probe(&bin, "health", None, 16)
                .await
                .unwrap_err();
            assert!(error.contains(expected), "{error}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn readiness_probe_timeout_terminates_child_with_open_or_closed_pipes() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("astra");
        let pid_file = dir.path().join("pid");
        for output in ["", "printf '{}\\n'; exec 1>&- 2>&-"] {
            crate::test_support::write_executable_shim(
                &bin,
                format!(
                    "#!/bin/sh\nprintf '%s' $$ > '{}'\n{output}\nexec sleep 30\n",
                    pid_file.display()
                ),
            )
            .unwrap();
            let error = super::capture_readiness_probe(&bin, "health", None, 16)
                .await
                .unwrap_err();
            assert!(error.contains("timed out"), "{error}");
            let pid = std::fs::read_to_string(&pid_file).unwrap();
            assert!(pid.parse::<u32>().unwrap() > 1);
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    let alive = tokio::process::Command::new("kill")
                        .args(["-0", pid.as_str()])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                        .await
                        .unwrap()
                        .success();
                    if !alive {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("timed-out child must be killed and reaped");
        }
    }

    #[test]
    fn shared_server_readiness_contract() {
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/server_readiness.json")).unwrap();
        for case in cases.as_array().unwrap() {
            let body = serde_json::to_vec(&case["body"]).unwrap();
            let code = case["exit_code"].as_i64().unwrap() as i32;
            assert_eq!(
                super::validate_health_probe(&body, Some(code)).is_ok(),
                case["ready"].as_bool().unwrap(),
                "{}",
                case["name"]
            );
        }
        assert!(super::validate_health_probe(&vec![b' '; 65537], Some(0)).is_err());
    }

    #[test]
    fn canonical_binary_path_survives_a_later_cwd_change() {
        let current_dir = std::env::current_dir().unwrap();
        let dir = tempfile::tempdir_in(&current_dir).unwrap();
        let bin = dir.path().join("astra");
        std::fs::write(&bin, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&bin).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&bin, permissions).unwrap();
        }
        let relative = bin.strip_prefix(&current_dir).unwrap();
        let canonical = super::canonical_binary_path(relative).unwrap();
        assert!(canonical.is_absolute());
        let other_cwd = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new(relative)
                .current_dir(other_cwd.path())
                .output()
                .is_err()
        );
        let output = std::process::Command::new(&canonical)
            .current_dir(other_cwd.path())
            .output()
            .unwrap();
        assert!(output.status.success());
    }

    use super::*;

    async fn probe_with_response(
        status: &str,
        extra_headers: &str,
        body: &str,
    ) -> (Result<(), String>, String) {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_string();
        let extra_headers = extra_headers.to_string();
        let body = body.to_string();
        let captured_request = Arc::new(Mutex::new(Vec::new()));
        let captured_for_server = Arc::clone(&captured_request);
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            let _ = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let count = socket.read(&mut buffer).await.unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                    let header_end = request
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .map(|position| position + 4);
                    let Some(header_end) = header_end else {
                        continue;
                    };
                    let content_length = request[..header_end]
                        .split(|byte| *byte == b'\n')
                        .find_map(|line| {
                            let line = line.strip_suffix(b"\r").unwrap_or(line);
                            let colon = line.iter().position(|byte| *byte == b':')?;
                            let (name, value) = line.split_at(colon);
                            let value = &value[1..];
                            name.eq_ignore_ascii_case(b"content-length")
                                .then(|| {
                                    std::str::from_utf8(value)
                                        .ok()?
                                        .trim()
                                        .parse::<usize>()
                                        .ok()
                                })
                                .flatten()
                        })
                        .unwrap_or(0);
                    if request.len() >= header_end + content_length {
                        break;
                    }
                }
            })
            .await;
            *captured_for_server.lock().unwrap() = request;
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let result = probe_memoria_owner(&format!("http://{address}"), "probe-key").await;
        server.await.unwrap();
        let request = String::from_utf8_lossy(&captured_request.lock().unwrap()).into_owned();
        (result, request)
    }

    #[tokio::test]
    async fn owner_probe_accepts_only_retrieve_shapes() {
        let (result, request) = probe_with_response("200 OK", "", "[]").await;
        assert!(result.is_ok());
        let request_lower = request.to_ascii_lowercase();
        assert!(
            request.starts_with("POST /v1/memories/retrieve HTTP/1.1"),
            "{request}"
        );
        assert!(
            request_lower.contains("authorization: memoria-owner probe-key"),
            "{request}"
        );
        assert!(
            request_lower.contains("x-user-id: astra-owner-readiness-probe"),
            "{request}"
        );
        assert!(
            request.contains("__astra_owner_auth_readiness_probe__"),
            "{request}"
        );
        assert!(request.contains("\"top_k\":1"), "{request}");

        let (result, _) = probe_with_response("200 OK", "", r#"{"memories":[]}"#).await;
        assert!(result.is_ok());
        let (result, _) = probe_with_response("200 OK", "", r#"{"results":[]}"#).await;
        assert!(result.is_ok());
        let (result, _) = probe_with_response("200 OK", "", r#"{"error":"unavailable"}"#).await;
        assert!(result.is_err());
        let (result, _) = probe_with_response("200 OK", "", r#"{"status":"healthy"}"#).await;
        assert!(result.is_err());
        let (result, _) = probe_with_response("200 OK", "", "not-json").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn owner_probe_rejects_redirects_without_following_them() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use tokio::io::AsyncWriteExt;

        let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_address = target_listener.local_addr().unwrap();
        let target_visited = Arc::new(AtomicBool::new(false));
        let target_visited_by_server = Arc::clone(&target_visited);
        let target_server = tokio::spawn(async move {
            if let Ok((mut socket, _)) = target_listener.accept().await {
                target_visited_by_server.store(true, Ordering::SeqCst);
                let response =
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]";
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        let (result, _) = probe_with_response(
            "302 Found",
            &format!("Location: http://{target_address}/v1/memories/retrieve\r\n"),
            "[]",
        )
        .await;
        let error = result.expect_err("redirect must not certify storage readiness");
        assert!(error.contains("HTTP 302"), "{error}");
        assert!(!target_visited.load(Ordering::SeqCst));
        target_server.abort();
    }

    #[tokio::test]
    async fn owner_probe_explains_auth_failures_without_exposing_key() {
        let (result, _) =
            probe_with_response("401 Unauthorized", "", "secret backend detail").await;
        let error = result.expect_err("401 must fail readiness");
        assert!(error.contains("HTTP 401"), "{error}");
        assert!(!error.contains("probe-key"), "{error}");
        let (result, _) = probe_with_response("403 Forbidden", "", "secret backend detail").await;
        let error = result.expect_err("403 must fail readiness");
        assert!(error.contains("HTTP 403"), "{error}");
        assert!(!error.contains("probe-key"), "{error}");
    }

    #[tokio::test]
    async fn owner_probe_rejects_oversized_response() {
        let body = format!("[{}]", " ".repeat(64 * 1024));
        let (result, _) = probe_with_response("200 OK", "", &body).await;
        let error = result.expect_err("oversized response must fail readiness");
        assert!(error.contains("exceeds 64 KiB"), "{error}");
    }

    #[test]
    fn binary_not_found() {
        let result = check_binary(Path::new("/nonexistent/astra"));
        assert!(matches!(result, Err(PreflightError::BinaryNotFound)));
    }

    #[tokio::test]
    async fn server_unreachable_on_bad_binary() {
        let result = check_server(Path::new("/nonexistent/astra"), None).await;
        assert!(matches!(
            result,
            Err(PreflightError::ServerUnreachable { .. })
        ));
    }

    #[tokio::test]
    async fn model_check_spawn_failure() {
        let probe_workspace = tempfile::tempdir().unwrap();
        let result = check_model(
            Path::new("/nonexistent/astra"),
            "gpt-4",
            None,
            probe_workspace.path(),
            "harness-auto",
            None,
            false,
        )
        .await;
        assert!(matches!(
            result,
            Err(PreflightError::ModelUnavailable { .. })
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn model_probe_uses_requested_profile_without_resuming_user_session() {
        use crate::test_support::write_executable_shim;
        use std::fs;

        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("args.log");
        let bin = dir.path().join("astra-shim");
        write_executable_shim(
            &bin,
            format!(
                concat!(
                    "#!/bin/sh\n",
                    "printf '%s\\n' \"$@\" >> '{}'\n",
                    "if [ \"$3\" = session ] && [ \"$4\" = cancel ] && [ \"$5\" = 550e8400-e29b-41d4-a716-446655440000 ]; then\n",
                    "  printf '%s\\n' '{{\"session_id\":\"550e8400-e29b-41d4-a716-446655440000\",\"status\":\"cancelled\",\"execution_settled\":true}}'\n",
                    "  exit 0\n",
                    "fi\n",
                    "printf '%s\\n' 'Earlier log: Unable to obtain a valid access token' >&2\n",
                    "printf '%s\\n' '{{\"trace_id\":null,\"request_id\":null,\"run_id\":\"run-1\",\"session_id\":\"550e8400-e29b-41d4-a716-446655440000\",\"text\":\"pong\",\"final_state\":\"completed\",\"interruption_kind\":null,\"tool_result_class_counts\":{{}},\"prompt_tokens\":0,\"fresh_prompt_tokens\":0,\"cache\":{{\"hit\":false,\"read_tokens\":0,\"creation_tokens\":0}},\"completion_tokens\":0,\"llm_rounds\":0,\"tool_calls_count\":0,\"tools_used\":[],\"persistence_error\":null,\"exit_code\":0,\"success\":true,\"error_kind\":null}}'\n",
                    "exit 0\n",
                ),
                log.display()
            ),
        )
        .unwrap();

        let profile = check_model(
            &bin,
            "deepseek",
            Some("isolated-harness"),
            dir.path(),
            "isolated-harness",
            None,
            false,
        )
        .await
        .unwrap();
        assert_eq!(profile.as_deref(), Some("isolated-harness"));
        let args = fs::read_to_string(log).unwrap();
        assert!(args.contains("--profile\nisolated-harness\n"), "{args}");
        assert!(args.contains("--no-resume\n"), "{args}");
        assert!(!args.contains("admin\nregister\n"), "{args}");
        assert!(
            args.contains("session\ncancel\n550e8400-e29b-41d4-a716-446655440000\n"),
            "successful model probes must cancel their exact server session: {args}"
        );
    }

    #[test]
    fn detects_cli_auth_failure_from_stderr() {
        assert!(stderr_indicates_cli_auth_failure(
            "Error: Unable to obtain a valid access token; run `astra login` and retry."
        ));
        assert!(stderr_indicates_cli_auth_failure(
            "Error: Could not validate credentials"
        ));
        assert!(stderr_indicates_cli_auth_failure(
            "API Error (401): Could not validate credentials\n  Hint: Session expired — try /login"
        ));
        assert!(!stderr_indicates_cli_auth_failure(
            "Model 'foo' is inactive (connectivity failed or disabled)"
        ));
    }

    #[test]
    fn detects_model_inactive_from_stderr() {
        assert!(stderr_indicates_model_inactive(
            "Error: Model 'foo' is inactive (connectivity failed or disabled)",
            "foo"
        ));
        assert!(!stderr_indicates_model_inactive(
            "Error: Could not validate credentials",
            "foo"
        ));
    }

    #[test]
    fn explicit_http_unauthorized_is_auth_failure_not_model_unavailability() {
        assert!(stderr_indicates_cli_auth_failure(
            "server model registry request failed with status 401 Unauthorized"
        ));
        assert!(stderr_indicates_cli_auth_failure(
            "request failed with status 401"
        ));
        assert!(!stderr_indicates_cli_auth_failure(
            "request failed with status 403 Forbidden"
        ));
    }

    #[test]
    fn expected_build_requires_exact_revision_and_explicit_clean_evidence() {
        let expected = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert!(validate_build_identity("CLI", expected, Some(expected), Some(false)).is_ok());
        for (sha, dirty) in [
            (
                Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
                Some(false),
            ),
            (Some(expected), Some(true)),
            (Some(expected), None),
            (None, Some(false)),
        ] {
            assert!(validate_build_identity("CLI", expected, sha, dirty).is_err());
        }
        for invalid in [
            "",
            "abcdef",
            "HEAD",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaag",
        ] {
            assert!(
                validate_build_identity("harness", invalid, Some(invalid), Some(false)).is_err()
            );
        }
        for dirty in [
            serde_json::json!(false),
            serde_json::json!(true),
            serde_json::Value::Null,
        ] {
            let health = serde_json::json!({
                "status": "healthy", "database": "connected", "interaction_api_major": "3",
                "build_git_sha": expected, "build_git_dirty": dirty,
            });
            let readiness = parse_server_readiness(&serde_json::to_vec(&health).unwrap()).unwrap();
            assert_eq!(
                validate_build_identity(
                    "Server",
                    expected,
                    Some(&readiness.build_git_sha),
                    readiness.build_git_dirty
                )
                .is_ok(),
                dirty == false,
            );
        }
    }

    #[test]
    fn health_probe_distinguishes_core_readiness_from_optional_degradation() {
        let degraded_body = br#"{"status":"degraded","database":"connected","interaction_api_major":"3","build_git_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","memoria":"unavailable"}"#;
        assert!(validate_health_probe(degraded_body, Some(3)).is_ok());
        assert!(validate_health_probe(degraded_body, Some(1)).is_err());
        assert!(validate_health_probe(degraded_body, None).is_err());
        assert!(
            validate_health_probe(
                br#"{"status":"degraded","database":"unavailable"}"#,
                Some(3)
            )
            .is_err()
        );
        assert!(validate_health_probe(br#"{"status":"healthy","database":"connected","interaction_api_major":"3","build_git_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#, Some(3)).is_err());
        assert!(validate_health_probe(b"not JSON", Some(3)).is_err());
        let healthy = parse_server_readiness(
            br#"{"status":"healthy","database":"connected","interaction_api_major":"3","build_git_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","memoria":"available"}"#,
        )
        .unwrap();
        assert!(!healthy.degraded);
        assert!(healthy.unavailable_components.is_empty());
        assert_eq!(healthy.interaction_api_major, "3");
        assert_eq!(healthy.build_git_sha.len(), 40);

        let degraded = parse_server_readiness(
            br#"{"status":"degraded","database":"connected","interaction_api_major":"3","build_git_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","memoria":"unavailable"}"#,
        )
        .unwrap();
        assert!(degraded.degraded);
        assert_eq!(degraded.unavailable_components, ["memoria"]);

        assert!(
            parse_server_readiness(
                br#"{"status":"degraded","database":"unavailable","interaction_api_major":"3","build_git_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#
            )
            .is_err()
        );
        assert!(
            parse_server_readiness(
                br#"{"status":"unhealthy","database":"connected","interaction_api_major":"3","build_git_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#
            )
            .is_err()
        );
        let stale = parse_server_readiness(
                br#"{"status":"healthy","database":"connected","interaction_api_major":"2","build_git_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#,
        )
        .unwrap_err();
        assert!(stale.contains("expected 3"), "{stale}");
        assert!(
            parse_server_readiness(
                br#"{"status":"healthy","database":"connected","interaction_api_major":"3"}"#
            )
            .unwrap_err()
            .contains("build_git_sha")
        );
        assert!(parse_server_readiness(br#"healthy"#).is_err());
    }

    #[test]
    fn model_probe_rejects_typed_failure_when_process_exits_successfully() {
        let failure = br#"{"trace_id":null,"request_id":null,"run_id":"run-1","session_id":"550e8400-e29b-41d4-a716-446655440000","text":"","final_state":"interrupted","interruption_kind":"error","tool_result_class_counts":{},"prompt_tokens":0,"fresh_prompt_tokens":0,"cache":{"hit":false,"read_tokens":0,"creation_tokens":0},"completion_tokens":0,"llm_rounds":0,"tool_calls_count":0,"tools_used":[],"persistence_error":null,"exit_code":3,"success":false,"error_kind":"api_error"}"#;
        let error = validate_successful_model_probe(failure, "deepseek", 0).unwrap_err();
        assert!(error.contains("disagrees with process exit"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn binary_not_executable() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("astra");
        fs::write(&bin, "").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o644)).unwrap();

        let result = check_binary(&bin);
        assert!(matches!(result, Err(PreflightError::BinaryNotExecutable)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn auto_register_delegates_profile_persistence_to_cli() {
        use crate::test_support::write_executable_shim;
        use std::fs;

        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("args.log");
        let bin = dir.path().join("astra-shim");
        write_executable_shim(
            &bin,
            format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display()),
        )
        .unwrap();

        try_auto_register(&bin, "isolated-harness", dir.path())
            .await
            .unwrap();

        let calls = fs::read_to_string(log).unwrap();
        let lines: Vec<&str> = calls.lines().collect();
        assert_eq!(lines.len(), 2, "{calls}");
        assert!(
            lines[0].starts_with("--profile isolated-harness admin register"),
            "{calls}"
        );
        assert!(
            lines[1].starts_with("--profile isolated-harness admin login"),
            "{calls}"
        );
    }
}
