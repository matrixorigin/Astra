//! Request-scoped runtime MCP wiring for server-side agent loops.
//!
//! Chat requests may provide MCP server endpoints with opaque credentials for
//! the current turn. The runtime discovers tools for that request and keeps the
//! resulting schemas and connections in memory only.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use astra_core::{ErrorResponse, error_response_coded};
use astra_mcp::{
    MAX_RESULT_CONTENT_LENGTH, McpClientManager, McpServerConfig, McpTool, Transport,
    mcp_tool_schema_from_parts, sanitize_tool_name, tools_to_schemas_checked,
};
use astra_services::{
    McpDiscoveredToolData, McpRegisterRequestData, mcp_binding_tool_namespace, mcp_schema_hash,
    runs::RuntimeMcpBindingRequest,
};
use axum::{Json, http::StatusCode};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::RwLock;

const AGENT_BINDING_MCP_RPC_TIMEOUT_SECS: u64 = 30;

static AGENT_BINDING_MCP_HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

#[derive(Clone)]
pub(crate) struct RuntimeMcpBundle {
    pub schemas: Vec<Value>,
    pub manager: Option<Arc<RwLock<McpClientManager>>>,
    pub agent_binding_mcp: Option<Arc<AgentBindingMcpRuntime>>,
}

#[derive(Clone)]
pub(crate) struct AgentBindingMcpRuntime {
    server_name: String,
    endpoint_url: String,
    authorization: String,
    tool_names_by_public_name: Arc<HashMap<String, String>>,
}

#[derive(Clone, Debug, PartialEq)]
struct AgentBindingMcpTool {
    name: String,
    description: Option<String>,
    input_schema: Value,
}

#[derive(Debug)]
struct AgentBindingMcpRpcError {
    detail: String,
}

#[derive(Debug, Default, Deserialize)]
struct McpCredentialTransport {
    #[serde(default)]
    auth_token: Option<String>,
    #[serde(default)]
    headers: HashMap<String, String>,
}

fn mcp_error(
    status: StatusCode,
    detail: impl Into<String>,
    code: &'static str,
) -> (StatusCode, Json<ErrorResponse>) {
    error_response_coded(status, detail, code)
}

fn agent_binding_mcp_http_client() -> &'static reqwest::Client {
    AGENT_BINDING_MCP_HTTP_CLIENT.get_or_init(|| {
        astra_core::net::build_internal_http_client(
            reqwest::Client::builder().pool_idle_timeout(Duration::from_secs(90)),
            "agent binding MCP client",
        )
    })
}

fn agent_binding_mcp_rpc_error(detail: impl Into<String>) -> AgentBindingMcpRpcError {
    AgentBindingMcpRpcError {
        detail: detail.into(),
    }
}

fn agent_binding_mcp_error_response(
    status: StatusCode,
    error: AgentBindingMcpRpcError,
    code: &'static str,
) -> (StatusCode, Json<ErrorResponse>) {
    mcp_error(status, error.detail, code)
}

fn validate_url(url: &str) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let parsed = reqwest::Url::parse(url).map_err(|error| {
        mcp_error(
            StatusCode::BAD_REQUEST,
            format!("MCP server url is invalid: {error}"),
            "mcp_server_invalid",
        )
    })?;
    match parsed.scheme() {
        "http" | "https" | "ws" | "wss" => Ok(()),
        other => Err(mcp_error(
            StatusCode::BAD_REQUEST,
            format!("MCP server url scheme '{other}' is unsupported"),
            "mcp_server_invalid",
        )),
    }
}

fn validate_runtime_url(url: &str) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let parsed = reqwest::Url::parse(url).map_err(|error| {
        mcp_error(
            StatusCode::BAD_REQUEST,
            format!("runtime MCP binding url is invalid: {error}"),
            "mcp_runtime_binding_invalid",
        )
    })?;
    match parsed.scheme() {
        "http" | "https" | "ws" | "wss" => Ok(()),
        other => Err(mcp_error(
            StatusCode::BAD_REQUEST,
            format!("runtime MCP binding url scheme '{other}' is unsupported"),
            "mcp_runtime_binding_invalid",
        )),
    }
}

