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
    NativeToolId, ProviderBindingRef, ProviderClaim, ProviderClaimSource, ProviderClaimTrust,
    ProviderDiscoverySnapshot, ProviderIdentity, ProviderProtocolId, ProviderSemanticCacheContract,
    PublicToolAlias, ResolvedProviderSnapshot, SemanticFreshnessFact, SemanticFreshnessScope,
};
use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::RwLock;

const AGENT_BINDING_MCP_RPC_TIMEOUT_SECS: u64 = 120;
const AGENT_BINDING_SEMANTIC_READ_PREPARE_TIMEOUT: Duration = Duration::from_secs(2);
const AGENT_BINDING_SEMANTIC_READ_CONTRACT_VERSION: &str =
    astra_services::runs::RUNTIME_SEMANTIC_READ_MCP_CONTRACT_VERSION;
const AGENT_BINDING_SEMANTIC_READ_PREPARE_METHOD: &str = "astra/semantic-read/prepare";
const AGENT_BINDING_SEMANTIC_READ_COMPONENT: &str = "provider_runtime_descriptor.semantic_read";
const AGENT_BINDING_SEMANTIC_READ_CONDITION_METADATA_KEY: &str = "astra.semantic_read_condition";
const AGENT_BINDING_EFFECT_EXTENSION_NAMESPACE: &str = "astra.agent_binding.effect";
const AGENT_BINDING_EFFECT_EXTENSION_FIELD: &str = "side_effect_class";

static AGENT_BINDING_MCP_HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

#[derive(Clone)]
pub(crate) struct RuntimeMcpBundle {
    pub schemas: Vec<Value>,
    pub provider_snapshots: Vec<ResolvedProviderSnapshot>,
    pub provider_policy_index: astra_turn_core::provider_resolution::ResolvedProviderPolicyIndex,
    pub control_tools: crate::turn::terminal_control::RuntimeControlToolSnapshot,
    pub stop_after_success_tools: crate::turn::tool_completion::RuntimeStopAfterSuccessToolSnapshot,
    pub manager: Option<Arc<RwLock<McpClientManager>>>,
    pub agent_binding_mcp: Option<Arc<AgentBindingMcpRuntime>>,
    pub semantic_read_capabilities:
        crate::server::semantic_read_freshness::ProviderSemanticFreshnessRegistry,
}

impl RuntimeMcpBundle {
    pub(crate) fn configure_semantic_read_cache(
        &self,
        executor: &mut crate::server::runtime_tool_executor::RuntimeToolExecutor,
    ) -> Result<crate::server::runtime_tool_executor::SemanticReadCacheActivation, String> {
        executor.configure_semantic_read_cache(
            self.semantic_read_capabilities.clone(),
            astra_turn_types::SemanticReadCacheLimits::default(),
        )
    }
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
    resolve_mcp_snapshot_with_policy(discovery, aliases, trust_policy)
}

fn resolve_agent_binding_mcp_snapshot(
    tool_namespace: &str,
    discovery: &ProviderDiscoverySnapshot,
    semantic_read_enabled: bool,
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
    let mut trust_policy = astra_turn_core::provider_resolution::ProviderClaimTrustPolicy {
        standard_protocols: BTreeMap::from([("mcp".to_string(), ProviderClaimTrust::Advisory)]),
        ..Default::default()
    };
    if semantic_read_enabled {
        trust_policy.astra_components.insert(
            AGENT_BINDING_SEMANTIC_READ_COMPONENT.to_string(),
            ProviderClaimTrust::Trusted,
        );
    }
    trust_policy.provider_extensions.insert(
        AGENT_BINDING_EFFECT_EXTENSION_NAMESPACE.to_string(),
        ProviderClaimTrust::Trusted,
    );
    resolve_mcp_snapshot_with_policy(discovery, aliases, trust_policy)
}

fn resolve_mcp_snapshot_with_policy(
    discovery: &ProviderDiscoverySnapshot,
    aliases: BTreeMap<NativeToolId, PublicToolAlias>,
    trust_policy: astra_turn_core::provider_resolution::ProviderClaimTrustPolicy,
) -> Result<ResolvedProviderSnapshot, String> {
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
    semantic_read_tools: Arc<HashSet<String>>,
}

#[derive(Clone, Debug, PartialEq)]
struct AgentBindingMcpTool {
    tool: McpTool,
    side_effect_class: Option<AgentBindingSideEffectClass>,
    metadata: Option<Value>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentBindingSideEffectClass {
    Read,
    Write,
    ExternalEffect,
}

impl AgentBindingSideEffectClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::ExternalEffect => "external_effect",
        }
    }
}

fn parse_agent_binding_side_effect_class(
    tool_name: &str,
    value: Option<&Value>,
) -> Result<Option<AgentBindingSideEffectClass>, AgentBindingMcpRpcError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value == "read" => {
            Ok(Some(AgentBindingSideEffectClass::Read))
        }
        Some(Value::String(value)) if value == "write" => {
            Ok(Some(AgentBindingSideEffectClass::Write))
        }
        Some(Value::String(value)) if value == "external_effect" => {
            Ok(Some(AgentBindingSideEffectClass::ExternalEffect))
        }
        Some(Value::String(value)) => Err(agent_binding_mcp_rpc_error(format!(
            "Agent Binding MCP tool '{tool_name}' has unsupported side_effect_class '{value}'"
        ))),
        Some(_) => Err(agent_binding_mcp_rpc_error(format!(
            "Agent Binding MCP tool '{tool_name}' side_effect_class must be a string"
        ))),
    }
}

