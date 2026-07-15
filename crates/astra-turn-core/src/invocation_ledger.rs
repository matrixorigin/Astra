//! Pure durable-invocation ledger state machine.
//!
//! Persistence adapters must provide the same prepare/compare-and-set
//! semantics. This in-memory implementation is the executable contract used
//! by unit tests and local runtimes; it is not a semantic result cache.

use std::collections::BTreeMap;

use astra_turn_types::{
    DispatchCertainty, ToolInvocationFingerprint, ToolInvocationIdentity,
    ToolInvocationPrepareOutcome, ToolInvocationRecord, ToolInvocationState,
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
    ) -> Result<ToolInvocationPrepareOutcome, InvocationLedgerError> {
        if let Some(existing) = self.entries.get(&identity) {
            if existing.fingerprint != fingerprint {
                return Err(InvocationLedgerError::IdentityConflict { identity });
            }
            return Ok(ToolInvocationPrepareOutcome::Existing(existing.clone()));
        }

        let entry = ToolInvocationRecord {
            identity: identity.clone(),
            fingerprint,
            state: ToolInvocationState::Prepared,
            dispatch_certainty: DispatchCertainty::NotDispatched,
            attempt_count: 0,
        };
        self.entries.insert(identity, entry.clone());
        Ok(ToolInvocationPrepareOutcome::Prepared(entry))
    }

    /// Compare-and-set one legal state transition. Persistence adapters must
    /// make the expected-state check and update atomic.
    pub fn compare_and_transition(
        &mut self,
        identity: &ToolInvocationIdentity,
        expected: ToolInvocationState,
        next: ToolInvocationState,
        dispatch_certainty: DispatchCertainty,
    ) -> Result<ToolInvocationRecord, InvocationLedgerError> {
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
        if !expected.can_transition_to(next) {
            return Err(InvocationLedgerError::IllegalTransition {
                identity: identity.clone(),
                from: expected,
                to: next,
            });
        }
        let required_certainty = next.required_dispatch_certainty();
        if dispatch_certainty != required_certainty {
            return Err(InvocationLedgerError::CertaintyMismatch {
                state: next,
                expected: required_certainty,
                actual: dispatch_certainty,
            });
        }

        entry.state = next;
        entry.dispatch_certainty = dispatch_certainty;
        if expected == ToolInvocationState::Prepared && next == ToolInvocationState::Dispatched {
            entry.attempt_count = entry.attempt_count.saturating_add(1);
        }
        Ok(entry.clone())
    }

    pub fn get(&self, identity: &ToolInvocationIdentity) -> Option<&ToolInvocationRecord> {
        self.entries.get(identity)
    }
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
    #[error(
        "dispatch certainty {actual:?} is inconsistent with state {state:?}; expected {expected:?}"
    )]
    CertaintyMismatch {
        state: ToolInvocationState,
        expected: DispatchCertainty,
        actual: DispatchCertainty,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_turn_types::DurableToolReference;
    use serde_json::json;

    fn identity(invocation_id: &str) -> ToolInvocationIdentity {
        ToolInvocationIdentity::new("user", "session", "run", "turn", invocation_id).unwrap()
    }

    fn fingerprint(command: &str) -> ToolInvocationFingerprint {
        ToolInvocationFingerprint::new(
            DurableToolReference::built_in("bash", "registry-v1").unwrap(),
            &json!({"command": command}),
            "policy-v1",
        )
        .unwrap()
    }

    #[test]
    fn distinct_invocation_ids_with_equal_arguments_prepare_independently() {
        let mut ledger = InMemoryInvocationLedger::default();
        let shared = fingerprint("deploy");

        assert!(matches!(
            ledger.prepare(identity("call-1"), shared.clone()).unwrap(),
            ToolInvocationPrepareOutcome::Prepared(_)
        ));
        assert!(matches!(
            ledger.prepare(identity("call-2"), shared).unwrap(),
            ToolInvocationPrepareOutcome::Prepared(_)
        ));
    }

    #[test]
    fn same_identity_replays_only_when_fingerprint_matches() {
        let mut ledger = InMemoryInvocationLedger::default();
        let identity = identity("call-1");
        let original = fingerprint("deploy");
        ledger.prepare(identity.clone(), original.clone()).unwrap();

        assert!(matches!(
            ledger.prepare(identity.clone(), original).unwrap(),
            ToolInvocationPrepareOutcome::Existing(_)
        ));
        assert!(matches!(
            ledger.prepare(identity, fingerprint("destroy")),
            Err(InvocationLedgerError::IdentityConflict { .. })
        ));
    }

    #[test]
    fn dispatch_is_compare_and_set_and_cannot_repeat() {
        let mut ledger = InMemoryInvocationLedger::default();
        let identity = identity("call-1");
        ledger
            .prepare(identity.clone(), fingerprint("deploy"))
            .unwrap();

        let dispatched = ledger
            .compare_and_transition(
                &identity,
                ToolInvocationState::Prepared,
                ToolInvocationState::Dispatched,
                DispatchCertainty::Dispatched,
            )
            .unwrap();
        assert_eq!(dispatched.attempt_count, 1);
        assert!(matches!(
            ledger.compare_and_transition(
                &identity,
                ToolInvocationState::Prepared,
                ToolInvocationState::Dispatched,
                DispatchCertainty::Dispatched,
            ),
            Err(InvocationLedgerError::StateMismatch { .. })
        ));
    }

    #[test]
    fn ambiguous_dispatch_can_reconcile_but_never_redispatch() {
        let mut ledger = InMemoryInvocationLedger::default();
        let identity = identity("call-1");
        ledger
            .prepare(identity.clone(), fingerprint("deploy"))
            .unwrap();
        ledger
            .compare_and_transition(
                &identity,
                ToolInvocationState::Prepared,
                ToolInvocationState::Dispatched,
                DispatchCertainty::Dispatched,
            )
            .unwrap();
        ledger
            .compare_and_transition(
                &identity,
                ToolInvocationState::Dispatched,
                ToolInvocationState::OutcomeUnknown,
                DispatchCertainty::Unknown,
            )
            .unwrap();

        assert!(matches!(
            ledger.compare_and_transition(
                &identity,
                ToolInvocationState::OutcomeUnknown,
                ToolInvocationState::Dispatched,
                DispatchCertainty::Dispatched,
            ),
            Err(InvocationLedgerError::IllegalTransition { .. })
        ));
        let reconciled = ledger
            .compare_and_transition(
                &identity,
                ToolInvocationState::OutcomeUnknown,
                ToolInvocationState::Succeeded,
                DispatchCertainty::Dispatched,
            )
            .unwrap();
        assert_eq!(reconciled.state, ToolInvocationState::Succeeded);
        assert_eq!(reconciled.attempt_count, 1);
    }

    #[test]
    fn state_and_dispatch_certainty_cannot_disagree() {
        let mut ledger = InMemoryInvocationLedger::default();
        let identity = identity("call-1");
        ledger
            .prepare(identity.clone(), fingerprint("deploy"))
            .unwrap();

        assert!(matches!(
            ledger.compare_and_transition(
                &identity,
                ToolInvocationState::Prepared,
                ToolInvocationState::Dispatched,
                DispatchCertainty::Unknown,
            ),
            Err(InvocationLedgerError::CertaintyMismatch { .. })
        ));
        assert_eq!(
            ledger.get(&identity).unwrap().state,
            ToolInvocationState::Prepared
        );
    }
}
