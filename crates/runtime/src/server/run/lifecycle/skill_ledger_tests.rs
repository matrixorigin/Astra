use super::*;
use crate::turn::agentic_loop::{
    finalization::run_agentic_loop_with_host,
    host::tests::{
        MockHost, edge_tool_result, make_edge_tool, make_state, server_tool_result, text_result,
    },
};

#[tokio::test]
async fn failed_skill_with_suppressed_sibling_and_new_call_retry_can_complete() {
    let mut retry = make_edge_tool("bash", "Created report.md");
    retry.request_id = "call-create-report-retry".into();
    retry.args = json!({"command": "create-report"});
    let mut verify = make_edge_tool("read_file", "Report contents verified");
    verify.request_id = "call-verify-report".into();
    verify.args = json!({"path": "report.md"});
    let mut host = MockHost::new(vec![
        server_tool_result(
            vec![
                json!({
                    "id": "call-missing-skill",
                    "type": "function",
                    "function": {"name": "skill", "arguments": r#"{"skill_name":"missing-skill"}"#}
                }),
                json!({
                    "id": "call-create-report-original",
                    "type": "function",
                    "function": {"name": "bash", "arguments": r#"{"command":"create-report"}"#}
                }),
            ],
            Vec::new(),
            100,
            50,
            None,
        ),
        edge_tool_result(vec![retry], 100, 50, None),
        edge_tool_result(vec![verify], 100, 50, None),
        text_result("The report is ready.", 100, 20, None),
    ])
    .with_valid_tools(&["skill", "bash", "read_file"]);
    let mut state = make_state();
    state.current_run_id = Some("run-skill-sibling-retry".into());
    state
        .messages
        .push(json!({"role":"user", "content":"Create and verify a report."}));
    state.skills.resolver = Some(static_skill_resolver("missing-skill"));

    // Exercise real interception, model re-entry, receipt folding and the
    // server completion gate. Only the model and external executor are scripted.
    let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
    assert!(
        matches!(outcome, Ok(AgenticLoopOutcome::Completed)),
        "{outcome:?}"
    );
    let aggregate = state.tool_ledger_receipt.canonical_aggregate();
    enforce_completed_tool_ledger_closure(&outcome, &mut state);
    let (_, status, error) =
        AgenticRunLifecycleService::finalize_run_events(outcome, Vec::new(), &state);
    assert_eq!(
        status,
        RunStatus::Completed,
        "receipt={aggregate:?}; error={error:?}"
    );
    assert_eq!(error, None);
    assert_eq!(state.final_text, "The report is ready.");

    use astra_services::session_journal::ToolCallDisposition;
    for (call_id, disposition, ok) in [
        ("call-missing-skill", ToolCallDisposition::Executed, false),
        (
            "call-create-report-original",
            ToolCallDisposition::Suppressed,
            false,
        ),
        (
            "call-create-report-retry",
            ToolCallDisposition::Executed,
            true,
        ),
        ("call-verify-report", ToolCallDisposition::Executed, true),
    ] {
        let record = state
            .stall
            .tool_call_records
            .iter()
            .find(|record| record.tool_call_id.as_deref() == Some(call_id))
            .unwrap_or_else(|| panic!("missing terminal record for {call_id}"));
        assert_eq!(record.effective_disposition(), disposition, "{call_id}");
        assert_eq!(record.ok, ok, "{call_id}");
    }
    assert!(aggregate.is_complete_for(state.total_tool_calls));
}
