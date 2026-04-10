//! `ToolCallRecord` rows for headless early-exit paths.

use astra_services::session_journal::ToolCallRecord;

#[must_use]
pub fn journal_record_duplicate_within_turn(
    name: String,
    args_preview: Option<String>,
) -> ToolCallRecord {
    ToolCallRecord {
        name,
        ok: true,
        ms: 0,
        error: Some("duplicate_within_turn".to_string()),
        input_bytes: None,
        output_bytes: None,
        args_preview,
        result_preview: None,
    }
}

#[must_use]
pub fn journal_record_cross_turn_cache_hit(
    name: String,
    output_len: u32,
    args_preview: Option<String>,
) -> ToolCallRecord {
    ToolCallRecord {
        name,
        ok: true,
        ms: 0,
        error: Some("cached_cross_turn".to_string()),
        input_bytes: None,
        output_bytes: Some(output_len),
        args_preview,
        result_preview: None,
    }
}

#[must_use]
pub fn journal_record_unknown_tool(name: String, tool_elapsed_ms: u64) -> ToolCallRecord {
    ToolCallRecord {
        name: name.clone(),
        ok: false,
        ms: tool_elapsed_ms,
        error: Some(format!("unknown_tool: {name}")),
        input_bytes: None,
        output_bytes: None,
        args_preview: None,
        result_preview: None,
    }
}

#[must_use]
pub fn journal_record_blocked_tool(
    name: String,
    reason: String,
    args_preview: Option<String>,
    tool_elapsed_ms: u64,
) -> ToolCallRecord {
    ToolCallRecord {
        name,
        ok: false,
        ms: tool_elapsed_ms,
        error: Some(format!("blocked_tool: {reason}")),
        input_bytes: None,
        output_bytes: None,
        args_preview,
        result_preview: None,
    }
}

#[must_use]
pub fn journal_record_executed_tool_call(
    name: String,
    is_err: bool,
    tool_elapsed_ms: u64,
    args_size: u32,
    result_str: &str,
    args_preview: Option<String>,
) -> ToolCallRecord {
    // Truncate to 500 chars for cloud audit (up from 200, multi-line)
    let preview: String = result_str.chars().take(500).collect();
    let result_preview = if preview.is_empty() {
        None
    } else {
        Some(if result_str.chars().count() > 500 {
            format!("{preview}…")
        } else {
            preview
        })
    };

    ToolCallRecord {
        name,
        ok: !is_err,
        ms: tool_elapsed_ms,
        error: if is_err {
            // Keep up to 500 chars of error (multi-line) for better diagnostics
            Some(result_str.chars().take(500).collect())
        } else {
            None
        },
        input_bytes: Some(args_size),
        output_bytes: Some(result_str.len() as u32),
        args_preview,
        result_preview,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_record_fields() {
        let r = journal_record_duplicate_within_turn("bash".into(), Some("x".into()));
        assert!(r.ok);
        assert_eq!(r.error.as_deref(), Some("duplicate_within_turn"));
    }

    #[test]
    fn cache_hit_record_has_output_bytes() {
        let r = journal_record_cross_turn_cache_hit("read_file".into(), 12, None);
        assert_eq!(r.output_bytes, Some(12));
    }

    #[test]
    fn unknown_tool_error_tag() {
        let r = journal_record_unknown_tool("nope".into(), 7);
        assert!(!r.ok);
        assert_eq!(r.ms, 7);
        assert_eq!(r.error.as_deref(), Some("unknown_tool: nope"));
    }

    #[test]
    fn blocked_tool_error_tag() {
        let r = journal_record_blocked_tool(
            "bash".into(),
            "denied by policy".into(),
            Some(r#"{"command":"echo hi"}"#.into()),
            9,
        );
        assert!(!r.ok);
        assert_eq!(r.ms, 9);
        assert_eq!(r.error.as_deref(), Some("blocked_tool: denied by policy"));
        assert_eq!(r.args_preview.as_deref(), Some(r#"{"command":"echo hi"}"#));
    }

    #[test]
    fn executed_record_truncates_error_to_500_chars() {
        let r =
            journal_record_executed_tool_call("bash".into(), true, 10, 2, "first line\nrest", None);
        // Now keeps multi-line errors (up to 500 chars)
        assert_eq!(r.error.as_deref(), Some("first line\nrest"));
        assert_eq!(r.output_bytes, Some(15));
        // result_preview also populated for errors
        assert_eq!(r.result_preview.as_deref(), Some("first line\nrest"));
    }

    #[test]
    fn executed_record_result_preview_truncates_long_output() {
        let long_output = "x".repeat(600);
        let r = journal_record_executed_tool_call("grep".into(), false, 5, 10, &long_output, None);
        assert!(r.ok);
        assert!(r.error.is_none());
        let preview = r.result_preview.unwrap();
        assert_eq!(preview.chars().count(), 501); // 500 + "…"
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn executed_record_error_truncates_at_500_chars() {
        let long_error = "E".repeat(600);
        let r = journal_record_executed_tool_call("bash".into(), true, 5, 10, &long_error, None);
        assert_eq!(r.error.unwrap().len(), 500);
    }
}
