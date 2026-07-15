//! Request-scoped runtime MCP wiring for server-side agent loops.
//!
//! Chat requests may provide MCP server endpoints with opaque credentials for
//! the current turn. The runtime discovers tools for that request and keeps the
//! resulting schemas and connections in memory only.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use astra_core::{ErrorResponse, error_response_coded};
use astra_mcp::{
    MAX_RESULT_CONTENT_LENGTH, McpClientManager, McpServerConfig, McpTool, McpToolCallResult,
    Transport, mcp_resolved_provider_snapshot_to_schemas_checked, mcp_tool_to_provider_declaration,
    mcp_tool_to_schema, mcp_tools_to_provider_snapshot, sanitize_tool_name,
};
use astra_services::{
    McpDiscoveredToolData, McpRegisterRequestData, mcp_binding_tool_namespace, mcp_schema_hash,
    runs::RuntimeMcpBindingRequest,
};
use astra_turn_types::{
    NativeToolId, ProviderBindingRef, ProviderClaimTrust, ProviderDiscoverySnapshot,
    ProviderIdentity, ProviderProtocolId, PublicToolAlias, ResolvedProviderSnapshot,
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
    pub provider_snapshots: Vec<ResolvedProviderSnapshot>,
    pub control_tools: crate::turn::terminal_control::RuntimeControlToolSnapshot,
    pub stop_after_success_tools: crate::turn::tool_completion::RuntimeStopAfterSuccessToolSnapshot,
    pub manager: Option<Arc<RwLock<McpClientManager>>>,
    pub agent_binding_mcp: Option<Arc<AgentBindingMcpRuntime>>,
}