fn credential_transport(
    key_value: &Value,
) -> Result<McpCredentialTransport, (StatusCode, Json<ErrorResponse>)> {
    if !key_value.is_object() {
        return Err(mcp_error(
            StatusCode::BAD_REQUEST,
            "MCP binding key_value must be a JSON object",
            "mcp_key_value_invalid",
        ));
    }
    serde_json::from_value::<McpCredentialTransport>(key_value.clone()).map_err(|_| {
        mcp_error(
            StatusCode::BAD_REQUEST,
            "MCP binding key_value supports string auth_token and string headers only",
            "mcp_key_value_invalid",
        )
    })
}

fn server_config(
    tool_namespace: &str,
    transport: &str,
    url: &str,
    description: Option<&str>,
    key_value: &Value,
) -> Result<McpServerConfig, (StatusCode, Json<ErrorResponse>)> {
    let tool_namespace = sanitize_tool_name(tool_namespace.trim());
    if tool_namespace.is_empty() {
        return Err(mcp_error(
            StatusCode::BAD_REQUEST,
            "MCP binding tool namespace must not be empty after sanitization",
            "mcp_binding_invalid",
        ));
    }

    validate_url(url.trim())?;
    let credential = credential_transport(key_value)?;
    let transport = match transport.trim().to_ascii_lowercase().as_str() {
        "sse" => Transport::Sse {
            url: url.trim().to_string(),
            auth_token: credential.auth_token,
            headers: credential.headers,
        },
        "http" | "streamable_http" | "streamable-http" => Transport::StreamableHttp {
            url: url.trim().to_string(),
            auth_token: credential.auth_token,
            headers: credential.headers,
        },
        "ws" | "websocket" => Transport::Ws {
            url: url.trim().to_string(),
            auth_token: credential.auth_token,
            headers: credential.headers,
        },
        other => {
            return Err(mcp_error(
                StatusCode::BAD_REQUEST,
                format!("unsupported MCP transport '{other}'"),
                "mcp_transport_unsupported",
            ));
        }
    };

    Ok(McpServerConfig {
        name: tool_namespace,
        transport,
        description: description.unwrap_or_default().to_string(),
        enabled: true,
        retry: Default::default(),
    })
}

fn request_scoped_server_config(
    binding: &RuntimeMcpBindingRequest,
) -> Result<McpServerConfig, (StatusCode, Json<ErrorResponse>)> {
    let tool_namespace = sanitize_tool_name(binding.id.trim());
    if tool_namespace.is_empty() {
        return Err(mcp_error(
            StatusCode::BAD_REQUEST,
            "runtime_mcp_bindings[].id must not be empty after sanitization",
            "mcp_runtime_binding_invalid",
        ));
    }
    validate_runtime_url(binding.url.trim())?;
    let credential = McpCredentialTransport {
        auth_token: binding.auth_token.clone(),
        headers: binding.headers.clone(),
    };
    let transport = match binding.transport.trim().to_ascii_lowercase().as_str() {
        "sse" => Transport::Sse {
            url: binding.url.trim().to_string(),
            auth_token: credential.auth_token,
            headers: credential.headers,
        },
        "http" | "streamable_http" | "streamable-http" => Transport::StreamableHttp {
            url: binding.url.trim().to_string(),
            auth_token: credential.auth_token,
            headers: credential.headers,
        },
        "ws" | "websocket" => Transport::Ws {
            url: binding.url.trim().to_string(),
            auth_token: credential.auth_token,
            headers: credential.headers,
        },
        other => {
            return Err(mcp_error(
                StatusCode::BAD_REQUEST,
                format!("unsupported MCP transport '{other}'"),
                "mcp_runtime_binding_invalid",
            ));
        }
    };
    Ok(McpServerConfig {
        name: tool_namespace,
        transport,
        description: String::new(),
        enabled: true,
        retry: Default::default(),
    })
}

fn secret_field_name(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    matches!(
        normalized.as_str(),
        "authorization"
            | "bearer"
            | "token"
            | "authtoken"
            | "accesstoken"
            | "refreshtoken"
            | "apikey"
            | "secret"
            | "password"
            | "credential"
            | "credentials"
    ) || normalized.ends_with("token")
        || normalized.ends_with("apikey")
        || normalized.ends_with("secret")
        || normalized.ends_with("password")
}

fn collect_secret_strings(value: &Value, out: &mut Vec<String>) {
    collect_secret_strings_inner(value, out, false);
}

fn collect_secret_strings_inner(value: &Value, out: &mut Vec<String>, known_secret: bool) {
    match value {
        Value::String(s) if !s.is_empty() && (known_secret || s.len() >= 4) => {
            out.push(s.clone());
        }
        Value::Array(values) => {
            for value in values {
                collect_secret_strings_inner(value, out, known_secret);
            }
        }
        Value::Object(map) => {
            for (key, value) in map {
                collect_secret_strings_inner(value, out, known_secret || secret_field_name(key));
            }
        }
        _ => {}
    }
}