fn apply_agent_binding_side_effect_contract(
    declaration: &mut astra_turn_types::ProviderToolDeclaration,
    side_effect_class: AgentBindingSideEffectClass,
) {
    let source = || ProviderClaimSource::ProviderExtension {
        namespace: AGENT_BINDING_EFFECT_EXTENSION_NAMESPACE.to_string(),
        field: AGENT_BINDING_EFFECT_EXTENSION_FIELD.to_string(),
    };
    match side_effect_class {
        AgentBindingSideEffectClass::Read => {
            declaration.claims.read_only = Some(ProviderClaim::new(true, source()));
            declaration.claims.destructive = Some(ProviderClaim::new(false, source()));
            declaration.claims.idempotent = Some(ProviderClaim::new(true, source()));
        }
        AgentBindingSideEffectClass::Write => {
            declaration.claims.read_only = Some(ProviderClaim::new(false, source()));
            declaration.claims.idempotent = Some(ProviderClaim::new(false, source()));
        }
        AgentBindingSideEffectClass::ExternalEffect => {
            declaration.claims.read_only = Some(ProviderClaim::new(false, source()));
            declaration.claims.idempotent = Some(ProviderClaim::new(false, source()));
            declaration.claims.open_world = Some(ProviderClaim::new(true, source()));
        }
    }
    declaration.extension_fields.insert(
        AGENT_BINDING_EFFECT_EXTENSION_NAMESPACE.to_string(),
        json!({
            (AGENT_BINDING_EFFECT_EXTENSION_FIELD): side_effect_class.as_str(),
        }),
    );
}

fn agent_binding_tools_to_provider_declarations(
    tools: &[AgentBindingMcpTool],
) -> Result<Vec<astra_turn_types::ProviderToolDeclaration>, astra_turn_types::ProviderContractError>
{
    tools
        .iter()
        .map(|tool| {
            let mut declaration = mcp_tool_to_provider_declaration(&tool.tool)?;
            if let Some(metadata) = &tool.metadata {
                declaration
                    .extension_fields
                    .insert("astra.agent_binding.metadata".to_string(), metadata.clone());
            }
            if let Some(side_effect_class) = tool.side_effect_class {
                apply_agent_binding_side_effect_contract(&mut declaration, side_effect_class);
            }
            Ok(declaration)
        })
        .collect()
}

fn apply_agent_binding_semantic_read_contract(
    declarations: &mut [astra_turn_types::ProviderToolDeclaration],
    capability: Option<&astra_services::runs::RuntimeSemanticReadCapabilityRequest>,
) -> Result<HashSet<String>, String> {
    let Some(capability) = capability else {
        return Ok(HashSet::new());
    };
    if capability.contract_version != AGENT_BINDING_SEMANTIC_READ_CONTRACT_VERSION {
        return Err(format!(
            "unsupported semantic read contract version '{}'",
            capability.contract_version
        ));
    }
    if capability.tools.is_empty() {
        return Err("semantic read capability must name at least one native tool".to_string());
    }
    let configured = capability
        .tools
        .iter()
        .map(|tool| {
            if tool.is_empty() || tool.trim() != tool || tool.chars().any(char::is_control) {
                return Err(
                    "semantic read native tool names must be non-empty exact strings".to_string(),
                );
            }
            Ok(tool.clone())
        })
        .collect::<Result<HashSet<_>, _>>()?;
    if configured.len() != capability.tools.len() {
        return Err("semantic read capability contains duplicate native tool names".to_string());
    }

    let discovered = declarations
        .iter()
        .map(|declaration| declaration.native_tool_name.clone())
        .collect::<HashSet<_>>();
    if let Some(missing) = configured.difference(&discovered).next() {
        return Err(format!(
            "semantic read capability references undiscovered native tool '{missing}'"
        ));
    }

    let source = || ProviderClaimSource::AstraOwned {
        component: AGENT_BINDING_SEMANTIC_READ_COMPONENT.to_string(),
        field: "tools".to_string(),
    };
    for declaration in declarations {
        if configured.contains(&declaration.native_tool_name) {
            declaration.claims.read_only = Some(ProviderClaim::new(true, source()));
            declaration.claims.destructive = Some(ProviderClaim::new(false, source()));
            declaration.claims.idempotent = Some(ProviderClaim::new(true, source()));
            declaration.claims.semantic_cache = Some(ProviderClaim::new(
                ProviderSemanticCacheContract::RevisionBound,
                source(),
            ));
        }
    }
    Ok(configured)
}

#[derive(Debug, thiserror::Error)]
#[error("{detail}")]
pub(crate) struct AgentBindingMcpRpcError {
    detail: String,
    outcome_unknown: bool,
    timed_out: bool,
    provider_acknowledged: bool,
}

impl AgentBindingMcpRpcError {
    pub(crate) fn side_effects_maybe(&self) -> bool {
        self.outcome_unknown
    }

    pub(crate) fn is_timeout(&self) -> bool {
        self.timed_out
    }

    fn redacted(mut self, secrets: &Value) -> Self {
        self.detail = redact_known_secrets(&self.detail, secrets);
        self
    }

    fn after_send(mut self, risk: AgentBindingMcpRpcRisk) -> Self {
        if !self.provider_acknowledged {
            self.outcome_unknown = risk.outcome_unknown_after_send();
        }
        self
    }
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
        outcome_unknown: false,
        timed_out: false,
        provider_acknowledged: false,
    }
}

fn agent_binding_mcp_provider_error(detail: impl Into<String>) -> AgentBindingMcpRpcError {
    AgentBindingMcpRpcError {
        detail: detail.into(),
        outcome_unknown: false,
        timed_out: false,
        provider_acknowledged: true,
    }
}

fn agent_binding_mcp_timeout_error(detail: impl Into<String>) -> AgentBindingMcpRpcError {
    AgentBindingMcpRpcError {
        detail: detail.into(),
        outcome_unknown: true,
        timed_out: true,
        provider_acknowledged: false,
    }
}

#[derive(Clone, Copy)]
enum AgentBindingMcpRpcRisk {
    SideEffectFree,
    ToolInvocation,
}