pub(crate) fn resolve_mcp_snapshot(
    tool_namespace: &str,
    discovery: &ProviderDiscoverySnapshot,
) -> Result<ResolvedProviderSnapshot, String> {
    let aliases = discovery
        .tool_declarations
        .iter()
        .map(|declaration| {
            let alias = sanitize_tool_name(&format!(
                "mcp__{}__{}",
                tool_namespace, declaration.native_tool_name
            ));
            Ok((
                declaration.native_tool_id.clone(),
                PublicToolAlias::new(alias).map_err(|error| error.to_string())?,
            ))
        })
        .collect::<Result<BTreeMap<NativeToolId, PublicToolAlias>, String>>()?;
    // MCP annotations are protocol-defined hints, not authorization. Keep
    // them visible as advisory evidence until binding/deployment policy grants
    // stronger authority explicitly.
    let trust_policy = astra_turn_core::provider_resolution::ProviderClaimTrustPolicy {
        standard_protocols: BTreeMap::from([("mcp".to_string(), ProviderClaimTrust::Advisory)]),
        ..Default::default()
    };
    astra_turn_core::provider_resolution::resolve_provider_snapshot(
        discovery,
        &trust_policy,
        &aliases,
    )
    .map_err(|error| error.to_string())
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
    tool: McpTool,
    metadata: Option<Value>,
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
            "agent binding MCP HTTP client",
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

pub(crate) async fn discover_binding_tools(
    binding_id: &str,
    request: &McpRegisterRequestData,
) -> Result<Vec<McpDiscoveredToolData>, (StatusCode, Json<ErrorResponse>)> {
    if binding_id.trim().is_empty() {
        return Err(mcp_error(
            StatusCode::BAD_REQUEST,
            "binding_id must not be empty",
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
    let provider_snapshot = mcp_tools_to_provider_snapshot(
        ProviderIdentity::new(binding_id.to_string()).map_err(|error| {
            mcp_error(
                StatusCode::BAD_REQUEST,
                error.to_string(),
                "mcp_binding_invalid",
            )
        })?,
        ProviderBindingRef::new(binding_id.to_string()).map_err(|error| {
            mcp_error(
                StatusCode::BAD_REQUEST,
                error.to_string(),
                "mcp_binding_invalid",
            )
        })?,
        tools,
    )
    .map_err(|error| {
        mcp_error(
            StatusCode::BAD_GATEWAY,
            format!("invalid MCP discovery for binding '{binding_id}': {error}"),
            "mcp_discovery_invalid",
        )
    })?;
    let resolved_snapshot =
        resolve_mcp_snapshot(&tool_namespace, &provider_snapshot).map_err(|error| {
            mcp_error(
                StatusCode::CONFLICT,
                error,
                "mcp_provider_resolution_failed",
            )
        })?;
    let schemas = mcp_resolved_provider_snapshot_to_schemas_checked(&resolved_snapshot)
        .map_err(|error| mcp_error(StatusCode::CONFLICT, error, "mcp_public_name_conflict"))?;

    let mut discovered = Vec::with_capacity(resolved_snapshot.descriptors.len());
    for schema in schemas {
        let public_name = schema["function"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let alias = PublicToolAlias::new(public_name.clone()).map_err(|error| {
            mcp_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                error.to_string(),
                "mcp_provider_projection_invalid",
            )
        })?;
        let descriptor_ref = resolved_snapshot.alias_index.get(&alias).ok_or_else(|| {
            mcp_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("MCP schema alias '{public_name}' has no resolved descriptor"),
                "mcp_provider_projection_invalid",
            )
        })?;
        let descriptor = resolved_snapshot
            .descriptors
            .iter()
            .find(|descriptor| descriptor.descriptor_ref() == *descriptor_ref)
            .ok_or_else(|| {
                mcp_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("MCP schema alias '{public_name}' references a missing descriptor"),
                    "mcp_provider_projection_invalid",
                )
            })?;
        let hash_parts = json!({
            "descriptor_version": descriptor.descriptor_version.as_str(),
            "public_name": public_name,
        });
        discovered.push(McpDiscoveredToolData {
            tool_name: descriptor.native_tool_name.clone(),
            public_name,
            description: descriptor.description.clone(),
            input_schema_json: Some(descriptor.input_schema.clone()),
            output_schema_json: descriptor.output_schema.clone(),
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
    let mut provider_snapshots = Vec::with_capacity(bindings.len());
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
        let provider_snapshot = mcp_tools_to_provider_snapshot(
            ProviderIdentity::new(binding.id.clone()).map_err(|error| {
                mcp_error(
                    StatusCode::BAD_REQUEST,
                    error.to_string(),
                    "mcp_runtime_binding_invalid",
                )
            })?,
            ProviderBindingRef::new(binding.id.clone()).map_err(|error| {
                mcp_error(
                    StatusCode::BAD_REQUEST,
                    error.to_string(),
                    "mcp_runtime_binding_invalid",
                )
            })?,
            discovered,
        )
        .map_err(|error| {
            mcp_error(
                StatusCode::BAD_GATEWAY,
                format!(
                    "invalid MCP discovery for runtime binding '{}': {error}",
                    binding.id
                ),
                "mcp_runtime_discovery_invalid",
            )
        })?;
        let resolved_snapshot =
            resolve_mcp_snapshot(&tool_namespace, &provider_snapshot).map_err(|error| {
                mcp_error(
                    StatusCode::CONFLICT,
                    error,
                    "mcp_provider_resolution_failed",
                )
            })?;
        let binding_schemas = mcp_resolved_provider_snapshot_to_schemas_checked(&resolved_snapshot)
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
        provider_snapshots.push(resolved_snapshot);
    }

    Ok(Some(RuntimeMcpBundle {
        schemas,
        provider_snapshots,
        control_tools: Default::default(),
        stop_after_success_tools: Default::default(),
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
    for raw_tool in tools {
        let mut normalized = raw_tool.as_object().cloned().ok_or_else(|| {
            agent_binding_mcp_rpc_error("Agent Binding MCP tool declaration must be an object")
        })?;
        if !normalized.contains_key("inputSchema")
            && let Some(input_schema) = normalized.get("input_schema").cloned()
        {
            normalized.insert("inputSchema".to_string(), input_schema);
        }
        if !normalized.contains_key("outputSchema")
            && let Some(output_schema) = normalized.get("output_schema").cloned()
        {
            normalized.insert("outputSchema".to_string(), output_schema);
        }
        let metadata = normalized.get("metadata").cloned();
        let tool =
            serde_json::from_value::<McpTool>(Value::Object(normalized)).map_err(|error| {
                agent_binding_mcp_rpc_error(format!(
                    "Agent Binding MCP tool declaration is invalid: {error}"
                ))
            })?;
        if tool.name.trim().is_empty() {
            return Err(agent_binding_mcp_rpc_error(
                "Agent Binding MCP tool name must not be empty",
            ));
        }
        parsed.push(AgentBindingMcpTool { tool, metadata });
    }
    Ok(parsed)
}

fn agent_binding_tools_to_schemas_checked(
    server_name: &str,
    tools: &[AgentBindingMcpTool],
) -> Result<
    (
        Vec<Value>,
        crate::turn::terminal_control::RuntimeControlToolSnapshot,
        crate::turn::tool_completion::RuntimeStopAfterSuccessToolSnapshot,
    ),
    (String, &'static str),
> {
    let mut seen = HashSet::new();
    let mut schemas = Vec::with_capacity(tools.len());
    let mut control_tools = Vec::new();
    let mut stop_after_success_tools = Vec::new();
    for tool in tools {
        let schema = mcp_tool_to_schema(server_name, &tool.tool);
        let name = schema["function"]["name"].as_str().unwrap_or_default();
        if !seen.insert(name.to_string()) {
            return Err((
                format!("duplicate MCP public tool name after sanitization: {name}"),
                "mcp_public_name_conflict",
            ));
        }
        if let Some(descriptor) =
            crate::turn::terminal_control::RuntimeControlToolDescriptor::from_metadata(
                name,
                tool.metadata.as_ref(),
            )
            .map_err(|error| (error.to_string(), error.error_code()))?
        {
            control_tools.push(descriptor);
        }
        if crate::turn::tool_completion::stop_after_success_from_metadata(
            name,
            tool.metadata.as_ref(),
        )
        .map_err(|error| (error.to_string(), error.error_code()))?
        {
            stop_after_success_tools.push(name.to_string());
        }
        schemas.push(schema);
    }
    Ok((
        schemas,
        crate::turn::terminal_control::RuntimeControlToolSnapshot::new(control_tools),
        crate::turn::tool_completion::RuntimeStopAfterSuccessToolSnapshot::new(
            stop_after_success_tools,
        ),
    ))
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
                .map(|public_name| (public_name.to_string(), tool.tool.name.to_string()))
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
) -> Result<McpToolCallResult, AgentBindingMcpRpcError> {
    let mut parts = Vec::new();
    let structured_content = result.get("structuredContent").cloned();
    let protocol_metadata = result.get("_meta").cloned();
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
    } else if let Some(structured) = structured_content.as_ref() {
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

    let is_error = result
        .get("isError")
        .or_else(|| result.get("is_error"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    Ok(McpToolCallResult {
        output: text,
        structured_content,
        protocol_metadata,
        is_error,
    })
}

impl AgentBindingMcpRuntime {
    #[cfg(test)]
    pub(crate) fn for_tests(server_name: &str, public_tool_names: &[&str]) -> Self {
        let tool_names_by_public_name = public_tool_names
            .iter()
            .map(|name| ((*name).to_string(), (*name).to_string()))
            .collect();
        Self {
            server_name: server_name.to_string(),
            endpoint_url: "http://127.0.0.1:1/mcp".to_string(),
            authorization: "Bearer test".to_string(),
            tool_names_by_public_name: Arc::new(tool_names_by_public_name),
        }
    }

    pub(crate) fn owns_public_tool_name(&self, public_name: &str) -> bool {
        self.tool_names_by_public_name.contains_key(public_name)
    }

    pub(crate) async fn call_tool_by_mcp_name(
        &self,
        public_name: &str,
        args: &Value,
    ) -> Result<McpToolCallResult, String> {
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
    let (_adapter_schemas, control_tools, stop_after_success_tools) =
        agent_binding_tools_to_schemas_checked(&tool_namespace, &tools)
            .map_err(|(error, code)| mcp_error(StatusCode::BAD_GATEWAY, error, code))?;
    let declarations = tools
        .iter()
        .map(|tool| {
            let mut declaration = mcp_tool_to_provider_declaration(&tool.tool)?;
            if let Some(metadata) = &tool.metadata {
                declaration
                    .extension_fields
                    .insert("astra.agent_binding.metadata".to_string(), metadata.clone());
            }
            Ok(declaration)
        })
        .collect::<Result<Vec<_>, astra_turn_types::ProviderContractError>>()
        .map_err(|error| {
            mcp_error(
                StatusCode::BAD_GATEWAY,
                format!("invalid Agent Binding MCP discovery: {error}"),
                "agent_binding_discovery_invalid",
            )
        })?;
    let provider_snapshot = ProviderDiscoverySnapshot::new(
        ProviderIdentity::new(server_id.to_string()).map_err(|error| {
            mcp_error(
                StatusCode::BAD_REQUEST,
                error.to_string(),
                "agent_binding_capability_ref_invalid",
            )
        })?,
        ProviderBindingRef::new(server_id.to_string()).map_err(|error| {
            mcp_error(
                StatusCode::BAD_REQUEST,
                error.to_string(),
                "agent_binding_capability_ref_invalid",
            )
        })?,
        ProviderProtocolId::new("mcp").map_err(|error| {
            mcp_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                error.to_string(),
                "provider_contract_invalid",
            )
        })?,
        declarations,
    )
    .map_err(|error| {
        mcp_error(
            StatusCode::BAD_GATEWAY,
            format!("invalid Agent Binding MCP discovery: {error}"),
            "agent_binding_discovery_invalid",
        )
    })?;
    let resolved_snapshot =
        resolve_mcp_snapshot(&tool_namespace, &provider_snapshot).map_err(|error| {
            mcp_error(
                StatusCode::BAD_GATEWAY,
                format!("invalid Agent Binding MCP provider resolution: {error}"),
                "agent_binding_provider_resolution_failed",
            )
        })?;
    let schemas =
        mcp_resolved_provider_snapshot_to_schemas_checked(&resolved_snapshot).map_err(|error| {
            mcp_error(
                StatusCode::BAD_GATEWAY,
                error,
                "agent_binding_public_name_conflict",
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
        provider_snapshots: vec![resolved_snapshot],
        control_tools,
        stop_after_success_tools,
        manager: None,
        agent_binding_mcp: Some(Arc::new(agent_binding_mcp)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_binding_discovery_preserves_terminal_control_metadata_by_public_name() {
        let tools = parse_agent_binding_mcp_tools(json!({
            "tools": [{
                "name": "arbitrary-provider-name",
                "description": "Transfer runtime control",
                "input_schema": {
                    "type": "object",
                    "properties": {"action": {"type": "string"}},
                    "required": ["action"]
                },
                "metadata": {
                    "stop_after_success": true,
                    "control": {
                        "kind": "moi.control.handoff.v1",
                        "terminal": true,
                        "policy_id": "authoring.handoff.v1",
                        "ui_visibility": "hidden"
                    }
                }
            }]
        }))
        .expect("valid discovery response");

        let (schemas, control_tools, stop_after_success_tools) =
            agent_binding_tools_to_schemas_checked("provider", &tools).expect("valid schemas");
        let public_name = schemas[0]["function"]["name"]
            .as_str()
            .expect("public tool name");
        let descriptor = control_tools
            .descriptor(public_name)
            .expect("control descriptor must be indexed by public name");

        assert_eq!(public_name, "mcp__provider__arbitrary-provider-name");
        assert_eq!(descriptor.kind, "moi.control.handoff.v1");
        assert!(descriptor.terminal);
        assert_eq!(descriptor.policy_id, "authoring.handoff.v1");
        assert_eq!(
            stop_after_success_tools
                .successful_tool_name(
                    &[astra_services::session_journal::ToolCallRecord {
                        name: public_name.to_string(),
                        ok: true,
                        tool_call_id: Some("call-1".to_string()),
                        ..Default::default()
                    }],
                    &[json!({
                        "tool_call_id": "call-1",
                        "structuredContent": {"output": {"ok": true}}
                    })],
                )
                .as_deref(),
            Some(public_name)
        );
    }

    #[test]
    fn unsupported_terminal_control_kind_fails_before_entering_tool_surface() {
        let tools = parse_agent_binding_mcp_tools(json!({
            "tools": [{
                "name": "handoff",
                "inputSchema": {"type": "object"},
                "metadata": {
                    "control": {
                        "kind": "moi.control.handoff.v2",
                        "terminal": true,
                        "policy_id": "authoring.handoff.v2"
                    }
                }
            }]
        }))
        .expect("discovery parsing should not validate runtime semantics");

        let (message, code) =
            agent_binding_tools_to_schemas_checked("provider", &tools).unwrap_err();

        assert_eq!(code, "terminal_handoff_unsupported");
        assert!(message.contains("unsupported kind"));
    }

    #[test]
    fn invalid_stop_after_success_metadata_fails_before_entering_tool_surface() {
        let tools = parse_agent_binding_mcp_tools(json!({
            "tools": [{
                "name": "agent_builder",
                "inputSchema": {"type": "object"},
                "metadata": {"stop_after_success": "true"}
            }]
        }))
        .expect("discovery parsing should not validate runtime semantics");

        let (message, code) =
            agent_binding_tools_to_schemas_checked("provider", &tools).unwrap_err();

        assert_eq!(code, "stop_after_success_contract_violation");
        assert!(message.contains("stop_after_success"));
    }

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
                                }],
                                "structuredContent": {
                                    "artifacts": [{
                                        "artifact_id": "artifact_file_1",
                                        "type": "file",
                                        "data": {"file_id": "file_1"}
                                    }]
                                }
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
        assert_eq!(bundle.provider_snapshots.len(), 1);
        let snapshot = &bundle.provider_snapshots[0];
        let descriptor = &snapshot.descriptors[0];
        assert_eq!(descriptor.identity.native_tool_id.as_str(), "query");
        assert_eq!(
            descriptor.semantic_baseline.effect,
            astra_turn_types::ResolvedToolEffect::Unknown,
            "standard MCP hints are advisory and missing hints must remain conservative"
        );
        assert_eq!(
            snapshot
                .alias_index
                .get(&astra_turn_types::PublicToolAlias::new("mcp__tools__query").unwrap()),
            Some(&descriptor.descriptor_ref()),
            "the model-visible schema alias must resolve to the exact carried descriptor"
        );

        let output = bundle
            .agent_binding_mcp
            .as_ref()
            .unwrap()
            .call_tool_by_mcp_name("mcp__tools__query", &json!({"q": "hello"}))
            .await
            .expect("tool call should succeed");
        assert_eq!(output.output, "query-result");
        assert_eq!(
            output
                .structured_content
                .as_ref()
                .and_then(|value| value.get("artifacts"))
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );

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
        assert_eq!(bundle.provider_snapshots.len(), 1);
        assert!(bundle.provider_snapshots[0].descriptors.is_empty());
        assert!(bundle.control_tools.is_empty());
        assert!(bundle.manager.is_none());
        assert!(bundle.agent_binding_mcp.is_some());
        server.abort();
    }

    #[test]
    fn agent_binding_acknowledged_tool_error_remains_a_typed_result() {
        let result = extract_agent_binding_mcp_tool_result(&json!({
            "content": [{"type": "text", "text": "ok"}],
            "structuredContent": {"errorCode": "REJECTED"},
            "isError": true,
            "_meta": {"requestId": "request-1"}
        }))
        .expect("acknowledged tool errors are valid MCP results");

        assert!(result.is_error);
        assert_eq!(result.output, "ok");
        assert_eq!(
            result.protocol_metadata,
            Some(json!({"requestId": "request-1"}))
        );
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
