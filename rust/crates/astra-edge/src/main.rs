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

use astra_server_types::edge_ws_protocol::{
    EDGE_AUTH_TIMEOUT_SECS, EDGE_HEARTBEAT_INTERVAL_SECS, EdgeClientMessage, EdgeServerMessage,
};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
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
    let auth_msg = EdgeClientMessage::Auth {
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
        capabilities: None,
    };
    write
        .send(Message::Text(serde_json::to_string(&auth_msg)?.into()))
        .await?;

    // Wait for auth response
    let auth_timeout = Duration::from_secs(EDGE_AUTH_TIMEOUT_SECS);
    let auth_response = tokio::time::timeout(auth_timeout, read.next()).await;

    match auth_response {
        Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str::<EdgeServerMessage>(&text)
        {
            Ok(EdgeServerMessage::AuthOk { user_id }) => {
                tracing::info!(user_id = %user_id, "Authenticated successfully");
            }
            Ok(EdgeServerMessage::AuthError { message }) => {
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

    let session_id = format!("edge-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let executor = astra_tools::executor::DefaultToolExecutor::for_workspace(
        &workspace,
        args.edge_id.clone(),
        session_id,
        "astra-edge/0.1",
        Duration::from_secs(30),
    );

    // Heartbeat ticker
    let mut heartbeat = tokio::time::interval(Duration::from_secs(EDGE_HEARTBEAT_INTERVAL_SECS));
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
                        match serde_json::from_str::<EdgeServerMessage>(&text) {
                            Ok(EdgeServerMessage::ToolRequest {
                                request_id,
                                tool,
                                args: tool_args,
                                timeout_secs: _,
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
                                let result_msg = EdgeClientMessage::ToolResult {
                                    request_id,
                                    output,
                                    is_error,
                                    duration_ms: Some(duration_ms),
                                };
                                write.send(Message::Text(serde_json::to_string(&result_msg)?.into())).await?;
                            }
                            Ok(EdgeServerMessage::Pong) => {
                                // heartbeat ack
                            }
                            Ok(EdgeServerMessage::Closing { reason }) => {
                                tracing::info!(reason = %reason, "Server closing connection");
                                break;
                            }
                            Ok(EdgeServerMessage::AuthOk { .. } | EdgeServerMessage::AuthError { .. }) => {
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
                let ping = EdgeClientMessage::Ping;
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
