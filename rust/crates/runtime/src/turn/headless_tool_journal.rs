//! `ToolCallRecord` rows for headless early-exit paths (duplicate, cache hit, unknown tool).

use mo_agent_services::session_journal::ToolCallRecord;

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
    }
}

#[must_use]
pub fn journal_record_unknown_tool(name: String) -> ToolCallRecord {
    ToolCallRecord {
        name: name.clone(),
        ok: false,
        ms: 0,
        error: Some(format!("unknown_tool: {name}")),
        input_bytes: None,
        output_bytes: None,
        args_preview: None,
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
        let r = journal_record_unknown_tool("nope".into());
        assert!(!r.ok);
        assert_eq!(r.error.as_deref(), Some("unknown_tool: nope"));
    }
}