pub(crate) fn redact_known_secrets(raw: &str, key_value: &Value) -> String {
    let mut redacted = raw.to_string();
    let mut secrets = Vec::new();
    collect_secret_strings(key_value, &mut secrets);
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    secrets.dedup();
    for secret in secrets {
        redacted = redacted.replace(&secret, "[REDACTED]");
    }
    redact_mcp_error_text(&redacted)
}

fn runtime_binding_secret_value(binding: &RuntimeMcpBindingRequest) -> Value {
    json!({
        "auth_token": &binding.auth_token,
        "headers": &binding.headers,
        "url": &binding.url,
    })
}

pub(crate) fn redact_mcp_error_text(raw: &str) -> String {
    static BEARER_RE: OnceLock<Regex> = OnceLock::new();
    static SECRET_KV_RE: OnceLock<Regex> = OnceLock::new();

    let redacted = BEARER_RE
        .get_or_init(|| Regex::new("(?i)(bearer\\s+)[A-Za-z0-9._~+/=-]+").unwrap())
        .replace_all(raw, "${1}[REDACTED]")
        .to_string();
    SECRET_KV_RE
        .get_or_init(|| {
            Regex::new("(?i)((?:api[_-]?key|token|authorization)\\s*[:=]\\s*)[^\\s,;]+").unwrap()
        })
        .replace_all(&redacted, "${1}[REDACTED]")
        .to_string()
}

fn discovery_error(
    tool_namespace: &str,
    key_value: &Value,
    error: impl ToString,
) -> (StatusCode, Json<ErrorResponse>) {
    mcp_error(
        StatusCode::BAD_GATEWAY,
        format!(
            "MCP discovery failed for binding '{}': {}",
            tool_namespace,
            redact_known_secrets(&error.to_string(), key_value)
        ),
        "mcp_discovery_failed",
    )
}

fn output_schema_value(tool: &McpTool) -> Option<Value> {
    tool.output_schema
        .as_ref()
        .map(|schema| Value::Object((**schema).clone()))
}

fn input_schema_value(tool: &McpTool) -> Value {
    Value::Object((*tool.input_schema).clone())
}

