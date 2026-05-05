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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::process::Command;

use crate::ToolExecutor;

// ─── Constants ─────────────────────────────────────────────────────────────

/// Tools allowed inside the sandbox (safe, read-heavy subset).
pub const ALLOWED_TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "list_dir",
    "grep",
    "bash",
    "web_fetch",
];

/// Maximum execution time for the script (default).
pub const SCRIPT_TIMEOUT: Duration = Duration::from_secs(300);

/// Maximum number of tool calls the script can make.
pub const MAX_TOOL_CALLS: usize = 50;

/// Maximum stdout size in bytes.
pub const MAX_STDOUT_BYTES: usize = 50_000;

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
    bash(command, timeout=None)
    list_dir(path=".")
    grep(pattern, path=None, include=None)
    web_fetch(url, format="markdown")
"""
import json
import socket
import os

_SOCKET_PATH = os.environ["ASTRA_RPC_SOCKET"]


def _rpc_call(tool_name, args):
    """Send an RPC request to the agent and return the result."""
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(_SOCKET_PATH)
    request = json.dumps({"tool": tool_name, "args": args})
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


def bash(command, timeout=None):
    """Execute a shell command."""
    args = {"command": command}
    if timeout is not None:
        args["timeout"] = timeout
    return _rpc_call("bash", args)


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

/// A JSON RPC request from the Python script.
#[derive(Debug, serde::Deserialize)]
pub struct RpcRequest {
    pub tool: String,
    pub args: Value,
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
) -> Result<(), CodeExecError> {
    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();

    // Read one JSON line
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
    let resp_bytes = serde_json::to_vec(&response)
        .map_err(|e| CodeExecError::Internal(e.to_string()))?;
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
    // Create temp directory for socket + stub + script
    let tmp_dir = tempfile::tempdir()?;
    let tmp_path = tmp_dir.path().to_path_buf();
    let socket_path = tmp_path.join("rpc.sock");
    let stub_path = tmp_path.join("astra_tools.py");
    let script_path = tmp_path.join("script.py");

    // Write Python stub
    std::fs::write(&stub_path, generate_python_stub())?;

    // Write the user's script
    std::fs::write(&script_path, script)?;

    // Create UDS listener
    let listener = UnixListener::bind(&socket_path)?;

    // Shared call counter
    let call_count = Arc::new(AtomicUsize::new(0));

    // Spawn Python subprocess
    let mut child = Command::new("python3")
        .arg(&script_path)
        .env("ASTRA_RPC_SOCKET", &socket_path)
        .env(
            "PYTHONPATH",
            format!(
                "{}:{}",
                tmp_path.display(),
                std::env::var("PYTHONPATH").unwrap_or_default()
            ),
        )
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    // Collect stdout in background (capped)
    let stdout = child.stdout.take().expect("stdout piped");
    let max_stdout = config.max_stdout_bytes;
    let stdout_handle = tokio::spawn(async move {
        collect_stdout(stdout, max_stdout).await
    });

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
                            )
                            .await;
                            if let Err(CodeExecError::TooManyToolCalls(_)) = result {
                                // Kill the child — it exceeded the call limit
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
            // Timeout — kill the child
            let _ = child.kill().await;
            Err(CodeExecError::Timeout(config.timeout))
        }
    }
}

/// Collect stdout from a reader, capping at max_bytes.
async fn collect_stdout(
    stdout: tokio::process::ChildStdout,
    max_bytes: usize,
) -> String {
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

    // ── Test: Python stub generation ──────────────────────────────────────

    #[test]
    fn test_generate_python_stub_is_valid() {
        let stub = generate_python_stub();
        // Must contain all required function definitions
        assert!(stub.contains("def read_file("));
        assert!(stub.contains("def write_file("));
        assert!(stub.contains("def bash("));
        assert!(stub.contains("def list_dir("));
        assert!(stub.contains("def grep("));
        assert!(stub.contains("def web_fetch("));
        // Must contain the RPC infrastructure
        assert!(stub.contains("def _rpc_call("));
        assert!(stub.contains("ASTRA_RPC_SOCKET"));
        assert!(stub.contains("socket.AF_UNIX"));
        assert!(stub.contains("json.dumps"));
        assert!(stub.contains("json.loads"));
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
        let json_str = r#"{"tool": "read_file", "args": {"path": "foo.txt"}}"#;
        let req: RpcRequest = serde_json::from_str(json_str).unwrap();
        assert_eq!(req.tool, "read_file");
        assert_eq!(req.args["path"], "foo.txt");
    }

    #[test]
    fn test_rpc_request_parsing_empty_args() {
        let json_str = r#"{"tool": "list_dir", "args": {}}"#;
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
        assert!(ALLOWED_TOOLS.contains(&"bash"));
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

        // Create a UDS pair for testing
        let tmp = tempfile::tempdir().unwrap();
        let sock_path = tmp.path().join("test.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        // First call (count becomes 50) — should succeed
        let client1 = tokio::net::UnixStream::connect(&sock_path).await.unwrap();
        let (stream1, _) = listener.accept().await.unwrap();

        // Write a valid request to client1
        let req = r#"{"tool": "read_file", "args": {"path": "test.txt"}}"#;
        use tokio::io::AsyncWriteExt;
        let (_, mut writer1) = client1.into_split();
        writer1.write_all(req.as_bytes()).await.unwrap();
        writer1.write_all(b"\n").await.unwrap();
        writer1.shutdown().await.unwrap();

        let result =
            handle_rpc_connection(stream1, &executor, &call_count, &config).await;
        assert!(result.is_ok());
        assert_eq!(call_count.load(Ordering::SeqCst), 50);

        // Second call (count becomes 51) — should fail
        let client2 = tokio::net::UnixStream::connect(&sock_path).await.unwrap();
        let (stream2, _) = listener.accept().await.unwrap();

        let (_, mut writer2) = client2.into_split();
        writer2.write_all(req.as_bytes()).await.unwrap();
        writer2.write_all(b"\n").await.unwrap();
        writer2.shutdown().await.unwrap();

        let result =
            handle_rpc_connection(stream2, &executor, &call_count, &config).await;
        assert!(matches!(result, Err(CodeExecError::TooManyToolCalls(50))));
    }

    // ── Test: Disallowed tool via RPC ─────────────────────────────────────

    #[tokio::test]
    async fn test_disallowed_tool_rejected() {
        let call_count = AtomicUsize::new(0);
        let config = CodeExecConfig::default();
        let executor = MockToolExecutor::new();

        let tmp = tempfile::tempdir().unwrap();
        let sock_path = tmp.path().join("test.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        let client = tokio::net::UnixStream::connect(&sock_path).await.unwrap();
        let (stream, _) = listener.accept().await.unwrap();

        // Try a disallowed tool
        let req = r#"{"tool": "git_commit", "args": {"message": "evil"}}"#;
        let (_, mut writer) = client.into_split();
        writer.write_all(req.as_bytes()).await.unwrap();
        writer.write_all(b"\n").await.unwrap();
        writer.shutdown().await.unwrap();

        let result =
            handle_rpc_connection(stream, &executor, &call_count, &config).await;
        // Should succeed (the handler writes an error response, doesn't return Err)
        assert!(result.is_ok());
        // Call count should NOT have incremented
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
        assert!(
            output.contains("Correctly rejected"),
            "got: {output}"
        );
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
