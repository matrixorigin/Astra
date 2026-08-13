use std::collections::HashSet;

use astra_turn_types::{
    NativeToolId, PROVIDER_INTERACTION_REQUEST_METADATA_KEY, ProviderBindingRef,
    ProviderCallOutcome, ProviderCallPayload, ProviderClaim, ProviderClaimSource,
    ProviderContractError, ProviderDiscoverySnapshot, ProviderIdentity, ProviderInteractionRequest,
    ProviderProtocolId, ProviderTaskSupport, ProviderToolClaims, ProviderToolDeclaration,
    ResolvedProviderSnapshot,
};
use rmcp::model::{CallToolResult, TaskSupport, Tool};
use serde_json::{Map, Value};

/// Maximum length for tool descriptions sent to the model.
pub const MAX_DESCRIPTION_LENGTH: usize = 2048;

/// Maximum character length for tool call result content (~25K tokens × 4 chars/token).
pub const MAX_RESULT_CONTENT_LENGTH: usize = 100_000;

const TRUNCATION_MARKER: &str = "… [truncated]";

/// Tool call result fields that must survive beyond model-facing text output.
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolCallResult {
    pub output: String,
    pub structured_content: Option<Value>,
    pub protocol_metadata: Option<Value>,
    pub is_error: bool,
}

impl McpToolCallResult {
    /// Convert the wire-specific MCP result into the provider-neutral outcome.
    /// The typed MCP flag is authoritative; result prose is never classified.
    pub fn into_provider_outcome(self) -> ProviderCallOutcome {
        if !self.is_error
            && let Some(raw) = self
                .protocol_metadata
                .as_ref()
                .and_then(|metadata| metadata.get(PROVIDER_INTERACTION_REQUEST_METADATA_KEY))
        {
            return match serde_json::from_value::<ProviderInteractionRequest>(raw.clone())
                .map_err(|error| error.to_string())
                .and_then(|request| {
                    request.validate().map_err(|error| error.to_string())?;
                    Ok(request)
                }) {
                Ok(request) => ProviderCallOutcome::InteractionRequired(request),
                Err(error) => ProviderCallOutcome::ToolFailure(ProviderCallPayload {
                    text: format!("Provider interaction contract is invalid: {error}"),
                    structured_content: None,
                    protocol_metadata: None,
                }),
            };
        }
        let payload = ProviderCallPayload {
            text: self.output,
            structured_content: self.structured_content,
            protocol_metadata: self.protocol_metadata,
        };
        if self.is_error {
            ProviderCallOutcome::ToolFailure(payload)
        } else {
            ProviderCallOutcome::Success(payload)
        }
    }
}

/// Returns the largest byte index <= `index` that lies on a UTF-8 char boundary.
fn floor_char_boundary(s: &str, index: usize) -> usize {
    let index = index.min(s.len());
    (0..=index)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0)
}

fn truncate_with_marker(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    let content_budget = max_len.saturating_sub(TRUNCATION_MARKER.len());
    let end = floor_char_boundary(s, content_budget);
    format!("{}{TRUNCATION_MARKER}", &s[..end])
}

/// Sanitize a tool name: only alphanumeric, underscore, hyphen allowed.
/// Replaces invalid chars with underscore.
pub fn sanitize_tool_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Convert MCP Tool to Astra tool schema (OpenAI function-calling format).
/// Naming: `mcp__{server}__{tool}` with sanitization.
pub fn mcp_tool_to_schema(server_name: &str, tool: &Tool) -> Value {
    let params = serde_json::to_value(tool.input_schema.as_ref()).unwrap_or_else(|_| {
        serde_json::json!({
            "type": "object",
            "properties": {},
        })
    });

    let raw_desc = tool.description.as_deref().unwrap_or("");
    mcp_tool_schema_from_parts(server_name, tool.name.as_ref(), raw_desc, params)
}

