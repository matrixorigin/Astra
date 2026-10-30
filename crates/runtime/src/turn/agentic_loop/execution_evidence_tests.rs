use super::*;
use crate::turn::agentic_loop::host::run_agentic_loop_with_host;
use crate::turn::agentic_loop::host::tests::{
    MockHost, edge_tool_result, make_edge_tool_with_args, make_state, text_result,
};
use astra_config::user_profile::{MutationCompletionScope, TurnIntent};
use astra_services::session_journal::{ToolCallDisposition, ToolCallRecord};
use serde_json::json;

#[derive(Clone, Copy, Debug)]
enum ExplicitReceipt {
    Valid,
    Missing,
    Forged,
    Failed,
    Partial,
    Changed,
}

async fn run_written_verification(
    command: &str,
    result_class: &str,
    exit_semantics: &str,
    receipt: Option<ExplicitReceipt>,
) -> (AgenticLoopState, MockHost) {
    let write = make_edge_tool_with_args(
        "write_file",
        json!({"path": "/workspace/main.go", "content": "package main\n"}),
        "written",
    );
    let args = if receipt.is_some() {
        json!({"command": command, "mode": "verify"})
    } else {
        json!({"command": command})
    };
    let mut checked = make_edge_tool_with_args("bash", args, "observed result");
    checked.tool_result_fields.as_mut().unwrap().extend([
        (
            "exit_code".into(),
            json!(if exit_semantics == "success" { 0 } else { 1 }),
        ),
        ("result_class".into(), json!(result_class)),
        ("exit_semantics".into(), json!(exit_semantics)),
    ]);
    if let Some(receipt) = receipt {
        let fields = checked.tool_result_fields.as_mut().unwrap();
        fields.insert(
            astra_tools::workspace_observation::SCOPE_FIELD.into(),
            json!(astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE),
        );
        fields.insert(
            astra_tools::workspace_observation::OBSERVED_FIELD.into(),
            json!(matches!(receipt, ExplicitReceipt::Changed)),
        );
        if !matches!(receipt, ExplicitReceipt::Missing) {
            fields.extend(
                astra_tools::workspace_observation::explicit_workspace_verification_receipt(),
            );
        }
        if matches!(receipt, ExplicitReceipt::Forged) {
            fields
                .get_mut(astra_tools::workspace_observation::OBSERVATION_RECEIPT_FIELD)
                .unwrap()["source"] = json!("model_claim");
        }
        if matches!(receipt, ExplicitReceipt::Partial) {
            fields.insert("workspace_mutation_partial".into(), json!(true));
        }
        if matches!(receipt, ExplicitReceipt::Failed) {
            checked.status = "failed".into();
            checked.output = "Error: verification failed".into();
        }
    }
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
    state.turn_intent =
        Some(TurnIntent::default().with_workspace_mutation(WorkspaceMutationIntent::MustMutate));
    run_agentic_loop_with_host(&mut host, &mut state)
        .await
        .unwrap();
    (state, host)
}

#[tokio::test]
async fn go_verification_after_write_finishes_without_redundant_observation() {
    for command in [
        "go test ./...",
        "printf '%s\\n' checking && GOWORK=off go test ./...",
        "mkdir -p /tmp/astra-cache && printf '%s\\n' checking && GOWORK=off go test ./...",
        "mkdir -p /tmp/astra-cache && go test ./...",
        "mkdir -p /tmp/astra-cache && GOWORK=off go test ./... 2>&1 | tail -10",
        "GOWORK=off go test ./...",
        "GOWORK=off go test -skip TestIntegration -covermode=atomic -coverpkg=./... ./...",
        "GOWORK=off go build ./... && GOWORK=off go vet ./... && echo BUILD_VET_OK",
        "cd /workspace && GOWORK=off go build -mod=readonly ./pkg/one/ ./pkg/two/ && GOWORK=off go vet -mod=readonly ./pkg/one/ ./pkg/two/ && echo BUILD_VET_OK",
    ] {
        let (state, host) = run_written_verification(command, "success", "success", None).await;
        assert!(
            state.interruption.is_none(),
            "{command}: {:?}",
            state.interruption
        );
        assert_eq!(
            host.turn_count(),
            3,
            "{command}: no redundant recovery rounds"
        );
        assert_eq!(state.stall.tool_call_records.len(), 2);
        assert_eq!(
            host.final_output_ready,
            vec!["Implemented and checked the change."]
        );
    }
}

