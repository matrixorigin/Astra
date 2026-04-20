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
        file_path: None,
        surgically_removed: None,
        original_tool_name: None,
        ..Default::default()
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
        file_path: None,
        surgically_removed: None,
        original_tool_name: None,
        ..Default::default()
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
        file_path: None,
        surgically_removed: None,
        original_tool_name: None,
        ..Default::default()
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
        file_path: None,
        surgically_removed: None,
        original_tool_name: None,
        ..Default::default()
    }
}

#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn journal_record_executed_tool_call(
    name: String,
    is_err: bool,
    tool_elapsed_ms: u64,
    args_size: u32,
    result_str: &str,
    args_preview: Option<String>,
    file_path: Option<String>,
    args_full: Option<String>,
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

    // Store full result up to 50 KB. Larger outputs (bash, read_file) are
    // already persisted to tool-results/<call_id>.txt by tool_result_storage.
    const MAX_RESULT_FULL_BYTES: usize = 50_000;
    let result_full = if result_str.is_empty() || result_str.len() > MAX_RESULT_FULL_BYTES {
        None
    } else {
        Some(result_str.to_string())
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
        file_path,
        surgically_removed: None,
        original_tool_name: None,
        args_full,
        result_full,
        ..Default::default()
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
        let r = journal_record_executed_tool_call(
            "bash".into(),
            true,
            10,
            2,
            "first line\nrest",
            None,
            None,
            None,
        );
        // Now keeps multi-line errors (up to 500 chars)
        assert_eq!(r.error.as_deref(), Some("first line\nrest"));
        assert_eq!(r.output_bytes, Some(15));
        // result_preview also populated for errors
        assert_eq!(r.result_preview.as_deref(), Some("first line\nrest"));
    }

    #[test]
    fn executed_record_result_preview_truncates_long_output() {
        let long_output = "x".repeat(600);
        let r = journal_record_executed_tool_call(
            "grep".into(),
            false,
            5,
            10,
            &long_output,
            None,
            None,
            None,
        );
        assert!(r.ok);
        assert!(r.error.is_none());
        let preview = r.result_preview.unwrap();
        assert_eq!(preview.chars().count(), 501); // 500 + "…"
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn executed_record_error_truncates_at_500_chars() {
        let long_error = "E".repeat(600);
        let r =
            journal_record_executed_tool_call("bash".into(), true, 5, 10, &long_error, None, None, None);
        assert_eq!(r.error.unwrap().len(), 500);
    }

    #[test]
    fn executed_record_stores_full_args_and_result() {
        let full_args = r#"{"path":"src/main.rs","offset":100,"limit":50}"#;
        let full_result = "x".repeat(1000);
        let r = journal_record_executed_tool_call(
            "read_file".into(),
            false,
            12,
            full_args.len() as u32,
            &full_result,
            Some("src/main.rs".into()),
            Some("src/main.rs".into()),
            Some(full_args.to_string()),
        );
        assert_eq!(r.args_full.as_deref(), Some(full_args), "args_full must store untruncated args");
        assert_eq!(r.result_full.as_ref().map(|s| s.len()), Some(1000), "result_full must store untruncated result");
        // previews are still truncated
        assert!(r.result_preview.unwrap().chars().count() <= 501);
    }

    #[test]
    fn executed_record_full_fields_none_when_not_provided() {
        let r = journal_record_executed_tool_call(
            "bash".into(), false, 5, 10, "ok", None, None, None,
        );
        assert!(r.args_full.is_none());
        assert_eq!(r.result_full.as_deref(), Some("ok"));
    }

    #[test]
    fn executed_record_result_full_capped_at_50kb() {
        let large = "x".repeat(51_000);
        let r = journal_record_executed_tool_call(
            "bash".into(), false, 5, 10, &large, None, None, None,
        );
        assert!(r.result_full.is_none(), "result_full must be None for outputs > 50KB");
        assert!(r.result_preview.is_some(), "result_preview still populated");
    }
}
