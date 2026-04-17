//! Shared interpretation of edge tool outputs and stable tool-call keys (§5.5 dedup).
//!
//! Used by CLI `chat_stream` / `stream_render` and available for `bridge_inprocess` or server paths.

use serde_json::Value;

/// Determine whether a tool result string indicates an error.
///
/// For structured JSON results (our tools), checks `"ok": false` or a non-null
/// `"error"` field. For plain-text results, falls back to `starts_with("error")`.
pub fn is_tool_error(result_str: &str) -> bool {
    if let Ok(v) = serde_json::from_str::<Value>(result_str) {
        if let Some(ok_val) = v.get("ok").and_then(|o| o.as_bool()) {
            return !ok_val;
        }
        if let Some(err) = v.get("error") {
            return !err.is_null() && err.as_str() != Some("");
        }
        if v.get("error_code").is_some() {
            return true;
        }
        if v.get("status").and_then(|s| s.as_str()) == Some("error") {
            return true;
        }
    }
    result_str.to_lowercase().starts_with("error")
}

/// `status` string for cloud `POST /tools/result` from edge executor output prefixes.
///
/// Matches the CLI convention: `Error:`, `Unknown tool:`, and `Sandbox:` imply `"error"`;
/// everything else is reported as `"success"` (the body may still describe failure in JSON).
#[must_use]
pub fn cloud_tool_result_status_label(output: &str) -> &'static str {
    if output.starts_with("Error:")
        || output.starts_with("Unknown tool:")
        || output.starts_with("Sandbox:")
    {
        "error"
    } else {
        "success"
    }
}

/// Classification of tool errors for rollback policy decisions.
///
/// **HardError**: Unrecoverable errors that may have left inconsistent state.
/// Examples: permission denied, disk full, sandbox violation, git conflicts.
/// These SHOULD trigger rollback in plan-subtask context.
///
/// **SoftError**: Recoverable errors where the tool simply couldn't complete
/// but left no side effects. The agent can retry or try a different approach.
/// Examples: file not found, old_str not unique, old_str == new_str.
/// These should NOT trigger rollback — let the agent decide what to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolErrorSeverity {
    /// No error — tool succeeded.
    Success,
    /// Soft error: recoverable, no side effects, agent can decide next step.
    SoftError,
    /// Hard error: may have inconsistent state, should trigger rollback.
    HardError,
}

/// Well-known soft error patterns that should NOT trigger rollback.
///
/// These errors indicate the tool couldn't complete its task but left no
/// side effects. The agent can retry with different parameters or try
/// a different approach.
///
/// NOTE: timeout/transient errors are handled separately in `classify_tool_error`
/// because they need tool-type awareness (mutation tool timeout → HardError).
const SOFT_ERROR_PATTERNS: &[&str] = &[
    // str_replace benign failures
    "old_str and new_str are identical",
    "old_str not found",
    "must be unique",
    "no change needed",
    // File access — specific patterns to avoid false positives
    "file not found",
    "no such file",
    "no such file or directory",
    "path does not exist",
    // Read-only failures (no mutation)
    "nothing to commit",
    "no changes detected",
    // create_file on existing file — specific pattern
    "file already exists",
];

/// Timeout/transient error patterns — soft for read-only tools, hard for mutation tools.
const TRANSIENT_ERROR_PATTERNS: &[&str] = &[
    "timed out",
    "timeout",
    "connection refused",
    "network error",
];

/// Tools that mutate state — timeout on these is a hard error because partial writes may exist.
const MUTATION_TOOLS: &[&str] = &[
    "str_replace",
    "edit",
    "write_file",
    "create",
    "create_file",
    "bash",
    "powershell",
    "npm",
    "pip",
    "cargo",
    "git_commit",
    "git_push",
];

/// Well-known hard error patterns that SHOULD trigger rollback.
///
/// These errors indicate potential inconsistent state that requires cleanup.
const HARD_ERROR_PATTERNS: &[&str] = &[
    // Permission / access violations
    "permission denied",
    "access denied",
    "operation not permitted",
    // Resource exhaustion
    "no space left",
    "disk full",
    "out of memory",
    "cannot allocate",
    "too many open files",
    // Safety violations
    "sandbox:",
    "blocked by safety",
    "security violation",
    // Git state issues
    "merge conflict",
    "rebase in progress",
    "cannot lock",
    // Partial write failures
    "write failed",
    "partial write",
    "interrupted system call",
];

