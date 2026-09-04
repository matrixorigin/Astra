//! Runtime boundary between admitted model execution and the durable inference ledger.
//!
//! Callers own semantic purpose and causal scope. This module owns the invariant
//! that route/invocation admission happens before provider I/O, every physical
//! request is observed, and the logical terminal matches the provider terminal.
//! Planning is not itself a durable ledger event: a process that exits before
//! admitting its first provider attempt or terminal settlement leaves no
//! invocation row, because it could not have caused provider I/O. The ledger
//! records provider delivery and terminal outcomes, not abandoned local intent.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use serde_json::json;

use crate::turn::llm::client::{
    LlmCall, LlmCallResult, LlmCancel, ProviderAttemptObserver, ProviderWireRequestIdentity,
};
use astra_core::SharedPool;

pub(crate) const INFERENCE_LEDGER_ERROR_SOURCE: &str = "inference_execution_ledger";

#[derive(Clone)]
pub(crate) struct DurableInferenceRunAuthority {
    durable: astra_services::InferenceRunAdmissionAuthority,
    cancel_flag: Option<Arc<AtomicBool>>,
    cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    execution_lease_lost: Option<Arc<AtomicBool>>,
}

impl DurableInferenceRunAuthority {
    pub(crate) fn new(
        expected_owner_generation: u64,
        expected_owner_pod_id: impl Into<String>,
        expected_control_epoch: i64,
        cancel_flag: Option<Arc<AtomicBool>>,
        cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
        execution_lease_lost: Option<Arc<AtomicBool>>,
    ) -> Self {
        Self {
            durable: astra_services::InferenceRunAdmissionAuthority {
                expected_owner_generation,
                expected_owner_pod_id: expected_owner_pod_id.into(),
                expected_control_epoch,
            },
            cancel_flag,
            cancel_token,
            execution_lease_lost,
        }
    }

    fn local_fence_error(&self, stage: &'static str) -> Option<astra_core::ClassifiedError> {
        if self
            .execution_lease_lost
            .as_ref()
            .is_some_and(|lost| lost.load(Ordering::Acquire))
        {
            return Some(
                astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::ContractViolation,
                    format!("durable inference authority was lost during {stage}"),
                )
                .with_details_json(
                    json!({
                        "source": INFERENCE_LEDGER_ERROR_SOURCE,
                        "stage": stage,
                        "authority": "execution_lease_lost",
                    })
                    .to_string(),
                ),
            );
        }
        if self
            .cancel_flag
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
            || self
                .cancel_token
                .as_ref()
                .is_some_and(|token| token.is_cancelled())
        {
            return Some(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::Cancelled,
                format!("LLM call cancelled during {stage}"),
            ));
        }
        None
    }

    async fn wait_for_local_fence(&self, stage: &'static str) -> astra_core::ClassifiedError {
        loop {
            if let Some(error) = self.local_fence_error(stage) {
                return error;
            }
            match self.cancel_token.as_ref() {
                Some(token) => {
                    tokio::select! {
                        _ = token.cancelled() => {}
                        _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
                    }
                }
                None => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
            }
        }
    }
}

pub(crate) struct DurableInferenceCallOutcome {
    logical_attempt: u32,
    result: Result<LlmCallResult, astra_core::ClassifiedError>,
}

impl DurableInferenceCallOutcome {
    #[must_use]
    pub(crate) fn logical_attempt(&self) -> u32 {
        self.logical_attempt
    }

    pub(crate) fn into_result(self) -> Result<LlmCallResult, astra_core::ClassifiedError> {
        self.result
    }

    #[must_use]
    pub(crate) fn admission_identity_is_occupied(&self) -> bool {
        let Some(error) = self.result.as_ref().err() else {
            return false;
        };
        let Some(details) = error
            .details_json
            .as_deref()
            .and_then(|details| serde_json::from_str::<serde_json::Value>(details).ok())
        else {
            return false;
        };
        details.get("source").and_then(serde_json::Value::as_str)
            == Some(INFERENCE_LEDGER_ERROR_SOURCE)
            && details.get("stage").and_then(serde_json::Value::as_str) == Some("admission")
            && details
                .get("service_error_kind")
                .and_then(serde_json::Value::as_str)
                == Some("conflict")
    }
}

#[derive(Debug)]
pub(crate) struct DurableInferenceAdmissionFailure {
    pub(crate) logical_attempt: u32,
    pub(crate) error: astra_core::ClassifiedError,
}

fn detached_reconciliation_timeout() -> std::time::Duration {
    #[cfg(not(test))]
    {
        std::time::Duration::from_secs(10)
    }
    #[cfg(test)]
    {
        if std::env::var_os("ASTRA_TEST_DB_IT").is_some() {
            std::time::Duration::from_secs(10)
        } else {
            std::time::Duration::from_millis(50)
        }
    }
}
const MAX_FOREGROUND_ADMISSION_RECOVERIES: u32 = 1;
#[cfg(not(test))]
const DEFAULT_PROVIDER_SETTLEMENT_CAPACITY: usize = 256;
#[cfg(test)]
const DEFAULT_PROVIDER_SETTLEMENT_CAPACITY: usize = 16;
#[cfg(not(test))]
const DEFAULT_PROVIDER_SETTLEMENT_WAIT_CAPACITY: usize = 2_048;
#[cfg(test)]
const DEFAULT_PROVIDER_SETTLEMENT_WAIT_CAPACITY: usize = 128;
#[cfg(not(test))]
const DEFAULT_PROVIDER_SETTLEMENT_WORKERS: usize = 4;
#[cfg(test)]
const DEFAULT_PROVIDER_SETTLEMENT_WORKERS: usize = 2;
#[cfg(not(test))]
const ENV_PROVIDER_SETTLEMENT_CAPACITY: &str = "ASTRA_PROVIDER_SETTLEMENT_CAPACITY";
#[cfg(not(test))]
const ENV_PROVIDER_SETTLEMENT_WAIT_CAPACITY: &str = "ASTRA_PROVIDER_SETTLEMENT_WAIT_CAPACITY";
#[cfg(not(test))]
const ENV_PROVIDER_SETTLEMENT_WORKERS: &str = "ASTRA_PROVIDER_SETTLEMENT_WORKERS";
#[cfg(not(test))]
const ENV_PROVIDER_SETTLEMENT_MAX_ACTIVE_PER_USER: &str =
    "ASTRA_PROVIDER_SETTLEMENT_MAX_ACTIVE_PER_USER";
#[cfg(not(test))]
const ENV_PROVIDER_SETTLEMENT_MAX_ACTIVE_PER_SESSION: &str =
    "ASTRA_PROVIDER_SETTLEMENT_MAX_ACTIVE_PER_SESSION";
#[cfg(not(test))]
static PROVIDER_SETTLEMENT_COORDINATOR: std::sync::OnceLock<Arc<ProviderSettlementCoordinator>> =
    std::sync::OnceLock::new();
static PROVIDER_SETTLEMENT_WORKER_RUNTIME: std::sync::OnceLock<
    Result<tokio::runtime::Runtime, String>,
> = std::sync::OnceLock::new();
#[async_trait]
pub(crate) trait InferenceLedgerPersistence: Send + Sync {
    #[cfg(not(test))]
    async fn next_logical_attempt_pair_base(
        &self,
        input: &astra_services::InferenceInvocationInput,
    ) -> astra_services::ServiceResult<u32>;

    #[cfg(test)]
    async fn next_logical_attempt_pair_base(
        &self,
        input: &astra_services::InferenceInvocationInput,
    ) -> astra_services::ServiceResult<u32> {
        Ok(input.scope.logical_attempt() & !1)
    }

    async fn admit_invocation(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
    ) -> astra_services::ServiceResult<()>;

    #[cfg(not(test))]
    async fn renew_invocation_owner(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
    ) -> astra_services::ServiceResult<()>;

    #[cfg(test)]
    async fn renew_invocation_owner(
        &self,
        _plan: &astra_services::InferenceInvocationPlan,
    ) -> astra_services::ServiceResult<()> {
        Ok(())
    }

    async fn settle_uncertain_admission(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> astra_services::ServiceResult<astra_services::InferenceInvocationAdmissionResolution>;

    async fn declare_settlement(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> astra_services::ServiceResult<()>;

    async fn declare_attempt_settlement(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
        attempt: &astra_services::InferenceProviderAttemptPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
        provider_delivery_state: astra_services::InferenceProviderDeliveryState,
    ) -> astra_services::ServiceResult<()>;

    /// Project a previously declared settlement to the canonical invocation.
    ///
    /// Implementations with a durable debt store should use its exact indexed
    /// identity. The terminal mirror is the general in-memory fallback.
    async fn reconcile_settlement(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> astra_services::ServiceResult<astra_services::InferenceSettlementReconcileOutcome> {
        self.finish_invocation(plan, terminal).await?;
        Ok(astra_services::InferenceSettlementReconcileOutcome::Settled)
    }

    async fn finish_invocation(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> astra_services::ServiceResult<()>;

    async fn begin_provider_attempt(
        &self,
        attempt: &astra_services::InferenceProviderAttemptPlan,
    ) -> astra_services::ServiceResult<()>;

    async fn finish_provider_attempt(
        &self,
        attempt: &astra_services::InferenceProviderAttemptPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> astra_services::ServiceResult<()>;
}

struct DatabaseInferenceLedgerPersistence {
    shared_pool: SharedPool,
}

#[async_trait]
impl InferenceLedgerPersistence for DatabaseInferenceLedgerPersistence {
    async fn next_logical_attempt_pair_base(
        &self,
        input: &astra_services::InferenceInvocationInput,
    ) -> astra_services::ServiceResult<u32> {
        astra_services::next_inference_logical_attempt_pair_base(&self.shared_pool, input).await
    }

    async fn admit_invocation(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
    ) -> astra_services::ServiceResult<()> {
        astra_services::admit_inference_invocation(&self.shared_pool, plan).await
    }

    async fn renew_invocation_owner(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
    ) -> astra_services::ServiceResult<()> {
        astra_services::renew_inference_invocation_owner(&self.shared_pool, plan).await
    }

    async fn settle_uncertain_admission(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> astra_services::ServiceResult<astra_services::InferenceInvocationAdmissionResolution> {
        astra_services::settle_uncertain_inference_admission(&self.shared_pool, plan, terminal)
            .await
    }

    async fn declare_settlement(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> astra_services::ServiceResult<()> {
        astra_services::declare_inference_settlement(&self.shared_pool, plan, terminal).await
    }

    async fn declare_attempt_settlement(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
        attempt: &astra_services::InferenceProviderAttemptPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
        provider_delivery_state: astra_services::InferenceProviderDeliveryState,
    ) -> astra_services::ServiceResult<()> {
        astra_services::declare_inference_attempt_settlement(
            &self.shared_pool,
            plan,
            attempt,
            terminal,
            provider_delivery_state,
        )
        .await
    }

    async fn reconcile_settlement(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> astra_services::ServiceResult<astra_services::InferenceSettlementReconcileOutcome> {
        astra_services::reconcile_inference_settlement(&self.shared_pool, plan, terminal).await
    }

    async fn finish_invocation(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> astra_services::ServiceResult<()> {
        astra_services::finish_inference_invocation(&self.shared_pool, plan, terminal).await
    }

    async fn begin_provider_attempt(
        &self,
        attempt: &astra_services::InferenceProviderAttemptPlan,
    ) -> astra_services::ServiceResult<()> {
        astra_services::begin_inference_provider_attempt(&self.shared_pool, attempt).await
    }

    async fn finish_provider_attempt(
        &self,
        attempt: &astra_services::InferenceProviderAttemptPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> astra_services::ServiceResult<()> {
        astra_services::finish_inference_provider_attempt(&self.shared_pool, attempt, terminal)
            .await
    }
}

#[cfg(not(test))]
const INFERENCE_OWNER_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
#[cfg(not(test))]
const INFERENCE_OWNER_FAIL_CLOSED_AFTER: std::time::Duration = std::time::Duration::from_secs(45);

struct InferenceOwnerLease {
    lost: AtomicBool,
    cancel: tokio_util::sync::CancellationToken,
    stop: tokio_util::sync::CancellationToken,
}

impl InferenceOwnerLease {
    fn start(
        persistence: Arc<dyn InferenceLedgerPersistence>,
        plan: astra_services::InferenceInvocationPlan,
        parent_cancel: Option<&tokio_util::sync::CancellationToken>,
    ) -> Arc<Self> {
        let lease = Arc::new(Self {
            lost: AtomicBool::new(false),
            cancel: parent_cancel
                .map(tokio_util::sync::CancellationToken::child_token)
                .unwrap_or_default(),
            stop: tokio_util::sync::CancellationToken::new(),
        });
        #[cfg(not(test))]
        Self::spawn_heartbeat(lease.clone(), persistence, plan);
        #[cfg(test)]
        let _ = (persistence, plan);
        lease
    }

    #[cfg(not(test))]
    fn spawn_heartbeat(
        lease: Arc<Self>,
        persistence: Arc<dyn InferenceLedgerPersistence>,
        plan: astra_services::InferenceInvocationPlan,
    ) {
        Self::spawn_heartbeat_with_timing(
            lease,
            persistence,
            plan,
            INFERENCE_OWNER_HEARTBEAT_INTERVAL,
            INFERENCE_OWNER_FAIL_CLOSED_AFTER,
        );
    }

    fn spawn_heartbeat_with_timing(
        lease: Arc<Self>,
        persistence: Arc<dyn InferenceLedgerPersistence>,
        plan: astra_services::InferenceInvocationPlan,
        heartbeat_interval: std::time::Duration,
        fail_closed_after: std::time::Duration,
    ) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(heartbeat_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            interval.tick().await;
            let mut last_success = std::time::Instant::now();
            loop {
                tokio::select! {
                    _ = lease.stop.cancelled() => break,
                    _ = interval.tick() => {}
                }
                match persistence.renew_invocation_owner(&plan).await {
                    Ok(()) => last_success = std::time::Instant::now(),
                    Err(error)
                        if matches!(
                            error.kind,
                            astra_services::ServiceErrorKind::Conflict
                                | astra_services::ServiceErrorKind::NotFound
                                | astra_services::ServiceErrorKind::Invalid
                                | astra_services::ServiceErrorKind::Verification
                        ) =>
                    {
                        tracing::warn!(
                            invocation_id = %plan.invocation_id(),
                            %error,
                            "durable inference owner lease was fenced by authoritative state"
                        );
                        lease.mark_lost();
                        break;
                    }
                    Err(error) => {
                        tracing::warn!(
                            invocation_id = %plan.invocation_id(),
                            %error,
                            "durable inference owner heartbeat failed; retrying within lease budget"
                        );
                        if last_success.elapsed() >= fail_closed_after {
                            lease.mark_lost();
                            break;
                        }
                    }
                }
            }
        });
    }

    fn mark_lost(&self) {
        self.lost.store(true, Ordering::Release);
        self.cancel.cancel();
    }

    fn stop(&self) {
        self.stop.cancel();
    }

    fn is_lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    fn cancellation_error(&self) -> astra_core::ClassifiedError {
        if self.is_lost() {
            contract_error(
                "owner lease",
                "inference owner lease belongs to a newer durable generation",
            )
        } else {
            astra_core::ClassifiedError::new(
                astra_core::ErrorKind::Cancelled,
                "LLM call cancelled by its durable inference owner",
            )
        }
    }

    fn ensure_live(&self, stage: &'static str) -> Result<(), astra_core::ClassifiedError> {
        if self.lost.load(Ordering::Acquire) {
            return Err(contract_error(
                stage,
                "inference owner lease belongs to a newer durable generation",
            ));
        }
        Ok(())
    }
}

async fn wait_for_provider_or_owner_cancel<F, T>(
    provider: F,
    owner_lease: Arc<InferenceOwnerLease>,
) -> Result<T, astra_core::ClassifiedError>
where
    F: std::future::Future<Output = Result<T, astra_core::ClassifiedError>>,
{
    tokio::pin!(provider);
    tokio::select! {
        biased;
        _ = owner_lease.cancel.cancelled() => Err(owner_lease.cancellation_error()),
        result = &mut provider => result,
    }
}

enum ProviderSettlementTask {
    Debt {
        attempt: Box<Option<astra_services::InferenceProviderAttemptPlan>>,
        terminal: astra_services::InferenceInvocationTerminal,
        provider_delivery_state: astra_services::InferenceProviderDeliveryState,
    },
    AdmissionUncertain,
    InvocationOwnerLost {
        state: Arc<tokio::sync::Mutex<ProviderAttemptState>>,
        operations: ProviderOperationGate,
    },
}

struct ProviderSettlementJob {
    persistence: Arc<dyn InferenceLedgerPersistence>,
    invocation: astra_services::InferenceInvocationPlan,
    task: ProviderSettlementTask,
    owner_lease: Option<Arc<InferenceOwnerLease>>,
    _reservation: ProviderSettlementReservation,
}

/// Shared ownership keeps the exact job and its admission reservation alive
/// while one isolated worker attempt executes. If that attempt panics, its
/// supervisor still owns this envelope and can requeue the same durable fact.
struct ProviderSettlementJobEnvelope {
    job: ProviderSettlementJob,
    attempt_number: AtomicU32,
}

#[derive(Clone, Copy)]
enum ProviderSettlementWorkerScope {
    #[cfg(test)]
    CallerRuntime,
    ProcessRuntime,
}

impl ProviderSettlementJob {
    fn task_label(&self) -> &'static str {
        match &self.task {
            ProviderSettlementTask::Debt { attempt, .. } if attempt.is_some() => "attempt_debt",
            ProviderSettlementTask::Debt { .. } => "logical_debt",
            ProviderSettlementTask::AdmissionUncertain => "admission_uncertain",
            ProviderSettlementTask::InvocationOwnerLost { .. } => "invocation_owner_lost",
        }
    }
}

struct ProviderSettlementCoordinator {
    capacity_limit: usize,
    waiting_limit: usize,
    max_active_per_user: usize,
    max_active_per_session: usize,
    max_waiting_per_user: usize,
    max_waiting_per_session: usize,
    admission: std::sync::Mutex<SettlementAdmissionState>,
    queue: std::sync::Mutex<SettlementJobQueue>,
    ready: tokio::sync::Notify,
    workers_started: AtomicBool,
    worker_count: usize,
    worker_scope: ProviderSettlementWorkerScope,
    metrics: ProviderSettlementCoordinatorMetrics,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SettlementAdmissionOwner {
    user_id: String,
    session_id: String,
    scope_kind: &'static str,
}

impl SettlementAdmissionOwner {
    fn new(user_id: &str, scope: &astra_turn_types::InferenceInvocationScope) -> Self {
        let session_id = scope
            .session_id()
            .or_else(|| scope.harness_run_id())
            .unwrap_or(scope.kind())
            .to_string();
        Self {
            user_id: user_id.to_string(),
            session_id,
            scope_kind: scope.kind(),
        }
    }

    #[cfg(test)]
    fn for_test(user_id: &str, session_id: &str) -> Self {
        Self {
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            scope_kind: "run",
        }
    }
}

struct SettlementAdmissionWaiter {
    id: u64,
    scope_kind: &'static str,
    sender: tokio::sync::oneshot::Sender<Result<ProviderSettlementReservation, ()>>,
}

/// Monotonic RR position without a finite wrap point or queue-wide reindex.
///
/// The common path is one inline `u64`. A carry extends `high`, so ordering and
/// uniqueness remain exact even across integer rollover while work stays
/// proportional to the number of counter limbs, never the number of waiters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SettlementRrTicket {
    high: Vec<u64>,
    low: u64,
}

impl SettlementRrTicket {
    fn increment(&mut self) {
        if let Some(next) = self.low.checked_add(1) {
            self.low = next;
            return;
        }
        self.low = 0;
        for digit in self.high.iter_mut().rev() {
            if let Some(next) = digit.checked_add(1) {
                *digit = next;
                return;
            }
            *digit = 0;
        }
        self.high.insert(0, 1);
    }
}

impl Ord for SettlementRrTicket {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.high
            .len()
            .cmp(&other.high.len())
            .then_with(|| self.high.cmp(&other.high))
            .then_with(|| self.low.cmp(&other.low))
    }
}

impl PartialOrd for SettlementRrTicket {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Default)]
struct SettlementSessionQueue {
    waiters: VecDeque<SettlementAdmissionWaiter>,
    rr_ticket: SettlementRrTicket,
}

#[derive(Default)]
struct SettlementUserQueue {
    sessions: HashMap<String, SettlementSessionQueue>,
    ready_sessions: BTreeSet<(SettlementRrTicket, String)>,
    rr_ticket: SettlementRrTicket,
}

struct SettlementAdmissionState {
    admission_open: bool,
    in_use: usize,
    queued: usize,
    next_waiter_id: u64,
    users: HashMap<String, SettlementUserQueue>,
    ready_users: BTreeSet<(SettlementRrTicket, String)>,
    next_rr_ticket: SettlementRrTicket,
    active_by_user: HashMap<String, usize>,
    active_by_session: HashMap<(String, String), usize>,
    queued_by_user: HashMap<String, usize>,
    queued_by_session: HashMap<(String, String), usize>,
}

impl Default for SettlementAdmissionState {
    fn default() -> Self {
        Self {
            admission_open: true,
            in_use: 0,
            queued: 0,
            next_waiter_id: 0,
            users: HashMap::new(),
            ready_users: BTreeSet::new(),
            next_rr_ticket: SettlementRrTicket::default(),
            active_by_user: HashMap::new(),
            active_by_session: HashMap::new(),
            queued_by_user: HashMap::new(),
            queued_by_session: HashMap::new(),
        }
    }
}

#[derive(Default)]
struct SettlementJobSessionQueue {
    jobs: VecDeque<Arc<ProviderSettlementJobEnvelope>>,
}

#[derive(Default)]
struct SettlementJobUserQueue {
    sessions: HashMap<String, SettlementJobSessionQueue>,
    session_order: VecDeque<String>,
}

#[derive(Default)]
struct SettlementJobQueue {
    users: HashMap<String, SettlementJobUserQueue>,
    user_order: VecDeque<String>,
    queued: usize,
}

#[derive(Default)]
struct ProviderSettlementCoordinatorMetrics {
    admitted_immediate: AtomicU64,
    queued_admissions: AtomicU64,
    admitted_from_queue: AtomicU64,
    rejected_bounded: AtomicU64,
    rejected_shutdown: AtomicU64,
    cancelled_waiters: AtomicU64,
    reconciliation_retries: AtomicU64,
    permanently_quarantined: AtomicU64,
    worker_panics: AtomicU64,
    #[cfg(test)]
    admission_ready_candidates_examined: AtomicU64,
}

struct ProviderSettlementReservation {
    coordinator: Weak<ProviderSettlementCoordinator>,
    owner: SettlementAdmissionOwner,
    active: bool,
}

impl Drop for ProviderSettlementReservation {
    fn drop(&mut self) {
        if self.active
            && let Some(coordinator) = self.coordinator.upgrade()
        {
            coordinator.release_reservation(&self.owner);
        }
    }
}

struct SettlementAdmissionWaitGuard {
    coordinator: Weak<ProviderSettlementCoordinator>,
    owner: SettlementAdmissionOwner,
    waiter_id: u64,
    armed: bool,
}

impl Drop for SettlementAdmissionWaitGuard {
    fn drop(&mut self) {
        if self.armed
            && let Some(coordinator) = self.coordinator.upgrade()
        {
            coordinator.cancel_waiter(&self.owner, self.waiter_id);
        }
    }
}

#[cfg(not(test))]
fn positive_env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn default_fair_share(capacity: usize) -> usize {
    if capacity < 8 {
        capacity
    } else {
        capacity.saturating_sub((capacity / 4).max(1)).max(1)
    }
}

fn proportional_waiting_share(
    waiting_capacity: usize,
    active_share: usize,
    active_capacity: usize,
) -> usize {
    if waiting_capacity == 0 {
        return 0;
    }
    let proportional = waiting_capacity
        .saturating_mul(active_share)
        .checked_div(active_capacity.max(1))
        .unwrap_or(0)
        .max(1);
    if waiting_capacity == 1 {
        1
    } else {
        // Even an unconstrained active owner cannot consume the final waiting
        // slot. That slot is the bounded fairness escape hatch for another
        // tenant/session when all active capacity is busy.
        proportional.min(waiting_capacity - 1)
    }
}

impl ProviderSettlementCoordinator {
    #[cfg(test)]
    fn new(capacity: usize, worker_count: usize) -> Arc<Self> {
        Self::new_with_limits_and_worker_scope(
            capacity,
            capacity.saturating_mul(8).max(32),
            worker_count,
            ProviderSettlementWorkerScope::CallerRuntime,
        )
    }

    #[cfg(test)]
    fn new_with_waiting_capacity(
        capacity: usize,
        waiting_capacity: usize,
        worker_count: usize,
    ) -> Arc<Self> {
        Self::new_with_limits_and_worker_scope(
            capacity,
            waiting_capacity,
            worker_count,
            ProviderSettlementWorkerScope::CallerRuntime,
        )
    }

    #[cfg(test)]
    fn new_with_fair_limits(
        capacity: usize,
        waiting_capacity: usize,
        worker_count: usize,
        max_active_per_user: usize,
        max_active_per_session: usize,
    ) -> Arc<Self> {
        Self::new_with_fair_limits_and_worker_scope(
            capacity,
            waiting_capacity,
            worker_count,
            max_active_per_user,
            max_active_per_session,
            ProviderSettlementWorkerScope::CallerRuntime,
        )
    }

    #[cfg(test)]
    fn new_process_scoped(capacity: usize, worker_count: usize) -> Arc<Self> {
        Self::new_with_limits_and_worker_scope(
            capacity,
            DEFAULT_PROVIDER_SETTLEMENT_WAIT_CAPACITY,
            worker_count,
            ProviderSettlementWorkerScope::ProcessRuntime,
        )
    }

    #[cfg(test)]
    fn new_with_limits_and_worker_scope(
        capacity: usize,
        waiting_limit: usize,
        worker_count: usize,
        worker_scope: ProviderSettlementWorkerScope,
    ) -> Arc<Self> {
        let capacity = capacity.max(1);
        let max_active_per_user = default_fair_share(capacity);
        let max_active_per_session = default_fair_share(max_active_per_user);
        Self::new_with_fair_limits_and_worker_scope(
            capacity,
            waiting_limit,
            worker_count,
            max_active_per_user,
            max_active_per_session,
            worker_scope,
        )
    }

    fn new_with_fair_limits_and_worker_scope(
        capacity: usize,
        waiting_limit: usize,
        worker_count: usize,
        max_active_per_user: usize,
        max_active_per_session: usize,
        worker_scope: ProviderSettlementWorkerScope,
    ) -> Arc<Self> {
        let capacity = capacity.max(1);
        let max_active_per_user = max_active_per_user.clamp(1, capacity);
        let max_active_per_session = max_active_per_session.clamp(1, max_active_per_user);
        let max_waiting_per_user =
            proportional_waiting_share(waiting_limit, max_active_per_user, capacity);
        let max_waiting_per_session = proportional_waiting_share(
            max_waiting_per_user,
            max_active_per_session,
            max_active_per_user,
        );
        Arc::new(Self {
            capacity_limit: capacity,
            waiting_limit,
            max_active_per_user,
            max_active_per_session,
            max_waiting_per_user,
            max_waiting_per_session,
            admission: std::sync::Mutex::new(SettlementAdmissionState::default()),
            queue: std::sync::Mutex::new(SettlementJobQueue::default()),
            ready: tokio::sync::Notify::new(),
            workers_started: AtomicBool::new(false),
            worker_count: worker_count.max(1).min(capacity),
            worker_scope,
            metrics: ProviderSettlementCoordinatorMetrics::default(),
        })
    }

    #[cfg(not(test))]
    fn runtime() -> Arc<Self> {
        PROVIDER_SETTLEMENT_COORDINATOR
            .get_or_init(|| {
                let capacity = positive_env_usize(
                    ENV_PROVIDER_SETTLEMENT_CAPACITY,
                    DEFAULT_PROVIDER_SETTLEMENT_CAPACITY,
                );
                let max_active_per_user = positive_env_usize(
                    ENV_PROVIDER_SETTLEMENT_MAX_ACTIVE_PER_USER,
                    default_fair_share(capacity),
                );
                Self::new_with_fair_limits_and_worker_scope(
                    capacity,
                    positive_env_usize(
                        ENV_PROVIDER_SETTLEMENT_WAIT_CAPACITY,
                        DEFAULT_PROVIDER_SETTLEMENT_WAIT_CAPACITY,
                    ),
                    positive_env_usize(
                        ENV_PROVIDER_SETTLEMENT_WORKERS,
                        DEFAULT_PROVIDER_SETTLEMENT_WORKERS,
                    ),
                    max_active_per_user,
                    positive_env_usize(
                        ENV_PROVIDER_SETTLEMENT_MAX_ACTIVE_PER_SESSION,
                        default_fair_share(max_active_per_user.min(capacity)),
                    ),
                    ProviderSettlementWorkerScope::ProcessRuntime,
                )
            })
            .clone()
    }

    #[cfg(test)]
    fn runtime() -> Arc<Self> {
        // Tokio unit tests use independent runtimes. A process-global sender
        // would outlive the runtime that owns its workers and make later tests
        // observe a closed coordinator.
        Self::new(
            DEFAULT_PROVIDER_SETTLEMENT_CAPACITY,
            DEFAULT_PROVIDER_SETTLEMENT_WORKERS,
        )
    }

    fn ensure_workers(self: &Arc<Self>) -> Result<(), astra_core::ClassifiedError> {
        let worker_handle = match self.worker_scope {
            #[cfg(test)]
            ProviderSettlementWorkerScope::CallerRuntime => tokio::runtime::Handle::try_current()
                .map_err(|error| {
                astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::ContractViolation,
                    format!("Provider settlement requires an async runtime: {error}"),
                )
            })?,
            ProviderSettlementWorkerScope::ProcessRuntime => {
                let runtime = PROVIDER_SETTLEMENT_WORKER_RUNTIME.get_or_init(|| {
                    tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(DEFAULT_PROVIDER_SETTLEMENT_WORKERS)
                        .thread_name("astra-provider-settlement")
                        .enable_all()
                        .build()
                        .map_err(|error| error.to_string())
                });
                runtime
                    .as_ref()
                    .map(tokio::runtime::Runtime::handle)
                    .cloned()
                    .map_err(|error| {
                        astra_core::ClassifiedError::new(
                            astra_core::ErrorKind::ResourceLimit,
                            format!("Provider settlement worker runtime is unavailable: {error}"),
                        )
                    })?
            }
        };
        if self
            .workers_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(());
        }
        for worker_id in 0..self.worker_count {
            let coordinator = self.clone();
            // This fixed task is a supervisor only. Each persistence attempt
            // runs in a replaceable child task while the supervisor retains
            // exact job ownership across panic or cancellation.
            worker_handle.spawn(async move { coordinator.worker_supervisor_loop(worker_id).await });
        }
        Ok(())
    }

    fn reservation(
        self: &Arc<Self>,
        owner: SettlementAdmissionOwner,
    ) -> ProviderSettlementReservation {
        ProviderSettlementReservation {
            coordinator: Arc::downgrade(self),
            owner,
            active: true,
        }
    }

    fn owner_is_eligible(
        &self,
        state: &SettlementAdmissionState,
        owner: &SettlementAdmissionOwner,
    ) -> bool {
        state.in_use < self.capacity_limit && self.owner_share_is_eligible(state, owner)
    }

    fn owner_share_is_eligible(
        &self,
        state: &SettlementAdmissionState,
        owner: &SettlementAdmissionOwner,
    ) -> bool {
        state
            .active_by_user
            .get(&owner.user_id)
            .copied()
            .unwrap_or(0)
            < self.max_active_per_user
            && state
                .active_by_session
                .get(&(owner.user_id.clone(), owner.session_id.clone()))
                .copied()
                .unwrap_or(0)
                < self.max_active_per_session
    }

    fn next_rr_ticket(state: &mut SettlementAdmissionState) -> SettlementRrTicket {
        let ticket = state.next_rr_ticket.clone();
        state.next_rr_ticket.increment();
        ticket
    }

    fn refresh_owner_ready(
        &self,
        state: &mut SettlementAdmissionState,
        owner: &SettlementAdmissionOwner,
    ) {
        let user_share_is_eligible = state
            .active_by_user
            .get(&owner.user_id)
            .copied()
            .unwrap_or(0)
            < self.max_active_per_user;
        let session_share_is_eligible = state
            .active_by_session
            .get(&(owner.user_id.clone(), owner.session_id.clone()))
            .copied()
            .unwrap_or(0)
            < self.max_active_per_session;
        let Some(user) = state.users.get_mut(&owner.user_id) else {
            return;
        };
        if let Some(session) = user.sessions.get(&owner.session_id) {
            let session_key = (session.rr_ticket.clone(), owner.session_id.clone());
            if session_share_is_eligible && !session.waiters.is_empty() {
                user.ready_sessions.insert(session_key);
            } else {
                user.ready_sessions.remove(&session_key);
            }
        }
        let user_key = (user.rr_ticket.clone(), owner.user_id.clone());
        let user_is_ready = user_share_is_eligible && !user.ready_sessions.is_empty();
        if user_is_ready {
            state.ready_users.insert(user_key);
        } else {
            state.ready_users.remove(&user_key);
        }
    }

    fn activate_owner(
        &self,
        state: &mut SettlementAdmissionState,
        owner: &SettlementAdmissionOwner,
    ) {
        state.in_use += 1;
        *state
            .active_by_user
            .entry(owner.user_id.clone())
            .or_default() += 1;
        *state
            .active_by_session
            .entry((owner.user_id.clone(), owner.session_id.clone()))
            .or_default() += 1;
        self.refresh_owner_ready(state, owner);
    }

    fn deactivate_owner(
        &self,
        state: &mut SettlementAdmissionState,
        owner: &SettlementAdmissionOwner,
    ) {
        debug_assert!(state.in_use > 0, "settlement reservation underflow");
        state.in_use = state.in_use.saturating_sub(1);
        if let Some(active) = state.active_by_user.get_mut(&owner.user_id) {
            *active = active.saturating_sub(1);
            if *active == 0 {
                state.active_by_user.remove(&owner.user_id);
            }
        }
        let session_key = (owner.user_id.clone(), owner.session_id.clone());
        if let Some(active) = state.active_by_session.get_mut(&session_key) {
            *active = active.saturating_sub(1);
            if *active == 0 {
                state.active_by_session.remove(&session_key);
            }
        }
        self.refresh_owner_ready(state, owner);
    }

    fn owner_waiting_share_available(
        &self,
        state: &SettlementAdmissionState,
        owner: &SettlementAdmissionOwner,
    ) -> bool {
        state
            .queued_by_user
            .get(&owner.user_id)
            .copied()
            .unwrap_or(0)
            < self.max_waiting_per_user
            && state
                .queued_by_session
                .get(&(owner.user_id.clone(), owner.session_id.clone()))
                .copied()
                .unwrap_or(0)
                < self.max_waiting_per_session
    }

    fn increment_waiting_owner(
        state: &mut SettlementAdmissionState,
        owner: &SettlementAdmissionOwner,
    ) {
        *state
            .queued_by_user
            .entry(owner.user_id.clone())
            .or_default() += 1;
        *state
            .queued_by_session
            .entry((owner.user_id.clone(), owner.session_id.clone()))
            .or_default() += 1;
    }

    fn decrement_waiting_owner(
        state: &mut SettlementAdmissionState,
        owner: &SettlementAdmissionOwner,
    ) {
        if let Some(waiting) = state.queued_by_user.get_mut(&owner.user_id) {
            *waiting = waiting.saturating_sub(1);
            if *waiting == 0 {
                state.queued_by_user.remove(&owner.user_id);
            }
        }
        let session_key = (owner.user_id.clone(), owner.session_id.clone());
        if let Some(waiting) = state.queued_by_session.get_mut(&session_key) {
            *waiting = waiting.saturating_sub(1);
            if *waiting == 0 {
                state.queued_by_session.remove(&session_key);
            }
        }
    }

    fn admission_error(
        &self,
        message: &'static str,
        outcome: &'static str,
        state: &SettlementAdmissionState,
    ) -> astra_core::ClassifiedError {
        astra_core::ClassifiedError::new(astra_core::ErrorKind::ResourceLimit, message)
            .with_details_json(
                json!({
                    "source": INFERENCE_LEDGER_ERROR_SOURCE,
                    "stage": "settlement_reservation",
                    "outcome": outcome,
                    "capacity": self.capacity_limit,
                    "in_use": state.in_use,
                    "waiting_capacity": self.waiting_limit,
                    "waiting": state.queued,
                })
                .to_string(),
            )
    }

    async fn reserve(
        self: &Arc<Self>,
        owner: SettlementAdmissionOwner,
    ) -> Result<ProviderSettlementReservation, astra_core::ClassifiedError> {
        self.ensure_workers()?;
        let queued = {
            let mut state = self
                .admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !state.admission_open {
                self.metrics
                    .rejected_shutdown
                    .fetch_add(1, Ordering::Relaxed);
                return Err(self.admission_error(
                    "Provider execution is closed while durable settlements drain for shutdown",
                    "shutdown",
                    &state,
                ));
            }
            // Every mutation that can make a queued waiter eligible dispatches
            // the fair RR queue before releasing this lock. Therefore idle
            // capacity plus queued work means every existing waiter is fenced
            // by its active user/session share. A different eligible owner may
            // use that otherwise-wasted capacity in O(1), while a release under
            // global contention still serves the existing RR queue first.
            if self.owner_is_eligible(&state, &owner) {
                self.activate_owner(&mut state, &owner);
                self.metrics
                    .admitted_immediate
                    .fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    target: "astra::inference_settlement::metrics",
                    outcome = "admitted_immediate",
                    scope = owner.scope_kind,
                    in_use = state.in_use,
                    waiting = state.queued,
                    capacity = self.capacity_limit,
                    "provider settlement admission"
                );
                return Ok(self.reservation(owner));
            }
            if state.queued >= self.waiting_limit {
                self.metrics
                    .rejected_bounded
                    .fetch_add(1, Ordering::Relaxed);
                return Err(self.admission_error(
                    "Provider execution is paused because the bounded durable settlement wait queue is full",
                    "wait_queue_full",
                    &state,
                ));
            }
            if !self.owner_waiting_share_available(&state, &owner) {
                self.metrics
                    .rejected_bounded
                    .fetch_add(1, Ordering::Relaxed);
                let user_waiting = state
                    .queued_by_user
                    .get(&owner.user_id)
                    .copied()
                    .unwrap_or(0);
                let session_waiting = state
                    .queued_by_session
                    .get(&(owner.user_id.clone(), owner.session_id.clone()))
                    .copied()
                    .unwrap_or(0);
                let outcome = if session_waiting >= self.max_waiting_per_session {
                    "wait_session_share_full"
                } else {
                    "wait_user_share_full"
                };
                return Err(self
                    .admission_error(
                        "Provider execution is paused because this owner reached its durable settlement waiting share",
                        outcome,
                        &state,
                    )
                    .with_details_json(
                        json!({
                            "source": INFERENCE_LEDGER_ERROR_SOURCE,
                            "stage": "settlement_reservation",
                            "outcome": outcome,
                            "capacity": self.capacity_limit,
                            "in_use": state.in_use,
                            "waiting_capacity": self.waiting_limit,
                            "waiting": state.queued,
                            "user_waiting": user_waiting,
                            "user_waiting_share": self.max_waiting_per_user,
                            "session_waiting": session_waiting,
                            "session_waiting_share": self.max_waiting_per_session,
                        })
                        .to_string(),
                    ));
            }
            let waiter_id = state.next_waiter_id;
            state.next_waiter_id = state.next_waiter_id.checked_add(1).ok_or_else(|| {
                self.admission_error(
                    "Provider settlement waiter identity space is exhausted",
                    "waiter_identity_exhausted",
                    &state,
                )
            })?;
            let (sender, receiver) = tokio::sync::oneshot::channel();
            self.push_waiter(
                &mut state,
                &owner,
                SettlementAdmissionWaiter {
                    id: waiter_id,
                    scope_kind: owner.scope_kind,
                    sender,
                },
            );
            self.metrics
                .queued_admissions
                .fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                target: "astra::inference_settlement::metrics",
                outcome = "queued",
                scope = owner.scope_kind,
                in_use = state.in_use,
                waiting = state.queued,
                capacity = self.capacity_limit,
                "provider settlement admission"
            );
            // Enqueueing does not change active ownership, so it cannot make
            // any waiter eligible. Releases/cancellations perform RR dispatch;
            // avoid an O(queue) scan on this admission hot path.
            (receiver, waiter_id)
        };
        let mut guard = SettlementAdmissionWaitGuard {
            coordinator: Arc::downgrade(self),
            owner: owner.clone(),
            waiter_id: queued.1,
            armed: true,
        };
        let outcome = queued.0.await;
        guard.armed = false;
        match outcome {
            Ok(Ok(reservation)) => Ok(reservation),
            Ok(Err(())) | Err(_) => {
                let state = self
                    .admission
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                Err(self.admission_error(
                    "Provider execution closed before a durable settlement reservation was granted",
                    "shutdown",
                    &state,
                ))
            }
        }
    }

    /// Pop exactly one share-eligible waiter in hierarchical RR order.
    ///
    /// `ready_users` and each user's `ready_sessions` are ordered indexes over
    /// the existing queue maps. Ineligible owners remain in those maps with
    /// their RR tickets, so becoming eligible restores their original place
    /// without scanning unrelated waiters.
    fn pop_next_ready_waiter(
        &self,
        state: &mut SettlementAdmissionState,
    ) -> Option<(SettlementAdmissionOwner, SettlementAdmissionWaiter)> {
        let (user_ticket, user_id) = state.ready_users.pop_first()?;
        let next_user_ticket = Self::next_rr_ticket(state);
        let next_session_ticket = Self::next_rr_ticket(state);
        #[cfg(test)]
        self.metrics
            .admission_ready_candidates_examined
            .fetch_add(1, Ordering::Relaxed);

        let (session_id, waiter, user_remains_ready, remove_user) = {
            let user = state
                .users
                .get_mut(&user_id)
                .expect("ready settlement user must exist");
            debug_assert_eq!(&user.rr_ticket, &user_ticket);
            let (session_ticket, session_id) = user
                .ready_sessions
                .pop_first()
                .expect("ready settlement user must have a ready session");
            let session = user
                .sessions
                .get_mut(&session_id)
                .expect("ready settlement session must exist");
            debug_assert_eq!(&session.rr_ticket, &session_ticket);
            let waiter = session
                .waiters
                .pop_front()
                .expect("ready settlement session must have a waiter");
            let remove_session = session.waiters.is_empty();
            if !remove_session {
                session.rr_ticket = next_session_ticket;
                user.ready_sessions
                    .insert((session.rr_ticket.clone(), session_id.clone()));
            }
            if remove_session {
                user.sessions.remove(&session_id);
            }
            user.rr_ticket = next_user_ticket.clone();
            (
                session_id,
                waiter,
                !user.ready_sessions.is_empty(),
                user.sessions.is_empty(),
            )
        };
        if remove_user {
            state.users.remove(&user_id);
        } else if user_remains_ready {
            state
                .ready_users
                .insert((next_user_ticket, user_id.clone()));
        }

        state.queued = state.queued.saturating_sub(1);
        let owner = SettlementAdmissionOwner {
            user_id,
            session_id,
            scope_kind: waiter.scope_kind,
        };
        Self::decrement_waiting_owner(state, &owner);
        Some((owner, waiter))
    }

    fn push_waiter(
        &self,
        state: &mut SettlementAdmissionState,
        owner: &SettlementAdmissionOwner,
        waiter: SettlementAdmissionWaiter,
    ) {
        let user_is_new = !state.users.contains_key(&owner.user_id);
        let session_is_new = state
            .users
            .get(&owner.user_id)
            .is_none_or(|user| !user.sessions.contains_key(&owner.session_id));
        let user_ticket = user_is_new.then(|| Self::next_rr_ticket(state));
        let session_ticket = session_is_new.then(|| Self::next_rr_ticket(state));
        let user =
            state
                .users
                .entry(owner.user_id.clone())
                .or_insert_with(|| SettlementUserQueue {
                    rr_ticket: user_ticket.expect("new settlement user owns an RR ticket"),
                    ..Default::default()
                });
        user.sessions
            .entry(owner.session_id.clone())
            .or_insert_with(|| SettlementSessionQueue {
                rr_ticket: session_ticket.expect("new settlement session owns an RR ticket"),
                ..Default::default()
            })
            .waiters
            .push_back(waiter);
        state.queued += 1;
        Self::increment_waiting_owner(state, owner);
        self.refresh_owner_ready(state, owner);
    }

    fn dispatch_available_locked(self: &Arc<Self>, state: &mut SettlementAdmissionState) {
        while state.admission_open && state.in_use < self.capacity_limit {
            let Some((owner, waiter)) = self.pop_next_ready_waiter(state) else {
                break;
            };
            self.activate_owner(state, &owner);
            match waiter.sender.send(Ok(self.reservation(owner.clone()))) {
                Ok(()) => {
                    self.metrics
                        .admitted_from_queue
                        .fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(
                        target: "astra::inference_settlement::metrics",
                        outcome = "admitted_fair",
                        scope = owner.scope_kind,
                        in_use = state.in_use,
                        waiting = state.queued,
                        capacity = self.capacity_limit,
                        "provider settlement admission"
                    );
                }
                Err(returned) => {
                    self.deactivate_owner(state, &owner);
                    if let Ok(mut reservation) = returned {
                        reservation.active = false;
                    }
                }
            }
        }
    }

    fn release_reservation(self: &Arc<Self>, owner: &SettlementAdmissionOwner) {
        let mut state = self
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.deactivate_owner(&mut state, owner);
        self.dispatch_available_locked(&mut state);
        tracing::debug!(
            target: "astra::inference_settlement::metrics",
            outcome = "released",
            scope = owner.scope_kind,
            in_use = state.in_use,
            waiting = state.queued,
            capacity = self.capacity_limit,
            "provider settlement admission"
        );
    }

    fn cancel_waiter(self: &Arc<Self>, owner: &SettlementAdmissionOwner, waiter_id: u64) {
        let mut state = self
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut removed = false;
        let mut remove_session = false;
        let mut remove_user = false;
        let mut remove_ready_user = None;
        if let Some(user) = state.users.get_mut(&owner.user_id) {
            if let Some(session) = user.sessions.get_mut(&owner.session_id)
                && let Some(position) = session
                    .waiters
                    .iter()
                    .position(|waiter| waiter.id == waiter_id)
            {
                session.waiters.remove(position);
                removed = true;
                remove_session = session.waiters.is_empty();
            }
            if remove_session {
                if let Some(session) = user.sessions.get(&owner.session_id) {
                    user.ready_sessions
                        .remove(&(session.rr_ticket.clone(), owner.session_id.clone()));
                }
                user.sessions.remove(&owner.session_id);
            }
            remove_user = user.sessions.is_empty();
            if user.ready_sessions.is_empty() {
                remove_ready_user = Some((user.rr_ticket.clone(), owner.user_id.clone()));
            }
        }
        if let Some(user_key) = remove_ready_user {
            state.ready_users.remove(&user_key);
        }
        if remove_user {
            state.users.remove(&owner.user_id);
        }
        if removed {
            state.queued = state.queued.saturating_sub(1);
            Self::decrement_waiting_owner(&mut state, owner);
            self.metrics
                .cancelled_waiters
                .fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                target: "astra::inference_settlement::metrics",
                outcome = "cancelled_waiter",
                scope = owner.scope_kind,
                in_use = state.in_use,
                waiting = state.queued,
                capacity = self.capacity_limit,
                "provider settlement admission"
            );
            self.dispatch_available_locked(&mut state);
        }
    }