#[tokio::test]
async fn go_verification_does_not_complete_when_a_predecessor_failed_or_changed_configuration() {
    for (command, class, semantics) in [
        (
            "sh -c 'exit 1' && GOWORK=off go test ./...",
            "inconclusive",
            "domain_negative",
        ),
        (
            "GOWORK=off go test ./... | head -1",
            "success",
            "pipeline_truncated",
        ),
        (
            "export GOFLAGS=-n && GOWORK=off go test ./...",
            "success",
            "success",
        ),
        (
            "export GOFLAGS=-n && GOWORK=off go test \"${PKGS:-./...}\"",
            "success",
            "success",
        ),
        ("GOFLAGS=-n go test -run '<<' ./...", "success", "success"),
        (
            "GOWORK=off go test -n ./... && cargo test",
            "success",
            "success",
        ),
        (
            "printf -v GOFLAGS %s -n && GOWORK=off go test ./...",
            "success",
            "success",
        ),
        (
            "GOFLAGS=$(cat /tmp/flags) && GOWORK=off go test ./...",
            "success",
            "success",
        ),
        (
            "printf '%5n' GOFLAGS && GOWORK=off go test ./...",
            "success",
            "success",
        ),
        (
            "mkdir -p /tmp/astra-cache && printf -v GOFLAGS %s -n && GOWORK=off go test ./...",
            "success",
            "success",
        ),
        ("true || GOWORK=off go test ./...", "success", "success"),
        (
            "mkdir -p /tmp/astra-cache && go test ./...",
            "inconclusive",
            "domain_negative",
        ),
        (
            "mkdir -p /tmp/astra-cache && go test ./...",
            "test_failure",
            "domain_negative",
        ),
        (
            "mkdir -p /tmp/astra-cache | cat && go test ./...",
            "success",
            "success",
        ),
        (
            "GOFLAGS=$(cat /tmp/flags) && mkdir -p /tmp/astra-cache && go test ./...",
            "success",
            "success",
        ),
        (
            "go env -w GOFLAGS=-n && mkdir -p /tmp/astra-cache && go test ./...",
            "success",
            "success",
        ),
        (
            "go generate ./... && mkdir -p /tmp/astra-cache && go test ./...",
            "success",
            "success",
        ),
        (
            "printf -v PATH /tmp/fakebin && mkdir -p /tmp/astra-cache && go test ./...",
            "success",
            "success",
        ),
        (
            "printf '%n' PATH && mkdir -p /tmp/astra-cache && go test ./...",
            "success",
            "success",
        ),
        (
            "mkdir -p /tmp/astra-cache && export PATH=/tmp/fakebin && go test ./... && cargo test",
            "success",
            "success",
        ),
    ] {
        let (state, host) = run_written_verification(command, class, semantics, None).await;
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete),
            "{command}"
        );
        assert_eq!(host.turn_count(), 5);
        assert_eq!(
            state.stall.tool_call_records.len(),
            2,
            "no invented recovery tool"
        );
    }
}

#[tokio::test]
async fn explicit_verification_accepts_equivalent_python_forms_after_write() {
    for (command, expected_presence) in [
        ("python3 -c 'assert 1 == 1'", Some(false)),
        ("python3 - <<'PY'\nassert 1 == 1\nPY", None),
    ] {
        assert_eq!(
            astra_turn_core::evaluation::bash_command_go_validation_presence(command),
            expected_presence
        );
        let (state, host) =
            run_written_verification(command, "success", "success", Some(ExplicitReceipt::Valid))
                .await;
        assert!(
            state.interruption.is_none(),
            "{command}: {:?}",
            state.interruption
        );
        assert_eq!(
            host.turn_count(),
            3,
            "{command}: no redundant recovery rounds"
        );
        assert_eq!(state.stall.tool_call_records.len(), 2);
        assert!(successful_post_mutation_observation(&state).unwrap());
        assert_eq!(
            WorkValidationEvidence::collect(&state, true).state,
            WorkValidationState::None
        );

        let failed_args = json!({"command": "GOWORK=off go test ./..."}).to_string();
        let mut with_go_debt = state;
        with_go_debt.stall.tool_call_records.insert(
            1,
            ToolCallRecord {
                name: "bash".into(),
                ok: false,
                disposition: Some(ToolCallDisposition::Executed),
                runtime_args_full: Some(failed_args),
                result_class: Some("test_failure".into()),
                exit_semantics: Some("domain_negative".into()),
                ..Default::default()
            },
        );
        let evidence = WorkValidationEvidence::collect(&with_go_debt, true);
        assert_eq!(evidence.state, WorkValidationState::Failed, "{command}");
        assert_eq!(
            evidence.failed_operation(),
            Some("GOWORK=off go test ./...")
        );
    }
}

fn failed_explicit_verification(success: &ToolCallRecord) -> ToolCallRecord {
    let mut failure = success.clone();
    failure.ok = false;
    failure.workspace_mutation_receipt = None;
    failure.result_class = Some("test_failure".into());
    failure.exit_semantics = Some("domain_negative".into());
    failure
}

fn go_validation_record(ok: bool) -> ToolCallRecord {
    ToolCallRecord {
        name: "bash".into(),
        ok,
        disposition: Some(ToolCallDisposition::Executed),
        runtime_args_full: Some(json!({"command": "GOWORK=off go test ./..."}).to_string()),
        result_class: Some(if ok { "success" } else { "test_failure" }.into()),
        exit_semantics: Some(if ok { "success" } else { "domain_negative" }.into()),
        ..Default::default()
    }
}

fn rejected_work_settlement() -> ToolCallRecord {
    ToolCallRecord {
        name: "settle_work_item".into(),
        disposition: Some(ToolCallDisposition::Rejected),
        result_full: Some(
            json!({
                "status":"rejected", "error_kind":"unresolved_work_validation",
                "validation_state":"failed"
            })
            .to_string(),
        ),
        ..Default::default()
    }
}

