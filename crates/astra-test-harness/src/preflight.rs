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
use crate::session_identity::cancel_server_session;

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
}

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
) -> Result<Option<String>, PreflightError> {
    check_binary(astra_bin)?;
    check_server(astra_bin).await?;
    let mut effective_profile = requested_profile.map(str::to_string);
    for model in models {
        effective_profile = check_model(astra_bin, model, effective_profile.as_deref()).await?;
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

async fn check_server(astra_bin: &Path) -> Result<(), PreflightError> {
    let output = Command::new(astra_bin)
        .args(["health"])
        .env("NO_PROXY", "localhost,127.0.0.1")
        .env("no_proxy", "localhost,127.0.0.1")
        .output()
        .await
        .map_err(|e| PreflightError::ServerUnreachable {
            detail: format!("failed to spawn: {e}"),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(PreflightError::ServerUnreachable {
            detail: format!(
                "exit {}: {}",
                output.status.code().unwrap_or(-1),
                stderr.trim()
            ),
        });
    }

    let readiness = parse_server_readiness(&output.stdout)
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
    cancel_server_session(astra_bin, profile, session_id).await
}

async fn check_model(
    astra_bin: &Path,
    model: &str,
    profile: Option<&str>,
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
        match try_auto_register(astra_bin, auto_profile).await {
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
async fn try_auto_register(astra_bin: &Path, profile: &str) -> Result<(), String> {
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
    use super::*;

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
        let result = check_model(Path::new("/nonexistent/astra"), "gpt-4", None).await;
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

        let profile = check_model(&bin, "deepseek", Some("isolated-harness"))
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

        try_auto_register(&bin, "isolated-harness").await.unwrap();

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