    fn available_permits(&self) -> usize {
        let state = self
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.capacity_limit.saturating_sub(state.in_use)
    }

    #[cfg(test)]
    fn queued_admissions(&self) -> usize {
        self.admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .queued
    }

    #[cfg(test)]
    fn reserve_immediate_for_test(
        self: &Arc<Self>,
        owner: SettlementAdmissionOwner,
    ) -> Result<ProviderSettlementReservation, astra_core::ClassifiedError> {
        self.ensure_workers()?;
        let mut state = self
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.admission_open || state.queued != 0 || !self.owner_is_eligible(&state, &owner) {
            return Err(self.admission_error(
                "test settlement reservation is not immediately available",
                "test_unavailable",
                &state,
            ));
        }
        self.activate_owner(&mut state, &owner);
        Ok(self.reservation(owner))
    }

    fn handoff(&self, job: ProviderSettlementJob) {
        self.handoff_envelope(Arc::new(ProviderSettlementJobEnvelope {
            job,
            attempt_number: AtomicU32::new(0),
        }));
    }

    fn handoff_envelope(&self, job: Arc<ProviderSettlementJobEnvelope>) {
        let owner = job.job._reservation.owner.clone();
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let user_is_new = !queue.users.contains_key(&owner.user_id);
        let user = queue.users.entry(owner.user_id.clone()).or_default();
        let session_is_new = !user.sessions.contains_key(&owner.session_id);
        user.sessions
            .entry(owner.session_id.clone())
            .or_default()
            .jobs
            .push_back(job);
        if session_is_new {
            user.session_order.push_back(owner.session_id);
        }
        if user_is_new {
            queue.user_order.push_back(owner.user_id);
        }
        queue.queued += 1;
        drop(queue);
        self.ready.notify_one();
    }

    fn pop_next_job(queue: &mut SettlementJobQueue) -> Option<Arc<ProviderSettlementJobEnvelope>> {
        while let Some(user_id) = queue.user_order.pop_front() {
            let remove_user;
            let selected = {
                let Some(user) = queue.users.get_mut(&user_id) else {
                    continue;
                };
                let mut selected = None;
                while let Some(session_id) = user.session_order.pop_front() {
                    let mut remove_session = false;
                    if let Some(session) = user.sessions.get_mut(&session_id) {
                        selected = session.jobs.pop_front();
                        remove_session = session.jobs.is_empty();
                    }
                    if remove_session {
                        user.sessions.remove(&session_id);
                    } else if user.sessions.contains_key(&session_id) {
                        user.session_order.push_back(session_id);
                    }
                    if selected.is_some() {
                        break;
                    }
                }
                remove_user = user.sessions.is_empty();
                selected
            };
            if remove_user {
                queue.users.remove(&user_id);
            } else {
                queue.user_order.push_back(user_id);
            }
            if selected.is_some() {
                queue.queued = queue.queued.saturating_sub(1);
                return selected;
            }
        }
        None
    }

    async fn next_job(&self) -> Arc<ProviderSettlementJobEnvelope> {
        loop {
            let notified = self.ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(job) = Self::pop_next_job(
                &mut self
                    .queue
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            ) {
                return job;
            }
            notified.await;
        }
    }

