//! Normalized reads of OpenAI-style `function.arguments` (object or stringified JSON).
//! Used for `approval_required` path hints and CLI permission prompts (thin-client tool delivery).

use serde_json::Value;

use super::cloud_approval_policy::{cloud_gated_tool_kind, CloudGatedToolKind};

/// Parse `function.arguments` from an LLM tool call: either a JSON object or a string of JSON.
pub fn normalize_llm_function_arguments(arguments: &Value) -> Value {
    match arguments {
        Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| Value::Object(Default::default())),
        v => v.clone(),
    }
}

/// Path-like keys commonly used across edge tools and providers.
pub fn path_hint_from_args(args: &Value) -> Option<String> {
    args.get("path")
        .or_else(|| args.get("file_path"))
        .or_else(|| args.get("target_file"))
        .and_then(Value::as_str)
        .map(String::from)
}

/// Shell-style tools: `command` (OpenAI) or `cmd` (legacy / aliases).
pub fn command_hint_from_args(args: &Value) -> Option<&str> {
    args.get("command")
        .or_else(|| args.get("cmd"))
        .and_then(Value::as_str)
}

/// One-line detail next to the CLI permission icon (aligned with cloud `approval_required` path).
pub fn permission_prompt_primary_detail(tool_name: &str, args: &Value) -> Option<String> {
    match cloud_gated_tool_kind(tool_name) {
        Some(CloudGatedToolKind::Execute) => command_hint_from_args(args).map(String::from),
        Some(CloudGatedToolKind::Write) => path_hint_from_args(args),
        None => command_hint_from_args(args)
            .map(String::from)
            .or_else(|| path_hint_from_args(args)),
    }
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
    fn path_hint_prefers_path_over_file_path() {
        let args = json!({"path": "a", "file_path": "b"});
        assert_eq!(path_hint_from_args(&args).as_deref(), Some("a"));
    }

    #[test]
    fn path_hint_falls_back_to_target_file() {
        let args = json!({"target_file": "z.rs"});
        assert_eq!(path_hint_from_args(&args).as_deref(), Some("z.rs"));
    }

    #[test]
    fn command_hint_prefers_command_over_cmd() {
        let args = json!({"command": "ls", "cmd": "echo"});
        assert_eq!(command_hint_from_args(&args), Some("ls"));
    }

    #[test]
    fn command_hint_uses_cmd_when_command_absent() {
        let args = json!({"cmd": "whoami"});
        assert_eq!(command_hint_from_args(&args), Some("whoami"));
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
}