impl AgentBindingMcpRpcRisk {
    fn outcome_unknown_after_send(self) -> bool {
        matches!(self, Self::ToolInvocation)
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

    let provider_policy_index =
        astra_turn_core::provider_resolution::ResolvedProviderPolicyIndex::from_snapshots(
            &provider_snapshots,
        )
        .map_err(|error| {
            mcp_error(
                StatusCode::CONFLICT,
                error.to_string(),
                "mcp_provider_policy_conflict",
            )
        })?;
    Ok(Some(RuntimeMcpBundle {
        schemas,
        provider_snapshots,
        provider_policy_index,
        control_tools: Default::default(),
        stop_after_success_tools: Default::default(),
        manager: Some(Arc::new(RwLock::new(manager))),
        agent_binding_mcp: None,
        semantic_read_capabilities: Default::default(),
    }))
}

async fn post_agent_binding_mcp_rpc(
    endpoint_url: &str,
    authorization: &str,
    payload: Value,
) -> Result<Value, AgentBindingMcpRpcError> {
    post_agent_binding_mcp_rpc_with_risk(
        endpoint_url,
        authorization,
        payload,
        Duration::from_secs(AGENT_BINDING_MCP_RPC_TIMEOUT_SECS),
        AgentBindingMcpRpcRisk::SideEffectFree,
    )
    .await
}

async fn post_agent_binding_mcp_rpc_with_risk(
    endpoint_url: &str,
    authorization: &str,
    payload: Value,
    timeout: Duration,
    risk: AgentBindingMcpRpcRisk,
) -> Result<Value, AgentBindingMcpRpcError> {
    let _permit = crate::capability_endpoint_pool::try_acquire_endpoint_permit(endpoint_url)
        .map_err(agent_binding_mcp_rpc_error)?;

    let (status, body) = tokio::time::timeout(timeout, async {
        let response = agent_binding_mcp_http_client()
            .post(endpoint_url)
            .header(reqwest::header::AUTHORIZATION, authorization)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .json(&payload)
            .send()
            .await
            .map_err(|error| {
                agent_binding_mcp_rpc_error(format!(
                    "Agent Binding MCP RPC to '{endpoint_url}' failed: {error}"
                ))
                .after_send(risk)
            })?;
        let status = response.status();
        let body = response.text().await.map_err(|error| {
            agent_binding_mcp_rpc_error(format!(
                "Agent Binding MCP RPC to '{endpoint_url}' failed while reading response: {error}"
            ))
            .after_send(risk)
        })?;
        Ok::<_, AgentBindingMcpRpcError>((status, body))
    })
    .await
    .map_err(|_| {
        agent_binding_mcp_timeout_error(format!(
            "Agent Binding MCP RPC to '{endpoint_url}' timed out after {}ms",
            timeout.as_millis()
        ))
        .after_send(risk)
    })??;

    if !status.is_success() {
        if let Err(error) = decode_agent_binding_mcp_rpc_response(&body)
            && error.provider_acknowledged
        {
            return Err(error);
        }
        return Err(agent_binding_mcp_rpc_error(format!(
            "Agent Binding MCP RPC to '{endpoint_url}' returned HTTP {status}: {body}"
        ))
        .after_send(risk));
    }

    decode_agent_binding_mcp_rpc_response(&body).map_err(|error| error.after_send(risk))
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
        return Err(agent_binding_mcp_provider_error(format!(
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
        let tool_name = normalized
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>");
        let side_effect_class =
            parse_agent_binding_side_effect_class(tool_name, normalized.get("side_effect_class"))?;
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
        parsed.push(AgentBindingMcpTool {
            tool,
            side_effect_class,
            metadata,
        });
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
        if let Some(descriptor) =
            crate::turn::tool_completion::RuntimeStopAfterSuccessToolDescriptor::from_metadata(
                name,
                tool.metadata.as_ref(),
            )
            .map_err(|error| (error.to_string(), error.error_code()))?
        {
            stop_after_success_tools.push(descriptor);
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
    snapshot: &ResolvedProviderSnapshot,
) -> Result<HashMap<String, String>, String> {
    let descriptors = snapshot
        .descriptors
        .iter()
        .map(|descriptor| {
            (
                descriptor.descriptor_ref(),
                descriptor.native_tool_name.as_str(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    snapshot
        .alias_index
        .iter()
        .map(|(public_alias, descriptor_ref)| {
            let native_name = descriptors.get(descriptor_ref).ok_or_else(|| {
                format!(
                    "Agent Binding alias '{}' references missing descriptor '{}@{}'",
                    public_alias,
                    descriptor_ref.identity.native_tool_id,
                    descriptor_ref.descriptor_version,
                )
            })?;
            Ok((
                public_alias.as_str().to_string(),
                (*native_name).to_string(),
            ))
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
        Self::for_tests_at_endpoint(server_name, public_tool_names, "http://127.0.0.1:1/mcp")
    }

    #[cfg(test)]
    pub(crate) fn for_tests_at_endpoint(
        server_name: &str,
        public_tool_names: &[&str],
        endpoint_url: &str,
    ) -> Self {
        let tool_names_by_public_name = public_tool_names
            .iter()
            .map(|name| ((*name).to_string(), (*name).to_string()))
            .collect();
        Self {
            server_name: server_name.to_string(),
            endpoint_url: endpoint_url.to_string(),
            authorization: "Bearer test".to_string(),
            tool_names_by_public_name: Arc::new(tool_names_by_public_name),
            semantic_read_tools: Arc::new(HashSet::new()),
        }
    }

    pub(crate) fn owns_public_tool_name(&self, public_name: &str) -> bool {
        self.tool_names_by_public_name.contains_key(public_name)
    }

    pub(crate) async fn call_tool_by_mcp_name(
        &self,
        public_name: &str,
        args: &Value,
        tool_call_id: &str,
        semantic_read_condition: Option<&astra_turn_types::SemanticReadCondition>,
        provider_interaction_response: Option<&astra_turn_types::ProviderInteractionResponse>,
    ) -> Result<McpToolCallResult, AgentBindingMcpRpcError> {
        if tool_call_id.trim().is_empty() {
            return Err(agent_binding_mcp_rpc_error(
                "Agent Binding MCP tool call identity is required",
            ));
        }
        let tool_name = self
            .tool_names_by_public_name
            .get(public_name)
            .ok_or_else(|| agent_binding_mcp_rpc_error(format!("Tool not found: {public_name}")))?;
        let mut params = json!({
            "name": tool_name,
            "arguments": agent_binding_tool_call_arguments(args),
            "call_id": tool_call_id,
        });
        let mut protocol_metadata = serde_json::Map::new();
        if let Some(condition) = semantic_read_condition {
            protocol_metadata.insert(
                AGENT_BINDING_SEMANTIC_READ_CONDITION_METADATA_KEY.to_string(),
                serde_json::to_value(condition).expect("semantic read condition must serialize"),
            );
        }
        if let Some(response) = provider_interaction_response {
            protocol_metadata.insert(
                astra_turn_types::PROVIDER_INTERACTION_RESPONSE_METADATA_KEY.to_string(),
                serde_json::to_value(response)
                    .expect("provider interaction response must serialize"),
            );
        }
        if !protocol_metadata.is_empty() {
            params["_meta"] = Value::Object(protocol_metadata);
        }
        let payload = json!({
            "jsonrpc": "2.0",
            "id": "astra-agent-binding-tools-call",
            "method": "tools/call",
            "params": params,
        });
        let result = post_agent_binding_mcp_rpc_with_risk(
            &self.endpoint_url,
            &self.authorization,
            payload,
            Duration::from_secs(AGENT_BINDING_MCP_RPC_TIMEOUT_SECS),
            AgentBindingMcpRpcRisk::ToolInvocation,
        )
        .await
        .map_err(|error| {
            error.redacted(&json!({
                "authorization": &self.authorization,
                "url": &self.endpoint_url,
            }))
        })?;
        extract_agent_binding_mcp_tool_result(&result).map_err(|error| {
            error.redacted(&json!({
                "authorization": &self.authorization,
                "url": &self.endpoint_url,
            }))
        })
    }

    pub(crate) fn server_name(&self) -> &str {
        &self.server_name
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentBindingSemanticReadPrepareResult {
    facts: Vec<AgentBindingSemanticFreshnessFact>,
    condition: AgentBindingSemanticReadPreparedCondition,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentBindingSemanticFreshnessFact {
    scope: SemanticFreshnessScope,
    subject: String,
    revision: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentBindingSemanticReadPreparedCondition {
    protocol: String,
    token: String,
}

#[async_trait]
impl crate::server::semantic_read_freshness::ProviderSemanticFreshnessSource
    for AgentBindingMcpRuntime
{
    async fn prepare(
        &self,
        request: crate::server::semantic_read_freshness::ProviderSemanticFreshnessRequest<'_>,
    ) -> Result<
        crate::server::semantic_read_freshness::ProviderSemanticFreshnessEvidence,
        crate::server::semantic_read_freshness::ProviderSemanticFreshnessSourceError,
    > {
        let native_tool = request.descriptor.identity.native_tool_id.as_str();
        if !self.semantic_read_tools.contains(native_tool) {
            return Ok(
                crate::server::semantic_read_freshness::ProviderSemanticFreshnessEvidence::Unavailable,
            );
        }
        let result = post_agent_binding_mcp_rpc_with_risk(
            &self.endpoint_url,
            &self.authorization,
            json!({
                "jsonrpc": "2.0",
                "id": "astra-agent-binding-semantic-read-prepare",
                "method": AGENT_BINDING_SEMANTIC_READ_PREPARE_METHOD,
                "params": {
                    "contractVersion": AGENT_BINDING_SEMANTIC_READ_CONTRACT_VERSION,
                    "name": native_tool,
                    "arguments": request.public_arguments,
                }
            }),
            AGENT_BINDING_SEMANTIC_READ_PREPARE_TIMEOUT,
            AgentBindingMcpRpcRisk::SideEffectFree,
        )
        .await
        .and_then(|value| {
            serde_json::from_value::<AgentBindingSemanticReadPrepareResult>(value).map_err(
                |error| {
                    agent_binding_mcp_rpc_error(format!(
                        "Agent Binding semantic read prepare result is invalid: {error}"
                    ))
                },
            )
        })
        .and_then(|prepared| {
            let facts = prepared
                .facts
                .into_iter()
                .map(|fact| SemanticFreshnessFact::new(fact.scope, &fact.subject, &fact.revision))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| {
                    agent_binding_mcp_rpc_error(format!(
                        "Agent Binding semantic read freshness evidence is invalid: {error}"
                    ))
                })?;
            if facts.is_empty() {
                return Err(agent_binding_mcp_rpc_error(
                    "Agent Binding semantic read freshness evidence must not be empty",
                ));
            }
            Ok(
                crate::server::semantic_read_freshness::ProviderSemanticFreshnessEvidence::Conditional {
                    facts,
                    protocol: prepared.condition.protocol,
                    token: prepared.condition.token,
                },
            )
        });
        result.map_err(|error| {
            let error = error.redacted(&json!({
                "authorization": &self.authorization,
                "url": &self.endpoint_url,
            }));
            tracing::warn!(
                provider_binding = %self.server_name,
                native_tool,
                timed_out = error.is_timeout(),
                transport_outcome_unknown = error.side_effects_maybe(),
                "Agent Binding semantic read freshness preparation failed; executing uncached"
            );
            crate::server::semantic_read_freshness::ProviderSemanticFreshnessSourceError::SourceFailed
        })
    }
}

pub(crate) async fn prepare_agent_binding_mcp_bundle(
    server_id: &str,
    endpoint_url: &str,
    authorization: &str,
    semantic_read_capability: Option<&astra_services::runs::RuntimeSemanticReadCapabilityRequest>,
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
    let mut declarations =
        agent_binding_tools_to_provider_declarations(&tools).map_err(|error| {
            mcp_error(
                StatusCode::BAD_GATEWAY,
                format!("invalid Agent Binding MCP discovery: {error}"),
                "agent_binding_discovery_invalid",
            )
        })?;
    let semantic_read_tools =
        apply_agent_binding_semantic_read_contract(&mut declarations, semantic_read_capability)
            .map_err(|error| {
                mcp_error(
                    StatusCode::BAD_REQUEST,
                    format!("invalid Agent Binding semantic read capability: {error}"),
                    "agent_binding_semantic_read_capability_invalid",
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
    let resolved_snapshot = resolve_agent_binding_mcp_snapshot(
        &tool_namespace,
        &provider_snapshot,
        !semantic_read_tools.is_empty(),
    )
    .map_err(|error| {
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
    let tool_names_by_public_name =
        tool_names_by_public_name(&resolved_snapshot).map_err(|error| {
            mcp_error(
                StatusCode::BAD_GATEWAY,
                error,
                "agent_binding_provider_resolution_failed",
            )
        })?;
    let agent_binding_mcp = Arc::new(AgentBindingMcpRuntime {
        server_name: tool_namespace,
        endpoint_url: endpoint_url.to_string(),
        authorization: authorization.to_string(),
        tool_names_by_public_name: Arc::new(tool_names_by_public_name),
        semantic_read_tools: Arc::new(semantic_read_tools),
    });
    let provider_snapshots = vec![resolved_snapshot];
    let provider_policy_index =
        astra_turn_core::provider_resolution::ResolvedProviderPolicyIndex::from_snapshots(
            &provider_snapshots,
        )
        .map_err(|error| {
            mcp_error(
                StatusCode::BAD_GATEWAY,
                error.to_string(),
                "agent_binding_provider_policy_invalid",
            )
        })?;
    let mut semantic_read_capabilities =
        crate::server::semantic_read_freshness::ProviderSemanticFreshnessRegistry::default();
    if !agent_binding_mcp.semantic_read_tools.is_empty() {
        semantic_read_capabilities
            .register(
                ProviderBindingRef::new(server_id.to_string()).map_err(|error| {
                    mcp_error(
                        StatusCode::BAD_REQUEST,
                        error.to_string(),
                        "agent_binding_capability_ref_invalid",
                    )
                })?,
                agent_binding_mcp.clone(),
            )
            .map_err(|error| {
                mcp_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    error.to_string(),
                    "agent_binding_semantic_read_capability_conflict",
                )
            })?;
    }
    Ok(RuntimeMcpBundle {
        schemas,
        provider_snapshots,
        provider_policy_index,
        control_tools,
        stop_after_success_tools,
        manager: None,
        agent_binding_mcp: Some(agent_binding_mcp),
        semantic_read_capabilities,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_binding_timeout_preserves_unknown_outcome_evidence() {
        let error = agent_binding_mcp_timeout_error("timed out");

        assert!(error.side_effects_maybe());
        assert!(error.is_timeout());
    }

    #[test]
    fn agent_binding_post_send_certainty_depends_on_rpc_semantics_and_acknowledgement() {
        let prepare_timeout = agent_binding_mcp_timeout_error("timed out")
            .after_send(AgentBindingMcpRpcRisk::SideEffectFree);
        assert!(prepare_timeout.is_timeout());
        assert!(!prepare_timeout.side_effects_maybe());

        let malformed_tool_response = agent_binding_mcp_rpc_error("malformed response")
            .after_send(AgentBindingMcpRpcRisk::ToolInvocation);
        assert!(malformed_tool_response.side_effects_maybe());

        let acknowledged_provider_error = agent_binding_mcp_provider_error("rejected")
            .after_send(AgentBindingMcpRpcRisk::ToolInvocation);
        assert!(!acknowledged_provider_error.side_effects_maybe());
    }

    #[test]
    fn agent_binding_side_effect_classes_resolve_into_provider_policy() {
        let tools = parse_agent_binding_mcp_tools(json!({
            "tools": [
                {
                    "name": "read-data",
                    "inputSchema": {"type": "object"},
                    "side_effect_class": "read"
                },
                {
                    "name": "write-file",
                    "inputSchema": {"type": "object"},
                    "side_effect_class": "write"
                },
                {
                    "name": "send-message",
                    "inputSchema": {"type": "object"},
                    "side_effect_class": "external_effect"
                }
            ]
        }))
        .expect("valid discovery response");
        let declarations = agent_binding_tools_to_provider_declarations(&tools)
            .expect("valid provider declarations");
        let discovery = ProviderDiscoverySnapshot::new(
            ProviderIdentity::new("provider-1").unwrap(),
            ProviderBindingRef::new("provider-1").unwrap(),
            ProviderProtocolId::new("mcp").unwrap(),
            declarations,
        )
        .unwrap();
        let resolved = resolve_agent_binding_mcp_snapshot("provider", &discovery, false)
            .expect("side-effect extension must resolve");
        let by_name = resolved
            .descriptors
            .iter()
            .map(|descriptor| (descriptor.native_tool_name.as_str(), descriptor))
            .collect::<HashMap<_, _>>();

        let read = by_name["read-data"];
        assert_eq!(
            read.semantic_baseline.effect,
            astra_turn_types::ResolvedToolEffect::ReadOnly
        );
        assert_eq!(
            read.semantic_baseline.idempotency,
            astra_turn_types::ResolvedToolIdempotency::PureRead
        );
        assert_eq!(
            read.semantic_baseline.concurrency,
            astra_turn_types::ResolvedConcurrencyBaseline::ParallelReadOnly
        );

        for name in ["write-file", "send-message"] {
            let descriptor = by_name[name];
            assert_eq!(
                descriptor.semantic_baseline.effect,
                astra_turn_types::ResolvedToolEffect::Mutating
            );
            assert_eq!(
                descriptor.semantic_baseline.idempotency,
                astra_turn_types::ResolvedToolIdempotency::NonIdempotent
            );
            assert_eq!(
                descriptor.semantic_baseline.concurrency,
                astra_turn_types::ResolvedConcurrencyBaseline::Serial
            );
        }
        assert_eq!(
            by_name["send-message"]
                .extension_fields
                .get(AGENT_BINDING_EFFECT_EXTENSION_NAMESPACE)
                .and_then(|value| value.get(AGENT_BINDING_EFFECT_EXTENSION_FIELD))
                .and_then(Value::as_str),
            Some("external_effect")
        );
    }

    #[test]
    fn agent_binding_discovery_rejects_unknown_side_effect_class() {
        let error = parse_agent_binding_mcp_tools(json!({
            "tools": [{
                "name": "unsafe-tool",
                "inputSchema": {"type": "object"},
                "side_effect_class": "mutation"
            }]
        }))
        .expect_err("unknown side effect class must fail discovery");

        assert!(
            error
                .detail
                .contains("unsupported side_effect_class 'mutation'")
        );
    }

    #[tokio::test]
    async fn agent_binding_rpc_deadline_preserves_effect_specific_certainty() {
        use axum::{Router, routing::post};

        let app = Router::new().route(
            "/mcp",
            post(|| async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Json(json!({"jsonrpc": "2.0", "id": "late", "result": {}}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let prepare = post_agent_binding_mcp_rpc_with_risk(
            &endpoint,
            "Bearer test",
            json!({"jsonrpc": "2.0", "id": "prepare", "method": "prepare"}),
            Duration::from_millis(10),
            AgentBindingMcpRpcRisk::SideEffectFree,
        )
        .await
        .expect_err("side-effect-free preparation must honor its deadline");
        assert!(prepare.is_timeout());
        assert!(!prepare.side_effects_maybe());

        let tool_call = post_agent_binding_mcp_rpc_with_risk(
            &endpoint,
            "Bearer test",
            json!({"jsonrpc": "2.0", "id": "call", "method": "tools/call"}),
            Duration::from_millis(10),
            AgentBindingMcpRpcRisk::ToolInvocation,
        )
        .await
        .expect_err("tool invocation must honor its deadline");
        assert!(tool_call.is_timeout());
        assert!(tool_call.side_effects_maybe());
        server.abort();
    }

    #[test]
    fn agent_binding_routes_sorted_aliases_by_descriptor_identity() {
        let tools = parse_agent_binding_mcp_tools(json!({
            "tools": [
                {
                    "name": "zebra",
                    "description": "Z tool",
                    "inputSchema": {"type": "object"}
                },
                {
                    "name": "alpha",
                    "description": "A tool",
                    "inputSchema": {"type": "object"}
                }
            ]
        }))
        .expect("valid discovery response");
        let native_tools = tools
            .iter()
            .map(|tool| tool.tool.clone())
            .collect::<Vec<_>>();
        let discovery = mcp_tools_to_provider_snapshot(
            ProviderIdentity::new("provider-1").unwrap(),
            ProviderBindingRef::new("binding-1").unwrap(),
            &native_tools,
        )
        .unwrap();
        let resolved = resolve_mcp_snapshot("tools", &discovery).unwrap();
        let schemas = mcp_resolved_provider_snapshot_to_schemas_checked(&resolved).unwrap();
        let routes = tool_names_by_public_name(&resolved).unwrap();

        assert_eq!(schemas[0]["function"]["name"], "mcp__tools__alpha");
        assert_eq!(schemas[1]["function"]["name"], "mcp__tools__zebra");
        assert_eq!(routes["mcp__tools__alpha"], "alpha");
        assert_eq!(routes["mcp__tools__zebra"], "zebra");
    }

    #[test]
    fn explicit_mcp_stable_alias_survives_discovery_resolution_and_tool_search() {
        let tools = parse_agent_binding_mcp_tools(json!({
            "tools": [{
                "name": "moi_qq_mail__ch_23f40bed5331",
                "description": "Send QQ mail",
                "inputSchema": {"type": "object"},
                "_meta": {
                    "astra/stableToolAlias": "moi_qq_mail"
                }
            }]
        }))
        .expect("valid discovery response");
        let native_tools = tools
            .iter()
            .map(|tool| tool.tool.clone())
            .collect::<Vec<_>>();
        let discovery = mcp_tools_to_provider_snapshot(
            ProviderIdentity::new("moi-tools").unwrap(),
            ProviderBindingRef::new("moi-tools").unwrap(),
            &native_tools,
        )
        .unwrap();
        assert_eq!(
            discovery.tool_declarations[0]
                .stable_tool_alias
                .as_ref()
                .map(|alias| alias.as_str()),
            Some("moi_qq_mail")
        );

        let resolved = resolve_mcp_snapshot("moi-tools", &discovery).unwrap();
        let schemas = mcp_resolved_provider_snapshot_to_schemas_checked(&resolved).unwrap();
        let result: Value = serde_json::from_str(&astra_tools::tool_search::tool_search(
            &schemas,
            &json!({"query": "select:moi_qq_mail"}),
        ))
        .unwrap();

        assert_eq!(result["selection_status"], "ok");
        assert_eq!(result["matches"][0]["matched_by"], "stable_alias");
        assert_eq!(
            result["resolved"][0],
            "mcp__moi-tools__moi_qq_mail__ch_23f40bed5331"
        );
    }

    #[test]
    fn duplicate_explicit_mcp_stable_aliases_fail_closed_as_ambiguous_selection() {
        let tools = parse_agent_binding_mcp_tools(json!({
            "tools": [
                {
                    "name": "moi_qq_mail__ch_primary",
                    "inputSchema": {"type": "object"},
                    "_meta": {"astra/stableToolAlias": "moi_qq_mail"}
                },
                {
                    "name": "moi_qq_mail__ch_secondary",
                    "inputSchema": {"type": "object"},
                    "_meta": {"astra/stableToolAlias": "moi_qq_mail"}
                }
            ]
        }))
        .expect("valid discovery response");
        let native_tools = tools
            .iter()
            .map(|tool| tool.tool.clone())
            .collect::<Vec<_>>();
        let discovery = mcp_tools_to_provider_snapshot(
            ProviderIdentity::new("moi-tools").unwrap(),
            ProviderBindingRef::new("moi-tools").unwrap(),
            &native_tools,
        )
        .unwrap();
        let resolved = resolve_mcp_snapshot("moi-tools", &discovery).unwrap();
        let schemas = mcp_resolved_provider_snapshot_to_schemas_checked(&resolved).unwrap();
        let result: Value = serde_json::from_str(&astra_tools::tool_search::tool_search(
            &schemas,
            &json!({"query": "select:moi_qq_mail"}),
        ))
        .unwrap();

        assert_eq!(result["selection_status"], "not_found");
        assert_eq!(result["ambiguous"][0]["requested"], "moi_qq_mail");
        assert_eq!(
            result["ambiguous"][0]["candidates"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

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
                    "success_final_template": "{{message}}",
                    "control": {
                        "kind": "moi.control.handoff.v1",
                        "target": "agent_authoring",
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
        assert_eq!(descriptor.target, "agent_authoring");
        assert!(descriptor.terminal);
        assert_eq!(descriptor.policy_id, "authoring.handoff.v1");
        assert_eq!(
            stop_after_success_tools
                .successful_tool_completion(
                    &[astra_services::session_journal::ToolCallRecord {
                        name: public_name.to_string(),
                        ok: true,
                        tool_call_id: Some("call-1".to_string()),
                        ..Default::default()
                    }],
                    &[json!({
                        "tool_call_id": "call-1",
                        "structuredContent": {
                            "output": {"ok": true, "message": "authoring started"}
                        }
                    })],
                )
                .map(|completion| (completion.tool_name, completion.final_text)),
            Some((
                public_name.to_string(),
                Some("authoring started".to_string())
            ))
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

        let calls = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
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
                        .push((authorization, body.clone()));

                    let response = match method.as_str() {
                        "tools/list" => json!({
                            "jsonrpc": "2.0",
                            "id": body.get("id").cloned().unwrap_or(Value::Null),
                            "result": {
                                "tools": [{
                                    "name": "query",
                                    "description": "Query data",
                                    "side_effect_class": "write",
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

        let bundle =
            prepare_agent_binding_mcp_bundle("tools", &endpoint, "Bearer runtime-grant", None)
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
            astra_turn_types::ResolvedToolEffect::Mutating,
            "the trusted Agent Binding side-effect extension must classify writes"
        );
        assert_eq!(
            snapshot
                .alias_index
                .get(&astra_turn_types::PublicToolAlias::new("mcp__tools__query").unwrap()),
            Some(&descriptor.descriptor_ref()),
            "the model-visible schema alias must resolve to the exact carried descriptor"
        );
        let invocation_policy = bundle
            .provider_policy_index
            .resolve("mcp__tools__query")
            .expect("visible provider alias must have one invocation policy");
        assert_eq!(invocation_policy.descriptor, descriptor.descriptor_ref());
        assert!(invocation_policy.requires_approval());
        assert!(!invocation_policy.parallelizable);

        for invalid_tool_call_id in ["", " \t"] {
            let missing_identity_error = bundle
                .agent_binding_mcp
                .as_ref()
                .unwrap()
                .call_tool_by_mcp_name(
                    "mcp__tools__query",
                    &json!({"q": "hello"}),
                    invalid_tool_call_id,
                    None,
                    None,
                )
                .await
                .expect_err("a blank tool call identity must fail before transport");
            assert!(
                missing_identity_error
                    .to_string()
                    .contains("tool call identity is required")
            );
        }
        assert_eq!(
            calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            1,
            "an invalid business identity must not emit a tools/call request"
        );

        let output = bundle
            .agent_binding_mcp
            .as_ref()
            .unwrap()
            .call_tool_by_mcp_name(
                "mcp__tools__query",
                &json!({"q": "hello"}),
                "model-tool-call-42",
                None,
                None,
            )
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

        bundle
            .agent_binding_mcp
            .as_ref()
            .unwrap()
            .call_tool_by_mcp_name(
                "mcp__tools__query",
                &json!({"q": "hello"}),
                "model-tool-call-42",
                None,
                Some(&astra_turn_types::ProviderInteractionResponse {
                    request_id: "model-tool-call-42:select".to_string(),
                    outcome: astra_turn_types::ProviderInteractionOutcome::Submitted,
                    payload: Some(json!({"selected": "opaque-1"})),
                }),
            )
            .await
            .expect("resumed tool call should preserve its identity and protocol metadata");

        let calls = calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].0, "Bearer runtime-grant");
        assert_eq!(calls[0].1["method"], "tools/list");
        assert_eq!(calls[1].0, "Bearer runtime-grant");
        assert_eq!(calls[1].1["method"], "tools/call");
        assert_eq!(calls[1].1["id"], "astra-agent-binding-tools-call");
        assert_eq!(
            calls[1].1.pointer("/params/call_id"),
            Some(&json!("model-tool-call-42")),
            "the model-authored tool call id must remain exact in the Agent Binding envelope"
        );
        assert_ne!(
            calls[1].1["id"], calls[1].1["params"]["call_id"],
            "JSON-RPC correlation and the business tool call identity are independent"
        );
        assert_eq!(
            calls[2].1.pointer("/params/call_id"),
            Some(&json!("model-tool-call-42")),
            "resuming a provider interaction must retry the same business tool call"
        );
        assert_eq!(
            calls[2]
                .1
                .pointer("/params/_meta/astra~1providerInteractionResponse/request_id"),
            Some(&json!("model-tool-call-42:select"))
        );
        assert_eq!(
            calls[2]
                .1
                .pointer("/params/_meta/astra~1providerInteractionResponse/payload/selected"),
            Some(&json!("opaque-1"))
        );
        server.abort();
    }

    #[tokio::test]
    async fn agent_binding_semantic_read_contract_is_authorized_conditioned_and_acknowledged() {
        use axum::{Router, routing::post};
        use std::sync::Mutex;

        let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
        let requests_for_handler = requests.clone();
        let app = Router::new().route(
            "/mcp",
            post(move |Json(body): Json<Value>| {
                let requests = requests_for_handler.clone();
                async move {
                    requests
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(body.clone());
                    let id = body.get("id").cloned().unwrap_or(Value::Null);
                    let result = match body.get("method").and_then(Value::as_str) {
                        Some("tools/list") => json!({
                            "tools": [{
                                "name": "query",
                                "description": "Query a revisioned resource",
                                "inputSchema": {"type": "object"}
                            }]
                        }),
                        Some(AGENT_BINDING_SEMANTIC_READ_PREPARE_METHOD) => json!({
                            "facts": [{
                                "scope": "resource",
                                "subject": "catalog/orders",
                                "revision": "etag-7"
                            }],
                            "condition": {
                                "protocol": "if-match",
                                "token": "etag-7"
                            }
                        }),
                        Some("tools/call") => {
                            let condition = body
                                .pointer("/params/_meta/astra.semantic_read_condition")
                                .cloned()
                                .expect("condition must reach the exact provider dispatch");
                            let condition = serde_json::from_value::<
                                astra_turn_types::SemanticReadCondition,
                            >(condition)
                            .expect("transported condition must remain valid");
                            json!({
                                "content": [{"type": "text", "text": "orders-v7"}],
                                "_meta": {
                                    "astra.semantic_read_condition_ack":
                                        astra_turn_types::SemanticReadConditionAck::for_condition(
                                            &condition
                                        ),
                                    "providerPrivate": "must-not-be-authority"
                                }
                            })
                        }
                        other => panic!("unexpected method: {other:?}"),
                    };
                    Json(json!({"jsonrpc": "2.0", "id": id, "result": result}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let capability = astra_services::runs::RuntimeSemanticReadCapabilityRequest {
            contract_version: AGENT_BINDING_SEMANTIC_READ_CONTRACT_VERSION.to_string(),
            tools: vec!["query".to_string()],
        };

        let bundle = prepare_agent_binding_mcp_bundle(
            "tools",
            &endpoint,
            "Bearer runtime-grant",
            Some(&capability),
        )
        .await
        .expect("trusted semantic read adapter should compose");
        let descriptor = &bundle.provider_snapshots[0].descriptors[0];
        assert_eq!(
            descriptor.semantic_baseline.effect,
            astra_turn_types::ResolvedToolEffect::ReadOnly
        );
        assert_eq!(
            descriptor.semantic_baseline.semantic_cache,
            astra_turn_types::ResolvedSemanticCacheBaseline::FreshnessBound
        );
        assert_eq!(bundle.semantic_read_capabilities.len(), 1);

        let evidence = crate::server::semantic_read_freshness::prepare_provider_semantic_freshness(
            Some(&bundle.semantic_read_capabilities),
            &descriptor.descriptor_ref(),
            &json!({"z": 1, "_run_id": "must-not-leak", "a": 2}),
        )
        .await
        .expect("provider prepare should return revision evidence");
        let crate::server::semantic_read_freshness::ProviderSemanticFreshnessEvidence::Conditional {
            facts,
            protocol,
            token,
        } = evidence
        else {
            panic!("configured adapter must return conditional evidence");
        };
        let freshness = astra_turn_types::SemanticReadFreshnessContext::new("user:u1", facts)
            .expect("freshness context");
        let condition = astra_turn_types::SemanticReadCondition::new(&protocol, &token, &freshness)
            .expect("condition");
        let result = bundle
            .agent_binding_mcp
            .as_ref()
            .unwrap()
            .call_tool_by_mcp_name(
                "mcp__tools__query",
                &json!({"a": 2, "z": 1}),
                "semantic-read-call",
                Some(&condition),
                None,
            )
            .await
            .expect("conditioned tool call should succeed");
        let acknowledgement = result
            .protocol_metadata
            .as_ref()
            .and_then(|metadata| {
                metadata.get(astra_turn_types::SEMANTIC_READ_CONDITION_ACK_METADATA_KEY)
            })
            .cloned()
            .and_then(|value| {
                serde_json::from_value::<astra_turn_types::SemanticReadConditionAck>(value).ok()
            })
            .expect("provider must return a typed acknowledgement");
        assert!(acknowledgement.confirms(&condition));

        let workspace = tempfile::tempdir().unwrap();
        let mut executor = crate::server::runtime_tool_executor::RuntimeToolExecutor::new(
            workspace.path().to_path_buf(),
            "u1".to_string(),
            "s1".to_string(),
            None,
            None,
        );
        executor.enable_durable_invocations();
        assert_eq!(
            bundle
                .configure_semantic_read_cache(&mut executor)
                .expect("production bundle composition should activate cache atomically"),
            crate::server::runtime_tool_executor::SemanticReadCacheActivation::Enabled {
                binding_count: 1
            }
        );

        let requests = requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let prepare = requests
            .iter()
            .find(|request| {
                request.get("method").and_then(Value::as_str)
                    == Some(AGENT_BINDING_SEMANTIC_READ_PREPARE_METHOD)
            })
            .expect("prepare request recorded");
        assert_eq!(
            prepare.pointer("/params/arguments"),
            Some(&json!({"a": 2, "z": 1})),
            "only canonical public arguments may cross the freshness boundary"
        );
        server.abort();
    }

    #[test]
    fn agent_binding_semantic_read_contract_rejects_unknown_native_tool() {
        let tools = parse_agent_binding_mcp_tools(json!({
            "tools": [{"name": "query", "inputSchema": {"type": "object"}}]
        }))
        .unwrap();
        let mut declarations = tools
            .iter()
            .map(|tool| mcp_tool_to_provider_declaration(&tool.tool).unwrap())
            .collect::<Vec<_>>();
        let capability = astra_services::runs::RuntimeSemanticReadCapabilityRequest {
            contract_version: AGENT_BINDING_SEMANTIC_READ_CONTRACT_VERSION.to_string(),
            tools: vec!["missing".to_string()],
        };
        let error =
            apply_agent_binding_semantic_read_contract(&mut declarations, Some(&capability))
                .expect_err("host capability must match exact discovery identity");
        assert!(error.contains("undiscovered native tool 'missing'"));
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

        let bundle =
            prepare_agent_binding_mcp_bundle("tools", &endpoint, "Bearer runtime-grant", None)
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
                None,
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
