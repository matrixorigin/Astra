use super::*;
use crate::turn::agentic_loop::host::run_agentic_loop_with_host;
use crate::turn::agentic_loop::host::tests::{
    MockHost, edge_tool_result, make_edge_tool_with_args, make_state, text_result,
};
use astra_config::user_profile::{MutationCompletionScope, TurnIntent};
use astra_services::session_journal::{ToolCallDisposition, ToolCallRecord};
use serde_json::json;

async fn run_written_verification(
    command: &str,
    result_class: &str,
    exit_semantics: &str,
    declared: bool,
) -> (AgenticLoopState, MockHost) {
    let write = make_edge_tool_with_args(
        "write_file",
        json!({"path": "/workspace/main.go", "content": "package main\n"}),
        "written",
    );
    let args = json!({"command": command});
    let mut checked = make_edge_tool_with_args("bash", args, "observed result");
    checked.tool_result_fields.as_mut().unwrap().extend([
        (
            "exit_code".into(),
            json!(if exit_semantics == "success" { 0 } else { 1 }),
        ),
        ("result_class".into(), json!(result_class)),
        ("exit_semantics".into(), json!(exit_semantics)),
    ]);
    let mut host = MockHost::new(vec![
        edge_tool_result(vec![write], 10, 2, None),
        edge_tool_result(vec![checked], 10, 2, None),
        text_result("Implemented and checked the change.", 10, 2, None),
        text_result("Implemented and checked the change.", 10, 2, None),
        text_result("Implemented and checked the change.", 10, 2, None),
    ])
    .with_valid_tools(&["write_file", "bash", "read_file"]);
    let mut state = make_state();
    state.hooks.workspace_root_hint = Some("/workspace".into());
    if declared {
        state.hooks.stop_hooks.push(astra_turn_types::StopHook {
            label: "requested check".into(),
            command: command.into(),
            working_dir: None,
            depends_on: Vec::new(),
            timeout_secs: None,
            authoritative: true,
        });
    }
    state.turn_intent =
        Some(TurnIntent::default().with_workspace_mutation(WorkspaceMutationIntent::MustMutate));
    run_agentic_loop_with_host(&mut host, &mut state)
        .await
        .unwrap();
    (state, host)
}

#[tokio::test]
async fn ordinary_commands_do_not_create_implicit_validation_obligations() {
    for command in [
        "GOWORK=off go test ./...",
        "./custom-check",
        "echo '<<' \"$PWD\"",
    ] {
        let (state, host) = run_written_verification(command, "success", "success", false).await;
        assert!(
            state.interruption.is_none(),
            "{command}: {:?}",
            state.interruption
        );
        assert_eq!(
            host.turn_count(),
            3,
            "ordinary results need no forced follow-up check"
        );
        assert_eq!(state.stall.tool_call_records.len(), 2);
    }
}

#[tokio::test]
async fn explicitly_declared_custom_check_uses_its_typed_command_result() {
    for command in ["./custom-check", "python3 - <<'PY'\nassert 1 == 1\nPY"] {
        let (state, host) = run_written_verification(command, "success", "success", true).await;
        assert!(
            state.interruption.is_none(),
            "{command}: {:?}",
            state.interruption
        );
        assert_eq!(host.turn_count(), 3);
        assert_eq!(
            missing_explicit_verification_hooks(&state).unwrap(),
            Some(Vec::new())
        );
    }
}

fn external_request() -> AgenticLoopState {
    let mut state = make_state();
    state.task_profile =
        astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
            true,
            false,
            astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
        );
    state.turn_intent = Some(
        TurnIntent::default()
            .with_workspace_mutation(WorkspaceMutationIntent::MustMutate)
            .with_mutation_completion_scope(MutationCompletionScope::External),
    );
    state
}

#[tokio::test]
async fn external_result_and_already_satisfied_state_finish_without_another_mutation() {
    for command in [
        "git worktree remove --force /tmp/review",
        "test ! -e /tmp/review",
    ] {
        let mut result = make_edge_tool_with_args("bash", json!({"command": command}), "absent");
        astra_tools::execution_outcome::insert_started_execution_fact(
            result.tool_result_fields.as_mut().unwrap(),
        );
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![result], 10, 2, None),
            text_result("The tool reports that the checkout is absent.", 10, 2, None),
        ])
        .with_valid_tools(&["bash"]);
        let mut state = external_request();
        let budget = state.max_turns;
        let outcome = run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .unwrap();
        assert!(matches!(outcome, AgenticLoopOutcome::Completed));
        assert_eq!(
            host.turn_count(),
            2,
            "no extra mutation/recovery round: {command}"
        );
        assert_eq!(state.max_turns, budget);
        assert!(state.interruption.is_none(), "{:?}", state.interruption);
        assert_eq!(state.stall.tool_call_records.len(), 1);
        assert_eq!(
            state.stall.tool_call_records[0].disposition,
            Some(ToolCallDisposition::Executed)
        );
        assert!(state.final_text.contains("checkout is absent"));
    }
}

#[test]
fn mixed_request_retains_only_its_workspace_completion_owner() {
    let mut state = external_request();
    assert_eq!(pending_completion_action(&state).unwrap(), None);
    state
        .turn_intent
        .as_mut()
        .unwrap()
        .mutation_completion_scope = MutationCompletionScope::Mixed;
}

