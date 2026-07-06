//! `astra-edge` — lightweight remote tool execution agent.
//!
//! Connects to an Astra server via WebSocket, authenticates, and executes
//! tool calls locally on the user's machine. Results are sent back over the
//! same WebSocket connection.
//!
//! ## Usage
//! ```bash
//! astra-edge --server-url https://astra.example.com --workspace-dir ~/projects/my-app
//! ```

use astra_credentials::{CredentialStore, CredentialsFile};
use astra_runtime_env::{
    ExecutorBinding, PolicyIntent, RunBinding, RuntimeBinding, RuntimeEnvironmentAdvertisement,
    ToolRegistry, WorkspaceAuthority, WorkspaceBinding,
};
use astra_server_types::edge_ws_protocol::{
    EdgeClientMessage, EdgeServerMessage, EDGE_AUTH_TIMEOUT_SECS, EDGE_HEARTBEAT_INTERVAL_SECS,
};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::Instrument;

/// Astra remote edge agent — execute tool calls locally for web sessions.
#[derive(Parser, Debug)]
#[command(name = "astra-edge", version, about)]
struct Args {
    /// Astra API/WebSocket base URL. Accepts http(s)://host[:port] or ws(s)://host[:port]/edge/ws.
    ///
    /// Defaults to ASTRA_SERVER_URL, then ASTRA_API_URL, then http://127.0.0.1:17001.
    #[arg(long)]
    server_url: Option<String>,

    /// Authentication token (JWT). When omitted, astra-edge reads the selected Astra CLI profile.
    #[arg(long, env = "ASTRA_TOKEN")]
    token: Option<String>,

    /// Astra CLI credentials profile to read when --token is omitted.
    #[arg(long)]
    profile: Option<String>,

    /// Local workspace directory for file operations
    #[arg(long, env = "ASTRA_WORKSPACE_DIR", default_value = ".")]
    workspace_dir: PathBuf,

    /// Edge agent identifier. Defaults to a stable id derived from hostname + canonical workspace.
    #[arg(long, env = "ASTRA_EDGE_ID")]
    edge_id: Option<String>,

    /// Auto-reconnect on disconnect
    #[arg(long, default_value_t = true)]
    reconnect: bool,
}

#[derive(Debug, Clone)]
struct EdgeConfig {
    server_url: String,
    token: String,
    workspace_dir: PathBuf,
    edge_id: String,
    reconnect: bool,
}

