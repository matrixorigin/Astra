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

/// Run all pre-flight checks in order. Returns on first failure.
pub async fn run_preflight(astra_bin: &Path, models: &[String]) -> Result<(), PreflightError> {
    check_binary(astra_bin)?;
    check_server(astra_bin).await?;
    if let Some(model) = models.first() {
        check_model(astra_bin, model).await?;
    }
    Ok(())
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
            detail: format!("exit {}: {}", output.status.code().unwrap_or(-1), stderr.trim()),
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

async fn check_model(astra_bin: &Path, model: &str) -> Result<(), PreflightError> {
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new(astra_bin)
            .args(["chat", "-m", "ping", "--model", model, "--json", "-y"])
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
    if stderr.contains("Could not validate credentials") || output.status.code() == Some(3) {
        // Try auto-login: register a harness user and retry.
        eprintln!("[astra-test] preflight: auth failed, attempting auto-register...");
        if try_auto_register(astra_bin).await {
            // Retry the model check after registration.
            let retry = tokio::time::timeout(
                Duration::from_secs(30),
                Command::new(astra_bin)
                    .args(["chat", "-m", "ping", "--model", model, "--json", "-y"])
                    .env("NO_PROXY", "localhost,127.0.0.1")
                    .env("no_proxy", "localhost,127.0.0.1")
                    .output(),
            )
            .await;
            match retry {
                Ok(Ok(o)) if o.status.success() => {
                    eprintln!("[astra-test] preflight: auto-register succeeded, model `{model}` OK");
                    return Ok(());
                }
                _ => {}
            }
        }
        return Err(PreflightError::AuthFailed {
            detail: "credentials invalid and auto-register failed. Run: astra-admin register && astra-admin login".to_string(),
        });
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

    eprintln!(
        "[astra-test] preflight: model `{model}` responded OK"
    );
    Ok(())
}

/// Try to register a test user via astra-admin and login via astra CLI.
/// Returns true if credentials are now valid.
async fn try_auto_register(astra_bin: &Path) -> bool {
    // Locate astra-admin next to astra binary.
    let admin_bin = astra_bin.with_file_name("astra-admin");
    if !admin_bin.exists() {
        return false;
    }

    // Register (may fail if user already exists — that's fine).
    let _ = Command::new(&admin_bin)
        .args(["register", "--username", "harness-auto", "--password", "harness-auto-pw"])
        .env("NO_PROXY", "localhost,127.0.0.1")
        .env("no_proxy", "localhost,127.0.0.1")
        .output()
        .await;

    // Login to get fresh tokens.
    let login_out = Command::new(&admin_bin)
        .args(["login", "--username", "harness-auto", "--password", "harness-auto-pw"])
        .env("NO_PROXY", "localhost,127.0.0.1")
        .env("no_proxy", "localhost,127.0.0.1")
        .output()
        .await;

    match login_out {
        Ok(o) if o.status.success() => {
            // Parse tokens from login output and write to credentials.
            let stdout = String::from_utf8_lossy(&o.stdout);
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&stdout) {
                let access = v.get("access_token").and_then(|t| t.as_str());
                let refresh = v.get("refresh_token").and_then(|t| t.as_str());
                if let (Some(a), Some(r)) = (access, refresh) {
                    return write_credentials(a, r);
                }
            }
            false
        }
        _ => false,
    }
}

/// Write fresh credentials to ~/.astra/credentials.json.
/// Only writes to the default profile when no valid token exists,
/// to avoid clobbering an active user session.
fn write_credentials(access_token: &str, refresh_token: &str) -> bool {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return false,
    };
    let creds_path = std::path::Path::new(&home).join(".astra/credentials.json");

    // Load existing credentials.
    let mut creds: serde_json::Value = std::fs::read_to_string(&creds_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({"profiles": {}}));

    // Only write if the default profile has no access_token.
    // If user already has credentials, don't overwrite — they should re-login.
    let has_existing_token = creds
        .get("profiles")
        .and_then(|p| p.get("default"))
        .and_then(|d| d.get("access_token"))
        .and_then(|t| t.as_str())
        .is_some_and(|t| !t.is_empty());

    if has_existing_token {
        return false;
    }

    // No existing credentials — safe to write default profile.
    if let Some(profiles) = creds.get_mut("profiles").and_then(|p| p.as_object_mut()) {
        profiles.insert(
            "default".to_string(),
            serde_json::json!({
                "username": "harness-auto",
                "access_token": access_token,
                "refresh_token": refresh_token,
                "last_session_id": null,
                "memoria_api_key": null
            }),
        );
    }
    if creds.get("current_profile").is_none() {
        creds["current_profile"] = serde_json::json!("default");
    }

    std::fs::write(&creds_path, serde_json::to_string_pretty(&creds).unwrap_or_default()).is_ok()
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
        assert!(matches!(result, Err(PreflightError::ServerUnreachable { .. })));
    }

    #[tokio::test]
    async fn model_check_spawn_failure() {
        let result = check_model(Path::new("/nonexistent/astra"), "gpt-4").await;
        assert!(matches!(result, Err(PreflightError::ModelUnavailable { .. })));
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
}