#[tokio::test]
async fn callback_outcome_facts_reach_the_model_before_recovery() {
    // Use the shared producer, not hand-written rejection metadata. This is
    // the same callback body/metadata the CLI sends after cancellation.
    for (started, unknown) in [(false, false), (true, false), (true, true)] {
        let mut result =
            make_edge_tool_with_args("bash", json!({"command": "perform-external-operation"}), "");
        let cancelled = astra_tools::cancelled_tool_result("bash", started);
        let cancelled = if unknown {
            cancelled.with_execution_outcome_unknown()
        } else {
            cancelled
        };
        result.output = cancelled.output;
        result
            .tool_result_fields
            .as_mut()
            .unwrap()
            .extend(cancelled.metadata.unwrap());
        result.status = "failed".into();
        let mut observed =
            make_edge_tool_with_args("bash", json!({"command": "pwd"}), "/workspace");
        observed.request_id = "req-bash-observation".into();
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![result], 10, 2, None),
            edge_tool_result(vec![observed], 10, 2, None),
            text_result("The operation was cancelled.", 10, 2, None),
        ])
        .with_valid_tools(&["bash"]);
        if started || unknown {
            // A successful-tool terminal cannot skip the model's
            // interpretation of this later observation.
            host = host.with_stop_after_success_completion("bash", Some("premature success"));
        }
        let mut state = external_request();
        let _ = run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .unwrap();
        let record = &state.stall.tool_call_records[0];
        assert_eq!(record.error_kind, Some(astra_core::ErrorKind::Cancelled));
        assert_eq!(record.was_executed(), started || unknown);
        assert!(
            !record.ok,
            "the unrelated observation must not settle the original call"
        );
        assert!(state.stall.tool_call_records[1].ok);
        assert_eq!(
            state.stall.tool_call_records.len(),
            2,
            "no automatic recovery write"
        );
        assert_eq!(host.turn_count(), 3);
        assert!(state.interruption.is_none(), "{:?}", state.interruption);
        let model_context = serde_json::to_string(&host.executed_messages[1]).unwrap();
        assert!(
            model_context.contains(if unknown {
                "Inspect the outcome before repeating it"
            } else if started {
                "failed"
            } else {
                "not_executed"
            }),
            "{model_context}"
        );
        assert_eq!(
            model_context.contains("Inspect the outcome before repeating it"),
            unknown
        );
        assert_eq!(state.final_text, "The operation was cancelled.");
        assert_eq!(host.final_output_ready, vec![state.final_text.clone()]);

        if started || unknown {
            // Resume the canonical messages and journal facts produced by the
            // real first round; a checkpoint must not authorize a replay or
            // lose the evidence needed by the next provider request.
            let record = record.clone();
            state.messages = host.executed_messages[1].clone();
            let checkpoint = super::super::finalization::build_current_heavy_checkpoint(&mut state)
                .expect("the active turn can be checkpointed");
            let checkpoint: astra_pipeline::step_protocol::HeavyCheckpoint =
                serde_json::from_str(&serde_json::to_string(&checkpoint).unwrap()).unwrap();
            let mut restored = external_request();
            restored.messages = checkpoint.messages;
            restored.stall.tool_call_records =
                serde_json::from_str(&serde_json::to_string(&[record]).unwrap()).unwrap();
            let mut resumed = MockHost::new(vec![text_result(
                "The operation's outcome is unknown.",
                10,
                2,
                None,
            )])
            .with_valid_tools(&["bash"]);
            run_agentic_loop_with_host(&mut resumed, &mut restored)
                .await
                .unwrap();
            assert_eq!(resumed.turn_count(), 1);
            assert!(resumed.admitted_tool_call_batches.is_empty());
            assert!(!restored.stall.tool_call_records[0].ok);
            assert_eq!(
                serde_json::to_string(&resumed.executed_messages[0])
                    .unwrap()
                    .contains("Inspect the outcome before repeating it"),
                unknown
            );
        }
    }
}

#[tokio::test]
async fn timeout_callback_remains_unconfirmed_after_an_unrelated_observation() {
    // The callback and journal must preserve the executor's timeout and startup
    // facts. A later successful observation cannot settle the earlier operation.
    for (command, started) in [
        ("perform-external-operation", true),
        ("perform-external-operation", false),
        ("pwd", true),
    ] {
        let mut result = make_edge_tool_with_args("bash", json!({"command": command}), "");
        let fields = result.tool_result_fields.as_mut().unwrap();
        fields.insert("error_kind".into(), json!("tool_timeout"));
        if started {
            astra_tools::execution_outcome::insert_started_execution_fact(fields);
        } else {
            astra_tools::execution_outcome::insert_not_executed_fact(fields);
        }
        result.status = "failed".into();
        result.output = "Error: command timed out".into();
        let mut observed =
            make_edge_tool_with_args("bash", json!({"command": "pwd"}), "/workspace");
        observed.request_id = "req-bash-probe".into();
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![result], 10, 2, None),
            edge_tool_result(vec![observed], 10, 2, None),
            text_result("Preserved partial report.", 10, 2, None),
        ])
        .with_valid_tools(&["bash"]);
        let mut state = external_request();
        let budget = state.max_turns;
        let _ = run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .unwrap();
        let record = &state.stall.tool_call_records[0];
        assert_eq!(record.error_kind, Some(astra_core::ErrorKind::ToolTimeout));
        assert_eq!(record.was_executed(), started);
        assert!(!record.ok);
        assert!(
            state.stall.tool_call_records[1].ok,
            "the unrelated probe succeeded"
        );
        assert!(state.interruption.is_none(), "{record:?}");
        assert_eq!(host.turn_count(), 3, "the model can observe and respond");
        assert_eq!(state.max_turns, budget);
        assert_eq!(state.stall.tool_call_records.len(), 2);
        assert_eq!(state.final_text, "Preserved partial report.");
        assert_eq!(host.final_output_ready, vec![state.final_text.clone()]);
    }
}
