//! `astra-edge` — lightweight remote tool execution agent.
//!
//! Connects to an Astra server via WebSocket, authenticates, and executes
//! tool calls locally on the user's machine. Results are sent back over the
//! same WebSocket connection.
//!
//! ## Usage
//! ```bash
//! astra-edge --server-url wss://astra.example.com --token <jwt> --workspace-dir ~/projects/my-app
//! ```

use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Astra remote edge agent — execute tool calls locally for web sessions.
#[derive(Parser, Debug)]
#[command(name = "astra-edge", version, about)]
struct Args {
    /// WebSocket URL of the Astra server (e.g., wss://astra.example.com/edge/ws)
    #[arg(long, env = "ASTRA_SERVER_URL")]
    server_url: String,

    /// Authentication token (JWT)
    #[arg(long, env = "ASTRA_TOKEN")]
    token: String,

    /// Local workspace directory for file operations
    #[arg(long, env = "ASTRA_WORKSPACE_DIR", default_value = ".")]
    workspace_dir: PathBuf,

    /// Edge agent identifier
    #[arg(long, env = "ASTRA_EDGE_ID", default_value_t = default_edge_id())]
    edge_id: String,

    /// Auto-reconnect on disconnect
    #[arg(long, default_value_t = true)]
    reconnect: bool,
}

fn default_edge_id() -> String {
    let hostname = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".into());
    format!("edge-{hostname}-{}", &uuid::Uuid::new_v4().to_string()[..8])
}

// ─── Protocol types (mirroring server edge_ws_protocol.rs) ───────────────────

#[derive(Serialize)]
#[serde(tag = "type")]
enum EdgeToServer {
    #[serde(rename = "edge_auth")]
    Auth {
        token: String,
        edge_agent_id: String,
        hostname: Option<String>,
        workspace_dir: Option<String>,
    },
    #[serde(rename = "edge_tool_result")]
    ToolResult {
        request_id: String,
        output: String,
        is_error: bool,
        duration_ms: u64,
    },
    #[serde(rename = "edge_ping")]
    Ping,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ServerToEdge {
    #[serde(rename = "edge_auth_ok")]
    AuthOk { user_id: String },
    #[serde(rename = "edge_auth_error")]
    AuthError { message: String },
    #[serde(rename = "edge_tool_request")]
    ToolRequest {
        request_id: String,
        tool: String,
        args: serde_json::Value,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
    },
    #[serde(rename = "edge_pong")]
    Pong,
    #[serde(rename = "edge_closing")]
    Closing { reason: String },
}

fn default_timeout() -> u64 {
    300
}

// ─── Local tool executor ─────────────────────────────────────────────────────

struct LocalToolExecutor {
    workspace: PathBuf,
}

impl LocalToolExecutor {
    fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }

