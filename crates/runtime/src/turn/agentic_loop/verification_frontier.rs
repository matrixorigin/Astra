//! Incremental completion obligations; presentation history is not recovery authority.

use astra_services::session_journal::ToolCallRecord;
use astra_turn_types::{StopHook, ToolInvocationCompletionRef};

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum VerificationRecoveryError {
    #[cfg(test)]
    #[error("verification checkpoint evidence is invalid")]
    InvalidEvidence,
    #[cfg(test)]
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
    fn bind(&self) -> Option<astra_turn_types::VerificationEvidence> {
        Some(astra_turn_types::VerificationEvidence {
            ordinal: self.ordinal,
            invocation: self.invocation.clone()?,
        })
    }

    #[cfg(test)]
    fn restore(evidence: &astra_turn_types::VerificationEvidence) -> Self {
        Self {
            ordinal: evidence.ordinal,
            invocation: Some(evidence.invocation.clone()),
        }
    }
}

#[derive(Clone)]
struct MutationSource {
    evidence: Evidence,
    delivered_path: Option<std::path::PathBuf>,
}

impl MutationSource {
    fn bind(&self) -> Option<astra_turn_types::WorkspaceMutationSource> {
        Some(astra_turn_types::WorkspaceMutationSource {
            evidence: self.evidence.bind()?,
            delivered_path: self
                .delivered_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
        })
    }

    #[cfg(test)]
    fn restore(source: &astra_turn_types::WorkspaceMutationSource) -> Self {
        Self {
            evidence: Evidence::restore(&source.evidence),
            delivered_path: source.delivered_path.as_ref().map(std::path::PathBuf::from),
        }
    }
}

#[derive(Clone)]
struct ObservationProof {
    evidence: Evidence,
    literal_source: Option<MutationSource>,
}

/// The workspace predicate has two distinct frontiers: an observation debt,
/// and the latest positive writer that may justify executing a delivered file.
/// A failed or opaque newer writer must erase that delivery even when it has
/// no explicit path. This reducer replaces backward scans and prefix searches.
#[derive(Clone, Default)]
pub(super) struct WorkspaceObservationFacts {
    barrier: Option<Evidence>,
    proof: Option<ObservationProof>,
    latest_source: Option<MutationSource>,
}

impl WorkspaceObservationFacts {
    pub(super) fn observe(
        &mut self,
        record: &ToolCallRecord,
        ordinal: u64,
        verified_explicit_hook: bool,
        root: Option<&str>,
    ) {
        if !record.was_executed() {
            return;
        }
        let args = super::lifecycle::extract_tool_args(record.authoritative_args_full());
        let literal_command = (record.name == "bash")
            .then(|| {
                args.as_ref()
                    .and_then(astra_turn_core::tool_argument_hints::command_hint_from_args)
                    .filter(|command| {
                        super::lifecycle::bash_command_has_literal_script_artifact_observation_shape(command)
                    })
            })
            .flatten();
        let evidence = Evidence {
            ordinal,
            invocation: record
                .execution_completion
                .as_ref()
                .and_then(|reference| reference.as_invocation())
                .cloned(),
        };
        let full_scope =
            super::lifecycle::record_has_full_scope_explicit_workspace_verification_receipt(record);
        let literal_source = literal_command
            .and_then(|command| {
                super::lifecycle::bash_literal_script_artifact_observation_target(command, root)
            })
            .and_then(|target| {
                self.latest_source
                    .as_ref()
                    .filter(|source| source.delivered_path.as_ref() == Some(&target))
            })
            .cloned();
        let observes = verified_explicit_hook
            || (record.ok
                && super::lifecycle::record_can_observe_bound_workspace(root, record)
                && (full_scope || literal_command.is_none() || literal_source.is_some()));
        let barrier = !verified_explicit_hook
            && crate::turn::tool_side_effects::tool_call_may_mutate_workspace(
                &record.name,
                args.as_ref(),
            )
            && !super::execution_phase::record_is_proven_external_scratch_mutation(root, record);
        if barrier {
            self.barrier = Some(evidence.clone());
            // A compound invocation may itself supply the post-mutation receipt.
            self.proof = None;
        }
        if observes && self.barrier.is_some() {
            self.proof = Some(ObservationProof {
                evidence: evidence.clone(),
                literal_source: if verified_explicit_hook || full_scope {
                    None
                } else {
                    literal_source
                },
            });
        }
        // Evaluate the observer against the PREVIOUS source, then replace the
        // source. Successful explicit hooks can still be positive writers.
        if super::execution_phase::tool_record_may_have_mutated_bound_workspace(root, record) {
            let delivered_path = (record.ok
                && super::lifecycle::record_has_typed_workspace_tool_receipt(record))
            .then(|| super::lifecycle::record_explicit_path(record))
            .flatten()
            .and_then(|path| super::lifecycle::normalize_workspace_path(&path, root));
            self.latest_source = Some(MutationSource {
                evidence,
                delivered_path,
            });
        }
    }

