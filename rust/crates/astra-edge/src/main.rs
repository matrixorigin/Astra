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
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::Instrument;

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
        #[serde(rename = "timeout_secs", default = "default_timeout")]
        _timeout_secs: u64,
    },
    #[serde(rename = "edge_pong")]
    Pong,
    #[serde(rename = "edge_closing")]
    Closing { reason: String },
}

fn default_timeout() -> u64 {
    300
}

// ─── Connection loop ─────────────────────────────────────────────────────────

async fn run_edge_connection(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let url = if args.server_url.ends_with("/edge/ws") {
        args.server_url.clone()
    } else {
        format!("{}/edge/ws", args.server_url.trim_end_matches('/'))
    };

    tracing::info!(url = %url, edge_id = %args.edge_id, "Connecting to server...");

    let (ws_stream, _) = connect_async(&url).await.map_err(|e| {
        tracing::error!(
            target: "astra.edge",
            edge_id = %args.edge_id,
            url = %url,
            error = %e,
            "WebSocket connect failed"
        );
        e
    })?;
    let (mut write, mut read) = ws_stream.split();

    tracing::info!("WebSocket connected, authenticating...");

    // Send auth
    let hostname = hostname::get().ok().and_then(|h| h.into_string().ok());
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
                tracing::error!(
                    target: "astra.edge",
                    edge_id = %args.edge_id,
                    detail = %message,
                    "server rejected edge authentication"
                );
                return Err(format!("Authentication failed: {message}").into());
            }
            _ => {
                tracing::error!(
                    target: "astra.edge",
                    edge_id = %args.edge_id,
                    "unexpected auth response payload"
                );
                return Err("Unexpected auth response".into());
            }
        },
        _ => {
            tracing::error!(
                target: "astra.edge",
                edge_id = %args.edge_id,
                "auth timeout or connection closed before auth_ok"
            );
            return Err("Auth timeout or connection closed".into());
        }
    }

    let workspace = args
        .workspace_dir
        .canonicalize()
        .unwrap_or_else(|_| args.workspace_dir.clone());

    // Build a production ToolContext (not test) with HTTP client for web_fetch/GitHub
    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("astra-edge/0.1")
        .build()
        .expect("Failed to create HTTP client");
    let ctx = astra_tools::ToolContext {
        project_root: workspace.clone(),
        workspace_root: workspace.clone(),
        user_id: args.edge_id.clone(),
        session_id: format!("edge-{}", &uuid::Uuid::new_v4().to_string()[..8]),
        sandbox: astra_tools::SandboxConfig::standard(&workspace),
        http_client: Some(http_client.clone()),
        logger: std::sync::Arc::new(astra_tools::TracingLogger),
        cancel_token: None,
    };

    // Build executor with optional GitHub client (from GITHUB_TOKEN + gh auth token)
    let mut executor = astra_tools::executor::DefaultToolExecutor::new(ctx);
    let tokens = astra_tools::github::resolve_github_tokens();
    if !tokens.is_empty() {
        let github =
            astra_tools::github::GitHubClient::from_tokens(http_client, tokens, Vec::new());
        executor = executor.with_github_client(github);
    }

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
                            Ok(ServerToEdge::ToolRequest {
                                request_id,
                                tool,
                                args: tool_args,
                                _timeout_secs: _,
                            }) => {
                                tracing::info!(tool = %tool, request_id = %request_id, "Executing tool");
                                let start = Instant::now();
                                let result = astra_tools::ToolExecutor::execute(&executor, &tool, &tool_args).await;
                                let output = result.output;
                                let is_error = result.is_error;
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
    let _ = astra_logging::init_from_env(
        astra_logging::LogInitConfig::new("info").with_service_name("astra-edge"),
    );

    let args = Args::parse();

    eprintln!(
        "astra-edge v{} — remote tool execution agent",
        env!("CARGO_PKG_VERSION")
    );
    eprintln!("  server:    {}", args.server_url);
    eprintln!("  edge-id:   {}", args.edge_id);
    eprintln!("  workspace: {}", args.workspace_dir.display());
    eprintln!();

    let mut exit_with_error = false;
    loop {
        let edge_span = tracing::info_span!(
            "edge.agent",
            edge_id = %args.edge_id,
            server_url = %args.server_url,
        );
        match run_edge_connection(&args).instrument(edge_span).await {
            Ok(()) => {
                if !args.reconnect {
                    break;
                }
                tracing::info!("Disconnected, reconnecting in 5s...");
            }
            Err(e) => {
                tracing::error!(error = %e, "Connection error");
                if !args.reconnect {
                    exit_with_error = true;
                    break;
                }
                tracing::info!("Reconnecting in 5s...");
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    astra_logging::shutdown_otel();
    if exit_with_error {
        std::process::exit(1);
    }
}