    fn resolve_path(&self, path_str: &str) -> Result<PathBuf, String> {
        let path = Path::new(path_str);
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace.join(path)
        };
        let canonical_ws = self
            .workspace
            .canonicalize()
            .unwrap_or_else(|_| self.workspace.clone());
        // Normalize by resolving .. components
        let mut normalized = PathBuf::new();
        for component in resolved.components() {
            match component {
                std::path::Component::ParentDir => {
                    normalized.pop();
                }
                _ => normalized.push(component.as_os_str()),
            }
        }
        if !normalized.starts_with(&canonical_ws) {
            return Err(format!("SANDBOX_DENIED: path escapes workspace: {path_str}"));
        }
        Ok(normalized)
    }

    async fn execute(&self, tool: &str, args: &serde_json::Value) -> (String, bool) {
        match tool {
            "read_file" => self.read_file(args),
            "write_file" => self.write_file(args),
            "str_replace" => self.str_replace(args),
            "delete_file" => self.delete_file(args),
            "list_dir" => self.list_dir(args),
            "bash" => self.bash(args).await,
            "grep" => self.grep(args),
            "glob" => self.glob_tool(args),
            "git_status" => self.git_cmd(&["status", "--porcelain"]),
            "git_diff" => self.git_diff(args),
            "git_log" => self.git_log(args),
            _ => (format!("Tool '{tool}' not available on this edge agent"), true),
        }
    }

    fn read_file(&self, args: &serde_json::Value) -> (String, bool) {
        let path_str = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ("Error: Missing 'path' parameter".into(), true),
        };
        let path = match self.resolve_path(path_str) {
            Ok(p) => p,
            Err(e) => return (e, true),
        };
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let start = args.get("start_line").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
                let end = args.get("end_line").and_then(|v| v.as_u64()).map(|v| v as usize);
                let lines: Vec<&str> = content.lines().collect();
                let start_idx = start.saturating_sub(1);
                let end_idx = end.unwrap_or(lines.len()).min(lines.len());
                let numbered: Vec<String> = lines[start_idx..end_idx]
                    .iter()
                    .enumerate()
                    .map(|(i, line)| format!("{}\t{line}", start_idx + i + 1))
                    .collect();
                (numbered.join("\n"), false)
            }
            Err(e) => (format!("Error: {e}"), true),
        }
    }

    fn write_file(&self, args: &serde_json::Value) -> (String, bool) {
        let path_str = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ("Error: Missing 'path' parameter".into(), true),
        };
        let content = match args.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return ("Error: Missing 'content' parameter".into(), true),
        };
        let path = match self.resolve_path(path_str) {
            Ok(p) => p,
            Err(e) => return (e, true),
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(&path, content) {
            Ok(_) => (
                format!("Successfully wrote {} bytes to {path_str}", content.len()),
                false,
            ),
            Err(e) => (format!("Error: {e}"), true),
        }
    }

    fn str_replace(&self, args: &serde_json::Value) -> (String, bool) {
        let path_str = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ("Error: Missing 'path' parameter".into(), true),
        };
        let old_str = match args.get("old_str").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return ("Error: Missing 'old_str' parameter".into(), true),
        };
        let new_str = args
            .get("new_str")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let path = match self.resolve_path(path_str) {
            Ok(p) => p,
            Err(e) => return (e, true),
        };
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return (format!("Error: {e}"), true),
        };
        let count = content.matches(old_str).count();
        if count == 0 {
            return (format!("Error: old_str not found in {path_str}"), true);
        }
        if count > 1 {
            return (
                format!("Error: old_str found {count} times in {path_str} — must be unique"),
                true,
            );
        }
        let new_content = content.replacen(old_str, new_str, 1);
        match std::fs::write(&path, new_content) {
            Ok(_) => (format!("Successfully replaced in {path_str}"), false),
            Err(e) => (format!("Error writing: {e}"), true),
        }
    }

    fn delete_file(&self, args: &serde_json::Value) -> (String, bool) {
        let path_str = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ("Error: Missing 'path' parameter".into(), true),
        };
        let path = match self.resolve_path(path_str) {
            Ok(p) => p,
            Err(e) => return (e, true),
        };
        if !path.exists() {
            return (format!("File not found: {path_str}"), true);
        }
        match std::fs::remove_file(&path) {
            Ok(_) => (format!("Successfully deleted {path_str}"), false),
            Err(e) => (format!("Error: {e}"), true),
        }
    }

    fn list_dir(&self, args: &serde_json::Value) -> (String, bool) {
        let path_str = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".");
        let path = match self.resolve_path(path_str) {
            Ok(p) => p,
            Err(e) => return (e, true),
        };
        match std::fs::read_dir(&path) {
            Ok(entries) => {
                let mut items: Vec<String> = entries
                    .filter_map(|e| e.ok())
                    .map(|e| {
                        let name = e.file_name().to_string_lossy().to_string();
                        if e.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                            format!("{name}/")
                        } else {
                            name
                        }
                    })
                    .collect();
                items.sort();
                (items.join("\n"), false)
            }
            Err(e) => (format!("Error: {e}"), true),
        }
    }

    async fn bash(&self, args: &serde_json::Value) -> (String, bool) {
        let command = match args.get("command").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return ("Error: Missing 'command' parameter".into(), true),
        };
        let timeout_secs = args
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(120);

        let result = tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            tokio::process::Command::new("bash")
                .arg("-c")
                .arg(command)
                .current_dir(&self.workspace)
                .output(),
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let code = output.status.code().unwrap_or(-1);
                if code == 0 {
                    if stderr.is_empty() {
                        (stdout, false)
                    } else {
                        (format!("{stdout}\nstderr:\n{stderr}"), false)
                    }
                } else {
                    let mut out = stdout;
                    if !stderr.is_empty() {
                        out.push_str(&format!("\nstderr:\n{stderr}"));
                    }
                    out.push_str(&format!("\n(exit code: {code})"));
                    (out, true)
                }
            }
            Ok(Err(e)) => (format!("Error: {e}"), true),
            Err(_) => ("Error: command timed out".into(), true),
        }
    }

    fn grep(&self, args: &serde_json::Value) -> (String, bool) {
        let pattern = match args.get("pattern").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ("Error: Missing 'pattern' parameter".into(), true),
        };
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".");
        let resolved = match self.resolve_path(path) {
            Ok(p) => p,
            Err(e) => return (e, true),
        };

        let output = std::process::Command::new("rg")
            .args(["--no-heading", "--line-number", "--max-count", "100", pattern])
            .current_dir(&resolved)
            .output();

        match output {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout).to_string();
                if stdout.trim().is_empty() {
                    ("No matches found".into(), false)
                } else {
                    (stdout, false)
                }
            }
            Err(_) => {
                // Fallback to grep if rg not available
                let output = std::process::Command::new("grep")
                    .args(["-rn", "--max-count=100", pattern, "."])
                    .current_dir(&resolved)
                    .output();
                match output {
                    Ok(o) => {
                        let stdout = String::from_utf8_lossy(&o.stdout).to_string();
                        if stdout.trim().is_empty() {
                            ("No matches found".into(), false)
                        } else {
                            (stdout, false)
                        }
                    }
                    Err(e) => (format!("Error: {e}"), true),
                }
            }
        }
    }

    fn glob_tool(&self, args: &serde_json::Value) -> (String, bool) {
        let pattern = match args.get("pattern").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ("Error: Missing 'pattern' parameter".into(), true),
        };
        // Use `find` as a simple glob approximation
        let output = std::process::Command::new("find")
            .args([".", "-name", pattern, "-maxdepth", "5"])
            .current_dir(&self.workspace)
            .output();
        match output {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout).to_string();
                if stdout.trim().is_empty() {
                    ("No files matched".into(), false)
                } else {
                    (stdout, false)
                }
            }
            Err(e) => (format!("Error: {e}"), true),
        }
    }

    fn git_cmd(&self, args: &[&str]) -> (String, bool) {
        match std::process::Command::new("git")
            .args(args)
            .current_dir(&self.workspace)
            .output()
        {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout).to_string();
                let stderr = String::from_utf8_lossy(&o.stderr).to_string();
                if o.status.success() {
                    (stdout, false)
                } else {
                    (format!("Error: {stderr}"), true)
                }
            }
            Err(e) => (format!("Error: {e}"), true),
        }
    }

    fn git_diff(&self, args: &serde_json::Value) -> (String, bool) {
        let mut cmd_args = vec!["diff"];
        let cached = args
            .get("staged")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if cached {
            cmd_args.push("--cached");
        }
        self.git_cmd(&cmd_args)
    }

    fn git_log(&self, args: &serde_json::Value) -> (String, bool) {
        let n = args.get("n").and_then(|v| v.as_u64()).unwrap_or(20).min(100);
        let n_str = format!("-{n}");
        self.git_cmd(&["log", "--oneline", &n_str])
    }
}

