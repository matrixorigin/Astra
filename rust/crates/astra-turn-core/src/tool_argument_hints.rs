//! Normalized reads of OpenAI-style `function.arguments` (object or stringified JSON).
//!
//! **Canonical keys only** (no legacy aliases): file targets use `path`; shell tools use `command`.
//! Edge executors and tool schemas should emit these names; hints do not read `file_path`, `target_file`, or `cmd`.

use serde_json::Value;

use super::cloud_approval_policy::{CloudGatedToolKind, cloud_gated_tool_kind_with_args};

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

/// Raw hint used for **permission rule matching** — `starts_with`
/// checks against deny/allow rule patterns depend on this being the
/// naked `command` / `path` value, not a formatted preview.
///
/// Must NOT be changed to wrap or decorate the value: rule patterns
/// like `bash(rm -rf:*)` match against raw commands, so returning
/// "$ rm -rf ..." would silently stop blocking them. Previously this
/// function also drove the approval-dialog display label, which
/// coupled the two concerns; now the display label is generated
/// separately via [`crate::tool_preview::render_preview`].
pub fn permission_prompt_primary_detail(tool_name: &str, args: &Value) -> Option<String> {
    if tool_name.starts_with("mcp_") {
        return Some(crate::tool_preview::mcp_args_summary(args));
    }
    match cloud_gated_tool_kind_with_args(tool_name, Some(args)) {
        Some(CloudGatedToolKind::Execute) => command_hint_from_args(args).map(String::from),
        Some(CloudGatedToolKind::Write) => path_hint_from_args(args),
        None => command_hint_from_args(args)
            .map(String::from)
            .or_else(|| path_hint_from_args(args)),
    }
}

/// Human-readable label for the **approval dialog** — the one-line
/// preview shown above the Accept / Reject buttons. Delegates to the
/// shared [`crate::tool_preview::render_preview`] so it matches what
/// the scrollback renders when the tool actually runs.
///
/// Kept separate from [`permission_prompt_primary_detail`] because
/// the two have different contracts: rule matching wants raw args,
/// display wants pretty labels.
pub fn permission_prompt_display_label(tool_name: &str, args: &Value) -> String {
    crate::tool_preview::render_preview(
        tool_name,
        args,
        crate::tool_preview::PreviewStyle::Concise,
        80,
    )
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

    // `permission_prompt_primary_detail` returns the RAW arg for rule
    // matching (rules use starts_with checks). The pretty display
    // label lives in `permission_prompt_display_label` — separating
    // the two avoids silently bypassing deny rules when the display
    // format changes.

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

    #[test]
    fn permission_display_label_uses_rich_preview() {
        let args = json!({"command": "ls -la"});
        assert_eq!(permission_prompt_display_label("bash", &args), "$ ls -la");
        let args = json!({"path": "foo.txt"});
        assert_eq!(
            permission_prompt_display_label("write_file", &args),
            "Writing: foo.txt"
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

    #[test]
    fn mcp_args_summary_long_unicode_no_panic() {
        let long_val = format!("{}end", "数据—".repeat(25));
        let args = json!({"data": long_val});
        let detail = permission_prompt_primary_detail("mcp_server_tool", &args).unwrap();
        assert!(detail.contains("data="));
        assert!(detail.contains('…'));
    }
}