/// Classify a tool error output for rollback policy decisions.
///
/// Returns [`ToolErrorSeverity::Success`] for successful outputs,
/// [`ToolErrorSeverity::SoftError`] for recoverable errors,
/// [`ToolErrorSeverity::HardError`] for errors that may need rollback.
///
/// The `tool` parameter affects classification of timeout/transient errors:
/// - Mutation tools (bash, str_replace, etc.) + timeout → HardError (partial writes possible)
/// - Read-only tools (grep, view, etc.) + timeout → SoftError (no side effects)
pub fn classify_tool_error(tool: &str, output: &str) -> ToolErrorSeverity {
    // Not an error at all
    if cloud_tool_result_status_label(output) != "error" {
        return ToolErrorSeverity::Success;
    }

    let lower = output.to_lowercase();

    // Check hard errors first (more specific)
    for pattern in HARD_ERROR_PATTERNS {
        if lower.contains(pattern) {
            return ToolErrorSeverity::HardError;
        }
    }

    // Check transient errors — severity depends on tool type
    for pattern in TRANSIENT_ERROR_PATTERNS {
        if lower.contains(pattern) {
            let tool_lower = tool.to_lowercase();
            let is_mutation = MUTATION_TOOLS.iter().any(|m| tool_lower.contains(m));
            return if is_mutation {
                // Mutation tool timeout may have left partial state
                ToolErrorSeverity::HardError
            } else {
                // Read-only tool timeout is recoverable
                ToolErrorSeverity::SoftError
            };
        }
    }

    // Check soft errors
    for pattern in SOFT_ERROR_PATTERNS {
        if lower.contains(pattern) {
            return ToolErrorSeverity::SoftError;
        }
    }

    // Default: unknown errors are treated as hard (safer for rollback)
    ToolErrorSeverity::HardError
}

/// Returns true if this tool error should trigger turn rollback.
///
/// Only hard errors trigger rollback. Soft errors (recoverable, no side effects)
/// allow the turn to continue so the agent can retry or try alternatives.
///
/// The `tool` parameter is used to distinguish mutation vs read-only tools
/// for timeout/transient error handling.
pub fn tool_error_triggers_rollback(tool: &str, output: &str) -> bool {
    matches!(classify_tool_error(tool, output), ToolErrorSeverity::HardError)
}

/// Detect OS-level resource exhaustion in tool output that wasn't flagged by [`is_tool_error`].
///
/// Scans **per-line** to avoid false positives in source or large file contents.
pub fn is_resource_limit_output(output: &str) -> bool {
    if output.len() > 8192 {
        return false;
    }
    for line in output.lines() {
        let l = line.trim().to_lowercase();
        if l.is_empty() {
            continue;
        }
        if l.starts_with("//")
            || l.starts_with('#')
            || l.starts_with("/*")
            || l.starts_with('*')
            || l.contains("||")
            || l.contains("fn ")
            || l.contains("let ")
            || l.contains("if ")
            || l.contains("match ")
            || l.contains("def ")
            || l.contains("import ")
        {
            continue;
        }
        if l.contains("resource temporarily unavailable")
            || l.contains("cannot allocate memory")
            || l.contains("cannot fork")
            || l.contains("no space left on device")
            || l.contains("too many open files")
            || l.contains("device or resource busy")
        {
            return true;
        }
        if l.starts_with("bash: fork:") || l.starts_with("sh: fork:") {
            return true;
        }
        if l.len() < 120
            && (l.contains("enomem") || l.contains("enospc") || l.contains("ebusy"))
            && (l.starts_with("error") || l.starts_with("fatal") || l.starts_with("failed"))
        {
            return true;
        }
        if l == "killed" || l.starts_with("killed:") {
            return true;
        }
        if l.len() < 200
            && (l.contains("资源暂时不足") || l.contains("内存不足") || l.contains("系统资源"))
        {
            return true;
        }
    }
    false
}

/// Normalize tool arguments for deterministic comparison (paths, key order, nested objects).
pub fn normalize_tool_arguments(val: &Value) -> Value {
    match val {
        Value::String(s) => {
            let trimmed = s.trim();
            let normalized = trimmed.trim_end_matches('/');
            Value::String(normalized.to_string())
        }
        Value::Object(map) => {
            let mut sorted: serde_json::Map<String, Value> = serde_json::Map::new();
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for k in keys {
                sorted.insert(k.clone(), normalize_tool_arguments(&map[k]));
            }
            Value::Object(sorted)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(normalize_tool_arguments).collect()),
        other => other.clone(),
    }
}