/// Decode an MCP declaration into Astra's provider-neutral discovery contract.
/// Optional hints retain both their absence and their MCP field provenance.
pub fn mcp_tool_to_provider_declaration(
    tool: &Tool,
) -> Result<ProviderToolDeclaration, ProviderContractError> {
    let protocol = ProviderProtocolId::new("mcp")?;
    let claim = |value: Option<bool>, field: &str| {
        value.map(|value| {
            ProviderClaim::new(
                value,
                ProviderClaimSource::StandardProtocol {
                    protocol: protocol.clone(),
                    field: field.to_string(),
                },
            )
        })
    };
    let annotations = tool.annotations.as_ref();
    let claims = ProviderToolClaims {
        read_only: claim(
            annotations.and_then(|value| value.read_only_hint),
            "annotations.readOnlyHint",
        ),
        destructive: claim(
            annotations.and_then(|value| value.destructive_hint),
            "annotations.destructiveHint",
        ),
        idempotent: claim(
            annotations.and_then(|value| value.idempotent_hint),
            "annotations.idempotentHint",
        ),
        open_world: claim(
            annotations.and_then(|value| value.open_world_hint),
            "annotations.openWorldHint",
        ),
        // Standard MCP annotations do not define a revision-token contract.
        semantic_cache: None,
    };
    let task_support = match tool
        .execution
        .as_ref()
        .and_then(|execution| execution.task_support)
    {
        None => ProviderTaskSupport::Unspecified,
        Some(TaskSupport::Forbidden) => ProviderTaskSupport::Forbidden,
        Some(TaskSupport::Optional) => ProviderTaskSupport::Optional,
        Some(TaskSupport::Required) => ProviderTaskSupport::Required,
    };

    // Keep protocol fields even when Astra has not assigned portable semantics
    // to them yet. The namespace prevents accidental interpretation by another
    // adapter and makes future resolution changes invalidate the snapshot hash.
    let mut extension_fields = Map::new();
    insert_serialized_extension(
        &mut extension_fields,
        "mcp.annotations",
        tool.annotations.as_ref(),
    )?;
    insert_serialized_extension(
        &mut extension_fields,
        "mcp.execution",
        tool.execution.as_ref(),
    )?;
    insert_serialized_extension(&mut extension_fields, "mcp.icons", tool.icons.as_ref())?;
    if let Some(meta) = &tool.meta {
        extension_fields.insert("mcp._meta".to_string(), Value::Object(meta.0.clone()));
    }

    let declaration = ProviderToolDeclaration {
        native_tool_id: NativeToolId::new(tool.name.to_string())?,
        native_tool_name: tool.name.to_string(),
        title: tool.title.clone(),
        description: tool.description.as_deref().map(str::to_string),
        input_schema: Value::Object(tool.input_schema.as_ref().clone()),
        output_schema: tool
            .output_schema
            .as_ref()
            .map(|schema| Value::Object(schema.as_ref().clone())),
        claims,
        task_support,
        extension_fields,
    };
    declaration.validate()?;
    Ok(declaration)
}

/// Build the immutable discovery snapshot for one MCP binding.
pub fn mcp_tools_to_provider_snapshot(
    provider_identity: ProviderIdentity,
    binding_ref: ProviderBindingRef,
    tools: &[Tool],
) -> Result<ProviderDiscoverySnapshot, ProviderContractError> {
    let declarations = tools
        .iter()
        .map(mcp_tool_to_provider_declaration)
        .collect::<Result<Vec<_>, _>>()?;
    ProviderDiscoverySnapshot::new(
        provider_identity,
        binding_ref,
        ProviderProtocolId::new("mcp")?,
        declarations,
    )
}

fn insert_serialized_extension<T: serde::Serialize>(
    extensions: &mut Map<String, Value>,
    key: &str,
    value: Option<&T>,
) -> Result<(), ProviderContractError> {
    let Some(value) = value else {
        return Ok(());
    };
    let value = serde_json::to_value(value)
        .map_err(|error| ProviderContractError::Serialization(error.to_string()))?;
    extensions.insert(key.to_string(), value);
    Ok(())
}

/// Convert cached MCP tool metadata to Astra tool schema.
pub fn mcp_tool_schema_from_parts(
    server_name: &str,
    tool_name: &str,
    description: &str,
    parameters: Value,
) -> Value {
    let description = truncate_with_marker(description, MAX_DESCRIPTION_LENGTH);
    let tool_name = sanitize_tool_name(&format!("mcp__{}__{}", server_name, tool_name));

    serde_json::json!({
        "type": "function",
        "function": {
            "name": tool_name,
            "description": description,
            "parameters": parameters,
        }
    })
}

