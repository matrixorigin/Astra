//! Pure durable-invocation ledger state machine.
//!
//! Persistence adapters must provide the same prepare/compare-and-set
//! semantics. This in-memory implementation is the executable contract used
//! by unit tests and local runtimes; it is not a semantic result cache.

use std::collections::BTreeMap;

use astra_turn_types::{
    DispatchCertainty, ToolInvocationDecision, ToolInvocationDispatchLease,
    ToolInvocationFingerprint, ToolInvocationIdentity, ToolInvocationPrepareOutcome,
    ToolInvocationRecord, ToolInvocationState, ToolInvocationTerminalOutcome,
};
use thiserror::Error;

#[derive(Default)]
pub struct InMemoryInvocationLedger {
    entries: BTreeMap<ToolInvocationIdentity, ToolInvocationRecord>,
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
        };
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
        entry.state = ToolInvocationState::Dispatched;
        entry.dispatch_certainty = DispatchCertainty::Dispatched;
        entry.attempt_count = entry.attempt_count.saturating_add(1);
        entry.dispatch_lease = Some(lease);
        Ok(entry.clone())
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
        Ok(entry.clone())
    }

    pub fn mark_outcome_unknown(
        &mut self,
        identity: &ToolInvocationIdentity,
        owner_id: &str,
    ) -> Result<ToolInvocationRecord, InvocationLedgerError> {
        let entry = self.dispatched_entry_for_owner(identity, owner_id)?;
        entry.state = ToolInvocationState::OutcomeUnknown;
        entry.dispatch_certainty = DispatchCertainty::Unknown;
        Ok(entry.clone())
    }

    pub fn compare_and_complete(
        &mut self,
        identity: &ToolInvocationIdentity,
        expected: ToolInvocationState,
        owner_id: Option<&str>,
        outcome: ToolInvocationTerminalOutcome,
    ) -> Result<ToolInvocationRecord, InvocationLedgerError> {
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
        Ok(entry.clone())
    }

    pub fn get(&self, identity: &ToolInvocationIdentity) -> Option<&ToolInvocationRecord> {
        self.entries.get(identity)
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
