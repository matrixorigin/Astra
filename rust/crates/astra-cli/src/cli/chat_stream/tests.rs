// ── ToolCallRecord ingestion completeness ──

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
        file_path: None,
        surgically_removed: None,
        original_tool_name: None,
        ..Default::default()
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