/// Convert MCP tools to schemas and fail if sanitized public names collide.
pub fn tools_to_schemas_checked(server_name: &str, tools: &[Tool]) -> Result<Vec<Value>, String> {
    let declarations = tools
        .iter()
        .map(mcp_tool_to_provider_declaration)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("invalid MCP tool declaration: {error}"))?;
    provider_declarations_to_schemas_checked(server_name, &declarations)
}

/// Project a validated MCP provider snapshot into model-facing function
/// schemas. The public alias is snapshot-scoped and never becomes identity.
pub fn mcp_provider_snapshot_to_schemas_checked(
    server_name: &str,
    snapshot: &ProviderDiscoverySnapshot,
) -> Result<Vec<Value>, String> {
    if snapshot.protocol.as_str() != "mcp" {
        return Err(format!(
            "cannot apply MCP schema projection to '{}' provider snapshot",
            snapshot.protocol
        ));
    }
    provider_declarations_to_schemas_checked(server_name, &snapshot.tool_declarations)
}

/// Project the exact aliases and descriptors from Astra's resolved snapshot.
/// This is the runtime path: model schemas can no longer be generated from a
/// parallel declaration/name map that permission or execution cannot identify.
pub fn mcp_resolved_provider_snapshot_to_schemas_checked(
    snapshot: &ResolvedProviderSnapshot,
) -> Result<Vec<Value>, String> {
    if snapshot.protocol.as_str() != "mcp" {
        return Err(format!(
            "cannot apply MCP schema projection to '{}' resolved provider snapshot",
            snapshot.protocol
        ));
    }

    let descriptors = snapshot
        .descriptors
        .iter()
        .map(|descriptor| (descriptor.descriptor_ref(), descriptor))
        .collect::<std::collections::BTreeMap<_, _>>();
    snapshot
        .alias_index
        .iter()
        .map(|(alias, descriptor_ref)| {
            let descriptor = descriptors.get(descriptor_ref).ok_or_else(|| {
                format!(
                    "resolved MCP alias '{}' references missing descriptor '{}@{}'",
                    alias,
                    descriptor_ref.identity.native_tool_id,
                    descriptor_ref.descriptor_version
                )
            })?;
            let public_name = alias.as_str();
            if sanitize_tool_name(public_name) != public_name {
                return Err(format!(
                    "resolved MCP public alias '{public_name}' is not model-safe"
                ));
            }
            Ok(serde_json::json!({
                "type": "function",
                "function": {
                    "name": public_name,
                    "description": truncate_with_marker(
                        descriptor.description.as_deref().unwrap_or_default(),
                        MAX_DESCRIPTION_LENGTH,
                    ),
                    "parameters": &descriptor.input_schema,
                }
            }))
        })
        .collect()
}

