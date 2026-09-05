use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;

/// CLI-owned Runner process. `kill_on_drop` makes every early-return path
/// bounded; astra-edge retains its own durable inference journal before exit.
pub(crate) struct ManagedLocalRunner {
    child: tokio::process::Child,
    edge_id: String,
    diagnostics: Arc<Mutex<Vec<u8>>>,
}

impl ManagedLocalRunner {
    /// Detect immediate startup failures before the caller begins waiting for
    /// catalog publication. This is a liveness check, not a claim that the
    /// Server connection is ready; catalog publication remains the readiness
    /// boundary for model selection.
    pub(crate) async fn wait_until_alive(&mut self, timeout: Duration) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    let diagnostic = self
                        .diagnostics
                        .lock()
                        .ok()
                        .and_then(|bytes| String::from_utf8(bytes.clone()).ok())
                        .map(|text| text.trim().to_string())
                        .filter(|text| !text.is_empty())
                        .map(|text| format!("; runner log: {text}"))
                        .unwrap_or_default();
                    return Err(format!(
                        "User Runner exited during startup ({status}){diagnostic}"
                    ));
                }
                Ok(None) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Ok(());
                    }
                }
                Err(error) => return Err(format!("inspect User Runner startup: {error}")),
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub(crate) fn edge_id(&self) -> &str {
        &self.edge_id
    }
}

impl ManagedLocalRunner {
    pub(crate) async fn stop(mut self) {
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), self.child.wait()).await;
    }
}

fn runner_binary(current_exe: &Path) -> Result<PathBuf, String> {
    if let Some(explicit) = std::env::var_os("ASTRA_EDGE_BIN") {
        return Ok(PathBuf::from(explicit));
    }
    let filename = if cfg!(windows) {
        "astra-edge.exe"
    } else {
        "astra-edge"
    };
    current_exe
        .parent()
        .map(|parent| parent.join(filename))
        .ok_or_else(|| "cannot locate the Astra installation directory".to_string())
}

/// Start the inference-capable User Runner beside the CLI. The child resolves
/// environment credentials from this exact terminal and reads stored secrets
/// only through its owner-protected backend; neither reaches Server.
pub(crate) fn start(
    api_origin: &str,
    profile: Option<&str>,
    workspace: &Path,
) -> Result<ManagedLocalRunner, String> {
    let executable = runner_binary(
        &std::env::current_exe().map_err(|error| format!("locate Astra executable: {error}"))?,
    )?;
    if !executable.is_file() {
        return Err(format!(
            "User Runner executable is missing at {}; reinstall Astra or set ASTRA_EDGE_BIN",
            executable.display()
        ));
    }
    // A CLI invocation is one credential attachment. Give it an independent
    // Edge/Runner identity and journal so another terminal can use the same
    // environment-variable name with a different value without inheriting
    // this process's provider account.
    let edge_id = format!("edge-session-{}", uuid::Uuid::new_v4().simple());
    let mut command = tokio::process::Command::new(executable);
    command
        .arg("--server-url")
        .arg(api_origin)
        .arg("--workspace-dir")
        .arg(workspace)
        .arg("--edge-id")
        .arg(&edge_id)
        .arg("--reconnect=true")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(profile) = profile {
        command.arg("--profile").arg(profile);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("start User Runner: {error}"))?;
    let diagnostics = Arc::new(Mutex::new(Vec::new()));
    async fn capture_stream<R>(mut stream: R, diagnostics: Arc<Mutex<Vec<u8>>>)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        let mut buffer = [0_u8; 4096];
        loop {
            match stream.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if let Ok(mut target) = diagnostics.lock() {
                        target.extend_from_slice(&buffer[..read]);
                        if target.len() > 64 * 1024 {
                            let keep_from = target.len() - 64 * 1024;
                            target.drain(..keep_from);
                        }
                    }
                }
            }
        }
    }
    if let Some(stream) = child.stdout.take() {
        tokio::spawn(capture_stream(stream, Arc::clone(&diagnostics)));
    }
    if let Some(stream) = child.stderr.take() {
        let diagnostics = Arc::clone(&diagnostics);
        tokio::spawn(capture_stream(stream, diagnostics));
    }
    Ok(ManagedLocalRunner {
        child,
        edge_id,
        diagnostics,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibling_binary_path_is_platform_specific_and_not_shell_interpreted() {
        let path = runner_binary(Path::new("/opt/astra/bin/astra")).unwrap();
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(if cfg!(windows) {
                "astra-edge.exe"
            } else {
                "astra-edge"
            })
        );
    }
}
