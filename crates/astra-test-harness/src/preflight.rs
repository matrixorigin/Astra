//! Pre-flight checks before running any test cases.
//!
//! Validates that the astra binary exists, the server is healthy,
//! and auth + model connectivity works. Fails fast with actionable
//! error messages so users don't waste time on doomed runs.

use std::path::Path;
use std::time::Duration;

use thiserror::Error;
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
    })
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
    dotenvy::dotenv().ok();
    // Model probes intentionally run from a disposable directory. Resolve the
    // executable before changing CWD so a caller-provided `./target/debug/astra`
    // remains executable during health, registration, retry, and cleanup.
    let astra_bin = canonical_binary_path(astra_bin)?;
    let readiness = check_server(&astra_bin).await?;
    if require_memoria {
        check_memoria_readiness(&readiness).await?;
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

async fn check_server(astra_bin: &Path) -> Result<ServerReadiness, PreflightError> {
    let output = Command::new(astra_bin)
        .args(["health"])
        .env("NO_PROXY", "localhost,127.0.0.1")
        .env("no_proxy", "localhost,127.0.0.1")
        .output()
        .await
        .map_err(|e| PreflightError::ServerUnreachable {
            detail: format!("failed to spawn: {e}"),
        })?;

    let readiness = validate_health_probe(&output.stdout, output.status.code())
        .map_err(|detail| PreflightError::ServerUnready { detail })?;
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
        command
            .env("NO_PROXY", "localhost,127.0.0.1")
            .env("no_proxy", "localhost,127.0.0.1")
            .current_dir(probe_workspace)
            .output(),
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

    if stderr_indicates_cli_auth_failure(&stderr) {
        // Try auto-login in an isolated profile and retry. The CLI owns its
        // credential store; the harness must never parse tokens and write that
        // file through a second implementation.
        let auto_profile = profile.unwrap_or("harness-auto");
        eprintln!(
            "[astra-test] preflight: auth failed, attempting auto-register in profile `{auto_profile}`..."
        );
        match try_auto_register(astra_bin, auto_profile, probe_workspace).await {
            Ok(()) => {
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
                    retry_command
                        .env("NO_PROXY", "localhost,127.0.0.1")
                        .env("no_proxy", "localhost,127.0.0.1")
                        .current_dir(probe_workspace)
                        .output(),
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
        .env("NO_PROXY", "localhost,127.0.0.1")
        .env("no_proxy", "localhost,127.0.0.1")
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
        .env("NO_PROXY", "localhost,127.0.0.1")
        .env("no_proxy", "localhost,127.0.0.1")
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
        let result = check_server(Path::new("/nonexistent/astra")).await;
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
                    "  printf '%s\\n' '{{\"status\":\"cancelled\"}}'\n",
                    "  exit 0\n",
                    "fi\n",
                    "printf '%s\\n' '{{\"trace_id\":null,\"request_id\":null,\"run_id\":\"run-1\",\"session_id\":\"550e8400-e29b-41d4-a716-446655440000\",\"text\":\"pong\",\"final_state\":\"completed\",\"interruption_kind\":null,\"tool_result_class_counts\":{{}},\"prompt_tokens\":0,\"fresh_prompt_tokens\":0,\"cache\":{{\"hit\":false,\"read_tokens\":0,\"creation_tokens\":0}},\"completion_tokens\":0,\"llm_rounds\":0,\"tool_calls_count\":0,\"tools_used\":[],\"persistence_error\":null,\"exit_code\":0,\"success\":true,\"error_kind\":null}}'\n",
                    "exit 0\n",
                ),
                log.display()
            ),
        )
        .unwrap();

        let profile = check_model(&bin, "deepseek", Some("isolated-harness"), dir.path())
            .await
            .unwrap();
        assert_eq!(profile.as_deref(), Some("isolated-harness"));
        let args = fs::read_to_string(log).unwrap();
        assert!(args.contains("--profile\nisolated-harness\n"), "{args}");
        assert!(args.contains("--no-resume\n"), "{args}");
        assert!(
            args.contains("session\ncancel\n550e8400-e29b-41d4-a716-446655440000\n"),
            "successful model probes must cancel their exact server session: {args}"
        );
    }

    #[test]
    fn detects_cli_auth_failure_from_stderr() {
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