/// Stable key for deduplicating `tool_request` (SSE) vs `tool_call` (same turn).
pub fn tool_dedup_signature(name: &str, args: &Value) -> String {
    let normalized = normalize_tool_arguments(args);
    format!(
        "{}:{}",
        name,
        serde_json::to_string(&normalized).unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_error_success_with_null_error_is_not_error() {
        let result = r#"{"ok":true,"tool":"github_list_prs","error":null,"count":6}"#;
        assert!(!is_tool_error(result));
    }

    #[test]
    fn tool_error_ok_false_is_error() {
        let result = r#"{"ok":false,"tool":"github_ci_status","error":"missing repo"}"#;
        assert!(is_tool_error(result));
    }

    #[test]
    fn tool_error_non_null_error_field_is_error() {
        let result = r#"{"error":"connection refused"}"#;
        assert!(is_tool_error(result));
    }

    #[test]
    fn tool_error_plain_text_error() {
        assert!(is_tool_error("Error: command not found"));
        assert!(is_tool_error("error reading file"));
    }

    #[test]
    fn tool_error_plain_text_success() {
        assert!(!is_tool_error("file contents here"));
        assert!(!is_tool_error("{}"));
        assert!(!is_tool_error("[]"));
    }

    #[test]
    fn cloud_tool_result_status_prefixes() {
        assert_eq!(cloud_tool_result_status_label("ok"), "success");
        assert_eq!(cloud_tool_result_status_label("Error: x"), "error");
        assert_eq!(cloud_tool_result_status_label("Unknown tool: y"), "error");
        assert_eq!(cloud_tool_result_status_label("Sandbox: z"), "error");
    }

    #[test]
    fn tool_error_empty_error_string_is_not_error() {
        let result = r#"{"error":""}"#;
        assert!(!is_tool_error(result));
    }

    #[test]
    fn tool_error_ok_true_with_error_string_trusts_ok_field() {
        let result = r#"{"ok":true,"error":"leftover field"}"#;
        assert!(!is_tool_error(result));
    }

    #[test]
    fn tool_error_nested_error_key_is_not_error() {
        let result = r#"{"ok":true,"data":{"error":"some inner issue"}}"#;
        assert!(!is_tool_error(result));
    }

    #[test]
    fn tool_error_array_response_is_not_error() {
        let result = r#"[{"name":"pr1"},{"name":"pr2"}]"#;
        assert!(!is_tool_error(result));
    }

    #[test]
    fn tool_error_error_count_field_is_not_error() {
        let result = r#"{"error_count":0,"status":"ok"}"#;
        assert!(!is_tool_error(result));
    }

    #[test]
    fn tool_error_html_error_page() {
        let result = "<html><body>error 502 Bad Gateway</body></html>";
        assert!(!is_tool_error(result));
    }

    #[test]
    fn tool_error_unicode_error_message() {
        let result = r#"{"ok":false,"error":"连接被拒绝"}"#;
        assert!(is_tool_error(result));
    }

    #[test]
    fn tool_error_ok_as_string_not_boolean() {
        let result = r#"{"ok":"false","error":"something"}"#;
        assert!(is_tool_error(result));
    }

    #[test]
    fn tool_error_empty_string_is_not_error() {
        assert!(!is_tool_error(""));
    }

    #[test]
    fn tool_error_whitespace_is_not_error() {
        assert!(!is_tool_error("   \n\t  "));
    }

    #[test]
    fn tool_error_bash_fork_resource_limit_is_not_detected_by_is_tool_error() {
        let fork_err = "bash: fork: retry: Resource temporarily unavailable\nbash: fork: Resource temporarily unavailable";
        assert!(!is_tool_error(fork_err));
        assert!(is_resource_limit_output(fork_err));
    }

    #[test]
    fn resource_limit_detects_oom_and_disk_full() {
        assert!(is_resource_limit_output("Cannot allocate memory"));
        assert!(is_resource_limit_output("No space left on device"));
        assert!(is_resource_limit_output("Too many open files"));
        assert!(is_resource_limit_output(
            "sh: fork: retry: Resource temporarily unavailable"
        ));
    }

    #[test]
    fn resource_limit_no_false_positive_on_git_fork() {
        assert!(!is_resource_limit_output(
            "Forked from user/repo\nfork: created successfully"
        ));
        assert!(!is_resource_limit_output(
            "commit abc123\nAuthor: user\n\n  fork: implement new feature"
        ));
    }

    #[test]
    fn resource_limit_no_false_positive_on_docs() {
        assert!(!is_resource_limit_output(
            "This function allocates memory for the buffer.\nSee out of memory handling docs."
        ));
        assert!(!is_resource_limit_output(
            "The fork() system call creates a new process."
        ));
    }

    #[test]
    fn is_tool_error_json_error_code() {
        assert!(is_tool_error(
            r#"{"error_code": 42, "message": "bad request"}"#
        ));
    }

    #[test]
    fn is_tool_error_json_status_error() {
        assert!(is_tool_error(r#"{"status": "error", "detail": "oops"}"#));
    }

    #[test]
    fn is_tool_error_json_status_ok_not_error() {
        assert!(!is_tool_error(r#"{"status": "ok", "data": []}"#));
    }

    #[test]
    fn is_tool_error_json_error_code_absent_not_error() {
        assert!(!is_tool_error(r#"{"result": "success"}"#));
    }

    #[test]
    fn resource_limit_enospc_in_error_context() {
        assert!(is_resource_limit_output("Error: ENOSPC"));
        assert!(is_resource_limit_output("error writing file: enospc"));
        assert!(is_resource_limit_output(
            "failed to write: ENOSPC (disk full)"
        ));
    }

    #[test]
    fn resource_limit_oom_killed() {
        assert!(is_resource_limit_output(
            "Killed: process ran out of memory"
        ));
        assert!(is_resource_limit_output("Killed"));
    }

    #[test]
    fn resource_limit_device_busy() {
        assert!(is_resource_limit_output("Error: Device or resource busy"));
    }

    #[test]
    fn resource_limit_chinese_oom() {
        assert!(is_resource_limit_output("错误：内存不足"));
    }

    #[test]
    fn resource_limit_chinese_system_resource() {
        assert!(is_resource_limit_output("错误：系统资源不足"));
    }

    #[test]
    fn resource_limit_no_false_positive_on_source_code_enospc() {
        let source_code = r#"
if let Err(e) = writeln!(file, "{line}") {
    if e.kind() == std::io::ErrorKind::Other
        || e.raw_os_error() == Some(28) // ENOSPC
        || e.to_string().contains("No space")
    {
        eprintln!("disk full");
    }
}
"#;
        assert!(!is_resource_limit_output(source_code));
    }

    #[test]
    fn resource_limit_no_false_positive_on_large_file() {
        let large = "x".repeat(9000);
        assert!(!is_resource_limit_output(&large));
        let mut large_with_pattern = "x".repeat(8200);
        large_with_pattern.push_str("\nbash: fork: Resource temporarily unavailable");
        assert!(!is_resource_limit_output(&large_with_pattern));
    }

    #[test]
    fn resource_limit_no_false_positive_on_comment_lines() {
        assert!(!is_resource_limit_output("// handle ENOMEM gracefully"));
        assert!(!is_resource_limit_output("# ENOSPC handling logic"));
        assert!(!is_resource_limit_output("/* EBUSY retry loop */"));
        assert!(!is_resource_limit_output("* Returns ENOMEM on failure"));
    }

    #[test]
    fn tool_dedup_signature_strips_trailing_slash() {
        let args = json!({"path": "src/", "pattern": "*.rs"});
        let sig1 = tool_dedup_signature("glob", &args);
        let args2 = json!({"path": "src", "pattern": "*.rs"});
        let sig2 = tool_dedup_signature("glob", &args2);
        assert_eq!(sig1, sig2);
    }

    #[test]
    fn tool_dedup_signature_sorts_keys() {
        let args1 = json!({"b": "2", "a": "1"});
        let args2 = json!({"a": "1", "b": "2"});
        assert_eq!(
            tool_dedup_signature("test", &args1),
            tool_dedup_signature("test", &args2)
        );
    }

    #[test]
    fn tool_dedup_signature_preserves_distinct_args() {
        let args1 = json!({"file": "a.rs"});
        let args2 = json!({"file": "b.rs"});
        assert_ne!(
            tool_dedup_signature("read_file", &args1),
            tool_dedup_signature("read_file", &args2)
        );
    }

    #[test]
    fn tool_dedup_signature_trims_whitespace() {
        let args1 = json!({"query": " hello world "});
        let args2 = json!({"query": "hello world"});
        assert_eq!(
            tool_dedup_signature("search", &args1),
            tool_dedup_signature("search", &args2)
        );
    }

    #[test]
    fn normalize_tool_arguments_handles_nested_objects() {
        let args = json!({"outer": {"z": 1, "a": 2}});
        let norm = normalize_tool_arguments(&args);
        let keys: Vec<&String> = norm["outer"].as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["a", "z"]);
    }

    #[test]
    fn normalize_tool_arguments_preserves_numbers_and_bools() {
        let args = json!({"count": 5, "verbose": true});
        let norm = normalize_tool_arguments(&args);
        assert_eq!(norm["count"], 5);
        assert_eq!(norm["verbose"], true);
    }

    // ── Error severity classification tests ──────────────────────────────────

    #[test]
    fn classify_success_output() {
        assert_eq!(
            classify_tool_error("grep", "File created successfully"),
            ToolErrorSeverity::Success
        );
        assert_eq!(
            classify_tool_error("str_replace", "Replaced 3 occurrences"),
            ToolErrorSeverity::Success
        );
    }

    #[test]
    fn classify_soft_error_str_replace_identical() {
        let output = "Error: old_str and new_str are identical — no change needed";
        assert_eq!(classify_tool_error("str_replace", output), ToolErrorSeverity::SoftError);
        assert!(!tool_error_triggers_rollback("str_replace", output));
    }

    #[test]
    fn classify_soft_error_file_not_found() {
        let output = "Error: file not found: /path/to/missing.rs";
        assert_eq!(classify_tool_error("read_file", output), ToolErrorSeverity::SoftError);
        assert!(!tool_error_triggers_rollback("read_file", output));
    }

    #[test]
    fn classify_soft_error_not_unique() {
        let output = "Error: old_str found 3 times — must be unique";
        assert_eq!(classify_tool_error("str_replace", output), ToolErrorSeverity::SoftError);
        assert!(!tool_error_triggers_rollback("str_replace", output));
    }

    #[test]
    fn classify_timeout_soft_for_read_only_tool() {
        // Read-only tool timeout → SoftError
        let output = "Error: command timed out after 30s";
        assert_eq!(classify_tool_error("grep", output), ToolErrorSeverity::SoftError);
        assert!(!tool_error_triggers_rollback("grep", output));
    }

    #[test]
    fn classify_timeout_hard_for_mutation_tool() {
        // Mutation tool timeout → HardError (may have partial writes)
        let output = "Error: command timed out after 30s";
        assert_eq!(classify_tool_error("bash", output), ToolErrorSeverity::HardError);
        assert!(tool_error_triggers_rollback("bash", output));
        
        // str_replace timeout
        assert_eq!(classify_tool_error("str_replace", output), ToolErrorSeverity::HardError);
        
        // write_file timeout
        assert_eq!(classify_tool_error("write_file", output), ToolErrorSeverity::HardError);
    }

    #[test]
    fn classify_hard_error_permission_denied() {
        let output = "Error: permission denied: /etc/passwd";
        assert_eq!(classify_tool_error("read_file", output), ToolErrorSeverity::HardError);
        assert!(tool_error_triggers_rollback("read_file", output));
    }

    #[test]
    fn classify_hard_error_disk_full() {
        let output = "Error: no space left on device";
        assert_eq!(classify_tool_error("write_file", output), ToolErrorSeverity::HardError);
        assert!(tool_error_triggers_rollback("write_file", output));
    }

    #[test]
    fn classify_hard_error_sandbox_violation() {
        let output = "Sandbox: blocked write to /etc/hosts";
        assert_eq!(classify_tool_error("bash", output), ToolErrorSeverity::HardError);
        assert!(tool_error_triggers_rollback("bash", output));
    }

    #[test]
    fn classify_hard_error_git_conflict() {
        let output = "Error: merge conflict in src/main.rs";
        assert_eq!(classify_tool_error("git_pull", output), ToolErrorSeverity::HardError);
        assert!(tool_error_triggers_rollback("git_pull", output));
    }

    #[test]
    fn classify_unknown_error_defaults_to_hard() {
        // Unknown errors should be treated as hard (safer for rollback)
        let output = "Error: some unknown catastrophic failure";
        assert_eq!(classify_tool_error("unknown_tool", output), ToolErrorSeverity::HardError);
        assert!(tool_error_triggers_rollback("unknown_tool", output));
    }

    #[test]
    fn classify_hard_wins_over_soft_when_both_present() {
        // Issue #141 review comment: add test for overlapping patterns
        let output = "Error: permission denied — file does not exist";
        assert_eq!(classify_tool_error("read_file", output), ToolErrorSeverity::HardError);
    }

    #[test]
    fn classify_connection_refused_by_tool_type() {
        let output = "Error: connection refused";
        // Read-only → SoftError
        assert_eq!(classify_tool_error("curl", output), ToolErrorSeverity::SoftError);
        // Mutation → HardError
        assert_eq!(classify_tool_error("bash", output), ToolErrorSeverity::HardError);
    }
}
