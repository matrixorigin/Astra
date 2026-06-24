use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use rmcp::{
    ClientHandler, Peer, RoleClient,
    model::{
        ArgumentInfo, CallToolRequestParams, CallToolResult, ClientCapabilities, ClientRequest,
        CompleteRequestParams, CompleteResult, GetPromptRequestParams, GetPromptResult,
        Implementation, InitializeRequestParams, ListRootsResult, LoggingLevel, PingRequest,
        Prompt, ReadResourceRequestParams, Reference, Resource, Root, RootsCapabilities,
        SetLevelRequestParams, SubscribeRequestParams, Tool, UnsubscribeRequestParams,
    },
    serve_client,
    service::{NotificationContext, RequestContext, RunningService, ServiceError},
    transport::TokioChildProcess,
};
use tokio::sync::RwLock;

use crate::error::McpError;
use crate::types::{McpServerConfig, Transport};

use super::{MCP_CONNECT_TIMEOUT_SECS, MCP_TOOL_CALL_TIMEOUT_SECS};
use crate::classic_sse::ClassicSseTransport;
use crate::tools::is_dangerous_env_var;

// ── ChangeHandler ──────────────────────────────────────────────────────

/// MCP client handler that tracks tool list change notifications.
#[derive(Debug, Clone)]
struct ChangeHandler {
    tools_changed: Arc<AtomicBool>,
    prompts_changed: Arc<AtomicBool>,
    resources_changed: Arc<AtomicBool>,
    roots: Arc<RwLock<Vec<Root>>>,
}

impl ChangeHandler {
    fn new(
        roots: Arc<RwLock<Vec<Root>>>,
    ) -> (Self, Arc<AtomicBool>, Arc<AtomicBool>, Arc<AtomicBool>) {
        let tools = Arc::new(AtomicBool::new(false));
        let prompts = Arc::new(AtomicBool::new(false));
        let resources = Arc::new(AtomicBool::new(false));
        (
            Self {
                tools_changed: tools.clone(),
                prompts_changed: prompts.clone(),
                resources_changed: resources.clone(),
                roots,
            },
            tools,
            prompts,
            resources,
        )
    }
}

impl ClientHandler for ChangeHandler {
    fn get_info(&self) -> InitializeRequestParams {
        let mut caps = ClientCapabilities::default();
        caps.roots = Some(RootsCapabilities {
            list_changed: Some(true),
        });
        InitializeRequestParams::new(
            caps,
            Implementation::new("astra", env!("CARGO_PKG_VERSION")),
        )
    }

    fn on_tool_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        self.tools_changed.store(true, Ordering::Release);
        std::future::ready(())
    }

    fn on_prompt_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        self.prompts_changed.store(true, Ordering::Release);
        std::future::ready(())
    }

    fn on_resource_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        self.resources_changed.store(true, Ordering::Release);
        std::future::ready(())
    }

    fn on_resource_updated(
        &self,
        _params: rmcp::model::ResourceUpdatedNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        self.resources_changed.store(true, Ordering::Release);
        std::future::ready(())
    }

    fn on_logging_message(
        &self,
        params: rmcp::model::LoggingMessageNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let level = match params.level {
            LoggingLevel::Debug => "DEBUG",
            LoggingLevel::Info => "INFO",
            LoggingLevel::Notice => "NOTICE",
            LoggingLevel::Warning => "WARN",
            LoggingLevel::Error => "ERROR",
            LoggingLevel::Critical => "CRIT",
            LoggingLevel::Alert => "ALERT",
            LoggingLevel::Emergency => "EMERG",
        };
        let logger = params.logger.as_deref().unwrap_or("mcp");
        let data = if let Some(s) = params.data.as_str() {
            s.to_string()
        } else {
            params.data.to_string()
        };
        tracing::info!(level, logger, data, "MCP server log message");
        std::future::ready(())
    }

    fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> impl std::future::Future<Output = Result<ListRootsResult, rmcp::model::ErrorData>> + Send + '_
    {
        let roots = self.roots.clone();
        async move {
            let r = roots.read().await;
            Ok(ListRootsResult::new(r.clone()))
        }
    }
}

// ── McpConnection ──────────────────────────────────────────────────────

