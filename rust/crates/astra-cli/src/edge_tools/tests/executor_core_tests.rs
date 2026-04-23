use super::*;
use astra_services::session_journal::{self, JournalDirGuard, JournalEvent, JournalEventType};
use astra_services::session_workspace::{self, ContextTraceSignal, WorkspaceMetadata};
use chrono::Utc;

// ── ToolExecutor ──────────────────────────────────────────────────────────

#[test]
fn executor_tool_count_matches_schemas() {
    let executor = test_executor();
    assert_eq!(executor.tool_count(), all_tool_schemas().len());
}

#[test]
fn executor_tool_names_match_schemas() {
    let executor = test_executor();
    let names = executor.tool_names();
    assert_eq!(names.len(), all_tool_schemas().len());
    assert!(names.contains(&"bash".to_string()));
}

#[tokio::test]
async fn execute_unknown_tool_returns_error() {
    let executor = test_executor();
    let result = executor.execute("nonexistent_tool", &json!({})).await;
    assert!(
        result.starts_with("Error:"),
        "unknown tool must return an Error: prefix (plain-text error contract) — got: {result}"
    );
    assert!(
        result.contains("'nonexistent_tool'"),
        "error must quote the rejected tool name — got: {result}"
    );
    assert!(
        result.contains("not available"),
        "error must use the canonical 'not available' phrasing so dispatchers can pattern-match — got: {result}"
    );
}

#[tokio::test]
async fn execute_delegate_returns_deferred_acknowledgment() {
    let executor = test_executor();
    let result = executor.execute("delegate", &json!({})).await;
    assert!(
        result.contains("Delegation request acknowledged"),
        "got: {result}"
    );
}

#[tokio::test]
async fn execute_reflect_returns_placeholder() {
    let executor = test_executor();
    let result = executor.execute("reflect", &json!({"focus": "auto"})).await;
    assert!(result.contains("reflect_requires_session"), "got: {result}");
}

#[tokio::test]
async fn execute_reflect_uses_local_surface_with_session() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(temp.path());
    let session_id = "executor-reflect-session";
    let mut ws = WorkspaceMetadata::with_context(session_id, "gpt-5.4", "/repo", Some("main"));
    ws.turn_count = 3;
    ws.last_context_trace = Some(ContextTraceSignal {
        turn_id: "turn-3".to_string(),
        captured_at: Some(Utc::now().to_rfc3339()),
        tool_selection: None,
        memory: None,
        history: None,
        budget: Some(
            astra_services::session_workspace::ContextTraceBudgetSignal {
                max_tokens: 10000,
                total_used: 8500,
                budget_pressure: 0.85,
                compression_triggered: false,
            },
        ),
        timing: None,
        explanations: vec![],
    });
    session_workspace::write_workspace(&ws).unwrap();

    session_journal::JournalWriter::new(session_id)
        .unwrap()
        .append(&JournalEvent {
            event_type: JournalEventType::TurnError,
            ts: Utc::now().to_rfc3339(),
            session_id: Some(session_id.to_string()),
            turn: Some(3),
            agentic_step: None,
            model: Some("gpt-5.4".to_string()),
            user_input: Some("debug".to_string()),
            assistant_output: None,
            tool_count: Some(1),
            tokens_in: Some(10),
            tokens_out: Some(20),
            duration_ms: Some(100),
            error: Some("bash failed".to_string()),
            config_key: None,
            config_value: None,
            turns_compacted: None,
            facts_stored: None,
            tools_selected: Some(vec!["bash".to_string()]),
            selected_skills: None,
            tools_used: Some(vec!["bash".to_string()]),
            tool_calls: Some(vec![session_journal::ToolCallRecord {
                name: "bash".to_string(),
                ok: false,
                ms: 100,
                error: Some("command failed".to_string()),
                input_bytes: None,
                output_bytes: None,
                args_preview: None,
                result_preview: None,
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
            }]),
            budget_used: Some(8500),
            budget_pressure: Some(0.85),
            stall_type: None,
            metadata: None,
            plan_subtask_id: None,
            ttft_ms: None,
            context_ms: None,
            selector_strategy: None,
            selector_ms: None,
            selector_tokens_in: None,
            selector_tokens_out: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            memoria_ms: None,
            session_lineage: None,
            coordination: None,
            edge_policy: None,
            selection_trace: None,
            context_assembly_trace: None,
            selector_confidence: None,
            routing_domain_hint: None,
            entity_learn_skipped_no_domain: false,
            round: None,
            tool_calls_returned: None,
            offset_ms: None,
            llm_rounds: None,
            total_llm_ms: None,
            total_tool_ms: None,
            parent_event_id: None,
            git_head: None,
            git_branch: None,
        })
        .unwrap();

    let executor = ToolExecutor::new(temp.path().to_path_buf()).with_active_session_id(session_id);
    let result = executor
        .execute("reflect", &json!({"focus": "performance"}))
        .await;
    let value: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(value["session_id"], session_id);
    assert_eq!(value["focus"], "performance");
    assert_eq!(
        value["reflection_context"]["tool_stats"][0]["tool_name"],
        "bash"
    );
}

#[test]
fn budget_pressure_defaults_to_zero() {
    let executor = test_executor();
    assert_eq!(executor.get_budget_pressure(), 0.0);
}

#[test]
fn budget_pressure_set_and_get() {
    let executor = test_executor();
    executor.set_budget_pressure(0.6);
    assert!((executor.get_budget_pressure() - 0.6).abs() < 1e-10);
}

#[test]
fn budget_pressure_clamps_to_range() {
    let executor = test_executor();
    executor.set_budget_pressure(1.5);
    assert_eq!(executor.get_budget_pressure(), 1.0);
    executor.set_budget_pressure(-0.5);
    assert_eq!(executor.get_budget_pressure(), 0.0);
}

// ── truncate_output ─────────────────────────────────────────────────────

#[test]
fn truncate_output_ascii_no_change() {
    let input = "hello world".to_string();
    let result = truncate_output(input.clone(), 100);
    assert_eq!(result, input);
}

#[test]
fn truncate_output_ascii_truncates() {
    let input = "hello world".to_string();
    let result = truncate_output(input, 5);
    assert!(result.starts_with("hello"));
    assert!(result.contains("[truncated]"));
}

#[test]
fn truncate_output_utf8_boundary_no_panic() {
    // 🔥 is 4 bytes, "ab🔥cd" = 2+4+2 = 8 bytes
    let input = "ab🔥cd".to_string();
    // Truncate at byte 3 — inside the 🔥 (bytes 2..5)
    let result = truncate_output(input, 3);
    // Should truncate at char boundary (byte 2, before 🔥)
    assert!(result.starts_with("ab"), "got: {result}");
    assert!(result.contains("[truncated]"));
}

#[test]
fn truncate_output_cjk_boundary_no_panic() {
    // Chinese chars are 3 bytes each
    let input = "你好世界".to_string(); // 12 bytes
    let result = truncate_output(input, 7); // Between 2nd and 3rd char
    assert!(result.contains("[truncated]"));
    // Should not panic — regression for char boundary issue
}
