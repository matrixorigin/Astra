//! Incremental completion obligations; presentation history is not recovery authority.

use std::collections::HashMap;

use astra_services::session_journal::ToolCallRecord;
use astra_turn_types::{StopHook, ToolInvocationCompletionRef};

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum VerificationRecoveryError {
    #[error("verification checkpoint evidence is invalid")]
    InvalidEvidence,
    #[error("verification checkpoint belongs to a different execution or contract")]
    ScopeMismatch,
    #[cfg(test)]
    #[error("verification invocation evidence is missing or does not match the ledger")]
    LedgerMismatch,
    #[error("verification history is unavailable")]
    HistoryUnavailable,
    #[error("verification contract changed beyond the retained history")]
    ContractChanged,
    #[error("verification execution ordinal is exhausted")]
    OrdinalExhausted,
}

#[derive(Clone)]
struct Evidence {
    ordinal: u64,
    invocation: Option<ToolInvocationCompletionRef>,
}

impl Evidence {
    fn restore(evidence: &astra_turn_types::VerificationEvidence) -> Self {
        Self {
            ordinal: evidence.ordinal,
            invocation: Some(evidence.invocation.clone()),
        }
    }
}

fn record_reports_workspace_change(record: &ToolCallRecord) -> bool {
    record.was_executed()
        && (super::lifecycle::record_has_typed_workspace_tool_receipt(record)
            || super::execution_phase::record_has_live_workspace_mutation_receipt(record)
            || record.workspace_writer_applied_bound == Some(true)
            || super::execution_phase::record_has_trusted_partial_workspace_mutation_receipt(
                record,
            ))
}

fn parallel_mutator_batches(records: &[ToolCallRecord]) -> HashMap<&str, Vec<usize>> {
    let mut batches: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, record) in records.iter().enumerate() {
        if record_reports_workspace_change(record)
            && let Some(batch_id) = record.batch_id.as_deref()
        {
            batches.entry(batch_id).or_default().push(index);
        }
    }
    batches
}

