//! Normalized reads of OpenAI-style `function.arguments` (object or stringified JSON).
//!
//! **Canonical keys only** (no legacy aliases): file targets use `path`; shell tools use `command`.
//! Edge executors and tool schemas should emit these names; hints do not read `file_path`, `target_file`, or `cmd`.

use serde_json::Value;

use super::cloud_approval_policy::{CloudGatedToolKind, cloud_gated_tool_kind};

/// Parse `function.arguments` from an LLM tool call: either a JSON object or a string of JSON.
pub fn normalize_llm_function_arguments(arguments: &Value) -> Value {
    match arguments {
        Value::String(s) => {
            serde_json::from_str(s).unwrap_or_else(|_| Value::Object(Default::default()))
        }
        v => v.clone(),
    }
}

/// Primary filesystem path from tool arguments (`path` only).
pub fn path_hint_from_args(args: &Value) -> Option<String> {
    args.get("path").and_then(Value::as_str).map(String::from)
}

/// Shell command line from tool arguments (`command` only).
pub fn command_hint_from_args(args: &Value) -> Option<&str> {
    args.get("command").and_then(Value::as_str)
}

/// One-line detail next to the CLI permission icon (aligned with cloud `approval_required` path).
pub fn permission_prompt_primary_detail(tool_name: &str, args: &Value) -> Option<String> {
    // MCP tools: show a compact summary of arguments since they don't have
    // standard "command" or "path" keys.
    if tool_name.starts_with("mcp_") {
        return Some(mcp_args_summary(args));
    }
    match cloud_gated_tool_kind(tool_name) {
        Some(CloudGatedToolKind::Execute) => command_hint_from_args(args).map(String::from),
        Some(CloudGatedToolKind::Write) => path_hint_from_args(args),
        None => command_hint_from_args(args)
            .map(String::from)
            .or_else(|| path_hint_from_args(args)),
    }
}

/// Compact summary of MCP tool arguments for permission prompts.
fn mcp_args_summary(args: &Value) -> String {
    let obj = match args.as_object() {
        Some(o) if !o.is_empty() => o,
        _ => return "(no arguments)".into(),
    };
    let mut parts: Vec<String> = Vec::new();
    for (k, v) in obj.iter().take(3) {
        let val_str = match v {
            Value::String(s) => {
                if s.len() > 60 {
                    format!("\"{}…\"", &s[..57])
                } else {
                    format!("\"{s}\"")
                }
            }
            other => {
                let s = other.to_string();
                if s.len() > 60 {
                    format!("{}…", &s[..57])
                } else {
                    s
                }
            }
        };
        parts.push(format!("{k}={val_str}"));
    }
    if obj.len() > 3 {
        parts.push(format!("+{} more", obj.len() - 3));
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_parses_stringified_json_object() {
        let raw = json!(r#"{"path": "src/x.rs"}"#);
        let v = normalize_llm_function_arguments(&raw);
        assert_eq!(v["path"], "src/x.rs");
    }

    #[test]
    fn normalize_invalid_string_falls_back_to_empty_object() {
        let raw = json!("not json {{{");
        let v = normalize_llm_function_arguments(&raw);
        assert!(v.as_object().map(|o| o.is_empty()).unwrap_or(false));
    }

    #[test]
    fn normalize_passes_through_object() {
        let raw = json!({"path": "p"});
        let v = normalize_llm_function_arguments(&raw);
        assert_eq!(v["path"], "p");
    }

    #[test]
    fn path_hint_reads_path_only() {
        let args = json!({"path": "src/lib.rs"});
        assert_eq!(path_hint_from_args(&args).as_deref(), Some("src/lib.rs"));
    }

    #[test]
    fn path_hint_ignores_legacy_file_keys() {
        let args = json!({"file_path": "x.rs", "target_file": "y.rs"});
        assert!(path_hint_from_args(&args).is_none());
    }

    #[test]
    fn command_hint_reads_command_only() {
        let args = json!({"command": "ls -la"});
        assert_eq!(command_hint_from_args(&args), Some("ls -la"));
    }

    #[test]
    fn command_hint_ignores_cmd_key() {
        let args = json!({"cmd": "whoami"});
        assert!(command_hint_from_args(&args).is_none());
    }

    #[test]
    fn permission_detail_execute_prefers_command_over_path() {
        let args = json!({"command": "ls", "path": "/tmp"});
        assert_eq!(
            permission_prompt_primary_detail("bash", &args).as_deref(),
            Some("ls")
        );
    }

    #[test]
    fn permission_detail_write_uses_path_not_command() {
        let args = json!({"command": "touch x", "path": "/p/x"});
        assert_eq!(
            permission_prompt_primary_detail("write_file", &args).as_deref(),
            Some("/p/x")
        );
    }

    #[test]
    fn permission_detail_read_falls_back_to_path() {
        let args = json!({"path": "/r"});
        assert_eq!(
            permission_prompt_primary_detail("read_file", &args).as_deref(),
            Some("/r")
        );
    }

    // ── MCP tool display tests ──

    #[test]
    fn mcp_args_summary_shows_key_values() {
        let args = json!({"query": "hello", "limit": 10});
        let detail = permission_prompt_primary_detail("mcp_search_server", &args).unwrap();
        assert!(detail.contains("query="));
        assert!(detail.contains("hello"));
        assert!(detail.contains("limit="));
    }

    #[test]
    fn mcp_args_summary_empty_args() {
        let detail = permission_prompt_primary_detail("mcp_server_tool", &json!({})).unwrap();
        assert_eq!(detail, "(no arguments)");
    }

    #[test]
    fn mcp_args_summary_truncates_long_values() {
        let long_val = "x".repeat(100);
        let args = json!({"data": long_val});
        let detail = permission_prompt_primary_detail("mcp_server_tool", &args).unwrap();
        assert!(detail.len() < 100);
        assert!(detail.contains("…"));
    }

    #[test]
    fn mcp_args_summary_limits_to_3_keys() {
        let args = json!({"a": 1, "b": 2, "c": 3, "d": 4, "e": 5});
        let detail = permission_prompt_primary_detail("mcp_server_tool", &args).unwrap();
        assert!(detail.contains("+2 more"));
    }
}