    async fn worker_supervisor_loop(self: Arc<Self>, worker_id: usize) {
        loop {
            let job = self.next_job().await;
            let attempt_number = job
                .attempt_number
                .fetch_add(1, Ordering::AcqRel)
                .saturating_add(1);
            // Persistence implementations and driver futures are isolated
            // from the fixed supervisor. Tokio reports a panic through the
            // JoinHandle, while this scope retains the exact job envelope and
            // its reservation for deterministic replay.
            let worker_job = job.clone();
            let result = tokio::spawn(async move {
                tokio::time::timeout(
                    detached_reconciliation_timeout(),
                    reconcile_provider_settlement_job(&worker_job.job),
                )
                .await
            })
            .await;
            match result {
                Ok(Ok(Ok(ProviderSettlementDisposition::Settled)))
                | Ok(Ok(Ok(ProviderSettlementDisposition::SweeperOwned))) => {
                    if let Some(owner_lease) = job.job.owner_lease.as_ref() {
                        owner_lease.stop();
                    }
                    drop(job);
                    continue;
                }
                Ok(Ok(Ok(ProviderSettlementDisposition::PermanentlyQuarantined))) => {
                    self.metrics
                        .permanently_quarantined
                        .fetch_add(1, Ordering::Relaxed);
                    tracing::error!(
                        worker_id,
                        attempt_number,
                        invocation_id = %job.job.invocation.invocation_id(),
                        task = job.job.task_label(),
                        "provider settlement is permanently quarantined; releasing runtime owner and retaining durable incident"
                    );
                    if let Some(owner_lease) = job.job.owner_lease.as_ref() {
                        owner_lease.stop();
                    }
                    drop(job);
                    continue;
                }
                Ok(Ok(Ok(ProviderSettlementDisposition::TransientPending))) => tracing::warn!(
                    worker_id,
                    attempt_number,
                    invocation_id = %job.job.invocation.invocation_id(),
                    task = job.job.task_label(),
                    "provider settlement remains transiently pending; retaining bounded owner"
                ),
                Ok(Ok(Err(error))) => tracing::warn!(
                    worker_id,
                    attempt_number,
                    invocation_id = %job.job.invocation.invocation_id(),
                    task = job.job.task_label(),
                    %error,
                    "provider settlement coordinator will retry durable handoff"
                ),
                Ok(Err(_)) => tracing::warn!(
                    worker_id,
                    attempt_number,
                    invocation_id = %job.job.invocation.invocation_id(),
                    task = job.job.task_label(),
                    timeout_ms = detached_reconciliation_timeout().as_millis(),
                    "provider settlement debt declaration timed out; retaining bounded owner"
                ),
                Err(error) => {
                    self.metrics.worker_panics.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(
                        worker_id,
                        attempt_number,
                        invocation_id = %job.job.invocation.invocation_id(),
                        task = job.job.task_label(),
                        panic = error.is_panic(),
                        cancelled = error.is_cancelled(),
                        %error,
                        "isolated provider settlement worker stopped; supervisor retained the exact job and will replace it"
                    );
                }
            }
            self.metrics
                .reconciliation_retries
                .fetch_add(1, Ordering::Relaxed);
            // A poison item must retain its reservation and exact terminal,
            // but it must not monopolize one of the fixed workers. Requeue at
            // the tail after a bounded delay so all users' already-reserved
            // settlements continue to make progress.
            #[cfg(not(test))]
            let backoff =
                std::time::Duration::from_millis(1_000 + u64::from(attempt_number % 8) * 250);
            #[cfg(test)]
            let backoff = std::time::Duration::from_millis(5);
            // The delayed retry owns the exact reservation, while this worker
            // immediately returns to the fair queue. A bounded set of poison
            // jobs therefore cannot consume all worker loops merely by being
            // in backoff.
            let coordinator = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(backoff).await;
                coordinator.handoff_envelope(job);
            });
        }
    }

    #[cfg(test)]
    fn queued_jobs(&self) -> usize {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .queued
    }

    async fn close_and_drain(&self, timeout: std::time::Duration) -> bool {
        let waiting = {
            let mut state = self
                .admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.admission_open = false;
            let mut waiting = Vec::with_capacity(state.queued);
            for user in std::mem::take(&mut state.users).into_values() {
                for session in user.sessions.into_values() {
                    waiting.extend(session.waiters);
                }
            }
            state.ready_users.clear();
            state.queued = 0;
            state.queued_by_user.clear();
            state.queued_by_session.clear();
            waiting
        };
        for waiter in waiting {
            self.metrics
                .rejected_shutdown
                .fetch_add(1, Ordering::Relaxed);
            let _ = waiter.sender.send(Err(()));
        }
        tokio::time::timeout(timeout, async {
            while self.available_permits() != self.capacity_limit {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .is_ok()
    }

    #[cfg(not(test))]
    async fn wait_for_drain(&self, timeout: std::time::Duration) -> bool {
        tokio::time::timeout(timeout, async {
            while self.available_permits() != self.capacity_limit {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .is_ok()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProviderSettlementDisposition {
    Settled,
    TransientPending,
    PermanentlyQuarantined,
    SweeperOwned,
}

impl From<astra_services::InferenceSettlementReconcileOutcome> for ProviderSettlementDisposition {
    fn from(outcome: astra_services::InferenceSettlementReconcileOutcome) -> Self {
        match outcome {
            astra_services::InferenceSettlementReconcileOutcome::Settled => Self::Settled,
            astra_services::InferenceSettlementReconcileOutcome::TransientPending => {
                Self::TransientPending
            }
            astra_services::InferenceSettlementReconcileOutcome::PermanentlyQuarantined => {
                Self::PermanentlyQuarantined
            }
        }
    }
}

async fn reconcile_provider_settlement_job(
    job: &ProviderSettlementJob,
) -> astra_services::ServiceResult<ProviderSettlementDisposition> {
    if job
        .owner_lease
        .as_ref()
        .is_some_and(|owner_lease| owner_lease.is_lost())
    {
        // Expiry transfers authority to the database sweeper. Retrying an old
        // process generation would only create a permanent poison job.
        return Ok(ProviderSettlementDisposition::SweeperOwned);
    }
    match &job.task {
        ProviderSettlementTask::Debt {
            attempt,
            terminal,
            provider_delivery_state,
        } => {
            if let Some(attempt) = attempt.as_ref() {
                job.persistence
                    .declare_attempt_settlement(
                        &job.invocation,
                        attempt,
                        terminal,
                        *provider_delivery_state,
                    )
                    .await?;
            } else {
                job.persistence
                    .declare_settlement(&job.invocation, terminal)
                    .await?;
            }
            // Debt is the crash-safe owner, not the user-visible terminal.
            // The same fixed worker now advances this exact identity; the
            // process sweeper remains the fallback if this bounded attempt is
            // interrupted or the process exits.
            job.persistence
                .reconcile_settlement(&job.invocation, terminal)
                .await
                .map(Into::into)
        }
        ProviderSettlementTask::AdmissionUncertain => {
            let terminal = pre_provider_cancelled_terminal();
            match job
                .persistence
                .settle_uncertain_admission(&job.invocation, &terminal)
                .await?
            {
                astra_services::InferenceInvocationAdmissionResolution::Settled => {
                    Ok(ProviderSettlementDisposition::Settled)
                }
                astra_services::InferenceInvocationAdmissionResolution::ExactTerminal => {
                    Ok(ProviderSettlementDisposition::Settled)
                }
                astra_services::InferenceInvocationAdmissionResolution::ScopeUnavailable => {
                    tracing::warn!(
                        invocation_id = %job.invocation.invocation_id(),
                        "ambiguous inference admission lost its durable scope before provider delivery; deletion owns cleanup"
                    );
                    Ok(ProviderSettlementDisposition::SweeperOwned)
                }
                astra_services::InferenceInvocationAdmissionResolution::AuthorityLost => {
                    tracing::warn!(
                        invocation_id = %job.invocation.invocation_id(),
                        "ambiguous inference admission was closed after exact run authority was lost"
                    );
                    Ok(ProviderSettlementDisposition::SweeperOwned)
                }
                astra_services::InferenceInvocationAdmissionResolution::ConflictingIdentity => {
                    // A different admission token under the content-addressed
                    // invocation id proves this caller never acquired provider
                    // authority. Preserve the conflicting durable row as the
                    // incident record, but do not let it poison global capacity.
                    tracing::error!(
                        invocation_id = %job.invocation.invocation_id(),
                        "ambiguous inference admission resolved to a conflicting durable owner"
                    );
                    Ok(ProviderSettlementDisposition::PermanentlyQuarantined)
                }
            }
        }
        ProviderSettlementTask::InvocationOwnerLost { state, operations } => {
            operations.close_and_wait().await;
            let (open_attempts, terminals) = {
                let state = state.lock().await;
                let open_attempts = state
                    .open_attempts
                    .iter()
                    .map(|(attempt_index, attempt)| {
                        let delivery_state = if state.delivery_authorized.contains(attempt_index) {
                            astra_services::InferenceProviderDeliveryState::DeliveryAuthorized
                        } else {
                            astra_services::InferenceProviderDeliveryState::PreDelivery
                        };
                        let terminal = state
                            .pending_terminals
                            .get(attempt_index)
                            .cloned()
                            .unwrap_or_else(|| match delivery_state {
                                astra_services::InferenceProviderDeliveryState::PreDelivery => {
                                    pre_provider_cancelled_terminal()
                                }
                                astra_services::InferenceProviderDeliveryState::DeliveryAuthorized => {
                                    owner_lost_delivery_unknown_terminal()
                                }
                            });
                        (attempt.clone(), terminal, delivery_state)
                    })
                    .collect::<Vec<_>>();
                let terminals = state.terminals.values().cloned().collect::<Vec<_>>();
                (open_attempts, terminals)
            };
            let terminal = match open_attempts.as_slice() {
                [] => {
                    let terminal = terminals
                        .last()
                        .cloned()
                        .unwrap_or_else(pre_provider_cancelled_terminal);
                    job.persistence
                        .declare_settlement(&job.invocation, &terminal)
                        .await?;
                    terminal
                }
                [(attempt, terminal, delivery_state)] => {
                    job.persistence
                        .declare_attempt_settlement(
                            &job.invocation,
                            attempt,
                            terminal,
                            *delivery_state,
                        )
                        .await?;
                    terminal.clone()
                }
                _ => {
                    // Sequential invocation policy should keep this cardinality
                    // at one. Still converge every physical row rather than
                    // leaking them if a buggy caller violated that contract.
                    for (attempt, terminal, _) in &open_attempts {
                        job.persistence
                            .finish_provider_attempt(attempt, terminal)
                            .await?;
                    }
                    let terminal = open_attempts
                        .last()
                        .map(|(_, terminal, _)| terminal.clone())
                        .unwrap_or_else(pre_provider_cancelled_terminal);
                    job.persistence
                        .declare_settlement(&job.invocation, &terminal)
                        .await?;
                    terminal
                }
            };
            job.persistence
                .reconcile_settlement(&job.invocation, &terminal)
                .await
                .map(Into::into)
        }
    }
}

#[cfg(not(test))]
pub(crate) async fn drain_provider_settlement_coordinator(timeout: std::time::Duration) -> bool {
    let Some(coordinator) = PROVIDER_SETTLEMENT_COORDINATOR.get() else {
        return true;
    };
    coordinator.close_and_drain(timeout).await
}

#[cfg(not(test))]
pub(crate) async fn wait_for_provider_settlement_coordinator(timeout: std::time::Duration) -> bool {
    let Some(coordinator) = PROVIDER_SETTLEMENT_COORDINATOR.get() else {
        return true;
    };
    coordinator.wait_for_drain(timeout).await
}

#[cfg(test)]
pub(crate) async fn wait_for_provider_settlement_coordinator(
    _timeout: std::time::Duration,
) -> bool {
    true
}

#[cfg(test)]
pub(crate) async fn drain_provider_settlement_coordinator(_timeout: std::time::Duration) -> bool {
    true
}

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct TestInferenceLedgerPersistence {
    state: Arc<std::sync::Mutex<TestInferenceLedgerState>>,
}

#[cfg(test)]
#[derive(Default)]
struct TestInferenceLedgerState {
    invocations: BTreeMap<String, TestInvocationState>,
    attempts: BTreeMap<String, TestProviderAttemptState>,
    owner_lease_lost: bool,
    owner_renewals: u32,
}

#[cfg(test)]
#[derive(Default)]
struct TestInvocationState {
    settlement: Option<astra_services::InferenceInvocationTerminal>,
    settlement_attempt_id: Option<String>,
    settlement_delivery_state: Option<astra_services::InferenceProviderDeliveryState>,
    terminal: Option<astra_services::InferenceInvocationTerminal>,
}

#[cfg(test)]
struct TestProviderAttemptState {
    invocation_id: String,
    canonical_transition_hash: Option<String>,
    terminal: Option<astra_services::InferenceInvocationTerminal>,
}

#[cfg(test)]
impl TestInferenceLedgerPersistence {
    fn lock(&self) -> std::sync::MutexGuard<'_, TestInferenceLedgerState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn assert_quiescent(&self) {
        let state = self.lock();
        assert!(
            state
                .invocations
                .values()
                .all(|invocation| invocation.terminal.is_some()),
            "every admitted test invocation must have one logical terminal"
        );
        assert!(
            state
                .attempts
                .values()
                .all(|attempt| attempt.terminal.is_some()),
            "every admitted test provider attempt must have one terminal"
        );
    }

    fn is_quiescent(&self) -> bool {
        let state = self.lock();
        state
            .invocations
            .values()
            .all(|invocation| invocation.terminal.is_some())
            && state
                .attempts
                .values()
                .all(|attempt| attempt.terminal.is_some())
    }

    fn logical_terminal_statuses(&self) -> Vec<astra_services::InferenceTerminalStatus> {
        self.lock()
            .invocations
            .values()
            .filter_map(|invocation| invocation.terminal.as_ref().map(|terminal| terminal.status))
            .collect()
    }

    fn has_explicit_settlement_debt(&self) -> bool {
        self.lock()
            .invocations
            .values()
            .any(|invocation| invocation.settlement.is_some())
    }

    fn fence_owner_lease(&self) {
        self.lock().owner_lease_lost = true;
    }

    /// Deterministic stand-in for the process-wide inference settlement
    /// sweeper. Unit tests call this only after asserting that the foreground
    /// and bounded runtime owners have stopped, so recovery authority is the
    /// durable debt rather than a leaked task.
    fn reconcile_settlement_debts(&self) {
        let mut state = self.lock();
        let settlements = state
            .invocations
            .iter()
            .filter_map(|(invocation_id, invocation)| {
                invocation.settlement.as_ref().map(|terminal| {
                    (
                        invocation_id.clone(),
                        invocation.settlement_attempt_id.clone(),
                        terminal.clone(),
                    )
                })
            })
            .collect::<Vec<_>>();
        for (invocation_id, attempt_id, terminal) in settlements {
            if let Some(attempt_id) = attempt_id
                && let Some(attempt) = state.attempts.get_mut(&attempt_id)
            {
                attempt.terminal = Some(terminal.clone());
            }
            let has_open = state.attempts.values().any(|attempt| {
                attempt.invocation_id == invocation_id && attempt.terminal.is_none()
            });
            if !has_open {
                state
                    .invocations
                    .get_mut(&invocation_id)
                    .expect("settlement invocation remains present")
                    .terminal = Some(terminal);
            }
        }
    }
}

#[cfg(test)]
#[async_trait]
impl InferenceLedgerPersistence for TestInferenceLedgerPersistence {
    async fn renew_invocation_owner(
        &self,
        _plan: &astra_services::InferenceInvocationPlan,
    ) -> astra_services::ServiceResult<()> {
        let mut state = self.lock();
        state.owner_renewals = state.owner_renewals.saturating_add(1);
        if state.owner_lease_lost {
            Err(astra_services::ServiceError::conflict(
                "test inference owner generation was transferred",
            ))
        } else {
            Ok(())
        }
    }

    async fn admit_invocation(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
    ) -> astra_services::ServiceResult<()> {
        let mut state = self.lock();
        if state
            .invocations
            .insert(
                plan.invocation_id().to_string(),
                TestInvocationState::default(),
            )
            .is_some()
        {
            return Err(astra_services::ServiceError::conflict(format!(
                "test inference invocation {} was admitted twice",
                plan.invocation_id()
            )));
        }
        Ok(())
    }

    async fn settle_uncertain_admission(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> astra_services::ServiceResult<astra_services::InferenceInvocationAdmissionResolution> {
        let mut state = self.lock();
        match state.invocations.get(plan.invocation_id()) {
            Some(invocation) if invocation.terminal.is_some() => {
                Ok(astra_services::InferenceInvocationAdmissionResolution::ExactTerminal)
            }
            Some(_) => {
                state
                    .invocations
                    .get_mut(plan.invocation_id())
                    .expect("test invocation remains present")
                    .settlement = Some(terminal.clone());
                Ok(astra_services::InferenceInvocationAdmissionResolution::Settled)
            }
            None => {
                state.invocations.insert(
                    plan.invocation_id().to_string(),
                    TestInvocationState {
                        settlement: Some(terminal.clone()),
                        ..Default::default()
                    },
                );
                Ok(astra_services::InferenceInvocationAdmissionResolution::Settled)
            }
        }
    }

    async fn declare_settlement(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> astra_services::ServiceResult<()> {
        let mut state = self.lock();
        let invocation = state
            .invocations
            .get_mut(plan.invocation_id())
            .ok_or_else(|| {
                astra_services::ServiceError::conflict(format!(
                    "test inference invocation {} was not admitted",
                    plan.invocation_id()
                ))
            })?;
        match invocation.settlement.as_ref() {
            Some(existing) if existing != terminal => {
                return Err(astra_services::ServiceError::conflict(format!(
                    "test inference invocation {} has a conflicting settlement",
                    plan.invocation_id()
                )));
            }
            Some(_) => {}
            None => invocation.settlement = Some(terminal.clone()),
        }
        Ok(())
    }

    async fn declare_attempt_settlement(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
        attempt: &astra_services::InferenceProviderAttemptPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
        provider_delivery_state: astra_services::InferenceProviderDeliveryState,
    ) -> astra_services::ServiceResult<()> {
        self.declare_settlement(plan, terminal).await?;
        let mut state = self.lock();
        let invocation = state
            .invocations
            .get_mut(plan.invocation_id())
            .expect("test invocation exists after settlement declaration");
        match invocation.settlement_attempt_id.as_deref() {
            Some(existing) if existing != attempt.attempt_id() => {
                return Err(astra_services::ServiceError::conflict(format!(
                    "test inference invocation {} has a conflicting settlement attempt",
                    plan.invocation_id()
                )));
            }
            Some(_) => {}
            None => invocation.settlement_attempt_id = Some(attempt.attempt_id().to_string()),
        }
        match invocation.settlement_delivery_state {
            Some(existing) if existing != provider_delivery_state => {
                return Err(astra_services::ServiceError::conflict(format!(
                    "test inference invocation {} has conflicting delivery authority",
                    plan.invocation_id()
                )));
            }
            Some(_) => {}
            None => invocation.settlement_delivery_state = Some(provider_delivery_state),
        }
        Ok(())
    }

    async fn reconcile_settlement(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> astra_services::ServiceResult<astra_services::InferenceSettlementReconcileOutcome> {
        self.reconcile_settlement_debts();
        self.finish_invocation(plan, terminal).await?;
        Ok(astra_services::InferenceSettlementReconcileOutcome::Settled)
    }

    async fn finish_invocation(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> astra_services::ServiceResult<()> {
        let mut state = self.lock();
        if state.attempts.values().any(|attempt| {
            attempt.invocation_id == plan.invocation_id() && attempt.terminal.is_none()
        }) {
            return Err(astra_services::ServiceError::conflict(format!(
                "test inference invocation {} still has an open provider attempt",
                plan.invocation_id()
            )));
        }
        if terminal.status == astra_services::InferenceTerminalStatus::Succeeded
            && !state.attempts.values().any(|attempt| {
                attempt.invocation_id == plan.invocation_id()
                    && attempt.terminal.as_ref() == Some(terminal)
            })
        {
            return Err(astra_services::ServiceError::conflict(format!(
                "test inference invocation {} has no matching successful provider terminal",
                plan.invocation_id()
            )));
        }
        let invocation = state
            .invocations
            .get_mut(plan.invocation_id())
            .ok_or_else(|| {
                astra_services::ServiceError::conflict(format!(
                    "test inference invocation {} was not admitted",
                    plan.invocation_id()
                ))
            })?;
        match invocation.terminal.as_ref() {
            Some(existing) if existing != terminal => {
                return Err(astra_services::ServiceError::conflict(format!(
                    "test inference invocation {} has a conflicting terminal",
                    plan.invocation_id()
                )));
            }
            Some(_) => {}
            None => invocation.terminal = Some(terminal.clone()),
        }
        Ok(())
    }

    async fn begin_provider_attempt(
        &self,
        attempt: &astra_services::InferenceProviderAttemptPlan,
    ) -> astra_services::ServiceResult<()> {
        let mut state = self.lock();
        let invocation = state
            .invocations
            .get(attempt.invocation_id())
            .ok_or_else(|| {
                astra_services::ServiceError::conflict(format!(
                    "test provider attempt {} has no admitted invocation",
                    attempt.attempt_id()
                ))
            })?;
        if invocation.settlement.is_some() || invocation.terminal.is_some() {
            return Err(astra_services::ServiceError::conflict(format!(
                "test provider attempt {} started after settlement",
                attempt.attempt_id()
            )));
        }
        if state
            .attempts
            .insert(
                attempt.attempt_id().to_string(),
                TestProviderAttemptState {
                    invocation_id: attempt.invocation_id().to_string(),
                    canonical_transition_hash: attempt
                        .canonical_transition_hash()
                        .map(str::to_string),
                    terminal: None,
                },
            )
            .is_some()
        {
            return Err(astra_services::ServiceError::conflict(format!(
                "test provider attempt {} was admitted twice",
                attempt.attempt_id()
            )));
        }
        Ok(())
    }

    async fn finish_provider_attempt(
        &self,
        attempt: &astra_services::InferenceProviderAttemptPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> astra_services::ServiceResult<()> {
        let mut state = self.lock();
        let attempt_state = state
            .attempts
            .get_mut(attempt.attempt_id())
            .ok_or_else(|| {
                astra_services::ServiceError::conflict(format!(
                    "test provider attempt {} was not admitted",
                    attempt.attempt_id()
                ))
            })?;
        match attempt_state.terminal.as_ref() {
            Some(existing) if existing != terminal => {
                return Err(astra_services::ServiceError::conflict(format!(
                    "test provider attempt {} has a conflicting terminal",
                    attempt.attempt_id()
                )));
            }
            Some(_) => {}
            None => attempt_state.terminal = Some(terminal.clone()),
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct DurableInferenceLedger {
    persistence: Arc<dyn InferenceLedgerPersistence>,
    settlement_coordinator: Arc<ProviderSettlementCoordinator>,
    user_id: String,
    admitted_execution: astra_services::AdmittedModelExecution,
    run_authority: Option<DurableInferenceRunAuthority>,
}

struct InvocationAdmissionGuard {
    persistence: Arc<dyn InferenceLedgerPersistence>,
    settlement_coordinator: Arc<ProviderSettlementCoordinator>,
    invocation: astra_services::InferenceInvocationPlan,
    reservation: Option<ProviderSettlementReservation>,
}

impl InvocationAdmissionGuard {
    fn into_reservation(mut self) -> ProviderSettlementReservation {
        self.reservation
            .take()
            .expect("admission guard owns its reserved settlement slot")
    }
}

impl Drop for InvocationAdmissionGuard {
    fn drop(&mut self) {
        let Some(reservation) = self.reservation.take() else {
            return;
        };
        // Drop cannot await. Transfer the immutable plan and the already-held
        // capacity slot synchronously; the fixed coordinator resolves whether
        // the admission committed before any provider I/O can be authorized.
        self.settlement_coordinator.handoff(ProviderSettlementJob {
            persistence: self.persistence.clone(),
            invocation: self.invocation.clone(),
            task: ProviderSettlementTask::AdmissionUncertain,
            owner_lease: None,
            _reservation: reservation,
        });
    }
}

impl DurableInferenceLedger {
    pub(crate) fn new(
        shared_pool: SharedPool,
        user_id: impl Into<String>,
        admitted_execution: astra_services::AdmittedModelExecution,
    ) -> Self {
        Self {
            persistence: Arc::new(DatabaseInferenceLedgerPersistence { shared_pool }),
            settlement_coordinator: ProviderSettlementCoordinator::runtime(),
            user_id: user_id.into(),
            admitted_execution,
            run_authority: None,
        }
    }

    pub(crate) fn required(
        shared_pool: Option<&SharedPool>,
        admitted_execution: Option<&astra_services::AdmittedModelExecution>,
        user_id: &str,
    ) -> Result<Self, astra_core::ClassifiedError> {
        Self::required_with_persistence(shared_pool, admitted_execution, user_id, None)
    }

    pub(crate) fn required_with_persistence(
        shared_pool: Option<&SharedPool>,
        admitted_execution: Option<&astra_services::AdmittedModelExecution>,
        user_id: &str,
        persistence: Option<Arc<dyn InferenceLedgerPersistence>>,
    ) -> Result<Self, astra_core::ClassifiedError> {
        let persistence = match persistence {
            Some(persistence) => persistence,
            None => {
                let shared_pool = shared_pool.ok_or_else(|| {
                    contract_error(
                        "admission",
                        "Server execution has no durable inference database",
                    )
                })?;
                Arc::new(DatabaseInferenceLedgerPersistence {
                    shared_pool: shared_pool.clone(),
                })
            }
        };
        let admitted_execution = admitted_execution.ok_or_else(|| {
            contract_error(
                "admission",
                "Server execution has no admitted Offering material",
            )
        })?;
        Ok(Self {
            persistence,
            settlement_coordinator: ProviderSettlementCoordinator::runtime(),
            user_id: user_id.to_string(),
            admitted_execution: admitted_execution.clone(),
            run_authority: None,
        })
    }

    #[must_use]
    pub(crate) fn with_run_authority(mut self, authority: DurableInferenceRunAuthority) -> Self {
        self.run_authority = Some(authority);
        self
    }

    fn invocation_input(
        &self,
        scope: astra_turn_types::InferenceInvocationScope,
        purpose: astra_turn_types::InferencePurpose,
        resolved_model_name: &str,
        upstream_model_name: &str,
        provider: &str,
    ) -> astra_services::InferenceInvocationInput {
        astra_services::InferenceInvocationInput {
            user_id: self.user_id.clone(),
            run_authority: matches!(
                &scope,
                astra_turn_types::InferenceInvocationScope::Run { .. }
            )
            .then(|| {
                self.run_authority
                    .as_ref()
                    .map(|authority| authority.durable.clone())
            })
            .flatten(),
            scope,
            offering_id: self.admitted_execution.offering_id.clone(),
            resolved_model_name: resolved_model_name.to_string(),
            upstream_model_name: upstream_model_name.to_string(),
            provider: provider.to_string(),
            purpose,
            execution_placement: self.admitted_execution.execution_placement,
            access_kind: self.admitted_execution.access_kind,
        }
    }

    pub(crate) async fn next_logical_attempt_pair_base(
        &self,
        scope: astra_turn_types::InferenceInvocationScope,
        purpose: astra_turn_types::InferencePurpose,
        resolved_model_name: &str,
        upstream_model_name: &str,
        provider: &str,
    ) -> Result<u32, astra_core::ClassifiedError> {
        self.persistence
            .next_logical_attempt_pair_base(&self.invocation_input(
                scope,
                purpose,
                resolved_model_name,
                upstream_model_name,
                provider,
            ))
            .await
            .map_err(|error| service_error("logical attempt cursor", error))
    }

    pub(crate) async fn admit(
        &self,
        scope: astra_turn_types::InferenceInvocationScope,
        purpose: astra_turn_types::InferencePurpose,
        resolved_model_name: &str,
        upstream_model_name: &str,
        provider: &str,
    ) -> Result<DurableInferenceInvocation, DurableInferenceAdmissionFailure> {
        let mut request_context = astra_services::ModelRequestContextSeed::server_default();
        if self.admitted_execution.execution_placement
            == astra_services::ModelExecutionPlacement::Edge
        {
            request_context.topology = astra_services::ModelRequestTopology::EdgeServer;
            request_context.execution_binding = "edge".to_string();
        }
        self.admit_with_request_context(
            scope,
            purpose,
            resolved_model_name,
            upstream_model_name,
            provider,
            request_context,
        )
        .await
    }

    pub(crate) async fn admit_with_request_context(
        &self,
        scope: astra_turn_types::InferenceInvocationScope,
        purpose: astra_turn_types::InferencePurpose,
        resolved_model_name: &str,
        upstream_model_name: &str,
        provider: &str,
        request_context: astra_services::ModelRequestContextSeed,
    ) -> Result<DurableInferenceInvocation, DurableInferenceAdmissionFailure> {
        let mut authoritative_logical_attempt = scope.logical_attempt();
        let result = self
            .admit_with_request_context_inner(
                scope,
                purpose,
                resolved_model_name,
                upstream_model_name,
                provider,
                request_context,
                &mut authoritative_logical_attempt,
            )
            .await;
        result.map_err(|error| DurableInferenceAdmissionFailure {
            logical_attempt: authoritative_logical_attempt,
            error,
        })
    }

    async fn admit_with_request_context_inner(
        &self,
        scope: astra_turn_types::InferenceInvocationScope,
        purpose: astra_turn_types::InferencePurpose,
        resolved_model_name: &str,
        upstream_model_name: &str,
        provider: &str,
        request_context: astra_services::ModelRequestContextSeed,
        authoritative_logical_attempt: &mut u32,
    ) -> Result<DurableInferenceInvocation, astra_core::ClassifiedError> {
        if self.admitted_execution.model_name != resolved_model_name
            || self.admitted_execution.provider != provider
            || self
                .admitted_execution
                .wire_model_name
                .as_deref()
                .unwrap_or(&self.admitted_execution.model_name)
                != upstream_model_name
        {
            return Err(contract_error(
                "admission",
                "resolved provider route drifted from the admitted Offering",
            ));
        }
        let request_context = normalize_request_context_for_execution(
            request_context,
            self.admitted_execution.execution_placement,
        );
        let mut scope = scope;
        let mut foreground_recoveries = 0_u32;
        loop {
            if let Some(authority) = self.run_authority.as_ref()
                && let Some(error) = authority.local_fence_error("logical invocation admission")
            {
                return Err(error);
            }
            let plan = astra_services::plan_inference_invocation(self.invocation_input(
                scope.clone(),
                purpose,
                resolved_model_name,
                upstream_model_name,
                provider,
            ))
            .map_err(|error| service_error("planning", error))?;
            // Reserve reconciliation capacity before durable invocation
            // admission and therefore before any provider I/O. A recovered
            // invocation releases this exact slot before reserving the next
            // logical attempt, so ambiguity cannot amplify global capacity.
            let admission_owner = SettlementAdmissionOwner::new(&self.user_id, &scope);
            let reservation_future = self.settlement_coordinator.reserve(admission_owner);
            tokio::pin!(reservation_future);
            let settlement_reservation = match self.run_authority.as_ref() {
                Some(authority) => tokio::select! {
                    biased;
                    reservation = &mut reservation_future => reservation?,
                    error = authority.wait_for_local_fence("settlement capacity admission") => {
                        return Err(error);
                    }
                },
                None => reservation_future.await?,
            };
            let admission_guard = InvocationAdmissionGuard {
                persistence: self.persistence.clone(),
                settlement_coordinator: self.settlement_coordinator.clone(),
                invocation: plan.clone(),
                reservation: Some(settlement_reservation),
            };
            let admission_started = std::time::Instant::now();
            let admission = {
                let admission_future = tokio::time::timeout(
                    detached_reconciliation_timeout(),
                    self.persistence.admit_invocation(&plan),
                );
                tokio::pin!(admission_future);
                match self.run_authority.as_ref() {
                    Some(authority) => tokio::select! {
                        biased;
                        result = &mut admission_future => result,
                        error = authority.wait_for_local_fence("logical invocation admission") => {
                            return Err(error);
                        }
                    },
                    None => admission_future.as_mut().await,
                }
            };
            // A database result and a local authority fence can become ready in
            // the same scheduler turn.  Re-check after the select so a timeout
            // or late admission ACK cannot outrank cancellation/lease loss and
            // accidentally authorize provider delivery.
            if let Some(authority) = self.run_authority.as_ref()
                && let Some(error) = authority.local_fence_error("logical invocation admission")
            {
                return Err(error);
            }
            let uncertainty = match admission {
                Ok(Ok(())) => {
                    tracing::debug!(
                        stage = "logical_invocation_admission",
                        outcome = "admitted",
                        invocation_id = %plan.invocation_id(),
                        logical_attempt = plan.logical_attempt(),
                        elapsed_ms = admission_started.elapsed().as_millis(),
                        "durable inference admission stage completed"
                    );
                    let settlement_reservation = Arc::new(std::sync::Mutex::new(Some(
                        admission_guard.into_reservation(),
                    )));
                    let owner_lease = InferenceOwnerLease::start(
                        self.persistence.clone(),
                        plan.clone(),
                        self.run_authority
                            .as_ref()
                            .and_then(|authority| authority.cancel_token.as_deref()),
                    );
                    return Ok(DurableInferenceInvocation {
                        observer: Arc::new(
                            DurableProviderAttemptObserver::new_with_persistence_and_coordinator(
                                self.persistence.clone(),
                                plan.clone(),
                                request_context.clone(),
                                self.settlement_coordinator.clone(),
                                settlement_reservation.clone(),
                                owner_lease.clone(),
                            ),
                        ),
                        persistence: self.persistence.clone(),
                        plan: plan.clone(),
                        settlement_coordinator: self.settlement_coordinator.clone(),
                        owner_lease,
                    });
                }
                Ok(Err(error))
                    if matches!(
                        error.kind,
                        astra_services::ServiceErrorKind::Persistence
                            | astra_services::ServiceErrorKind::ConflictTransient
                    ) =>
                {
                    tracing::warn!(
                        stage = "logical_invocation_admission",
                        outcome = "ambiguous_error",
                        service_error_kind = error.kind.as_str(),
                        invocation_id = %plan.invocation_id(),
                        logical_attempt = plan.logical_attempt(),
                        elapsed_ms = admission_started.elapsed().as_millis(),
                        %error,
                        "durable inference admission needs foreground ambiguity resolution"
                    );
                    "persistence_error"
                }
                Ok(Err(error)) => {
                    // Only persistence/transient-conflict failures are commit
                    // ambiguous. A typed rejection is authoritative and must
                    // release its reservation directly; enqueuing settlement
                    // for a definitively absent row would consume global
                    // recovery capacity and could later manufacture a stale
                    // pre-provider identity after scope authority changes.
                    drop(admission_guard.into_reservation());
                    return Err(service_error("admission", error));
                }
                Err(_) => {
                    tracing::warn!(
                        stage = "logical_invocation_admission",
                        outcome = "timeout",
                        invocation_id = %plan.invocation_id(),
                        logical_attempt = plan.logical_attempt(),
                        elapsed_ms = admission_started.elapsed().as_millis(),
                        timeout_ms = detached_reconciliation_timeout().as_millis(),
                        "durable inference admission needs foreground ambiguity resolution"
                    );
                    "timeout"
                }
            };

            let recovery_started = std::time::Instant::now();
            let recovery_terminal = pre_provider_cancelled_terminal();
            let recovery = {
                let recovery_future = tokio::time::timeout(
                    detached_reconciliation_timeout(),
                    self.persistence
                        .settle_uncertain_admission(&plan, &recovery_terminal),
                );
                tokio::pin!(recovery_future);
                match self.run_authority.as_ref() {
                    Some(authority) => tokio::select! {
                        biased;
                        result = &mut recovery_future => result,
                        error = authority.wait_for_local_fence("logical invocation admission recovery") => {
                            return Err(error);
                        }
                    },
                    None => recovery_future.as_mut().await,
                }
            };
            // Recovery is only an admission fact, not renewed run authority.
            // Preserve the same local fence at the recovery linearization
            // boundary before considering a replacement identity.
            if let Some(authority) = self.run_authority.as_ref()
                && let Some(error) =
                    authority.local_fence_error("logical invocation admission recovery")
            {
                return Err(error);
            }
            let resolution = match recovery {
                Ok(Ok(resolution)) => resolution,
                Ok(Err(error)) => {
                    tracing::warn!(
                        stage = "logical_invocation_admission_recovery",
                        outcome = "error",
                        admission_uncertainty = uncertainty,
                        invocation_id = %plan.invocation_id(),
                        logical_attempt = plan.logical_attempt(),
                        elapsed_ms = recovery_started.elapsed().as_millis(),
                        %error,
                        "foreground admission ambiguity resolution failed; bounded coordinator retains ownership"
                    );
                    return Err(service_error(
                        "logical invocation admission recovery",
                        error,
                    ));
                }
                Err(_) => {
                    tracing::warn!(
                        stage = "logical_invocation_admission_recovery",
                        outcome = "timeout",
                        admission_uncertainty = uncertainty,
                        invocation_id = %plan.invocation_id(),
                        logical_attempt = plan.logical_attempt(),
                        elapsed_ms = recovery_started.elapsed().as_millis(),
                        timeout_ms = detached_reconciliation_timeout().as_millis(),
                        "foreground admission ambiguity resolution timed out; bounded coordinator retains ownership"
                    );
                    return Err(ledger_timeout_error_for_stage(
                        "logical_invocation_admission_recovery",
                    ));
                }
            };

            match resolution {
                astra_services::InferenceInvocationAdmissionResolution::Settled
                | astra_services::InferenceInvocationAdmissionResolution::ExactTerminal => {}
                astra_services::InferenceInvocationAdmissionResolution::ScopeUnavailable => {
                    // The recovery transaction conclusively proved that this
                    // caller no longer owns a live inference scope. Do not
                    // enqueue redundant work or create a replacement scope.
                    drop(admission_guard.into_reservation());
                    return Err(contract_error(
                        "logical invocation admission recovery",
                        "durable scope authority was lost before provider delivery",
                    ));
                }
                astra_services::InferenceInvocationAdmissionResolution::AuthorityLost => {
                    drop(admission_guard.into_reservation());
                    return Err(contract_error(
                        "logical invocation admission recovery",
                        "durable run authority was lost before replacement provider delivery",
                    ));
                }
                astra_services::InferenceInvocationAdmissionResolution::ConflictingIdentity => {
                    // Another fencing token owns the content-addressed row.
                    // Delivery by this caller would no longer be exact-once.
                    drop(admission_guard.into_reservation());
                    return Err(contract_error(
                        "logical invocation admission recovery",
                        "durable admission authority belongs to a conflicting owner",
                    ));
                }
            }
            tracing::info!(
                stage = "logical_invocation_admission_recovery",
                outcome = "settled_pre_provider",
                admission_uncertainty = uncertainty,
                resolution = ?resolution,
                invocation_id = %plan.invocation_id(),
                logical_attempt = plan.logical_attempt(),
                elapsed_ms = recovery_started.elapsed().as_millis(),
                "foreground admission ambiguity resolved without provider delivery"
            );
            // Exact pre-provider terminal authority is now durable. Releasing
            // the original reservation before retrying prevents one caller
            // from consuming two coordinator slots under high concurrency.
            drop(admission_guard.into_reservation());
            if foreground_recoveries >= MAX_FOREGROUND_ADMISSION_RECOVERIES {
                return Err(ledger_timeout_error_for_stage(
                    "logical_invocation_retry_admission",
                ));
            }
            let next_logical_attempt = scope.logical_attempt().checked_add(1).ok_or_else(|| {
                contract_error(
                    "logical invocation admission recovery",
                    "logical_attempt overflowed while advancing recovered authority",
                )
            })?;
            foreground_recoveries += 1;
            tracing::info!(
                stage = "logical_invocation_admission_retry",
                outcome = "retrying",
                previous_logical_attempt = scope.logical_attempt(),
                logical_attempt = next_logical_attempt,
                "retrying the same inference round under a fresh durable identity"
            );
            scope = scope.with_logical_attempt(next_logical_attempt);
            *authoritative_logical_attempt = next_logical_attempt;
        }
    }

    pub(crate) async fn execute_nonstream(
        &self,
        client: &reqwest::Client,
        scope: astra_turn_types::InferenceInvocationScope,
        call: LlmCall<'_>,
        timeout: std::time::Duration,
    ) -> DurableInferenceCallOutcome {
        let invocation = match self
            .admit(
                scope,
                call.purpose,
                call.route.model_name,
                call.route.wire_model_name.unwrap_or(call.route.model_name),
                call.route.provider,
            )
            .await
        {
            Ok(invocation) => invocation,
            Err(failure) => {
                return DurableInferenceCallOutcome {
                    logical_attempt: failure.logical_attempt,
                    result: Err(failure.error),
                };
            }
        };
        let logical_attempt = invocation.logical_attempt();
        let result = async {
            let owner_lease = invocation.owner_lease.clone();
            let attempt_observer = invocation.attempt_observer_arc();
            let settlement = NonstreamInvocationSupervisor::start(Arc::new(invocation));
            let provider = crate::turn::llm::client::call_llm_nonstream_with_attempt_observer(
                client,
                call,
                timeout,
                Some(attempt_observer.as_ref()),
            );
            let result = wait_for_provider_or_owner_cancel(provider, owner_lease).await;
            match result {
                Ok(result) => {
                    if let Err(e) = settlement
                        .settle(NonstreamSettlementCommand::Terminal(terminal_from_result(
                            &result,
                        )))
                        .await
                    {
                        tracing::error!(
                            ?result.response_id,
                            %e,
                            "LLM call succeeded and its provider attempt terminal was recorded, but logical invocation settlement failed"
                        );
                        return Err(e);
                    }
                    Ok(result)
                }
                Err(error) => {
                    let command = if is_ledger_error(&error) {
                        // The foreground deadline does not cancel the detached
                        // provider-attempt owner. Hand logical settlement to the
                        // same supervisor: it waits for that owner, preserves an
                        // exact late terminal when one exists, and otherwise
                        // records an explicit delivery-unknown settlement debt.
                        NonstreamSettlementCommand::DeliveryUnknown(
                            ledger_reconciliation_terminal(&error),
                        )
                    } else if matches!(
                        error.kind,
                        astra_core::ErrorKind::Cancelled
                            | astra_core::ErrorKind::BudgetExhausted
                            | astra_core::ErrorKind::ProviderDeadline
                    ) {
                        NonstreamSettlementCommand::DeliveryUnknown(
                            delivery_unknown_terminal_from_error(&error),
                        )
                    } else {
                        NonstreamSettlementCommand::Terminal(terminal_from_error(&error))
                    };
                    if matches!(&command, NonstreamSettlementCommand::DeliveryUnknown(_)) {
                        settlement.handoff(command)?;
                        return Err(error);
                    }
                    if let Err(e) = settlement.settle(command).await {
                        tracing::error!(
                            %error,
                            %e,
                            "LLM call failed and its provider attempt terminal was recorded, but logical invocation settlement failed"
                        );
                        return Err(e);
                    }
                    Err(error)
                }
            }
        }
        .await;
        DurableInferenceCallOutcome {
            logical_attempt,
            result,
        }
    }

    /// Execute a bounded auxiliary call through the same streaming transport
    /// used by the interactive agent.  Some providers deliver a short
    /// response promptly over SSE but hold a non-stream response until a
    /// longer server-side completion window; semantic admission must not
    /// inherit that tail latency.  The durable invocation/attempt lifecycle
    /// remains identical to [`Self::execute_nonstream`].
    pub(crate) async fn execute_stream_no_tool_choice(
        &self,
        scope: astra_turn_types::InferenceInvocationScope,
        call: LlmCall<'_>,
    ) -> DurableInferenceCallOutcome {
        let invocation = match self
            .admit(
                scope,
                call.purpose,
                call.route.model_name,
                call.route.wire_model_name.unwrap_or(call.route.model_name),
                call.route.provider,
            )
            .await
        {
            Ok(invocation) => invocation,
            Err(failure) => {
                return DurableInferenceCallOutcome {
                    logical_attempt: failure.logical_attempt,
                    result: Err(failure.error),
                };
            }
        };
        let logical_attempt = invocation.logical_attempt();
        let result = async {
            let owner_cancel = invocation.owner_lease.cancel.clone();
            let attempt_observer = invocation.attempt_observer_arc();
            let settlement = NonstreamInvocationSupervisor::start(Arc::new(invocation));
            let cancel_flag = self
                .run_authority
                .as_ref()
                .and_then(|authority| authority.cancel_flag.as_deref());
            let cancel = match cancel_flag {
                Some(flag) => LlmCancel::FlagAndToken(flag, &owner_cancel),
                None => LlmCancel::Token(&owner_cancel),
            };
            let result =
                crate::turn::llm::client::call_llm_and_collect_with_stream_callback_and_no_tool_choice(
                    call,
                    cancel,
                    None,
                    Some(attempt_observer.as_ref()),
                )
                .await
            ;
            match result {
                Ok(result) => {
                    settlement
                        .settle(NonstreamSettlementCommand::Terminal(terminal_from_result(
                            &result,
                        )))
                        .await?;
                    Ok(result)
                }
                Err(error) => {
                    let command = if is_ledger_error(&error) {
                        NonstreamSettlementCommand::DeliveryUnknown(ledger_reconciliation_terminal(
                            &error,
                        ))
                    } else if matches!(
                        error.kind,
                        astra_core::ErrorKind::Cancelled
                            | astra_core::ErrorKind::BudgetExhausted
                            | astra_core::ErrorKind::ProviderDeadline
                    ) {
                        NonstreamSettlementCommand::DeliveryUnknown(
                            delivery_unknown_terminal_from_error(&error),
                        )
                    } else {
                        NonstreamSettlementCommand::Terminal(terminal_from_error(&error))
                    };
                    if matches!(&command, NonstreamSettlementCommand::DeliveryUnknown(_)) {
                        settlement.handoff(command)?;
                        return Err(error);
                    }
                    settlement.settle(command).await?;
                    Err(error)
                }
            }
        }
        .await;
        DurableInferenceCallOutcome {
            logical_attempt,
            result,
        }
    }

    #[cfg(test)]
    async fn execute_stream_with_total_budget_for_test(
        &self,
        scope: astra_turn_types::InferenceInvocationScope,
        call: LlmCall<'_>,
        total_budget: std::time::Duration,
    ) -> Result<LlmCallResult, astra_core::ClassifiedError> {
        let invocation = self
            .admit(
                scope,
                call.purpose,
                call.route.model_name,
                call.route.wire_model_name.unwrap_or(call.route.model_name),
                call.route.provider,
            )
            .await
            .map_err(|failure| failure.error)?;
        let attempt_observer = invocation.attempt_observer_arc();
        let settlement = NonstreamInvocationSupervisor::start(Arc::new(invocation));
        let result = crate::turn::llm::client::call_llm_and_collect_with_total_budget_for_test(
            call,
            Some(attempt_observer.as_ref()),
            total_budget,
        )
        .await;
        match result {
            Ok(result) => {
                settlement
                    .settle(NonstreamSettlementCommand::Terminal(terminal_from_result(
                        &result,
                    )))
                    .await?;
                Ok(result)
            }
            Err(error) => {
                let command = if is_ledger_error(&error) {
                    NonstreamSettlementCommand::DeliveryUnknown(ledger_reconciliation_terminal(
                        &error,
                    ))
                } else if matches!(
                    error.kind,
                    astra_core::ErrorKind::Cancelled
                        | astra_core::ErrorKind::BudgetExhausted
                        | astra_core::ErrorKind::ProviderDeadline
                ) {
                    NonstreamSettlementCommand::DeliveryUnknown(
                        delivery_unknown_terminal_from_error(&error),
                    )
                } else {
                    NonstreamSettlementCommand::Terminal(terminal_from_error(&error))
                };
                if matches!(&command, NonstreamSettlementCommand::DeliveryUnknown(_)) {
                    settlement.handoff(command)?;
                    return Err(error);
                }
                settlement.settle(command).await?;
                Err(error)
            }
        }
    }
}

fn normalize_request_context_for_execution(
    mut context: astra_services::ModelRequestContextSeed,
    placement: astra_services::ModelExecutionPlacement,
) -> astra_services::ModelRequestContextSeed {
    context.execution_binding = match placement {
        astra_services::ModelExecutionPlacement::Server => "server",
        astra_services::ModelExecutionPlacement::Edge => "edge",
    }
    .to_string();
    if placement == astra_services::ModelExecutionPlacement::Edge
        && context.topology == astra_services::ModelRequestTopology::ServerOnly
    {
        context.topology = astra_services::ModelRequestTopology::EdgeServer;
    }
    context
}

#[async_trait]
trait NonstreamInvocationSettlement: Send + Sync + 'static {
    async fn settle_terminal(
        &self,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> Result<(), astra_core::ClassifiedError>;

    async fn settle_caller_drop(&self) -> Result<(), astra_core::ClassifiedError>;

    async fn settle_delivery_unknown(
        &self,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> Result<(), astra_core::ClassifiedError>;
}

enum NonstreamSettlementCommand {
    Terminal(astra_services::InferenceInvocationTerminal),
    DeliveryUnknown(astra_services::InferenceInvocationTerminal),
}

/// Owns logical settlement independently of the caller future.
///
/// Dropping the caller closes `command_tx`, but Tokio keeps the detached task
/// alive so it can converge the durable attempt and invocation. Once a normal
/// provider outcome is sent, the same task remains the sole terminal writer
/// even if the caller is cancelled while awaiting the durable commit.
struct NonstreamInvocationSupervisor {
    command_tx: Option<tokio::sync::oneshot::Sender<NonstreamSettlementCommand>>,
    task: tokio::task::JoinHandle<Result<(), astra_core::ClassifiedError>>,
}

impl NonstreamInvocationSupervisor {
    fn start(owner: Arc<dyn NonstreamInvocationSettlement>) -> Self {
        let (command_tx, command_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            match command_rx.await {
                Ok(NonstreamSettlementCommand::Terminal(terminal)) => {
                    owner.settle_terminal(&terminal).await
                }
                Ok(NonstreamSettlementCommand::DeliveryUnknown(terminal)) => {
                    let result = owner.settle_delivery_unknown(&terminal).await;
                    if let Err(error) = &result {
                        tracing::error!(
                            %error,
                            "detached delivery-unknown inference settlement failed"
                        );
                    }
                    result
                }
                Err(_) => {
                    let result = owner.settle_caller_drop().await;
                    if let Err(error) = &result {
                        tracing::error!(
                            %error,
                            "detached non-streaming inference settlement failed after caller cancellation"
                        );
                    }
                    result
                }
            }
        });
        Self {
            command_tx: Some(command_tx),
            task,
        }
    }

    async fn settle(
        mut self,
        command: NonstreamSettlementCommand,
    ) -> Result<(), astra_core::ClassifiedError> {
        let command_tx = self.command_tx.take().ok_or_else(|| {
            contract_error("settlement", "non-streaming invocation already settled")
        })?;
        command_tx.send(command).map_err(|_| {
            contract_error(
                "settlement",
                "non-streaming settlement owner stopped before receiving its terminal",
            )
        })?;
        self.task.await.map_err(|error| {
            contract_error(
                "settlement",
                format!("non-streaming settlement owner failed: {error}"),
            )
        })?
    }

    fn handoff(
        mut self,
        command: NonstreamSettlementCommand,
    ) -> Result<(), astra_core::ClassifiedError> {
        let command_tx = self.command_tx.take().ok_or_else(|| {
            contract_error("settlement", "non-streaming invocation already settled")
        })?;
        command_tx.send(command).map_err(|_| {
            contract_error(
                "settlement",
                "non-streaming settlement owner stopped before receiving its terminal",
            )
        })?;
        // Dropping a Tokio JoinHandle detaches rather than aborts the task.
        // The supervisor retains the invocation owner and converges durable
        // state without extending the caller's already-expired budget.
        drop(self.task);
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct DurableInferenceInvocation {
    persistence: Arc<dyn InferenceLedgerPersistence>,
    plan: astra_services::InferenceInvocationPlan,
    observer: Arc<DurableProviderAttemptObserver>,
    settlement_coordinator: Arc<ProviderSettlementCoordinator>,
    owner_lease: Arc<InferenceOwnerLease>,
}

/// Exact identity of the latest durably admitted physical provider request.
///
/// This is the bridge between transport-owned serialized bytes and the
/// turn-level context trace. It is populated only after the attempt row
/// commits, so a trace can never claim that an unadmitted request was sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DurableProviderRequestIdentity {
    pub request_id: String,
    pub request_hash: String,
    pub attempt: u32,
    pub protocol: crate::turn::llm::client::LlmProviderProtocol,
    pub provider_wire_bytes: u64,
    pub composition: crate::turn::llm::client::ProviderWireComposition,
    pub fingerprints: crate::turn::llm::client::ProviderWireFingerprints,
}

/// One admitted physical request and its terminal fact, when observed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DurableProviderAttemptFact {
    pub request: DurableProviderRequestIdentity,
    /// Runtime transport observation for this physical attempt. Admission and
    /// dispatch are separate facts; one retry starting transport must not make
    /// another merely prepared request look sent.
    pub dispatch_started: bool,
    pub terminal: Option<astra_services::InferenceInvocationTerminal>,
}

impl DurableInferenceInvocation {
    /// Authoritative logical attempt selected by durable admission.
    ///
    /// This can be exactly one greater than the requested attempt when the
    /// original admission timed out, was conclusively settled pre-provider,
    /// and foreground recovery admitted a fresh identity.
    pub(crate) fn logical_attempt(&self) -> u32 {
        self.plan.logical_attempt()
    }

    pub(crate) fn attempt_observer(&self) -> &dyn ProviderAttemptObserver {
        self.observer.as_ref()
    }

    pub(crate) fn attempt_observer_arc(&self) -> Arc<dyn ProviderAttemptObserver> {
        self.observer.clone()
    }

    /// Bind the canonical append WAL before the first physical attempt is
    /// admitted. The observer carries it unchanged into the same transaction
    /// that fences the exact provider body.
    pub(crate) fn bind_provider_canonical_transitions(
        &self,
        transitions: Vec<astra_turn_types::ProviderCanonicalTransitionV2>,
    ) -> Result<(), astra_core::ClassifiedError> {
        for transition in &transitions {
            transition.validate().map_err(|error| {
                contract_error(
                    "canonical transition binding",
                    format!("invalid transition: {error}"),
                )
            })?;
        }
        if self.observer.next_attempt.load(Ordering::Acquire) != 0 {
            return Err(contract_error(
                "canonical transition binding",
                "provider attempt admission already started",
            ));
        }
        let mut bound = self
            .observer
            .canonical_transitions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !bound.is_empty() && *bound != transitions {
            return Err(contract_error(
                "canonical transition binding",
                "a different transition set is already bound",
            ));
        }
        *bound = transitions;
        Ok(())
    }

    pub(crate) async fn provider_attempt_facts(&self) -> Vec<DurableProviderAttemptFact> {
        let dispatched_attempts = self
            .observer
            .dispatched_attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        self.observer
            .state
            .lock()
            .await
            .attempt_facts(&dispatched_attempts)
    }

    /// Transition id acknowledged by the same durable transaction that
    /// admitted a physical provider attempt. Prepared/request-observer state is
    /// deliberately not sufficient authority to advance the runtime WAL head.
    pub(crate) fn admitted_canonical_transition_id(&self) -> Option<String> {
        self.observer
            .admitted_canonical_transition_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    #[must_use]
    pub(crate) fn provider_dispatch_started(&self) -> bool {
        self.observer.dispatch_started.load(Ordering::Acquire)
    }

    async fn take_settlement_reservation(
        &self,
        stage: &'static str,
    ) -> Result<ProviderSettlementReservation, astra_core::ClassifiedError> {
        self.observer
            .settlement_reservation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .ok_or_else(|| contract_error(stage, "invocation settlement reservation is absent"))
    }

    async fn handoff_logical_settlement(
        &self,
        terminal: astra_services::InferenceInvocationTerminal,
    ) -> Result<(), astra_core::ClassifiedError> {
        let reservation = self
            .take_settlement_reservation("logical settlement handoff")
            .await?;
        self.settlement_coordinator.handoff(ProviderSettlementJob {
            persistence: self.persistence.clone(),
            invocation: self.plan.clone(),
            task: ProviderSettlementTask::Debt {
                attempt: Box::new(None),
                terminal,
                provider_delivery_state:
                    astra_services::InferenceProviderDeliveryState::PreDelivery,
            },
            owner_lease: Some(self.owner_lease.clone()),
            _reservation: reservation,
        });
        Ok(())
    }

    /// Close every physical attempt that was admitted but has not reached a
    /// durable terminal yet.
    ///
    /// This is used by the client-disconnect supervisor after the response
    /// stream itself has been dropped. The supervisor cannot claim that the
    /// provider did not receive the request, so callers must pass a
    /// `delivery_unknown` terminal rather than inventing success or failure.
    pub(crate) async fn finish_open_attempts(
        &self,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> Result<(), astra_core::ClassifiedError> {
        self.persistence
            .declare_settlement(&self.plan, terminal)
            .await
            .map_err(|error| service_error("settlement declaration", error))?;
        self.observer.finish_open_attempts(terminal).await
    }

    /// Converge a logical invocation after its response consumer disappears.
    ///
    /// An open provider attempt is necessarily delivery-unknown. If the
    /// physical attempt already reached a durable terminal, preserve that exact
    /// terminal (including usage and response id). If provider I/O never began,
    /// the logical invocation is simply cancelled.
    pub(crate) async fn finish_after_disconnect(
        &self,
        delivery_unknown: &astra_services::InferenceInvocationTerminal,
    ) -> Result<(), astra_core::ClassifiedError> {
        self.finish_after_disconnect_with_partial_provider_facts(
            delivery_unknown,
            astra_services::InferenceUsage::default(),
            None,
        )
        .await
    }

    pub(crate) async fn finish_after_disconnect_with_partial_provider_facts(
        &self,
        delivery_unknown: &astra_services::InferenceInvocationTerminal,
        usage: astra_services::InferenceUsage,
        provider_response_id: Option<String>,
    ) -> Result<(), astra_core::ClassifiedError> {
        let mut delivery_unknown = delivery_unknown.clone();
        delivery_unknown.usage = usage;
        delivery_unknown.provider_response_id = provider_response_id;
        let logical_terminal = self
            .observer
            .terminal_after_disconnect(&delivery_unknown)
            .await?;
        match logical_terminal {
            Some(logical_terminal) => self.finish(&logical_terminal).await,
            // An exact attempt+logical settlement debt is now durable. The
            // process-wide inference settlement sweeper is the sole remaining
            // owner; the foreground must not wait for a stalled database write.
            None => Ok(()),
        }
    }

    pub(crate) async fn finish_result(
        &self,
        result: &LlmCallResult,
    ) -> Result<(), astra_core::ClassifiedError> {
        self.finish(&terminal_from_result(result)).await
    }

    pub(crate) async fn finish_error(
        &self,
        error: &astra_core::ClassifiedError,
    ) -> Result<(), astra_core::ClassifiedError> {
        if is_ledger_error(error) {
            // The detached provider-attempt operation still owns its exact
            // terminal. Keep the foreground bounded, but transfer logical
            // settlement to another owner instead of abandoning the admitted
            // invocation. Once the operation gate drains, that owner preserves
            // an exact late terminal or fences the invocation with an explicit
            // delivery-unknown settlement debt.
            let owner = self.clone();
            let terminal = ledger_reconciliation_terminal(error);
            tokio::spawn(async move {
                if let Err(settlement_error) = owner.finish_after_disconnect(&terminal).await {
                    tracing::error!(
                        %settlement_error,
                        "detached inference-ledger settlement failed"
                    );
                }
            });
            return Ok(());
        }
        if matches!(
            error.kind,
            astra_core::ErrorKind::Cancelled
                | astra_core::ErrorKind::BudgetExhausted
                | astra_core::ErrorKind::ProviderDeadline
        ) {
            let owner = self.clone();
            let terminal = delivery_unknown_terminal_from_error(error);
            tokio::spawn(async move {
                if let Err(settlement_error) = owner.finish_after_disconnect(&terminal).await {
                    tracing::error!(
                        %settlement_error,
                        "detached inference control-error settlement failed"
                    );
                }
            });
            return Ok(());
        }
        self.finish_error_with_partial_provider_facts(
            error,
            astra_services::InferenceUsage::default(),
            None,
        )
        .await
    }

    pub(crate) async fn finish_error_with_partial_provider_facts(
        &self,
        error: &astra_core::ClassifiedError,
        usage: astra_services::InferenceUsage,
        provider_response_id: Option<String>,
    ) -> Result<(), astra_core::ClassifiedError> {
        let mut fallback = if is_ledger_error(error) {
            ledger_reconciliation_terminal(error)
        } else {
            terminal_from_error(error)
        };
        fallback.usage = usage;
        fallback.provider_response_id = provider_response_id;
        let observed_terminal = {
            let state = self.observer.state.lock().await;
            state.quiescent_terminal()
        };
        if let Some(terminal) = observed_terminal {
            // The transport already committed the physical terminal before
            // surfacing the error to its caller. Preserve its measured usage
            // and provider response identity instead of replacing those facts
            // with a zero-usage wrapper error.
            return self.finish(&terminal).await;
        }
        if let Err(error) = self.finish_open_attempts(&fallback).await {
            tracing::warn!(
                %error,
                "provider terminal mirror failed; transferring its exact outcome to the bounded settlement coordinator"
            );
            let logical_terminal = self.observer.terminal_after_disconnect(&fallback).await?;
            return match logical_terminal {
                Some(logical_terminal) => self.finish(&logical_terminal).await,
                None => Ok(()),
            };
        }
        self.finish(&fallback).await
    }

    pub(crate) async fn finish(
        &self,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> Result<(), astra_core::ClassifiedError> {
        self.owner_lease.ensure_live("logical terminal")?;
        // Successful provider terminalization atomically creates the matching
        // debt in the services layer. Other terminal kinds are retryable at
        // the physical layer, so only the logical owner may declare them final.
        // Establish that recovery owner before the logical mirror performs any
        // fallible reads.
        if terminal.status != astra_services::InferenceTerminalStatus::Succeeded {
            match tokio::time::timeout(
                detached_reconciliation_timeout(),
                self.persistence.declare_settlement(&self.plan, terminal),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(
                        %error,
                        "logical settlement declaration failed; transferring its reserved owner"
                    );
                    return self.handoff_logical_settlement(terminal.clone()).await;
                }
                Err(_) => {
                    tracing::warn!(
                        timeout_ms = detached_reconciliation_timeout().as_millis(),
                        "logical settlement declaration timed out; transferring its reserved owner"
                    );
                    return self.handoff_logical_settlement(terminal.clone()).await;
                }
            }
        }
        // Success already has an exact debt from the atomic physical terminal
        // transaction; non-success established its logical debt above. The
        // bounded mirror may fail without losing the global recovery owner.
        let result = tokio::time::timeout(
            detached_reconciliation_timeout(),
            self.persistence.finish_invocation(&self.plan, terminal),
        )
        .await;
        drop(
            self.take_settlement_reservation("logical terminal mirror")
                .await?,
        );
        match result {
            Ok(Ok(())) => {
                self.owner_lease.stop();
                Ok(())
            }
            Ok(Err(error)) => Err(service_error("terminal commit", error)),
            Err(_) => Err(ledger_timeout_error_for_stage("logical_terminal_mirror")),
        }
    }
}

#[async_trait]
impl NonstreamInvocationSettlement for DurableInferenceInvocation {
    async fn settle_terminal(
        &self,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> Result<(), astra_core::ClassifiedError> {
        self.finish(terminal).await
    }

    async fn settle_caller_drop(&self) -> Result<(), astra_core::ClassifiedError> {
        let delivery_unknown = terminal_from_error(&astra_core::ClassifiedError::new(
            astra_core::ErrorKind::StreamTransport,
            "Non-streaming inference caller stopped after durable admission",
        ));
        self.finish_after_disconnect(&delivery_unknown).await
    }

    async fn settle_delivery_unknown(
        &self,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> Result<(), astra_core::ClassifiedError> {
        self.finish_after_disconnect(terminal).await
    }
}

struct DurableProviderAttemptObserver {
    persistence: Arc<dyn InferenceLedgerPersistence>,
    settlement_coordinator: Arc<ProviderSettlementCoordinator>,
    settlement_reservation: Arc<std::sync::Mutex<Option<ProviderSettlementReservation>>>,
    invocation: astra_services::InferenceInvocationPlan,
    request_context: astra_services::ModelRequestContextSeed,
    canonical_transitions: std::sync::Mutex<Vec<astra_turn_types::ProviderCanonicalTransitionV2>>,
    admitted_canonical_transition_id: std::sync::Mutex<Option<String>>,
    next_attempt: AtomicU32,
    dispatch_started: AtomicBool,
    dispatched_attempts: std::sync::Mutex<BTreeSet<u32>>,
    state: Arc<tokio::sync::Mutex<ProviderAttemptState>>,
    operations: ProviderOperationGate,
    owner_lease: Arc<InferenceOwnerLease>,
}

#[derive(Default)]
struct ProviderAttemptState {
    open_attempts: BTreeMap<u32, astra_services::InferenceProviderAttemptPlan>,
    requests: BTreeMap<u32, DurableProviderRequestIdentity>,
    delivery_authorized: BTreeSet<u32>,
    settlement_handed_off: BTreeSet<u32>,
    pending_terminals: BTreeMap<u32, astra_services::InferenceInvocationTerminal>,
    terminals: BTreeMap<u32, astra_services::InferenceInvocationTerminal>,
}

impl ProviderAttemptState {
    fn attempt_facts(
        &self,
        dispatched_attempts: &BTreeSet<u32>,
    ) -> Vec<DurableProviderAttemptFact> {
        self.requests
            .iter()
            .map(|(attempt, request)| DurableProviderAttemptFact {
                request: request.clone(),
                dispatch_started: dispatched_attempts.contains(attempt),
                terminal: self.terminals.get(attempt).cloned(),
            })
            .collect()
    }

    fn quiescent_terminal(&self) -> Option<astra_services::InferenceInvocationTerminal> {
        self.open_attempts
            .is_empty()
            .then(|| {
                self.terminals
                    .last_key_value()
                    .map(|(_, terminal)| terminal.clone())
            })
            .flatten()
    }
}

#[derive(Clone, Default)]
struct ProviderOperationGate {
    state: Arc<std::sync::Mutex<ProviderOperationGateState>>,
    drained: Arc<tokio::sync::Notify>,
}

#[derive(Default)]
struct ProviderOperationGateState {
    closed: bool,
    active: u32,
}

struct ProviderOperationPermit {
    gate: ProviderOperationGate,
}

impl ProviderOperationGate {
    fn register(
        &self,
        stage: &'static str,
    ) -> Result<ProviderOperationPermit, astra_core::ClassifiedError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return Err(contract_error(
                stage,
                "provider attempt settlement has already started",
            ));
        }
        state.active = state
            .active
            .checked_add(1)
            .ok_or_else(|| contract_error(stage, "provider operation count overflow"))?;
        Ok(ProviderOperationPermit { gate: self.clone() })
    }

    async fn close_and_wait(&self) {
        loop {
            let drained = self.drained.notified();
            tokio::pin!(drained);
            // `notified()` is lazy. Explicitly enable the pinned waiter before
            // inspecting the count so the final permit cannot drop in the
            // check-to-first-poll window and lose `notify_waiters()`.
            drained.as_mut().enable();
            {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.closed = true;
                if state.active == 0 {
                    return;
                }
            }
            drained.await;
        }
    }

    fn is_closed(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed
    }
}

impl Drop for ProviderOperationPermit {
    fn drop(&mut self) {
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active = state
            .active
            .checked_sub(1)
            .expect("provider operation permits have one matching registration");
        if state.active == 0 {
            self.gate.drained.notify_waiters();
        }
    }
}

async fn finish_attempt_batch<T, F, Fut>(
    attempts: Vec<T>,
    mut finish: F,
) -> (Vec<T>, Option<astra_core::ClassifiedError>)
where
    T: Clone + std::fmt::Debug,
    F: FnMut(T) -> Fut,
    Fut: std::future::Future<Output = Result<(), astra_core::ClassifiedError>>,
{
    let mut completed = Vec::with_capacity(attempts.len());
    let mut first_error = None;
    for attempt in attempts {
        match finish(attempt.clone()).await {
            Ok(()) => completed.push(attempt),
            Err(error) => {
                astra_core::agent_error!(
                    "llm",
                    "provider attempt {attempt:?} terminal commit failed: {error}"
                );
                first_error.get_or_insert(error);
            }
        }
    }
    (completed, first_error)
}

impl DurableProviderAttemptObserver {
    #[cfg(test)]
    fn new_with_persistence(
        persistence: Arc<dyn InferenceLedgerPersistence>,
        invocation: astra_services::InferenceInvocationPlan,
        request_context: astra_services::ModelRequestContextSeed,
    ) -> Self {
        let settlement_coordinator = ProviderSettlementCoordinator::runtime();
        let settlement_reservation = settlement_coordinator
            .reserve_immediate_for_test(SettlementAdmissionOwner::for_test("test", "test"))
            .expect("test provider settlement reservation");
        let owner_lease = InferenceOwnerLease::start(persistence.clone(), invocation.clone(), None);
        Self::new_with_persistence_and_coordinator(
            persistence,
            invocation,
            request_context,
            settlement_coordinator,
            Arc::new(std::sync::Mutex::new(Some(settlement_reservation))),
            owner_lease,
        )
    }

    fn new_with_persistence_and_coordinator(
        persistence: Arc<dyn InferenceLedgerPersistence>,
        invocation: astra_services::InferenceInvocationPlan,
        request_context: astra_services::ModelRequestContextSeed,
        settlement_coordinator: Arc<ProviderSettlementCoordinator>,
        settlement_reservation: Arc<std::sync::Mutex<Option<ProviderSettlementReservation>>>,
        owner_lease: Arc<InferenceOwnerLease>,
    ) -> Self {
        Self {
            persistence,
            settlement_coordinator,
            settlement_reservation,
            invocation,
            request_context,
            canonical_transitions: std::sync::Mutex::new(Vec::new()),
            admitted_canonical_transition_id: std::sync::Mutex::new(None),
            next_attempt: AtomicU32::new(0),
            dispatch_started: AtomicBool::new(false),
            dispatched_attempts: std::sync::Mutex::new(BTreeSet::new()),
            state: Arc::new(tokio::sync::Mutex::new(ProviderAttemptState::default())),
            operations: ProviderOperationGate::default(),
            owner_lease,
        }
    }

    async fn finish_open_attempts(
        &self,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> Result<(), astra_core::ClassifiedError> {
        self.operations.close_and_wait().await;
        let mut state = self.state.lock().await;
        let attempts = state
            .open_attempts
            .iter()
            .map(|(index, attempt)| (*index, attempt.clone()))
            .collect::<Vec<_>>();
        let (completed, first_error) =
            finish_attempt_batch(attempts, |(_attempt_index, attempt)| async move {
                self.persistence
                    .finish_provider_attempt(&attempt, terminal)
                    .await
                    .map_err(|error| service_error("provider attempt terminal commit", error))
            })
            .await;
        for (attempt_index, _) in completed {
            state.open_attempts.remove(&attempt_index);
            state.delivery_authorized.remove(&attempt_index);
            state.terminals.insert(attempt_index, terminal.clone());
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }

    async fn terminal_after_disconnect(
        &self,
        fallback: &astra_services::InferenceInvocationTerminal,
    ) -> Result<Option<astra_services::InferenceInvocationTerminal>, astra_core::ClassifiedError>
    {
        // Stop admitting new physical operations. Normal controlled callers
        // drop their persistence future when a deadline wins, so this drains
        // immediately. A defensive bound prevents a foreign/buggy caller from
        // pinning one task and the logical invocation forever.
        let _ = tokio::time::timeout(
            detached_reconciliation_timeout(),
            self.operations.close_and_wait(),
        )
        .await;

        let (attempt, terminal, provider_delivery_state) = {
            let mut state = self.state.lock().await;
            if state.open_attempts.len() > 1 {
                return Err(contract_error(
                    "disconnect settlement",
                    "multiple provider attempts remained open for one sequential invocation",
                ));
            }
            let Some((attempt_index, attempt)) = state.open_attempts.last_key_value() else {
                return Ok(Some(
                    state
                        .terminals
                        .last_key_value()
                        .map(|(_, terminal)| terminal.clone())
                        .unwrap_or_else(|| fallback.clone()),
                ));
            };
            let attempt_index = *attempt_index;
            let attempt = attempt.clone();
            if state.settlement_handed_off.contains(&attempt_index) {
                return Ok(None);
            }
            let provider_delivery_authorized = state.delivery_authorized.contains(&attempt_index);
            let provider_delivery_state = if provider_delivery_authorized {
                astra_services::InferenceProviderDeliveryState::DeliveryAuthorized
            } else {
                astra_services::InferenceProviderDeliveryState::PreDelivery
            };
            let terminal = state
                .pending_terminals
                .get(&attempt_index)
                .cloned()
                .unwrap_or_else(|| {
                    if provider_delivery_authorized {
                        fallback.clone()
                    } else {
                        terminal_from_error(&astra_core::ClassifiedError::new(
                            astra_core::ErrorKind::Cancelled,
                            "Provider attempt admission was not acknowledged; HTTP delivery was not authorized",
                        ))
                    }
                });
            state.settlement_handed_off.insert(attempt_index);
            (attempt, terminal, provider_delivery_state)
        };
        let reservation = self
            .settlement_reservation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .ok_or_else(|| {
                contract_error(
                    "disconnect settlement",
                    "invocation has no reconciliation reservation",
                )
            })?;

        // The admission-time reservation makes this handoff infallible and
        // bounded. Fixed process-wide workers own all retries until the exact
        // debt (or an idempotent equivalent) is authoritatively confirmed.
        self.settlement_coordinator.handoff(ProviderSettlementJob {
            persistence: self.persistence.clone(),
            invocation: self.invocation.clone(),
            task: ProviderSettlementTask::Debt {
                attempt: Box::new(Some(attempt)),
                terminal,
                provider_delivery_state,
            },
            owner_lease: Some(self.owner_lease.clone()),
            _reservation: reservation,
        });
        Ok(None)
    }
}

impl Drop for DurableProviderAttemptObserver {
    fn drop(&mut self) {
        let Some(reservation) = self
            .settlement_reservation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            return;
        };
        if self.owner_lease.is_lost() {
            drop(reservation);
            return;
        }
        // This observer is the final shared owner of both the logical
        // invocation and its exact attempt state. A panic, task abort, or
        // forced caller drop therefore transfers ownership synchronously to
        // the bounded coordinator instead of silently releasing capacity.
        self.settlement_coordinator.handoff(ProviderSettlementJob {
            persistence: self.persistence.clone(),
            invocation: self.invocation.clone(),
            task: ProviderSettlementTask::InvocationOwnerLost {
                state: self.state.clone(),
                operations: self.operations.clone(),
            },
            owner_lease: Some(self.owner_lease.clone()),
            _reservation: reservation,
        });
    }
}

#[async_trait]
impl ProviderAttemptObserver for DurableProviderAttemptObserver {
    async fn begin_attempt(
        &self,
        wire: &ProviderWireRequestIdentity,
    ) -> Result<u32, astra_core::ClassifiedError> {
        // The permit is registered synchronously before the first await. The
        // persistence future remains owned by this caller: if its hard
        // deadline wins, dropping this future cancels the database operation
        // and releases the permit instead of leaking an unbounded Tokio task.
        // A commit whose acknowledgement was lost is recovered from the
        // invocation settlement debt before any provider request can be sent.
        self.owner_lease.ensure_live("provider attempt admission")?;
        let _permit = self.operations.register("provider attempt admission")?;
        let attempt_index = self.next_attempt.fetch_add(1, Ordering::AcqRel);
        let service_wire = astra_services::InferenceProviderWireIdentity::new(
            wire.protocol.as_str(),
            wire.provider_wire_hash.clone(),
            wire.provider_wire_bytes,
        )
        .map_err(|error| service_error("provider wire identity", error))?
        .with_composition(astra_services::ModelRequestWireComposition {
            system_bytes: wire.composition.system_bytes,
            conversation_bytes: wire.composition.conversation_bytes,
            tool_schema_bytes: wire.composition.tool_schema_bytes,
            provider_envelope_bytes: wire.composition.provider_envelope_bytes,
            system_items: wire.composition.system_items,
            conversation_items: wire.composition.conversation_items,
            tool_schema_items: wire.composition.tool_schema_items,
        });
        let canonical_transitions = self
            .canonical_transitions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let canonical_transition_id = canonical_transitions
            .first()
            .map(|transition| transition.transition_id.clone());
        let attempt = astra_services::plan_inference_provider_attempt_with_context(
            &self.invocation,
            attempt_index,
            service_wire,
            self.request_context.clone(),
        )
        .with_canonical_transitions(&canonical_transitions)
        .map_err(|error| service_error("provider canonical transition", error))?;
        let request = DurableProviderRequestIdentity {
            request_id: attempt.request_id().to_string(),
            request_hash: wire.provider_wire_hash.clone(),
            attempt: attempt_index,
            protocol: wire.protocol,
            provider_wire_bytes: wire.provider_wire_bytes,
            composition: wire.composition.clone(),
            fingerprints: wire.fingerprints.clone(),
        };
        {
            let mut state = self.state.lock().await;
            // Publish the stable identity before persistence. If the commit is
            // acknowledged late (or its acknowledgement is lost), disconnect
            // settlement can still create an exact pre-delivery attempt debt.
            state.requests.insert(attempt_index, request);
            state.open_attempts.insert(attempt_index, attempt.clone());
        }
        if let Err(error) = self.persistence.begin_provider_attempt(&attempt).await {
            // Database errors at this boundary are commit-ambiguous. Retain
            // the stable attempt identity and its pre-reserved reconciliation
            // slot; the caller's ledger-error settlement will close it as a
            // pre-delivery cancellation without ever authorizing HTTP.
            return Err(service_error("provider attempt admission", error));
        }
        if let Some(canonical_transition_id) = canonical_transition_id {
            let mut admitted = self
                .admitted_canonical_transition_id
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if admitted
                .as_deref()
                .is_some_and(|existing| existing != canonical_transition_id)
            {
                return Err(contract_error(
                    "provider attempt admission",
                    "physical retries admitted different canonical transition ids",
                ));
            }
            *admitted = Some(canonical_transition_id);
        }
        self.owner_lease
            .ensure_live("provider delivery authorization")?;
        if self.operations.is_closed() {
            return Err(contract_error(
                "provider attempt admission",
                "settlement began before provider delivery was authorized",
            ));
        }
        self.state
            .lock()
            .await
            .delivery_authorized
            .insert(attempt_index);
        Ok(attempt_index)
    }

    async fn finish_attempt(
        &self,
        attempt_index: u32,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> Result<(), astra_core::ClassifiedError> {
        self.owner_lease.ensure_live("provider attempt terminal")?;
        let _permit = self.operations.register("provider attempt terminal")?;
        let attempt = {
            let mut state = self.state.lock().await;
            let attempt = state
                .open_attempts
                .get(&attempt_index)
                .cloned()
                .ok_or_else(|| {
                    contract_error(
                        "provider attempt terminal",
                        format!("attempt {attempt_index} is not open"),
                    )
                })?;
            // Retain the exact provider outcome before entering persistence.
            // If the caller's settlement deadline drops this future, bounded
            // disconnect reconciliation can retry it by stable attempt id.
            state
                .pending_terminals
                .insert(attempt_index, terminal.clone());
            attempt
        };
        self.persistence
            .finish_provider_attempt(&attempt, terminal)
            .await
            .map_err(|error| service_error("provider attempt terminal commit", error))?;
        let mut state = self.state.lock().await;
        state.open_attempts.remove(&attempt_index);
        state.delivery_authorized.remove(&attempt_index);
        state.pending_terminals.remove(&attempt_index);
        state.terminals.insert(attempt_index, terminal.clone());
        Ok(())
    }

    fn note_dispatch_started(&self, attempt_index: u32) {
        self.dispatched_attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(attempt_index);
        self.dispatch_started.store(true, Ordering::Release);
    }
}

fn contract_error(
    stage: &'static str,
    error: impl std::fmt::Display,
) -> astra_core::ClassifiedError {
    astra_core::ClassifiedError::new(
        astra_core::ErrorKind::ContractViolation,
        format!("durable inference {stage} failed: {error}"),
    )
    .with_details_json(
        json!({
            "source": INFERENCE_LEDGER_ERROR_SOURCE,
            "stage": stage,
        })
        .to_string(),
    )
}

fn service_error(
    stage: &'static str,
    error: astra_services::ServiceError,
) -> astra_core::ClassifiedError {
    let kind = match error.kind {
        astra_services::ServiceErrorKind::Persistence => astra_core::ErrorKind::DatabaseError,
        astra_services::ServiceErrorKind::Network => astra_core::ErrorKind::Network,
        astra_services::ServiceErrorKind::Invalid | astra_services::ServiceErrorKind::NotFound => {
            astra_core::ErrorKind::InvalidRequest
        }
        astra_services::ServiceErrorKind::Verification
        | astra_services::ServiceErrorKind::Conflict
        | astra_services::ServiceErrorKind::ConflictTransient
        | astra_services::ServiceErrorKind::Internal => astra_core::ErrorKind::ContractViolation,
    };
    astra_core::ClassifiedError::new(kind, format!("durable inference {stage} failed: {error}"))
        .with_details_json(
            json!({
                "source": INFERENCE_LEDGER_ERROR_SOURCE,
                "stage": stage,
                "service_error_kind": error.kind.as_str(),
            })
            .to_string(),
        )
}

fn ledger_timeout_error_for_stage(stage: &'static str) -> astra_core::ClassifiedError {
    astra_core::ClassifiedError::new(
        astra_core::ErrorKind::DatabaseError,
        format!("durable inference ledger timed out during {stage}"),
    )
    .with_details_json(
        json!({
            "source": INFERENCE_LEDGER_ERROR_SOURCE,
            "deadline": {
                "scope": "inference_ledger",
                "phase": stage,
            }
        })
        .to_string(),
    )
}

pub(crate) fn is_ledger_error(error: &astra_core::ClassifiedError) -> bool {
    error
        .details_json
        .as_deref()
        .and_then(|details| serde_json::from_str::<serde_json::Value>(details).ok())
        .and_then(|details| {
            details
                .get("source")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|source| source == INFERENCE_LEDGER_ERROR_SOURCE)
}

pub(crate) fn terminal_from_error(
    error: &astra_core::ClassifiedError,
) -> astra_services::InferenceInvocationTerminal {
    let status = match error.kind {
        astra_core::ErrorKind::Cancelled => astra_services::InferenceTerminalStatus::Cancelled,
        astra_core::ErrorKind::StreamIdle | astra_core::ErrorKind::StreamTransport => {
            astra_services::InferenceTerminalStatus::DeliveryUnknown
        }
        _ => astra_services::InferenceTerminalStatus::Failed,
    };
    let message = crate::turn::llm::client::redact_provider_secrets(&error.message);
    astra_services::InferenceInvocationTerminal {
        status,
        usage: astra_services::InferenceUsage::default(),
        usage_status: astra_services::InferenceUsageStatus::Unavailable,
        provider_response_id: None,
        error_kind: Some(error.kind.as_str().to_string()),
        error_message: Some(
            astra_text_utils::str_preview::truncate_str(&message, 1_000).to_string(),
        ),
    }
}

fn pre_provider_cancelled_terminal() -> astra_services::InferenceInvocationTerminal {
    terminal_from_error(&astra_core::ClassifiedError::new(
        astra_core::ErrorKind::Cancelled,
        "Inference owner stopped before provider delivery was authorized",
    ))
}

fn owner_lost_delivery_unknown_terminal() -> astra_services::InferenceInvocationTerminal {
    delivery_unknown_terminal_from_error(&astra_core::ClassifiedError::new(
        astra_core::ErrorKind::StreamTransport,
        "Inference owner stopped after provider delivery was authorized",
    ))
}

fn delivery_unknown_terminal_from_error(
    error: &astra_core::ClassifiedError,
) -> astra_services::InferenceInvocationTerminal {
    let mut terminal = terminal_from_error(error);
    terminal.status = astra_services::InferenceTerminalStatus::DeliveryUnknown;
    terminal
}

fn unsettled_attempt_terminal() -> astra_services::InferenceInvocationTerminal {
    astra_services::InferenceInvocationTerminal {
        status: astra_services::InferenceTerminalStatus::DeliveryUnknown,
        usage: astra_services::InferenceUsage::default(),
        usage_status: astra_services::InferenceUsageStatus::Unavailable,
        provider_response_id: None,
        error_kind: Some("inference_ledger".to_string()),
        error_message: Some(
            "provider attempt terminal state could not be committed durably".to_string(),
        ),
    }
}

fn ledger_reconciliation_terminal(
    error: &astra_core::ClassifiedError,
) -> astra_services::InferenceInvocationTerminal {
    let phase = error
        .details_json
        .as_deref()
        .and_then(|details| serde_json::from_str::<serde_json::Value>(details).ok())
        .and_then(|details| {
            details
                .pointer("/deadline/phase")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
    if phase.as_deref() == Some("provider_attempt_admission") {
        return terminal_from_error(&astra_core::ClassifiedError::new(
            astra_core::ErrorKind::Cancelled,
            "Provider attempt admission was not acknowledged; HTTP delivery was not authorized",
        ));
    }
    unsettled_attempt_terminal()
}

fn terminal_from_result(result: &LlmCallResult) -> astra_services::InferenceInvocationTerminal {
    let usage = crate::turn::token_usage::TokenUsage::from_partial_json_map(&result.usage);
    let mut terminal = astra_services::InferenceInvocationTerminal::succeeded(
        astra_services::InferenceUsage {
            input: astra_turn_types::NormalizedPromptCacheUsage::new(
                usage.input_tokens,
                usage.cached_input_tokens,
                usage.cache_creation_tokens,
            ),
            output_tokens: usage.output_tokens,
        },
        result.response_id.clone(),
    );
    if result.usage.is_empty() {
        terminal.usage_status = astra_services::InferenceUsageStatus::Unavailable;
    }
    terminal
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, response::Response, routing::post};

    async fn spawn_test_server(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("test server address");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve test app");
        });
        format!("http://{addr}")
    }

    fn test_ledger_with_persistence(
        base_url: &str,
        persistence: Arc<dyn InferenceLedgerPersistence>,
    ) -> DurableInferenceLedger {
        let execution = astra_services::AdmittedModelExecution {
            offering_id: "offering-test".to_string(),
            access_kind: astra_services::ModelAccessKind::SelfHosted,
            execution_placement: astra_services::ModelExecutionPlacement::Server,
            model_name: "model-test".to_string(),
            wire_model_name: None,
            api_key: "test-key".to_string(),
            base_url: base_url.to_string(),
            provider: "openai".to_string(),
            cache_capability: None,
            thinking_capability: None,
            request_body_overrides: None,
            context_window: Some(8_192),
            max_completion_tokens: Some(1_024),
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout_ms: None,
        };
        DurableInferenceLedger::required_with_persistence(
            None,
            Some(&execution),
            "user-test",
            Some(persistence),
        )
        .expect("test durable ledger")
        .with_run_authority(DurableInferenceRunAuthority::new(
            0,
            "test-inference-owner",
            0,
            None,
            None,
            None,
        ))
    }

    fn test_ledger(
        base_url: &str,
    ) -> (DurableInferenceLedger, Arc<TestInferenceLedgerPersistence>) {
        let persistence = Arc::new(TestInferenceLedgerPersistence::default());
        let ledger = test_ledger_with_persistence(base_url, persistence.clone());
        (ledger, persistence)
    }

    fn test_scope(operation_id: &str) -> astra_turn_types::InferenceInvocationScope {
        astra_turn_types::InferenceInvocationScope::Run {
            session_id: "session-test".to_string(),
            run_id: "run-test".to_string(),
            turn: 1,
            round: 1,
            operation_id: operation_id.to_string(),
            logical_attempt: 0,
        }
    }

    fn test_call<'a>(base_url: &'a str, messages: &'a [serde_json::Value]) -> LlmCall<'a> {
        LlmCall {
            purpose: astra_turn_types::InferencePurpose::SubAgent,
            messages,
            tools: &[],
            cache_capability: None,
            route: crate::turn::llm::client::LlmExecutionRoute {
                model_name: "model-test",
                wire_model_name: None,
                api_key: "test-key",
                base_url,
                provider: "openai",
                header_overrides: None,
                request_body_overrides: None,
                completions_url_override: None,
                request_timeout: None,
            },
            max_output_tokens: Some(64),
            temperature: None,
            has_fallback: false,
            thinking: &astra_turn_core::thinking_config::ThinkingConfig::Off,
        }
    }

    async fn wait_for_quiescent(persistence: &TestInferenceLedgerPersistence) {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !persistence.is_quiescent() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached inference settlement must converge");
        persistence.assert_quiescent();
    }

    #[tokio::test]
    async fn nonstream_total_budget_converges_logical_delivery_unknown() {
        let app = Router::new().route(
            "/chat/completions",
            post(|| async {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                Response::builder()
                    .status(200)
                    .body(Body::from(
                        r#"{"choices":[{"message":{"content":"late"}}]}"#,
                    ))
                    .unwrap()
            }),
        );
        let base = spawn_test_server(app).await;
        let (ledger, persistence) = test_ledger(&base);
        let messages = vec![serde_json::json!({"role":"user","content":"x"})];
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("test client");
        let started = std::time::Instant::now();

        let error = ledger
            .execute_nonstream(
                &client,
                test_scope("nonstream_timeout"),
                test_call(&base, &messages),
                std::time::Duration::from_millis(30),
            )
            .await
            .into_result()
            .expect_err("non-streaming request must obey its hard total budget");

        assert_eq!(error.kind, astra_core::ErrorKind::ProviderDeadline);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        wait_for_quiescent(&persistence).await;
        assert_eq!(
            persistence.logical_terminal_statuses(),
            vec![astra_services::InferenceTerminalStatus::DeliveryUnknown]
        );
    }

    #[tokio::test]
    async fn aborting_nonstream_caller_projects_exact_delivery_unknown_without_a_sweeper() {
        let provider_started = Arc::new(tokio::sync::Notify::new());
        let app = Router::new().route(
            "/chat/completions",
            post({
                let provider_started = provider_started.clone();
                move || {
                    let provider_started = provider_started.clone();
                    async move {
                        provider_started.notify_one();
                        std::future::pending::<Response>().await
                    }
                }
            }),
        );
        let base = spawn_test_server(app).await;
        let (ledger, persistence) = test_ledger(&base);
        let caller = tokio::spawn(async move {
            let messages = vec![serde_json::json!({"role":"user","content":"x"})];
            let client = reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("test client");
            ledger
                .execute_nonstream(
                    &client,
                    test_scope("nonstream_caller_drop"),
                    test_call(&base, &messages),
                    std::time::Duration::from_secs(30),
                )
                .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            provider_started.notified(),
        )
        .await
        .expect("provider delivery must start before the caller is dropped");

        caller.abort();
        let cancellation = match caller.await {
            Err(cancellation) => cancellation,
            Ok(_) => panic!("caller task must be aborted"),
        };
        assert!(cancellation.is_cancelled());

        // No explicit test sweeper is invoked: the admission-time bounded
        // settlement owner must advance its own exact debt to the canonical
        // physical and logical terminals.
        wait_for_quiescent(&persistence).await;
        assert_eq!(
            persistence.logical_terminal_statuses(),
            vec![astra_services::InferenceTerminalStatus::DeliveryUnknown]
        );
    }

    #[tokio::test]
    async fn stream_total_budget_converges_logical_delivery_unknown() {
        let app = Router::new().route(
            "/chat/completions",
            post(|| async {
                let stream = async_stream::stream! {
                    yield Ok::<_, std::convert::Infallible>(axum::body::Bytes::from_static(
                        b"data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
                    ));
                    std::future::pending::<()>().await;
                };
                Response::builder()
                    .status(200)
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(stream))
                    .unwrap()
            }),
        );
        let base = spawn_test_server(app).await;
        let (ledger, persistence) = test_ledger(&base);
        let messages = vec![serde_json::json!({"role":"user","content":"x"})];
        let started = std::time::Instant::now();

        let error = ledger
            .execute_stream_with_total_budget_for_test(
                test_scope("stream_timeout"),
                test_call(&base, &messages),
                std::time::Duration::from_millis(40),
            )
            .await
            .expect_err("streaming request must obey its hard total budget");

        assert_eq!(error.kind, astra_core::ErrorKind::ProviderDeadline);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        wait_for_quiescent(&persistence).await;
        assert_eq!(
            persistence.logical_terminal_statuses(),
            vec![astra_services::InferenceTerminalStatus::DeliveryUnknown]
        );
    }

    #[tokio::test]
    async fn nonstream_ledger_timeout_hands_off_and_preserves_exact_success() {
        let app = Router::new().route(
            "/chat/completions",
            post(|| async {
                Response::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"id":"response-1","choices":[{"message":{"content":"answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":2}}"#,
                    ))
                    .unwrap()
            }),
        );
        let base = spawn_test_server(app).await;
        let persistence = Arc::new(DelayedTrackedTerminalPersistence::default());
        let ledger = test_ledger_with_persistence(&base, persistence.clone());
        let messages = vec![serde_json::json!({"role":"user","content":"x"})];
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("test client");

        let error = ledger
            .execute_nonstream(
                &client,
                test_scope("nonstream_ledger_timeout"),
                test_call(&base, &messages),
                std::time::Duration::from_millis(100),
            )
            .await
            .into_result()
            .expect_err("foreground must not outlive its ledger settlement reserve");

        assert_eq!(error.kind, astra_core::ErrorKind::DatabaseError);
        assert!(is_ledger_error(&error));
        assert_eq!(persistence.finish_entered.load(Ordering::SeqCst), 1);
        persistence.release_finish.notify_one();
        wait_for_quiescent(&persistence.inner).await;
        assert_eq!(
            persistence.inner.logical_terminal_statuses(),
            vec![astra_services::InferenceTerminalStatus::Succeeded]
        );
    }

    #[derive(Default)]
    struct DelayedAdmissionPersistence {
        begin_entered: AtomicU32,
        release_begin: tokio::sync::Notify,
        finished: AtomicU32,
    }

    #[derive(Default)]
    struct StalledTerminalPersistence {
        inner: TestInferenceLedgerPersistence,
        finish_entered: AtomicU32,
        active_finish_workers: AtomicU32,
    }

    struct ActiveWorkerGuard<'a>(&'a AtomicU32);

    impl Drop for ActiveWorkerGuard<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[derive(Default)]
    struct DelayedTrackedAdmissionPersistence {
        inner: TestInferenceLedgerPersistence,
        begin_entered: AtomicU32,
        release_begin: tokio::sync::Notify,
    }

    #[derive(Default)]
    struct DelayedTrackedTerminalPersistence {
        inner: TestInferenceLedgerPersistence,
        finish_entered: AtomicU32,
        release_finish: tokio::sync::Notify,
    }

    #[derive(Default)]
    struct ControlledReconcilePersistence {
        inner: TestInferenceLedgerPersistence,
        panics_remaining: AtomicU32,
        permanently_quarantined: AtomicBool,
        reconcile_calls: AtomicU32,
    }

    #[derive(Default)]
    struct AmbiguousLogicalAdmissionPersistence {
        inner: TestInferenceLedgerPersistence,
        commit_before_ack_loss: AtomicBool,
        admit_entered: AtomicU32,
        uncertain_settlements: AtomicU32,
        provider_attempts: AtomicU32,
        admitted_logical_attempts: std::sync::Mutex<Vec<u32>>,
        stall_retry_admission: AtomicBool,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum AdmissionRecoveryFailureMode {
        ConclusiveRejection,
        ScopeUnavailable,
        AuthorityLost,
        ConflictingIdentity,
        Stall,
    }

    struct AdmissionRecoveryFailurePersistence {
        mode: AdmissionRecoveryFailureMode,
        admission_entered: AtomicU32,
        uncertain_settlements: AtomicU32,
        provider_attempts: AtomicU32,
    }

    impl AdmissionRecoveryFailurePersistence {
        fn new(mode: AdmissionRecoveryFailureMode) -> Self {
            Self {
                mode,
                admission_entered: AtomicU32::new(0),
                uncertain_settlements: AtomicU32::new(0),
                provider_attempts: AtomicU32::new(0),
            }
        }
    }

    #[derive(Default)]
    struct FailFirstLogicalTerminalPersistence {
        inner: TestInferenceLedgerPersistence,
        finish_calls: AtomicU32,
    }

    #[derive(Default)]
    struct StalledSettlementDeclarationPersistence {
        admit_calls: AtomicU32,
        declaration_calls: AtomicU32,
        active_declarations: AtomicU32,
        max_active_declarations: AtomicU32,
        allow_declaration: AtomicBool,
        permanently_reject_attempt_zero: AtomicBool,
    }

    struct ActiveDeclarationGuard<'a>(&'a AtomicU32);

    impl Drop for ActiveDeclarationGuard<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl InferenceLedgerPersistence for StalledSettlementDeclarationPersistence {
        async fn admit_invocation(
            &self,
            _plan: &astra_services::InferenceInvocationPlan,
        ) -> astra_services::ServiceResult<()> {
            self.admit_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn settle_uncertain_admission(
            &self,
            _plan: &astra_services::InferenceInvocationPlan,
            _terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<astra_services::InferenceInvocationAdmissionResolution>
        {
            Ok(astra_services::InferenceInvocationAdmissionResolution::Settled)
        }

        async fn declare_settlement(
            &self,
            _plan: &astra_services::InferenceInvocationPlan,
            _terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            Ok(())
        }

        async fn declare_attempt_settlement(
            &self,
            _plan: &astra_services::InferenceInvocationPlan,
            attempt: &astra_services::InferenceProviderAttemptPlan,
            _terminal: &astra_services::InferenceInvocationTerminal,
            _provider_delivery_state: astra_services::InferenceProviderDeliveryState,
        ) -> astra_services::ServiceResult<()> {
            self.declaration_calls.fetch_add(1, Ordering::SeqCst);
            let active = self.active_declarations.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active_declarations
                .fetch_max(active, Ordering::SeqCst);
            let _active = ActiveDeclarationGuard(&self.active_declarations);
            if self.permanently_reject_attempt_zero.load(Ordering::SeqCst)
                && attempt.attempt_index() == 0
            {
                return Err(astra_services::ServiceError::conflict(
                    "injected permanent settlement conflict",
                ));
            }
            if !self.allow_declaration.load(Ordering::SeqCst) {
                std::future::pending().await
            }
            Ok(())
        }

        async fn finish_invocation(
            &self,
            _plan: &astra_services::InferenceInvocationPlan,
            _terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            Ok(())
        }

        async fn begin_provider_attempt(
            &self,
            _attempt: &astra_services::InferenceProviderAttemptPlan,
        ) -> astra_services::ServiceResult<()> {
            Ok(())
        }

        async fn finish_provider_attempt(
            &self,
            _attempt: &astra_services::InferenceProviderAttemptPlan,
            _terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl InferenceLedgerPersistence for ControlledReconcilePersistence {
        async fn admit_invocation(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
        ) -> astra_services::ServiceResult<()> {
            self.inner.admit_invocation(plan).await
        }

        async fn settle_uncertain_admission(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<astra_services::InferenceInvocationAdmissionResolution>
        {
            self.inner.settle_uncertain_admission(plan, terminal).await
        }

        async fn declare_settlement(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            self.inner.declare_settlement(plan, terminal).await
        }

        async fn declare_attempt_settlement(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            attempt: &astra_services::InferenceProviderAttemptPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
            provider_delivery_state: astra_services::InferenceProviderDeliveryState,
        ) -> astra_services::ServiceResult<()> {
            self.inner
                .declare_attempt_settlement(plan, attempt, terminal, provider_delivery_state)
                .await
        }

        async fn reconcile_settlement(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<astra_services::InferenceSettlementReconcileOutcome>
        {
            self.reconcile_calls.fetch_add(1, Ordering::SeqCst);
            let remaining = self.panics_remaining.load(Ordering::SeqCst);
            if remaining > 0 {
                self.panics_remaining.fetch_sub(1, Ordering::SeqCst);
                panic!("injected provider settlement worker panic");
            }
            if self.permanently_quarantined.load(Ordering::SeqCst) {
                return Ok(
                    astra_services::InferenceSettlementReconcileOutcome::PermanentlyQuarantined,
                );
            }
            self.inner.reconcile_settlement(plan, terminal).await
        }

        async fn finish_invocation(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            self.inner.finish_invocation(plan, terminal).await
        }

        async fn begin_provider_attempt(
            &self,
            attempt: &astra_services::InferenceProviderAttemptPlan,
        ) -> astra_services::ServiceResult<()> {
            self.inner.begin_provider_attempt(attempt).await
        }

        async fn finish_provider_attempt(
            &self,
            attempt: &astra_services::InferenceProviderAttemptPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            self.inner.finish_provider_attempt(attempt, terminal).await
        }
    }

    #[async_trait]
    impl InferenceLedgerPersistence for FailFirstLogicalTerminalPersistence {
        async fn admit_invocation(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
        ) -> astra_services::ServiceResult<()> {
            self.inner.admit_invocation(plan).await
        }

        async fn settle_uncertain_admission(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<astra_services::InferenceInvocationAdmissionResolution>
        {
            self.inner.settle_uncertain_admission(plan, terminal).await
        }

        async fn declare_settlement(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            self.inner.declare_settlement(plan, terminal).await
        }

        async fn declare_attempt_settlement(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            attempt: &astra_services::InferenceProviderAttemptPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
            provider_delivery_state: astra_services::InferenceProviderDeliveryState,
        ) -> astra_services::ServiceResult<()> {
            self.inner
                .declare_attempt_settlement(plan, attempt, terminal, provider_delivery_state)
                .await
        }

        async fn finish_invocation(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            if self.finish_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(astra_services::ServiceError::with_source(
                    astra_services::ServiceErrorKind::Persistence,
                    "read logical invocation before terminal mirror",
                    std::io::Error::other("injected pre-mirror read failure"),
                ));
            }
            self.inner.finish_invocation(plan, terminal).await
        }

        async fn begin_provider_attempt(
            &self,
            attempt: &astra_services::InferenceProviderAttemptPlan,
        ) -> astra_services::ServiceResult<()> {
            self.inner.begin_provider_attempt(attempt).await
        }

        async fn finish_provider_attempt(
            &self,
            attempt: &astra_services::InferenceProviderAttemptPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            self.inner.finish_provider_attempt(attempt, terminal).await
        }
    }

    #[async_trait]
    impl InferenceLedgerPersistence for StalledTerminalPersistence {
        async fn admit_invocation(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
        ) -> astra_services::ServiceResult<()> {
            self.inner.admit_invocation(plan).await
        }

        async fn settle_uncertain_admission(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<astra_services::InferenceInvocationAdmissionResolution>
        {
            self.inner.settle_uncertain_admission(plan, terminal).await
        }

        async fn declare_settlement(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            self.inner.declare_settlement(plan, terminal).await
        }

        async fn declare_attempt_settlement(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            attempt: &astra_services::InferenceProviderAttemptPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
            provider_delivery_state: astra_services::InferenceProviderDeliveryState,
        ) -> astra_services::ServiceResult<()> {
            self.inner
                .declare_attempt_settlement(plan, attempt, terminal, provider_delivery_state)
                .await
        }

        async fn finish_invocation(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            self.inner.finish_invocation(plan, terminal).await
        }

        async fn begin_provider_attempt(
            &self,
            attempt: &astra_services::InferenceProviderAttemptPlan,
        ) -> astra_services::ServiceResult<()> {
            self.inner.begin_provider_attempt(attempt).await
        }

        async fn finish_provider_attempt(
            &self,
            _attempt: &astra_services::InferenceProviderAttemptPlan,
            _terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            self.finish_entered.fetch_add(1, Ordering::SeqCst);
            self.active_finish_workers.fetch_add(1, Ordering::SeqCst);
            let _active = ActiveWorkerGuard(&self.active_finish_workers);
            std::future::pending().await
        }
    }

    #[async_trait]
    impl InferenceLedgerPersistence for DelayedTrackedAdmissionPersistence {
        async fn admit_invocation(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
        ) -> astra_services::ServiceResult<()> {
            self.inner.admit_invocation(plan).await
        }

        async fn settle_uncertain_admission(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<astra_services::InferenceInvocationAdmissionResolution>
        {
            self.inner.settle_uncertain_admission(plan, terminal).await
        }

        async fn declare_settlement(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            self.inner.declare_settlement(plan, terminal).await
        }

        async fn declare_attempt_settlement(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            attempt: &astra_services::InferenceProviderAttemptPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
            provider_delivery_state: astra_services::InferenceProviderDeliveryState,
        ) -> astra_services::ServiceResult<()> {
            self.inner
                .declare_attempt_settlement(plan, attempt, terminal, provider_delivery_state)
                .await
        }

        async fn reconcile_settlement(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<astra_services::InferenceSettlementReconcileOutcome>
        {
            self.inner.reconcile_settlement(plan, terminal).await
        }

        async fn finish_invocation(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            self.inner.finish_invocation(plan, terminal).await
        }

        async fn begin_provider_attempt(
            &self,
            attempt: &astra_services::InferenceProviderAttemptPlan,
        ) -> astra_services::ServiceResult<()> {
            // Commit first, then withhold the acknowledgement. Cancelling this
            // future therefore exercises the real ambiguous-COMMIT window.
            self.inner.begin_provider_attempt(attempt).await?;
            self.begin_entered.fetch_add(1, Ordering::SeqCst);
            self.release_begin.notified().await;
            Ok(())
        }

        async fn finish_provider_attempt(
            &self,
            attempt: &astra_services::InferenceProviderAttemptPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            self.inner.finish_provider_attempt(attempt, terminal).await
        }
    }

    #[async_trait]
    impl InferenceLedgerPersistence for DelayedTrackedTerminalPersistence {
        async fn admit_invocation(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
        ) -> astra_services::ServiceResult<()> {
            self.inner.admit_invocation(plan).await
        }

        async fn settle_uncertain_admission(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<astra_services::InferenceInvocationAdmissionResolution>
        {
            self.inner.settle_uncertain_admission(plan, terminal).await
        }

        async fn declare_settlement(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            self.inner.declare_settlement(plan, terminal).await
        }

        async fn declare_attempt_settlement(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            attempt: &astra_services::InferenceProviderAttemptPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
            provider_delivery_state: astra_services::InferenceProviderDeliveryState,
        ) -> astra_services::ServiceResult<()> {
            self.inner
                .declare_attempt_settlement(plan, attempt, terminal, provider_delivery_state)
                .await
        }

        async fn reconcile_settlement(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<astra_services::InferenceSettlementReconcileOutcome>
        {
            self.inner.reconcile_settlement(plan, terminal).await
        }

        async fn finish_invocation(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            self.inner.finish_invocation(plan, terminal).await
        }

        async fn begin_provider_attempt(
            &self,
            attempt: &astra_services::InferenceProviderAttemptPlan,
        ) -> astra_services::ServiceResult<()> {
            self.inner.begin_provider_attempt(attempt).await
        }

        async fn finish_provider_attempt(
            &self,
            attempt: &astra_services::InferenceProviderAttemptPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            self.finish_entered.fetch_add(1, Ordering::SeqCst);
            self.release_finish.notified().await;
            self.inner.finish_provider_attempt(attempt, terminal).await
        }
    }

    #[async_trait]
    impl InferenceLedgerPersistence for DelayedAdmissionPersistence {
        async fn admit_invocation(
            &self,
            _plan: &astra_services::InferenceInvocationPlan,
        ) -> astra_services::ServiceResult<()> {
            Ok(())
        }

        async fn settle_uncertain_admission(
            &self,
            _plan: &astra_services::InferenceInvocationPlan,
            _terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<astra_services::InferenceInvocationAdmissionResolution>
        {
            Ok(astra_services::InferenceInvocationAdmissionResolution::Settled)
        }

        async fn declare_settlement(
            &self,
            _plan: &astra_services::InferenceInvocationPlan,
            _terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            Ok(())
        }

        async fn declare_attempt_settlement(
            &self,
            _plan: &astra_services::InferenceInvocationPlan,
            _attempt: &astra_services::InferenceProviderAttemptPlan,
            _terminal: &astra_services::InferenceInvocationTerminal,
            _provider_delivery_state: astra_services::InferenceProviderDeliveryState,
        ) -> astra_services::ServiceResult<()> {
            Ok(())
        }

        async fn finish_invocation(
            &self,
            _plan: &astra_services::InferenceInvocationPlan,
            _terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            Ok(())
        }

        async fn begin_provider_attempt(
            &self,
            _attempt: &astra_services::InferenceProviderAttemptPlan,
        ) -> astra_services::ServiceResult<()> {
            self.begin_entered.fetch_add(1, Ordering::SeqCst);
            self.release_begin.notified().await;
            Ok(())
        }

        async fn finish_provider_attempt(
            &self,
            _attempt: &astra_services::InferenceProviderAttemptPlan,
            _terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            self.finished.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[async_trait]
    impl InferenceLedgerPersistence for AmbiguousLogicalAdmissionPersistence {
        async fn admit_invocation(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
        ) -> astra_services::ServiceResult<()> {
            self.admitted_logical_attempts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(plan.logical_attempt());
            if plan.logical_attempt() != 0 && !self.stall_retry_admission.load(Ordering::SeqCst) {
                return self.inner.admit_invocation(plan).await;
            }
            if self.commit_before_ack_loss.load(Ordering::SeqCst) {
                self.inner.admit_invocation(plan).await?;
            }
            self.admit_entered.fetch_add(1, Ordering::SeqCst);
            std::future::pending().await
        }

        async fn settle_uncertain_admission(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<astra_services::InferenceInvocationAdmissionResolution>
        {
            self.uncertain_settlements.fetch_add(1, Ordering::SeqCst);
            self.inner.settle_uncertain_admission(plan, terminal).await
        }

        async fn declare_settlement(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            self.inner.declare_settlement(plan, terminal).await
        }

        async fn declare_attempt_settlement(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            attempt: &astra_services::InferenceProviderAttemptPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
            provider_delivery_state: astra_services::InferenceProviderDeliveryState,
        ) -> astra_services::ServiceResult<()> {
            self.inner
                .declare_attempt_settlement(plan, attempt, terminal, provider_delivery_state)
                .await
        }

        async fn finish_invocation(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            self.inner.finish_invocation(plan, terminal).await
        }

        async fn begin_provider_attempt(
            &self,
            attempt: &astra_services::InferenceProviderAttemptPlan,
        ) -> astra_services::ServiceResult<()> {
            self.provider_attempts.fetch_add(1, Ordering::SeqCst);
            self.inner.begin_provider_attempt(attempt).await
        }

        async fn finish_provider_attempt(
            &self,
            attempt: &astra_services::InferenceProviderAttemptPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            self.inner.finish_provider_attempt(attempt, terminal).await
        }
    }

    #[async_trait]
    impl InferenceLedgerPersistence for AdmissionRecoveryFailurePersistence {
        async fn admit_invocation(
            &self,
            _plan: &astra_services::InferenceInvocationPlan,
        ) -> astra_services::ServiceResult<()> {
            self.admission_entered.fetch_add(1, Ordering::SeqCst);
            if self.mode == AdmissionRecoveryFailureMode::ConclusiveRejection {
                return Err(astra_services::ServiceError::not_found(
                    "test run authority is unavailable",
                ));
            }
            std::future::pending().await
        }

        async fn settle_uncertain_admission(
            &self,
            _plan: &astra_services::InferenceInvocationPlan,
            _terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<astra_services::InferenceInvocationAdmissionResolution>
        {
            self.uncertain_settlements.fetch_add(1, Ordering::SeqCst);
            match self.mode {
                AdmissionRecoveryFailureMode::ConclusiveRejection => {
                    panic!("conclusive admission rejection cannot require settlement")
                }
                AdmissionRecoveryFailureMode::ScopeUnavailable => {
                    Ok(astra_services::InferenceInvocationAdmissionResolution::ScopeUnavailable)
                }
                AdmissionRecoveryFailureMode::AuthorityLost => {
                    Ok(astra_services::InferenceInvocationAdmissionResolution::AuthorityLost)
                }
                AdmissionRecoveryFailureMode::ConflictingIdentity => {
                    Ok(astra_services::InferenceInvocationAdmissionResolution::ConflictingIdentity)
                }
                AdmissionRecoveryFailureMode::Stall => std::future::pending().await,
            }
        }

        async fn declare_settlement(
            &self,
            _plan: &astra_services::InferenceInvocationPlan,
            _terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            Ok(())
        }

        async fn declare_attempt_settlement(
            &self,
            _plan: &astra_services::InferenceInvocationPlan,
            _attempt: &astra_services::InferenceProviderAttemptPlan,
            _terminal: &astra_services::InferenceInvocationTerminal,
            _provider_delivery_state: astra_services::InferenceProviderDeliveryState,
        ) -> astra_services::ServiceResult<()> {
            Ok(())
        }

        async fn finish_invocation(
            &self,
            _plan: &astra_services::InferenceInvocationPlan,
            _terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            Ok(())
        }

        async fn begin_provider_attempt(
            &self,
            _attempt: &astra_services::InferenceProviderAttemptPlan,
        ) -> astra_services::ServiceResult<()> {
            self.provider_attempts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn finish_provider_attempt(
            &self,
            _attempt: &astra_services::InferenceProviderAttemptPlan,
            _terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            Ok(())
        }
    }

    fn test_invocation_plan() -> astra_services::InferenceInvocationPlan {
        test_invocation_plan_for("user-1", "session-1", "run-1", "agent_turn")
    }

    fn test_invocation_plan_for(
        user_id: &str,
        session_id: &str,
        run_id: &str,
        operation_id: &str,
    ) -> astra_services::InferenceInvocationPlan {
        astra_services::plan_inference_invocation(astra_services::InferenceInvocationInput {
            user_id: user_id.into(),
            scope: astra_turn_types::InferenceInvocationScope::Run {
                session_id: session_id.into(),
                run_id: run_id.into(),
                turn: 1,
                round: 1,
                operation_id: operation_id.into(),
                logical_attempt: 0,
            },
            run_authority: Some(astra_services::InferenceRunAdmissionAuthority {
                expected_owner_generation: 0,
                expected_owner_pod_id: "test-inference-owner".into(),
                expected_control_epoch: 0,
            }),
            offering_id: "offering-1".into(),
            resolved_model_name: "model-1".into(),
            upstream_model_name: "model-1".into(),
            provider: "openai".into(),
            purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
            execution_placement: astra_services::ModelExecutionPlacement::Server,
            access_kind: astra_services::ModelAccessKind::SelfHosted,
        })
        .unwrap()
    }

    fn test_provider_attempt(
        plan: &astra_services::InferenceInvocationPlan,
        attempt_index: u32,
    ) -> astra_services::InferenceProviderAttemptPlan {
        astra_services::plan_inference_provider_attempt(
            plan,
            attempt_index,
            astra_services::InferenceProviderWireIdentity::new(
                "openai_compatible",
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                2,
            )
            .expect("test provider wire"),
        )
    }

    #[tokio::test]
    async fn owner_heartbeat_loss_cancels_local_provider_authority() {
        let persistence = Arc::new(TestInferenceLedgerPersistence::default());
        let plan = test_invocation_plan();
        persistence
            .admit_invocation(&plan)
            .await
            .expect("admit heartbeat test invocation");
        let owner_lease = InferenceOwnerLease::start(persistence.clone(), plan.clone(), None);
        persistence.fence_owner_lease();
        InferenceOwnerLease::spawn_heartbeat_with_timing(
            owner_lease.clone(),
            persistence.clone(),
            plan,
            std::time::Duration::from_millis(5),
            std::time::Duration::from_millis(20),
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            owner_lease.cancel.cancelled(),
        )
        .await
        .expect("authoritative owner loss must cancel local provider work");
        assert!(owner_lease.is_lost());
        assert!(persistence.lock().owner_renewals >= 1);
        assert!(
            owner_lease
                .ensure_live("provider delivery authorization")
                .is_err()
        );
    }

    #[tokio::test]
    async fn owner_lease_loss_interrupts_an_inflight_nonstream_transport() {
        let persistence = Arc::new(TestInferenceLedgerPersistence::default());
        let plan = test_invocation_plan();
        let owner_lease = InferenceOwnerLease::start(persistence, plan, None);
        let lost = owner_lease.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            lost.mark_lost();
        });
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_for_provider_or_owner_cancel(
                std::future::pending::<Result<(), astra_core::ClassifiedError>>(),
                owner_lease,
            ),
        )
        .await
        .expect("owner fence must interrupt the in-flight transport")
        .expect_err("provider future cannot outlive its durable owner generation");
        assert_eq!(error.kind, astra_core::ErrorKind::ContractViolation);
        assert!(is_ledger_error(&error));
    }

    fn ledger_timeout_error(phase: &str) -> astra_core::ClassifiedError {
        astra_core::ClassifiedError::new(
            astra_core::ErrorKind::DatabaseError,
            format!("durable inference ledger timed out during {phase}"),
        )
        .with_details_json(
            json!({
                "source": INFERENCE_LEDGER_ERROR_SOURCE,
                "deadline": {
                    "scope": "inference_ledger",
                    "phase": phase
                }
            })
            .to_string(),
        )
    }

    #[tokio::test]
    async fn stalled_provider_terminal_hands_off_exact_debt_without_leaking_worker() {
        let persistence = Arc::new(StalledTerminalPersistence::default());
        let plan = test_invocation_plan();
        persistence
            .admit_invocation(&plan)
            .await
            .expect("admit logical invocation");
        let observer = Arc::new(DurableProviderAttemptObserver::new_with_persistence(
            persistence.clone(),
            plan.clone(),
            astra_services::ModelRequestContextSeed::server_default(),
        ));
        let invocation = DurableInferenceInvocation {
            persistence: persistence.clone(),
            plan,
            observer: observer.clone(),
            settlement_coordinator: observer.settlement_coordinator.clone(),
            owner_lease: observer.owner_lease.clone(),
        };
        let wire = ProviderWireRequestIdentity {
            protocol: crate::turn::llm::client::LlmProviderProtocol::OpenAiCompatible,
            provider_wire_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_string(),
            provider_wire_bytes: 2,
            composition: crate::turn::llm::client::ProviderWireComposition {
                provider_envelope_bytes: 2,
                ..Default::default()
            },
            fingerprints: Default::default(),
        };
        let attempt = observer
            .begin_attempt(&wire)
            .await
            .expect("admit provider attempt");
        let terminal = delivery_unknown_terminal_from_error(&astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "provider work deadline elapsed",
        ));
        let mut finisher = Box::pin(observer.finish_attempt(attempt, &terminal));
        tokio::select! {
            result = &mut finisher => panic!("provider terminal unexpectedly completed: {result:?}"),
            _ = async {
                while persistence.finish_entered.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            } => {}
        }

        let facts = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            invocation.provider_attempt_facts(),
        )
        .await
        .expect("host attempt projection must not wait for terminal persistence");
        assert_eq!(facts.len(), 1);
        assert!(facts[0].terminal.is_none());

        // This models the controlled observer deadline: dropping the exact DB
        // future must synchronously release its worker/permit.
        drop(finisher);
        assert_eq!(persistence.active_finish_workers.load(Ordering::SeqCst), 0);

        let error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "provider work deadline elapsed",
        );
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            invocation.finish_error(&error),
        )
        .await
        .expect("host logical settlement must hand off after the hard deadline")
        .expect("logical settlement handoff");

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !persistence.inner.has_explicit_settlement_debt()
                || persistence.active_finish_workers.load(Ordering::SeqCst) != 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("bounded owner must leave exact debt and release its worker");
        assert_eq!(
            persistence.finish_entered.load(Ordering::SeqCst),
            1,
            "the fixed coordinator records exact debt instead of spawning another stalled terminal writer"
        );

        persistence.inner.reconcile_settlement_debts();
        persistence.inner.assert_quiescent();
    }

    #[tokio::test]
    async fn settlement_coordinator_bounds_workers_and_fails_closed_before_admission() {
        let persistence = Arc::new(StalledSettlementDeclarationPersistence::default());
        let coordinator = ProviderSettlementCoordinator::new_with_waiting_capacity(3, 0, 1);
        let plan = test_invocation_plan();
        let terminal = terminal_from_error(&astra_core::ClassifiedError::new(
            astra_core::ErrorKind::Cancelled,
            "pre-delivery cancellation",
        ));

        for attempt_index in 0..3 {
            let reservation = coordinator
                .reserve(SettlementAdmissionOwner::for_test("user-a", "session-a"))
                .await
                .expect("capacity is reserved before provider delivery");
            coordinator.handoff(ProviderSettlementJob {
                persistence: persistence.clone(),
                invocation: plan.clone(),
                task: ProviderSettlementTask::Debt {
                    attempt: Box::new(Some(test_provider_attempt(&plan, attempt_index))),
                    terminal: terminal.clone(),
                    provider_delivery_state:
                        astra_services::InferenceProviderDeliveryState::PreDelivery,
                },
                owner_lease: None,
                _reservation: reservation,
            });
        }
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while persistence.active_declarations.load(Ordering::SeqCst) != 1
                || coordinator.queued_jobs() != 2
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("one fixed worker and two bounded queued items");
        assert_eq!(
            persistence.max_active_declarations.load(Ordering::SeqCst),
            1,
            "a permanently pending database must not create one task per invocation"
        );

        let mut ledger = test_ledger_with_persistence("http://127.0.0.1:1", persistence.clone());
        ledger.settlement_coordinator = coordinator.clone();
        let saturation = match ledger
            .admit(
                test_scope("settlement_capacity"),
                astra_turn_types::InferencePurpose::PrimaryAgent,
                "model-test",
                "model-test",
                "openai",
            )
            .await
        {
            Ok(_) => panic!("the next invocation must fail before durable or provider admission"),
            Err(failure) => failure.error,
        };
        assert_eq!(saturation.kind, astra_core::ErrorKind::ResourceLimit);
        assert_eq!(
            persistence.admit_calls.load(Ordering::SeqCst),
            0,
            "capacity backpressure must precede even logical admission"
        );

        persistence.allow_declaration.store(true, Ordering::SeqCst);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while coordinator.available_permits() != 3
                || persistence.active_declarations.load(Ordering::SeqCst) != 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("confirmed debt declarations release every reservation");

        let admitted = ledger
            .admit(
                test_scope("settlement_capacity_recovered"),
                astra_turn_types::InferencePurpose::PrimaryAgent,
                "model-test",
                "model-test",
                "openai",
            )
            .await
            .map_err(|failure| failure.error)
            .expect("released capacity admits a later invocation");
        assert_eq!(persistence.admit_calls.load(Ordering::SeqCst), 1);
        drop(admitted);
        assert!(
            coordinator
                .close_and_drain(std::time::Duration::from_secs(1))
                .await,
            "shutdown reports clean only after every reservation is released"
        );
        assert!(
            coordinator
                .reserve(SettlementAdmissionOwner::for_test("user-a", "session-a"))
                .await
                .is_err(),
            "shutdown must permanently fence new provider admission"
        );
    }

    #[tokio::test]
    async fn permanent_settlement_incidents_release_all_capacity_for_a_healthy_user() {
        let coordinator = ProviderSettlementCoordinator::new(2, 1);
        let terminal = terminal_from_error(&astra_core::ClassifiedError::new(
            astra_core::ErrorKind::Cancelled,
            "permanent settlement incident",
        ));

        for index in 0..2 {
            let persistence = Arc::new(ControlledReconcilePersistence::default());
            persistence
                .permanently_quarantined
                .store(true, Ordering::SeqCst);
            let user_id = format!("quarantined-user-{index}");
            let session_id = format!("quarantined-session-{index}");
            let run_id = format!("quarantined-run-{index}");
            let plan =
                test_invocation_plan_for(&user_id, &session_id, &run_id, "permanent_settlement");
            persistence
                .admit_invocation(&plan)
                .await
                .expect("admit quarantined test invocation");
            let reservation = coordinator
                .reserve(SettlementAdmissionOwner::for_test(&user_id, &session_id))
                .await
                .expect("reserve permanent incident owner");
            coordinator.handoff(ProviderSettlementJob {
                persistence,
                invocation: plan,
                task: ProviderSettlementTask::Debt {
                    attempt: Box::new(None),
                    terminal: terminal.clone(),
                    provider_delivery_state:
                        astra_services::InferenceProviderDeliveryState::PreDelivery,
                },
                owner_lease: None,
                _reservation: reservation,
            });
        }

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while coordinator.available_permits() != 2
                || coordinator
                    .metrics
                    .permanently_quarantined
                    .load(Ordering::SeqCst)
                    != 2
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("permanent incidents must release every reservation without retrying");
        assert_eq!(coordinator.queued_jobs(), 0);

        let healthy = Arc::new(TestInferenceLedgerPersistence::default());
        let mut ledger = test_ledger_with_persistence("http://127.0.0.1:1", healthy.clone());
        ledger.settlement_coordinator = coordinator.clone();
        let invocation = ledger
            .admit(
                test_scope("healthy_after_permanent_incidents"),
                astra_turn_types::InferencePurpose::PrimaryAgent,
                "model-test",
                "model-test",
                "openai",
            )
            .await
            .map_err(|failure| failure.error)
            .expect("healthy user admission must not be starved by quarantined incidents");
        invocation
            .finish_error(&astra_core::ClassifiedError::new(
                astra_core::ErrorKind::Cancelled,
                "finish healthy test invocation",
            ))
            .await
            .expect("finish healthy test invocation");
        wait_for_quiescent(&healthy).await;
        assert_eq!(coordinator.available_permits(), 2);
    }

    #[tokio::test]
    async fn worker_supervisor_replays_exact_job_after_four_panics_and_serves_healthy_user() {
        let coordinator = ProviderSettlementCoordinator::new(2, 1);
        let terminal = terminal_from_error(&astra_core::ClassifiedError::new(
            astra_core::ErrorKind::Cancelled,
            "supervised settlement",
        ));
        let poison = Arc::new(ControlledReconcilePersistence::default());
        poison.panics_remaining.store(4, Ordering::SeqCst);
        let poison_plan = test_invocation_plan_for(
            "panic-user",
            "panic-session",
            "panic-run",
            "panic_settlement",
        );
        poison
            .admit_invocation(&poison_plan)
            .await
            .expect("admit panic test invocation");
        let healthy = Arc::new(ControlledReconcilePersistence::default());
        let healthy_plan = test_invocation_plan_for(
            "healthy-user",
            "healthy-session",
            "healthy-run",
            "healthy_settlement",
        );
        healthy
            .admit_invocation(&healthy_plan)
            .await
            .expect("admit healthy test invocation");

        for (owner, persistence, plan) in [
            (
                SettlementAdmissionOwner::for_test("panic-user", "panic-session"),
                poison.clone(),
                poison_plan,
            ),
            (
                SettlementAdmissionOwner::for_test("healthy-user", "healthy-session"),
                healthy.clone(),
                healthy_plan,
            ),
        ] {
            let reservation = coordinator
                .reserve(owner)
                .await
                .expect("reserve supervised settlement owner");
            coordinator.handoff(ProviderSettlementJob {
                persistence,
                invocation: plan,
                task: ProviderSettlementTask::Debt {
                    attempt: Box::new(None),
                    terminal: terminal.clone(),
                    provider_delivery_state:
                        astra_services::InferenceProviderDeliveryState::PreDelivery,
                },
                owner_lease: None,
                _reservation: reservation,
            });
        }

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while coordinator.available_permits() != 2
                || poison.reconcile_calls.load(Ordering::SeqCst) != 5
                || healthy.reconcile_calls.load(Ordering::SeqCst) != 1
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("supervisor must replace panicked attempts without losing either exact job");
        assert_eq!(coordinator.metrics.worker_panics.load(Ordering::SeqCst), 4);
        assert_eq!(coordinator.queued_jobs(), 0);
        poison.inner.assert_quiescent();
        healthy.inner.assert_quiescent();
    }

    #[test]
    fn process_scoped_settlement_workers_outlive_the_creator_runtime() {
        let coordinator = ProviderSettlementCoordinator::new_process_scoped(1, 1);
        let first_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("first caller runtime");
        first_runtime.block_on(async {
            coordinator
                .ensure_workers()
                .expect("start process-scoped workers");
            tokio::task::yield_now().await;
        });
        drop(first_runtime);

        let persistence = Arc::new(StalledSettlementDeclarationPersistence::default());
        persistence.allow_declaration.store(true, Ordering::SeqCst);
        let plan = test_invocation_plan();
        let terminal = terminal_from_error(&astra_core::ClassifiedError::new(
            astra_core::ErrorKind::Cancelled,
            "pre-delivery cancellation",
        ));
        let second_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("second caller runtime");
        second_runtime.block_on(async {
            let reservation = coordinator
                .reserve(SettlementAdmissionOwner::for_test("user-a", "session-a"))
                .await
                .expect("settlement reservation");
            coordinator.handoff(ProviderSettlementJob {
                persistence,
                invocation: plan.clone(),
                task: ProviderSettlementTask::Debt {
                    attempt: Box::new(Some(test_provider_attempt(&plan, 0))),
                    terminal,
                    provider_delivery_state:
                        astra_services::InferenceProviderDeliveryState::PreDelivery,
                },
                owner_lease: None,
                _reservation: reservation,
            });

            tokio::time::timeout(std::time::Duration::from_millis(250), async {
                while coordinator.available_permits() != 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("settlement worker must survive the caller runtime that first initialized it");
            assert!(
                coordinator
                    .close_and_drain(std::time::Duration::from_secs(1))
                    .await
            );
        });
    }

    #[tokio::test]
    async fn shutdown_drain_keeps_workers_for_a_reserved_late_handoff() {
        let coordinator = ProviderSettlementCoordinator::new(1, 1);
        let reservation = coordinator
            .reserve(SettlementAdmissionOwner::for_test("user-a", "session-a"))
            .await
            .expect("settlement reservation");
        let drain_coordinator = coordinator.clone();
        let drain = tokio::spawn(async move {
            drain_coordinator
                .close_and_drain(std::time::Duration::from_secs(1))
                .await
        });
        while coordinator
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .admission_open
        {
            tokio::task::yield_now().await;
        }

        let persistence = Arc::new(StalledSettlementDeclarationPersistence::default());
        persistence.allow_declaration.store(true, Ordering::SeqCst);
        let plan = test_invocation_plan();
        coordinator.handoff(ProviderSettlementJob {
            persistence,
            invocation: plan.clone(),
            task: ProviderSettlementTask::Debt {
                attempt: Box::new(Some(test_provider_attempt(&plan, 0))),
                terminal: terminal_from_error(&astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::Cancelled,
                    "pre-delivery cancellation",
                )),
                provider_delivery_state:
                    astra_services::InferenceProviderDeliveryState::PreDelivery,
            },
            owner_lease: None,
            _reservation: reservation,
        });

        assert!(
            drain.await.expect("join shutdown drain"),
            "a settlement reserved before shutdown must still be processed after admission closes"
        );
    }

    #[tokio::test]
    async fn ambiguous_logical_admission_recovers_once_without_duplicate_provider_io() {
        for commit_before_ack_loss in [false, true] {
            let provider_requests = Arc::new(AtomicU32::new(0));
            let provider_requests_for_handler = provider_requests.clone();
            let app = Router::new().route(
                "/chat/completions",
                post(move || {
                    let provider_requests = provider_requests_for_handler.clone();
                    async move {
                        provider_requests.fetch_add(1, Ordering::SeqCst);
                        Response::builder()
                            .status(200)
                            .header("content-type", "application/json")
                            .body(Body::from(
                                r#"{"id":"recovered-response","choices":[{"message":{"content":"answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":2}}"#,
                            ))
                            .unwrap()
                    }
                }),
            );
            let base = spawn_test_server(app).await;
            let persistence = Arc::new(AmbiguousLogicalAdmissionPersistence::default());
            persistence
                .commit_before_ack_loss
                .store(commit_before_ack_loss, Ordering::SeqCst);
            let coordinator = ProviderSettlementCoordinator::new(2, 1);
            let mut ledger = test_ledger_with_persistence(&base, persistence.clone());
            ledger.settlement_coordinator = coordinator.clone();
            let messages = vec![serde_json::json!({"role":"user","content":"x"})];
            let client = reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("test client");

            let result = ledger
                .execute_nonstream(
                    &client,
                    test_scope(if commit_before_ack_loss {
                        "logical_commit_ack_lost"
                    } else {
                        "logical_rollback_ack_lost"
                    }),
                    test_call(&base, &messages),
                    std::time::Duration::from_secs(1),
                )
                .await
                .into_result()
                .expect("one foreground recovery should preserve the provider call");

            assert_eq!(result.full_text, "answer");
            assert_eq!(persistence.uncertain_settlements.load(Ordering::SeqCst), 1);
            assert_eq!(persistence.provider_attempts.load(Ordering::SeqCst), 1);
            assert_eq!(provider_requests.load(Ordering::SeqCst), 1);
            assert_eq!(
                *persistence
                    .admitted_logical_attempts
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                vec![0, 1]
            );
            persistence.inner.reconcile_settlement_debts();
            persistence.inner.assert_quiescent();
            let statuses = persistence.inner.logical_terminal_statuses();
            assert_eq!(statuses.len(), 2);
            assert_eq!(
                statuses
                    .iter()
                    .filter(|status| {
                        **status == astra_services::InferenceTerminalStatus::Cancelled
                    })
                    .count(),
                1
            );
            assert_eq!(
                statuses
                    .iter()
                    .filter(|status| {
                        **status == astra_services::InferenceTerminalStatus::Succeeded
                    })
                    .count(),
                1
            );
            assert!(
                coordinator
                    .close_and_drain(std::time::Duration::from_secs(1))
                    .await
            );
        }
    }

    #[tokio::test]
    async fn admission_recovery_fails_closed_when_scope_or_fencing_authority_is_lost() {
        for mode in [
            AdmissionRecoveryFailureMode::ScopeUnavailable,
            AdmissionRecoveryFailureMode::AuthorityLost,
            AdmissionRecoveryFailureMode::ConflictingIdentity,
        ] {
            let persistence = Arc::new(AdmissionRecoveryFailurePersistence::new(mode));
            let coordinator = ProviderSettlementCoordinator::new(1, 1);
            let mut ledger =
                test_ledger_with_persistence("http://127.0.0.1:1", persistence.clone());
            ledger.settlement_coordinator = coordinator.clone();

            let error = match ledger
                .admit(
                    test_scope("authority_lost_during_admission_recovery"),
                    astra_turn_types::InferencePurpose::PrimaryAgent,
                    "model-test",
                    "model-test",
                    "openai",
                )
                .await
            {
                Ok(_) => panic!("lost scope or fencing authority must not retry provider delivery"),
                Err(failure) => failure.error,
            };

            assert_eq!(error.kind, astra_core::ErrorKind::ContractViolation);
            assert_eq!(persistence.uncertain_settlements.load(Ordering::SeqCst), 1);
            assert_eq!(persistence.provider_attempts.load(Ordering::SeqCst), 0);
            assert_eq!(
                coordinator.available_permits(),
                1,
                "a conclusive authority loss must release its global reservation"
            );
            assert!(
                coordinator
                    .close_and_drain(std::time::Duration::from_secs(1))
                    .await
            );
        }
    }

    #[tokio::test]
    async fn conclusive_admission_rejection_releases_capacity_without_settlement_work() {
        let persistence = Arc::new(AdmissionRecoveryFailurePersistence::new(
            AdmissionRecoveryFailureMode::ConclusiveRejection,
        ));
        let coordinator = ProviderSettlementCoordinator::new(1, 1);
        let mut ledger = test_ledger_with_persistence("http://127.0.0.1:1", persistence.clone())
            .with_run_authority(DurableInferenceRunAuthority::new(
                0,
                "test-inference-owner",
                0,
                None,
                None,
                None,
            ));
        ledger.settlement_coordinator = coordinator.clone();

        let failure = match ledger
            .admit(
                test_scope("conclusive_admission_rejection"),
                astra_turn_types::InferencePurpose::PrimaryAgent,
                "model-test",
                "model-test",
                "openai",
            )
            .await
        {
            Ok(_) => panic!("a conclusive scope rejection cannot authorize provider delivery"),
            Err(failure) => failure,
        };

        assert_eq!(failure.logical_attempt, 0);
        assert_eq!(failure.error.kind, astra_core::ErrorKind::InvalidRequest);
        assert_eq!(persistence.uncertain_settlements.load(Ordering::SeqCst), 0);
        assert_eq!(persistence.provider_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(coordinator.available_permits(), 1);
        assert!(
            coordinator
                .close_and_drain(std::time::Duration::from_secs(1))
                .await
        );
    }

    #[tokio::test]
    async fn cancellation_interrupts_ambiguous_admission_without_provider_delivery() {
        let persistence = Arc::new(AdmissionRecoveryFailurePersistence::new(
            AdmissionRecoveryFailureMode::AuthorityLost,
        ));
        let coordinator = ProviderSettlementCoordinator::new(1, 1);
        let cancel_token = Arc::new(tokio_util::sync::CancellationToken::new());
        let mut ledger = test_ledger_with_persistence("http://127.0.0.1:1", persistence.clone())
            .with_run_authority(DurableInferenceRunAuthority::new(
                0,
                "test-inference-owner",
                0,
                None,
                Some(cancel_token.clone()),
                None,
            ));
        ledger.settlement_coordinator = coordinator.clone();

        let admission = tokio::spawn(async move {
            ledger
                .admit(
                    test_scope("cancel_ambiguous_admission"),
                    astra_turn_types::InferencePurpose::PrimaryAgent,
                    "model-test",
                    "model-test",
                    "openai",
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while persistence.admission_entered.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("admission must reach the ambiguous database boundary");
        cancel_token.cancel();

        let admission_result =
            tokio::time::timeout(std::time::Duration::from_millis(250), admission)
                .await
                .expect("cancel must interrupt the foreground database wait")
                .expect("admission task must join");
        let failure = match admission_result {
            Ok(_) => panic!("cancelled admission cannot authorize provider delivery"),
            Err(failure) => failure,
        };
        assert_eq!(failure.error.kind, astra_core::ErrorKind::Cancelled);
        assert_eq!(failure.logical_attempt, 0);
        assert_eq!(persistence.provider_attempts.load(Ordering::SeqCst), 0);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while persistence.uncertain_settlements.load(Ordering::SeqCst) == 0
                || coordinator.available_permits() != 1
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the exact abandoned identity must settle without leaking capacity");
        assert!(
            coordinator
                .close_and_drain(std::time::Duration::from_secs(1))
                .await
        );
    }

    #[tokio::test]
    async fn lease_loss_interrupts_ambiguous_admission_without_replacement_delivery() {
        let persistence = Arc::new(AdmissionRecoveryFailurePersistence::new(
            AdmissionRecoveryFailureMode::AuthorityLost,
        ));
        let coordinator = ProviderSettlementCoordinator::new(1, 1);
        let lease_lost = Arc::new(AtomicBool::new(false));
        let mut ledger = test_ledger_with_persistence("http://127.0.0.1:1", persistence.clone())
            .with_run_authority(DurableInferenceRunAuthority::new(
                0,
                "test-inference-owner",
                0,
                None,
                None,
                Some(lease_lost.clone()),
            ));
        ledger.settlement_coordinator = coordinator.clone();

        let admission = tokio::spawn(async move {
            ledger
                .admit(
                    test_scope("lease_lost_ambiguous_admission"),
                    astra_turn_types::InferencePurpose::PrimaryAgent,
                    "model-test",
                    "model-test",
                    "openai",
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while persistence.admission_entered.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("admission must reach the ambiguous database boundary");
        lease_lost.store(true, Ordering::Release);

        let admission_result =
            tokio::time::timeout(std::time::Duration::from_millis(250), admission)
                .await
                .expect("lease loss must interrupt the foreground database wait")
                .expect("admission task must join");
        let failure = match admission_result {
            Ok(_) => panic!("lease-lost admission cannot authorize provider delivery"),
            Err(failure) => failure,
        };
        assert_eq!(failure.error.kind, astra_core::ErrorKind::ContractViolation);
        assert!(
            failure
                .error
                .details_json
                .as_deref()
                .is_some_and(|details| { details.contains("execution_lease_lost") })
        );
        assert_eq!(failure.logical_attempt, 0);
        assert_eq!(persistence.provider_attempts.load(Ordering::SeqCst), 0);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while persistence.uncertain_settlements.load(Ordering::SeqCst) == 0
                || coordinator.available_permits() != 1
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("lease-lost identity must settle without leaking capacity");
        assert!(
            coordinator
                .close_and_drain(std::time::Duration::from_secs(1))
                .await
        );
    }

    #[tokio::test]
    async fn repeated_admission_ambiguity_is_bounded_to_one_fresh_identity() {
        let persistence = Arc::new(AmbiguousLogicalAdmissionPersistence::default());
        persistence
            .stall_retry_admission
            .store(true, Ordering::SeqCst);
        let coordinator = ProviderSettlementCoordinator::new(1, 1);
        let mut ledger = test_ledger_with_persistence("http://127.0.0.1:1", persistence.clone());
        ledger.settlement_coordinator = coordinator.clone();

        let error = match ledger
            .admit(
                test_scope("repeated_admission_ambiguity"),
                astra_turn_types::InferencePurpose::PrimaryAgent,
                "model-test",
                "model-test",
                "openai",
            )
            .await
        {
            Ok(_) => panic!("a second ambiguous identity must not cause an unbounded retry loop"),
            Err(failure) => failure.error,
        };

        assert_eq!(error.kind, astra_core::ErrorKind::DatabaseError);
        assert!(
            error
                .details_json
                .as_deref()
                .is_some_and(|details| { details.contains("logical_invocation_retry_admission") })
        );
        assert_eq!(persistence.uncertain_settlements.load(Ordering::SeqCst), 2);
        assert_eq!(persistence.provider_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(
            *persistence
                .admitted_logical_attempts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![0, 1]
        );
        persistence.inner.reconcile_settlement_debts();
        persistence.inner.assert_quiescent();
        assert!(
            persistence
                .inner
                .logical_terminal_statuses()
                .iter()
                .all(|status| *status == astra_services::InferenceTerminalStatus::Cancelled)
        );
        assert_eq!(coordinator.available_permits(), 1);
        assert!(
            coordinator
                .close_and_drain(std::time::Duration::from_secs(1))
                .await
        );
    }

    #[tokio::test]
    async fn admission_recovery_timeout_retains_bounded_owner_without_provider_io() {
        let persistence = Arc::new(AdmissionRecoveryFailurePersistence::new(
            AdmissionRecoveryFailureMode::Stall,
        ));
        let coordinator = ProviderSettlementCoordinator::new(1, 1);
        let mut ledger = test_ledger_with_persistence("http://127.0.0.1:1", persistence.clone());
        ledger.settlement_coordinator = coordinator.clone();

        let error = match ledger
            .admit(
                test_scope("admission_recovery_timeout"),
                astra_turn_types::InferencePurpose::PrimaryAgent,
                "model-test",
                "model-test",
                "openai",
            )
            .await
        {
            Ok(_) => panic!("an unresolved recovery must remain fail closed"),
            Err(failure) => failure.error,
        };

        assert_eq!(error.kind, astra_core::ErrorKind::DatabaseError);
        assert!(
            error
                .details_json
                .as_deref()
                .is_some_and(|details| details.contains("logical_invocation_admission_recovery"))
        );
        assert_eq!(persistence.provider_attempts.load(Ordering::SeqCst), 0);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while persistence.uncertain_settlements.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the same bounded reservation must transfer to background recovery");
        assert_eq!(coordinator.available_permits(), 0);
        assert!(
            !coordinator
                .close_and_drain(std::time::Duration::from_millis(20))
                .await,
            "unresolved authority must not be released or silently redelivered"
        );
    }

    #[tokio::test]
    async fn dropping_admitted_invocation_hands_off_terminal_owner() {
        let persistence = Arc::new(TestInferenceLedgerPersistence::default());
        let coordinator = ProviderSettlementCoordinator::new(2, 1);
        let mut ledger = test_ledger_with_persistence("http://127.0.0.1:1", persistence.clone());
        ledger.settlement_coordinator = coordinator.clone();
        let invocation = ledger
            .admit(
                test_scope("drop_admitted_invocation"),
                astra_turn_types::InferencePurpose::PrimaryAgent,
                "model-test",
                "model-test",
                "openai",
            )
            .await
            .map_err(|failure| failure.error)
            .expect("logical admission");

        drop(invocation);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !persistence.has_explicit_settlement_debt() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("final observer Drop must synchronously hand off its reservation");
        persistence.reconcile_settlement_debts();
        persistence.assert_quiescent();
        assert_eq!(
            persistence.logical_terminal_statuses(),
            vec![astra_services::InferenceTerminalStatus::Cancelled]
        );
        assert!(
            coordinator
                .close_and_drain(std::time::Duration::from_secs(1))
                .await
        );
    }

    #[tokio::test]
    async fn settlement_coordinator_rotates_poison_jobs_without_starving_later_users() {
        let persistence = Arc::new(StalledSettlementDeclarationPersistence::default());
        persistence.allow_declaration.store(true, Ordering::SeqCst);
        persistence
            .permanently_reject_attempt_zero
            .store(true, Ordering::SeqCst);
        let coordinator = ProviderSettlementCoordinator::new_with_fair_limits(8, 16, 1, 6, 4);
        let plan = test_invocation_plan();
        let terminal = terminal_from_error(&astra_core::ClassifiedError::new(
            astra_core::ErrorKind::Cancelled,
            "pre-delivery cancellation",
        ));

        for reservation_index in 0..6 {
            let reservation = coordinator
                .reserve(SettlementAdmissionOwner::for_test(
                    "user-a",
                    if reservation_index < 4 {
                        "session-a"
                    } else {
                        "session-b"
                    },
                ))
                .await
                .expect("settlement reservation");
            coordinator.handoff(ProviderSettlementJob {
                persistence: persistence.clone(),
                invocation: plan.clone(),
                task: ProviderSettlementTask::Debt {
                    attempt: Box::new(Some(test_provider_attempt(&plan, 0))),
                    terminal: terminal.clone(),
                    provider_delivery_state:
                        astra_services::InferenceProviderDeliveryState::PreDelivery,
                },
                owner_lease: None,
                _reservation: reservation,
            });
        }
        let noisy_coordinator = coordinator.clone();
        let noisy_waiter = tokio::spawn(async move {
            noisy_coordinator
                .reserve(SettlementAdmissionOwner::for_test("user-a", "session-c"))
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while coordinator.queued_admissions() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the noisy user reaches its hard active share");
        let waiting_coordinator = coordinator.clone();
        let healthy_user = tokio::spawn(async move {
            waiting_coordinator
                .reserve(SettlementAdmissionOwner::for_test("user-b", "session-b"))
                .await
        });

        let healthy_user_reservation =
            tokio::time::timeout(std::time::Duration::from_secs(1), healthy_user)
                .await
                .expect("a poison owner must not starve a later user")
                .expect("join healthy user reservation")
                .expect("healthy user receives the released reservation");
        coordinator.handoff(ProviderSettlementJob {
            persistence: persistence.clone(),
            invocation: plan.clone(),
            task: ProviderSettlementTask::Debt {
                attempt: Box::new(Some(test_provider_attempt(&plan, 1))),
                terminal,
                provider_delivery_state:
                    astra_services::InferenceProviderDeliveryState::PreDelivery,
            },
            owner_lease: None,
            _reservation: healthy_user_reservation,
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while coordinator.available_permits() != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the healthy user's fair-queued settlement must complete");
        assert!(persistence.declaration_calls.load(Ordering::SeqCst) >= 2);
        noisy_waiter.abort();
        let _ = noisy_waiter.await;
        assert!(
            !coordinator
                .close_and_drain(std::time::Duration::from_millis(20))
                .await,
            "the poison item remains explicitly unconfirmed at shutdown"
        );
    }

    #[tokio::test]
    async fn settlement_admission_is_user_and_session_round_robin_under_contention() {
        let coordinator = ProviderSettlementCoordinator::new_with_waiting_capacity(1, 16, 1);
        let first = coordinator
            .reserve(SettlementAdmissionOwner::for_test("user-a", "session-a"))
            .await
            .expect("initial reservation");
        let (granted_tx, mut granted_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut releases = Vec::new();
        for (label, user, session) in [
            ("a1", "user-a", "session-a"),
            ("a2", "user-a", "session-a"),
            ("a-other-session", "user-a", "session-b"),
            ("b1", "user-b", "session-a"),
        ] {
            let task_coordinator = coordinator.clone();
            let granted_tx = granted_tx.clone();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            releases.push((label, release_tx));
            tokio::spawn(async move {
                let reservation = task_coordinator
                    .reserve(SettlementAdmissionOwner::for_test(user, session))
                    .await
                    .expect("queued fair reservation");
                granted_tx.send(label).expect("record fair grant");
                let _ = release_rx.await;
                drop(reservation);
            });
            let expected_queued = releases.len();
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while coordinator.queued_admissions() != expected_queued {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("contender enters the fair queue in deterministic order");
        }
        drop(first);

        let expected = ["a1", "b1", "a-other-session", "a2"];
        for expected_label in expected {
            let actual = tokio::time::timeout(std::time::Duration::from_secs(1), granted_rx.recv())
                .await
                .expect("fair grant arrives")
                .expect("grant channel remains open");
            assert_eq!(actual, expected_label);
            let (_, release) = releases
                .iter_mut()
                .find(|(label, _)| *label == actual)
                .expect("matching release");
            let release = std::mem::replace(release, tokio::sync::oneshot::channel().0);
            let _ = release.send(());
        }
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while coordinator.available_permits() != 1 || coordinator.queued_admissions() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("all fair reservations release");
    }

    #[test]
    fn settlement_rr_tickets_remain_unique_and_ordered_across_integer_carries() {
        let mut ticket = SettlementRrTicket {
            high: Vec::new(),
            low: u64::MAX - 1,
        };
        let before_low_carry = ticket.clone();
        ticket.increment();
        let at_low_max = ticket.clone();
        ticket.increment();
        let after_low_carry = ticket.clone();
        assert!(before_low_carry < at_low_max);
        assert!(at_low_max < after_low_carry);

        ticket = SettlementRrTicket {
            high: vec![u64::MAX],
            low: u64::MAX,
        };
        let before_high_extension = ticket.clone();
        ticket.increment();
        let after_high_extension = ticket;
        assert!(before_high_extension < after_high_extension);

        let unique = BTreeSet::from([
            before_low_carry,
            at_low_max,
            after_low_carry,
            before_high_extension,
            after_high_extension,
        ]);
        assert_eq!(unique.len(), 5);
    }

    #[tokio::test]
    async fn ineligible_tenant_waiters_cannot_strand_idle_fair_capacity() {
        let coordinator = ProviderSettlementCoordinator::new_with_fair_limits(8, 2, 1, 6, 6);
        let mut active_a = Vec::new();
        for _ in 0..6 {
            active_a.push(
                coordinator
                    .reserve(SettlementAdmissionOwner::for_test("user-a", "session-a"))
                    .await
                    .expect("fill tenant A active share"),
            );
        }

        let waiting_coordinator = coordinator.clone();
        let waiting_a = tokio::spawn(async move {
            waiting_coordinator
                .reserve(SettlementAdmissionOwner::for_test("user-a", "session-a"))
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while coordinator.queued_admissions() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("tenant A reaches its exact waiting share");

        let overflow = match coordinator
            .reserve(SettlementAdmissionOwner::for_test("user-a", "session-a"))
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("one tenant cannot consume the final global waiting slot"),
        };
        assert!(
            overflow
                .details_json
                .as_deref()
                .is_some_and(|details| details.contains("wait_session_share_full"))
        );

        let active_b = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            coordinator.reserve(SettlementAdmissionOwner::for_test("user-b", "session-b")),
        )
        .await
        .expect("eligible tenant B must use idle fair capacity in O(1)")
        .expect("tenant B receives an immediate reservation");
        assert_eq!(coordinator.available_permits(), 1);
        assert_eq!(coordinator.queued_admissions(), 1);

        drop(active_b);
        drop(active_a.pop());
        let granted_a = tokio::time::timeout(std::time::Duration::from_secs(1), waiting_a)
            .await
            .expect("tenant A waiter is granted after its active share falls")
            .expect("join tenant A waiter")
            .expect("tenant A waiter receives reservation");
        drop(granted_a);
        drop(active_a);
        assert_eq!(coordinator.available_permits(), 8);
        assert_eq!(coordinator.queued_admissions(), 0);
    }

    #[tokio::test]
    async fn global_saturation_preserves_a_wait_slot_and_next_release_for_another_tenant() {
        let coordinator = ProviderSettlementCoordinator::new_with_fair_limits(8, 2, 1, 6, 6);
        let mut active_a = Vec::new();
        for _ in 0..6 {
            active_a.push(
                coordinator
                    .reserve(SettlementAdmissionOwner::for_test("user-a", "session-a"))
                    .await
                    .expect("fill tenant A active share"),
            );
        }
        let mut blockers = Vec::new();
        for _ in 0..2 {
            blockers.push(
                coordinator
                    .reserve(SettlementAdmissionOwner::for_test(
                        "blocking-user",
                        "blocking-session",
                    ))
                    .await
                    .expect("fill remaining global capacity"),
            );
        }
        assert_eq!(coordinator.available_permits(), 0);

        let waiter_a_coordinator = coordinator.clone();
        let waiter_a = tokio::spawn(async move {
            waiter_a_coordinator
                .reserve(SettlementAdmissionOwner::for_test("user-a", "session-a"))
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while coordinator.queued_admissions() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("tenant A occupies only its waiting share");
        let waiter_b_coordinator = coordinator.clone();
        let waiter_b = tokio::spawn(async move {
            waiter_b_coordinator
                .reserve(SettlementAdmissionOwner::for_test("user-b", "session-b"))
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while coordinator.queued_admissions() != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("tenant B retains the final global waiting slot");

        drop(blockers.pop());
        let granted_b = tokio::time::timeout(std::time::Duration::from_secs(1), waiter_b)
            .await
            .expect("next fair release skips only the ineligible A waiter")
            .expect("join tenant B waiter")
            .expect("tenant B receives the released reservation");
        assert_eq!(coordinator.queued_admissions(), 1);
        assert!(!waiter_a.is_finished());

        drop(granted_b);
        drop(active_a.pop());
        let granted_a = tokio::time::timeout(std::time::Duration::from_secs(1), waiter_a)
            .await
            .expect("tenant A is granted once its active share permits")
            .expect("join tenant A waiter")
            .expect("tenant A receives the later reservation");
        drop(granted_a);
        drop(active_a);
        drop(blockers);
        assert_eq!(coordinator.available_permits(), 8);
        assert_eq!(coordinator.queued_admissions(), 0);
        let state = coordinator
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(state.users.is_empty());
        assert!(state.ready_users.is_empty());
        assert!(state.queued_by_user.is_empty());
        assert!(state.queued_by_session.is_empty());
    }

    #[tokio::test]
    async fn release_work_is_bounded_when_a_noisy_tenant_has_1536_ineligible_waiters() {
        const NOISY_WAITERS: usize = 1_536;
        let coordinator = ProviderSettlementCoordinator::new_with_fair_limits(8, 2_048, 1, 6, 6);
        let mut active_a = Vec::new();
        for _ in 0..6 {
            active_a.push(
                coordinator
                    .reserve(SettlementAdmissionOwner::for_test("user-a", "session-a"))
                    .await
                    .expect("fill noisy tenant active share"),
            );
        }
        let mut blockers = Vec::new();
        for _ in 0..2 {
            blockers.push(
                coordinator
                    .reserve(SettlementAdmissionOwner::for_test(
                        "blocking-user",
                        "blocking-session",
                    ))
                    .await
                    .expect("fill remaining global capacity"),
            );
        }

        let mut noisy_waiters = Vec::with_capacity(NOISY_WAITERS);
        for index in 0..NOISY_WAITERS {
            let coordinator = coordinator.clone();
            noisy_waiters.push(tokio::spawn(async move {
                coordinator
                    .reserve(SettlementAdmissionOwner::for_test(
                        "user-a",
                        if index + 1 == NOISY_WAITERS {
                            "session-b"
                        } else {
                            "session-a"
                        },
                    ))
                    .await
            }));
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while coordinator.queued_admissions() != NOISY_WAITERS {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("all noisy waiters enter the bounded queue");

        let examined_before = coordinator
            .metrics
            .admission_ready_candidates_examined
            .load(Ordering::Relaxed);
        drop(blockers.pop());
        let examined = coordinator
            .metrics
            .admission_ready_candidates_examined
            .load(Ordering::Relaxed)
            .saturating_sub(examined_before);
        assert!(
            examined <= 1,
            "one release examined {examined} waiters behind the coordinator mutex"
        );

        let healthy_b = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            coordinator.reserve(SettlementAdmissionOwner::for_test(
                "healthy-user-b",
                "healthy-session-b",
            )),
        )
        .await
        .expect("healthy user B latency must not depend on tenant A queue depth")
        .expect("healthy user B uses idle fair capacity");
        let healthy_c_coordinator = coordinator.clone();
        let healthy_c = tokio::spawn(async move {
            healthy_c_coordinator
                .reserve(SettlementAdmissionOwner::for_test(
                    "healthy-user-c",
                    "healthy-session-c",
                ))
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while coordinator.queued_admissions() != NOISY_WAITERS + 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("healthy user C enters the ready queue under global saturation");
        let examined_before = coordinator
            .metrics
            .admission_ready_candidates_examined
            .load(Ordering::Relaxed);
        drop(blockers.pop());
        let healthy_c = tokio::time::timeout(std::time::Duration::from_millis(100), healthy_c)
            .await
            .expect("healthy user C grant latency must not depend on tenant A queue depth")
            .expect("join healthy user C")
            .expect("healthy user C receives the released reservation");
        let examined = coordinator
            .metrics
            .admission_ready_candidates_examined
            .load(Ordering::Relaxed)
            .saturating_sub(examined_before);
        assert_eq!(examined, 1, "one release selects one ready owner");
        drop(healthy_b);
        drop(healthy_c);

        assert!(
            !coordinator
                .close_and_drain(std::time::Duration::from_millis(1))
                .await
        );
        {
            let state = coordinator
                .admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(state.users.is_empty());
            assert!(state.ready_users.is_empty());
            assert_eq!(state.queued, 0);
            assert!(state.queued_by_user.is_empty());
            assert!(state.queued_by_session.is_empty());
        }
        for waiter in noisy_waiters {
            let error = match waiter.await.expect("join noisy waiter") {
                Ok(_) => panic!("shutdown must reject queued admission"),
                Err(error) => error,
            };
            assert_eq!(error.kind, astra_core::ErrorKind::ResourceLimit);
        }
        drop(active_a);
        drop(blockers);
    }

    #[tokio::test]
    async fn hundreds_of_healthy_multi_user_reservations_wait_boundedly_instead_of_failing() {
        const USERS: usize = 16;
        const SESSIONS_PER_USER: usize = 4;
        const CALLS_PER_SESSION: usize = 5;
        const OPERATIONS: usize = USERS * SESSIONS_PER_USER * CALLS_PER_SESSION;
        let coordinator = ProviderSettlementCoordinator::new_with_waiting_capacity(16, 384, 2);
        let mut tasks = Vec::with_capacity(OPERATIONS);
        for user in 0..USERS {
            for session in 0..SESSIONS_PER_USER {
                for _ in 0..CALLS_PER_SESSION {
                    let coordinator = coordinator.clone();
                    tasks.push(tokio::spawn(async move {
                        let reservation = coordinator
                            .reserve(SettlementAdmissionOwner::for_test(
                                &format!("user-{user}"),
                                &format!("session-{session}"),
                            ))
                            .await?;
                        tokio::task::yield_now().await;
                        drop(reservation);
                        Ok::<_, astra_core::ClassifiedError>(())
                    }));
                }
            }
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for task in tasks {
                task.await.expect("join healthy reservation")?;
            }
            Ok::<_, astra_core::ClassifiedError>(())
        })
        .await
        .expect("320 healthy operations finish within the bounded budget")
        .expect("no healthy operation is rejected at the former 256 boundary");
        assert_eq!(coordinator.available_permits(), 16);
        assert_eq!(coordinator.queued_admissions(), 0);
        assert!(
            coordinator
                .metrics
                .queued_admissions
                .load(Ordering::Relaxed)
                > 0,
            "the test must exercise bounded waiting rather than only the fast path"
        );
        assert!(
            coordinator
                .metrics
                .admitted_from_queue
                .load(Ordering::Relaxed)
                > 0,
            "queued healthy work must receive a fair admission grant"
        );
    }

    #[tokio::test]
    async fn cancelling_a_queued_reservation_removes_it_without_capacity_leak() {
        let coordinator = ProviderSettlementCoordinator::new_with_waiting_capacity(1, 4, 1);
        let active = coordinator
            .reserve(SettlementAdmissionOwner::for_test("user-a", "session-a"))
            .await
            .expect("active reservation");
        let waiting_coordinator = coordinator.clone();
        let waiting = tokio::spawn(async move {
            waiting_coordinator
                .reserve(SettlementAdmissionOwner::for_test("user-b", "session-b"))
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while coordinator.queued_admissions() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reservation enters wait queue");
        waiting.abort();
        let _ = waiting.await;
        assert_eq!(coordinator.queued_admissions(), 0);
        assert_eq!(
            coordinator
                .metrics
                .cancelled_waiters
                .load(Ordering::Relaxed),
            1
        );
        {
            let state = coordinator
                .admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(state.users.is_empty());
            assert!(state.ready_users.is_empty());
            assert!(state.queued_by_user.is_empty());
            assert!(state.queued_by_session.is_empty());
        }
        drop(active);
        assert_eq!(coordinator.available_permits(), 1);
    }

    #[tokio::test]
    async fn shutdown_rejects_queued_reservations_and_drains_exact_active_ownership() {
        let coordinator = ProviderSettlementCoordinator::new_with_waiting_capacity(1, 4, 1);
        let active = coordinator
            .reserve(SettlementAdmissionOwner::for_test("user-a", "session-a"))
            .await
            .expect("active reservation");
        let waiting_coordinator = coordinator.clone();
        let waiting = tokio::spawn(async move {
            waiting_coordinator
                .reserve(SettlementAdmissionOwner::for_test("user-b", "session-b"))
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while coordinator.queued_admissions() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reservation enters wait queue");

        assert!(
            !coordinator
                .close_and_drain(std::time::Duration::from_millis(20))
                .await,
            "shutdown must retain the exact active owner"
        );
        let error = match waiting.await.expect("join queued reservation") {
            Ok(_) => panic!("queued work must be rejected after shutdown"),
            Err(error) => error,
        };
        assert_eq!(error.kind, astra_core::ErrorKind::ResourceLimit);
        assert_eq!(coordinator.queued_admissions(), 0);
        {
            let state = coordinator
                .admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(state.users.is_empty());
            assert!(state.ready_users.is_empty());
            assert!(state.queued_by_user.is_empty());
            assert!(state.queued_by_session.is_empty());
        }
        drop(active);
        assert!(
            coordinator
                .close_and_drain(std::time::Duration::from_secs(1))
                .await
        );
    }

    #[tokio::test]
    async fn ledger_timeout_hands_logical_invocation_to_detached_settlement() {
        let persistence = Arc::new(TestInferenceLedgerPersistence::default());
        let plan = test_invocation_plan();
        persistence
            .admit_invocation(&plan)
            .await
            .expect("admit logical invocation");
        let observer = Arc::new(DurableProviderAttemptObserver::new_with_persistence(
            persistence.clone(),
            plan.clone(),
            astra_services::ModelRequestContextSeed::server_default(),
        ));
        let invocation = DurableInferenceInvocation {
            persistence: persistence.clone(),
            plan,
            observer: observer.clone(),
            settlement_coordinator: observer.settlement_coordinator.clone(),
            owner_lease: observer.owner_lease.clone(),
        };
        let error = ledger_timeout_error("provider_attempt_admission");

        invocation
            .finish_error(&error)
            .await
            .expect("ledger owner must retain settlement authority");

        assert!(is_ledger_error(&error));
        wait_for_quiescent(&persistence).await;
        assert_eq!(
            persistence.logical_terminal_statuses(),
            vec![astra_services::InferenceTerminalStatus::Cancelled],
            "without an admitted physical request the detached owner must close the logical invocation as pre-delivery cancellation"
        );
    }

    #[tokio::test]
    async fn ledger_admission_timeout_closes_late_attempt_and_logical_invocation() {
        let persistence = Arc::new(DelayedTrackedAdmissionPersistence::default());
        let plan = test_invocation_plan();
        persistence
            .admit_invocation(&plan)
            .await
            .expect("admit logical invocation");
        let observer = Arc::new(DurableProviderAttemptObserver::new_with_persistence(
            persistence.clone(),
            plan.clone(),
            astra_services::ModelRequestContextSeed::server_default(),
        ));
        let invocation = DurableInferenceInvocation {
            persistence: persistence.clone(),
            plan,
            observer: observer.clone(),
            settlement_coordinator: observer.settlement_coordinator.clone(),
            owner_lease: observer.owner_lease.clone(),
        };
        let wire = ProviderWireRequestIdentity {
            protocol: crate::turn::llm::client::LlmProviderProtocol::OpenAiCompatible,
            provider_wire_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .into(),
            provider_wire_bytes: 2,
            composition: crate::turn::llm::client::ProviderWireComposition {
                provider_envelope_bytes: 2,
                ..Default::default()
            },
            fingerprints: Default::default(),
        };
        let mut admission = Box::pin(observer.begin_attempt(&wire));
        tokio::select! {
            result = &mut admission => panic!("admission unexpectedly completed: {result:?}"),
            _ = async {
                while persistence.begin_entered.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            } => {}
        }
        drop(admission);

        invocation
            .finish_error(&ledger_timeout_error("provider_attempt_admission"))
            .await
            .expect("foreground ledger timeout must hand settlement off");
        persistence.release_begin.notify_one();

        wait_for_quiescent(&persistence.inner).await;
        assert_eq!(
            persistence.inner.logical_terminal_statuses(),
            vec![astra_services::InferenceTerminalStatus::Cancelled]
        );
        let state = persistence.inner.lock();
        assert_eq!(state.attempts.len(), 1);
        assert!(state.attempts.values().all(|attempt| {
            attempt.terminal.as_ref().map(|terminal| terminal.status)
                == Some(astra_services::InferenceTerminalStatus::Cancelled)
        }));
    }

    #[tokio::test]
    async fn settlement_closing_during_ambiguous_admission_never_authorizes_delivery() {
        let persistence = Arc::new(DelayedTrackedAdmissionPersistence::default());
        let plan = test_invocation_plan();
        persistence
            .admit_invocation(&plan)
            .await
            .expect("admit logical invocation");
        let observer = Arc::new(DurableProviderAttemptObserver::new_with_persistence(
            persistence.clone(),
            plan.clone(),
            astra_services::ModelRequestContextSeed::server_default(),
        ));
        let invocation = DurableInferenceInvocation {
            persistence: persistence.clone(),
            plan,
            observer: observer.clone(),
            settlement_coordinator: observer.settlement_coordinator.clone(),
            owner_lease: observer.owner_lease.clone(),
        };
        let wire = ProviderWireRequestIdentity {
            protocol: crate::turn::llm::client::LlmProviderProtocol::OpenAiCompatible,
            provider_wire_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .into(),
            provider_wire_bytes: 2,
            composition: crate::turn::llm::client::ProviderWireComposition {
                provider_envelope_bytes: 2,
                ..Default::default()
            },
            fingerprints: Default::default(),
        };

        let admitting_observer = observer.clone();
        let admission = tokio::spawn(async move { admitting_observer.begin_attempt(&wire).await });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while persistence.begin_entered.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("provider-attempt row should commit before its acknowledgement is released");

        invocation
            .finish_error(&ledger_timeout_error("provider_attempt_admission"))
            .await
            .expect("foreground timeout must transfer settlement ownership");
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !observer.operations.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("settlement must close admission before recovery");

        persistence.release_begin.notify_one();
        let admission_error = admission
            .await
            .expect("admission task should not panic")
            .expect_err("a late acknowledgement cannot authorize provider delivery");
        assert_eq!(
            admission_error.kind,
            astra_core::ErrorKind::ContractViolation
        );

        wait_for_quiescent(&persistence.inner).await;
        let state = persistence.inner.lock();
        assert_eq!(state.attempts.len(), 1);
        assert!(state.attempts.values().all(|attempt| {
            attempt.terminal.as_ref().map(|terminal| terminal.status)
                == Some(astra_services::InferenceTerminalStatus::Cancelled)
        }));
        assert_eq!(
            state
                .invocations
                .values()
                .filter_map(|invocation| invocation.terminal.as_ref())
                .map(|terminal| terminal.status)
                .collect::<Vec<_>>(),
            vec![astra_services::InferenceTerminalStatus::Cancelled]
        );
        assert!(
            observer
                .state
                .try_lock()
                .expect("observer state should be quiescent")
                .delivery_authorized
                .is_empty(),
            "an admission acknowledged after settlement closed must never permit HTTP delivery"
        );
    }

    #[tokio::test]
    async fn ledger_terminal_timeout_preserves_late_exact_terminal_for_logical_invocation() {
        let persistence = Arc::new(DelayedTrackedTerminalPersistence::default());
        let plan = test_invocation_plan();
        persistence
            .admit_invocation(&plan)
            .await
            .expect("admit logical invocation");
        let observer = Arc::new(DurableProviderAttemptObserver::new_with_persistence(
            persistence.clone(),
            plan.clone(),
            astra_services::ModelRequestContextSeed::server_default(),
        ));
        let invocation = DurableInferenceInvocation {
            persistence: persistence.clone(),
            plan,
            observer: observer.clone(),
            settlement_coordinator: observer.settlement_coordinator.clone(),
            owner_lease: observer.owner_lease.clone(),
        };
        let wire = ProviderWireRequestIdentity {
            protocol: crate::turn::llm::client::LlmProviderProtocol::OpenAiCompatible,
            provider_wire_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .into(),
            provider_wire_bytes: 2,
            composition: crate::turn::llm::client::ProviderWireComposition {
                provider_envelope_bytes: 2,
                ..Default::default()
            },
            fingerprints: Default::default(),
        };
        let attempt = observer
            .begin_attempt(&wire)
            .await
            .expect("admit physical provider attempt");
        let provider_error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ServerError,
            "provider returned a terminal failure",
        );
        let terminal = terminal_from_error(&provider_error);
        let mut terminalization = Box::pin(observer.finish_attempt(attempt, &terminal));
        tokio::select! {
            result = &mut terminalization => panic!("terminalization unexpectedly completed: {result:?}"),
            _ = async {
                while persistence.finish_entered.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            } => {}
        }
        drop(terminalization);

        invocation
            .finish_error(&ledger_timeout_error("provider_attempt_terminalization"))
            .await
            .expect("foreground ledger timeout must hand settlement off");
        persistence.release_finish.notify_one();

        wait_for_quiescent(&persistence.inner).await;
        let state = persistence.inner.lock();
        let logical = state
            .invocations
            .values()
            .next()
            .and_then(|invocation| invocation.terminal.as_ref())
            .expect("logical invocation terminal");
        assert_eq!(logical, &terminal);
    }

    #[tokio::test]
    async fn cancelled_uncommitted_admission_does_not_leak_a_worker() {
        let persistence = Arc::new(DelayedAdmissionPersistence::default());
        let observer = DurableProviderAttemptObserver::new_with_persistence(
            persistence.clone(),
            test_invocation_plan(),
            astra_services::ModelRequestContextSeed::server_default(),
        );
        let wire = ProviderWireRequestIdentity {
            protocol: crate::turn::llm::client::LlmProviderProtocol::OpenAiCompatible,
            provider_wire_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .into(),
            provider_wire_bytes: 2,
            composition: crate::turn::llm::client::ProviderWireComposition {
                provider_envelope_bytes: 2,
                ..Default::default()
            },
            fingerprints: Default::default(),
        };
        let mut admission = Box::pin(observer.begin_attempt(&wire));
        tokio::select! {
            result = &mut admission => panic!("admission unexpectedly completed: {result:?}"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
        }
        assert_eq!(persistence.begin_entered.load(Ordering::SeqCst), 1);
        drop(admission);
        persistence.release_begin.notify_one();
        tokio::task::yield_now().await;

        assert_eq!(
            persistence.finished.load(Ordering::SeqCst),
            0,
            "a cancelled pre-commit future must not leave a detached closer"
        );
        let state = observer.state.lock().await;
        assert_eq!(state.open_attempts.len(), 1);
        assert!(state.terminals.is_empty());
        drop(state);
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            observer.operations.close_and_wait(),
        )
        .await
        .expect("dropping admission must release its operation permit");
    }

    fn test_ledger_for_persistence(
        persistence: TestInferenceLedgerPersistence,
    ) -> DurableInferenceLedger {
        let execution = astra_services::AdmittedModelExecution::from_endpoint(
            "offer-test".to_string(),
            "model-test".to_string(),
            "openai".to_string(),
            "http://provider.test/v1/chat/completions".to_string(),
            "Bearer test".to_string(),
            None,
            128_000,
        );
        DurableInferenceLedger::required_with_persistence(
            None,
            Some(&execution),
            "user-test",
            Some(Arc::new(persistence)),
        )
        .expect("test persistence satisfies durable admission")
    }

    async fn test_invocation(
        persistence: TestInferenceLedgerPersistence,
    ) -> DurableInferenceInvocation {
        test_ledger_for_persistence(persistence)
            .with_run_authority(DurableInferenceRunAuthority::new(
                0,
                "test-inference-owner",
                0,
                None,
                None,
                None,
            ))
            .admit(
                astra_turn_types::InferenceInvocationScope::Run {
                    session_id: "session-test".to_string(),
                    run_id: "run-test".to_string(),
                    turn: 1,
                    round: 0,
                    operation_id: "agent_turn".to_string(),
                    logical_attempt: 0,
                },
                astra_turn_types::InferencePurpose::PrimaryAgent,
                "model-test",
                "model-test",
                "openai",
            )
            .await
            .expect("test invocation plan")
    }

    fn test_wire_identity() -> ProviderWireRequestIdentity {
        ProviderWireRequestIdentity {
            protocol: crate::turn::llm::client::LlmProviderProtocol::OpenAiCompatible,
            provider_wire_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_string(),
            provider_wire_bytes: 128,
            composition: crate::turn::llm::client::ProviderWireComposition {
                provider_envelope_bytes: 128,
                ..Default::default()
            },
            fingerprints: Default::default(),
        }
    }

    #[tokio::test]
    async fn canonical_transition_commits_with_attempt_before_dispatch() {
        let persistence = TestInferenceLedgerPersistence::default();
        let invocation = test_invocation(persistence.clone()).await;
        let base = vec![serde_json::json!({"role": "user", "content": "goal"})];
        let content = astra_turn_types::render_append_only_runtime_authority_frame(
            "test_authority",
            astra_turn_types::RuntimeAuthorityLifetime::NextAssistantDecision,
            "opaque test authority",
        )
        .unwrap();
        let mut authority = serde_json::json!({"role": "user", "content": content});
        astra_turn_types::mark_append_only_required_context(
            &mut authority,
            "test_authority",
            astra_turn_types::RuntimeAuthorityLifetime::NextAssistantDecision,
        );
        let transition =
            astra_turn_types::ProviderCanonicalTransitionV2::new(None, &base, vec![authority])
                .unwrap();
        let transition_id = transition.transition_id.clone();
        invocation
            .bind_provider_canonical_transitions(vec![transition])
            .unwrap();

        let attempt_index = invocation
            .attempt_observer()
            .begin_attempt(&test_wire_identity())
            .await
            .expect("attempt admission commits its canonical WAL");
        assert_eq!(
            invocation.admitted_canonical_transition_id().as_deref(),
            Some(transition_id.as_str())
        );
        assert!(!invocation.provider_dispatch_started());
        assert!(
            persistence
                .lock()
                .attempts
                .values()
                .all(|attempt| attempt.canonical_transition_hash.is_some()),
            "the durable attempt must own the transition before transport starts"
        );
        assert!(
            invocation
                .bind_provider_canonical_transitions(Vec::new())
                .is_err(),
            "transition identity freezes when attempt admission starts"
        );

        let terminal = pre_provider_cancelled_terminal();
        invocation
            .attempt_observer()
            .finish_attempt(attempt_index, &terminal)
            .await
            .unwrap();
        invocation.finish(&terminal).await.unwrap();
        persistence.assert_quiescent();
    }

    #[tokio::test]
    async fn invocation_is_durable_before_its_first_provider_attempt() {
        let persistence = TestInferenceLedgerPersistence::default();
        let invocation = test_invocation(persistence.clone()).await;
        {
            let state = persistence.lock();
            assert_eq!(state.invocations.len(), 1);
            assert!(state.attempts.is_empty());
        }

        let attempt_index = invocation
            .attempt_observer()
            .begin_attempt(&test_wire_identity())
            .await
            .expect("combined invocation and provider-attempt admission");
        {
            let state = persistence.lock();
            assert_eq!(state.invocations.len(), 1);
            assert_eq!(state.attempts.len(), 1);
        }

        let terminal = astra_services::InferenceInvocationTerminal::succeeded(
            astra_services::InferenceUsage::default(),
            Some("provider-response".to_string()),
        );
        invocation
            .attempt_observer()
            .finish_attempt(attempt_index, &terminal)
            .await
            .expect("provider attempt terminal");
        invocation
            .finish(&terminal)
            .await
            .expect("logical terminal");
        persistence.assert_quiescent();
    }

    #[tokio::test]
    async fn pre_delivery_terminal_admits_invocation_before_finishing_it() {
        let persistence = TestInferenceLedgerPersistence::default();
        let invocation = test_invocation(persistence.clone()).await;
        let terminal = terminal_from_error(&astra_core::ClassifiedError::new(
            astra_core::ErrorKind::Cancelled,
            "cancelled before provider delivery",
        ));

        invocation
            .finish(&terminal)
            .await
            .expect("logical terminal");

        let state = persistence.lock();
        assert_eq!(state.invocations.len(), 1);
        assert!(state.attempts.is_empty());
        assert_eq!(
            state
                .invocations
                .values()
                .next()
                .and_then(|invocation| invocation.terminal.as_ref()),
            Some(&terminal)
        );
    }

    #[test]
    fn execution_placement_is_normalized_independently_from_surface_topology() {
        let cli_edge = normalize_request_context_for_execution(
            astra_services::ModelRequestContextSeed {
                topology: astra_services::ModelRequestTopology::CliServer,
                interaction_owner: "cli".to_string(),
                loop_owner: "server".to_string(),
                ..astra_services::ModelRequestContextSeed::server_default()
            },
            astra_services::ModelExecutionPlacement::Edge,
        );
        assert_eq!(
            cli_edge.topology,
            astra_services::ModelRequestTopology::CliServer
        );
        assert_eq!(cli_edge.interaction_owner, "cli");
        assert_eq!(cli_edge.loop_owner, "server");
        assert_eq!(cli_edge.execution_binding, "edge");

        let server_edge = normalize_request_context_for_execution(
            astra_services::ModelRequestContextSeed::server_default(),
            astra_services::ModelExecutionPlacement::Edge,
        );
        assert_eq!(
            server_edge.topology,
            astra_services::ModelRequestTopology::EdgeServer
        );
        assert_eq!(server_edge.interaction_owner, "server");
        assert_eq!(server_edge.loop_owner, "server");
        assert_eq!(server_edge.execution_binding, "edge");

        let edge_server = normalize_request_context_for_execution(
            server_edge,
            astra_services::ModelExecutionPlacement::Server,
        );
        assert_eq!(
            edge_server.topology,
            astra_services::ModelRequestTopology::EdgeServer
        );
        assert_eq!(edge_server.execution_binding, "server");
    }

    #[test]
    fn execution_placement_cannot_rewrite_control_or_causal_facts() {
        let mut seed = astra_services::ModelRequestContextSeed::server_default();
        seed.actor_id = Some("actor-1".to_string());
        seed.execution_principal = Some("principal-1".to_string());
        seed.interaction_owner = "web-controller".to_string();
        seed.loop_owner = "server".to_string();
        seed.lineage.branch_id = Some("branch-1".to_string());
        seed.lineage.writer_epoch = Some(7);
        seed.lineage.conversation_root_hash = Some("a".repeat(64));
        seed.budget.usable_input_limit_tokens = Some(800_000);
        seed.composition.history_user_tokens = Some(42);
        seed.cache.current_identity = Some("cache-1".to_string());

        let edge = normalize_request_context_for_execution(
            seed.clone(),
            astra_services::ModelExecutionPlacement::Edge,
        );

        assert_eq!(
            edge.topology,
            astra_services::ModelRequestTopology::EdgeServer
        );
        assert_eq!(edge.execution_binding, "edge");
        assert_eq!(edge.interaction_owner, seed.interaction_owner);
        assert_eq!(edge.loop_owner, seed.loop_owner);
        assert_eq!(edge.actor_id, seed.actor_id);
        assert_eq!(edge.execution_principal, seed.execution_principal);
        assert_eq!(edge.lineage, seed.lineage);
        assert_eq!(edge.budget, seed.budget);
        assert_eq!(edge.composition, seed.composition);
        assert_eq!(edge.cache, seed.cache);

        let server = normalize_request_context_for_execution(
            edge,
            astra_services::ModelExecutionPlacement::Server,
        );
        assert_eq!(
            server.topology,
            astra_services::ModelRequestTopology::EdgeServer
        );
        assert_eq!(server.execution_binding, "server");
        assert_eq!(server.interaction_owner, "web-controller");
        assert_eq!(server.lineage, seed.lineage);
    }

    #[test]
    fn required_ledger_fails_closed_without_a_database() {
        let error = match DurableInferenceLedger::required(None, None, "user-7") {
            Ok(_) => panic!("real provider execution must not bypass durable attempt admission"),
            Err(error) => error,
        };

        assert_eq!(error.kind, astra_core::ErrorKind::ContractViolation);
        assert!(
            error.message.contains("no durable inference database"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn closing_operation_gate_waits_for_registered_detached_work() {
        let gate = ProviderOperationGate::default();
        let permit = gate
            .register("test operation")
            .expect("first operation is admitted");
        let release = Arc::new(tokio::sync::Notify::new());
        let released = release.clone();
        let detached = tokio::spawn(async move {
            released.notified().await;
            drop(permit);
        });

        let closing_gate = gate.clone();
        let mut closing = tokio::spawn(async move {
            closing_gate.close_and_wait().await;
        });
        tokio::task::yield_now().await;

        assert!(
            !closing.is_finished(),
            "cleanup must wait for the already registered durable operation"
        );
        assert!(
            gate.register("late operation").is_err(),
            "closing the gate must synchronously reject new provider operations"
        );

        release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(1), &mut closing)
            .await
            .expect("cleanup should finish once detached work drains")
            .expect("cleanup task should not panic");
        detached.await.expect("detached work should not panic");
    }

    #[tokio::test]
    async fn attempt_batch_continues_after_an_individual_terminal_failure() {
        let visited = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let observed = visited.clone();
        let (completed, error) = finish_attempt_batch(vec![0, 1, 2], move |attempt_index| {
            let observed = observed.clone();
            async move {
                observed.lock().await.push(attempt_index);
                if attempt_index == 0 {
                    Err(astra_core::ClassifiedError::new(
                        astra_core::ErrorKind::DatabaseError,
                        "first terminal write failed",
                    ))
                } else {
                    Ok(())
                }
            }
        })
        .await;

        assert_eq!(*visited.lock().await, vec![0, 1, 2]);
        assert_eq!(completed, vec![1, 2]);
        assert_eq!(
            error.expect("the first error remains observable").message,
            "first terminal write failed"
        );
    }

    #[derive(Default)]
    struct RecordingSettlementOwner {
        dropped: tokio::sync::Notify,
        drop_count: AtomicU32,
    }

    #[async_trait]
    impl NonstreamInvocationSettlement for RecordingSettlementOwner {
        async fn settle_terminal(
            &self,
            _terminal: &astra_services::InferenceInvocationTerminal,
        ) -> Result<(), astra_core::ClassifiedError> {
            Ok(())
        }

        async fn settle_caller_drop(&self) -> Result<(), astra_core::ClassifiedError> {
            self.drop_count.fetch_add(1, Ordering::AcqRel);
            self.dropped.notify_one();
            Ok(())
        }

        async fn settle_delivery_unknown(
            &self,
            _terminal: &astra_services::InferenceInvocationTerminal,
        ) -> Result<(), astra_core::ClassifiedError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn nonstream_settlement_outlives_a_dropped_caller() {
        let owner = Arc::new(RecordingSettlementOwner::default());
        let supervisor = NonstreamInvocationSupervisor::start(owner.clone());

        drop(supervisor);

        tokio::time::timeout(std::time::Duration::from_secs(1), owner.dropped.notified())
            .await
            .expect("dropped caller must wake the independent settlement owner");
        assert_eq!(owner.drop_count.load(Ordering::Acquire), 1);
    }

    struct FailingTerminalSettlementOwner;

    #[async_trait]
    impl NonstreamInvocationSettlement for FailingTerminalSettlementOwner {
        async fn settle_terminal(
            &self,
            _terminal: &astra_services::InferenceInvocationTerminal,
        ) -> Result<(), astra_core::ClassifiedError> {
            Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::DatabaseError,
                "terminal commit rejected",
            ))
        }

        async fn settle_caller_drop(&self) -> Result<(), astra_core::ClassifiedError> {
            Ok(())
        }

        async fn settle_delivery_unknown(
            &self,
            _terminal: &astra_services::InferenceInvocationTerminal,
        ) -> Result<(), astra_core::ClassifiedError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn nonstream_settlement_failure_is_returned_to_the_caller() {
        let supervisor =
            NonstreamInvocationSupervisor::start(Arc::new(FailingTerminalSettlementOwner));
        let terminal = astra_services::InferenceInvocationTerminal::succeeded(
            astra_services::InferenceUsage::default(),
            None,
        );

        let error = supervisor
            .settle(NonstreamSettlementCommand::Terminal(terminal))
            .await
            .expect_err("a logical terminal commit failure must fail the call");

        assert_eq!(error.kind, astra_core::ErrorKind::DatabaseError);
        assert_eq!(error.message, "terminal commit rejected");
    }

    #[tokio::test]
    async fn logical_mirror_failure_cannot_drop_confirmed_settlement_debt() {
        let persistence = Arc::new(FailFirstLogicalTerminalPersistence::default());
        let plan = test_invocation_plan();
        persistence
            .admit_invocation(&plan)
            .await
            .expect("admit logical invocation");
        let observer = Arc::new(DurableProviderAttemptObserver::new_with_persistence(
            persistence.clone(),
            plan.clone(),
            astra_services::ModelRequestContextSeed::server_default(),
        ));
        let invocation = DurableInferenceInvocation {
            persistence: persistence.clone(),
            observer: observer.clone(),
            plan,
            settlement_coordinator: observer.settlement_coordinator.clone(),
            owner_lease: observer.owner_lease.clone(),
        };
        let supervisor = NonstreamInvocationSupervisor::start(Arc::new(invocation));
        let terminal = terminal_from_error(&astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ServerError,
            "provider rejected the request",
        ));

        let error = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            supervisor.settle(NonstreamSettlementCommand::Terminal(terminal.clone())),
        )
        .await
        .expect("foreground logical mirror remains bounded")
        .expect_err("injected logical mirror failure remains visible");

        assert_eq!(error.kind, astra_core::ErrorKind::DatabaseError);
        assert!(
            persistence.inner.has_explicit_settlement_debt(),
            "the exact logical settlement must be durable before its mirror begins"
        );
        persistence.inner.reconcile_settlement_debts();
        persistence.inner.assert_quiescent();
        assert_eq!(
            persistence.inner.logical_terminal_statuses(),
            vec![terminal.status]
        );
    }

    #[derive(Default)]
    struct FailingDetachedSettlementOwner {
        attempted: tokio::sync::Notify,
        attempts: AtomicU32,
    }

    #[async_trait]
    impl NonstreamInvocationSettlement for FailingDetachedSettlementOwner {
        async fn settle_terminal(
            &self,
            _terminal: &astra_services::InferenceInvocationTerminal,
        ) -> Result<(), astra_core::ClassifiedError> {
            Ok(())
        }

        async fn settle_caller_drop(&self) -> Result<(), astra_core::ClassifiedError> {
            Ok(())
        }

        async fn settle_delivery_unknown(
            &self,
            _terminal: &astra_services::InferenceInvocationTerminal,
        ) -> Result<(), astra_core::ClassifiedError> {
            self.attempts.fetch_add(1, Ordering::AcqRel);
            self.attempted.notify_one();
            Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::DatabaseError,
                "detached terminal commit rejected",
            ))
        }
    }

    #[tokio::test]
    async fn detached_delivery_unknown_settlement_attempts_persistence_without_blocking_caller() {
        let owner = Arc::new(FailingDetachedSettlementOwner::default());
        let supervisor = NonstreamInvocationSupervisor::start(owner.clone());
        let terminal = delivery_unknown_terminal_from_error(&astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "provider work deadline elapsed",
        ));

        supervisor
            .handoff(NonstreamSettlementCommand::DeliveryUnknown(terminal))
            .expect("caller only waits for settlement ownership handoff");

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            owner.attempted.notified(),
        )
        .await
        .expect("detached settlement must attempt durable convergence");
        assert_eq!(owner.attempts.load(Ordering::Acquire), 1);
    }

    #[test]
    fn terminal_status_distinguishes_pre_delivery_failure_from_uncertain_delivery() {
        let connect_failure = terminal_from_error(&astra_core::ClassifiedError::new(
            astra_core::ErrorKind::Network,
            "connection refused",
        ));
        let stream_failure = terminal_from_error(&astra_core::ClassifiedError::new(
            astra_core::ErrorKind::StreamTransport,
            "connection reset after delivery",
        ));

        assert_eq!(
            connect_failure.status,
            astra_services::InferenceTerminalStatus::Failed
        );
        assert_eq!(
            stream_failure.status,
            astra_services::InferenceTerminalStatus::DeliveryUnknown
        );
        assert_eq!(
            unsettled_attempt_terminal().status,
            astra_services::InferenceTerminalStatus::DeliveryUnknown,
            "a lost durable provider terminal must never be reported as safely retryable"
        );
    }

    #[test]
    fn quiescent_transport_terminal_preserves_partial_provider_facts() {
        let terminal = astra_services::InferenceInvocationTerminal {
            status: astra_services::InferenceTerminalStatus::DeliveryUnknown,
            usage: astra_services::InferenceUsage {
                input: astra_turn_types::NormalizedPromptCacheUsage::new(200, 800, 100),
                output_tokens: 50,
            },
            usage_status: astra_services::InferenceUsageStatus::ProviderPartial,
            provider_response_id: Some("provider-response-7".to_string()),
            error_kind: Some("stream_transport".to_string()),
            error_message: Some("partial delivery".to_string()),
        };
        let mut state = ProviderAttemptState::default();
        state.terminals.insert(0, terminal.clone());

        assert_eq!(state.quiescent_terminal(), Some(terminal));
    }

    #[test]
    fn physical_attempt_inventory_keeps_earlier_retries_in_order() {
        let mut state = ProviderAttemptState::default();
        for attempt in [0_u32, 1] {
            state.requests.insert(
                attempt,
                DurableProviderRequestIdentity {
                    request_id: format!("request-{attempt}"),
                    request_hash:
                        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                            .to_string(),
                    attempt,
                    protocol: crate::turn::llm::client::LlmProviderProtocol::OpenAiCompatible,
                    provider_wire_bytes: 128,
                    composition: crate::turn::llm::client::ProviderWireComposition {
                        provider_envelope_bytes: 128,
                        ..Default::default()
                    },
                    fingerprints: Default::default(),
                },
            );
        }
        state.terminals.insert(
            0,
            astra_services::InferenceInvocationTerminal {
                status: astra_services::InferenceTerminalStatus::Failed,
                usage: astra_services::InferenceUsage::default(),
                usage_status: astra_services::InferenceUsageStatus::Unavailable,
                provider_response_id: Some("provider-429".to_string()),
                error_kind: Some("rate_limit".to_string()),
                error_message: None,
            },
        );

        let facts = state.attempt_facts(&BTreeSet::from([1]));
        assert_eq!(
            facts
                .iter()
                .map(|fact| fact.request.request_id.as_str())
                .collect::<Vec<_>>(),
            vec!["request-0", "request-1"]
        );
        assert_eq!(
            facts[0]
                .terminal
                .as_ref()
                .and_then(|terminal| terminal.provider_response_id.as_deref()),
            Some("provider-429")
        );
        assert!(facts[1].terminal.is_none());
        assert!(!facts[0].dispatch_started);
        assert!(facts[1].dispatch_started);
    }
}