fn has_parallel_mutating_peer(
    record: &ToolCallRecord,
    index: usize,
    batches: &HashMap<&str, Vec<usize>>,
) -> bool {
    record.batch_id.as_deref().is_some_and(|batch_id| {
        batches
            .get(batch_id)
            .is_some_and(|indices| indices.iter().any(|peer| *peer != index))
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use astra_services::session_journal::ToolCallDisposition;

    fn hook(command: &str) -> StopHook {
        StopHook {
            label: "same label".into(),
            command: command.into(),
            working_dir: None,
            depends_on: vec![],
            timeout_secs: None,

            authoritative: true,
        }
    }

    fn record(name: &str, ok: bool, command: &str) -> ToolCallRecord {
        let mut record = ToolCallRecord {
            name: name.into(),
            ok,
            disposition: Some(ToolCallDisposition::Executed),
            runtime_args_full: Some(serde_json::json!({"command": command}).to_string()),
            exit_semantics: (name == "bash" && ok).then(|| "success".into()),
            result_class: (name == "bash" && ok).then(|| "success".into()),
            ..Default::default()
        };
        if name == "write_file" && ok {
            record.runtime_args_full =
                Some(serde_json::json!({"path":"/app/value.txt", "content":"updated"}).to_string());
            record.workspace_mutation_observed = Some(true);
            record.workspace_mutation_scope =
                Some(astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into());
            record.workspace_mutation_receipt =
                astra_tools::workspace_observation::typed_workspace_tool_receipt()
                    .remove(astra_tools::workspace_observation::RECEIPT_FIELD);
        }
        record
    }

    fn executed(
        ledger: &mut astra_turn_core::invocation_ledger::InMemoryInvocationLedger,
        id: &str,
        name: &str,
        command: &str,
    ) -> ToolCallRecord {
        let args = if name == "write_file" {
            serde_json::json!({"path":"/app/value.txt", "content":command})
        } else {
            serde_json::json!({"command":command})
        };
        executed_with_args(ledger, id, name, args)
    }

    fn executed_with_args(
        ledger: &mut astra_turn_core::invocation_ledger::InMemoryInvocationLedger,
        id: &str,
        name: &str,
        args: serde_json::Value,
    ) -> ToolCallRecord {
        use astra_turn_types::*;
        let identity = ToolInvocationIdentity::new("user", "session", "run", "chain", id).unwrap();
        let decision = ToolInvocationDecision::new(&serde_json::json!({"allowed": true})).unwrap();
        let fingerprint = ToolInvocationFingerprint::new(
            DurableToolReference::built_in(name, "v1").unwrap(),
            &args,
            &decision.decision_id,
        )
        .unwrap();
        ledger
            .prepare(identity.clone(), fingerprint, decision)
            .unwrap();
        ledger
            .claim_dispatch(
                &identity,
                ToolInvocationDispatchLease::new("owner", u64::MAX).unwrap(),
            )
            .unwrap();
        let mut record = ToolCallRecord {
            runtime_args_full: Some(args.to_string()),
            ..record(name, true, "")
        };
        let mut metadata = std::collections::BTreeMap::new();
        if name == "write_file" {
            metadata = astra_tools::workspace_observation::typed_workspace_tool_receipt()
                .into_iter()
                .collect();
        }
        if let Some(class) = &record.result_class {
            metadata.insert("result_class".into(), serde_json::json!(class));
        }
        let terminal = ledger
            .compare_and_complete(
                &identity,
                ToolInvocationState::Dispatched,
                Some("owner"),
                ToolInvocationTerminalOutcome::Succeeded {
                    result: ToolInvocationResultPayload {
                        output: "ok".into(),
                        metadata,
                        exit_semantics: record.exit_semantics.clone(),
                    },
                },
            )
            .unwrap();
        record.execution_completion = Some(
            ToolInvocationCompletionRef::from_record(&terminal)
                .unwrap()
                .into(),
        );
        record
    }

    fn restore_from_ledger(
        frontier: &VerificationFrontier,
        records: &[ToolCallRecord],
        ledger: &astra_turn_core::invocation_ledger::InMemoryInvocationLedger,
    ) -> VerificationFrontier {
        let astra_turn_types::VerificationHandoff::Bound { snapshot } =
            frontier.export(Some("/app"), &[], records, Some("chain"))
        else {
            panic!("bounded workspace evidence must export");
        };
        let wire = serde_json::to_vec(&snapshot).unwrap();
        let snapshot: astra_turn_types::BoundVerificationFrontier =
            serde_json::from_slice(&wire).unwrap();
        let rows = snapshot
            .evidence()
            .map(|evidence| {
                (
                    evidence.invocation.identity.storage_key(),
                    ledger.get(&evidence.invocation.identity).unwrap().clone(),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>()
            .into_values()
            .collect::<Vec<_>>();
        assert!(
            VerificationFrontier::restore(
                Some("/different"),
                &snapshot,
                "user",
                "session",
                "run",
                "chain",
                &[],
                &rows
            )
            .is_err()
        );
        if !rows.is_empty() {
            assert!(
                VerificationFrontier::restore(
                    Some("/app"),
                    &snapshot,
                    "user",
                    "session",
                    "run",
                    "chain",
                    &[],
                    &rows[..rows.len() - 1]
                )
                .is_err()
            );
            let mut changed = rows.clone();
            changed[0].fingerprint.canonical_arguments_hash = "wrong".into();
            assert!(
                VerificationFrontier::restore(
                    Some("/app"),
                    &snapshot,
                    "user",
                    "session",
                    "run",
                    "chain",
                    &[],
                    &changed
                )
                .is_err()
            );
        }
        let mut unrelated_ledger =
            astra_turn_core::invocation_ledger::InMemoryInvocationLedger::default();
        let unrelated = executed(
            &mut unrelated_ledger,
            "unrelated-ledger-row",
            "read_file",
            "",
        );
        let mut extra_rows = rows.clone();
        extra_rows.push(
            unrelated_ledger
                .get(unrelated.execution_completion.unwrap().identity())
                .unwrap()
                .clone(),
        );
        assert!(matches!(
            VerificationFrontier::restore(
                Some("/app"),
                &snapshot,
                "user",
                "session",
                "run",
                "chain",
                &[],
                &extra_rows
            ),
            Err(VerificationRecoveryError::LedgerMismatch)
        ));

        VerificationFrontier::restore(
            Some("/app"),
            &snapshot,
            "user",
            "session",
            "run",
            "chain",
            &[],
            &rows,
        )
        .unwrap()
    }

    pub(crate) fn restored_workspace_barrier() -> VerificationFrontier {
        let mut ledger = astra_turn_core::invocation_ledger::InMemoryInvocationLedger::default();
        let records = vec![executed_with_args(
            &mut ledger,
            "writer",
            "write_file",
            serde_json::json!({"path":"/app/solution.py"}),
        )];
        let mut frontier = VerificationFrontier::default();
        frontier.advance(Some("/app"), &[], &records).unwrap();
        restore_from_ledger(&frontier, &records, &ledger)
    }

    #[test]
    fn parallel_checks_require_successful_matching_commands() {
        for invalid in [
            None,
            Some("unclassified"),
            Some("mutation"),
            Some("failure"),
        ] {
            let mut ledger =
                astra_turn_core::invocation_ledger::InMemoryInvocationLedger::default();
            let writer = executed_with_args(
                &mut ledger,
                "writer",
                "write_file",
                serde_json::json!({"path": "/app/answer.txt", "content": "new"}),
            );
            let hooks = [hook("./a"), hook("./b")];
            let mut records = vec![writer];
            for (i, command) in ["./a", "./b"].into_iter().enumerate() {
                let args = serde_json::json!({"command": command});
                let mut verifier =
                    executed_with_args(&mut ledger, &format!("verify-{i}"), "bash", args);
                verifier.batch_id = Some("verify-batch".into());
                verifier.parallel = Some(true);
                if i == 0 {
                    match invalid {
                        Some("unclassified") => verifier.result_class = None,
                        Some("mutation") => verifier.workspace_mutation_partial = Some(true),
                        Some("failure") => verifier.ok = false,
                        _ => {}
                    }
                }
                records.push(verifier);
            }
            let mut frontier = VerificationFrontier::default();
            frontier.advance(Some("/app"), &hooks, &records).unwrap();
            assert_eq!(
                frontier.missing().unwrap().is_empty(),
                invalid.is_none(),
                "{invalid:?}"
            );
        }
    }

    #[test]
    fn parallel_explicit_verifier_does_not_prove_a_concurrent_write() {
        let mut ledger = astra_turn_core::invocation_ledger::InMemoryInvocationLedger::default();
        let mut writer = executed_with_args(
            &mut ledger,
            "writer",
            "write_file",
            serde_json::json!({"path": "/app/answer.txt", "content": "new"}),
        );
        let mut verifier = executed_with_args(
            &mut ledger,
            "verifier",
            "bash",
            serde_json::json!({"command": "./verify"}),
        );
        writer.batch_id = Some("parallel-verify".into());
        verifier.batch_id = writer.batch_id.clone();
        let hooks = [hook("./verify")];
        for pair in [
            [writer.clone(), verifier.clone()],
            [verifier.clone(), writer.clone()],
        ] {
            let mut frontier = VerificationFrontier::default();
            frontier.advance(Some("/app"), &hooks, &pair).unwrap();
            assert_eq!(frontier.missing(), Some(vec!["same label".into()]));
        }
    }

    #[test]
    fn incremental_frontier_rejects_a_late_member_of_a_processed_batch() {
        let mut ledger = astra_turn_core::invocation_ledger::InMemoryInvocationLedger::default();
        let mut writer = executed_with_args(
            &mut ledger,
            "writer",
            "write_file",
            serde_json::json!({"path": "/app/answer.txt", "content": "new"}),
        );
        let mut read = executed_with_args(
            &mut ledger,
            "read",
            "read_file",
            serde_json::json!({"path": "/app/answer.txt"}),
        );
        writer.batch_id = Some("incomplete-batch".into());
        read.batch_id = writer.batch_id.clone();
        let mut frontier = VerificationFrontier::default();
        frontier
            .advance(Some("/app"), &[], &[writer.clone()])
            .unwrap();
        assert_eq!(
            frontier.advance(Some("/app"), &[], &[writer, read]),
            Err(VerificationRecoveryError::HistoryUnavailable)
        );
    }

    pub(crate) fn restored_read_only_prefix() -> VerificationFrontier {
        let mut ledger = astra_turn_core::invocation_ledger::InMemoryInvocationLedger::default();
        let records = vec![executed(&mut ledger, "read", "read_file", "")];
        let mut frontier = VerificationFrontier::default();
        frontier.advance(None, &[], &records).unwrap();
        let astra_turn_types::VerificationHandoff::Bound { snapshot } =
            frontier.export(None, &[], &records, Some("chain"))
        else {
            panic!("read-only prefix must export")
        };
        VerificationFrontier::restore(None, &snapshot, "user", "session", "run", "chain", &[], &[])
            .unwrap()
    }

    #[test]
    fn restore_verification_ordinal_exhaustion_does_not_partially_advance() {
        let mut frontier = restored_read_only_prefix();
        frontier.processed_through = u64::MAX - 1;
        let records = [record("read_file", true, ""), record("read_file", true, "")];
        assert_eq!(
            frontier.advance(None, &[], &records),
            Err(VerificationRecoveryError::OrdinalExhausted)
        );
        assert_eq!(frontier.processed_through, u64::MAX - 1);
        assert_eq!(frontier.cursor, 0);
        frontier.advance(None, &[], &records[..1]).unwrap();
        assert_eq!(frontier.processed_through, u64::MAX);
        assert_eq!(
            frontier.advance(None, &[], &records),
            Err(VerificationRecoveryError::OrdinalExhausted)
        );
        assert_eq!(frontier.cursor, 1);
    }

    #[test]
    fn restore_verification_uses_exact_ledger_and_preserves_prefix_obligations() {
        let mut ledger = astra_turn_core::invocation_ledger::InMemoryInvocationLedger::default();
        let hooks = vec![hook("./a"), hook("./a"), hook("./b")];
        let mut history = vec![
            executed(&mut ledger, "write", "write_file", ""),
            executed(&mut ledger, "check-a", "bash", "./a"),
        ];
        let mut uninterrupted = VerificationFrontier::default();
        uninterrupted.advance(None, &hooks, &history).unwrap();
        let astra_turn_types::VerificationHandoff::Bound { snapshot } =
            uninterrupted.export(None, &hooks, &history, Some("chain"))
        else {
            panic!("actual ledger-bound history must export")
        };
        let rows = history
            .iter()
            .map(|record| {
                ledger
                    .get(record.execution_completion.as_ref().unwrap().identity())
                    .unwrap()
                    .clone()
            })
            .collect::<Vec<_>>();
        let mut restored = VerificationFrontier::restore(
            None, &snapshot, "user", "session", "run", "chain", &hooks, &rows,
        )
        .unwrap();
        assert_eq!(
            restored.evaluate(None, &hooks, &[]).unwrap(),
            uninterrupted.missing()
        );
        assert_eq!(restored.cursor, 0);
        assert_eq!(restored.processed_through, 2);
        let paired = history
            .iter()
            .map(
                |record| crate::server::server_loop_host::HistoricalToolCallContinuation {
                    record: record.clone(),
                    completion: record.execution_completion.clone(),
                },
            )
            .collect::<Vec<_>>();
        let cursor = VerificationFrontier::adopted_history_cursor(
            &snapshot, &paired, "user", "session", "run", "chain",
        )
        .unwrap();
        let mut complete_history =
            VerificationFrontier::restore_bound_snapshot(None, &snapshot, "chain", &hooks, cursor)
                .unwrap();
        complete_history.advance(None, &hooks, &history).unwrap();
        assert_eq!(complete_history.processed_through, 2);
        assert_eq!(complete_history.missing(), uninterrupted.missing());
        let mut changed_prefix = paired.clone();
        changed_prefix[1].completion = None;
        assert!(
            VerificationFrontier::adopted_history_cursor(
                &snapshot,
                &changed_prefix,
                "user",
                "session",
                "run",
                "chain",
            )
            .is_err()
        );
        assert!(
            VerificationFrontier::restore(
                None, &snapshot, "other", "session", "run", "chain", &hooks, &rows
            )
            .is_err()
        );
        assert!(
            VerificationFrontier::restore(
                None,
                &snapshot,
                "user",
                "session",
                "run",
                "chain",
                &hooks,
                &rows[..1]
            )
            .is_err()
        );
        let mut changed = rows.clone();
        changed[1].fingerprint.canonical_arguments_hash = "different-arguments".into();
        assert!(
            VerificationFrontier::restore(
                None, &snapshot, "user", "session", "run", "chain", &hooks, &changed
            )
            .is_err()
        );
        let mut changed = rows.clone();
        changed[1].outcome = Some(astra_turn_types::ToolInvocationTerminalOutcome::Succeeded {
            result: astra_turn_types::ToolInvocationResultPayload {
                output: "different".into(),
                metadata: Default::default(),
                exit_semantics: None,
            },
        });
        assert!(
            VerificationFrontier::restore(
                None, &snapshot, "user", "session", "run", "chain", &hooks, &changed
            )
            .is_err()
        );
        let suffix = vec![executed(&mut ledger, "check-b", "bash", "./b")];
        history.extend(suffix.clone());
        complete_history.advance(None, &hooks, &history).unwrap();
        assert_eq!(complete_history.processed_through, 3);
        complete_history.advance(None, &hooks, &history).unwrap();
        assert_eq!(complete_history.processed_through, 3);
        uninterrupted.advance(None, &hooks, &history).unwrap();
        assert_eq!(complete_history.missing(), uninterrupted.missing());
        restored.advance(None, &hooks, &suffix).unwrap();
        assert_eq!(restored.missing(), uninterrupted.missing());
        assert_eq!(restored.processed_through, uninterrupted.processed_through);
        assert_eq!(
            restored.evaluate(None, &[hook("./changed")], &suffix),
            Err(VerificationRecoveryError::ContractChanged)
        );
        assert_eq!(
            restored.advance(None, &hooks, &[]),
            Err(VerificationRecoveryError::HistoryUnavailable)
        );
        assert_eq!(restored.processed_through, 3);
        let mut suffix = suffix;
        suffix.push(executed(&mut ledger, "write-again", "write_file", ""));
        history.push(suffix.last().unwrap().clone());
        restored.advance(None, &hooks, &suffix).unwrap();
        uninterrupted.advance(None, &hooks, &history).unwrap();
        assert_eq!(restored.missing(), uninterrupted.missing());
        assert_eq!(restored.missing(), Some(vec!["same label".into(); 3]));
    }

    // Independent full-history ordering oracle. The producer fact selector
    // is shared; this intentionally recomputes the query rather than folding
    // the incremental cursor/retained-prefix representation.
    fn full_scan(hooks: &[StopHook], records: &[ToolCallRecord]) -> Option<Vec<String>> {
        let hooks: Vec<_> = hooks.iter().filter(|hook| hook.authoritative).collect();
        if hooks.is_empty() {
            return None;
        }
        let changed_at = records.iter().rposition(record_reports_workspace_change);
        Some(
            hooks
                .iter()
                .filter(|hook| {
                    !records
                        .iter()
                        .enumerate()
                        .rev()
                        .find(|(_, record)| {
                            let args = super::super::lifecycle::extract_tool_args(
                                record.authoritative_args_full(),
                            );
                            record.was_executed()
                                && super::super::execution_phase::tool_call_verifies_explicit_hook(
                                    &record.name,
                                    args.as_ref(),
                                    hook,
                                    None,
                                )
                        })
                        .is_some_and(|(index, record)| {
                            changed_at.is_none_or(|change| index >= change)
                                && super::super::execution_phase::record_verifies_explicit_hook(
                                    record, hook, None,
                                )
                        })
                })
                .map(|hook| hook.label.clone())
                .collect(),
        )
    }

    #[test]
    fn incremental_prefixes_match_full_history_and_queries_do_not_advance() {
        let hooks = vec![hook("./a"), hook("./b")];
        let mut rejected = record("write_file", false, "");
        rejected.disposition = Some(ToolCallDisposition::Rejected);
        let choices = [
            record("write_file", true, ""),
            record("write_file", false, ""),
            record("read_file", true, ""),
            record("bash", true, "./a"),
            record("bash", true, "./b"),
            record("bash", false, "./a"),
            rejected,
        ];
        for mut sequence in 0..choices.len().pow(4) {
            let mut frontier = VerificationFrontier::default();
            let mut records = vec![];
            for _ in 0..4 {
                records.push(choices[sequence % choices.len()].clone());
                sequence /= choices.len();
                let before = frontier.processed_through;
                assert_eq!(
                    frontier.evaluate(None, &hooks, &records).unwrap(),
                    full_scan(&hooks, &records)
                );
                assert_eq!(frontier.processed_through, before);
                frontier.advance(None, &hooks, &records).unwrap();
                assert_eq!(frontier.missing(), full_scan(&hooks, &records));
                frontier.advance(None, &hooks, &records).unwrap();
                assert_eq!(frontier.processed_through, records.len() as u64);
                assert!(
                    frontier.proofs.len() + usize::from(frontier.mutation.is_some())
                        <= hooks.len() + 1
                );
            }
        }
    }

    #[test]
    fn changed_hook_requires_its_own_execution_outcome() {
        let records = vec![record("bash", true, "./a")];
        let mut frontier = VerificationFrontier::default();
        frontier.advance(None, &[hook("./a")], &records).unwrap();
        assert_eq!(frontier.missing(), Some(vec![]));
        frontier.advance(None, &[hook("./b")], &records).unwrap();
        assert_eq!(frontier.missing(), Some(vec!["same label".into()]));
        assert_eq!(frontier.missing(), full_scan(&[hook("./b")], &records));
    }

    #[test]
    fn export_requires_only_retained_obligation_evidence() {
        use astra_turn_types::{VerificationHandoff, VerificationUnavailable};
        let frontier = VerificationFrontier::default();
        let hooks = vec![hook("./a")];
        assert!(matches!(
            frontier.export(None, &hooks, &[], None),
            VerificationHandoff::Unavailable {
                detail: VerificationUnavailable::MissingTurnChain
            }
        ));
        let mut ledger = astra_turn_core::invocation_ledger::InMemoryInvocationLedger::default();
        let records = vec![
            executed(&mut ledger, "read", "read_file", ""),
            executed(&mut ledger, "verify", "bash", "./a"),
        ];
        assert!(
            matches!(
                frontier.export(None, &hooks, &records, Some("chain")),
                VerificationHandoff::Bound { .. }
            ),
            "declared check outcomes can be retained without prior changes"
        );
        let mut records = records;
        records.push(record("write_file", true, ""));
        assert!(matches!(
            frontier.export(None, &hooks, &records, Some("chain")),
            VerificationHandoff::Unavailable {
                detail: VerificationUnavailable::UnboundMutation
            }
        ));
        assert!(
            matches!(
                frontier.export(None, &[], &records, Some("chain")),
                VerificationHandoff::Unavailable {
                    detail: VerificationUnavailable::InvalidEvidence
                }
            ),
            "no explicit hooks cannot erase an unbound general workspace barrier"
        );
        assert_eq!(
            frontier.evaluate(None, &hooks, &records).unwrap(),
            Some(vec!["same label".into()])
        );
    }

    #[test]
    fn long_history_retains_only_hook_frontier_and_repeated_folds_are_idempotent() {
        let hooks = vec![hook("./a"), hook("./a"), hook("./b")];
        let mut records = vec![
            record("write_file", true, ""),
            record("bash", true, "./a"),
            record("bash", true, "./b"),
        ];
        let mut frontier = VerificationFrontier::default();
        frontier.advance(None, &hooks, &records).unwrap();
        assert_eq!(frontier.missing(), Some(vec![]));
        for index in 0..10_000 {
            records.push(record("read_file", true, ""));
            frontier.advance(None, &hooks, &records).unwrap();
            frontier.advance(None, &hooks, &records).unwrap();
            assert_eq!(frontier.processed_through, records.len() as u64);
            let retained = frontier
                .mutation
                .iter()
                .chain(frontier.proofs.iter().flatten())
                .count();
            assert!(retained <= hooks.len() + 1);
            if index % 257 == 0 {
                assert_eq!(frontier.missing(), full_scan(&hooks, &records));
            }
        }
        records.push(record("write_file", true, ""));
        frontier.advance(None, &hooks, &records).unwrap();
        assert_eq!(frontier.missing(), Some(vec!["same label".into(); 3]));
        assert_eq!(frontier.missing(), full_scan(&hooks, &records));
    }
    #[test]
    fn a_later_failed_declared_check_replaces_its_earlier_success() {
        let args = serde_json::json!({"command":"./requested-check"});
        let mut ledger = astra_turn_core::invocation_ledger::InMemoryInvocationLedger::default();
        let first = executed_with_args(&mut ledger, "first", "bash", args.clone());
        let mut latest = executed_with_args(&mut ledger, "latest", "bash", args);
        latest.ok = false;
        latest.result_class = Some("test_failure".into());
        latest.exit_semantics = Some("domain_negative".into());
        let hooks = [hook("./requested-check")];
        let mut frontier = VerificationFrontier::default();
        frontier
            .advance(Some("/app"), &hooks, std::slice::from_ref(&first))
            .unwrap();
        assert_eq!(frontier.missing(), Some(Vec::new()));
        frontier
            .advance(Some("/app"), &hooks, &[first, latest])
            .unwrap();
        assert_eq!(frontier.missing(), Some(vec!["same label".into()]));
    }

    #[tokio::test]
    async fn declared_commands_record_outcomes_without_claiming_global_freshness() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("value.txt"), "before").unwrap();
        let ctx = astra_tools::ToolContext::test(workspace.path());
        let command = "printf after > value.txt && printf 'check result\\n'";
        let args = serde_json::json!({"command":command});
        let result = astra_tools::shell_ops::execute_bash(&ctx, &args).await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("value.txt")).unwrap(),
            "after"
        );
        let fields = result.metadata.as_ref().unwrap();
        assert_eq!(fields["exit_code"], 0);
        assert!(
            !fields.contains_key(astra_tools::workspace_observation::OBSERVATION_RECEIPT_FIELD)
        );
        let mut ledger = astra_turn_core::invocation_ledger::InMemoryInvocationLedger::default();
        let mut record = executed_with_args(&mut ledger, "ordinary-check", "bash", args);
        record.result_class = fields
            .get("result_class")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        record.exit_semantics = fields
            .get("exit_semantics")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let hooks = [hook(command)];
        let mut frontier = VerificationFrontier::default();
        frontier
            .advance(workspace.path().to_str(), &hooks, &[])
            .unwrap();
        assert_eq!(frontier.missing(), Some(vec!["same label".into()]));
        frontier
            .advance(
                workspace.path().to_str(),
                &hooks,
                std::slice::from_ref(&record),
            )
            .unwrap();
        assert_eq!(
            frontier.missing(),
            Some(Vec::new()),
            "only the declared invocation's result is established"
        );
        let mut failed = record;
        failed.ok = false;
        failed.result_class = Some("test_failure".into());
        failed.exit_semantics = Some("domain_negative".into());
        let mut failure = VerificationFrontier::default();
        failure
            .advance(workspace.path().to_str(), &hooks, &[failed])
            .unwrap();
        assert_eq!(failure.missing(), Some(vec!["same label".into()]));
    }
}

/// Retains at most one mutation and one proof per authoritative hook.
/// The vector cursor is process-local; the ordinal belongs to execution order.
#[derive(Clone)]
pub(crate) struct VerificationFrontier {
    contract: Vec<StopHook>,
    workspace_root: Option<String>,
    cursor: usize,
    processed_through: u64,
    mutation: Option<Evidence>,
    proofs: Vec<Option<Evidence>>,
    /// The saved prefix has exact evidence, but cannot be reclassified from
    /// this process's suffix-only ToolCallRecord vector.
    historical_prefix: bool,
}

impl Default for VerificationFrontier {
    fn default() -> Self {
        Self {
            contract: Vec::new(),
            workspace_root: None,
            cursor: 0,
            processed_through: 0,
            mutation: None,
            proofs: Vec::new(),
            historical_prefix: false,
        }
    }
}

impl VerificationFrontier {
    /// Rebuild the reducer from a semantically scoped snapshot. The caller is
    /// responsible for proving the snapshot's provenance; this helper owns the
    /// invariant checks and the one reconstruction path shared by raw tests
    /// and atomically adopted execution handoffs.
    fn restore_bound_snapshot(
        workspace_root: Option<&str>,
        snapshot: &astra_turn_types::BoundVerificationFrontier,
        turn_chain_id: &str,
        hooks: &[StopHook],
        cursor: usize,
    ) -> Result<Self, VerificationRecoveryError> {
        snapshot
            .validate()
            .map_err(|_| VerificationRecoveryError::InvalidEvidence)?;
        if snapshot.canonical_turn_chain_id != turn_chain_id
            || snapshot.workspace_root.as_deref() != workspace_root
            || !snapshot
                .contract
                .iter()
                .eq(hooks.iter().filter(|hook| hook.authoritative))
        {
            return Err(VerificationRecoveryError::ScopeMismatch);
        }
        Ok(Self {
            contract: snapshot.contract.clone(),
            workspace_root: snapshot.workspace_root.clone(),
            cursor,
            processed_through: snapshot.processed_through,
            mutation: snapshot.mutation.as_ref().map(Evidence::restore),
            proofs: snapshot
                .proofs
                .iter()
                .map(|proof| proof.as_ref().map(Evidence::restore))
                .collect(),
            historical_prefix: snapshot.processed_through != 0,
        })
    }

    fn adopted_history_cursor(
        snapshot: &astra_turn_types::BoundVerificationFrontier,
        history: &[crate::server::server_loop_host::HistoricalToolCallContinuation],
        user_id: &str,
        session_id: &str,
        run_id: &str,
        turn_chain_id: &str,
    ) -> Result<usize, VerificationRecoveryError> {
        let history_len = u64::try_from(history.len())
            .map_err(|_| VerificationRecoveryError::OrdinalExhausted)?;
        if snapshot.processed_through != history_len {
            return Err(VerificationRecoveryError::InvalidEvidence);
        }
        for evidence in snapshot.evidence() {
            let identity = &evidence.invocation.identity;
            if identity.user_id != user_id
                || identity.session_id != session_id
                || identity.run_id != run_id
                || identity.turn_chain_id != turn_chain_id
            {
                return Err(VerificationRecoveryError::ScopeMismatch);
            }
            let index = usize::try_from(
                evidence
                    .ordinal
                    .checked_sub(1)
                    .ok_or(VerificationRecoveryError::InvalidEvidence)?,
            )
            .map_err(|_| VerificationRecoveryError::OrdinalExhausted)?;
            let Some(entry) = history.get(index) else {
                return Err(VerificationRecoveryError::InvalidEvidence);
            };
            if !entry.record.was_executed()
                || entry
                    .completion
                    .as_ref()
                    .and_then(|completion| completion.as_invocation())
                    != Some(&evidence.invocation)
            {
                return Err(VerificationRecoveryError::InvalidEvidence);
            }
        }
        Ok(history.len())
    }

    /// Restore only the verification prefix carried by an atomically adopted
    /// execution handoff. `AdoptedExecutionHandoff` is deliberately not
    /// deserializable: its locked checkpoint and receipt are the provenance
    /// boundary. This method therefore accepts no arbitrary snapshot, no
    /// trusted flag, and no historical ledger rows.
    ///
    /// The current workspace and hook contract are required to match the
    /// retained prefix. This restores observation facts only; current tool,
    /// Work, and execution permissions remain owned by their live admission
    /// paths. In particular, an `OutcomeUnknown` invocation is copied with its
    /// original state and is never promoted to a successful outcome.
    pub(crate) fn restore_from_adopted_handoff(
        workspace_root: Option<&str>,
        hooks: &[StopHook],
        adopted: &astra_services::session_context_coordinator::ResumedExecutionTurn,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        turn_chain_id: &str,
    ) -> Result<Self, VerificationRecoveryError> {
        let receipt = adopted.receipt();
        let checkpoint = adopted.checkpoint();
        if checkpoint.checkpoint_kind != "execution_handoff"
            || checkpoint.checkpoint_version != "execution_handoff_v1"
            || checkpoint.user_id != user_id
            || checkpoint.session_id != session_id
            || checkpoint.run_id != run_id
            || receipt.run_id != run_id
        {
            return Err(VerificationRecoveryError::ScopeMismatch);
        }

        let payload =
            crate::server::server_loop_host::RuntimeExecutionHandoff::from_adopted(adopted)
                .map_err(|_| VerificationRecoveryError::InvalidEvidence)?;
        if payload.original_facts.canonical_turn_chain_id.as_deref() != Some(turn_chain_id) {
            return Err(VerificationRecoveryError::ScopeMismatch);
        }
        let snapshot = match &payload.verification {
            astra_turn_types::VerificationHandoff::Bound { snapshot } => snapshot.as_ref(),
            astra_turn_types::VerificationHandoff::Unavailable { .. } => {
                return Err(VerificationRecoveryError::HistoryUnavailable);
            }
        };
        let cursor = Self::adopted_history_cursor(
            snapshot,
            &payload.tool_history,
            user_id,
            session_id,
            run_id,
            turn_chain_id,
        )?;
        Self::restore_bound_snapshot(workspace_root, snapshot, turn_chain_id, hooks, cursor)
    }

    /// Semantic association comes from the immutable owner checkpoint;
    /// terminal authenticity comes from these exact invocation ledger rows.
    #[cfg(test)]
    pub(crate) fn restore(
        workspace_root: Option<&str>,
        snapshot: &astra_turn_types::BoundVerificationFrontier,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        turn_chain_id: &str,
        hooks: &[StopHook],
        records: &[astra_turn_types::ToolInvocationRecord],
    ) -> Result<Self, VerificationRecoveryError> {
        let restored =
            Self::restore_bound_snapshot(workspace_root, snapshot, turn_chain_id, hooks, 0)?;
        let mut actual = std::collections::BTreeMap::new();
        for record in records {
            let reference = ToolInvocationCompletionRef::from_record(record)
                .map_err(|_| VerificationRecoveryError::LedgerMismatch)?;
            if actual
                .insert(reference.identity.storage_key(), reference)
                .is_some()
            {
                return Err(VerificationRecoveryError::LedgerMismatch);
            }
        }
        let mut expected = std::collections::BTreeSet::new();
        for evidence in snapshot.evidence() {
            let identity = &evidence.invocation.identity;
            if identity.user_id != user_id
                || identity.session_id != session_id
                || identity.run_id != run_id
                || identity.turn_chain_id != turn_chain_id
            {
                return Err(VerificationRecoveryError::ScopeMismatch);
            }
            let key = identity.storage_key();
            if actual.get(&key) != Some(&evidence.invocation) {
                return Err(VerificationRecoveryError::LedgerMismatch);
            }
            expected.insert(key);
        }
        if expected.len() != actual.len() {
            return Err(VerificationRecoveryError::LedgerMismatch);
        }
        Ok(restored)
    }

    pub(crate) fn export(
        &self,
        workspace_root: Option<&str>,
        hooks: &[StopHook],
        records: &[ToolCallRecord],
        canonical_turn_chain_id: Option<&str>,
    ) -> astra_turn_types::VerificationHandoff {
        use astra_turn_types::{
            BoundVerificationFrontier, VerificationEvidence, VerificationHandoff,
            VerificationUnavailable,
        };
        let unavailable = |detail| VerificationHandoff::Unavailable { detail };
        let Some(chain) = canonical_turn_chain_id.filter(|chain| !chain.trim().is_empty()) else {
            return unavailable(VerificationUnavailable::MissingTurnChain);
        };
        if self.cursor > records.len() {
            return unavailable(VerificationUnavailable::HistoryUnavailable);
        }
        let mut view = self.clone();
        if view.advance(workspace_root, hooks, records).is_err() {
            return unavailable(VerificationUnavailable::HistoryUnavailable);
        }
        let bind = |evidence: &Evidence| {
            evidence
                .invocation
                .clone()
                .map(|invocation| VerificationEvidence {
                    ordinal: evidence.ordinal,
                    invocation,
                })
        };
        // Without an authoritative obligation or source mutation, old
        // verifier calls are irrelevant to the current completion predicate.
        let mutation = if view.contract.is_empty() {
            None
        } else if let Some(evidence) = &view.mutation {
            let Some(bound) = bind(evidence) else {
                return unavailable(VerificationUnavailable::UnboundMutation);
            };
            Some(bound)
        } else {
            None
        };
        let mut proofs = vec![None; view.contract.len()];
        if mutation.is_some() {
            for (index, evidence) in view.proofs.iter().enumerate() {
                if let Some(evidence) = evidence {
                    let Some(bound) = bind(evidence) else {
                        return unavailable(VerificationUnavailable::UnboundProof {
                            hook_index: index,
                        });
                    };
                    proofs[index] = Some(bound);
                }
            }
        }
        // Handoff custody is enforced independently from named verification.
        // Never turn an unbound side-effecting invocation into an empty proof.
        if records.iter().any(|record| {
            record.was_executed()
                && record.execution_completion.is_none()
                && crate::turn::tool_side_effects::tool_call_may_mutate_any_state(
                    &record.name,
                    super::lifecycle::extract_tool_args(record.authoritative_args_full()).as_ref(),
                )
        }) {
            return unavailable(VerificationUnavailable::InvalidEvidence);
        }
        let snapshot = BoundVerificationFrontier {
            canonical_turn_chain_id: chain.into(),
            contract: view.contract,
            processed_through: view.processed_through,
            mutation,
            proofs,
            workspace_root: view.workspace_root,
        };
        if snapshot.validate().is_err() {
            return unavailable(VerificationUnavailable::InvalidEvidence);
        }
        VerificationHandoff::Bound {
            snapshot: Box::new(snapshot),
        }
    }

    pub(crate) fn evaluate(
        &self,
        workspace_root: Option<&str>,
        hooks: &[StopHook],
        records: &[ToolCallRecord],
    ) -> Result<Option<Vec<String>>, VerificationRecoveryError> {
        if self.cursor == records.len()
            && self.workspace_root.as_deref() == workspace_root
            && self
                .contract
                .iter()
                .eq(hooks.iter().filter(|hook| hook.authoritative))
        {
            return Ok(self.missing());
        }
        // Completion queries remain read-only. The tool boundary owns the
        // persistent accumulator; a query may see a newly added contract or
        // a not-yet-folded suffix and evaluates that view without mutating it.
        let mut view = self.clone();
        view.advance(workspace_root, hooks, records)?;
        Ok(view.missing())
    }

    pub(crate) fn advance(
        &mut self,
        workspace_root: Option<&str>,
        hooks: &[StopHook],
        records: &[ToolCallRecord],
    ) -> Result<(), VerificationRecoveryError> {
        let contract: Vec<_> = hooks
            .iter()
            .filter(|hook| hook.authoritative)
            .cloned()
            .collect();
        if self.cursor > records.len() {
            return Err(VerificationRecoveryError::HistoryUnavailable);
        }
        let contract_changed =
            self.contract != contract || self.workspace_root.as_deref() != workspace_root;
        if contract_changed && self.historical_prefix {
            return Err(VerificationRecoveryError::ContractChanged);
        }
        if self.cursor > 0
            && self.cursor < records.len()
            && records[self.cursor - 1].batch_id.is_some()
            && records[self.cursor - 1].batch_id == records[self.cursor].batch_id
        {
            // A batch is folded only after the tool phase has collected every
            // member. Otherwise an earlier observer could have been granted
            // proof before its concurrent writer arrived.
            return Err(VerificationRecoveryError::HistoryUnavailable);
        }
        let (base, start) = if contract_changed {
            (0, 0)
        } else {
            (self.processed_through, self.cursor)
        };
        let additional = u64::try_from(records.len() - start)
            .map_err(|_| VerificationRecoveryError::OrdinalExhausted)?;
        base.checked_add(additional)
            .ok_or(VerificationRecoveryError::OrdinalExhausted)?;
        if contract_changed {
            // A different declaration needs its own outcomes. Rebuild from
            // retained execution facts instead of carrying unrelated proof.
            *self = Self {
                proofs: vec![None; contract.len()],
                contract,
                workspace_root: workspace_root.map(str::to_owned),
                ..Self::default()
            };
        }
        if !contract_changed && self.cursor == records.len() {
            return Ok(());
        }
        // Tool-phase hands off a complete batch with the round snapshot. A
        // retained prefix cannot gain a later member of the same batch.
        let suffix = &records[self.cursor..];
        let parallel_mutators = parallel_mutator_batches(suffix);
        for (index, record) in suffix.iter().enumerate() {
            self.processed_through += 1;
            if !record.was_executed() {
                continue;
            }
            let parallel_mutating_peer =
                has_parallel_mutating_peer(record, index, &parallel_mutators);
            let evidence = Evidence {
                ordinal: self.processed_through,
                invocation: record
                    .execution_completion
                    .as_ref()
                    .and_then(|reference| reference.as_invocation())
                    .cloned(),
            };
            if record_reports_workspace_change(record) {
                self.mutation = Some(evidence.clone());
                self.proofs.fill(None);
            }
            let args = super::lifecycle::extract_tool_args(record.authoritative_args_full());
            for (hook, proof) in self.contract.iter().zip(&mut self.proofs) {
                if super::execution_phase::tool_call_verifies_explicit_hook(
                    &record.name,
                    args.as_ref(),
                    hook,
                    workspace_root,
                ) {
                    *proof = (!parallel_mutating_peer
                        && super::execution_phase::record_verifies_explicit_hook(
                            record,
                            hook,
                            workspace_root,
                        ))
                    .then(|| evidence.clone());
                }
            }
        }
        self.cursor = records.len();
        tracing::trace!(
            processed_through = self.processed_through,
            retained_evidence = self
                .mutation
                .iter()
                .chain(self.proofs.iter().flatten())
                .count(),
            bound_evidence = self
                .mutation
                .iter()
                .chain(self.proofs.iter().flatten())
                .filter(|evidence| evidence.invocation.is_some())
                .count(),
            "advanced explicit verification frontier"
        );
        Ok(())
    }

    pub(crate) fn missing(&self) -> Option<Vec<String>> {
        if self.contract.is_empty() {
            return None;
        }
        let changed_through = self.mutation.as_ref().map_or(0, |change| change.ordinal);
        Some(
            self.contract
                .iter()
                .zip(&self.proofs)
                .filter(|(_, proof)| {
                    !proof
                        .as_ref()
                        .is_some_and(|proof| proof.ordinal >= changed_through)
                })
                .map(|(hook, _)| hook.label.clone())
                .collect(),
        )
    }
}