fn normalized_hostname() -> String {
    let hostname = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".into());
    hostname
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '.' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn default_edge_id(workspace_dir: &Path) -> String {
    let workspace = canonical_workspace_dir(workspace_dir).unwrap_or_else(|_| {
        // Fall back to the non-canonical path for edge ID stability
        workspace_dir.to_path_buf()
    });
    let mut hasher = Sha256::new();
    hasher.update(workspace.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let suffix = digest[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("edge-{}-{suffix}", normalized_hostname())
}

fn default_server_url() -> String {
    std::env::var("ASTRA_SERVER_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("ASTRA_API_URL")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| "http://127.0.0.1:17001".to_string())
}

fn edge_ws_url(server_url: &str) -> Result<String, String> {
    let trimmed = server_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("server URL must not be empty".to_string());
    }
    let with_ws_scheme = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if trimmed.starts_with("ws://") || trimmed.starts_with("wss://") {
        trimmed.to_string()
    } else if trimmed.contains("://") {
        return Err(format!(
            "unsupported server URL scheme in '{trimmed}'; use http(s):// or ws(s)://"
        ));
    } else {
        format!("ws://{trimmed}")
    };

    if let Ok(mut url) = reqwest::Url::parse(&with_ws_scheme) {
        if !matches!(url.scheme(), "ws" | "wss") {
            return Err(format!(
                "unsupported edge WebSocket URL scheme '{}'; use ws:// or wss://",
                url.scheme()
            ));
        }
        url.set_path(&normalized_edge_ws_path(url.path()));
        url.set_query(None);
        url.set_fragment(None);
        Ok(url.to_string())
    } else {
        Err(format!("invalid server URL '{server_url}'"))
    }
}

fn normalized_edge_ws_path(path: &str) -> String {
    let segments = path
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();

    if segments.is_empty() {
        return "/edge/ws".to_string();
    }

    if let Some(index) = segments
        .windows(2)
        .position(|window| window == ["edge", "ws"])
    {
        return format!("/{}", segments[..index + 2].join("/"));
    }

    format!("/{}/edge/ws", segments.join("/"))
}

fn token_from_credentials(
    creds: &CredentialsFile,
    profile_override: Option<&str>,
) -> Result<(String, String), String> {
    let profile_name =
        CredentialStore::resolve_profile_name(profile_override, creds.current_profile.as_deref());
    let profile = creds
        .profiles
        .get(&profile_name)
        .ok_or_else(|| format!("no profile '{profile_name}', run `astra login` first"))?;
    let token = profile
        .access_token
        .clone()
        .ok_or_else(|| format!("profile '{profile_name}' is not logged in; run `astra login`"))?;
    Ok((profile_name, token))
}

fn resolve_token(args: &Args) -> Result<String, String> {
    if let Some(token) = args.token.as_ref().filter(|token| !token.trim().is_empty()) {
        return Ok(token.clone());
    }
    let creds = CredentialStore::new()
        .load()
        .map_err(|error| format!("failed to read Astra credentials: {error}"))?;
    let (profile_name, token) = token_from_credentials(&creds, args.profile.as_deref())?;
    tracing::info!(profile = %profile_name, "using Astra CLI profile token");
    Ok(token)
}

fn resolve_config(args: Args) -> Result<EdgeConfig, String> {
    let raw_server_url = args.server_url.clone().unwrap_or_else(default_server_url);
    let workspace_dir = canonical_workspace_dir(&args.workspace_dir)?;
    let token = resolve_token(&args)?;
    let edge_id = args
        .edge_id
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default_edge_id(&workspace_dir));
    Ok(EdgeConfig {
        server_url: edge_ws_url(&raw_server_url)?,
        token,
        workspace_dir,
        edge_id,
        reconnect: args.reconnect,
    })
}

fn canonical_workspace_dir(workspace_dir: &Path) -> Result<PathBuf, String> {
    workspace_dir.canonicalize().map_err(|error| {
        format!(
            "failed to canonicalize workspace directory '{}': {error}",
            workspace_dir.display()
        )
    })
}

fn edge_runtime_environment_capabilities(edge_id: &str, workspace: &Path) -> Value {
    let registry = ToolRegistry::builtins();
    let workspace = canonical_workspace_dir(workspace)
        .unwrap_or_else(|_| workspace.to_path_buf())
        .to_string_lossy()
        .to_string();
    let binding = RunBinding::resolve(
        WorkspaceBinding::edge_workspace(workspace, WorkspaceAuthority::ReadWrite),
        ExecutorBinding::edge_agent(edge_id.to_string()),
        RuntimeBinding::host_process(format!("edge-host:{edge_id}")),
        PolicyIntent::local_developer(),
        &registry,
    );

    serde_json::to_value(RuntimeEnvironmentAdvertisement::new(binding))
        .expect("runtime environment advertisement serializes")
}

// ─── Connection loop ─────────────────────────────────────────────────────────

async fn run_edge_connection(config: &EdgeConfig) -> Result<(), Box<dyn std::error::Error>> {
    let url = config.server_url.clone();

    tracing::info!(url = %url, edge_id = %config.edge_id, "Connecting to server...");

    let (ws_stream, _) = connect_async(&url).await.map_err(|e| {
        tracing::error!(
            target: "astra.edge",
            edge_id = %config.edge_id,
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
    let workspace = canonical_workspace_dir(&config.workspace_dir).map_err(|e| {
        Box::new(std::io::Error::new(std::io::ErrorKind::NotFound, e)) as Box<dyn std::error::Error>
    })?;
    let auth_msg = EdgeClientMessage::Auth {
        token: config.token.clone(),
        edge_agent_id: config.edge_id.clone(),
        hostname,
        workspace_dir: Some(workspace.to_string_lossy().to_string()),
        capabilities: Some(edge_runtime_environment_capabilities(
            &config.edge_id,
            &workspace,
        )),
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
                    edge_id = %config.edge_id,
                    detail = %message,
                    "server rejected edge authentication"
                );
                return Err(format!("Authentication failed: {message}").into());
            }
            _ => {
                tracing::error!(
                    target: "astra.edge",
                    edge_id = %config.edge_id,
                    "unexpected auth response payload"
                );
                return Err("Unexpected auth response".into());
            }
        },
        _ => {
            tracing::error!(
                target: "astra.edge",
                edge_id = %config.edge_id,
                "auth timeout or connection closed before auth_ok"
            );
            return Err("Auth timeout or connection closed".into());
        }
    }

    let session_id = format!("edge-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let executor = astra_tools::executor::DefaultToolExecutor::for_workspace(
        &workspace,
        config.edge_id.clone(),
        session_id,
        "astra-edge/0.1",
        Duration::from_secs(30),
    );

    // Heartbeat ticker
    let mut heartbeat = tokio::time::interval(Duration::from_secs(EDGE_HEARTBEAT_INTERVAL_SECS));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    tracing::info!(
        workspace = %config.workspace_dir.display(),
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
                                let result = astra_tools::ToolExecutor::execute(
                                    &executor,
                                    &tool,
                                    &tool_args,
                                )
                                .await;
                                let output = result.output;
                                let is_error = result.is_error;
                                let tool_result_fields = result.metadata;
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
                                    tool_result_fields,
                                };
                                write.send(Message::Text(serde_json::to_string(&result_msg)?.into())).await?;
                            }
                            Ok(EdgeServerMessage::Pong) => {
                                // heartbeat ack
                            }
                            Ok(EdgeServerMessage::ToolCancel { request_id }) => {
                                tracing::warn!(
                                    request_id = %request_id,
                                    "Server cancelled tool request, but edge has no in-flight cancellation hook yet; ignoring"
                                );
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
    let config = match resolve_config(args) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("Error: {error}");
            std::process::exit(2);
        }
    };

    eprintln!(
        "astra-edge v{} — remote tool execution agent",
        env!("CARGO_PKG_VERSION")
    );
    eprintln!("  server:    {}", config.server_url);
    eprintln!("  edge-id:   {}", config.edge_id);
    eprintln!("  workspace: {}", config.workspace_dir.display());
    eprintln!();

    let mut exit_with_error = false;
    let mut reconnect_delay_secs: u64 = 1;
    let max_reconnect_delay_secs: u64 = 60;
    loop {
        let edge_span = tracing::info_span!(
            "edge.agent",
            edge_id = %config.edge_id,
            server_url = %config.server_url,
        );
        match run_edge_connection(&config).instrument(edge_span).await {
            Ok(()) => {
                reconnect_delay_secs = 1; // reset on clean disconnect
                if !config.reconnect {
                    break;
                }
                tracing::info!(
                    delay = reconnect_delay_secs,
                    "Disconnected, reconnecting..."
                );
            }
            Err(e) => {
                let err_str = e.to_string();
                // Permanent errors: authentication failures, invalid config
                let is_permanent = err_str.contains("Authentication failed")
                    || err_str.contains("401")
                    || err_str.contains("403")
                    || err_str.contains("invalid token");
                if is_permanent {
                    tracing::error!(
                        error = %e,
                        "Permanent authentication failure — not retrying"
                    );
                    exit_with_error = true;
                    break;
                }
                tracing::error!(error = %e, "Connection error");
                if !config.reconnect {
                    exit_with_error = true;
                    break;
                }
                tracing::info!(
                    delay = reconnect_delay_secs,
                    "Reconnecting..."
                );
            }
        }
        // Exponential backoff with jitter
        // jitter in [0.5*delay, 1.5*delay) — spreads out thundering herd
        let jitter = reconnect_delay_secs as f64 * (0.5 + fastrand::f64());
        tokio::time::sleep(Duration::from_secs_f64(jitter)).await;
        reconnect_delay_secs = (reconnect_delay_secs * 2).min(max_reconnect_delay_secs);
    }

    astra_logging::shutdown_otel();
    if exit_with_error {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_runtime_environment_capabilities_describe_local_edge_runtime() {
        let value = edge_runtime_environment_capabilities("edge-test", Path::new("/workspace/app"));

        assert_eq!(
            value["schema_version"],
            RuntimeEnvironmentAdvertisement::SCHEMA_VERSION
        );
        assert_eq!(value["binding"]["workspace"]["kind"], "edge_workspace");
        assert_eq!(value["binding"]["workspace"]["authority"], "read_write");
        assert_eq!(value["binding"]["executor"]["kind"], "edge_agent");
        assert_eq!(value["binding"]["executor"]["executor_id"], "edge-test");
        assert_eq!(
            value["binding"]["runtime"]["session_manager"],
            "host_process"
        );
        assert_eq!(
            value["binding"]["capabilities"]["runtime"]["runtime_has_shell"],
            true
        );
        assert_eq!(
            value["binding"]["capabilities"]["runtime"]["runtime_has_git"],
            true
        );
        assert!(value["binding"]["tool_surface"]["tool_names"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name.as_str() == Some("bash")));
    }

    #[test]
    fn default_edge_id_is_stable_for_the_same_workspace() {
        let workspace = Path::new("/workspace/app");
        assert_eq!(default_edge_id(workspace), default_edge_id(workspace));
    }

    #[test]
    fn default_edge_id_is_workspace_scoped() {
        assert_ne!(
            default_edge_id(Path::new("/workspace/app-a")),
            default_edge_id(Path::new("/workspace/app-b"))
        );
    }

    #[test]
    fn edge_ws_url_accepts_api_or_ws_base_urls() {
        assert_eq!(
            edge_ws_url("http://127.0.0.1:17001").unwrap(),
            "ws://127.0.0.1:17001/edge/ws"
        );
        assert_eq!(
            edge_ws_url("https://astra.example.com").unwrap(),
            "wss://astra.example.com/edge/ws"
        );
        assert_eq!(
            edge_ws_url("wss://astra.example.com/edge/ws").unwrap(),
            "wss://astra.example.com/edge/ws"
        );
        assert_eq!(
            edge_ws_url("https://astra.example.com/edge/ws/extra-path").unwrap(),
            "wss://astra.example.com/edge/ws"
        );
        assert_eq!(
            edge_ws_url("https://astra.example.com/prefix").unwrap(),
            "wss://astra.example.com/prefix/edge/ws"
        );
        assert_eq!(
            edge_ws_url("https://astra.example.com/prefix/edge/ws/extra-path").unwrap(),
            "wss://astra.example.com/prefix/edge/ws"
        );
        assert_eq!(
            edge_ws_url("https://astra.example.com/not-edge/ws").unwrap(),
            "wss://astra.example.com/not-edge/ws/edge/ws"
        );
        assert_eq!(
            edge_ws_url("https://astra.example.com/edge/ws?debug=1#fragment").unwrap(),
            "wss://astra.example.com/edge/ws"
        );
        assert_eq!(
            edge_ws_url("127.0.0.1:17001").unwrap(),
            "ws://127.0.0.1:17001/edge/ws"
        );
        assert!(edge_ws_url("ftp://astra.example.com").is_err());
        assert!(edge_ws_url("").is_err());
    }

    #[test]
    fn token_from_credentials_uses_current_or_explicit_profile() {
        let mut creds = CredentialsFile {
            current_profile: Some("work".to_string()),
            profiles: Default::default(),
        };
        creds.profiles.insert(
            "work".to_string(),
            astra_credentials::Profile {
                access_token: Some("work-token".to_string()),
                ..Default::default()
            },
        );
        creds.profiles.insert(
            "other".to_string(),
            astra_credentials::Profile {
                access_token: Some("other-token".to_string()),
                ..Default::default()
            },
        );

        assert_eq!(
            token_from_credentials(&creds, None).unwrap(),
            ("work".to_string(), "work-token".to_string())
        );
        assert_eq!(
            token_from_credentials(&creds, Some("other")).unwrap(),
            ("other".to_string(), "other-token".to_string())
        );
    }
}
