//! Pre-flight checks before running any test cases.
//!
//! Validates that the astra binary exists, the server is healthy,
//! and auth + model connectivity works. Fails fast with actionable
//! error messages so users don't waste time on doomed runs.

use std::path::Path;
use std::time::Duration;

use thiserror::Error;
use tokio::process::Command;

/// Errors surfaced by pre-flight checks.
#[derive(Debug, Error)]
pub enum PreflightError {
    #[error("astra binary not found at path")]
    BinaryNotFound,
    #[error("astra binary exists but is not executable")]
    BinaryNotExecutable,
    #[error("server unreachable: {detail}")]
    ServerUnreachable { detail: String },
    #[error("authentication failed: {detail}")]
    AuthFailed { detail: String },
    #[error("model `{model}` unavailable: {detail}")]
    ModelUnavailable { model: String, detail: String },
}

fn stderr_indicates_cli_auth_failure(stderr: &str) -> bool {
    stderr.contains("Could not validate credentials")
        || stderr.contains("Session expired")
        || stderr.contains("try /login")
}

fn stderr_indicates_model_inactive(stderr: &str, model: &str) -> bool {
    stderr.contains(&format!("Model '{model}' is inactive"))
        || stderr.contains("is inactive (connectivity failed or disabled)")
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

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.contains("\"status\"") || !stdout.contains("healthy") {
        return Err(PreflightError::ServerUnreachable {
            detail: format!("unexpected health response: {}", stdout.trim()),
        });
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
                    Ok(Ok(o)) if o.status.success() => {
                        eprintln!(
                            "[astra-test] preflight: profile `{auto_profile}` authenticated, model `{model}` OK"
                        );
                        return Ok(Some(auto_profile.to_string()));
                    }
                    Ok(Ok(o)) => {
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
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", log.display()),
        )
        .unwrap();

        let profile = check_model(&bin, "deepseek", Some("isolated-harness"))
            .await
            .unwrap();
        assert_eq!(profile.as_deref(), Some("isolated-harness"));
        let args = fs::read_to_string(log).unwrap();
        assert!(args.contains("--profile\nisolated-harness\n"), "{args}");
        assert!(args.contains("--no-resume\n"), "{args}");
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
