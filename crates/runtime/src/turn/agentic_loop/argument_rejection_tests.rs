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

#[tokio::test]
async fn server_edit_normalization_rejection_survives_durable_replay_and_callback() {
    use crate::server::runtime_tool_executor::RuntimeToolExecutor;
    use crate::server::tool_execution_binding::{
        ExecutorBinding, ToolPermissionGrantSnapshot, ToolPermissionGrantSource, WorkspaceBinding,
    };
    use astra_turn_types::ToolInvocationTerminalOutcome;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("f.txt");
    std::fs::write(&path, "before\n").unwrap();
    let mut executor = RuntimeToolExecutor::new(
        directory.path().to_path_buf(),
        "edit-user".into(),
        "edit-session".into(),
        None,
        None,
    );
    executor.set_execution_bindings(
        WorkspaceBinding::server_sandbox(directory.path()),
        ExecutorBinding::server_local(),
    );
    executor.enable_durable_invocations();
    let grant = ToolPermissionGrantSnapshot {
        source: ToolPermissionGrantSource::ImplicitPolicy,
        reason: None,
        updates_hash: None,
    };
    // This mixed shape passes schema admission but is rejected by the pure
    // normalizer in the server handler before target preparation.
    for (call_id, args, rejected, succeeds) in [
        (
            "mixed",
            json!({"path":"f.txt", "old_str":"before", "new_str":"after", "edits":[]}),
            true,
            false,
        ),
        (
            "post-read",
            json!({"path":"f.txt", "old_str":"absent", "new_str":"after"}),
            false,
            false,
        ),
        (
            "valid",
            json!({"path":"f.txt", "old_str":"before", "new_str":"after"}),
            false,
            true,
        ),
    ] {
        astra_tools::schemas::validate_tool_arguments_against_schema(
            "str_replace",
            &args,
            astra_tools::schemas::builtin_tool_schema("str_replace").unwrap(),
        )
        .unwrap();
        let first = executor
            .execute_invocation_with_metadata(
                "edit-run",
                "edit-turn",
                call_id,
                "str_replace",
                &args,
                None,
                Some(&grant),
            )
            .await;
        let replay = executor
            .execute_invocation_with_metadata(
                "edit-run",
                "edit-turn",
                call_id,
                "str_replace",
                &args,
                None,
                Some(&grant),
            )
            .await;
        assert_eq!(first.is_error, !succeeds, "{first:?}");
        assert_eq!(replay.output, first.output);
        assert_eq!(replay.metadata.as_ref().unwrap()["invocation_replay"], true);
        for produced in [first, replay] {
            let fields = produced.metadata.as_ref().unwrap();
            assert_eq!(fields["workspace"]["kind"], "server_sandbox");
            assert_eq!(fields["executor"]["kind"], "server_local");
            let terminal =
                crate::server::tool_invocation_runtime::terminal_outcome_from_result(&produced);
            if rejected {
                assert_eq!(fields["durable_invocation_state"], "rejected");
                assert_eq!(
                    produced.output,
                    astra_tools::fs_ops::normalize_str_replace_args(&args).unwrap_err()
                );
                assert_eq!(fields["execution_started"], false);
                assert_eq!(fields["execution_fact"], "not_executed");
                assert_eq!(fields["disposition"], "rejected");
                assert_eq!(
                    fields["error_kind"],
                    astra_core::ErrorKind::ToolInvalidArgs.as_str()
                );
                assert!(fields.contains_key("recovery_evidence"));
                assert!(matches!(
                    terminal,
                    ToolInvocationTerminalOutcome::Rejected { .. }
                ));
            } else {
                assert_ne!(fields.get("execution_started"), Some(&json!(false)));
                assert_ne!(fields.get("execution_fact"), Some(&json!("not_executed")));
                if succeeds {
                    assert!(matches!(
                        terminal,
                        ToolInvocationTerminalOutcome::Succeeded { .. }
                    ));
                } else {
                    assert!(matches!(
                        terminal,
                        ToolInvocationTerminalOutcome::Failed { .. }
                    ));
                }
            }
            let mut callback =
                make_edge_tool_with_args("str_replace", args.clone(), &produced.output);
            callback.status = if produced.is_error {
                "failed"
            } else {
                "completed"
            }
            .into();
            // Supply only the callback transport advertisement; execution and
            // failure facts still come entirely from the real server producer.
            callback.tool_result_fields = Some(edge_runtime_environment_fields());
            callback
                .tool_result_fields
                .as_mut()
                .unwrap()
                .extend(produced.metadata.unwrap());
            let mut turns = vec![edge_tool_result(vec![callback], 10, 2, None)];
            if succeeds {
                // The real loop requires a post-mutation observation before
                // finalizing. Obtain it from the server rather than minting a
                // test-only verification or workspace receipt.
                let verify_args = json!({"path":"f.txt"});
                let verification = executor
                    .execute_with_metadata("read_file", &verify_args)
                    .await;
                assert!(!verification.is_error, "{verification:?}");
                assert!(verification.output.contains("after"));
                let mut callback =
                    make_edge_tool_with_args("read_file", verify_args, &verification.output);
                callback.tool_result_fields = Some(edge_runtime_environment_fields());
                callback
                    .tool_result_fields
                    .as_mut()
                    .unwrap()
                    .extend(verification.metadata.unwrap());
                turns.push(edge_tool_result(vec![callback], 10, 2, None));
            }
            turns.push(text_result(
                "The edit result has been reported.",
                10,
                2,
                None,
            ));
            let mut host = MockHost::new(turns).with_valid_tools(&["str_replace", "read_file"]);
            let mut state = make_state();
            let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
            assert!(
                outcome.is_ok(),
                "{call_id}: {outcome:?}; {:?}",
                state.stall.tool_call_records
            );
            assert_eq!(
                state.stall.tool_call_records.len(),
                if succeeds { 2 } else { 1 }
            );
            if succeeds {
                let verification = &state.stall.tool_call_records[1];
                assert_eq!(verification.name, "read_file");
                assert!(verification.ok && verification.was_executed());
            }
            let record = &state.stall.tool_call_records[0];
            assert_eq!(
                record.disposition,
                Some(if rejected {
                    ToolCallDisposition::Rejected
                } else {
                    ToolCallDisposition::Executed
                }),
                "{call_id}: {record:?}"
            );
            if rejected {
                assert_eq!(
                    record.error_kind,
                    Some(astra_core::ErrorKind::ToolInvalidArgs)
                );
            }
            assert_eq!(record.was_executed(), !rejected);
            state.provider_adaptation.action_convergence_attempted = true;
            assert_eq!(
                advance_provider_recovery_epoch_from_new_records(&mut state, 0),
                !rejected
            );
            assert_eq!(
                state.provider_adaptation.action_convergence_attempted,
                rejected
            );
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            if succeeds { "after\n" } else { "before\n" }
        );
    }
}