pub(crate) async fn discover_binding_tools(
    binding_id: i64,
    request: &McpRegisterRequestData,
) -> Result<Vec<McpDiscoveredToolData>, (StatusCode, Json<ErrorResponse>)> {
    if binding_id <= 0 {
        return Err(mcp_error(
            StatusCode::BAD_REQUEST,
            "binding_id must be positive",
            "mcp_binding_invalid",
        ));
    }
    let tool_namespace = mcp_binding_tool_namespace(binding_id);

    let config = server_config(
        &tool_namespace,
        &request.server.transport,
        &request.server.url,
        request.server.description.as_deref(),
        &request.binding.key_value,
    )?;
    let mut manager = McpClientManager::new();
    manager
        .connect(config)
        .await
        .map_err(|error| discovery_error(&tool_namespace, &request.binding.key_value, error))?;

    let conn = manager.get(&tool_namespace).ok_or_else(|| {
        mcp_error(
            StatusCode::BAD_GATEWAY,
            "MCP discovery failed after connection initialization",
            "mcp_discovery_failed",
        )
    })?;
    let tools = conn.tools();
    let schemas = tools_to_schemas_checked(&tool_namespace, tools)
        .map_err(|error| mcp_error(StatusCode::CONFLICT, error, "mcp_public_name_conflict"))?;

    let mut discovered = Vec::with_capacity(tools.len());
    for (tool, schema) in tools.iter().zip(schemas) {
        let public_name = schema["function"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let input_schema_json = input_schema_value(tool);
        let output_schema_json = output_schema_value(tool);
        let description = tool.description.as_deref().map(str::to_string);
        let hash_parts = json!({
            "tool_name": tool.name.as_ref(),
            "public_name": public_name,
            "description": description,
            "input_schema_json": input_schema_json,
            "output_schema_json": output_schema_json,
        });
        discovered.push(McpDiscoveredToolData {
            tool_name: tool.name.to_string(),
            public_name,
            description,
            input_schema_json: Some(input_schema_value(tool)),
            output_schema_json,
            schema_hash: mcp_schema_hash(&hash_parts),
        });
    }

    Ok(discovered)
}

pub(crate) async fn prepare_request_scoped_runtime_bundle(
    bindings: &[RuntimeMcpBindingRequest],
) -> Result<Option<RuntimeMcpBundle>, (StatusCode, Json<ErrorResponse>)> {
    if bindings.is_empty() {
        return Ok(None);
    }

    let mut namespaces = HashSet::new();
    let mut configs = Vec::with_capacity(bindings.len());

    for binding in bindings {
        let config = request_scoped_server_config(binding)?;
        let tool_namespace = config.name.clone();
        if !namespaces.insert(tool_namespace.clone()) {
            return Err(mcp_error(
                StatusCode::BAD_REQUEST,
                format!(
                    "duplicate runtime MCP binding id after sanitization: {}",
                    binding.id
                ),
                "mcp_runtime_binding_invalid",
            ));
        }
        configs.push((binding, config, tool_namespace));
    }

    let mut manager = McpClientManager::new();
    let mut schemas = Vec::new();
    let mut public_names = HashSet::new();

    for (binding, config, tool_namespace) in configs {
        let secret_value = runtime_binding_secret_value(binding);
        manager.connect(config).await.map_err(|error| {
            mcp_error(
                StatusCode::BAD_GATEWAY,
                format!(
                    "MCP connection failed for runtime binding '{}': {}",
                    binding.id,
                    redact_known_secrets(&error.to_string(), &secret_value)
                ),
                "mcp_runtime_discovery_failed",
            )
        })?;

        let conn = manager.get(&tool_namespace).ok_or_else(|| {
            mcp_error(
                StatusCode::BAD_GATEWAY,
                format!(
                    "runtime MCP binding '{}' connected without a session",
                    binding.id
                ),
                "mcp_runtime_discovery_failed",
            )
        })?;

        let discovered = conn.tools();
        if discovered.is_empty() {
            return Err(mcp_error(
                StatusCode::BAD_GATEWAY,
                format!("runtime MCP binding '{}' returned no tools", binding.id),
                "mcp_runtime_discovery_failed",
            ));
        }
        let binding_schemas = tools_to_schemas_checked(&tool_namespace, discovered)
            .map_err(|error| mcp_error(StatusCode::CONFLICT, error, "mcp_public_name_conflict"))?;
        for schema in binding_schemas {
            let public_name = schema["function"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if !public_names.insert(public_name.clone()) {
                return Err(mcp_error(
                    StatusCode::BAD_GATEWAY,
                    format!("duplicate MCP public tool name: {public_name}"),
                    "mcp_public_name_conflict",
                ));
            }
            schemas.push(schema);
        }
    }

    Ok(Some(RuntimeMcpBundle {
        schemas,
        manager: Some(Arc::new(RwLock::new(manager))),
        agent_binding_mcp: None,
    }))
}

async fn post_agent_binding_mcp_rpc(
    endpoint_url: &str,
    authorization: &str,
    payload: Value,
) -> Result<Value, AgentBindingMcpRpcError> {
    let _permit = crate::capability_endpoint_pool::try_acquire_endpoint_permit(endpoint_url)
        .map_err(agent_binding_mcp_rpc_error)?;

    let response = tokio::time::timeout(
        Duration::from_secs(AGENT_BINDING_MCP_RPC_TIMEOUT_SECS),
        agent_binding_mcp_http_client()
            .post(endpoint_url)
            .header(reqwest::header::AUTHORIZATION, authorization)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .json(&payload)
            .send(),
    )
    .await
    .map_err(|_| {
        agent_binding_mcp_rpc_error(format!(
            "Agent Binding MCP RPC to '{endpoint_url}' timed out after {AGENT_BINDING_MCP_RPC_TIMEOUT_SECS}s"
        ))
    })?
    .map_err(|error| {
        agent_binding_mcp_rpc_error(format!(
            "Agent Binding MCP RPC to '{endpoint_url}' failed: {error}"
        ))
    })?;

    let status = response.status();
    let body = response.text().await.map_err(|error| {
        agent_binding_mcp_rpc_error(format!(
            "Agent Binding MCP RPC to '{endpoint_url}' failed while reading response: {error}"
        ))
    })?;

    if !status.is_success() {
        return Err(agent_binding_mcp_rpc_error(format!(
            "Agent Binding MCP RPC to '{endpoint_url}' returned HTTP {status}: {body}"
        )));
    }

    decode_agent_binding_mcp_rpc_response(&body)
}

fn decode_agent_binding_mcp_rpc_response(body: &str) -> Result<Value, AgentBindingMcpRpcError> {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        return json_rpc_result(value);
    }

    for value in sse_json_values(body) {
        match json_rpc_result(value) {
            Ok(result) => return Ok(result),
            Err(error) if error.detail.contains("JSON-RPC error") => return Err(error),
            Err(_) => continue,
        }
    }

    Err(agent_binding_mcp_rpc_error(
        "Agent Binding MCP response was neither JSON-RPC JSON nor SSE JSON-RPC data",
    ))
}

fn sse_json_values(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.trim().strip_prefix("data:"))
        .map(str::trim)
        .filter(|data| !data.is_empty() && *data != "[DONE]")
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .collect()
}

