//! Shell operations: bash execution, grep, glob.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::ToolResult;

/// Execute a bash command with timeout in a workspace directory.
pub async fn execute_bash(workspace_root: &Path, args: &Value) -> ToolResult {
    let command = match args.get("command").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return ToolResult::error("Error: Missing 'command' parameter".into()),
    };
    let timeout_secs = args
        .get("timeout")
        .and_then(|v| v.as_f64())
        .unwrap_or(30.0)
        .min(120.0);

    let timeout = Duration::from_secs_f64(timeout_secs);
    let output = tokio::time::timeout(timeout, async {
        tokio::process::Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(workspace_root)
            .output()
            .await
    })
    .await;

    match output {
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let mut result = String::new();
            if !stdout.is_empty() {
                result.push_str(&stdout);
            }
            if !stderr.is_empty() {
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str("stderr:\n");
                result.push_str(&stderr);
            }
            if !out.status.success() {
                result.push_str(&format!(
                    "\n(exit code: {})",
                    out.status.code().unwrap_or(-1)
                ));
            }
            if result.is_empty() {
                ToolResult::text("(command completed with no output)".into())
            } else {
                ToolResult::text(result)
            }
        }
        Ok(Err(e)) => ToolResult::error(format!("Error: Failed to execute command: {e}")),
        Err(_) => ToolResult::error(format!("Error: Command timed out after {timeout_secs}s")),
    }
}

/// Run grep with safe file type filtering.
pub fn grep(workspace_root: &Path, args: &Value) -> ToolResult {
    let pattern = match args.get("pattern").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ToolResult::error("Error: Missing 'pattern' parameter".into()),
    };
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let resolved = match crate::fs_ops::resolve_path(workspace_root, path) {
        Ok(p) => p,
        Err(e) => return ToolResult::error(e),
    };

    let mut cmd = std::process::Command::new("grep");
    cmd.arg("-rn")
        .arg("--include=*.rs")
        .arg("--include=*.ts")
        .arg("--include=*.tsx")
        .arg("--include=*.js")
        .arg("--include=*.jsx")
        .arg("--include=*.py")
        .arg("--include=*.go")
        .arg("--include=*.java")
        .arg("--include=*.toml")
        .arg("--include=*.json")
        .arg("--include=*.yaml")
        .arg("--include=*.yml")
        .arg("--include=*.md")
        .arg(pattern)
        .arg(&resolved)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    match cmd.output() {
        Ok(out) => {
            let result = String::from_utf8_lossy(&out.stdout).to_string();
            if result.is_empty() {
                ToolResult::text(format!("No matches found for pattern: {pattern}"))
            } else {
                ToolResult::text(result)
            }
        }
        Err(e) => ToolResult::error(format!("Error: grep failed: {e}")),
    }
}

/// Run find-based glob matching.
pub fn glob(workspace_root: &Path, args: &Value) -> ToolResult {
    let pattern = match args.get("pattern").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ToolResult::error("Error: Missing 'pattern' parameter".into()),
    };

    let mut cmd = std::process::Command::new("find");
    cmd.arg(workspace_root)
        .arg("-name")
        .arg(pattern)
        .arg("-type")
        .arg("f")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    match cmd.output() {
        Ok(out) => {
            let result = String::from_utf8_lossy(&out.stdout).to_string();
            if result.is_empty() {
                ToolResult::text(format!("No files found matching pattern: {pattern}"))
            } else {
                ToolResult::text(result)
            }
        }
        Err(e) => ToolResult::error(format!("Error: glob failed: {e}")),
    }
}
