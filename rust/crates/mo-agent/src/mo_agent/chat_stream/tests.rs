// ── ToolCallRecord ingestion completeness ──

/// Verify ToolCallRecord can represent all early-exit paths
/// (duplicate, cached, unknown tool, permission denied) so that
/// DB ingestion captures 100% of tool_calls.
#[test]
fn tool_call_record_covers_early_exit_paths() {
    use astra_services::session_journal::ToolCallRecord;

    // Duplicate within turn
    let dup = ToolCallRecord {
        name: "read_file".to_string(),
        ok: true,
        ms: 0,
        error: Some("duplicate_within_turn".to_string()),
        input_bytes: None,
        output_bytes: None,
        args_preview: Some("src/main.rs".to_string()),
        result_preview: None,
    };
    assert!(dup.ok);
    assert_eq!(dup.ms, 0);

    // Cross-turn cache hit
    let cached = ToolCallRecord {
        name: "grep".to_string(),
        ok: true,
        ms: 0,
        error: Some("cached_cross_turn".to_string()),
        input_bytes: None,
        output_bytes: Some(500),
        args_preview: Some("/TODO/ in src/".to_string()),
        result_preview: None,
    };
    assert!(cached.ok);

    // Unknown tool
    let unknown = ToolCallRecord {
        name: "nonexistent_tool".to_string(),
        ok: false,
        ms: 0,
        error: Some("unknown_tool: nonexistent_tool".to_string()),
        input_bytes: None,
        output_bytes: None,
        args_preview: None,
        result_preview: None,
    };
    assert!(!unknown.ok);
    assert!(unknown.error.as_ref().unwrap().starts_with("unknown_tool:"));

    // Permission denied
    let denied = ToolCallRecord {
        name: "bash".to_string(),
        ok: false,
        ms: 0,
        error: Some("permission_denied".to_string()),
        input_bytes: None,
        output_bytes: None,
        args_preview: Some("rm -rf /".to_string()),
        result_preview: None,
    };
    assert!(!denied.ok);

    // All records serialize cleanly (required for DB ingestion)
    let records = vec![dup, cached, unknown, denied];
    let json = serde_json::to_string(&records).unwrap();
    assert!(json.contains("duplicate_within_turn"));
    assert!(json.contains("cached_cross_turn"));
    assert!(json.contains("unknown_tool"));
    assert!(json.contains("permission_denied"));
}

/// ToolCallRecord round-trips through JSON correctly.
#[test]
fn tool_call_record_json_roundtrip() {
    use astra_services::session_journal::ToolCallRecord;

    let original = ToolCallRecord {
        name: "web_fetch".to_string(),
        ok: true,
        ms: 42,
        error: None,
        input_bytes: Some(100),
        output_bytes: Some(5000),
        args_preview: Some("https://example.com".to_string()),
        result_preview: None,
    };
    let json = serde_json::to_string(&original).unwrap();
    let restored: ToolCallRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.name, "web_fetch");
    assert_eq!(restored.ms, 42);
    assert!(restored.ok);
    assert!(restored.error.is_none());
    // error field should be absent when None (skip_serializing_if)
    assert!(!json.contains("error"));
}
