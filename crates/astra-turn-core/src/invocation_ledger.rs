//! Pure durable-invocation ledger state machine.
//!
//! Persistence adapters must provide the same prepare/compare-and-set
//! semantics. This in-memory implementation is the executable contract used
//! by unit tests and local runtimes; it is not a semantic result cache.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use astra_turn_types::{
    DispatchCertainty, ToolInvocationCompletionSource, ToolInvocationDecision,
    ToolInvocationDispatchLease, ToolInvocationFingerprint, ToolInvocationIdentity,
    ToolInvocationPrepareOutcome, ToolInvocationRecord, ToolInvocationResultPayload,
    ToolInvocationState, ToolInvocationTerminalOutcome,
};
use thiserror::Error;

#[derive(Default)]
pub struct InMemoryInvocationLedger {
    entries: BTreeMap<ToolInvocationIdentity, ToolInvocationRecord>,
    run_members: BTreeMap<(String, String), BTreeSet<ToolInvocationIdentity>>,
    terminal_order: VecDeque<ToolInvocationIdentity>,
}

impl InMemoryInvocationLedger {
    pub fn prepare(
        &mut self,
        identity: ToolInvocationIdentity,
        fingerprint: ToolInvocationFingerprint,
        decision: ToolInvocationDecision,
    ) -> Result<ToolInvocationPrepareOutcome, InvocationLedgerError> {
        if fingerprint.policy_decision_id != decision.decision_id {
            return Err(InvocationLedgerError::DecisionMismatch { identity });
        }
        if let Some(existing) = self.entries.get(&identity) {
            if !existing.fingerprint.same_tool_and_arguments(&fingerprint) {
                return Err(InvocationLedgerError::IdentityConflict { identity });
            }
            return Ok(ToolInvocationPrepareOutcome::Existing(existing.clone()));
        }

        let entry = ToolInvocationRecord {
            identity: identity.clone(),
            fingerprint,
            decision,
            state: ToolInvocationState::Prepared,
            dispatch_certainty: DispatchCertainty::NotDispatched,
            attempt_count: 0,
            dispatch_lease: None,
            outcome: None,
            completion_source: None,
        };
        self.run_members
            .entry((identity.user_id.clone(), identity.run_id.clone()))
            .or_default()
            .insert(identity.clone());
        self.entries.insert(identity, entry.clone());
        Ok(ToolInvocationPrepareOutcome::Prepared(entry))
    }

    /// Atomically claim the only transition that may cross the provider route
    /// boundary. The lease owner is thereafter required for renewal and
    /// completion.
    pub fn claim_dispatch(
        &mut self,
        identity: &ToolInvocationIdentity,
        lease: ToolInvocationDispatchLease,
    ) -> Result<ToolInvocationRecord, InvocationLedgerError> {
        let candidate = self.dispatch_claim_candidate(identity, lease)?;
        self.commit_dispatch_claim(candidate)
    }

    /// Validate one dispatch transition without mutating the authoritative
    /// map. Process-local run admission uses this single-record candidate so
    /// a failed control fence leaves neither a claimed invocation nor a
    /// stranded admission grant; cloning the full ledger would make every
    /// tool dispatch O(total process history).
    pub fn dispatch_claim_candidate(
        &self,
        identity: &ToolInvocationIdentity,
        lease: ToolInvocationDispatchLease,
    ) -> Result<ToolInvocationRecord, InvocationLedgerError> {
        let mut entry = self.entries.get(identity).cloned().ok_or_else(|| {
            InvocationLedgerError::MissingInvocation {
                identity: identity.clone(),
            }
        })?;
        if entry.state != ToolInvocationState::Prepared {
            return Err(InvocationLedgerError::StateMismatch {
                identity: identity.clone(),
                expected: ToolInvocationState::Prepared,
                actual: entry.state,
            });
        }
        entry.state = ToolInvocationState::Dispatched;
        entry.dispatch_certainty = DispatchCertainty::Dispatched;
        entry.attempt_count = entry.attempt_count.saturating_add(1);
        entry.dispatch_lease = Some(lease);
        Ok(entry)
    }

