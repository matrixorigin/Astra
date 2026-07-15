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

mod invocation_journal;

use astra_credentials::{CredentialStore, CredentialsFile};
use astra_runtime_env::{
    ExecutorBinding, PolicyIntent, RunBinding, RuntimeBinding, RuntimeEnvironmentAdvertisement,
    ToolRegistry, WorkspaceAuthority, WorkspaceBinding,
};
use astra_server_types::edge_ws_protocol::{
    EDGE_AUTH_TIMEOUT_SECS, EDGE_HEARTBEAT_INTERVAL_SECS, EdgeClientMessage, EdgeServerMessage,
};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, client_async_tls_with_config, connect_async,
    tungstenite::Message,
};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use invocation_journal::{DurableEdgeResult, EdgeInvocationJournal, JournalError, PrepareOutcome};

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

struct CompletedEdgeInvocation {
    request_id: String,
    generation: u64,
    result: astra_tools::ToolResult,
    duration_ms: u64,
}

struct InFlightEdgeInvocation {
    generation: u64,
    cancel: CancellationToken,
}

#[derive(Default)]
struct EdgeInvocationTracker {
    in_flight: HashMap<String, InFlightEdgeInvocation>,
}

impl EdgeInvocationTracker {
    fn begin(&mut self, request_id: &str, generation: u64) -> Result<CancellationToken, u64> {
        if let Some(active) = self.in_flight.get(request_id) {
            return Err(active.generation);
        }
        let cancel = CancellationToken::new();
        self.in_flight.insert(
            request_id.to_string(),
            InFlightEdgeInvocation {
                generation,
                cancel: cancel.clone(),
            },
        );
        Ok(cancel)
    }

    fn cancel(&self, request_id: &str) -> Option<u64> {
        let active = self.in_flight.get(request_id)?;
        active.cancel.cancel();
        Some(active.generation)
    }

    fn finish_if_current(&mut self, request_id: &str, generation: u64) -> bool {
        if self
            .in_flight
            .get(request_id)
            .is_none_or(|active| active.generation != generation)
        {
            return false;
        }
        self.in_flight.remove(request_id);
        true
    }

    fn cancel_all(self) {
        for active in self.in_flight.into_values() {
            active.cancel.cancel();
        }
    }
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

fn edge_invocation_journal_path(edge_id: &str, workspace_dir: &Path) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(edge_id.as_bytes());
    hasher.update([0]);
    hasher.update(workspace_dir.to_string_lossy().as_bytes());
    let key = format!("{:x}", hasher.finalize());
    let base = CredentialStore::new()
        .path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(".astra"));
    base.join("edge-invocations").join(format!("{key}.json"))
}