fn json_rpc_result(value: Value) -> Result<Value, AgentBindingMcpRpcError> {
    if let Some(error) = value.get("error") {
        let code = error
            .get("code")
            .map(Value::to_string)
            .unwrap_or_else(|| "unknown".to_string());
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown MCP JSON-RPC error");
        return Err(agent_binding_mcp_rpc_error(format!(
            "Agent Binding MCP JSON-RPC error {code}: {message}"
        )));
    }
    value.get("result").cloned().ok_or_else(|| {
        agent_binding_mcp_rpc_error("Agent Binding MCP JSON-RPC response missing result")
    })
}

fn parse_agent_binding_mcp_tools(
    result: Value,
) -> Result<Vec<AgentBindingMcpTool>, AgentBindingMcpRpcError> {
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            agent_binding_mcp_rpc_error("Agent Binding MCP tools/list result missing tools array")
        })?;
    let mut parsed = Vec::with_capacity(tools.len());
    for tool in tools {
        let name = tool.get("name").and_then(Value::as_str).ok_or_else(|| {
            agent_binding_mcp_rpc_error("Agent Binding MCP tool missing string name")
        })?;
        if name.is_empty() {
            return Err(agent_binding_mcp_rpc_error(
                "Agent Binding MCP tool name must not be empty",
            ));
        }
        let description = tool
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string);
        let input_schema = tool
            .get("inputSchema")
            .or_else(|| tool.get("input_schema"))
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
        if !input_schema.is_object() {
            return Err(agent_binding_mcp_rpc_error(format!(
                "Agent Binding MCP tool '{name}' inputSchema must be a JSON object"
            )));
        }
        parsed.push(AgentBindingMcpTool {
            name: name.to_string(),
            description,
            input_schema,
        });
    }
    Ok(parsed)
}

fn agent_binding_tools_to_schemas_checked(
    server_name: &str,
    tools: &[AgentBindingMcpTool],
) -> Result<Vec<Value>, String> {
    let mut seen = HashSet::new();
    let mut schemas = Vec::with_capacity(tools.len());
    for tool in tools {
        let schema = mcp_tool_schema_from_parts(
            server_name,
            &tool.name,
            tool.description.as_deref().unwrap_or(""),
            tool.input_schema.clone(),
        );
        let name = schema["function"]["name"].as_str().unwrap_or_default();
        if !seen.insert(name.to_string()) {
            return Err(format!(
                "duplicate MCP public tool name after sanitization: {name}"
            ));
        }
        schemas.push(schema);
    }
    Ok(schemas)
}

fn tool_names_by_public_name(
    schemas: &[Value],
    tools: &[AgentBindingMcpTool],
) -> HashMap<String, String> {
    schemas
        .iter()
        .zip(tools)
        .filter_map(|(schema, tool)| {
            schema["function"]["name"]
                .as_str()
                .map(|public_name| (public_name.to_string(), tool.name.clone()))
        })
        .collect()
}

fn agent_binding_tool_call_arguments(args: &Value) -> Value {
    match args {
        Value::Object(_) => args.clone(),
        Value::Null => json!({}),
        other => json!({ "input": other }),
    }
}

