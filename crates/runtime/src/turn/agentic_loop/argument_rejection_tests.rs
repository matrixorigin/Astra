use super::*;
use crate::turn::agentic_loop::host::run_agentic_loop_with_host;
use crate::turn::agentic_loop::host::tests::{
    MockHost, edge_runtime_environment_fields, edge_tool_result, make_edge_tool_with_args,
    make_state, text_result,
};
use astra_services::session_journal::ToolCallDisposition;
use serde_json::json;

#[tokio::test]
async fn actual_pure_edit_rejection_survives_callback_journal_without_recovery_credit() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("f.txt");
    std::fs::write(&path, "before\n").unwrap();
    for args in [
        json!({"path":"f.txt", "old_str":"before", "new_str":"before"}),
        json!({"old_str":"before", "new_str":"after"}),
    ] {
        let produced = astra_tools::fs_ops::str_replace(directory.path(), &args);
        assert!(produced.is_error);
        let mut callback = make_edge_tool_with_args("str_replace", args, &produced.output);
        callback.status = "failed".into();
        callback.tool_result_fields = Some(edge_runtime_environment_fields());
        callback
            .tool_result_fields
            .as_mut()
            .unwrap()
            .extend(produced.metadata.unwrap());
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![callback], 10, 2, None),
            text_result(
                "The requested replacement was identical; no edit was made.",
                10,
                2,
                None,
            ),
        ])
        .with_valid_tools(&["str_replace"]);
        let mut state = make_state();
        let _ = run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(state.stall.tool_call_records.len(), 1);
        let record = &state.stall.tool_call_records[0];
        assert_eq!(record.disposition, Some(ToolCallDisposition::Rejected));
        assert_eq!(
            record.error_kind,
            Some(astra_core::ErrorKind::ToolInvalidArgs)
        );
        assert!(!record.was_executed());
        state.provider_adaptation.action_convergence_attempted = true;
        assert!(!advance_provider_recovery_epoch_from_new_records(
            &mut state, 0
        ));
        assert!(state.provider_adaptation.action_convergence_attempted);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "before\n");
    }
}