fn provider_declarations_to_schemas_checked(
    server_name: &str,
    declarations: &[ProviderToolDeclaration],
) -> Result<Vec<Value>, String> {
    let mut seen = HashSet::new();
    let mut schemas = Vec::with_capacity(declarations.len());
    for declaration in declarations {
        let schema = mcp_tool_schema_from_parts(
            server_name,
            &declaration.native_tool_name,
            declaration.description.as_deref().unwrap_or_default(),
            declaration.input_schema.clone(),
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

/// Extract tool call result content as string, with default truncation limit.
pub fn extract_result_text(result: &CallToolResult) -> String {
    extract_tool_call_result(result).output
}

/// Extract tool call result content as string, truncated to `max_len` chars.
pub fn extract_result_text_with_limit(result: &CallToolResult, max_len: usize) -> String {
    extract_tool_call_result_with_limit(result, max_len).output
}

/// Extract model-facing text while preserving structured MCP result content.
pub fn extract_tool_call_result(result: &CallToolResult) -> McpToolCallResult {
    extract_tool_call_result_with_limit(result, MAX_RESULT_CONTENT_LENGTH)
}

/// Extract model-facing text while preserving structured MCP result content,
/// truncating only the text channel.
pub fn extract_tool_call_result_with_limit(
    result: &CallToolResult,
    max_len: usize,
) -> McpToolCallResult {
    use rmcp::model::RawContent;

    let mut parts = Vec::new();
    let mut total_len = 0;

    for content in &result.content {
        if let RawContent::Text(text) = &content.raw {
            let remaining = max_len.saturating_sub(total_len);
            if remaining == 0 {
                break;
            }
            if text.text.len() <= remaining {
                total_len += text.text.len();
                parts.push(text.text.clone());
            } else {
                let end = floor_char_boundary(&text.text, remaining);
                parts.push(text.text[..end].to_string());
                total_len += end;
                break;
            }
        }
    }

    let joined = parts.join("\n");
    let output = if total_len >= max_len {
        tracing::warn!(
            "MCP tool result truncated: {total_len} chars exceeded {max_len} char limit"
        );
        format!(
            "{}\n\n[OUTPUT TRUNCATED - exceeded {} char limit]",
            joined, max_len
        )
    } else {
        joined
    };
    McpToolCallResult {
        output,
        structured_content: result.structured_content.clone(),
        protocol_metadata: result
            .meta
            .as_ref()
            .map(|metadata| Value::Object(metadata.0.clone())),
        is_error: result.is_error.unwrap_or(false),
    }
}

// ── Environment variable filtering for stdio transport ──────────────────

const BLOCKED_ENV_PREFIXES: &[&str] = &[
    "LD_",           // LD_PRELOAD, LD_LIBRARY_PATH
    "DYLD_",         // macOS equivalent of LD_*
    "SUDO_",         // SUDO_ASKPASS, SUDO_USER
    "SSH_AUTH_SOCK", // SSH agent socket hijacking
];

const BLOCKED_ENV_EXACT: &[&str] = &[
    "IFS",
    "BASH_ENV",
    "ENV",
    "CDPATH",
    "GLOBIGNORE",
    "SHELLOPTS",
    "BASHOPTS",
    "PROMPT_COMMAND",
    "PYTHONPATH",
    "NODE_PATH",
    "JAVA_TOOL_OPTIONS",
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "DISPLAY",
    "RUST_BACKTRACE",
];

/// Check if an environment variable name is dangerous and should be blocked.
pub fn is_dangerous_env_var(key: &str) -> bool {
    let upper = key.to_uppercase();
    if BLOCKED_ENV_EXACT.iter().any(|&e| upper == e) {
        return true;
    }
    BLOCKED_ENV_PREFIXES
        .iter()
        .any(|&prefix| upper.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn sanitize_valid_names() {
        assert_eq!(sanitize_tool_name("read_file"), "read_file");
        assert_eq!(sanitize_tool_name("mcp_fs_read-file"), "mcp_fs_read-file");
    }

    #[test]
    fn sanitize_special_chars() {
        assert_eq!(sanitize_tool_name("read file"), "read_file");
        assert_eq!(sanitize_tool_name("tool.name"), "tool_name");
        assert_eq!(sanitize_tool_name("ns::func"), "ns__func");
    }

    #[test]
    fn schema_conversion() {
        let schema_map: serde_json::Map<String, Value> =
            serde_json::from_value(serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }))
            .unwrap();

        let tool = Tool::new("read_file", "Read a file", Arc::new(schema_map));
        let schema = mcp_tool_to_schema("filesystem", &tool);
        let func = schema["function"].as_object().unwrap();
        assert_eq!(func["name"], "mcp__filesystem__read_file");
        assert_eq!(func["description"], "Read a file");
    }

    #[test]
    fn tools_to_schemas_checked_rejects_sanitized_collision() {
        let empty: serde_json::Map<String, Value> = serde_json::Map::new();
        let tools = vec![
            Tool::new("query.sql", "Query", Arc::new(empty.clone())),
            Tool::new("query sql", "Query", Arc::new(empty)),
        ];
        let err = tools_to_schemas_checked("jinpan", &tools).unwrap_err();
        assert!(err.contains("mcp__jinpan__query_sql"));
    }

    #[test]
    fn extract_result_text_multiple() {
        use rmcp::model::{Content, RawContent};

        let result = CallToolResult::success(vec![
            Content::new(RawContent::text("Line 1"), None),
            Content::new(RawContent::text("Line 2"), None),
        ]);
        assert_eq!(extract_result_text(&result), "Line 1\nLine 2");
    }

    #[test]
    fn extract_result_text_truncation() {
        use rmcp::model::{Content, RawContent};

        let big_text = "a".repeat(200);
        let result = CallToolResult::success(vec![Content::new(RawContent::text(&big_text), None)]);
        let text = extract_result_text_with_limit(&result, 100);
        assert!(text.contains("[OUTPUT TRUNCATED"));
    }

    #[test]
    fn extract_tool_call_result_preserves_structured_content() {
        use rmcp::model::{Content, Meta, RawContent};

        let structured = serde_json::json!({
            "artifacts": [{
                "artifact_id": "artifact_file_1",
                "type": "file",
                "data": {"file_id": "file_1"}
            }]
        });
        let mut result =
            CallToolResult::success(vec![Content::new(RawContent::text("created file"), None)]);
        result.structured_content = Some(structured.clone());
        result.meta = Some(Meta(Map::from_iter([(
            "requestId".to_string(),
            Value::String("request-1".to_string()),
        )])));

        let extracted = extract_tool_call_result(&result);
        assert_eq!(extracted.output, "created file");
        assert_eq!(extracted.structured_content, Some(structured));
        assert_eq!(
            extracted.protocol_metadata,
            Some(serde_json::json!({"requestId": "request-1"}))
        );
        assert!(!extracted.is_error);
    }

    #[test]
    fn extract_tool_call_result_preserves_typed_failure_without_reading_prose() {
        use rmcp::model::{Content, RawContent};

        let failure = CallToolResult::error(vec![Content::new(RawContent::text("ok"), None)]);
        let success = CallToolResult::success(vec![Content::new(
            RawContent::text("error: quoted documentation"),
            None,
        )]);

        let failure = extract_tool_call_result(&failure);
        let success = extract_tool_call_result(&success);
        assert!(failure.is_error);
        assert!(matches!(
            failure.into_provider_outcome(),
            ProviderCallOutcome::ToolFailure(_)
        ));
        assert!(!success.is_error);
        assert!(matches!(
            success.into_provider_outcome(),
            ProviderCallOutcome::Success(_)
        ));
    }

    #[test]
    fn provider_interaction_metadata_becomes_a_typed_provider_outcome() {
        let result = McpToolCallResult {
            output: String::new(),
            structured_content: None,
            protocol_metadata: Some(serde_json::json!({
                PROVIDER_INTERACTION_REQUEST_METADATA_KEY: {
                    "request_id": "call-1:select",
                    "payload": {"provider_owned": true},
                    "timeout_ms": 1000,
                }
            })),
            is_error: false,
        };

        assert!(matches!(
            result.into_provider_outcome(),
            ProviderCallOutcome::InteractionRequired(ProviderInteractionRequest {
                request_id,
                payload,
                timeout_ms: Some(1000),
            }) if request_id == "call-1:select" && payload == serde_json::json!({"provider_owned": true})
        ));
    }

    #[test]
    fn malformed_provider_interaction_metadata_is_a_tool_failure() {
        let result = McpToolCallResult {
            output: String::new(),
            structured_content: None,
            protocol_metadata: Some(serde_json::json!({
                PROVIDER_INTERACTION_REQUEST_METADATA_KEY: {
                    "request_id": " ",
                    "payload": [],
                }
            })),
            is_error: false,
        };

        let ProviderCallOutcome::ToolFailure(payload) = result.into_provider_outcome() else {
            panic!("malformed interaction must fail the provider tool call");
        };
        assert!(
            payload
                .text
                .contains("Provider interaction contract is invalid")
        );
        assert!(payload.protocol_metadata.is_none());
    }

    #[test]
    fn provider_declaration_preserves_mcp_claims_schemas_and_task_support() {
        use rmcp::model::{ToolAnnotations, ToolExecution};

        let input = Map::from_iter([("type".to_string(), Value::String("object".to_string()))]);
        let output = Map::from_iter([("type".to_string(), Value::String("object".to_string()))]);
        let mut tool = Tool::new("write", "Write data", Arc::new(input));
        tool.title = Some("Writer".to_string());
        tool.output_schema = Some(Arc::new(output));
        tool.annotations = Some(ToolAnnotations::from_raw(
            Some("Annotated writer".to_string()),
            Some(false),
            Some(true),
            Some(true),
            Some(false),
        ));
        tool.execution = Some(ToolExecution::from_raw(Some(TaskSupport::Optional)));

        let declaration = mcp_tool_to_provider_declaration(&tool).unwrap();
        assert_eq!(declaration.native_tool_id.as_str(), "write");
        assert_eq!(declaration.title.as_deref(), Some("Writer"));
        assert!(declaration.output_schema.is_some());
        assert_eq!(
            declaration
                .claims
                .read_only
                .as_ref()
                .map(|claim| claim.value),
            Some(false)
        );
        assert_eq!(
            declaration
                .claims
                .destructive
                .as_ref()
                .map(|claim| claim.value),
            Some(true)
        );
        assert_eq!(
            declaration
                .claims
                .idempotent
                .as_ref()
                .map(|claim| claim.value),
            Some(true)
        );
        assert_eq!(
            declaration
                .claims
                .open_world
                .as_ref()
                .map(|claim| claim.value),
            Some(false)
        );
        assert_eq!(declaration.task_support, ProviderTaskSupport::Optional);
        assert!(declaration.extension_fields.contains_key("mcp.annotations"));
        assert!(declaration.extension_fields.contains_key("mcp.execution"));
    }

    #[test]
    fn absent_mcp_hints_remain_unknown_in_provider_declaration() {
        let tool = Tool::new("read", "Read data", Arc::new(Map::new()));
        let declaration = mcp_tool_to_provider_declaration(&tool).unwrap();

        assert_eq!(declaration.claims, ProviderToolClaims::default());
        assert_eq!(declaration.task_support, ProviderTaskSupport::Unspecified);
        assert!(declaration.extension_fields.is_empty());
    }

    #[test]
    fn mcp_snapshot_is_stable_when_server_discovery_order_changes() {
        let tools = vec![
            Tool::new("z", "Z", Arc::new(Map::new())),
            Tool::new("a", "A", Arc::new(Map::new())),
        ];
        let snapshot = |tools: &[Tool]| {
            mcp_tools_to_provider_snapshot(
                ProviderIdentity::new("server-1").unwrap(),
                ProviderBindingRef::new("binding-1").unwrap(),
                tools,
            )
            .unwrap()
        };

        let first = snapshot(&tools);
        let second = snapshot(&tools.into_iter().rev().collect::<Vec<_>>());
        assert_eq!(first.content_hash, second.content_hash);
        assert_eq!(
            first
                .tool_declarations
                .iter()
                .map(|tool| tool.native_tool_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "z"]
        );
        let projected = mcp_provider_snapshot_to_schemas_checked("server", &first).unwrap();
        assert_eq!(projected[0]["function"]["name"], "mcp__server__a");
        assert_eq!(projected[1]["function"]["name"], "mcp__server__z");
    }

    #[test]
    fn mcp_projection_rejects_a_non_mcp_snapshot() {
        let snapshot = ProviderDiscoverySnapshot::new(
            ProviderIdentity::new("server-1").unwrap(),
            ProviderBindingRef::new("binding-1").unwrap(),
            ProviderProtocolId::new("custom").unwrap(),
            vec![
                mcp_tool_to_provider_declaration(&Tool::new("read", "Read", Arc::new(Map::new())))
                    .unwrap(),
            ],
        )
        .unwrap();

        let error = mcp_provider_snapshot_to_schemas_checked("server", &snapshot).unwrap_err();
        assert!(error.contains("custom"));
    }

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate_with_marker("hello", 10), "hello");
    }

    #[test]
    fn mcp_tool_to_schema_truncates_long_description() {
        let empty: serde_json::Map<String, Value> = serde_json::Map::new();
        let long_desc = "x".repeat(5000);
        let tool = Tool::new("test_tool", long_desc, Arc::new(empty));
        let schema = mcp_tool_to_schema("server", &tool);
        let desc = schema["function"]["description"].as_str().unwrap();
        assert!(desc.len() <= MAX_DESCRIPTION_LENGTH);
        assert!(desc.ends_with("… [truncated]"));
    }
}