fn admit_completion_call(
    state: &mut AgenticLoopState,
    call: &serde_json::Value,
) -> ToolCallAdmission {
    apply_completion_action_admission(
        state,
        ToolCallAdmission {
            admitted: vec![
                astra_turn_core::tool::deferred_activation::CanonicalToolInvocation::ordinary(
                    call.clone(),
                ),
            ],
            rejected: Vec::new(),
            completion_action_applied: false,
        },
        std::slice::from_ref(call),
    )
}

#[tokio::test]
async fn declared_verifier_retry_settles_after_exact_success_for_both_python_forms() {
    for command in [
        "python3 -c 'assert 1 == 1'",
        "python3 - <<'PY'\nassert 1 == 1\nPY",
    ] {
        let (state, _) =
            run_written_verification(command, "success", "success", Some(ExplicitReceipt::Valid))
                .await;
        let retry = state.stall.tool_call_records.last().unwrap().clone();
        let failure = failed_explicit_verification(&retry);
        let args = failure.authoritative_args_full().unwrap().to_owned();
        let operation =
            astra_turn_core::evaluation::normalize_validation_attempt_prefix("bash", &args)
                .unwrap();
        let call = json!({"id":"retry", "type":"function", "function": {
            "name":"bash", "arguments":args.clone()
        }});

        let mut work = make_state();
        work.hooks.workspace_root_hint = Some("/workspace".into());
        work.stall.tool_call_records.push(failure);
        let failed = WorkValidationEvidence::collect(&work, true);
        assert_eq!(failed.state, WorkValidationState::Failed, "{command}");
        assert_eq!(failed.failed_operation(), Some(operation.as_str()));
        assert_eq!(
            pending_terminal_completion_action_for_work_state(&work, true).unwrap(),
            Some(CompletionAction::CanonicalWorkValidation)
        );
        work.stall
            .tool_call_records
            .push(rejected_work_settlement());
        advance_rejected_work_settlement_recovery_for_work_state(&mut work, true, 1);
        let window = work
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .unwrap();
        assert_eq!(window.action, CompletionAction::CanonicalWorkValidation);
        assert_eq!(
            work.hooks
                .completion_settlement
                .canonical_validation_recovery_operation
                .as_deref(),
            Some(operation.as_str())
        );
        let hint = completion_action_hint_for_state(&work, &window.action);
        assert!(hint.to_string().contains("same arguments"));
        assert!(!hint.to_string().contains(command));

        let mut different = call.clone();
        let mut different_args: serde_json::Value = serde_json::from_str(&args).unwrap();
        different_args["workdir"] = json!("nested");
        different["function"]["arguments"] = json!(different_args.to_string());
        let rejected = admit_completion_call(&mut work, &different);
        assert!(rejected.admitted.is_empty());
        assert_eq!(rejected.rejected.len(), 1);
        assert!(rejected.rejected[0].result.contains("same arguments"));
        let accepted = admit_completion_call(&mut work, &call);
        assert_eq!(accepted.admitted.len(), 1);
        assert!(accepted.rejected.is_empty());
        let retry_start = work.stall.tool_call_records.len();
        work.stall.tool_call_records.push(retry.clone());
        assert_eq!(
            WorkValidationEvidence::collect(&work, true).state,
            WorkValidationState::None
        );
        assert_eq!(
            pending_completion_action_for_work_state(&work, true).unwrap(),
            None
        );
        advance_completion_action_window_after_tool_round_for_work_state_from_record_index(
            &mut work,
            true,
            retry_start,
        );
        assert!(work.hooks.completion_settlement.work_settlement_only);
        assert_eq!(
            pending_terminal_completion_action_for_work_state(&work, true).unwrap(),
            None
        );
        work.stall.tool_call_records.push(retry);
        assert_eq!(
            WorkValidationEvidence::collect(&work, true).state,
            WorkValidationState::None
        );
    }
}

