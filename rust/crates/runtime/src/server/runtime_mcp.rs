//! Request-scoped runtime MCP wiring for server-side agent loops.
//!
//! Chat requests may provide MCP server endpoints with opaque credentials for
//! the current turn. The runtime discovers tools for that request and keeps the
//! resulting schemas and connections in memory only.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use astra_core::{ErrorResponse, error_response_coded};
use astra_mcp::{
    McpClientManager, McpServerConfig, McpTool, Transport, sanitize_tool_name,
    tools_to_schemas_checked,
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

#[derive(Clone)]
pub(crate) struct RuntimeMcpBundle {
    pub manager: Arc<RwLock<McpClientManager>>,
    pub schemas: Vec<Value>,
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

fn collect_secret_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) if s.len() >= 4 => {
            out.push(s.clone());
        }
        Value::Array(values) => {
            for value in values {
                collect_secret_strings(value, out);
            }
        }
        Value::Object(map) => {
            for value in map.values() {
                collect_secret_strings(value, out);
            }
        }
        _ => {}
    }
}

fn redact_known_secrets(raw: &str, key_value: &Value) -> String {
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
        manager: Arc::new(RwLock::new(manager)),
        schemas,
    }))
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
}