// ─── Connection loop ─────────────────────────────────────────────────────────

async fn run_edge_connection(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let url = if args.server_url.ends_with("/edge/ws") {
        args.server_url.clone()
    } else {
        format!("{}/edge/ws", args.server_url.trim_end_matches('/'))
    };

    tracing::info!(url = %url, edge_id = %args.edge_id, "Connecting to server...");

    let (ws_stream, _) = connect_async(&url).await?;
    let (mut write, mut read) = ws_stream.split();

    tracing::info!("WebSocket connected, authenticating...");

    // Send auth
    let hostname = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok());
    let auth_msg = EdgeToServer::Auth {
        token: args.token.clone(),
        edge_agent_id: args.edge_id.clone(),
        hostname,
        workspace_dir: Some(
            args.workspace_dir
                .canonicalize()
                .unwrap_or_else(|_| args.workspace_dir.clone())
                .to_string_lossy()
                .to_string(),
        ),
    };
    write
        .send(Message::Text(serde_json::to_string(&auth_msg)?.into()))
        .await?;

    // Wait for auth response
    let auth_timeout = Duration::from_secs(30);
    let auth_response = tokio::time::timeout(auth_timeout, read.next()).await;

    match auth_response {
        Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str::<ServerToEdge>(&text) {
            Ok(ServerToEdge::AuthOk { user_id }) => {
                tracing::info!(user_id = %user_id, "Authenticated successfully");
            }
            Ok(ServerToEdge::AuthError { message }) => {
                return Err(format!("Authentication failed: {message}").into());
            }
            _ => {
                return Err("Unexpected auth response".into());
            }
        },
        _ => {
            return Err("Auth timeout or connection closed".into());
        }
    }

    let executor = LocalToolExecutor::new(
        args.workspace_dir
            .canonicalize()
            .unwrap_or_else(|_| args.workspace_dir.clone()),
    );

    // Heartbeat ticker
    let mut heartbeat = tokio::time::interval(Duration::from_secs(30));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    tracing::info!(
        workspace = %args.workspace_dir.display(),
        "Edge agent ready — waiting for tool calls"
    );

    loop {
        tokio::select! {
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ServerToEdge>(&text) {
                            Ok(ServerToEdge::ToolRequest { request_id, tool, args: tool_args, timeout_secs: _ }) => {
                                tracing::info!(tool = %tool, request_id = %request_id, "Executing tool");
                                let start = Instant::now();
                                let (output, is_error) = executor.execute(&tool, &tool_args).await;
                                let duration_ms = start.elapsed().as_millis() as u64;
                                tracing::info!(
                                    tool = %tool,
                                    request_id = %request_id,
                                    duration_ms = duration_ms,
                                    is_error = is_error,
                                    output_len = output.len(),
                                    "Tool execution complete"
                                );
                                let result_msg = EdgeToServer::ToolResult {
                                    request_id,
                                    output,
                                    is_error,
                                    duration_ms,
                                };
                                write.send(Message::Text(serde_json::to_string(&result_msg)?.into())).await?;
                            }
                            Ok(ServerToEdge::Pong) => {
                                // heartbeat ack
                            }
                            Ok(ServerToEdge::Closing { reason }) => {
                                tracing::info!(reason = %reason, "Server closing connection");
                                break;
                            }
                            Ok(ServerToEdge::AuthOk { .. } | ServerToEdge::AuthError { .. }) => {
                                // ignore duplicate auth
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "Failed to parse server message");
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = write.send(Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        tracing::info!("Connection closed");
                        break;
                    }
                    _ => {}
                }
            }
            _ = heartbeat.tick() => {
                let ping = EdgeToServer::Ping;
                if write.send(Message::Text(serde_json::to_string(&ping)?.into())).await.is_err() {
                    tracing::warn!("Failed to send heartbeat");
                    break;
                }
            }
        }
    }

    Ok(())
}

// ─── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    eprintln!(
        "astra-edge v{} — remote tool execution agent",
        env!("CARGO_PKG_VERSION")
    );
    eprintln!("  server:    {}", args.server_url);
    eprintln!("  edge-id:   {}", args.edge_id);
    eprintln!("  workspace: {}", args.workspace_dir.display());
    eprintln!();

    loop {
        match run_edge_connection(&args).await {
            Ok(()) => {
                if !args.reconnect {
                    break;
                }
                tracing::info!("Disconnected, reconnecting in 5s...");
            }
            Err(e) => {
                tracing::error!(error = %e, "Connection error");
                if !args.reconnect {
                    std::process::exit(1);
                }
                tracing::info!("Reconnecting in 5s...");
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