/// Running MCP client connection.
pub struct McpConnection {
    pub name: String,
    peer: Peer<RoleClient>,
    tools: Vec<Tool>,
    connected_at: Option<Instant>,
    pub(crate) config: McpServerConfig,
    tools_changed: Arc<AtomicBool>,
    prompts_changed: Arc<AtomicBool>,
    resources_changed: Arc<AtomicBool>,
    #[allow(dead_code)]
    ws_bridge_handles: Option<(tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>)>,
    _running: Option<RunningService<RoleClient, ChangeHandler>>,
}

impl McpConnection {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn uptime(&self) -> Option<std::time::Duration> {
        self.connected_at.map(|t| t.elapsed())
    }

    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    pub async fn refresh_tools_if_changed(&mut self) -> Result<bool, ServiceError> {
        if self.tools_changed.swap(false, Ordering::AcqRel) {
            self.tools = self.peer.list_all_tools().await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn has_pending_tool_change(&self) -> bool {
        self.tools_changed.load(Ordering::Acquire)
    }

    pub fn consume_prompt_change(&self) -> bool {
        self.prompts_changed.swap(false, Ordering::AcqRel)
    }

    pub fn consume_resource_change(&self) -> bool {
        self.resources_changed.swap(false, Ordering::AcqRel)
    }

    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<CallToolResult, ServiceError> {
        let arguments = match arguments {
            serde_json::Value::Object(map) => Some(map),
            serde_json::Value::Null => None,
            _ => Some(serde_json::Map::from_iter([(
                "input".to_string(),
                arguments,
            )])),
        };
        let params = if let Some(args) = arguments {
            CallToolRequestParams::new(name.to_string()).with_arguments(args)
        } else {
            CallToolRequestParams::new(name.to_string())
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(MCP_TOOL_CALL_TIMEOUT_SECS),
            self.peer.call_tool(params),
        )
        .await
        .map_err(|_| {
            tracing::warn!(
                "MCP tool '{}' on server '{}' timed out after {}s",
                name,
                self.name,
                MCP_TOOL_CALL_TIMEOUT_SECS
            );
            ServiceError::Timeout {
                timeout: std::time::Duration::from_secs(MCP_TOOL_CALL_TIMEOUT_SECS),
            }
        })?
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t.name == name)
    }

    pub async fn list_resources(&self) -> Result<Vec<Resource>, ServiceError> {
        self.peer.list_all_resources().await
    }

    pub async fn read_resource(&self, uri: &str) -> Result<String, McpError> {
        let params = ReadResourceRequestParams::new(uri.to_string());
        let result = self
            .peer
            .read_resource(params)
            .await
            .map_err(McpError::Service)?;
        let text = result
            .contents
            .into_iter()
            .filter_map(|c| match c {
                rmcp::model::ResourceContents::TextResourceContents { text, .. } => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(text)
    }

    pub async fn subscribe_resource(&self, uri: &str) -> Result<(), ServiceError> {
        self.peer.subscribe(SubscribeRequestParams::new(uri)).await
    }

    pub async fn unsubscribe_resource(&self, uri: &str) -> Result<(), ServiceError> {
        self.peer
            .unsubscribe(UnsubscribeRequestParams::new(uri))
            .await
    }

    pub async fn set_log_level(&self, level: LoggingLevel) -> Result<(), ServiceError> {
        self.peer.set_level(SetLevelRequestParams::new(level)).await
    }

    pub async fn list_prompts(&self) -> Result<Vec<Prompt>, ServiceError> {
        self.peer.list_all_prompts().await
    }

    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<GetPromptResult, ServiceError> {
        let mut params = GetPromptRequestParams::new(name.to_string());
        if let Some(args) = arguments {
            params.arguments = Some(args);
        }
        self.peer.get_prompt(params).await
    }

    pub async fn ping(&self) -> Result<(), ServiceError> {
        let ping = PingRequest {
            method: Default::default(),
            extensions: Default::default(),
        };
        self.peer
            .send_request(ClientRequest::PingRequest(ping))
            .await?;
        Ok(())
    }

    /// Request argument completions from the server.
    pub async fn complete(
        &self,
        reference: Reference,
        argument_name: &str,
        argument_value: &str,
    ) -> Result<CompleteResult, ServiceError> {
        let params = CompleteRequestParams::new(
            reference,
            ArgumentInfo {
                name: argument_name.to_string(),
                value: argument_value.to_string(),
            },
        );
        self.peer.complete(params).await
    }

    /// Discover `skill://` resources and return (name, content) pairs.
    pub async fn discover_skill_resources(&self) -> Vec<(String, String)> {
        let resources = match self.list_resources().await {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        let mut skills = Vec::new();
        for res in &resources {
            if res.raw.uri.starts_with("skill://")
                && let Ok(content) = self.read_resource(&res.raw.uri).await
                && !content.is_empty()
            {
                skills.push((res.raw.name.clone(), content));
            }
        }
        skills
    }
}

// ── Public connection API ──────────────────────────────────────────────

/// Connect to an MCP server with exponential backoff retry.
pub async fn connect_to_server(
    config: McpServerConfig,
    roots: Arc<RwLock<Vec<Root>>>,
) -> Result<McpConnection, McpError> {
    let retry = config.retry.clone();
    let name = config.name.clone();

    let mut last_error = None;
    for attempt in 0..=retry.max_retries {
        if attempt > 0 {
            let delay_ms = std::cmp::min(
                retry.initial_delay_ms * 2u64.saturating_pow(attempt - 1),
                retry.max_delay_ms,
            );
            tracing::info!(
                "Retrying MCP connection to {name} (attempt {attempt}/{max}, backoff {delay_ms}ms)",
                max = retry.max_retries,
            );
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }

        match connect_once(&config, roots.clone()).await {
            Ok(conn) => return Ok(conn),
            Err(e) => {
                if matches!(e, McpError::InvalidConfig(_)) {
                    return Err(e);
                }
                last_error = Some(e);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        McpError::Initialize(format!(
            "{name}: all {n} retries exhausted",
            n = retry.max_retries
        ))
    }))
}

/// Single connection attempt (no retry).
async fn connect_once(
    config: &McpServerConfig,
    roots: Arc<RwLock<Vec<Root>>>,
) -> Result<McpConnection, McpError> {
    match &config.transport {
        Transport::Stdio { command, args, env } => {
            connect_stdio(&config.name, command, args, env, config.clone(), roots).await
        }
        Transport::Sse {
            url,
            auth_token,
            headers,
        } => {
            connect_sse(
                &config.name,
                url,
                auth_token.as_deref(),
                headers,
                config.clone(),
                roots,
            )
            .await
        }
        Transport::StreamableHttp {
            url,
            auth_token,
            headers,
        } => {
            connect_streamable_http(
                &config.name,
                url,
                auth_token.as_deref(),
                headers,
                config.clone(),
                roots,
            )
            .await
        }
        Transport::Ws {
            url,
            auth_token,
            headers,
        } => {
            connect_ws(
                &config.name,
                url,
                auth_token.as_deref(),
                headers,
                config.clone(),
                roots,
            )
            .await
        }
    }
}

async fn connect_stdio(
    name: &str,
    command: &[String],
    args: &[String],
    env: &HashMap<String, String>,
    config: McpServerConfig,
    roots: Arc<RwLock<Vec<Root>>>,
) -> Result<McpConnection, McpError> {
    if command.is_empty() {
        return Err(McpError::InvalidConfig(
            "command cannot be empty".to_string(),
        ));
    }

    let mut cmd = tokio::process::Command::new(&command[0]);
    if command.len() > 1 {
        cmd.args(&command[1..]);
    }
    cmd.args(args);
    for (key, value) in env {
        if is_dangerous_env_var(key) {
            tracing::warn!("MCP server '{}': blocked dangerous env var '{}'", name, key);
            continue;
        }
        cmd.env(key, value);
    }

    let transport = TokioChildProcess::new(cmd).map_err(|e| McpError::Spawn(e.to_string()))?;
    let (handler, tools_changed, prompts_changed, resources_changed) = ChangeHandler::new(roots);

    let running = tokio::time::timeout(
        std::time::Duration::from_secs(MCP_CONNECT_TIMEOUT_SECS),
        serve_client(handler, transport),
    )
    .await
    .map_err(|_| {
        McpError::Initialize(format!(
            "{name}: connection timed out after {MCP_CONNECT_TIMEOUT_SECS}s"
        ))
    })?
    .map_err(|e| McpError::Initialize(e.to_string()))?;

    let peer = running.peer().clone();
    let tools = fetch_tools_with_timeout(&peer, name).await?;

    Ok(McpConnection {
        name: name.to_string(),
        peer,
        tools,
        connected_at: Some(Instant::now()),
        config,
        tools_changed,
        prompts_changed,
        resources_changed,
        ws_bridge_handles: None,
        _running: Some(running),
    })
}

async fn connect_sse(
    name: &str,
    url: &str,
    auth_token: Option<&str>,
    headers: &HashMap<String, String>,
    config: McpServerConfig,
    roots: Arc<RwLock<Vec<Root>>>,
) -> Result<McpConnection, McpError> {
    if url.is_empty() {
        return Err(McpError::InvalidConfig("url cannot be empty".to_string()));
    }

    let transport = ClassicSseTransport::connect(name, url, http_header_map(auth_token, headers)?)
        .await
        .map_err(|e| McpError::Initialize(format!("SSE connect to {url}: {e}")))?;
    let (handler, tools_changed, prompts_changed, resources_changed) = ChangeHandler::new(roots);
    let running = tokio::time::timeout(
        std::time::Duration::from_secs(MCP_CONNECT_TIMEOUT_SECS),
        serve_client(handler, transport),
    )
    .await
    .map_err(|_| {
        McpError::Initialize(format!(
            "{name}: SSE connection timed out after {MCP_CONNECT_TIMEOUT_SECS}s"
        ))
    })?
    .map_err(|e| McpError::Initialize(format!("SSE connect to {url}: {e}")))?;

    let peer = running.peer().clone();
    let tools = fetch_tools_with_timeout(&peer, name).await?;

    Ok(McpConnection {
        name: name.to_string(),
        peer,
        tools,
        connected_at: Some(Instant::now()),
        config,
        tools_changed,
        prompts_changed,
        resources_changed,
        ws_bridge_handles: None,
        _running: Some(running),
    })
}

async fn connect_streamable_http(
    name: &str,
    url: &str,
    auth_token: Option<&str>,
    headers: &HashMap<String, String>,
    config: McpServerConfig,
    roots: Arc<RwLock<Vec<Root>>>,
) -> Result<McpConnection, McpError> {
    use rmcp::transport::streamable_http_client::{
        StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
    };

    if url.is_empty() {
        return Err(McpError::InvalidConfig("url cannot be empty".to_string()));
    }

    let mut transport_config = StreamableHttpClientTransportConfig::with_uri(url);
    transport_config.reinit_on_expired_session = true;

    if let Some(token) = auth_token {
        transport_config = transport_config.auth_header(token);
    }

    if !headers.is_empty() {
        let mut custom = HashMap::new();
        for (k, v) in headers {
            let header_name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| McpError::InvalidConfig(format!("invalid header name '{k}': {e}")))?;
            let header_value = reqwest::header::HeaderValue::from_str(v).map_err(|e| {
                McpError::InvalidConfig(format!("invalid header value for '{k}': {e}"))
            })?;
            custom.insert(header_name, header_value);
        }
        transport_config = transport_config.custom_headers(custom);
    }

    let transport = StreamableHttpClientTransport::from_config(transport_config);
    let (handler, tools_changed, prompts_changed, resources_changed) = ChangeHandler::new(roots);
    let running = tokio::time::timeout(
        std::time::Duration::from_secs(MCP_CONNECT_TIMEOUT_SECS),
        serve_client(handler, transport),
    )
    .await
    .map_err(|_| {
        McpError::Initialize(format!(
            "{name}: SSE connection timed out after {MCP_CONNECT_TIMEOUT_SECS}s"
        ))
    })?
    .map_err(|e| McpError::Initialize(format!("Streamable HTTP connect to {url}: {e}")))?;

    let peer = running.peer().clone();
    let tools = fetch_tools_with_timeout(&peer, name).await?;

    Ok(McpConnection {
        name: name.to_string(),
        peer,
        tools,
        connected_at: Some(Instant::now()),
        config,
        tools_changed,
        prompts_changed,
        resources_changed,
        ws_bridge_handles: None,
        _running: Some(running),
    })
}

fn http_header_map(
    auth_token: Option<&str>,
    headers: &HashMap<String, String>,
) -> Result<reqwest::header::HeaderMap, McpError> {
    let mut out = reqwest::header::HeaderMap::new();
    if let Some(token) = auth_token {
        let value =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")).map_err(|e| {
                McpError::InvalidConfig(format!("invalid Authorization header value: {e}"))
            })?;
        out.insert(reqwest::header::AUTHORIZATION, value);
    }
    for (key, value) in headers {
        let header_name = reqwest::header::HeaderName::from_bytes(key.as_bytes())
            .map_err(|e| McpError::InvalidConfig(format!("invalid header name '{key}': {e}")))?;
        let header_value = reqwest::header::HeaderValue::from_str(value).map_err(|e| {
            McpError::InvalidConfig(format!("invalid header value for '{key}': {e}"))
        })?;
        out.insert(header_name, header_value);
    }
    Ok(out)
}

async fn connect_ws(
    name: &str,
    url: &str,
    auth_token: Option<&str>,
    headers: &HashMap<String, String>,
    config: McpServerConfig,
    roots: Arc<RwLock<Vec<Root>>>,
) -> Result<McpConnection, McpError> {
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio_tungstenite::tungstenite;

    if url.is_empty() {
        return Err(McpError::InvalidConfig("url cannot be empty".to_string()));
    }

    let mut request = tungstenite::http::Request::builder()
        .uri(url)
        .header("Sec-WebSocket-Version", "13")
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header(
            "Sec-WebSocket-Key",
            tungstenite::handshake::client::generate_key(),
        );
    if let Some(token) = auth_token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    for (key, value) in headers {
        request = request.header(key.as_str(), value.as_str());
    }
    let request = request
        .body(())
        .map_err(|e| McpError::InvalidConfig(format!("invalid WebSocket request: {e}")))?;

    let (ws_stream, _response) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| McpError::Initialize(format!("WebSocket connect to {url}: {e}")))?;