#[tokio::test]
async fn failed_declared_observation_has_one_bounded_retry_without_a_repair() {
    for command in [
        "python3 -c 'assert 1 == 1'",
        "python3 - <<'PY'\nassert 1 == 1\nPY",
        "grep -q sentinel /workspace/file",
    ] {
        if command.starts_with("grep ") {
            assert_eq!(
                astra_tools::exit_semantics::classify_exit(command, 1),
                astra_tools::exit_semantics::ExitSemantics::EmptyResult
            );
            assert_eq!(
                astra_tools::exit_semantics::classify_command_result(command, "", "", Some(1)),
                astra_tools::exit_semantics::CommandResultClass::EmptyResult
            );
        }
        let (state, _) =
            run_written_verification(command, "success", "success", Some(ExplicitReceipt::Valid))
                .await;
        let writer = state.stall.tool_call_records[0].clone();
        let mut retry = state.stall.tool_call_records.last().unwrap().clone();
        retry.batch_id = None;
        retry.round = Some(2);
        let call = json!({"id":"retry", "type":"function", "function": {
            "name":"bash", "arguments":retry.authoritative_args_full().unwrap()
        }});
        for (class, semantics, error_kind, ok) in [
            (Some("test_failure"), "domain_negative", None, true),
            (
                Some("execution_error"),
                "execution_error",
                Some(astra_core::ErrorKind::Unknown),
                false,
            ),
            (
                Some("execution_error"),
                "execution_error",
                Some(astra_core::ErrorKind::Unknown),
                true,
            ),
            (Some("empty_result"), "empty_result", None, false),
            (None, "empty_result", None, false),
        ] {
            if (semantics == "empty_result") != command.starts_with("grep ") {
                continue;
            }
            for active_work in [false, true] {
                let mut failure = failed_explicit_verification(&retry);
                // Transport status does not replace the producer's
                // command-result semantics.
                failure.ok = ok;
                failure.result_class = class.map(str::to_owned);
                failure.exit_semantics = Some(semantics.into());
                failure.error_kind = error_kind;
                let mut work = make_state();
                work.hooks.workspace_root_hint = Some("/workspace".into());
                work.stall.tool_call_records = vec![writer.clone(), failure];
                work.hooks
                    .completion_settlement
                    .post_mutation_observation_retries = 1;
                work.hooks.completion_settlement.completion_action_window =
                    Some(super::super::host::CompletionActionWindow {
                        action: CompletionAction::PostMutationObservation,
                        attempts_remaining: 0,
                        mismatch_corrections_remaining: 1,
                        consumed: true,
                        matched: true,
                    });
                advance_completion_action_window_after_tool_round_for_work_state_from_record_index(
                    &mut work,
                    active_work,
                    1,
                );
                let window = work
                    .hooks
                    .completion_settlement
                    .completion_action_window
                    .as_ref()
                    .unwrap();
                assert_eq!(
                    window.action,
                    if active_work {
                        CompletionAction::CanonicalWorkValidation
                    } else {
                        CompletionAction::PostMutationObservation
                    },
                    "{command}: {class:?}"
                );
                assert!(!window.consumed);
                assert_eq!(
                    work.hooks
                        .completion_settlement
                        .post_mutation_repair_retries,
                    0
                );
                let admitted = admit_completion_call(&mut work, &call);
                assert_eq!(admitted.admitted.len(), 1, "{command}: {class:?}");
                assert!(admitted.rejected.is_empty());
                work.stall.tool_call_records.push(retry.clone());
                assert_eq!(
                    pending_completion_action_for_work_state(&work, active_work).unwrap(),
                    None
                );
                advance_completion_action_window_after_tool_round_for_work_state_from_record_index(
                    &mut work,
                    active_work,
                    2,
                );
                if active_work {
                    assert!(work.hooks.completion_settlement.work_settlement_only);
                } else {
                    assert!(work.hooks.completion_settlement.text_only);
                }
            }
            let mut failure = failed_explicit_verification(&retry);
            failure.ok = ok;
            failure.result_class = class.map(str::to_owned);
            failure.exit_semantics = Some(semantics.into());
            failure.error_kind = error_kind;
            let mut rejected = make_state();
            rejected.hooks.workspace_root_hint = Some("/workspace".into());
            rejected.stall.tool_call_records = vec![failure, rejected_work_settlement()];
            advance_rejected_work_settlement_recovery_for_work_state(&mut rejected, true, 1);
            assert_eq!(
                admit_completion_call(&mut rejected, &call).admitted.len(),
                1,
                "rejected settlement: {command}: {class:?}: transport ok={ok}"
            );
        }
        for (kind, partial, changed, class, semantics) in [
            (
                Some(astra_core::ErrorKind::ToolTimeout),
                false,
                false,
                "execution_error",
                "execution_error",
            ),
            (
                Some(astra_core::ErrorKind::Cancelled),
                false,
                false,
                "execution_error",
                "execution_error",
            ),
            (
                Some(astra_core::ErrorKind::ToolOutcomeUnknown),
                false,
                false,
                "execution_error",
                "execution_error",
            ),
            (
                Some(astra_core::ErrorKind::Unknown),
                false,
                false,
                "tool_outcome_unknown",
                "execution_error",
            ),
            (None, false, false, "execution_error", "timed_out"),
            (None, false, false, "success", "execution_error"),
            (None, false, false, "execution_error", "empty_result"),
            (None, false, false, "empty_result", "execution_error"),
            (None, true, false, "test_failure", "domain_negative"),
            (None, false, true, "test_failure", "domain_negative"),
        ] {
            let mut failure = failed_explicit_verification(&retry);
            failure.error_kind = kind;
            failure.workspace_mutation_partial = Some(partial);
            failure.workspace_mutation_observed = Some(changed);
            failure.result_class = Some(class.into());
            failure.exit_semantics = Some(semantics.into());
            let mut work = make_state();
            work.hooks.workspace_root_hint = Some("/workspace".into());
            work.stall.tool_call_records = vec![writer.clone(), failure.clone()];
            work.hooks.completion_settlement.completion_action_window =
                Some(super::super::host::CompletionActionWindow {
                    action: CompletionAction::PostMutationObservation,
                    attempts_remaining: 0,
                    mismatch_corrections_remaining: 1,
                    consumed: true,
                    matched: true,
                });
            advance_completion_action_window_after_tool_round_for_work_state_from_record_index(
                &mut work, true, 1,
            );
            assert_eq!(
                work.hooks
                    .completion_settlement
                    .post_mutation_observation_failed_action_retries,
                0
            );
            assert!(
                work.hooks
                    .completion_settlement
                    .completion_action_window
                    .as_ref()
                    .unwrap()
                    .consumed
            );
            assert!(work.hooks.completion_settlement.text_only);
            assert_eq!(
                pending_terminal_completion_action_for_work_state(&work, true).unwrap(),
                Some(CompletionAction::CanonicalWorkValidation)
            );
            let admission =
                crate::turn::agentic::tool_interception::reject_tool_calls_at_text_only_boundary(
                    std::slice::from_ref(&call),
                    Some("tool_calls"),
                );
            assert!(admission.admitted.is_empty());
            assert_eq!(admission.rejected.len(), 1);

            let mut rejected = make_state();
            rejected.hooks.workspace_root_hint = Some("/workspace".into());
            rejected.hooks.completion_settlement.work_settlement_only = true;
            rejected.stall.tool_call_records = vec![failure, rejected_work_settlement()];
            advance_rejected_work_settlement_recovery_for_work_state(&mut rejected, true, 1);
            assert!(
                rejected
                    .hooks
                    .completion_settlement
                    .completion_action_window
                    .is_none()
            );
            assert!(rejected.hooks.completion_settlement.work_settlement_only);
            assert_eq!(
                pending_terminal_completion_action_for_work_state(&rejected, true).unwrap(),
                Some(CompletionAction::CanonicalWorkValidation)
            );
        }
    }
}