/// Build the runtime-environment capability advertisement for this edge.
///
/// Callers must pass an already-canonical `workspace` path; `run_edge_agent`
/// enforces this via [`canonical_workspace_dir`] before calling here.
fn edge_runtime_environment_capabilities(edge_id: &str, workspace: &Path) -> Value {
    let registry = ToolRegistry::builtins();
    let workspace = workspace.to_string_lossy().to_string();
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

// ─── Proxy helpers ───────────────────────────────────────────────────────────

/// Parse `host` and `port` from a WebSocket URL (`ws://` or `wss://`).
///
/// Handles IPv6 bracket notation (`[::1]:port`) and strips any path/query
/// component.  Does not support userinfo — WebSocket URLs with credentials
/// are not a use-case here.
fn parse_ws_target(ws_url: &str) -> Option<(String, u16)> {
    let (rest, default_port) = if let Some(r) = ws_url.strip_prefix("wss://") {
        (r, 443u16)
    } else {
        let r = ws_url.strip_prefix("ws://")?;
        (r, 80u16)
    };
    // Drop path/query/fragment — only the authority matters.
    let authority = rest.split('/').next()?;
    parse_host_port(authority, default_port)
}

/// Parse `host` and `port` from an HTTP(S) proxy URL.
///
/// Accepts `http://` and `https://` schemes, strips userinfo (e.g.
/// `user:pass@`), handles IPv6 bracket notation, and defaults to port 3128
/// when no explicit port is present.
fn parse_proxy_addr(proxy_url: &str) -> Option<(String, u16)> {
    let rest = proxy_url
        .strip_prefix("http://")
        .or_else(|| proxy_url.strip_prefix("https://"))?;
    // Drop path/query/fragment.
    let authority = rest.split('/').next()?;
    // Strip optional userinfo (`user:pass@`).
    let host_port = match authority.rfind('@') {
        Some(at) => &authority[at + 1..],
        None => authority,
    };
    parse_host_port(host_port, 3128)
}

/// Parse `host:port` from an authority string, handling IPv6 bracket notation.
fn parse_host_port(authority: &str, default_port: u16) -> Option<(String, u16)> {
    if authority.starts_with('[') {
        // IPv6: `[::1]` or `[::1]:port`
        let bracket_end = authority.find(']')?;
        let host = authority.get(1..bracket_end)?.to_string();
        let port = if authority.as_bytes().get(bracket_end + 1) == Some(&b':') {
            authority.get(bracket_end + 2..)?.parse().ok()?
        } else {
            default_port
        };
        Some((host, port))
    } else if let Some(colon_pos) = authority.rfind(':') {
        let host = authority[..colon_pos].to_string();
        let port: u16 = authority[colon_pos + 1..].parse().ok()?;
        Some((host, port))
    } else {
        Some((authority.to_string(), default_port))
    }
}

// Connect to the WebSocket server via HTTP CONNECT proxy.
// Returns the same type as `connect_async` so callers stay uniform.
async fn connect_via_proxy(
    ws_url: &str,
    proxy_url: &str,
) -> Result<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>, Box<dyn std::error::Error>> {
    if proxy_url.starts_with("https://") {
        return Err(format!(
            "HTTPS proxies are not supported for WebSocket CONNECT \
             (the CONNECT handshake is sent in plaintext over a raw TCP connection). \
             Configure an http:// proxy instead: {proxy_url}"
        )
        .into());
    }
    let (ws_host, ws_port) =
        parse_ws_target(ws_url).ok_or_else(|| format!("Cannot parse WebSocket URL: {ws_url}"))?;
    let (proxy_host, proxy_port) = parse_proxy_addr(proxy_url)
        .ok_or_else(|| format!("Cannot parse proxy URL: {proxy_url}"))?;

    tracing::info!(proxy_host = %proxy_host, proxy_port, target = %format!("{ws_host}:{ws_port}"), "CONNECT via proxy");

    let mut tcp = tokio::net::TcpStream::connect(format!("{proxy_host}:{proxy_port}")).await?;

    // HTTP CONNECT handshake
    let req = format!("CONNECT {ws_host}:{ws_port} HTTP/1.1\r\nHost: {ws_host}:{ws_port}\r\n\r\n");
    tcp.write_all(req.as_bytes()).await?;

    // Read proxy response headers (200 Connection Established). Read until
    // end-of-headers (\r\n\r\n) so we don't mis-parse when the status line is
    // split across TCP packets.
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 1024];
    while buf.len() < 16 * 1024 {
        let n = tcp.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let resp = String::from_utf8_lossy(&buf);
    if !resp.starts_with("HTTP/1.1 200") && !resp.starts_with("HTTP/1.0 200") {
        let first_line = resp.lines().next().unwrap_or("(empty)").to_string();
        return Err(format!("Proxy CONNECT rejected: {first_line}").into());
    }

    // For wss://, the CONNECT tunnel is plain TCP — we must perform a TLS
    // handshake inside the tunnel before sending the WebSocket upgrade.
    // For ws://, no TLS is needed; send the upgrade directly over the tunnel.
    if ws_url.starts_with("wss://") {
        let mut root_store = rustls::RootCertStore::empty();
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls_config = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        let connector = Connector::Rustls(std::sync::Arc::new(tls_config));
        let (ws_stream, _) =
            client_async_tls_with_config(ws_url, tcp, None, Some(connector)).await?;
        Ok(ws_stream)
    } else {
        let (ws_stream, _) =
            tokio_tungstenite::client_async(ws_url, MaybeTlsStream::Plain(tcp)).await?;
        Ok(ws_stream)
    }
}

// ─── Connection loop ─────────────────────────────────────────────────────────

async fn run_edge_connection(config: &EdgeConfig) -> Result<(), Box<dyn std::error::Error>> {
    let url = config.server_url.clone();

    tracing::info!(url = %url, edge_id = %config.edge_id, "Connecting to server...");

    // Use HTTP proxy when http_proxy / HTTP_PROXY environment variable is set.
    let proxy = std::env::var("http_proxy")
        .or_else(|_| std::env::var("HTTP_PROXY"))
        .ok()
        .filter(|p| !p.is_empty());

    let ws_stream = if let Some(ref proxy_url) = proxy {
        connect_via_proxy(&url, proxy_url).await.map_err(|e| {
            tracing::error!(
                target: "astra.edge",
                edge_id = %config.edge_id,
                url = %url,
                proxy = %proxy_url.rsplit('@').next().unwrap_or(proxy_url),
                error = %e,
                "WebSocket connect via proxy failed"
            );
            e
        })?
    } else {
        let (ws, _) = connect_async(&url).await.map_err(|e| {
            tracing::error!(
                target: "astra.edge",
                edge_id = %config.edge_id,
                url = %url,
                error = %e,
                "WebSocket connect failed"
            );
            e
        })?;
        ws
    };
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
    let executor = Arc::new(astra_tools::executor::DefaultToolExecutor::for_workspace(
        &workspace,
        config.edge_id.clone(),
        session_id,
        "astra-edge/0.1",
        Duration::from_secs(30),
    ));
    let (completed_tx, mut completed_rx) = mpsc::channel::<CompletedEdgeInvocation>(1_024);
    let mut invocations = EdgeInvocationTracker::default();
    let journal_path = edge_invocation_journal_path(&config.edge_id, &workspace);
    let mut journal = EdgeInvocationJournal::open(journal_path).await?;

    // Results remain in the durable outbox until the server acknowledges the
    // exact delivery generation. Reconnect therefore starts by replaying them.
    for pending in journal.pending_results()? {
        let message = pending.result.client_message(
            pending.request_id,
            pending.identity,
            pending.delivery_generation,
        );
        write
            .send(Message::Text(serde_json::to_string(&message)?.into()))
            .await?;
    }

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
                        tracing::debug!(
                            frame_len = text.len(),
                            "edge received server text frame"
                        );
                        match serde_json::from_str::<EdgeServerMessage>(&text) {
                            Ok(EdgeServerMessage::ToolRequest {
                                request_id,
                                identity,
                                delivery_generation,
                                tool,
                                args: tool_args,
                                timeout_secs,
                            }) => {
                                match journal
                                    .prepare(
                                        &request_id,
                                        &identity,
                                        delivery_generation,
                                        &tool,
                                        &tool_args,
                                    )
                                    .await
                                {
                                    Ok(PrepareOutcome::Replay(result)) => {
                                        let message = result.client_message(
                                            request_id,
                                            identity,
                                            delivery_generation,
                                        );
                                        write.send(Message::Text(serde_json::to_string(&message)?.into())).await?;
                                        continue;
                                    }
                                    Ok(PrepareOutcome::Active) => {
                                        tracing::warn!(
                                            request_id = %request_id,
                                            delivery_generation,
                                            "Duplicate edge delivery joined the existing invocation"
                                        );
                                        continue;
                                    }
                                    Ok(PrepareOutcome::Execute) => {}
                                    Err(error @ (JournalError::Full | JournalError::WalFull)) => {
                                        let result = DurableEdgeResult::not_dispatched_rejection(
                                            format!("Edge invocation admission is temporarily saturated: {error}"),
                                        );
                                        let message = result.client_message(
                                            request_id,
                                            identity,
                                            delivery_generation,
                                        );
                                        write.send(Message::Text(serde_json::to_string(&message)?.into())).await?;
                                        continue;
                                    }
                                    Err(error @ JournalError::IdentityConflict { .. }) => {
                                        let result = DurableEdgeResult::from_tool_result(
                                            astra_tools::ToolResult::error(format!(
                                                "Edge invocation identity conflict before dispatch: {error}"
                                            )),
                                            0,
                                        );
                                        let message = result.client_message(
                                            request_id,
                                            identity,
                                            delivery_generation,
                                        );
                                        write.send(Message::Text(serde_json::to_string(&message)?.into())).await?;
                                        continue;
                                    }
                                    Err(error) => return Err(error.into()),
                                }
                                journal.mark_running(&request_id, delivery_generation).await?;
                                let cancel = match invocations.begin(&request_id, delivery_generation) {
                                    Ok(cancel) => cancel,
                                    Err(active_generation) => {
                                        return Err(format!(
                                            "edge invocation tracker conflicts with durable journal for {request_id}: active generation {active_generation}, incoming {delivery_generation}"
                                        ).into());
                                    }
                                };
                                let executor = executor.clone();
                                let completed_tx = completed_tx.clone();
                                tracing::info!(tool = %tool, request_id = %request_id, generation = delivery_generation, "Executing tool");
                                tokio::spawn(async move {
                                    let start = Instant::now();
                                    let execution = astra_tools::ToolExecutor::execute_with_cancel(
                                        executor.as_ref(),
                                        &tool,
                                        &tool_args,
                                        Some(&cancel),
                                    );
                                    let result = tokio::select! {
                                        _ = cancel.cancelled() => astra_tools::ToolResult::error(
                                            format!("Tool '{tool}' cancelled before completion")
                                        ),
                                        result = tokio::time::timeout(Duration::from_secs(timeout_secs), execution) => {
                                            match result {
                                                Ok(result) => result,
                                                Err(_) => astra_tools::ToolResult::error(
                                                    format!("Tool '{tool}' timed out after {timeout_secs}s")
                                                ),
                                            }
                                        }
                                    };
                                    let completion = CompletedEdgeInvocation {
                                        request_id,
                                        generation: delivery_generation,
                                        result,
                                        duration_ms: start.elapsed().as_millis() as u64,
                                    };
                                    let _ = completed_tx.send(completion).await;
                                });
                            }
                            Ok(EdgeServerMessage::Pong) => {
                                // heartbeat ack
                            }
                            Ok(EdgeServerMessage::ToolCancel { request_id, delivery_generation }) => {
                                if let Some(generation) = invocations.cancel(&request_id).filter(|generation| *generation == delivery_generation) {
                                    tracing::info!(
                                        request_id = %request_id,
                                        generation,
                                        "Cancelled in-flight edge invocation"
                                    );
                                } else {
                                    tracing::debug!(request_id = %request_id, "Ignoring cancellation for non-active edge invocation");
                                }
                            }
                            Ok(EdgeServerMessage::ToolResultAck { request_id, delivery_generation }) => {
                                if !journal.acknowledge(&request_id, delivery_generation).await? {
                                    tracing::warn!(
                                        request_id = %request_id,
                                        delivery_generation,
                                        "Ignoring stale or unknown edge result acknowledgement"
                                    );
                                }
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
            Some(completed) = completed_rx.recv() => {
                if !invocations.finish_if_current(&completed.request_id, completed.generation) {
                    tracing::warn!(
                        request_id = %completed.request_id,
                        generation = completed.generation,
                        "Discarding stale edge invocation completion"
                    );
                    continue;
                }
                let result = journal
                    .complete(
                        &completed.request_id,
                        completed.generation,
                        DurableEdgeResult::from_tool_result(completed.result, completed.duration_ms),
                    )
                    .await?;
                tracing::info!(
                    request_id = %completed.request_id,
                    generation = completed.generation,
                    duration_ms = completed.duration_ms,
                    is_error = result.is_error,
                    output_len = result.output.len(),
                    "Tool execution complete"
                );
                let record = journal.pending_results()?.into_iter().find(|pending| {
                    pending.request_id == completed.request_id
                        && pending.delivery_generation == completed.generation
                }).ok_or_else(|| format!("durable edge result {} disappeared before delivery", completed.request_id))?;
                let result_msg = record.result.client_message(
                    record.request_id,
                    record.identity,
                    record.delivery_generation,
                );
                write.send(Message::Text(serde_json::to_string(&result_msg)?.into())).await?;
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

    invocations.cancel_all();

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
                tracing::info!(delay = reconnect_delay_secs, "Reconnecting...");
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
    fn invocation_tracker_deduplicates_and_fences_stale_completions() {
        let mut tracker = EdgeInvocationTracker::default();
        let generation = 7;
        tracker.begin("request-1", generation).unwrap();
        assert_eq!(tracker.begin("request-1", 8).unwrap_err(), generation);
        assert!(!tracker.finish_if_current("request-1", generation + 1));
        assert_eq!(tracker.begin("request-1", 8).unwrap_err(), generation);
        assert!(tracker.finish_if_current("request-1", generation));

        let next_generation = generation + 1;
        tracker.begin("request-1", next_generation).unwrap();
        assert!(!tracker.finish_if_current("request-1", generation));
        assert!(tracker.finish_if_current("request-1", next_generation));
    }

    #[test]
    fn invocation_tracker_routes_cancellation_to_the_exact_active_request() {
        let mut tracker = EdgeInvocationTracker::default();
        let first_generation = 1;
        let first_cancel = tracker.begin("request-1", first_generation).unwrap();
        let second_cancel = tracker.begin("request-2", 2).unwrap();

        assert_eq!(tracker.cancel("request-1"), Some(first_generation));
        assert!(first_cancel.is_cancelled());
        assert!(!second_cancel.is_cancelled());
        assert_eq!(tracker.cancel("missing"), None);
    }

    #[test]
    fn edge_runtime_environment_capabilities_describe_local_edge_runtime() {
        let workspace = canonical_workspace_dir(Path::new(".")).expect("canonical test workspace");
        let value = edge_runtime_environment_capabilities("edge-test", &workspace);

        assert_eq!(
            value["schema_version"],
            RuntimeEnvironmentAdvertisement::SCHEMA_VERSION
        );
        assert_eq!(value["binding"]["workspace"]["kind"], "edge_workspace");
        assert_eq!(value["binding"]["workspace"]["authority"], "read_write");
        assert_eq!(
            value["binding"]["workspace"]["cwd"],
            workspace.to_string_lossy().as_ref()
        );
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
        assert!(
            value["binding"]["tool_surface"]["tool_names"]
                .as_array()
                .unwrap()
                .iter()
                .any(|name| name.as_str() == Some("bash"))
        );
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