    /// Publish a candidate produced by [`Self::dispatch_claim_candidate`].
    /// The caller must serialize this with the candidate read. Revalidation
    /// remains fail-closed so accidental unlocked use cannot overwrite a
    /// concurrent terminal or dispatch owner.
    pub fn commit_dispatch_claim(
        &mut self,
        candidate: ToolInvocationRecord,
    ) -> Result<ToolInvocationRecord, InvocationLedgerError> {
        let identity = candidate.identity.clone();
        let current = self.entries.get(&identity).ok_or_else(|| {
            InvocationLedgerError::MissingInvocation {
                identity: identity.clone(),
            }
        })?;
        if current.state != ToolInvocationState::Prepared {
            return Err(InvocationLedgerError::StateMismatch {
                identity,
                expected: ToolInvocationState::Prepared,
                actual: current.state,
            });
        }
        if candidate.state != ToolInvocationState::Dispatched
            || !current
                .fingerprint
                .same_tool_and_arguments(&candidate.fingerprint)
            || current.decision != candidate.decision
        {
            return Err(InvocationLedgerError::IdentityConflict { identity });
        }
        self.entries.insert(identity, candidate.clone());
        Ok(candidate)
    }

    pub fn renew_dispatch(
        &mut self,
        identity: &ToolInvocationIdentity,
        lease: ToolInvocationDispatchLease,
    ) -> Result<ToolInvocationRecord, InvocationLedgerError> {
        let entry = self.dispatched_entry_for_owner(identity, &lease.owner_id)?;
        let current = entry
            .dispatch_lease
            .as_ref()
            .expect("dispatched entries must have a validated lease");
        if lease.expires_at_epoch_ms <= current.expires_at_epoch_ms {
            return Err(InvocationLedgerError::LeaseNotExtended {
                identity: Box::new(identity.clone()),
                current_expiry_epoch_ms: current.expires_at_epoch_ms,
                proposed_expiry_epoch_ms: lease.expires_at_epoch_ms,
            });
        }
        entry.dispatch_lease = Some(lease);
        Ok(entry.clone())
    }

    /// Convert an abandoned dispatch to uncertainty only after its owner lease
    /// has expired. An active lease is returned unchanged.
    pub fn reconcile_expired_dispatch(
        &mut self,
        identity: &ToolInvocationIdentity,
        now_epoch_ms: u64,
    ) -> Result<ToolInvocationRecord, InvocationLedgerError> {
        let entry = self.entries.get_mut(identity).ok_or_else(|| {
            InvocationLedgerError::MissingInvocation {
                identity: identity.clone(),
            }
        })?;
        if entry.state != ToolInvocationState::Dispatched {
            return Err(InvocationLedgerError::StateMismatch {
                identity: identity.clone(),
                expected: ToolInvocationState::Dispatched,
                actual: entry.state,
            });
        }
        let lease = entry.dispatch_lease.as_ref().ok_or_else(|| {
            InvocationLedgerError::MissingDispatchLease {
                identity: identity.clone(),
            }
        })?;
        if !lease.is_expired_at(now_epoch_ms) {
            return Ok(entry.clone());
        }
        entry.state = ToolInvocationState::OutcomeUnknown;
        entry.dispatch_certainty = DispatchCertainty::Unknown;
        let completed = entry.clone();
        self.terminal_order.push_back(identity.clone());
        Ok(completed)
    }

    pub fn mark_outcome_unknown(
        &mut self,
        identity: &ToolInvocationIdentity,
        owner_id: &str,
    ) -> Result<ToolInvocationRecord, InvocationLedgerError> {
        let entry = self.dispatched_entry_for_owner(identity, owner_id)?;
        entry.state = ToolInvocationState::OutcomeUnknown;
        entry.dispatch_certainty = DispatchCertainty::Unknown;
        let completed = entry.clone();
        self.terminal_order.push_back(identity.clone());
        Ok(completed)
    }

    pub fn compare_and_complete(
        &mut self,
        identity: &ToolInvocationIdentity,
        expected: ToolInvocationState,
        owner_id: Option<&str>,
        outcome: ToolInvocationTerminalOutcome,
    ) -> Result<ToolInvocationRecord, InvocationLedgerError> {
        outcome.validate()?;
        let next = outcome.state();
        let entry = self.entries.get_mut(identity).ok_or_else(|| {
            InvocationLedgerError::MissingInvocation {
                identity: identity.clone(),
            }
        })?;
        if entry.state != expected {
            return Err(InvocationLedgerError::StateMismatch {
                identity: identity.clone(),
                expected,
                actual: entry.state,
            });
        }
        if expected == ToolInvocationState::Dispatched {
            let owner_id =
                owner_id.ok_or_else(|| InvocationLedgerError::DispatchOwnerRequired {
                    identity: identity.clone(),
                })?;
            ensure_owner(identity, entry, owner_id)?;
        }
        if !expected.can_transition_to(next) {
            return Err(InvocationLedgerError::IllegalTransition {
                identity: identity.clone(),
                from: expected,
                to: next,
            });
        }
        entry.state = next;
        entry.dispatch_certainty = DispatchCertainty::Dispatched;
        entry.outcome = Some(outcome);
        entry.completion_source = None;
        let completed = entry.clone();
        self.terminal_order.push_back(identity.clone());
        Ok(completed)
    }