#[tokio::test]
async fn declared_retry_with_missing_optional_result_fields_still_requires_fresh_named_proof() {
    let (state, _) = run_written_verification(
        "python3 - <<'PY'\nassert 1 == 1\nPY",
        "success",
        "success",
        Some(ExplicitReceipt::Valid),
    )
    .await;
    let mut retry = state.stall.tool_call_records.last().unwrap().clone();
    let failure = failed_explicit_verification(&retry);
    let operation = astra_turn_core::evaluation::normalize_validation_attempt_prefix(
        "bash",
        failure.authoritative_args_full().unwrap(),
    )
    .unwrap();
    retry.result_class = None;
    retry.exit_semantics = None;
    assert!(
        super::super::lifecycle::record_has_full_scope_explicit_workspace_verification_receipt(
            &retry
        )
    );
    let mut work = make_state();
    work.hooks.workspace_root_hint = Some("/workspace".into());
    work.stall.tool_call_records = vec![go_validation_record(true), failure, retry];
    work.hooks
        .completion_settlement
        .canonical_validation_recovery_operation = Some(operation);
    work.hooks
        .completion_settlement
        .canonical_validation_recovery_retries = 1;
    work.hooks.completion_settlement.completion_action_window =
        Some(super::super::host::CompletionActionWindow {
            action: CompletionAction::CanonicalWorkValidation,
            attempts_remaining: 0,
            mismatch_corrections_remaining: 1,
            consumed: true,
            matched: true,
        });
    assert_eq!(
        WorkValidationEvidence::collect(&work, true).state,
        WorkValidationState::Stale
    );
    advance_completion_action_window_after_tool_round_for_work_state_from_record_index(
        &mut work, true, 2,
    );
    let window = work
        .hooks
        .completion_settlement
        .completion_action_window
        .as_ref()
        .unwrap();
    assert_eq!(window.action, CompletionAction::CanonicalWorkValidation);
    assert!(!window.consumed);
    assert!(
        work.hooks
            .completion_settlement
            .canonical_validation_recovery_operation
            .is_none()
    );
    assert!(!work.hooks.completion_settlement.work_settlement_only);
    let call = json!({"id":"fresh", "type":"function", "function": {
        "name":"bash", "arguments":json!({"command":"cargo test"}).to_string()
    }});
    assert_eq!(admit_completion_call(&mut work, &call).admitted.len(), 1);
    let mut fresh = go_validation_record(true);
    fresh.runtime_args_full = Some(json!({"command":"cargo test"}).to_string());
    work.stall.tool_call_records.push(fresh);
    assert_eq!(
        WorkValidationEvidence::collect(&work, true).state,
        WorkValidationState::Passed
    );
    advance_completion_action_window_after_tool_round_for_work_state_from_record_index(
        &mut work, true, 3,
    );
    assert!(work.hooks.completion_settlement.work_settlement_only);
}