    let (mut ws_sink, mut ws_read) = ws_stream.split();

    let (rmcp_read, mut bridge_write) = tokio::io::duplex(64 * 1024);
    let (mut bridge_read, rmcp_write) = tokio::io::duplex(64 * 1024);

    let ws_name = name.to_string();
    let reader_name = ws_name.clone();
    let ws_read_handle = tokio::spawn(async move {
        loop {
            match ws_read.next().await {
                Some(Ok(tungstenite::Message::Text(text))) => {
                    if bridge_write.write_all(text.as_bytes()).await.is_err()
                        || bridge_write.write_all(b"\n").await.is_err()
                    {
                        break;
                    }
                }
                Some(Ok(tungstenite::Message::Close(_))) => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    tracing::warn!("MCP WebSocket read error [{reader_name}]: {e}");
                    break;
                }
                None => break,
            }
        }
        drop(bridge_write);
    });

    let writer_name = ws_name;
    let ws_write_handle = tokio::spawn(async move {
        let mut reader = BufReader::new(&mut bridge_read);
        let mut line = String::new();
        loop {
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line.trim_end();
                    if !trimmed.is_empty()
                        && ws_sink
                            .send(tungstenite::Message::Text(trimmed.to_owned().into()))
                            .await
                            .is_err()
                    {
                        break;
                    }
                    line.clear();
                }
                Err(e) => {
                    tracing::warn!("MCP WebSocket write-bridge error [{writer_name}]: {e}");
                    break;
                }
            }
        }
    });

    let (handler, tools_changed, prompts_changed, resources_changed) = ChangeHandler::new(roots);
    let running = serve_client(handler, (rmcp_read, rmcp_write))
        .await
        .map_err(|e| McpError::Initialize(format!("MCP init over WebSocket {url}: {e}")))?;

    let peer = running.peer().clone();
    let tools = fetch_tools_with_timeout(&peer, name).await?;

    Ok(McpConnection {
        name: name.to_string(),
        peer,
        tools,
        connected_at: Some(Instant::now()),
        config,
        tools_changed,
        prompts_changed,
        resources_changed,
        ws_bridge_handles: Some((ws_read_handle, ws_write_handle)),
        _running: Some(running),
    })
}

async fn fetch_tools_with_timeout(
    peer: &rmcp::service::Peer<rmcp::RoleClient>,
    name: &str,
) -> Result<Vec<Tool>, McpError> {
    tokio::time::timeout(
        std::time::Duration::from_secs(MCP_CONNECT_TIMEOUT_SECS),
        peer.list_all_tools(),
    )
    .await
    .map_err(|_| {
        McpError::Initialize(format!(
            "{name}: tool list fetch timed out after {MCP_CONNECT_TIMEOUT_SECS}s"
        ))
    })?
    .map_err(McpError::Service)
}