fn extract_agent_binding_mcp_tool_result(
    result: &Value,
) -> Result<String, AgentBindingMcpRpcError> {
    let mut parts = Vec::new();
    if let Some(content) = result.get("content").and_then(Value::as_array) {
        for item in content {
            if item.get("type").and_then(Value::as_str) == Some("text")
                && let Some(text) = item.get("text").and_then(Value::as_str)
            {
                parts.push(text.to_string());
                continue;
            }
            parts.push(item.to_string());
        }
    } else if let Some(structured) = result.get("structuredContent") {
        parts.push(structured.to_string());
    } else if !result.is_null() {
        parts.push(result.to_string());
    }

    let mut text = parts.join("\n");
    if text.len() > MAX_RESULT_CONTENT_LENGTH {
        let end = text.floor_char_boundary(MAX_RESULT_CONTENT_LENGTH);
        text = format!(
            "{}\n\n[OUTPUT TRUNCATED - exceeded {} char limit]",
            &text[..end],
            MAX_RESULT_CONTENT_LENGTH
        );
    }

    if result
        .get("isError")
        .or_else(|| result.get("is_error"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        return Err(agent_binding_mcp_rpc_error(if text.is_empty() {
            "Agent Binding MCP tool returned isError=true".to_string()
        } else {
            text
        }));
    }

    Ok(text)
}

impl AgentBindingMcpRuntime {
    pub(crate) async fn call_tool_by_mcp_name(
        &self,
        public_name: &str,
        args: &Value,
    ) -> Result<String, String> {
        let tool_name = self
            .tool_names_by_public_name
            .get(public_name)
            .ok_or_else(|| format!("Tool not found: {public_name}"))?;
        let payload = json!({
            "jsonrpc": "2.0",
            "id": "astra-agent-binding-tools-call",
            "method": "tools/call",
            "params": {
                "name": tool_name,
                "arguments": agent_binding_tool_call_arguments(args),
            }
        });
        let result = post_agent_binding_mcp_rpc(&self.endpoint_url, &self.authorization, payload)
            .await
            .map_err(|error| {
                redact_known_secrets(
                    &error.detail,
                    &json!({
                        "authorization": &self.authorization,
                        "url": &self.endpoint_url,
                    }),
                )
            })?;
        extract_agent_binding_mcp_tool_result(&result).map_err(|error| {
            redact_known_secrets(
                &error.detail,
                &json!({
                    "authorization": &self.authorization,
                    "url": &self.endpoint_url,
                }),
            )
        })
    }

    pub(crate) fn server_name(&self) -> &str {
        &self.server_name
    }
}

pub(crate) async fn prepare_agent_binding_mcp_bundle(
    server_id: &str,
    endpoint_url: &str,
    authorization: &str,
) -> Result<RuntimeMcpBundle, (StatusCode, Json<ErrorResponse>)> {
    let tool_namespace = sanitize_tool_name(server_id);
    if tool_namespace.is_empty() {
        return Err(mcp_error(
            StatusCode::BAD_REQUEST,
            "agent binding MCP server id must not be empty after sanitization",
            "agent_binding_capability_ref_invalid",
        ));
    }
    astra_services::validate_registered_endpoint_url(
        "agent_binding.capability_server.endpoint_url",
        endpoint_url,
        "agent_binding_capability_ref_invalid",
    )?;
    let secret_value = json!({
        "authorization": authorization,
        "url": endpoint_url,
    });
    let result = post_agent_binding_mcp_rpc(
        endpoint_url,
        authorization,
        json!({
            "jsonrpc": "2.0",
            "id": "astra-agent-binding-tools-list",
            "method": "tools/list",
            "params": {}
        }),
    )
    .await
    .map_err(|error| {
        agent_binding_mcp_error_response(
            StatusCode::BAD_GATEWAY,
            agent_binding_mcp_rpc_error(format!(
                "Agent Binding MCP discovery failed for server '{}': {}",
                server_id,
                redact_known_secrets(&error.detail, &secret_value)
            )),
            "agent_binding_discovery_failed",
        )
    })?;
    let tools = parse_agent_binding_mcp_tools(result).map_err(|error| {
        agent_binding_mcp_error_response(
            StatusCode::BAD_GATEWAY,
            agent_binding_mcp_rpc_error(format!(
                "Agent Binding MCP discovery failed for server '{}': {}",
                server_id,
                redact_known_secrets(&error.detail, &secret_value)
            )),
            "agent_binding_discovery_failed",
        )
    })?;
    let schemas =
        agent_binding_tools_to_schemas_checked(&tool_namespace, &tools).map_err(|error| {
            mcp_error(
                StatusCode::BAD_GATEWAY,
                error,
                "agent_binding_schema_invalid",
            )
        })?;
    let tool_names_by_public_name = tool_names_by_public_name(&schemas, &tools);
    let agent_binding_mcp = AgentBindingMcpRuntime {
        server_name: tool_namespace,
        endpoint_url: endpoint_url.to_string(),
        authorization: authorization.to_string(),
        tool_names_by_public_name: Arc::new(tool_names_by_public_name),
    };
    Ok(RuntimeMcpBundle {
        schemas,
        manager: None,
        agent_binding_mcp: Some(Arc::new(agent_binding_mcp)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_bearer_and_token_fragments() {
        let raw = "upstream said Authorization: Bearer abc.def token=secret";
        let redacted = redact_mcp_error_text(raw);
        assert!(!redacted.contains("abc.def"));
        assert!(!redacted.contains("secret"));
        assert!(redacted.contains("[REDACTED]"));
    }

    #[test]
    fn runtime_binding_secret_value_redacts_secret_url() {
        let binding = RuntimeMcpBindingRequest {
            id: "external_nl2sql".to_string(),
            transport: "streamable_http".to_string(),
            url: "http://tool-server/mcp/http?token=url-secret".to_string(),
            auth_token: None,
            headers: HashMap::new(),
        };
        let secret_value = runtime_binding_secret_value(&binding);
        let redacted = redact_known_secrets(
            "upstream failed at http://tool-server/mcp/http?token=url-secret",
            &secret_value,
        );

        assert!(!redacted.contains("url-secret"));
        assert!(redacted.contains("[REDACTED]"));
    }

    #[test]
    fn redact_known_secrets_redacts_short_values_only_for_secret_fields() {
        let redacted = redact_known_secrets(
            "upstream echoed abc but non-secret x remains",
            &json!({
                "token": "abc",
                "url": "x",
            }),
        );

        assert!(!redacted.contains("abc"));
        assert!(redacted.contains("non-secret x remains"));
        assert!(redacted.contains("[REDACTED]"));
    }

    #[test]
    fn request_scoped_server_config_preserves_headers_and_sanitizes_namespace() {
        let binding = RuntimeMcpBindingRequest {
            id: "external nl2sql".to_string(),
            transport: "streamable-http".to_string(),
            url: "https://tools.example.test/mcp/http".to_string(),
            auth_token: Some("token-value".to_string()),
            headers: HashMap::from([(
                "Authorization".to_string(),
                "Bearer runtime-grant".to_string(),
            )]),
        };

        let config = request_scoped_server_config(&binding).expect("valid runtime binding");

        assert_eq!(config.name, "external_nl2sql");
        match config.transport {
            Transport::StreamableHttp {
                url,
                auth_token,
                headers,
            } => {
                assert_eq!(url, "https://tools.example.test/mcp/http");
                assert_eq!(auth_token.as_deref(), Some("token-value"));
                assert_eq!(
                    headers.get("Authorization").map(String::as_str),
                    Some("Bearer runtime-grant")
                );
            }
            other => panic!("unexpected transport: {other:?}"),
        }
    }

    #[test]
    fn request_scoped_server_config_reports_runtime_validation_errors() {
        let invalid_url = RuntimeMcpBindingRequest {
            id: "external_nl2sql".to_string(),
            transport: "streamable_http".to_string(),
            url: "file:///tmp/mcp.sock".to_string(),
            auth_token: None,
            headers: HashMap::new(),
        };
        let err = match request_scoped_server_config(&invalid_url) {
            Ok(_) => panic!("url must be rejected"),
            Err(err) => err,
        };
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            err.1.0.error_code.as_deref(),
            Some("mcp_runtime_binding_invalid")
        );

        let invalid_transport = RuntimeMcpBindingRequest {
            transport: "stdio".to_string(),
            url: "http://127.0.0.1/mcp".to_string(),
            ..invalid_url
        };
        let err = match request_scoped_server_config(&invalid_transport) {
            Ok(_) => panic!("transport must be rejected"),
            Err(err) => err,
        };
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            err.1.0.error_code.as_deref(),
            Some("mcp_runtime_binding_invalid")
        );
    }

    #[tokio::test]
    async fn request_scoped_runtime_bundle_rejects_duplicate_ids_before_connecting() {
        let bindings = vec![
            RuntimeMcpBindingRequest {
                id: "external nl2sql".to_string(),
                transport: "streamable_http".to_string(),
                url: "http://127.0.0.1:1/mcp".to_string(),
                auth_token: None,
                headers: HashMap::new(),
            },
            RuntimeMcpBindingRequest {
                id: "external/nl2sql".to_string(),
                transport: "streamable_http".to_string(),
                url: "http://127.0.0.1:1/mcp".to_string(),
                auth_token: None,
                headers: HashMap::new(),
            },
        ];

        let err = match prepare_request_scoped_runtime_bundle(&bindings).await {
            Ok(_) => panic!("duplicate sanitized binding ids should be rejected"),
            Err(err) => err,
        };

        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            err.1.0.error_code.as_deref(),
            Some("mcp_runtime_binding_invalid")
        );
    }

    #[tokio::test]
    async fn agent_binding_mcp_runtime_uses_stateless_per_call_authorization() {
        use axum::{Router, http::HeaderMap, routing::post};
        use std::sync::Mutex;

        let calls = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let calls_for_handler = calls.clone();
        let app = Router::new().route(
            "/mcp",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let calls = calls_for_handler.clone();
                async move {
                    let authorization = headers
                        .get(reqwest::header::AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    let method = body
                        .get("method")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    calls
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push((authorization, method.clone()));

                    let response = match method.as_str() {
                        "tools/list" => json!({
                            "jsonrpc": "2.0",
                            "id": body.get("id").cloned().unwrap_or(Value::Null),
                            "result": {
                                "tools": [{
                                    "name": "query",
                                    "description": "Query data",
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": {
                                            "q": {"type": "string"}
                                        }
                                    }
                                }]
                            }
                        }),
                        "tools/call" => json!({
                            "jsonrpc": "2.0",
                            "id": body.get("id").cloned().unwrap_or(Value::Null),
                            "result": {
                                "content": [{
                                    "type": "text",
                                    "text": "query-result"
                                }]
                            }
                        }),
                        other => json!({
                            "jsonrpc": "2.0",
                            "id": body.get("id").cloned().unwrap_or(Value::Null),
                            "error": {
                                "code": -32601,
                                "message": format!("unknown method {other}")
                            }
                        }),
                    };
                    Json(response)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let endpoint = format!("http://{addr}/mcp");

        let bundle = prepare_agent_binding_mcp_bundle("tools", &endpoint, "Bearer runtime-grant")
            .await
            .expect("agent binding MCP discovery should succeed");

        assert!(bundle.manager.is_none());
        assert!(bundle.agent_binding_mcp.is_some());
        assert_eq!(bundle.schemas[0]["function"]["name"], "mcp__tools__query");

        let output = bundle
            .agent_binding_mcp
            .as_ref()
            .unwrap()
            .call_tool_by_mcp_name("mcp__tools__query", &json!({"q": "hello"}))
            .await
            .expect("tool call should succeed");
        assert_eq!(output, "query-result");

        let calls = calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(
            *calls,
            vec![
                ("Bearer runtime-grant".to_string(), "tools/list".to_string()),
                ("Bearer runtime-grant".to_string(), "tools/call".to_string()),
            ]
        );
        server.abort();
    }

    #[tokio::test]
    async fn agent_binding_mcp_discovery_allows_empty_tool_list() {
        use axum::{Router, routing::post};

        let app = Router::new().route(
            "/mcp",
            post(|Json(body): Json<Value>| async move {
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": body.get("id").cloned().unwrap_or(Value::Null),
                    "result": {
                        "tools": []
                    }
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let endpoint = format!("http://{addr}/mcp");

        let bundle = prepare_agent_binding_mcp_bundle("tools", &endpoint, "Bearer runtime-grant")
            .await
            .expect("empty Agent Binding MCP discovery should be allowed");

        assert!(bundle.schemas.is_empty());
        assert!(bundle.manager.is_none());
        assert!(bundle.agent_binding_mcp.is_some());
        server.abort();
    }

    #[tokio::test]
    async fn agent_binding_mcp_revalidates_registered_endpoint_strictly() {
        for endpoint_url in [
            "ws://capabilities.example.test/mcp",
            "https://user:pass@capabilities.example.test/mcp",
            "https://capabilities.example.test/mcp?token=secret",
            "https://capabilities.example.test/mcp#fragment",
        ] {
            let err = match prepare_agent_binding_mcp_bundle(
                "tools",
                endpoint_url,
                "Bearer runtime-grant",
            )
            .await
            {
                Ok(_) => panic!(
                    "invalid registered capability endpoint must fail before discovery: {endpoint_url}"
                ),
                Err(err) => err,
            };
            assert_eq!(err.0, StatusCode::BAD_REQUEST, "{endpoint_url}");
            assert_eq!(
                err.1.0.error_code.as_deref(),
                Some("agent_binding_capability_ref_invalid"),
                "{endpoint_url}"
            );
        }
    }
}
