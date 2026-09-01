//! Request-scoped ownership of one canonical session execution lease.
//!
//! A fresh headless request does not know its Server-minted session identity
//! before the first accepted SSE frame.  This holder lets the stream bind that
//! identity exactly once, before edge work is flushed, while retaining the
//! lease through the caller's canonical local commit.

use astra_core::ErrorKind;
use astra_services::session_journal::{SessionExecutionLease, SessionExecutionLeaseError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RequestSessionLeaseFailure {
    pub(crate) message: String,
    pub(crate) kind: ErrorKind,
}

#[derive(Debug)]
struct BoundSessionExecutionLease {
    session_id: String,
    lease: SessionExecutionLease,
}

#[derive(Debug, Default)]
struct RequestSessionExecutionLeaseState {
    bound: Option<BoundSessionExecutionLease>,
    failure: Option<RequestSessionLeaseFailure>,
    retain_unsettled_owner_on_drop: bool,
}

static PROCESS_UNSETTLED_SESSION_OWNERS: std::sync::OnceLock<
    std::sync::Mutex<Vec<BoundSessionExecutionLease>>,
> = std::sync::OnceLock::new();

/// One request's fail-closed session execution authority.
///
/// The mutex is request-private. It is held during acquisition and canonical
/// commit so two callbacks for the same request cannot race identity or drop
/// the lease between terminal validation and persistence.
#[derive(Debug, Default)]
pub(crate) struct RequestSessionExecutionLease {
    state: std::sync::Mutex<RequestSessionExecutionLeaseState>,
}

impl RequestSessionExecutionLease {
    pub(crate) fn new(
        initial_session_id: Option<&str>,
    ) -> Result<std::sync::Arc<Self>, RequestSessionLeaseFailure> {
        let holder = std::sync::Arc::new(Self::default());
        if let Some(session_id) = initial_session_id {
            holder.bind(session_id)?;
        }
        Ok(holder)
    }

    /// Bind the first accepted canonical identity. Repeating the exact same
    /// identity is idempotent; any later identity is a terminal contract
    /// failure and never acquires a second lease.
    pub(crate) fn bind(&self, session_id: &str) -> Result<(), RequestSessionLeaseFailure> {
        let mut state = astra_core::sync_poison::recover_mutex_lock(&self.state);
        if let Some(failure) = state.failure.clone() {
            return Err(failure);
        }
        let session_id = match validated_session_id(session_id) {
            Ok(session_id) => session_id,
            Err(failure) => {
                state.failure = Some(failure.clone());
                return Err(failure);
            }
        };
        if let Some(bound) = state.bound.as_ref() {
            if bound.session_id == session_id {
                return Ok(());
            }
            let failure = RequestSessionLeaseFailure {
                message: format!(
                    "server session identity changed within one request: expected `{}`, received `{session_id}`",
                    bound.session_id
                ),
                kind: ErrorKind::ContractViolation,
            };
            state.failure = Some(failure.clone());
            return Err(failure);
        }

        match SessionExecutionLease::try_acquire(&session_id) {
            Ok(lease) => {
                state.bound = Some(BoundSessionExecutionLease { session_id, lease });
                Ok(())
            }
            Err(error) => {
                let failure = lease_acquisition_failure(error);
                state.failure = Some(failure.clone());
                Err(failure)
            }
        }
    }

    /// Admit edge work only after the request owns the canonical session.
    /// Calling this at the flush boundary also turns a missing identity into a
    /// durable terminal failure instead of leaving the Server waiting for a
    /// tool result that the client must not execute.
    pub(crate) fn admit_edge_work(&self) -> Result<(), RequestSessionLeaseFailure> {
        let mut state = astra_core::sync_poison::recover_mutex_lock(&self.state);
        if let Some(failure) = state.failure.clone() {
            return Err(failure);
        }
        if state.bound.is_some() {
            return Ok(());
        }
        let failure = RequestSessionLeaseFailure {
            message: "server attempted edge work before binding a canonical session identity"
                .to_string(),
            kind: ErrorKind::ContractViolation,
        };
        state.failure = Some(failure.clone());
        Err(failure)
    }

    /// Validate the terminal stream identity without manufacturing a fallback.
    pub(crate) fn validate_terminal_identity(
        &self,
        terminal_session_id: Option<&str>,
    ) -> Result<(), RequestSessionLeaseFailure> {
        let mut state = astra_core::sync_poison::recover_mutex_lock(&self.state);
        if let Some(failure) = state.failure.clone() {
            return Err(failure);
        }
        let Some(bound) = state.bound.as_ref() else {
            let failure = RequestSessionLeaseFailure {
                message: "server stream ended without binding a canonical session identity"
                    .to_string(),
                kind: ErrorKind::ContractViolation,
            };
            state.failure = Some(failure.clone());
            return Err(failure);
        };
        let terminal_session_id = match terminal_session_id {
            Some(session_id) => match validated_session_id(session_id) {
                Ok(session_id) => session_id,
                Err(failure) => {
                    state.failure = Some(failure.clone());
                    return Err(failure);
                }
            },
            None => {
                let failure = RequestSessionLeaseFailure {
                    message: "terminal stream result omitted its canonical session identity"
                        .to_string(),
                    kind: ErrorKind::ContractViolation,
                };
                state.failure = Some(failure.clone());
                return Err(failure);
            }
        };
        if terminal_session_id != bound.session_id {
            let failure = RequestSessionLeaseFailure {
                message: format!(
                    "terminal stream session identity `{terminal_session_id}` did not match bound identity `{}`",
                    bound.session_id
                ),
                kind: ErrorKind::ContractViolation,
            };
            state.failure = Some(failure.clone());
            return Err(failure);
        }
        Ok(())
    }

    pub(crate) fn failure(&self) -> Option<RequestSessionLeaseFailure> {
        astra_core::sync_poison::recover_mutex_lock(&self.state)
            .failure
            .clone()
    }

    /// Preserve local exclusion when remote cancellation did not positively
    /// settle. The OS releases this process-lifetime quarantine on process
    /// exit; within a long-lived CLI, no later request can mistake an unknown
    /// remote owner for a free canonical session.
    pub(crate) fn retain_unsettled_owner_until_process_exit(&self) {
        let mut state = astra_core::sync_poison::recover_mutex_lock(&self.state);
        if state.bound.is_some() {
            state.retain_unsettled_owner_on_drop = true;
        }
    }

    pub(crate) fn mark_remote_execution_settled(&self) {
        astra_core::sync_poison::recover_mutex_lock(&self.state).retain_unsettled_owner_on_drop =
            false;
    }

    /// Run canonical persistence while retaining the matching lease.
    pub(crate) fn with_matching_lease<T>(
        &self,
        session_id: Option<&str>,
        persist: impl FnOnce(&SessionExecutionLease) -> T,
    ) -> Result<T, RequestSessionLeaseFailure> {
        self.validate_terminal_identity(session_id)?;
        let state = astra_core::sync_poison::recover_mutex_lock(&self.state);
        if let Some(failure) = state.failure.clone() {
            return Err(failure);
        }
        let bound = state
            .bound
            .as_ref()
            .ok_or_else(|| RequestSessionLeaseFailure {
                message: "canonical persistence has no bound session execution lease".to_string(),
                kind: ErrorKind::ContractViolation,
            })?;
        Ok(persist(&bound.lease))
    }
}

impl Drop for RequestSessionExecutionLease {
    fn drop(&mut self) {
        let state = match self.state.get_mut() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !state.retain_unsettled_owner_on_drop {
            return;
        }
        let Some(bound) = state.bound.take() else {
            return;
        };
        let registry =
            PROCESS_UNSETTLED_SESSION_OWNERS.get_or_init(|| std::sync::Mutex::new(Vec::new()));
        astra_core::sync_poison::recover_mutex_lock(registry).push(bound);
    }
}

fn validated_session_id(session_id: &str) -> Result<String, RequestSessionLeaseFailure> {
    if session_id.is_empty() || session_id.trim() != session_id {
        return Err(RequestSessionLeaseFailure {
            message:
                "server session identity must be non-empty and contain no surrounding whitespace"
                    .to_string(),
            kind: ErrorKind::ContractViolation,
        });
    }
    Ok(session_id.to_string())
}

fn lease_acquisition_failure(error: SessionExecutionLeaseError) -> RequestSessionLeaseFailure {
    RequestSessionLeaseFailure {
        message: error.to_string(),
        kind: ErrorKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::RequestSessionExecutionLease;
    use astra_services::session_journal::SessionExecutionLease;

    fn session_id(label: &str) -> String {
        format!("request-lease-{label}-{}", uuid::Uuid::new_v4())
    }

    #[test]
    fn duplicate_same_identity_is_idempotent_and_retains_one_lease() {
        let session_id = session_id("duplicate");
        let holder = RequestSessionExecutionLease::new(None).unwrap();
        holder.bind(&session_id).unwrap();
        holder.bind(&session_id).unwrap();
        assert!(SessionExecutionLease::try_acquire(&session_id).is_err());
    }

    #[test]
    fn changed_identity_fails_without_acquiring_the_second_session() {
        let first = session_id("first");
        let second = session_id("second");
        let holder = RequestSessionExecutionLease::new(None).unwrap();
        holder.bind(&first).unwrap();
        let error = holder.bind(&second).unwrap_err();
        assert!(error.message.contains("changed within one request"));
        assert!(SessionExecutionLease::try_acquire(&first).is_err());
        assert!(SessionExecutionLease::try_acquire(&second).is_ok());
    }

    #[test]
    fn same_session_conflict_fails_closed_and_restart_can_reacquire_after_drop() {
        let session_id = session_id("conflict-restart");
        let first = RequestSessionExecutionLease::new(Some(&session_id)).unwrap();
        let conflict = RequestSessionExecutionLease::new(Some(&session_id)).unwrap_err();
        assert!(conflict.message.contains("already has an active execution"));
        drop(first);
        RequestSessionExecutionLease::new(Some(&session_id))
            .expect("a new process/request can acquire after the old owner exits");
    }

    #[test]
    fn missing_mismatched_and_pre_binding_edge_work_fail_closed() {
        let missing = RequestSessionExecutionLease::new(None).unwrap();
        assert!(missing.validate_terminal_identity(None).is_err());

        let before_tool = RequestSessionExecutionLease::new(None).unwrap();
        assert!(before_tool.admit_edge_work().is_err());
        assert!(before_tool.bind(&session_id("too-late")).is_err());

        let bound_id = session_id("bound");
        let mismatch_id = session_id("mismatch");
        let mismatched = RequestSessionExecutionLease::new(Some(&bound_id)).unwrap();
        assert!(
            mismatched
                .validate_terminal_identity(Some(&mismatch_id))
                .is_err()
        );
    }

    #[test]
    fn distinct_session_admission_scales_without_a_global_lock() {
        let started = std::time::Instant::now();
        let mut threads = Vec::with_capacity(256);
        for index in 0..256 {
            let session_id = session_id(&format!("concurrent-{index}"));
            threads.push(std::thread::spawn(move || {
                RequestSessionExecutionLease::new(Some(&session_id)).unwrap()
            }));
        }
        let holders = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        let elapsed = started.elapsed();
        assert_eq!(holders.len(), 256);
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "256 distinct session admissions took {elapsed:?}"
        );
    }
}
