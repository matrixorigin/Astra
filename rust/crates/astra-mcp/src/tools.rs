use std::collections::HashSet;

use rmcp::model::{CallToolResult, Tool};
use serde_json::Value;

/// Maximum length for tool descriptions sent to the model.
pub const MAX_DESCRIPTION_LENGTH: usize = 2048;

/// Maximum character length for tool call result content (~25K tokens × 4 chars/token).
pub const MAX_RESULT_CONTENT_LENGTH: usize = 100_000;

const TRUNCATION_MARKER: &str = "… [truncated]";

fn truncate_with_marker(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    let content_budget = max_len.saturating_sub(TRUNCATION_MARKER.len());
    let end = s.floor_char_boundary(content_budget);
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
    let mut seen = HashSet::new();
    let mut schemas = Vec::with_capacity(tools.len());
    for tool in tools {
        let schema = mcp_tool_to_schema(server_name, tool);
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
    extract_result_text_with_limit(result, MAX_RESULT_CONTENT_LENGTH)
}

/// Extract tool call result content as string, truncated to `max_len` chars.
pub fn extract_result_text_with_limit(result: &CallToolResult, max_len: usize) -> String {
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
                let end = text.text.floor_char_boundary(remaining);
                parts.push(text.text[..end].to_string());
                total_len += end;
                break;
            }
        }
    }

    let joined = parts.join("\n");
    if total_len >= max_len {
        tracing::warn!(
            "MCP tool result truncated: {total_len} chars exceeded {max_len} char limit"
        );
        format!(
            "{}\n\n[OUTPUT TRUNCATED - exceeded {} char limit]",
            joined, max_len
        )
    } else {
        joined
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