#[tokio::test]
async fn declared_verifier_retry_preserves_named_debt_and_freshness() {
    for command in [
        "python3 -c 'assert 1 == 1'",
        "python3 - <<'PY'\nassert 1 == 1\nPY",
    ] {
        let (state, _) =
            run_written_verification(command, "success", "success", Some(ExplicitReceipt::Valid))
                .await;
        let retry = state.stall.tool_call_records.last().unwrap().clone();
        let failure = failed_explicit_verification(&retry);
        let mut unchanged_failure = failure.clone();
        unchanged_failure.workspace_mutation_receipt =
            astra_tools::workspace_observation::unchanged_bash_observation_receipt_with_ownership(
                astra_tools::workspace_observation::INVOCATION_CGROUP_OWNERSHIP,
            )
            .remove(astra_tools::workspace_observation::OBSERVATION_RECEIPT_FIELD);
        assert!(
            super::super::lifecycle::is_authoritative_unchanged_bash_observation_record(
                &unchanged_failure
            )
        );
        let mut work = make_state();
        work.hooks.workspace_root_hint = Some("/workspace".into());
        work.stall.tool_call_records =
            vec![go_validation_record(false), failure.clone(), retry.clone()];
        let evidence = WorkValidationEvidence::collect(&work, true);
        assert!(
            matches!(
                evidence.state,
                WorkValidationState::Failed | WorkValidationState::Stale
            ),
            "{command}: prior named failure remains blocking"
        );
        assert_eq!(
            evidence.failed_operation(),
            Some("GOWORK=off go test ./...")
        );
        assert_eq!(
            pending_terminal_completion_action_for_work_state(&work, true).unwrap(),
            Some(CompletionAction::CanonicalWorkValidation)
        );

        work.stall.tool_call_records = vec![go_validation_record(true), failure, retry.clone()];
        assert_eq!(
            WorkValidationEvidence::collect(&work, true).state,
            WorkValidationState::Stale,
            "a failure without unchanged evidence invalidates prior named proof: {command}"
        );
        assert_eq!(
            pending_terminal_completion_action_for_work_state(&work, true).unwrap(),
            Some(CompletionAction::CanonicalWorkValidation)
        );

        work.stall.tool_call_records = vec![go_validation_record(true), unchanged_failure.clone()];
        assert_eq!(
            WorkValidationEvidence::collect(&work, true).state,
            WorkValidationState::Failed,
            "v1 unchanged evidence is not a successful verification: {command}"
        );
        work.stall.tool_call_records.push(retry.clone());
        assert_eq!(
            WorkValidationEvidence::collect(&work, true).state,
            WorkValidationState::Passed,
            "v1 preserves prior proof; v2 clears only its matching debt: {command}"
        );

        let mut scratch_verifier = retry.clone();
        scratch_verifier.runtime_args_full = Some(
            json!({
                "command": format!("touch /tmp/astra-verifier-scratch && {command}"),
                "mode": "verify"
            })
            .to_string(),
        );
        work.stall.tool_call_records = vec![go_validation_record(true), scratch_verifier];
        assert_eq!(
            WorkValidationEvidence::collect(&work, true).state,
            WorkValidationState::Passed,
            "an authoritative unchanged receipt outranks a conservative mutation shape: {command}"
        );

        let mut partial_failure = unchanged_failure.clone();
        partial_failure.workspace_mutation_partial = Some(true);
        work.stall.tool_call_records =
            vec![go_validation_record(true), partial_failure, retry.clone()];
        assert_eq!(
            WorkValidationEvidence::collect(&work, true).state,
            WorkValidationState::Stale,
            "partial execution cannot preserve previous named proof: {command}"
        );

        work.stall.tool_call_records = vec![
            go_validation_record(true),
            state.stall.tool_call_records[0].clone(),
            unchanged_failure,
            retry,
        ];
        assert_eq!(
            WorkValidationEvidence::collect(&work, true).state,
            WorkValidationState::Stale,
            "a prior workspace mutation remains stale: {command}"
        );
    }
}

#[tokio::test]
async fn declared_verifier_debt_requires_the_same_live_valid_receipt() {
    for command in [
        "python3 -c 'assert 1 == 1'",
        "python3 - <<'PY'\nassert 1 == 1\nPY",
    ] {
        let (state, _) =
            run_written_verification(command, "success", "success", Some(ExplicitReceipt::Valid))
                .await;
        let valid = state.stall.tool_call_records.last().unwrap().clone();
        let failure = failed_explicit_verification(&valid);

        let mut missing = valid.clone();
        missing.workspace_mutation_receipt = None;
        let mut forged = valid.clone();
        forged.workspace_mutation_receipt.as_mut().unwrap()["source"] = json!("model_claim");
        let failed = failed_explicit_verification(&valid);
        let mut contradictory = valid.clone();
        contradictory.result_class = Some("test_failure".into());
        contradictory.exit_semantics = Some("domain_negative".into());
        let mut partial = valid.clone();
        partial.workspace_mutation_partial = Some(true);
        let mut changed = valid.clone();
        changed.workspace_mutation_observed = Some(true);
        let mut wrong_scope = valid.clone();
        wrong_scope.workspace_mutation_scope = Some("other_scope".into());
        let mut restored = valid.clone();
        restored.runtime_args_full = None;
        let mut v1_only = valid.clone();
        v1_only.workspace_mutation_receipt =
            astra_tools::workspace_observation::unchanged_bash_observation_receipt_with_ownership(
                astra_tools::workspace_observation::INVOCATION_CGROUP_OWNERSHIP,
            )
            .remove(astra_tools::workspace_observation::OBSERVATION_RECEIPT_FIELD);
        let mut other_command = valid.clone();
        other_command.runtime_args_full =
            Some(json!({"command":"python3 -c 'assert 2 == 2'", "mode":"verify"}).to_string());
        let mut other_workdir = valid.clone();
        let mut args: serde_json::Value =
            serde_json::from_str(valid.authoritative_args_full().unwrap()).unwrap();
        args["workdir"] = json!("nested");
        other_workdir.runtime_args_full = Some(args.to_string());

        for (label, invalid) in [
            ("missing", missing),
            ("forged", forged),
            ("failed", failed),
            ("contradictory", contradictory),
            ("partial", partial),
            ("changed", changed),
            ("wrong scope", wrong_scope),
            ("no live arguments", restored),
            ("v1 only", v1_only),
            ("other command", other_command),
            ("other workdir", other_workdir),
        ] {
            let mut work = make_state();
            work.hooks.workspace_root_hint = Some("/workspace".into());
            work.stall.tool_call_records = vec![failure.clone(), invalid];
            assert_eq!(
                WorkValidationEvidence::collect(&work, true).state,
                WorkValidationState::Failed,
                "{command}: {label} must not clear the failed invocation"
            );
            assert_eq!(
                pending_terminal_completion_action_for_work_state(&work, true).unwrap(),
                Some(CompletionAction::CanonicalWorkValidation),
                "{command}: {label}"
            );
            work.stall.tool_call_records.push(valid.clone());
            assert_eq!(
                WorkValidationEvidence::collect(&work, true).state,
                WorkValidationState::None,
                "{command}: {label} followed by the correct receipt"
            );
        }

        let mut dry_run = valid;
        dry_run.runtime_args_full =
            Some(json!({"command":"GOFLAGS=-n go test ./...", "mode":"verify"}).to_string());
        let mut work = make_state();
        work.hooks.workspace_root_hint = Some("/workspace".into());
        work.stall.tool_call_records.push(dry_run);
        assert_eq!(
            WorkValidationEvidence::collect(&work, true).state,
            WorkValidationState::Stale,
            "declared freshness does not turn a known Go dry run into named proof"
        );
    }
}