    /// Atomically complete a prepared invocation from a trusted semantic read
    /// observation without claiming or crossing the provider route boundary.
    pub fn complete_from_semantic_read_cache(
        &mut self,
        identity: &ToolInvocationIdentity,
        result: ToolInvocationResultPayload,
        completion_source: ToolInvocationCompletionSource,
    ) -> Result<ToolInvocationRecord, InvocationLedgerError> {
        result.validate()?;
        completion_source.validate()?;
        let entry = self.entries.get_mut(identity).ok_or_else(|| {
            InvocationLedgerError::MissingInvocation {
                identity: identity.clone(),
            }
        })?;
        if entry.state != ToolInvocationState::Prepared {
            return Err(InvocationLedgerError::StateMismatch {
                identity: identity.clone(),
                expected: ToolInvocationState::Prepared,
                actual: entry.state,
            });
        }
        entry.state = ToolInvocationState::Succeeded;
        entry.dispatch_certainty = DispatchCertainty::NotDispatched;
        entry.outcome = Some(ToolInvocationTerminalOutcome::Succeeded { result });
        entry.completion_source = Some(completion_source);
        entry.validate()?;
        let completed = entry.clone();
        self.terminal_order.push_back(identity.clone());
        Ok(completed)
    }

    pub fn get(&self, identity: &ToolInvocationIdentity) -> Option<&ToolInvocationRecord> {
        self.entries.get(identity)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// A terminal run may retire prepared and completed records, but it must
    /// retain any invocation that crossed dispatch or whose outcome remains
    /// uncertain so late acknowledgements can still reconcile safely.
    pub fn has_unsettled_dispatch(&self) -> bool {
        self.entries.values().any(|record| {
            matches!(
                record.state,
                ToolInvocationState::Dispatched | ToolInvocationState::OutcomeUnknown
            )
        })
    }

    /// Return one terminal candidate for lifecycle-aware retirement. The
    /// runtime must prove the owning run is terminal or gone before removal;
    /// terminal tool results inside an active/paused run remain replay truth.
    pub fn take_oldest_terminal_candidate(&mut self) -> Option<ToolInvocationIdentity> {
        while let Some(identity) = self.terminal_order.pop_front() {
            if self
                .entries
                .get(&identity)
                .is_some_and(|record| record.state.is_terminal())
            {
                return Some(identity);
            }
        }
        None
    }

    pub fn defer_terminal_candidate(&mut self, identity: ToolInvocationIdentity) {
        if self
            .entries
            .get(&identity)
            .is_some_and(|record| record.state.is_terminal())
        {
            self.terminal_order.push_back(identity);
        }
    }

    pub fn remove_run(&mut self, user_id: &str, run_id: &str) -> usize {
        let Some(identities) = self
            .run_members
            .remove(&(user_id.to_string(), run_id.to_string()))
        else {
            return 0;
        };
        let removed = identities.len();
        for identity in identities {
            self.entries.remove(&identity);
        }
        // Stale terminal queue entries are discarded incrementally by
        // `take_oldest_terminal_candidate`; avoiding a full queue retain here
        // keeps lifecycle retirement proportional to this run's own history.
        removed
    }

    fn dispatched_entry_for_owner(
        &mut self,
        identity: &ToolInvocationIdentity,
        owner_id: &str,
    ) -> Result<&mut ToolInvocationRecord, InvocationLedgerError> {
        let entry = self.entries.get_mut(identity).ok_or_else(|| {
            InvocationLedgerError::MissingInvocation {
                identity: identity.clone(),
            }
        })?;
        if entry.state != ToolInvocationState::Dispatched {
            return Err(InvocationLedgerError::StateMismatch {
                identity: identity.clone(),
                expected: ToolInvocationState::Dispatched,
                actual: entry.state,
            });
        }
        ensure_owner(identity, entry, owner_id)?;
        Ok(entry)
    }
}

fn ensure_owner(
    identity: &ToolInvocationIdentity,
    entry: &ToolInvocationRecord,
    owner_id: &str,
) -> Result<(), InvocationLedgerError> {
    let lease = entry.dispatch_lease.as_ref().ok_or_else(|| {
        InvocationLedgerError::MissingDispatchLease {
            identity: identity.clone(),
        }
    })?;
    if lease.owner_id != owner_id {
        return Err(InvocationLedgerError::DispatchOwnerMismatch {
            identity: identity.clone(),
        });
    }
    Ok(())
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum InvocationLedgerError {
    #[error("invocation identity conflicts with its previously prepared fingerprint: {identity:?}")]
    IdentityConflict { identity: ToolInvocationIdentity },
    #[error("invocation does not exist: {identity:?}")]
    MissingInvocation { identity: ToolInvocationIdentity },
    #[error(
        "invocation state compare-and-set failed for {identity:?}: expected {expected:?}, actual {actual:?}"
    )]
    StateMismatch {
        identity: ToolInvocationIdentity,
        expected: ToolInvocationState,
        actual: ToolInvocationState,
    },
    #[error("illegal invocation state transition for {identity:?}: {from:?} -> {to:?}")]
    IllegalTransition {
        identity: ToolInvocationIdentity,
        from: ToolInvocationState,
        to: ToolInvocationState,
    },
    #[error("invocation fingerprint and durable decision disagree: {identity:?}")]
    DecisionMismatch { identity: ToolInvocationIdentity },
    #[error("dispatched invocation is missing its owner lease: {identity:?}")]
    MissingDispatchLease { identity: ToolInvocationIdentity },
    #[error("dispatch owner does not own invocation: {identity:?}")]
    DispatchOwnerMismatch { identity: ToolInvocationIdentity },
    #[error("dispatch owner is required to complete invocation: {identity:?}")]
    DispatchOwnerRequired { identity: ToolInvocationIdentity },
    #[error(
        "dispatch lease renewal must extend its deadline for {identity:?}: current {current_expiry_epoch_ms}, proposed {proposed_expiry_epoch_ms}"
    )]
    LeaseNotExtended {
        identity: Box<ToolInvocationIdentity>,
        current_expiry_epoch_ms: u64,
        proposed_expiry_epoch_ms: u64,
    },
    #[error(transparent)]
    Contract(#[from] astra_turn_types::ToolInvocationContractError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_turn_types::DurableToolReference;
    use astra_turn_types::ToolInvocationResultPayload;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn identity(invocation_id: &str) -> ToolInvocationIdentity {
        ToolInvocationIdentity::new("user", "session", "run", "turn", invocation_id).unwrap()
    }

    fn decision() -> ToolInvocationDecision {
        ToolInvocationDecision::new(&json!({"route": "test"})).unwrap()
    }

    fn named_decision(name: &str) -> ToolInvocationDecision {
        ToolInvocationDecision::new(&json!({"route": "test", "policy": name})).unwrap()
    }

    fn fingerprint(command: &str) -> ToolInvocationFingerprint {
        ToolInvocationFingerprint::new(
            DurableToolReference::built_in("bash", "registry-v1").unwrap(),
            &json!({"command": command}),
            decision().decision_id,
        )
        .unwrap()
    }

    fn success(output: &str) -> ToolInvocationTerminalOutcome {
        ToolInvocationTerminalOutcome::Succeeded {
            result: ToolInvocationResultPayload {
                output: output.to_string(),
                metadata: BTreeMap::new(),
                exit_semantics: None,
            },
        }
    }

    fn result(output: &str) -> ToolInvocationResultPayload {
        ToolInvocationResultPayload {
            output: output.to_string(),
            metadata: BTreeMap::new(),
            exit_semantics: None,
        }
    }

    fn cache_completion() -> ToolInvocationCompletionSource {
        ToolInvocationCompletionSource::semantic_read_cache(
            format!("sha256:{}", "a".repeat(64)),
            format!("sha256:{}", "b".repeat(64)),
        )
        .unwrap()
    }

    fn lease(owner_id: &str, expires_at_epoch_ms: u64) -> ToolInvocationDispatchLease {
        ToolInvocationDispatchLease::new(owner_id, expires_at_epoch_ms).unwrap()
    }

    #[test]
    fn distinct_invocation_ids_with_equal_arguments_prepare_independently() {
        let mut ledger = InMemoryInvocationLedger::default();
        let shared = fingerprint("deploy");

        assert!(matches!(
            ledger
                .prepare(identity("call-1"), shared.clone(), decision())
                .unwrap(),
            ToolInvocationPrepareOutcome::Prepared(_)
        ));
        assert!(matches!(
            ledger
                .prepare(identity("call-2"), shared, decision())
                .unwrap(),
            ToolInvocationPrepareOutcome::Prepared(_)
        ));
    }

    #[test]
    fn semantic_cache_completion_never_claims_provider_dispatch() {
        let mut ledger = InMemoryInvocationLedger::default();
        let identity = identity("call-cache");
        ledger
            .prepare(identity.clone(), fingerprint("read"), decision())
            .unwrap();

        let completed = ledger
            .complete_from_semantic_read_cache(
                &identity,
                result("cached observation"),
                cache_completion(),
            )
            .unwrap();

        assert_eq!(completed.state, ToolInvocationState::Succeeded);
        assert_eq!(
            completed.dispatch_certainty,
            DispatchCertainty::NotDispatched
        );
        assert_eq!(completed.attempt_count, 0);
        assert!(completed.dispatch_lease.is_none());
        assert!(completed.completion_source.is_some());
        assert_eq!(
            completed.outcome.unwrap().result().output,
            "cached observation"
        );
        assert!(matches!(
            ledger.claim_dispatch(&identity, lease("owner", 10)),
            Err(InvocationLedgerError::StateMismatch {
                expected: ToolInvocationState::Prepared,
                actual: ToolInvocationState::Succeeded,
                ..
            })
        ));
    }

    #[test]
    fn invalid_terminal_payload_is_rejected_before_ledger_state_changes() {
        let mut ledger = InMemoryInvocationLedger::default();
        let identity = identity("call-bounded-result");
        ledger
            .prepare(identity.clone(), fingerprint("read"), decision())
            .unwrap();
        ledger
            .claim_dispatch(&identity, lease("owner-a", 10_000))
            .unwrap();
        let oversized = ToolInvocationTerminalOutcome::Succeeded {
            result: ToolInvocationResultPayload {
                output: "x".repeat(astra_turn_types::TOOL_INVOCATION_RESULT_OUTPUT_MAX_BYTES + 1),
                metadata: BTreeMap::new(),
                exit_semantics: None,
            },
        };

        let error = ledger
            .compare_and_complete(
                &identity,
                ToolInvocationState::Dispatched,
                Some("owner-a"),
                oversized,
            )
            .unwrap_err();
        assert!(matches!(error, InvocationLedgerError::Contract(_)));
        assert_eq!(
            ledger.get(&identity).unwrap().state,
            ToolInvocationState::Dispatched
        );
        assert!(ledger.get(&identity).unwrap().outcome.is_none());
    }

    #[test]
    fn same_identity_replays_only_when_fingerprint_matches() {
        let mut ledger = InMemoryInvocationLedger::default();
        let identity = identity("call-1");
        let original = fingerprint("deploy");
        ledger
            .prepare(identity.clone(), original.clone(), decision())
            .unwrap();

        assert!(matches!(
            ledger
                .prepare(identity.clone(), original, decision())
                .unwrap(),
            ToolInvocationPrepareOutcome::Existing(_)
        ));
        assert!(matches!(
            ledger.prepare(identity, fingerprint("destroy"), decision()),
            Err(InvocationLedgerError::IdentityConflict { .. })
        ));
    }

    #[test]
    fn prepared_resume_keeps_original_decision_when_live_policy_changes() {
        let mut ledger = InMemoryInvocationLedger::default();
        let identity = identity("call-1");
        let args = json!({"command": "deploy"});
        let original_decision = named_decision("original");
        let original_fingerprint = ToolInvocationFingerprint::new(
            DurableToolReference::built_in("bash", "registry-v1").unwrap(),
            &args,
            &original_decision.decision_id,
        )
        .unwrap();
        ledger
            .prepare(
                identity.clone(),
                original_fingerprint,
                original_decision.clone(),
            )
            .unwrap();
        let changed_decision = named_decision("changed");
        let changed_fingerprint = ToolInvocationFingerprint::new(
            DurableToolReference::built_in("bash", "registry-v1").unwrap(),
            &args,
            &changed_decision.decision_id,
        )
        .unwrap();

        let resumed = ledger
            .prepare(identity, changed_fingerprint, changed_decision)
            .unwrap();
        assert!(matches!(
            resumed,
            ToolInvocationPrepareOutcome::Existing(record)
                if record.decision == original_decision
        ));
    }

    #[test]
    fn dispatch_is_compare_and_set_and_cannot_repeat() {
        let mut ledger = InMemoryInvocationLedger::default();
        let identity = identity("call-1");
        ledger
            .prepare(identity.clone(), fingerprint("deploy"), decision())
            .unwrap();

        let dispatched = ledger
            .claim_dispatch(&identity, lease("worker-1", 100))
            .unwrap();
        assert_eq!(dispatched.attempt_count, 1);
        assert_eq!(
            dispatched.dispatch_lease.as_ref().unwrap().owner_id,
            "worker-1"
        );
        assert!(matches!(
            ledger.claim_dispatch(&identity, lease("worker-2", 200)),
            Err(InvocationLedgerError::StateMismatch { .. })
        ));
    }

    #[test]
    fn ambiguous_dispatch_can_reconcile_but_never_redispatch() {
        let mut ledger = InMemoryInvocationLedger::default();
        let identity = identity("call-1");
        ledger
            .prepare(identity.clone(), fingerprint("deploy"), decision())
            .unwrap();
        ledger
            .claim_dispatch(&identity, lease("worker-1", 100))
            .unwrap();

        assert_eq!(
            ledger
                .reconcile_expired_dispatch(&identity, 99)
                .unwrap()
                .state,
            ToolInvocationState::Dispatched
        );
        let unknown = ledger.reconcile_expired_dispatch(&identity, 100).unwrap();
        assert_eq!(unknown.state, ToolInvocationState::OutcomeUnknown);

        assert!(matches!(
            ledger.claim_dispatch(&identity, lease("worker-2", 200)),
            Err(InvocationLedgerError::StateMismatch { .. })
        ));
        let reconciled = ledger
            .compare_and_complete(
                &identity,
                ToolInvocationState::OutcomeUnknown,
                None,
                success("deployed"),
            )
            .unwrap();
        assert_eq!(reconciled.state, ToolInvocationState::Succeeded);
        assert_eq!(reconciled.attempt_count, 1);
        assert_eq!(reconciled.outcome.unwrap().result().output, "deployed");
    }

    #[test]
    fn completion_requires_the_dispatch_owner() {
        let mut ledger = InMemoryInvocationLedger::default();
        let identity = identity("call-1");
        ledger
            .prepare(identity.clone(), fingerprint("deploy"), decision())
            .unwrap();
        ledger
            .claim_dispatch(&identity, lease("worker-1", 100))
            .unwrap();

        assert!(matches!(
            ledger.compare_and_complete(
                &identity,
                ToolInvocationState::Dispatched,
                Some("worker-2"),
                success("wrong owner"),
            ),
            Err(InvocationLedgerError::DispatchOwnerMismatch { .. })
        ));
        let completed = ledger
            .compare_and_complete(
                &identity,
                ToolInvocationState::Dispatched,
                Some("worker-1"),
                success("deployed"),
            )
            .unwrap();
        assert_eq!(completed.state, ToolInvocationState::Succeeded);
    }

    #[test]
    fn only_owner_can_extend_and_renewal_must_move_deadline_forward() {
        let mut ledger = InMemoryInvocationLedger::default();
        let identity = identity("call-1");
        ledger
            .prepare(identity.clone(), fingerprint("deploy"), decision())
            .unwrap();

        ledger
            .claim_dispatch(&identity, lease("worker-1", 100))
            .unwrap();

        assert!(matches!(
            ledger.renew_dispatch(&identity, lease("worker-2", 200)),
            Err(InvocationLedgerError::DispatchOwnerMismatch { .. })
        ));
        assert!(matches!(
            ledger.renew_dispatch(&identity, lease("worker-1", 100)),
            Err(InvocationLedgerError::LeaseNotExtended { .. })
        ));
        let renewed = ledger
            .renew_dispatch(&identity, lease("worker-1", 200))
            .unwrap();
        assert_eq!(renewed.dispatch_lease.unwrap().expires_at_epoch_ms, 200);
    }
}