    pub(super) fn is_satisfied(&self) -> bool {
        self.barrier.is_none() || self.proof.is_some()
    }

    fn bind(&self) -> Option<astra_turn_types::BoundWorkspaceObservation> {
        Some(astra_turn_types::BoundWorkspaceObservation {
            barrier: match &self.barrier {
                None => None,
                Some(evidence) => Some(evidence.bind()?),
            },
            proof: match &self.proof {
                None => None,
                Some(proof) => Some(astra_turn_types::WorkspaceObservationProof {
                    evidence: proof.evidence.bind()?,
                    literal_source: match &proof.literal_source {
                        None => None,
                        Some(source) => Some(source.bind()?),
                    },
                }),
            },
            latest_source: match &self.latest_source {
                None => None,
                Some(source) => Some(source.bind()?),
            },
        })
    }

    #[cfg(test)]
    fn restore(snapshot: &astra_turn_types::BoundWorkspaceObservation) -> Self {
        Self {
            barrier: snapshot.barrier.as_ref().map(Evidence::restore),
            proof: snapshot.proof.as_ref().map(|proof| ObservationProof {
                evidence: Evidence::restore(&proof.evidence),
                literal_source: proof.literal_source.as_ref().map(MutationSource::restore),
            }),
            latest_source: snapshot.latest_source.as_ref().map(MutationSource::restore),
        }
    }
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
            cache_key: None,
            authoritative: true,
        }
    }

    fn record(name: &str, ok: bool, command: &str) -> ToolCallRecord {
        ToolCallRecord {
            name: name.into(),
            ok,
            disposition: Some(ToolCallDisposition::Executed),
            runtime_args_full: Some(serde_json::json!({"command": command}).to_string()),
            ..Default::default()
        }
    }

    fn executed(
        ledger: &mut astra_turn_core::invocation_ledger::InMemoryInvocationLedger,
        id: &str,
        name: &str,
        command: &str,
    ) -> ToolCallRecord {
        executed_with_args(ledger, id, name, serde_json::json!({"command": command}))
    }

    #[test]
    fn task_resolution_recovered_workspace_proof_cannot_cross_later_writer() {
        let mut ledger = astra_turn_core::invocation_ledger::InMemoryInvocationLedger::default();
        let read = executed_with_args(
            &mut ledger,
            "read",
            "list_dir",
            serde_json::json!({"path": "/app"}),
        );
        let write = executed_with_args(
            &mut ledger,
            "write",
            "write_file",
            serde_json::json!({"path": "/app/artifact", "content": "changed"}),
        );
        let reference = read.execution_completion.clone().unwrap();
        let records = vec![read, write];
        let mut frontier = VerificationFrontier::default();
        frontier.advance(Some("/app"), &[], &records).unwrap();
        assert!(
            !frontier
                .task_resolution_workspace_evidence_is_current(
                    Some("/app"),
                    &[],
                    &records,
                    std::slice::from_ref(&reference)
                )
                .unwrap()
        );
        // Both policy scope and frontier ordinals must survive real recovery.
        frontier = restore_from_ledger(&frontier, &records, &ledger);
        let mut policy = crate::turn::runtime_policy::RuntimePolicyEvaluationState::default();
        crate::turn::runtime_policy::evaluate_tool_boundary(
            &mut policy,
            astra_turn_core::context_feedback::RuntimePolicySubject::Run,
            &records,
            2,
        )
        .unwrap();
        let policy =
            crate::turn::runtime_policy::RuntimePolicyEvaluationState::deserialize_continuation(
                policy
                    .serialize_continuation(serde_json::value::Serializer)
                    .unwrap(),
            )
            .unwrap();
        let workspace_refs = policy.task_resolution_workspace_evidence(&["read".into()]);
        assert_eq!(workspace_refs, vec![reference]);
        assert!(
            !frontier
                .task_resolution_workspace_evidence_is_current(
                    Some("/app"),
                    &[],
                    &[],
                    &workspace_refs
                )
                .unwrap()
        );
        assert!(
            frontier
                .task_resolution_workspace_evidence_is_current(Some("/app"), &[], &[], &[])
                .unwrap(),
            "external evidence does not acquire an implicit workspace obligation"
        );
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
        let terminal = ledger
            .compare_and_complete(
                &identity,
                ToolInvocationState::Dispatched,
                Some("owner"),
                ToolInvocationTerminalOutcome::Succeeded {
                    result: ToolInvocationResultPayload {
                        output: "ok".into(),
                        metadata: Default::default(),
                        exit_semantics: None,
                    },
                },
            )
            .unwrap();
        ToolCallRecord {
            execution_completion: Some(
                ToolInvocationCompletionRef::from_record(&terminal)
                    .unwrap()
                    .into(),
            ),
            runtime_args_full: Some(args.to_string()),
            ..record(name, true, "")
        }
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
        let mut changed_path = snapshot.clone();
        let mut has_path = false;
        for source in changed_path.workspace.latest_source.iter_mut().chain(
            changed_path
                .workspace
                .proof
                .iter_mut()
                .filter_map(|proof| proof.literal_source.as_mut()),
        ) {
            if source.delivered_path.is_some() {
                source.delivered_path = Some("/app/dir/../solution.py".into());
                has_path = true;
            }
        }
        if has_path {
            changed_path.validate().unwrap();
            assert!(matches!(
                VerificationFrontier::restore(
                    Some("/app"),
                    &changed_path,
                    "user",
                    "session",
                    "run",
                    "chain",
                    &[],
                    &rows
                ),
                Err(VerificationRecoveryError::ScopeMismatch)
            ));
        }
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
    fn workspace_literal_proof_keeps_exact_delivery_ancestry_across_recovery() {
        let mut ledger = astra_turn_core::invocation_ledger::InMemoryInvocationLedger::default();
        let mut delivery = executed_with_args(
            &mut ledger,
            "delivery",
            "write_file",
            serde_json::json!({"path":"/app/solution.py"}),
        );
        let fields = astra_tools::workspace_observation::typed_workspace_tool_receipt();
        delivery.workspace_mutation_observed = Some(true);
        delivery.workspace_mutation_scope =
            Some(astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into());
        delivery.workspace_mutation_receipt = fields
            .get(astra_tools::workspace_observation::RECEIPT_FIELD)
            .cloned();
        let mut original = VerificationFrontier::default();
        original
            .advance(Some("/app"), &[], &[delivery.clone()])
            .unwrap();
        let mut restored = restore_from_ledger(&original, &[delivery], &ledger);
        let script = executed_with_args(
            &mut ledger,
            "script",
            "bash",
            serde_json::json!({"command":"cd /app && python3 solution.py"}),
        );
        let mut suffix = vec![script];
        restored.advance(Some("/app"), &[], &suffix).unwrap();
        assert!(
            restored
                .evaluate_workspace_observation(Some("/app"), &[], &suffix)
                .unwrap()
        );
        assert_eq!(
            restored
                .observation
                .proof
                .as_ref()
                .unwrap()
                .literal_source
                .as_ref()
                .unwrap()
                .evidence
                .invocation
                .as_ref()
                .unwrap()
                .identity
                .invocation_id,
            "delivery"
        );
        let mut twice = restore_from_ledger(&restored, &suffix, &ledger);
        assert!(
            twice
                .evaluate_workspace_observation(Some("/app"), &[], &[])
                .unwrap()
        );

        let mut weak = executed_with_args(
            &mut ledger,
            "weak-writer",
            "bash",
            serde_json::json!({"command":"opaque-writer"}),
        );
        let fields = astra_tools::workspace_observation::changed_receipt_with_ownership(
            astra_tools::workspace_observation::FOREGROUND_PROCESS_GROUP_OWNERSHIP,
        );
        weak.workspace_mutation_observed = Some(true);
        weak.workspace_mutation_scope =
            Some(astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into());
        weak.workspace_mutation_receipt = fields
            .get(astra_tools::workspace_observation::RECEIPT_FIELD)
            .cloned();
        suffix = vec![weak];
        twice.advance(Some("/app"), &[], &suffix).unwrap();
        let mut after_weak = restore_from_ledger(&twice, &suffix, &ledger);
        assert!(
            after_weak
                .observation
                .latest_source
                .as_ref()
                .unwrap()
                .delivered_path
                .is_none()
        );
        let old_script = vec![executed_with_args(
            &mut ledger,
            "old-script",
            "bash",
            serde_json::json!({"command":"cd /app && python3 solution.py"}),
        )];
        after_weak.advance(Some("/app"), &[], &old_script).unwrap();
        assert!(
            !after_weak
                .evaluate_workspace_observation(Some("/app"), &[], &old_script)
                .unwrap()
        );
    }

    #[test]
    fn workspace_evidence_roundtrip_preserves_barrier_and_weak_source_without_upgrading_it() {
        for weak in [false, true] {
            let mut ledger =
                astra_turn_core::invocation_ledger::InMemoryInvocationLedger::default();
            let mut writer = if weak {
                executed_with_args(
                    &mut ledger,
                    "writer",
                    "bash",
                    serde_json::json!({"command":"opaque-writer"}),
                )
            } else {
                executed_with_args(
                    &mut ledger,
                    "writer",
                    "write_file",
                    serde_json::json!({"path":"/app/solution.py"}),
                )
            };
            if weak {
                let fields = astra_tools::workspace_observation::changed_receipt_with_ownership(
                    astra_tools::workspace_observation::FOREGROUND_PROCESS_GROUP_OWNERSHIP,
                );
                writer.workspace_mutation_observed = Some(true);
                writer.workspace_mutation_scope =
                    Some(astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into());
                writer.workspace_mutation_receipt = fields
                    .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                    .cloned();
                assert!(
                    super::super::execution_phase::record_has_weak_workspace_mutation_receipt(
                        &writer
                    )
                );
                assert!(
                    !super::super::execution_phase::record_has_trusted_workspace_mutation_receipt(
                        &writer
                    )
                );
            }
            let mut history = vec![writer.clone()];
            let mut original = VerificationFrontier::default();
            original.advance(Some("/app"), &[], &history).unwrap();
            let mut restored = restore_from_ledger(&original, &history, &ledger);
            assert!(
                !restored
                    .evaluate_workspace_observation(Some("/app"), &[], &[])
                    .unwrap()
            );
            assert!(
                restored
                    .observation
                    .latest_source
                    .as_ref()
                    .unwrap()
                    .delivered_path
                    .is_none()
            );
            let suffix = vec![executed_with_args(
                &mut ledger,
                "read",
                "read_file",
                serde_json::json!({"path":"/app/solution.py"}),
            )];
            restored.advance(Some("/app"), &[], &suffix).unwrap();
            history.extend(suffix.clone());
            original.advance(Some("/app"), &[], &history).unwrap();
            assert!(
                restored
                    .evaluate_workspace_observation(Some("/app"), &[], &suffix)
                    .unwrap()
            );
            assert_eq!(
                restored.export(Some("/app"), &[], &suffix, Some("chain")),
                original.export(Some("/app"), &[], &history, Some("chain"))
            );
            let twice = restore_from_ledger(&restored, &suffix, &ledger);
            assert!(
                twice
                    .evaluate_workspace_observation(Some("/app"), &[], &[])
                    .unwrap()
            );
            if weak {
                assert!(
                    !super::super::execution_phase::record_has_trusted_workspace_mutation_receipt(
                        &writer
                    )
                );
            }
        }
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

    /// A successfully restored prefix followed by loss of one local suffix
    /// record. Exercise unavailable history without inventing a missing-state
    /// variant that strict snapshot decoding no longer permits.
    pub(crate) fn restored_prefix_with_missing_history(
        retained: &[ToolCallRecord],
    ) -> VerificationFrontier {
        let mut frontier = restored_read_only_prefix();
        let mut ledger = astra_turn_core::invocation_ledger::InMemoryInvocationLedger::default();
        let mut suffix = retained.to_vec();
        suffix.push(executed(&mut ledger, "lost-read", "read_file", ""));
        frontier.advance(None, &[], &suffix).unwrap();
        frontier
    }

    #[test]
    fn workspace_observation_root_change_rebuilds_only_complete_history() {
        let records = vec![ToolCallRecord {
            name: "write_file".into(),
            ok: true,
            disposition: Some(ToolCallDisposition::Executed),
            runtime_args_full: Some(serde_json::json!({"path":"/tmp/owned/file"}).to_string()),
            ..Default::default()
        }];
        let mut frontier = VerificationFrontier::default();
        frontier.advance(Some("/app"), &[], &records).unwrap();
        assert!(
            frontier
                .evaluate_workspace_observation(Some("/app"), &[], &records)
                .unwrap()
        );
        assert!(
            !frontier
                .evaluate_workspace_observation(Some("/tmp/owned"), &[], &records)
                .unwrap()
        );
        // A read-only view does not mutate the source cursor or root binding.
        assert_eq!(frontier.cursor, 1);
        assert_eq!(frontier.workspace_root.as_deref(), Some("/app"));
        frontier.advance(Some("/tmp/owned"), &[], &records).unwrap();
        assert!(
            !frontier
                .evaluate_workspace_observation(Some("/tmp/owned"), &[], &records)
                .unwrap()
        );
        assert_eq!(frontier.processed_through, 1);

        let restored = restored_prefix_with_missing_history(&[]);
        assert_eq!(
            restored.evaluate_workspace_observation(None, &[], &[]),
            Err(VerificationRecoveryError::HistoryUnavailable)
        );
        assert_eq!(
            restored.evaluate_workspace_observation(Some("/app"), &[], &[]),
            Err(VerificationRecoveryError::HistoryUnavailable)
        );
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
        uninterrupted.advance(None, &hooks, &history).unwrap();
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

    // Independent full-history oracle: this is the previous production query.
    fn full_scan(hooks: &[StopHook], records: &[ToolCallRecord]) -> Option<Vec<String>> {
        let hooks: Vec<_> = hooks.iter().filter(|hook| hook.authoritative).collect();
        if hooks.is_empty() {
            return None;
        }
        let barrier = records
            .iter()
            .enumerate()
            .rev()
            .find(|(_, record)| {
                record.was_executed()
                    && crate::turn::tool_side_effects::tool_call_may_mutate_workspace(
                        &record.name,
                        super::super::lifecycle::extract_tool_args(
                            record.authoritative_args_full(),
                        )
                        .as_ref(),
                    )
                    && !hooks.iter().any(|hook| {
                        super::super::execution_phase::record_verifies_explicit_hook(record, hook)
                    })
            })
            .map(|(index, _)| index)?;
        Some(
            hooks
                .iter()
                .filter(|hook| {
                    !records.iter().enumerate().any(|(index, record)| {
                        index > barrier
                            && super::super::execution_phase::record_verifies_explicit_hook(
                                record, hook,
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
    fn changed_hook_reclassifies_previous_verifier_as_mutation() {
        // A successful shell verifier may itself have written build artifacts.
        let records = vec![record("bash", true, "./a")];
        let mut frontier = VerificationFrontier::default();
        frontier.advance(None, &[hook("./a")], &records).unwrap();
        assert_eq!(frontier.missing(), None);
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
        let records = vec![record("read_file", true, ""), record("bash", true, "./a")];
        assert!(
            matches!(
                frontier.export(None, &hooks, &records, Some("chain")),
                VerificationHandoff::Bound { .. }
            ),
            "no mutation means no verification debt"
        );
        let mut records = records;
        records.push(record("write_file", false, ""));
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
                    detail: VerificationUnavailable::UnboundWorkspaceEvidence
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
        records.push(record("write_file", false, ""));
        frontier.advance(None, &hooks, &records).unwrap();
        assert_eq!(frontier.missing(), Some(vec!["same label".into(); 3]));
        assert_eq!(frontier.missing(), full_scan(&hooks, &records));
    }
}

/// Retains at most one mutation and one proof per authoritative hook.
/// The vector cursor is process-local; the ordinal belongs to execution order.
#[derive(Clone)]
pub(crate) struct VerificationFrontier {
    contract: Vec<StopHook>,
    workspace_root: Option<String>,
    observation: WorkspaceObservationFacts,
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
            observation: WorkspaceObservationFacts::default(),
            cursor: 0,
            processed_through: 0,
            mutation: None,
            proofs: Vec::new(),
            historical_prefix: false,
        }
    }
}

impl VerificationFrontier {
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
        for source in snapshot.workspace.latest_source.iter().chain(
            snapshot
                .workspace
                .proof
                .iter()
                .filter_map(|proof| proof.literal_source.as_ref()),
        ) {
            if let Some(path) = &source.delivered_path {
                if super::lifecycle::normalize_workspace_path(path, workspace_root).as_deref()
                    != Some(std::path::Path::new(path))
                {
                    return Err(VerificationRecoveryError::ScopeMismatch);
                }
            }
        }
        Ok(Self {
            contract: snapshot.contract.clone(),
            workspace_root: snapshot.workspace_root.clone(),
            observation: WorkspaceObservationFacts::restore(&snapshot.workspace),
            cursor: 0,
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
        let Some(workspace) = view.observation.bind() else {
            return unavailable(VerificationUnavailable::UnboundWorkspaceEvidence);
        };
        let snapshot = BoundVerificationFrontier {
            canonical_turn_chain_id: chain.into(),
            contract: view.contract,
            processed_through: view.processed_through,
            mutation,
            proofs,
            workspace_root: view.workspace_root,
            workspace,
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

    pub(crate) fn evaluate_workspace_observation(
        &self,
        workspace_root: Option<&str>,
        hooks: &[StopHook],
        records: &[ToolCallRecord],
    ) -> Result<bool, VerificationRecoveryError> {
        if self.cursor == records.len()
            && self.workspace_root.as_deref() == workspace_root
            && self
                .contract
                .iter()
                .eq(hooks.iter().filter(|hook| hook.authoritative))
        {
            return Ok(self.observation.is_satisfied());
        }
        let mut view = self.clone();
        view.advance(workspace_root, hooks, records)?;
        Ok(view.observation.is_satisfied())
    }

    /// An assessment may interpret a different observer as satisfying a task,
    /// but may not reuse a workspace observation across a later known writer.
    /// External evidence has no implicit workspace-freshness requirement.
    pub(crate) fn task_resolution_workspace_evidence_is_current(
        &self,
        workspace_root: Option<&str>,
        hooks: &[StopHook],
        records: &[ToolCallRecord],
        workspace_evidence: &[astra_turn_types::task_resolution::ToolExecutionEvidenceRef],
    ) -> Result<bool, VerificationRecoveryError> {
        let mut view = self.clone();
        view.advance(workspace_root, hooks, records)?;
        let Some(barrier) = view.observation.barrier.as_ref() else {
            return Ok(true);
        };
        let base = view.processed_through.saturating_sub(records.len() as u64);
        for reference in workspace_evidence {
            let ordinal = records
                .iter()
                .position(|record| record.execution_completion.as_ref() == Some(reference))
                .map(|index| base + index as u64 + 1)
                .or_else(|| {
                    view.observation
                        .proof
                        .as_ref()
                        .map(|proof| &proof.evidence)
                        .into_iter()
                        .chain(std::iter::once(barrier))
                        .find(|evidence| {
                            reference.as_invocation().is_some_and(|reference| {
                                evidence.invocation.as_ref() == Some(reference)
                            })
                        })
                        .map(|evidence| evidence.ordinal)
                });
            if ordinal.is_none_or(|ordinal| ordinal < barrier.ordinal) {
                // A restored prefix must use retained bound ordinals, never
                // the absence of a local record as evidence of freshness.
                return Ok(false);
            }
        }
        Ok(true)
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
            // Changing a hook can turn its former successful verifier into a
            // mutation. Rebuild with the same reducer, not a guessed reset.
            *self = Self {
                proofs: vec![None; contract.len()],
                contract,
                workspace_root: workspace_root.map(str::to_owned),
                ..Self::default()
            };
        }
        for record in &records[self.cursor..] {
            self.processed_through += 1;
            if !record.was_executed() {
                continue;
            }
            let evidence = Evidence {
                ordinal: self.processed_through,
                invocation: record
                    .execution_completion
                    .as_ref()
                    .and_then(|reference| reference.as_invocation())
                    .cloned(),
            };
            let mut verified = false;
            for (hook, proof) in self.contract.iter().zip(&mut self.proofs) {
                if super::execution_phase::record_verifies_explicit_hook(record, hook) {
                    *proof = Some(evidence.clone());
                    verified = true;
                }
            }
            if !verified
                && crate::turn::tool_side_effects::tool_call_may_mutate_workspace(
                    &record.name,
                    super::lifecycle::extract_tool_args(record.authoritative_args_full()).as_ref(),
                )
            {
                self.mutation = Some(evidence);
                self.proofs.fill(None);
            }
            self.observation
                .observe(record, self.processed_through, verified, workspace_root);
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
        let mutation = self.mutation.as_ref()?;
        Some(
            self.contract
                .iter()
                .zip(&self.proofs)
                .filter(|(_, proof)| {
                    !proof
                        .as_ref()
                        .is_some_and(|proof| proof.ordinal > mutation.ordinal)
                })
                .map(|(hook, _)| hook.label.clone())
                .collect(),
        )
    }
}