#[tokio::test]
async fn named_verifier_receipt_does_not_require_duplicate_legacy_success_tags() {
    for command in ["GOWORK=off go test ./...", "cargo test --workspace"] {
        let (state, _) =
            run_written_verification(command, "success", "success", Some(ExplicitReceipt::Valid))
                .await;
        let mut success = state.stall.tool_call_records.last().unwrap().clone();
        success.result_class = None;
        success.exit_semantics = None;
        assert!(
            super::super::lifecycle::record_has_full_scope_explicit_workspace_verification_receipt(
                &success
            )
        );
        let failure = failed_explicit_verification(&success);
        let mut work = make_state();
        work.hooks.workspace_root_hint = Some("/workspace".into());
        work.stall.tool_call_records = vec![failure, success.clone()];
        assert_eq!(
            WorkValidationEvidence::collect(&work, true).state,
            WorkValidationState::Passed,
            "{command}"
        );
        assert_eq!(
            pending_terminal_completion_action_for_work_state(&work, true).unwrap(),
            None,
            "{command}"
        );

        let mut contradictory = success;
        contradictory.result_class = Some("test_failure".into());
        work.stall.tool_call_records = vec![contradictory];
        assert_eq!(
            WorkValidationEvidence::collect(&work, true).state,
            WorkValidationState::Failed,
            "{command}"
        );
        assert_eq!(
            pending_terminal_completion_action_for_work_state(&work, true).unwrap(),
            Some(CompletionAction::CanonicalWorkValidation),
            "{command}"
        );
    }

    let (state, _) = run_written_verification(
        "go test ./...",
        "success",
        "success",
        Some(ExplicitReceipt::Valid),
    )
    .await;
    let mut record = state.stall.tool_call_records.last().unwrap().clone();
    record.result_class = None;
    record.exit_semantics = None;
    let mut work = make_state();
    work.hooks.workspace_root_hint = Some("/workspace".into());
    for args in [
        json!({"command":"GOFLAGS=-n go test ./...", "mode":"verify"}),
        json!({"command":"go test ./...", "mode":"verify", "workdir":"/tmp"}),
    ] {
        record.runtime_args_full = Some(args.to_string());
        work.stall.tool_call_records = vec![record.clone()];
        assert_eq!(
            WorkValidationEvidence::collect(&work, true).state,
            WorkValidationState::Stale,
            "{args}"
        );
        assert_eq!(
            pending_terminal_completion_action_for_work_state(&work, true).unwrap(),
            Some(CompletionAction::CanonicalWorkValidation),
            "{args}"
        );
    }
}

#[tokio::test]
async fn explicit_verification_requires_a_valid_successful_unchanged_receipt() {
    let command = "python3 - <<'PY'\nassert 1 == 1\nPY";
    for receipt in [
        ExplicitReceipt::Missing,
        ExplicitReceipt::Forged,
        ExplicitReceipt::Failed,
        ExplicitReceipt::Partial,
        ExplicitReceipt::Changed,
    ] {
        let (class, semantics) = if matches!(receipt, ExplicitReceipt::Failed) {
            ("test_failure", "domain_negative")
        } else {
            ("success", "success")
        };
        let (state, _) = run_written_verification(command, class, semantics, Some(receipt)).await;
        let record = state.stall.tool_call_records.last().unwrap();
        assert!(
            !super::super::lifecycle::record_has_full_scope_explicit_workspace_verification_receipt(
                record
            ),
            "{receipt:?}: {record:?}"
        );
        assert!(
            !successful_post_mutation_observation(&state).unwrap(),
            "{receipt:?}"
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
    assert_eq!(
        pending_completion_action(&state).unwrap(),
        Some(CompletionAction::RequiredWorkspaceMutation)
    );
}

#[test]
fn unrelated_success_cannot_clear_started_cancellation() {
    for disposition in [ToolCallDisposition::Rejected, ToolCallDisposition::Executed] {
        let mut state = external_request();
        state.final_text = "Operation result remains uncertain.".into();
        let args = json!({"command": "perform-external-operation"}).to_string();
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: false,
            runtime_args_full: Some(args),
            error_kind: Some(astra_core::ErrorKind::Cancelled),
            disposition: Some(disposition),
            ..Default::default()
        });
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            runtime_args_full: Some(json!({"command": "test -e /tmp/other"}).to_string()),
            disposition: Some(ToolCallDisposition::Executed),
            ..Default::default()
        });
        assert!(!enforce_workspace_completion_before_text_completion(
            &mut state
        ));
        assert_eq!(
            state.interruption.is_some(),
            disposition == ToolCallDisposition::Executed
        );
        if disposition == ToolCallDisposition::Executed {
            assert_eq!(
                state.interruption.as_ref().unwrap().kind,
                InterruptionKind::ExecutionIncomplete
            );
            assert!(state.final_text.contains("effects remain unverified"));
        }
    }
}

#[tokio::test]
async fn callback_outcome_facts_survive_journaling_and_control_completion() {
    // Use the shared producer, not hand-written rejection metadata. This is
    // the same callback body/metadata the CLI sends after cancellation.
    for (started, unknown) in [(false, false), (true, false), (false, true)] {
        let mut result =
            make_edge_tool_with_args("bash", json!({"command": "perform-external-operation"}), "");
        let cancelled = astra_tools::cancelled_tool_result("bash", started);
        result.output = cancelled.output;
        result
            .tool_result_fields
            .as_mut()
            .unwrap()
            .extend(cancelled.metadata.unwrap());
        if unknown {
            let fields = result.tool_result_fields.as_mut().unwrap();
            // A lost worker/result cannot assert that dispatch never occurred.
            for key in [
                "status",
                "cancelled",
                "reason",
                "execution_started",
                "side_effects_maybe",
            ] {
                fields.remove(key);
            }
            fields.remove("disposition");
            fields.insert("error_kind".into(), json!("tool_outcome_unknown"));
            fields.insert("execution_fact".into(), json!("unknown"));
            fields.insert("side_effects_maybe".into(), json!(true));
            result.output = "Error: shell acknowledgement lost; reconcile outcome".into();
        }
        result.status = "failed".into();
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![result], 10, 2, None),
            text_result("The operation succeeded.", 10, 2, None),
        ])
        .with_valid_tools(&["bash"]);
        let mut state = external_request();
        let _ = run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .unwrap();
        let record = &state.stall.tool_call_records[0];
        assert_eq!(
            record.error_kind,
            Some(if unknown {
                astra_core::ErrorKind::ToolOutcomeUnknown
            } else {
                astra_core::ErrorKind::Cancelled
            })
        );
        assert_eq!(record.was_executed(), started || unknown);
        assert_eq!(
            state.interruption.is_some(),
            started || unknown,
            "{record:?}"
        );
        assert_eq!(state.stall.tool_call_records.len(), 1, "no recovery write");
        if started || unknown {
            let interruption = state.interruption.as_ref().unwrap();
            assert_eq!(interruption.kind, InterruptionKind::ExecutionIncomplete);
            assert!(
                interruption
                    .error_detail
                    .as_deref()
                    .unwrap()
                    .contains("interrupted or unknown outcome")
            );
            assert!(!record.ok);
            // Interruption is lifecycle truth. Provider content stays a
            // partial answer; it cannot promote the operation to success.
            assert_eq!(state.final_text, "The operation succeeded.");
            assert_eq!(host.final_output_ready, vec![state.final_text.clone()]);
        }
    }
}

#[tokio::test]
async fn timeout_callback_requires_reconciliation_only_for_started_side_effects() {
    // The callback and journal must preserve the executor's timeout and startup
    // facts. A later successful observation cannot settle the earlier operation.
    for (command, started, incomplete) in [
        ("perform-external-operation", true, true),
        ("perform-external-operation", false, false),
        ("pwd", true, false),
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
        assert_eq!(state.interruption.is_some(), incomplete, "{record:?}");
        if incomplete {
            assert_eq!(
                state.interruption.as_ref().unwrap().kind,
                InterruptionKind::ExecutionIncomplete
            );
        }
        assert_eq!(
            host.turn_count(),
            3,
            "no automatic replay or recovery round"
        );
        assert_eq!(state.max_turns, budget);
        assert_eq!(state.stall.tool_call_records.len(), 2);
        assert_eq!(state.final_text, "Preserved partial report.");
        assert_eq!(host.final_output_ready, vec![state.final_text.clone()]);
    }
}
