//! Cloud preference sync via server REST.
//!
//! Edge-cloud contract: the CLI MUST NOT connect to MatrixOne
//! directly. This module previously built a `sqlx::Pool` and
//! invoked `MatrixOneSyncService` in-process; both moved to the
//! server (`/preferences` endpoints) so the only way the CLI
//! reaches the preference store is through HTTP.
//!
//! ## Sync Flow
//!
//! - **Preferences**: [`try_cloud_pull_preferences`].
//!   Pulls go through [`crate::preferences_client`] now.

use astra_core::sync_poison::recover_mutex_lock;
use astra_services::session_journal;
use astra_services::state_sync::pref_keys;
use astra_services::{
    SyncOutboxDeliverySettlement, SyncOutboxJournalDelta, SyncOutboxJournalDeltaOutcome,
    SyncOutboxRecord, SyncOutboxSettlementReport, SyncOutboxStatus, SyncOutboxStore,
};
use astra_sync_protocol::{
    MAX_SYNC_OUTBOX_BATCH_EVENTS, SYNC_OUTBOX_BATCH_SCHEMA_VERSION,
    SYNC_OUTBOX_SESSION_UNAVAILABLE_ERROR_CODE, SyncOutboxBatchResponse, SyncOutboxEventBatch,
    SyncOutboxReconcileRequest,
};
use astra_turn_core::tool_health_persistence::ToolHealthEntry;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use futures_util::{FutureExt, StreamExt};
use tokio::sync::mpsc;

use crate::cli::session::session_runtime;
use crate::cli::session::session_side_effects::{
    enqueue_ingestion_for_immediate_drain_pub, enqueue_ingestion_pub,
};
use crate::{ExplainMode, SessionState};

const SYNC_OUTBOX_DRAIN_LIMIT: usize = 64;
/// Bound owned record clones retained for one drain, independently of request batches.
const SYNC_OUTBOX_DRAIN_SERIALIZED_BYTE_BUDGET: usize = 4 * 1024 * 1024;
const SYNC_OUTBOX_DRAIN_BACKGROUND_ROUNDS: usize = 4;
const SYNC_OUTBOX_DEGRADED_DRAIN_COOLDOWN: Duration = Duration::from_secs(30);
const SYNC_OUTBOX_RECORD_DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);
/// Stops starting new work well before the 5 minute outbox in-flight lease so
/// the settlement write still fits inside the lease.
const SYNC_OUTBOX_DELIVERY_DEADLINE: Duration = Duration::from_secs(180);
const SYNC_OUTBOX_DEADLINE_ERROR: &str =
    "sync outbox drain deadline reached before delivery; record stays queued";
const SYNC_OUTBOX_MAX_CONCURRENT_SESSIONS: usize = 8;
/// Keep the encoded JSON body within the server's global `DefaultBodyLimit`.
const SYNC_OUTBOX_BATCH_BODY_LIMIT_BYTES: usize = 4 * 1024 * 1024;
const SYNC_OUTBOX_JOURNAL_INGEST_QUEUE_CAPACITY: usize = 128;
const SYNC_OUTBOX_JOURNAL_INGEST_OVERFLOW_CAPACITY: usize = 1_024;
const SYNC_OUTBOX_JOURNAL_INGEST_BATCH_WINDOW: Duration = Duration::from_millis(25);
/// Bound recovery memory while amortizing the monolithic outbox transaction.
/// A batch pays one parse/rewrite/fsync, never one per source session.
const SYNC_OUTBOX_JOURNAL_RECOVERY_BATCH_SOURCES: usize = 64;
const SYNC_OUTBOX_JOURNAL_INGEST_RETRY_DELAY: Duration = Duration::from_millis(250);
const SYNC_OUTBOX_JOURNAL_INGEST_MAX_RETRY_DELAY: Duration = Duration::from_secs(4);
const SYNC_OUTBOX_JOURNAL_INGEST_MAX_CONSECUTIVE_FAILURES: u32 = 5;
static SYNC_OUTBOX_DRAIN_OWNER: AtomicU64 = AtomicU64::new(0);
static SYNC_OUTBOX_NEXT_DRAIN_OWNER: AtomicU64 = AtomicU64::new(1);
static SYNC_OUTBOX_RETRY_WAKE_DEADLINE_MS: AtomicU64 = AtomicU64::new(0);

/// Process-local hint queue for the durable journal→outbox projector. A hint
/// can be dropped under pressure because the projector also scans canonical
/// journals on startup and maintains durable source offsets; the fact itself
/// never lives only in this queue.
static SYNC_OUTBOX_JOURNAL_INGEST_DISPATCHER: LazyLock<
    Mutex<Option<mpsc::Sender<JournalIngestHint>>>,
> = LazyLock::new(|| Mutex::new(None));
static SYNC_OUTBOX_JOURNAL_INGEST_OVERFLOW: LazyLock<Mutex<JournalIngestOverflow>> =
    LazyLock::new(|| Mutex::new(JournalIngestOverflow::default()));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JournalIngestScheduleOutcome {
    Scheduled,
    Coalesced,
    RecoveryScanQueued,
    RuntimeUnavailable,
    InvalidSession,
}

impl JournalIngestScheduleOutcome {
    pub(crate) fn accepted(self) -> bool {
        matches!(
            self,
            Self::Scheduled | Self::Coalesced | Self::RecoveryScanQueued
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct JournalIngestHint {
    owner_id: String,
    session_id: String,
}

impl JournalIngestHint {
    fn new(owner_scope: &astra_services::OwnerScope, session_id: impl Into<String>) -> Self {
        Self {
            owner_id: owner_scope.id().to_string(),
            session_id: session_id.into(),
        }
    }

    fn owner_scope(&self) -> astra_services::OwnerScope {
        astra_services::OwnerScope::user(self.owner_id.clone())
            .expect("journal ingest hints only capture validated owner scopes")
    }
}

#[derive(Default)]
struct JournalIngestOverflow {
    sessions: BTreeSet<JournalIngestHint>,
    reconcile_all: bool,
    reconcile_owner_ids: BTreeSet<String>,
}

fn defer_journal_ingest_hint(session_id: String) -> JournalIngestScheduleOutcome {
    let owner_scope = astra_services::local_owner_scope();
    defer_journal_ingest_hint_for_owner(&owner_scope, session_id)
}

fn defer_journal_ingest_hint_for_owner(
    owner_scope: &astra_services::OwnerScope,
    session_id: String,
) -> JournalIngestScheduleOutcome {
    let hint = JournalIngestHint::new(owner_scope, session_id);
    let mut overflow = recover_mutex_lock(&SYNC_OUTBOX_JOURNAL_INGEST_OVERFLOW);
    if overflow.sessions.contains(&hint) {
        return JournalIngestScheduleOutcome::Coalesced;
    }
    if overflow.sessions.len() < SYNC_OUTBOX_JOURNAL_INGEST_OVERFLOW_CAPACITY {
        overflow.sessions.insert(hint);
        return JournalIngestScheduleOutcome::Coalesced;
    }

    // The canonical journals and their source offsets are durable. Once the
    // bounded hint set saturates, one full reconciliation marker represents
    // every further session without retaining attacker-controlled IDs.
    overflow.reconcile_all = true;
    overflow
        .reconcile_owner_ids
        .insert(owner_scope.id().to_string());
    JournalIngestScheduleOutcome::RecoveryScanQueued
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JournalIngestFailureDisposition {
    RetryAfter(Duration),
    QuarantineUntilNewHint,
}

#[derive(Default)]
struct JournalIngestRetryBudget {
    consecutive_failures: BTreeMap<String, u32>,
}

impl JournalIngestRetryBudget {
    fn observe_success(&mut self, session_id: &str) {
        self.consecutive_failures.remove(session_id);
    }

    fn observe_failure(&mut self, session_id: &str) -> JournalIngestFailureDisposition {
        let failures = self
            .consecutive_failures
            .entry(session_id.to_string())
            .and_modify(|failures| *failures = failures.saturating_add(1))
            .or_insert(1);
        if *failures >= SYNC_OUTBOX_JOURNAL_INGEST_MAX_CONSECUTIVE_FAILURES {
            self.consecutive_failures.remove(session_id);
            return JournalIngestFailureDisposition::QuarantineUntilNewHint;
        }
        let multiplier = 2u32.pow(failures.saturating_sub(1));
        JournalIngestFailureDisposition::RetryAfter(std::cmp::min(
            SYNC_OUTBOX_JOURNAL_INGEST_RETRY_DELAY * multiplier,
            SYNC_OUTBOX_JOURNAL_INGEST_MAX_RETRY_DELAY,
        ))
    }
}

struct SyncOutboxDrainScheduleGuard {
    owner: u64,
}

impl Drop for SyncOutboxDrainScheduleGuard {
    fn drop(&mut self) {
        let _ = SYNC_OUTBOX_DRAIN_OWNER.compare_exchange(
            self.owner,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

struct SyncOutboxRetryWakeGuard {
    deadline: u64,
}

impl Drop for SyncOutboxRetryWakeGuard {
    fn drop(&mut self) {
        let _ = SYNC_OUTBOX_RETRY_WAKE_DEADLINE_MS.compare_exchange(
            self.deadline,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

/// Deliberately detach bounded, durable outbox work from the initiating turn.
///
/// Scheduling is backpressured by the drain/wake ownership guards. The outbox
/// itself is durable, so runtime shutdown may cancel this task without losing
/// the queued records; a later process reclaims stale in-flight records.
fn spawn_detached_tracked(
    handle: &tokio::runtime::Handle,
    fut: impl Future<Output = ()> + Send + 'static,
) {
    let task = handle.spawn(async move {
        let result = std::panic::AssertUnwindSafe(fut).catch_unwind().await;
        if let Err(err) = result {
            let msg = err
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| err.downcast_ref::<String>().map(|s| s.as_str()));
            tracing::error!(
                panic = msg.unwrap_or("unknown"),
                "sync-outbox background task panicked; restart CLI to recover"
            );
        }
    });
    drop(task);
}

/// Run the complete local outbox transaction on Tokio's blocking pool.
///
/// `SyncOutboxStore` intentionally exposes synchronous, durable filesystem
/// transactions. Moving only the lock polling sleep would still leave file
/// open/read/write/rename/fsync on an async worker, so async callers cross the
/// boundary here around the whole store operation.
async fn run_sync_outbox_io<T>(
    store: SyncOutboxStore,
    operation: impl FnOnce(SyncOutboxStore) -> std::io::Result<T> + Send + 'static,
) -> std::io::Result<T>
where
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || operation(store))
        .await
        .map_err(|error| std::io::Error::other(format!("sync outbox worker failed: {error}")))?
}

/// Notify the journal→outbox projector that a canonical session journal gained
/// durable records. This never performs an outbox fsync on the initiating
/// turn. The notification is merely a latency hint: durable source offsets
/// plus startup reconciliation make a full process crash recoverable.
pub(crate) fn schedule_sync_outbox_journal_ingestion(
    session_id: &str,
) -> JournalIngestScheduleOutcome {
    let owner_scope = astra_services::local_owner_scope();
    schedule_sync_outbox_journal_ingestion_for_owner(&owner_scope, session_id)
}

pub(crate) fn schedule_sync_outbox_journal_ingestion_for_owner(
    owner_scope: &astra_services::OwnerScope,
    session_id: &str,
) -> JournalIngestScheduleOutcome {
    if session_id.trim().is_empty() {
        return JournalIngestScheduleOutcome::InvalidSession;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::warn!(
            target: "astra_cli::cloud_sync",
            session_id,
            "journal outbox projection was deferred because no Tokio runtime is active"
        );
        return JournalIngestScheduleOutcome::RuntimeUnavailable;
    };
    let hint = JournalIngestHint::new(owner_scope, session_id);
    let sender = journal_ingest_sender(&handle);
    match sender.try_send(hint) {
        Ok(()) => JournalIngestScheduleOutcome::Scheduled,
        Err(mpsc::error::TrySendError::Full(hint)) => {
            defer_journal_ingest_hint_for_owner(&hint.owner_scope(), hint.session_id)
        }
        Err(mpsc::error::TrySendError::Closed(hint)) => {
            // Runtime teardown can close a previously global sender. Keep the
            // source in the overflow set; the next active runtime recreates
            // the worker and reconciles it from the canonical journal.
            let outcome = defer_journal_ingest_hint_for_owner(&hint.owner_scope(), hint.session_id);
            *recover_mutex_lock(&SYNC_OUTBOX_JOURNAL_INGEST_DISPATCHER) = None;
            outcome
        }
    }
}

/// Start a non-blocking recovery scan for every locally owned journal. This is
/// deliberately separate from live enqueue: it closes the crash window where
/// a durable journal append happened after the last in-memory hint but before
/// the projector wrote the derived outbox record.
pub(crate) fn schedule_sync_outbox_journal_reconcile_all() {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let owner_scope = astra_services::local_owner_scope();
    spawn_detached_tracked(&handle, async move {
        let scan_owner = owner_scope.clone();
        let sessions = match tokio::task::spawn_blocking(move || {
            session_journal::list_sessions_for_owner(&scan_owner)
        })
        .await
        {
            Ok(Ok(sessions)) => sessions,
            Ok(Err(error)) => {
                tracing::warn!(
                    target: "astra_cli::cloud_sync",
                    ?error,
                    "could not list local journals for sync-outbox recovery"
                );
                return;
            }
            Err(error) => {
                tracing::warn!(
                    target: "astra_cli::cloud_sync",
                    ?error,
                    "journal recovery worker did not complete"
                );
                return;
            }
        };
        for session_id in sessions {
            let _ = schedule_sync_outbox_journal_ingestion_for_owner(&owner_scope, &session_id);
        }
    });
}

fn journal_ingest_sender(handle: &tokio::runtime::Handle) -> mpsc::Sender<JournalIngestHint> {
    let mut slot = recover_mutex_lock(&SYNC_OUTBOX_JOURNAL_INGEST_DISPATCHER);
    if let Some(sender) = slot.as_ref().filter(|sender| !sender.is_closed()) {
        return sender.clone();
    }
    let (sender, receiver) = mpsc::channel(SYNC_OUTBOX_JOURNAL_INGEST_QUEUE_CAPACITY);
    handle.spawn(run_sync_outbox_journal_ingest_worker(receiver));
    *slot = Some(sender.clone());
    sender
}

async fn run_sync_outbox_journal_ingest_worker(mut receiver: mpsc::Receiver<JournalIngestHint>) {
    let mut pending_sessions = BTreeSet::new();
    let mut delayed_sessions = BTreeMap::<JournalIngestHint, tokio::time::Instant>::new();
    let mut retry_budget = JournalIngestRetryBudget::default();
    loop {
        let now = tokio::time::Instant::now();
        let ready_after_backoff = delayed_sessions
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(hint, _)| hint.clone())
            .collect::<Vec<_>>();
        for hint in ready_after_backoff {
            delayed_sessions.remove(&hint);
            pending_sessions.insert(hint);
        }

        if pending_sessions.is_empty() {
            if let Some(next_retry) = delayed_sessions.values().min().copied() {
                tokio::select! {
                    hint = receiver.recv() => match hint {
                        Some(hint) => {
                            if !delayed_sessions.contains_key(&hint) {
                                pending_sessions.insert(hint);
                            }
                        }
                        None => break,
                    },
                    _ = tokio::time::sleep_until(next_retry) => continue,
                }
            } else {
                match receiver.recv().await {
                    Some(hint) => {
                        pending_sessions.insert(hint);
                    }
                    None => break,
                }
            }
        }

        let deadline = tokio::time::sleep(SYNC_OUTBOX_JOURNAL_INGEST_BATCH_WINDOW);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                hint = receiver.recv() => match hint {
                    Some(hint) => {
                        if !delayed_sessions.contains_key(&hint) {
                            pending_sessions.insert(hint);
                        }
                    }
                    None => break,
                },
                _ = &mut deadline => break,
            }
        }
        let overflow = std::mem::take(&mut *recover_mutex_lock(
            &SYNC_OUTBOX_JOURNAL_INGEST_OVERFLOW,
        ));
        for hint in overflow.sessions {
            if !delayed_sessions.contains_key(&hint) {
                pending_sessions.insert(hint);
            }
        }
        if overflow.reconcile_all {
            for owner_id in overflow.reconcile_owner_ids {
                let owner_scope = astra_services::OwnerScope::user(owner_id.clone())
                    .expect("overflow owner ids came from validated owner scopes");
                let scan_owner = owner_scope.clone();
                match tokio::task::spawn_blocking(move || {
                    session_journal::list_sessions_for_owner(&scan_owner)
                })
                .await
                {
                    Ok(Ok(sessions)) => {
                        for session_id in sessions {
                            let hint = JournalIngestHint::new(&owner_scope, session_id);
                            if !delayed_sessions.contains_key(&hint) {
                                pending_sessions.insert(hint);
                            }
                        }
                    }
                    Ok(Err(error)) => tracing::warn!(
                        target: "astra_cli::cloud_sync",
                        owner_id,
                        ?error,
                        "bounded journal-ingest overflow recovery scan failed"
                    ),
                    Err(error) => tracing::warn!(
                        target: "astra_cli::cloud_sync",
                        owner_id,
                        ?error,
                        "bounded journal-ingest overflow recovery task stopped"
                    ),
                }
            }
        }

        let mut sessions_by_owner = BTreeMap::<String, Vec<String>>::new();
        for hint in std::mem::take(&mut pending_sessions) {
            sessions_by_owner
                .entry(hint.owner_id)
                .or_default()
                .push(hint.session_id);
        }
        for (owner_id, sessions) in sessions_by_owner {
            let owner_scope = astra_services::OwnerScope::user(owner_id.clone())
                .expect("pending owner ids came from validated owner scopes");
            for batch in sessions.chunks(SYNC_OUTBOX_JOURNAL_RECOVERY_BATCH_SOURCES) {
                let mut projected_any = false;
                for (session_id, outcome) in
                    reconcile_sync_outbox_journals_for_owner(&owner_scope, batch).await
                {
                    let hint = JournalIngestHint {
                        owner_id: owner_id.clone(),
                        session_id,
                    };
                    let retry_key = format!("{}\0{}", hint.owner_id, hint.session_id);
                    match outcome {
                        Ok(projected) => {
                            retry_budget.observe_success(&retry_key);
                            projected_any |= projected;
                        }
                        Err(error) => match retry_budget.observe_failure(&retry_key) {
                            JournalIngestFailureDisposition::RetryAfter(delay) => {
                                tracing::warn!(
                                    target: "astra_cli::cloud_sync",
                                    owner_id = hint.owner_id,
                                    session_id = hint.session_id,
                                    ?error,
                                    retry_after_ms = delay.as_millis(),
                                    "journal-to-outbox projection failed; retry remains bounded"
                                );
                                delayed_sessions.insert(hint, tokio::time::Instant::now() + delay);
                            }
                            JournalIngestFailureDisposition::QuarantineUntilNewHint => {
                                tracing::error!(
                                    target: "astra_cli::cloud_sync",
                                    owner_id = hint.owner_id,
                                    session_id = hint.session_id,
                                    ?error,
                                    "journal-to-outbox projection exhausted its retry budget; canonical journal remains durable and a new journal hint or process recovery will retry"
                                );
                            }
                        },
                    }
                }
                if projected_any && astra_services::local_owner_scope().id() == owner_id {
                    schedule_sync_outbox_drain();
                }
                // A startup recovery with hundreds of sources must remain
                // cooperative with the foreground turn and TUI runtime.
                tokio::task::yield_now().await;
            }
        }
    }
}

async fn reconcile_sync_outbox_journal(session_id: &str) -> Result<bool, std::io::Error> {
    let owner_scope = astra_services::local_owner_scope();
    reconcile_sync_outbox_journals_for_owner(&owner_scope, &[session_id.to_string()])
        .await
        .into_iter()
        .next()
        .map(|(_, outcome)| outcome)
        .unwrap_or_else(|| {
            Err(std::io::Error::other(
                "journal reconciliation produced no outcome",
            ))
        })
}

async fn reconcile_sync_outbox_journals_for_owner(
    owner_scope: &astra_services::OwnerScope,
    session_ids: &[String],
) -> Vec<(String, Result<bool, std::io::Error>)> {
    const STALE_OFFSET_RETRIES: usize = 3;
    if session_ids.is_empty() {
        return Vec::new();
    }
    let store = match SyncOutboxStore::for_owner(owner_scope) {
        Ok(store) => store,
        Err(error) => {
            return session_ids
                .iter()
                .cloned()
                .map(|session_id| {
                    (
                        session_id,
                        Err(std::io::Error::new(error.kind(), error.to_string())),
                    )
                })
                .collect();
        }
    };
    let mut pending = session_ids.iter().cloned().collect::<BTreeSet<_>>();
    let mut outcomes = BTreeMap::<String, Result<bool, std::io::Error>>::new();
    for attempt in 0..STALE_OFFSET_RETRIES {
        if pending.is_empty() {
            break;
        }
        let current_sessions = pending.iter().cloned().collect::<Vec<_>>();
        pending.clear();
        let offset_sessions = current_sessions.clone();
        let offsets = match run_sync_outbox_io(store.clone(), move |store| {
            store.journal_source_offsets(&offset_sessions)
        })
        .await
        {
            Ok(offsets) => offsets,
            Err(error) => {
                let message = error.to_string();
                for session_id in current_sessions {
                    outcomes.insert(
                        session_id,
                        Err(std::io::Error::new(error.kind(), message.clone())),
                    );
                }
                break;
            }
        };
        let read_owner = owner_scope.clone();
        let read_result = tokio::task::spawn_blocking(move || {
            current_sessions
                .into_iter()
                .map(|session_id| {
                    let offset = offsets.get(&session_id).copied().unwrap_or(0);
                    let delta = session_journal::read_durable_journal_append_delta_for_owner(
                        &read_owner,
                        &session_id,
                        offset,
                    );
                    (session_id, offset, delta)
                })
                .collect::<Vec<_>>()
        })
        .await;
        let reads = match read_result {
            Ok(reads) => reads,
            Err(error) => {
                let message = format!("journal delta worker failed: {error}");
                for session_id in session_ids {
                    if !outcomes.contains_key(session_id) {
                        outcomes.insert(
                            session_id.clone(),
                            Err(std::io::Error::other(message.clone())),
                        );
                    }
                }
                break;
            }
        };

        let mut deltas = Vec::new();
        for (session_id, offset, delta) in reads {
            match delta {
                Ok(delta) if delta.next_offset == offset => {
                    outcomes.insert(session_id, Ok(false));
                }
                Ok(delta) => deltas.push(SyncOutboxJournalDelta {
                    source_session_id: session_id,
                    expected_offset: offset,
                    next_offset: delta.next_offset,
                    events: delta.events,
                }),
                Err(error) => {
                    outcomes.insert(session_id, Err(error));
                }
            }
        }
        if deltas.is_empty() {
            continue;
        }
        let delta_session_ids = deltas
            .iter()
            .map(|delta| delta.source_session_id.clone())
            .collect::<Vec<_>>();
        let batch_outcome = run_sync_outbox_io(store.clone(), move |store| {
            store.append_journal_deltas(&deltas)
        })
        .await;
        match batch_outcome {
            Ok(batch_outcome) => {
                for (session_id, outcome) in batch_outcome.outcomes {
                    match outcome {
                        SyncOutboxJournalDeltaOutcome::Appended { .. } => {
                            outcomes.insert(session_id, Ok(true));
                        }
                        SyncOutboxJournalDeltaOutcome::StaleSourceOffset { .. } => {
                            if attempt + 1 < STALE_OFFSET_RETRIES {
                                pending.insert(session_id);
                            } else {
                                outcomes.insert(
                                    session_id,
                                    Err(std::io::Error::other(
                                        "sync outbox journal source offset changed repeatedly during projection",
                                    )),
                                );
                            }
                        }
                    }
                }
            }
            Err(error) => {
                let message = error.to_string();
                for session_id in delta_session_ids {
                    outcomes.insert(
                        session_id,
                        Err(std::io::Error::new(error.kind(), message.clone())),
                    );
                }
            }
        }
        if !pending.is_empty() && attempt + 1 < STALE_OFFSET_RETRIES {
            tokio::time::sleep(Duration::from_millis(5 * (attempt as u64 + 1))).await;
        }
    }
    session_ids
        .iter()
        .cloned()
        .map(|session_id| {
            let outcome = outcomes.remove(&session_id).unwrap_or_else(|| {
                Err(std::io::Error::other(
                    "journal reconciliation produced no terminal outcome",
                ))
            });
            (session_id, outcome)
        })
        .collect()
}

pub(crate) async fn read_sync_outbox_status() -> std::io::Result<SyncOutboxStatus> {
    run_sync_outbox_io(SyncOutboxStore::local(), |store| store.status()).await
}

pub(crate) async fn retry_deferred_sync_outbox_records() -> std::io::Result<u64> {
    run_sync_outbox_io(SyncOutboxStore::local(), |store| store.retry_deferred_now()).await
}

pub(crate) async fn repair_retry_exhausted_sync_outbox_records() -> std::io::Result<u64> {
    run_sync_outbox_io(SyncOutboxStore::local(), |store| {
        store.repair_retry_exhausted_poison()
    })
    .await
}

/// Result from cloud pull attempt at session start.
pub(crate) struct CloudPullResult {
    /// True when the server's preferences endpoint responded
    /// successfully (regardless of whether it returned data).
    pub cloud_reachable: bool,
}

#[must_use = "sync drain outcomes must be surfaced or deliberately handled"]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SyncOutboxDrainReport {
    pub cloud_configured: bool,
    pub attempted: u32,
    pub acked: u32,
    pub failed: u32,
    pub terminal: u32,
    pub remaining_ready: u32,
    pub pending_high_watermark_exceeded: bool,
    pub blocker: Option<SyncOutboxDrainBlocker>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncOutboxDrainBlocker {
    MissingAccessToken,
    InvalidCloudBaseUrl,
    LocalOutboxRead,
    LocalSettlementWrite,
    LocalStatusRead,
}

impl SyncOutboxDrainReport {
    pub(crate) fn is_incomplete(&self) -> bool {
        self.failed > 0 || self.terminal > 0 || self.blocker.is_some()
    }

    fn can_retry_automatically(&self) -> bool {
        self.failed > 0 && self.blocker.is_none()
    }

    pub(crate) fn user_notice(&self) -> Option<String> {
        if !self.is_incomplete() {
            return None;
        }
        let detail = match self.blocker {
            Some(SyncOutboxDrainBlocker::MissingAccessToken) => {
                "no authenticated cloud token was available".to_string()
            }
            Some(SyncOutboxDrainBlocker::InvalidCloudBaseUrl) => {
                "the configured cloud URL is invalid".to_string()
            }
            Some(SyncOutboxDrainBlocker::LocalOutboxRead) => {
                "the local sync outbox could not be read".to_string()
            }
            Some(SyncOutboxDrainBlocker::LocalSettlementWrite) => {
                "delivery results could not be persisted locally".to_string()
            }
            Some(SyncOutboxDrainBlocker::LocalStatusRead) => {
                "the remaining local sync status could not be read".to_string()
            }
            None if self.failed > 0 && self.terminal > 0 => format!(
                "{} record(s) remain retryable and {} reached a terminal local state",
                self.failed, self.terminal
            ),
            None if self.failed > 0 => format!(
                "{} queued record(s) could not yet be confirmed by the server",
                self.failed
            ),
            None => format!("{} record(s) reached a terminal local state", self.terminal),
        };
        let recovery = if self.can_retry_automatically() && self.terminal > 0 {
            "Retryable records remain queued in the background; use /sync repair for terminal records."
        } else if self.can_retry_automatically() {
            "They remain queued for background retry."
        } else if self.blocker.is_some() {
            "They remain in the local outbox; use /sync after resolving the issue."
        } else {
            "Use /sync to inspect or repair the terminal records."
        };
        Some(format!("Cloud sync is incomplete: {detail}. {recovery}"))
    }
}

pub(crate) fn schedule_sync_outbox_drain() {
    schedule_sync_outbox_drain_after(Duration::ZERO);
}

fn schedule_sync_outbox_drain_after(delay: Duration) {
    if resolve_cloud_base().is_none() {
        return;
    }
    if !delay.is_zero() {
        schedule_sync_outbox_retry_wake(delay);
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::warn!(
            target: "astra_cli::cloud_sync",
            "sync outbox drain was not scheduled because no Tokio runtime is active"
        );
        return;
    };
    let Some(schedule_guard) = try_claim_sync_outbox_drain_schedule() else {
        return;
    };
    let auth_snapshot = crate::cli::cli_config::cli_utils::cli_owner_auth_snapshot();
    spawn_detached_tracked(&handle, async move {
        let mut blocked = false;
        for _ in 0..SYNC_OUTBOX_DRAIN_BACKGROUND_ROUNDS {
            let report =
                try_drain_sync_outbox_for_snapshot(SYNC_OUTBOX_DRAIN_LIMIT, &auth_snapshot).await;
            blocked = report.blocker.is_some();
            if report.attempted > 0 && report.is_incomplete() {
                tracing::warn!(
                    target: "astra_cli::cloud_sync",
                    attempted = report.attempted,
                    acked = report.acked,
                    failed = report.failed,
                    terminal = report.terminal,
                    blocker = ?report.blocker,
                    "background sync outbox drain did not fully converge"
                );
            }
            if !report.cloud_configured || report.remaining_ready == 0 || report.attempted == 0 {
                break;
            }
            if report.pending_high_watermark_exceeded {
                // A huge monolithic outbox makes every claim/settlement an
                // O(backlog) transaction. Preserve forward progress, but cap
                // this wake to one round so sync recovery cannot monopolize
                // the foreground CLI runtime.
                break;
            }
        }
        if blocked {
            return;
        }
        let status_store = match SyncOutboxStore::for_owner(&auth_snapshot.owner_scope) {
            Ok(store) => store,
            Err(error) => {
                tracing::warn!(
                    target: "astra_cli::cloud_sync",
                    ?error,
                    owner_id = auth_snapshot.owner_scope.id(),
                    "failed to resolve owner-scoped sync outbox while scheduling the next drain"
                );
                return;
            }
        };
        let next_delay = match run_sync_outbox_io(status_store, |store| store.status()).await {
            Ok(status) => next_sync_outbox_drain_delay(&status),
            Err(error) => {
                tracing::warn!(
                    target: "astra_cli::cloud_sync",
                    ?error,
                    "failed to read sync outbox status while scheduling the next drain"
                );
                None
            }
        };
        drop(schedule_guard);
        if let Some(delay) = next_delay {
            schedule_sync_outbox_drain_after(delay);
        }
    });
}

fn schedule_sync_outbox_retry_wake(delay: Duration) {
    let Some(now) = unix_ms() else {
        return;
    };
    let deadline = now.saturating_add(delay.as_millis().min(u128::from(u64::MAX)) as u64);
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::warn!(
            target: "astra_cli::cloud_sync",
            "sync outbox retry wake was not scheduled because no Tokio runtime is active"
        );
        return;
    };
    let Some(wake_guard) = claim_sync_outbox_retry_wake_deadline(deadline) else {
        return;
    };
    spawn_detached_tracked(&handle, async move {
        let _wake_guard = wake_guard;
        tokio::time::sleep(delay).await;
        if SYNC_OUTBOX_RETRY_WAKE_DEADLINE_MS
            .compare_exchange(deadline, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            schedule_sync_outbox_drain();
        }
    });
}

fn try_claim_sync_outbox_drain_schedule() -> Option<SyncOutboxDrainScheduleGuard> {
    let owner = loop {
        let candidate = SYNC_OUTBOX_NEXT_DRAIN_OWNER.fetch_add(1, Ordering::Relaxed);
        if candidate != 0 {
            break candidate;
        }
    };
    SYNC_OUTBOX_DRAIN_OWNER
        .compare_exchange(0, owner, Ordering::AcqRel, Ordering::Acquire)
        .ok()
        .map(|_| SyncOutboxDrainScheduleGuard { owner })
}

#[cfg(test)]
fn release_sync_outbox_drain_schedule() {
    SYNC_OUTBOX_DRAIN_OWNER.store(0, Ordering::Release);
}

fn claim_sync_outbox_retry_wake_deadline(deadline: u64) -> Option<SyncOutboxRetryWakeGuard> {
    let mut current = SYNC_OUTBOX_RETRY_WAKE_DEADLINE_MS.load(Ordering::Acquire);
    loop {
        if current != 0 && current <= deadline {
            return None;
        }
        match SYNC_OUTBOX_RETRY_WAKE_DEADLINE_MS.compare_exchange_weak(
            current,
            deadline,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Some(SyncOutboxRetryWakeGuard { deadline }),
            Err(next) => current = next,
        }
    }
}

#[cfg(test)]
fn release_sync_outbox_retry_wake_schedule() {
    SYNC_OUTBOX_RETRY_WAKE_DEADLINE_MS.store(0, Ordering::Release);
}

fn next_sync_outbox_drain_delay(status: &SyncOutboxStatus) -> Option<Duration> {
    if status.claimable > 0 {
        if status.pending_high_watermark_exceeded {
            return Some(SYNC_OUTBOX_DEGRADED_DRAIN_COOLDOWN);
        }
        return Some(Duration::ZERO);
    }
    let retry_at = status.next_retry_after_unix_ms?;
    let now = unix_ms()?;
    Some(Duration::from_millis(retry_at.saturating_sub(now)))
}

/// Parse a boolean preference value. Accepts "true"/"1"/"yes"/"on" as true,
/// "false"/"0"/"no"/"off" as false, anything else returns the default.
fn parse_bool_pref(value: &str, default: bool) -> bool {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => true,
        "false" | "0" | "no" | "off" => false,
        _ => default,
    }
}

/// Resolve the astra server base URL. Returns `None` when no server
/// is configured (offline mode). Native auth owns its endpoint; legacy profiles
/// retain `ASTRA_API_URL` selection.
fn resolve_cloud_base() -> Option<String> {
    session_runtime::resolve_cloud_base()
}

/// Check whether the cloud preference endpoint is reachable.
/// Best-effort: returns `cloud_reachable: false` when cloud is
/// unconfigured or unreachable.
pub(crate) async fn try_cloud_pull(profile_name: &str) -> CloudPullResult {
    let Some(cloud_base) = resolve_cloud_base() else {
        return CloudPullResult {
            cloud_reachable: false,
        };
    };
    let token = session_runtime::current_access_token(Some(profile_name));
    let cloud_reachable =
        crate::cli::preferences_client::probe_cloud_reachable(&cloud_base, token.as_deref()).await;
    CloudPullResult { cloud_reachable }
}

/// Pull user preferences from cloud at session start.
/// Merges cloud preferences into local state (cloud-wins). Returns
/// keys merged (for journal audit).
pub(crate) async fn try_cloud_pull_preferences(state: &mut SessionState) -> Vec<String> {
    let Some(cloud_base) = resolve_cloud_base() else {
        return Vec::new();
    };
    // Best-effort: pick token from whichever profile the session
    // currently holds. Empty token still works for local dev
    // servers without auth; the server's auth_service decides.
    let token = session_runtime::current_access_token(None);
    let prefs =
        match crate::cli::preferences_client::pull_all_preferences(&cloud_base, token.as_deref())
            .await
        {
            Ok(prefs) => prefs,
            Err(e) => {
                tracing::warn!(
                    target: "astra_cli::cloud_sync",
                    error = %e,
                    "preference pull skipped"
                );
                return Vec::new();
            }
        };
    if prefs.is_empty() {
        return Vec::new();
    }
    let keys: Vec<String> = prefs.iter().map(|(k, _)| k.clone()).collect();
    for (key, value) in &prefs {
        match key.as_str() {
            pref_keys::EXPLAIN_MODE => {
                state.explain = match value.as_str() {
                    "on" => ExplainMode::On,
                    "verbose" => ExplainMode::Verbose,
                    _ => ExplainMode::Off,
                };
            }
            pref_keys::BLOCKED_TOOLS => {
                if let Ok(tools) = serde_json::from_str::<Vec<String>>(value) {
                    let existing: std::collections::HashSet<String> = state
                        .tool_health_entries
                        .iter()
                        .map(|e| e.name.clone())
                        .collect();
                    for tool_name in tools {
                        if !existing.contains(&tool_name) {
                            state.tool_health_entries.push(ToolHealthEntry {
                                name: tool_name,
                                total_calls: 0,
                                total_failures: 0,
                                input_validation_failures: 0,
                                failure_rate: 0.0,
                                last_updated_epoch: 0,
                                recent_outcomes: vec![],
                            });
                        }
                    }
                }
            }
            pref_keys::AUTO_MEMORY_ENABLED => {
                state.auto_memory_enabled = parse_bool_pref(value, true);
            }
            pref_keys::NOTIFICATIONS_ENABLED => {
                state.notifications_enabled = parse_bool_pref(value, true);
            }
            pref_keys::NOTIFICATION_METHOD => match value.parse() {
                Ok(method) => state.notification_method = method,
                Err(err) => tracing::warn!(
                    value = %value,
                    error = %err,
                    "ignoring invalid notification_method preference"
                ),
            },
            pref_keys::NOTIFICATION_THRESHOLD_SECS => {
                if let Ok(n) = value.parse::<u64>() {
                    state.notification_threshold_secs = n;
                }
            }
            _ => {}
        }
    }
    // Status is recorded in the journal (`append_cloud_pull_sync_journal`)
    // and reflected in `/state` / `/account`. We intentionally do not
    // write to stderr here: this path runs after `/login` while the
    // TUI owns the terminal, and a stray "✓ Pulled N preferences"
    // line would scribble across the rendered viewport.
    keys
}

pub(crate) async fn try_drain_sync_outbox(limit: usize) -> SyncOutboxDrainReport {
    let auth_snapshot = crate::cli::cli_config::cli_utils::cli_owner_auth_snapshot();
    try_drain_sync_outbox_for_snapshot(limit, &auth_snapshot).await
}

async fn try_drain_sync_outbox_for_snapshot(
    limit: usize,
    auth_snapshot: &crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot,
) -> SyncOutboxDrainReport {
    if auth_snapshot.native_binding.is_none() && crate::cli::native_auth::active().is_some() {
        return SyncOutboxDrainReport {
            cloud_configured: true,
            blocker: Some(SyncOutboxDrainBlocker::MissingAccessToken),
            ..Default::default()
        };
    }
    let Some(cloud_base) = auth_snapshot
        .native_binding
        .as_ref()
        .map(|binding| binding.endpoint().to_owned())
        .or_else(resolve_cloud_base)
    else {
        return SyncOutboxDrainReport::default();
    };
    let mut report = SyncOutboxDrainReport {
        cloud_configured: true,
        ..Default::default()
    };
    let store = match SyncOutboxStore::for_owner(&auth_snapshot.owner_scope) {
        Ok(store) => store,
        Err(error) => {
            report.blocker = Some(SyncOutboxDrainBlocker::LocalOutboxRead);
            tracing::warn!(
                target: "astra_cli::cloud_sync",
                ?error,
                owner_id = auth_snapshot.owner_scope.id(),
                "sync outbox drain skipped because its owner scope is invalid"
            );
            return report;
        }
    };
    match run_sync_outbox_io(store.clone(), |store| store.status()).await {
        Ok(status) => {
            report.remaining_ready = status.claimable.min(u64::from(u32::MAX)) as u32;
            report.pending_high_watermark_exceeded = status.pending_high_watermark_exceeded;
            if report.remaining_ready == 0 {
                return report;
            }
        }
        Err(error) => {
            report.blocker = Some(SyncOutboxDrainBlocker::LocalOutboxRead);
            tracing::warn!(
                target: "astra_cli::cloud_sync",
                ?error,
                "sync outbox drain skipped because local outbox status is unreadable"
            );
            return report;
        }
    }
    let token = if let Some(binding) = &auth_snapshot.native_binding {
        binding.access_token().await.ok()
    } else {
        auth_snapshot.access_token().await
    };
    let Some(token) = token else {
        report.blocker = Some(SyncOutboxDrainBlocker::MissingAccessToken);
        tracing::debug!(
            target: "astra_cli::cloud_sync",
            "sync outbox drain skipped because no access token is available"
        );
        return report;
    };
    let client = match astra_thin_client::ThinClient::new(&cloud_base, Some(token.clone())) {
        Ok(client) => match &auth_snapshot.native_binding {
            Some(binding) => client.with_bearer_provider(binding.clone()),
            None => client,
        },
        Err(error) => {
            report.blocker = Some(SyncOutboxDrainBlocker::InvalidCloudBaseUrl);
            tracing::warn!(
                target: "astra_cli::cloud_sync",
                ?error,
                "sync outbox drain skipped because cloud base URL is invalid"
            );
            return report;
        }
    };
    let records = match run_sync_outbox_io(store.clone(), move |store| {
        store.claim_ready_records(limit, SYNC_OUTBOX_DRAIN_SERIALIZED_BYTE_BUDGET)
    })
    .await
    {
        Ok(records) => records,
        Err(error) => {
            report.blocker = Some(SyncOutboxDrainBlocker::LocalOutboxRead);
            tracing::warn!(
                target: "astra_cli::cloud_sync",
                ?error,
                "sync outbox drain skipped because local outbox is unreadable"
            );
            return report;
        }
    };
    report.attempted = report
        .attempted
        .saturating_add(records.len().min(u32::MAX as usize) as u32);
    let settlements = deliver_sync_outbox_records(&client, &token, records).await;
    if !settlements.is_empty() {
        let attempted_settlements = settlements.len();
        let settlement_result = run_sync_outbox_io(store.clone(), move |store| {
            store.settle_delivery_batch(&settlements)
        })
        .await;
        record_settlement_result(&mut report, attempted_settlements, settlement_result);
    }
    match run_sync_outbox_io(store, |store| store.status()).await {
        Ok(status) => {
            report.remaining_ready = status.claimable.min(u64::from(u32::MAX)) as u32;
            report.pending_high_watermark_exceeded = status.pending_high_watermark_exceeded;
        }
        Err(error) => {
            report
                .blocker
                .get_or_insert(SyncOutboxDrainBlocker::LocalStatusRead);
            tracing::warn!(
                target: "astra_cli::cloud_sync",
                ?error,
                "failed to read sync outbox status after drain"
            );
        }
    }
    report
}

struct SyncOutboxDeliveryBatch {
    records: Vec<SyncOutboxRecord>,
    request: SyncOutboxEventBatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SyncOutboxReconciliation {
    Confirmed,
    /// The cloud stores this event_id with a different payload. No retry can
    /// resolve that, so the record must become terminal.
    Conflict,
    Missing,
}

/// One HTTP batch attempt: authoritative per-record verdicts, a session that is
/// gone, or a request-level failure whose commit outcome is unknown.
enum SyncOutboxBatchAttempt {
    Verdicts(SyncOutboxBatchResponse),
    SessionUnavailable,
    RequestFailed(String),
}

async fn attempt_sync_outbox_batch(
    client: &astra_thin_client::ThinClient,
    token: &str,
    request: &SyncOutboxEventBatch,
) -> SyncOutboxBatchAttempt {
    match tokio::time::timeout(
        SYNC_OUTBOX_RECORD_DELIVERY_TIMEOUT,
        client.post_sync_outbox_events_json(Some(token), request),
    )
    .await
    {
        Ok(Ok(response)) if response.schema_version == SYNC_OUTBOX_BATCH_SCHEMA_VERSION => {
            SyncOutboxBatchAttempt::Verdicts(response)
        }
        Ok(Ok(response)) => SyncOutboxBatchAttempt::RequestFailed(format!(
            "cloud returned an unsupported sync batch response version: {}",
            response.schema_version
        )),
        Ok(Err(astra_thin_client::ThinClientError::Api { status, body }))
            if status.as_u16() == 409
                && sync_outbox_error_code(&body).as_deref()
                    == Some(SYNC_OUTBOX_SESSION_UNAVAILABLE_ERROR_CODE) =>
        {
            SyncOutboxBatchAttempt::SessionUnavailable
        }
        Ok(Err(error)) => SyncOutboxBatchAttempt::RequestFailed(error.to_string()),
        Err(_) => SyncOutboxBatchAttempt::RequestFailed(format!(
            "cloud event batch delivery timed out after {}ms",
            SYNC_OUTBOX_RECORD_DELIVERY_TIMEOUT.as_millis()
        )),
    }
}

fn sync_outbox_error_code(body: &str) -> Option<String> {
    serde_json::from_str::<astra_core::ErrorResponse>(body)
        .ok()
        .and_then(|response| response.error_code)
}

async fn deliver_sync_outbox_records(
    client: &astra_thin_client::ThinClient,
    token: &str,
    records: Vec<SyncOutboxRecord>,
) -> Vec<SyncOutboxDeliverySettlement> {
    deliver_sync_outbox_records_within(
        client,
        token,
        records,
        Instant::now() + SYNC_OUTBOX_DELIVERY_DEADLINE,
    )
    .await
}

/// Delivers sessions concurrently within a fixed bound. Batches of one session
/// stay ordered, and no request starts after `deadline`, so a slow session
/// cannot make the whole drain outlive the outbox in-flight lease.
async fn deliver_sync_outbox_records_within(
    client: &astra_thin_client::ThinClient,
    token: &str,
    records: Vec<SyncOutboxRecord>,
    deadline: Instant,
) -> Vec<SyncOutboxDeliverySettlement> {
    futures_util::stream::iter(
        sync_outbox_session_batches(records, SYNC_OUTBOX_BATCH_BODY_LIMIT_BYTES)
            .map(|batches| deliver_sync_outbox_session(client, token, batches, deadline)),
    )
    .buffered(SYNC_OUTBOX_MAX_CONCURRENT_SESSIONS)
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .flatten()
    .collect()
}

async fn deliver_sync_outbox_session(
    client: &astra_thin_client::ThinClient,
    token: &str,
    mut batches: SyncOutboxSessionBatches,
    deadline: Instant,
) -> Vec<SyncOutboxDeliverySettlement> {
    let mut settlements = Vec::new();
    for batch in batches.by_ref() {
        if Instant::now() >= deadline {
            settlements.extend(deferred_settlements(
                batch.records.into_iter(),
                SYNC_OUTBOX_DEADLINE_ERROR,
            ));
            break;
        }
        deliver_sync_outbox_batch_records(client, token, batch, &mut settlements, deadline).await;
    }
    settlements.extend(deferred_settlements(
        batches.into_remaining(),
        SYNC_OUTBOX_DEADLINE_ERROR,
    ));
    settlements
}

/// Settles one batch. Per-record verdicts are authoritative; a request-level
/// failure is reconciled with one read-only batch read, so healthy siblings are
/// never re-posted and one bad record never charges the whole batch.
async fn deliver_sync_outbox_batch_records(
    client: &astra_thin_client::ThinClient,
    token: &str,
    SyncOutboxDeliveryBatch { records, request }: SyncOutboxDeliveryBatch,
    settlements: &mut Vec<SyncOutboxDeliverySettlement>,
    deadline: Instant,
) {
    // The builder admits an oversized singleton so the record is never lost.
    // It can never fit the server body limit, so settle it here instead of
    // sending a request that is guaranteed to fail.
    if records.len() == 1
        && serde_json::to_vec(&request)
            .is_ok_and(|body| body.len() > SYNC_OUTBOX_BATCH_BODY_LIMIT_BYTES)
    {
        settlements.push(SyncOutboxDeliverySettlement::Failed {
            record_id: records[0].record_id.clone(),
            error: format!(
                "record exceeds the {SYNC_OUTBOX_BATCH_BODY_LIMIT_BYTES} byte sync batch body limit"
            ),
        });
        return;
    }
    match attempt_sync_outbox_batch(client, token, &request).await {
        SyncOutboxBatchAttempt::Verdicts(response) => {
            settle_sync_outbox_verdicts(records, response, settlements);
        }
        SyncOutboxBatchAttempt::SessionUnavailable => {
            settlements.extend(records.into_iter().map(|record| {
                SyncOutboxDeliverySettlement::Failed {
                    record_id: record.record_id,
                    error: "cloud session is unavailable for event ingestion".to_string(),
                }
            }));
        }
        SyncOutboxBatchAttempt::RequestFailed(error) => {
            if Instant::now() >= deadline {
                settlements.extend(deferred_settlements(
                    records.into_iter(),
                    SYNC_OUTBOX_DEADLINE_ERROR,
                ));
                return;
            }
            settle_sync_outbox_reconciliation(client, token, records, error, settlements).await;
        }
    }
}

/// Per-record verdicts are authoritative: accepted records are acked and
/// rejected records terminate, so one bad record never charges its siblings.
fn settle_sync_outbox_verdicts(
    records: Vec<SyncOutboxRecord>,
    response: SyncOutboxBatchResponse,
    settlements: &mut Vec<SyncOutboxDeliverySettlement>,
) {
    let expected = records
        .iter()
        .map(|record| record.record_id.as_str())
        .collect::<BTreeSet<_>>();
    let mut acks = BTreeMap::new();
    let mut rejections = BTreeMap::new();
    let usable = response.acks.iter().all(|ack| {
        expected.contains(ack.record_id.as_str())
            && acks.insert(ack.record_id.clone(), ack).is_none()
    }) && response.rejections.iter().all(|rejection| {
        expected.contains(rejection.record_id.as_str())
            && !acks.contains_key(&rejection.record_id)
            && rejections
                .insert(rejection.record_id.clone(), rejection)
                .is_none()
    });
    if !usable {
        settlements.extend(deferred_settlements(
            records.into_iter(),
            "cloud returned an uncorrelated sync batch verdict",
        ));
        return;
    }
    for record in records {
        if let Some(ack) = acks.get(&record.record_id) {
            if ack.matches(&record.record_id, &record.payload_hash) {
                settlements.push(SyncOutboxDeliverySettlement::Ack {
                    record_id: record.record_id,
                    payload_hash: record.payload_hash,
                });
            } else {
                settlements.push(SyncOutboxDeliverySettlement::Failed {
                    record_id: record.record_id,
                    error: "cloud acknowledged a different payload for this record".to_string(),
                });
            }
        } else if let Some(rejection) = rejections.get(&record.record_id) {
            settlements.push(SyncOutboxDeliverySettlement::Failed {
                record_id: record.record_id,
                error: format!("{}: {}", rejection.error_code, rejection.detail),
            });
        } else {
            settlements.push(SyncOutboxDeliverySettlement::Deferred {
                record_id: record.record_id,
                error: "cloud accepted the batch without a verdict for this record".to_string(),
            });
        }
    }
}

/// A request-level failure has an unknown commit outcome. One read-only batch
/// reconciliation settles every record without re-posting healthy siblings.
async fn settle_sync_outbox_reconciliation(
    client: &astra_thin_client::ThinClient,
    token: &str,
    records: Vec<SyncOutboxRecord>,
    delivery_error: String,
    settlements: &mut Vec<SyncOutboxDeliverySettlement>,
) {
    let Some(reconciliations) = reconcile_sync_outbox_deliveries(client, token, &records).await
    else {
        settlements.extend(deferred_settlements(records.into_iter(), &delivery_error));
        return;
    };
    for (record, reconciliation) in records.into_iter().zip(reconciliations) {
        match reconciliation {
            SyncOutboxReconciliation::Confirmed => {
                settlements.push(SyncOutboxDeliverySettlement::Ack {
                    record_id: record.record_id,
                    payload_hash: record.payload_hash,
                });
            }
            SyncOutboxReconciliation::Conflict => {
                let error = format!(
                    "cloud stores {} with a different payload; retry cannot reconcile it",
                    record.record_id
                );
                settlements.push(SyncOutboxDeliverySettlement::Failed {
                    record_id: record.record_id,
                    error,
                });
            }
            SyncOutboxReconciliation::Missing => {
                settlements.push(SyncOutboxDeliverySettlement::Deferred {
                    record_id: record.record_id,
                    error: delivery_error.clone(),
                });
            }
        }
    }
}

/// Records skipped for time stay retryable; deferral never consumes the finite
/// poison budget, so the next drain picks them up.
fn deferred_settlements(
    records: impl Iterator<Item = SyncOutboxRecord>,
    error: &str,
) -> impl Iterator<Item = SyncOutboxDeliverySettlement> {
    let error = error.to_string();
    records.map(move |record| SyncOutboxDeliverySettlement::Deferred {
        record_id: record.record_id,
        error: error.clone(),
    })
}

/// Splits one drain into per-session units so sessions can be delivered
/// concurrently while each session keeps its event order.
fn sync_outbox_session_batches(
    records: Vec<SyncOutboxRecord>,
    byte_limit: usize,
) -> impl Iterator<Item = SyncOutboxSessionBatches> {
    let mut by_session = BTreeMap::<String, Vec<SyncOutboxRecord>>::new();
    for record in records {
        by_session
            .entry(record.session_id.clone())
            .or_default()
            .push(record);
    }
    let empty_batch_bytes = serde_json::to_vec(&SyncOutboxEventBatch::new(Vec::new()))
        .expect("sync outbox batch JSON must serialize")
        .len();
    by_session
        .into_values()
        .map(move |records| SyncOutboxSessionBatches {
            records: records.into_iter().peekable(),
            byte_limit,
            empty_batch_bytes,
        })
}

/// Builds one request at a time. Original outbox records remain bounded by the
/// drain limit, while derived event JSON is retained only for the active batch.
struct SyncOutboxSessionBatches {
    records: std::iter::Peekable<std::vec::IntoIter<SyncOutboxRecord>>,
    byte_limit: usize,
    empty_batch_bytes: usize,
}

impl SyncOutboxSessionBatches {
    fn into_remaining(self) -> impl Iterator<Item = SyncOutboxRecord> {
        self.records
    }
}

impl Iterator for SyncOutboxSessionBatches {
    type Item = SyncOutboxDeliveryBatch;

    fn next(&mut self) -> Option<Self::Item> {
        let mut records = Vec::with_capacity(MAX_SYNC_OUTBOX_BATCH_EVENTS);
        let mut events = Vec::with_capacity(MAX_SYNC_OUTBOX_BATCH_EVENTS);
        let mut encoded_bytes = self.empty_batch_bytes;
        while let Some(record) = self.records.peek() {
            let event = event_body_from_outbox_record(record);
            let event_bytes = serde_json::to_vec(&event)
                .expect("sync outbox event JSON must serialize")
                .len();
            let additional_bytes = event_bytes + usize::from(!events.is_empty());
            if !events.is_empty()
                && (records.len() == MAX_SYNC_OUTBOX_BATCH_EVENTS
                    || encoded_bytes + additional_bytes > self.byte_limit)
            {
                break;
            }
            encoded_bytes += additional_bytes;
            records.push(self.records.next().expect("peeked sync outbox record"));
            events.push(event);
        }
        (!events.is_empty()).then(|| SyncOutboxDeliveryBatch {
            records,
            request: SyncOutboxEventBatch::new(events),
        })
    }
}

fn record_settlement_result(
    report: &mut SyncOutboxDrainReport,
    attempted_settlements: usize,
    result: std::io::Result<SyncOutboxSettlementReport>,
) {
    match result {
        Ok(settlement) => {
            report.acked += settlement.acked;
            report.failed += settlement.failed;
            report.terminal += settlement.missing.saturating_add(settlement.poisoned);
            if settlement.missing > 0 || settlement.poisoned > 0 {
                tracing::warn!(
                    target: "astra_cli::cloud_sync",
                    missing = settlement.missing,
                    poisoned = settlement.poisoned,
                    "sync outbox settlement produced non-retryable local outcomes"
                );
            }
        }
        Err(error) => {
            report.failed += attempted_settlements.min(u32::MAX as usize) as u32;
            report.blocker = Some(SyncOutboxDrainBlocker::LocalSettlementWrite);
            tracing::warn!(
                target: "astra_cli::cloud_sync",
                ?error,
                "failed to persist sync outbox delivery settlements"
            );
        }
    }
}

fn unix_ms() -> Option<u64> {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => Some(duration.as_millis() as u64),
        Err(error) => {
            tracing::error!(
                ?error,
                "system clock is before UNIX epoch; skipping sync outbox scheduling"
            );
            None
        }
    }
}

fn event_body_from_outbox_record(record: &SyncOutboxRecord) -> Value {
    let mut metadata = record
        .payload
        .get("metadata")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    metadata.insert(
        "sync_outbox".to_string(),
        json!({
            "schema_version": record.schema_version,
            "record_id": record.record_id,
            "sequence": record.sequence,
            "payload_hash": record.payload_hash,
            "event_ts": record.event_ts,
        }),
    );
    json!({
        "event_id": record.record_id,
        "session_id": record.session_id,
        "event_type": record.event_type,
        "content": record.canonical_payload_json(),
        "agent_id": "edge_sync",
        "agent_version": env!("CARGO_PKG_VERSION"),
        "metadata": Value::Object(metadata),
    })
}

/// Read-only reconciliation for a batch whose commit outcome is unknown.
/// `None` means the response cannot be trusted; callers must defer.
async fn reconcile_sync_outbox_deliveries(
    client: &astra_thin_client::ThinClient,
    token: &str,
    records: &[SyncOutboxRecord],
) -> Option<Vec<SyncOutboxReconciliation>> {
    if records.is_empty() {
        return Some(Vec::new());
    }
    let request = SyncOutboxReconcileRequest::new(
        records
            .iter()
            .map(|record| record.record_id.clone())
            .collect(),
    );
    let response = match tokio::time::timeout(
        SYNC_OUTBOX_RECORD_DELIVERY_TIMEOUT,
        client.reconcile_sync_outbox_events_json(Some(token), &request),
    )
    .await
    {
        Ok(Ok(response)) => response,
        _ => return None,
    };
    let expected = records
        .iter()
        .map(|record| record.record_id.as_str())
        .collect::<BTreeSet<_>>();
    let mut seen = BTreeSet::new();
    if response.schema_version != SYNC_OUTBOX_BATCH_SCHEMA_VERSION
        || !response.rejections.is_empty()
        || response.acks.len() > records.len()
        || !response.acks.iter().all(|ack| {
            expected.contains(ack.record_id.as_str()) && seen.insert(ack.record_id.as_str())
        })
    {
        return None;
    }
    Some(
        records
            .iter()
            .map(|record| {
                match response
                    .acks
                    .iter()
                    .find(|ack| ack.record_id == record.record_id)
                {
                    Some(ack) if ack.matches(&record.record_id, &record.payload_hash) => {
                        SyncOutboxReconciliation::Confirmed
                    }
                    Some(_) => SyncOutboxReconciliation::Conflict,
                    None => SyncOutboxReconciliation::Missing,
                }
            })
            .collect(),
    )
}

// ═══════════════════════════════════════════ Journal Helpers ═══════════════════════

/// When set to `1`, `session_startup` also journals a sync marker if MatrixOne was reachable but
/// returned no preferences (audit / connectivity proof).
pub(crate) const ASTRA_JOURNAL_CLOUD_EMPTY_ACK: &str = "ASTRA_JOURNAL_CLOUD_EMPTY_ACK";

pub(crate) fn cloud_pull_warrants_sync_marker(
    pull: &CloudPullResult,
    pref_keys: &[String],
) -> bool {
    pull.cloud_reachable && !pref_keys.is_empty()
}

fn cloud_pull_empty_ack_desired_for_source(source: &str) -> bool {
    if source == "post_login" {
        return true;
    }
    std::env::var(ASTRA_JOURNAL_CLOUD_EMPTY_ACK).ok().as_deref() == Some("1")
}

pub(crate) fn should_append_cloud_pull_journal(
    pull: &CloudPullResult,
    pref_keys: &[String],
    source: &str,
) -> bool {
    if !pull.cloud_reachable {
        return false;
    }
    if cloud_pull_warrants_sync_marker(pull, pref_keys) {
        return true;
    }
    cloud_pull_empty_ack_desired_for_source(source)
}

pub(crate) fn append_cloud_pull_sync_journal(
    state: &SessionState,
    profile: &str,
    source: &str,
    pull: &CloudPullResult,
    pref_keys: &[String],
) {
    append_cloud_pull_sync_journal_with_enqueue(
        state,
        profile,
        source,
        pull,
        pref_keys,
        enqueue_ingestion_pub,
    );
}

pub(crate) fn append_cloud_pull_sync_journal_for_immediate_drain(
    state: &SessionState,
    profile: &str,
    source: &str,
    pull: &CloudPullResult,
    pref_keys: &[String],
) {
    append_cloud_pull_sync_journal_with_enqueue(
        state,
        profile,
        source,
        pull,
        pref_keys,
        enqueue_ingestion_for_immediate_drain_pub,
    );
}

fn append_cloud_pull_sync_journal_with_enqueue(
    state: &SessionState,
    profile: &str,
    source: &str,
    pull: &CloudPullResult,
    pref_keys: &[String],
    enqueue: fn(&SessionState, &session_journal::JournalEvent),
) {
    if !should_append_cloud_pull_journal(pull, pref_keys, source) {
        return;
    }
    let Some(sid) = state.session_id.as_deref() else {
        return;
    };
    let reachable_empty_ack =
        pull.cloud_reachable && !cloud_pull_warrants_sync_marker(pull, pref_keys);
    let evt = session_journal::JournalEvent::cloud_pull_sync_marker(
        Some(sid),
        profile,
        source,
        pref_keys,
        reachable_empty_ack,
    );
    let Some(writer) = state.journal.as_ref() else {
        tracing::warn!(
            session_id = sid,
            context = "cloud_sync:cloud_pull_sync_marker",
            "skipping cloud pull sync marker because the live session has no journal"
        );
        return;
    };
    if writer.session_id() != sid {
        tracing::error!(
            session_id = sid,
            journal_session_id = writer.session_id(),
            context = "cloud_sync:cloud_pull_sync_marker",
            "refusing to append a cloud pull marker through another session's journal"
        );
        return;
    }
    if let Err(error) = writer.append(&evt) {
        tracing::warn!(
            session_id = sid,
            context = "cloud_sync:cloud_pull_sync_marker",
            ?error,
            "failed to append session journal event"
        );
    } else {
        enqueue(state, &evt);
    }
}

/// Re-sync preferences and queued edge events after authentication. Login is
/// still successful when sync is delayed, but the structured report makes the
/// partial outcome visible to every UI path.
pub(crate) async fn post_auth_cloud_resync(
    profile: Option<&str>,
    state: &mut SessionState,
) -> SyncOutboxDrainReport {
    let profile_name = profile.unwrap_or("default");
    let pull = try_cloud_pull(profile_name).await;
    let pref_keys = try_cloud_pull_preferences(state).await;
    append_cloud_pull_sync_journal_for_immediate_drain(
        state,
        profile_name,
        "post_login",
        &pull,
        &pref_keys,
    );
    let report = try_drain_sync_outbox(SYNC_OUTBOX_DRAIN_LIMIT).await;
    if report.is_incomplete() {
        tracing::warn!(
            target: "astra_cli::cloud_sync",
            attempted = report.attempted,
            acked = report.acked,
            failed = report.failed,
            terminal = report.terminal,
            remaining_ready = report.remaining_ready,
            blocker = ?report.blocker,
            "post-authentication cloud sync did not fully converge"
        );
    }
    if report.can_retry_automatically() {
        schedule_sync_outbox_drain();
    }
    report
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        CloudPullResult, JournalIngestFailureDisposition, JournalIngestRetryBudget,
        JournalIngestScheduleOutcome, MAX_SYNC_OUTBOX_BATCH_EVENTS,
        SYNC_OUTBOX_BATCH_BODY_LIMIT_BYTES, SYNC_OUTBOX_JOURNAL_INGEST_MAX_CONSECUTIVE_FAILURES,
        SYNC_OUTBOX_JOURNAL_INGEST_OVERFLOW, SYNC_OUTBOX_JOURNAL_INGEST_OVERFLOW_CAPACITY,
        SyncOutboxDeliveryBatch, SyncOutboxDrainReport, claim_sync_outbox_retry_wake_deadline,
        cloud_pull_warrants_sync_marker, defer_journal_ingest_hint,
        deliver_sync_outbox_batch_records, deliver_sync_outbox_records,
        deliver_sync_outbox_records_within, event_body_from_outbox_record,
        next_sync_outbox_drain_delay, reconcile_sync_outbox_journal, record_settlement_result,
        release_sync_outbox_drain_schedule, release_sync_outbox_retry_wake_schedule,
        run_sync_outbox_io, schedule_sync_outbox_journal_ingestion,
        should_append_cloud_pull_journal, sync_outbox_session_batches,
        try_claim_sync_outbox_drain_schedule,
    };

    fn delivery_batches(
        records: Vec<SyncOutboxRecord>,
    ) -> impl Iterator<Item = SyncOutboxDeliveryBatch> {
        sync_outbox_session_batches(records, SYNC_OUTBOX_BATCH_BODY_LIMIT_BYTES).flatten()
    }

    fn delivery_batches_with_limit(
        records: Vec<SyncOutboxRecord>,
        byte_limit: usize,
    ) -> impl Iterator<Item = SyncOutboxDeliveryBatch> {
        sync_outbox_session_batches(records, byte_limit).flatten()
    }
    use astra_services::session_journal::{JournalEvent, JournalWriter, ProcessJournalDirGuard};
    use astra_services::{
        SyncDomain, SyncOutboxRecord, SyncOutboxRecordState, SyncOutboxSettlementReport,
        SyncOutboxStatus, SyncOutboxStore,
    };
    use fs2::FileExt;

    fn delivery_record(sequence: u64, session_id: &str) -> SyncOutboxRecord {
        SyncOutboxRecord {
            schema_version: 2,
            sequence,
            record_id: format!("record-{sequence}"),
            domain: SyncDomain::Events,
            session_id: session_id.to_string(),
            event_type: "journal".to_string(),
            event_ts: String::new(),
            payload_hash: format!("sha256:{sequence}"),
            payload: serde_json::json!({}),
            state: SyncOutboxRecordState::Pending,
            attempts: 0,
            next_retry_after_unix_ms: None,
            last_error: None,
            poison_kind: None,
            poison_reason: None,
            created_at_unix_ms: 0,
            updated_at_unix_ms: 0,
            acked_at_unix_ms: None,
        }
    }

    async fn mount_sync_outbox_reconciliation(
        server: &wiremock::MockServer,
        records: &[SyncOutboxRecord],
        confirmed: &[bool],
    ) {
        use astra_sync_protocol::{
            SyncOutboxAck, SyncOutboxBatchResponse, SyncOutboxIngestionStatus,
            SyncOutboxReconcileRequest,
        };
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, ResponseTemplate};

        assert_eq!(records.len(), confirmed.len());
        let request = SyncOutboxReconcileRequest::new(
            records
                .iter()
                .map(|record| record.record_id.clone())
                .collect(),
        );
        let acks = records
            .iter()
            .zip(confirmed)
            .filter(|(_, confirmed)| **confirmed)
            .map(|(record, _)| {
                SyncOutboxAck::new(
                    &record.record_id,
                    &record.payload_hash,
                    SyncOutboxIngestionStatus::IdempotentReplay,
                )
            })
            .collect();
        Mock::given(method("POST"))
            .and(path("/sync/outbox/events/reconcile"))
            .and(body_json(serde_json::to_value(request).unwrap()))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(SyncOutboxBatchResponse::new(acks, Vec::new())),
            )
            .expect(1)
            .mount(server)
            .await;
    }

    fn sync_outbox_conflict_response(error_code: &str) -> wiremock::ResponseTemplate {
        wiremock::ResponseTemplate::new(409).set_body_json(
            astra_core::ErrorResponse::new("sync outbox request rejected")
                .with_error_code(error_code),
        )
    }

    async fn assert_one_batch_and_reconciliation_request(server: &wiremock::MockServer) {
        let requests = server
            .received_requests()
            .await
            .expect("received outbox requests");
        assert_eq!(
            requests.len(),
            2,
            "one batch POST and one reconciliation POST; no per-record calls"
        );
        assert!(
            requests
                .iter()
                .all(|request| request.method.as_str() == "POST")
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.url.path() == "/sync/outbox/events")
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.url.path() == "/sync/outbox/events/reconcile")
                .count(),
            1
        );
    }

    #[test]
    fn delivery_batches_are_bounded_and_session_ordered() {
        let batches = delivery_batches(vec![
            delivery_record(1, "session-a"),
            delivery_record(2, "session-b"),
            delivery_record(3, "session-a"),
        ])
        .collect::<Vec<_>>();
        assert_eq!(batches.len(), 2);
        assert_eq!(
            batches[0]
                .records
                .iter()
                .map(|record| record.sequence)
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert!(batches.iter().all(|batch| {
            let session_id = &batch.records[0].session_id;
            batch
                .records
                .iter()
                .all(|record| &record.session_id == session_id)
        }));

        let oversized = (0..=MAX_SYNC_OUTBOX_BATCH_EVENTS as u64)
            .map(|sequence| delivery_record(sequence, "session-a"))
            .collect();
        let batches = delivery_batches(oversized).collect::<Vec<_>>();
        assert_eq!(
            batches
                .iter()
                .map(|batch| batch.records.len())
                .collect::<Vec<_>>(),
            vec![MAX_SYNC_OUTBOX_BATCH_EVENTS, 1]
        );
        assert!(
            batches[0]
                .records
                .windows(2)
                .all(|pair| pair[0].sequence < pair[1].sequence)
        );
    }

    #[test]
    fn delivery_batches_also_bound_encoded_body_bytes() {
        let records = (1..=3)
            .map(|sequence| {
                let mut record = delivery_record(sequence, "session-a");
                record.payload = serde_json::json!({"blob": "x".repeat(900)});
                record
            })
            .collect();
        let batches = delivery_batches_with_limit(records, 2_048).collect::<Vec<_>>();
        assert_eq!(batches.len(), 3);
        assert!(batches.iter().all(|batch| {
            serde_json::to_vec(&batch.request)
                .expect("batch JSON")
                .len()
                <= 2_048
        }));

        let mut oversized = delivery_record(4, "session-a");
        oversized.payload = serde_json::json!({"blob": "x".repeat(3_000)});
        let batches = delivery_batches_with_limit(vec![oversized], 2_048).collect::<Vec<_>>();
        assert_eq!(
            batches.len(),
            1,
            "an oversized singleton must not be dropped"
        );
        assert_eq!(batches[0].records.len(), 1);
    }

    #[tokio::test]
    async fn expired_drain_deadline_defers_without_requests() {
        use astra_thin_client::ThinClient;
        use wiremock::MockServer;

        let server = MockServer::start().await;
        let client = ThinClient::new(&server.uri(), None).expect("thin client");
        let records = vec![
            delivery_record(1, "session-a"),
            delivery_record(2, "session-b"),
        ];

        let settlements = deliver_sync_outbox_records_within(
            &client,
            "token",
            records.clone(),
            Instant::now() - Duration::from_millis(1),
        )
        .await;

        assert_eq!(settlements.len(), records.len());
        assert!(settlements.iter().all(|settlement| matches!(
            settlement,
            astra_services::SyncOutboxDeliverySettlement::Deferred { .. }
        )));
        assert!(
            server
                .received_requests()
                .await
                .expect("recorded requests")
                .is_empty(),
            "an expired drain deadline must not start more requests"
        );
    }

    #[tokio::test]
    async fn singleton_conflict_with_different_payload_is_terminal() {
        use astra_sync_protocol::SyncOutboxEventBatch;
        use astra_thin_client::ThinClient;
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let client = ThinClient::new(&server.uri(), None).expect("thin client");
        let record = delivery_record(1, "session-a");
        let request = SyncOutboxEventBatch::new(vec![event_body_from_outbox_record(&record)]);
        Mock::given(method("POST"))
            .and(path("/sync/outbox/events"))
            .and(body_json(serde_json::to_value(&request).unwrap()))
            .respond_with(sync_outbox_conflict_response(
                astra_sync_protocol::SYNC_OUTBOX_RECORD_CONFLICT_ERROR_CODE,
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/sync/outbox/events/reconcile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "acks": [{
                    "schema_version": 1,
                    "record_id": record.record_id,
                    "payload_hash": "sha256:someone-else",
                    "ingestion_status": "idempotent_replay"
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let settlements = deliver_sync_outbox_records(&client, "token", vec![record]).await;
        assert!(
            matches!(
                settlements.as_slice(),
                [astra_services::SyncOutboxDeliverySettlement::Failed { .. }]
            ),
            "a permanent payload conflict must consume the poison budget instead of deferring forever: {settlements:?}"
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn expired_deadline_defers_batch_without_reconciliation() {
        use astra_sync_protocol::SyncOutboxEventBatch;
        use astra_thin_client::ThinClient;
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer};

        let server = MockServer::start().await;
        let client = ThinClient::new(&server.uri(), None).expect("thin client");
        let records = vec![
            delivery_record(1, "session-a"),
            delivery_record(2, "session-a"),
        ];
        let request =
            SyncOutboxEventBatch::new(records.iter().map(event_body_from_outbox_record).collect());
        Mock::given(method("POST"))
            .and(path("/sync/outbox/events"))
            .and(body_json(serde_json::to_value(&request).unwrap()))
            .respond_with(sync_outbox_conflict_response(
                astra_sync_protocol::SYNC_OUTBOX_RECORD_CONFLICT_ERROR_CODE,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let batch = delivery_batches(records).next().expect("one batch");
        let mut settlements = Vec::new();
        deliver_sync_outbox_batch_records(
            &client,
            "token",
            batch,
            &mut settlements,
            Instant::now() - Duration::from_millis(1),
        )
        .await;

        assert_eq!(settlements.len(), 2);
        assert!(settlements.iter().all(|settlement| matches!(
            settlement,
            astra_services::SyncOutboxDeliverySettlement::Deferred { .. }
        )));
        assert_eq!(
            server
                .received_requests()
                .await
                .expect("recorded requests")
                .len(),
            1,
            "an expired drain deadline must not start reconciliation"
        );
    }

    #[tokio::test]
    async fn request_level_bad_request_defers_without_isolation() {
        use astra_sync_protocol::SyncOutboxEventBatch;
        use astra_thin_client::ThinClient;
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let client = ThinClient::new(&server.uri(), None).expect("thin client");
        let records = vec![
            delivery_record(1, "session-a"),
            delivery_record(2, "session-a"),
        ];
        let request =
            SyncOutboxEventBatch::new(records.iter().map(event_body_from_outbox_record).collect());
        Mock::given(method("POST"))
            .and(path("/sync/outbox/events"))
            .and(body_json(serde_json::to_value(&request).unwrap()))
            .respond_with(ResponseTemplate::new(400).set_body_string("unsupported batch schema"))
            .expect(1)
            .mount(&server)
            .await;
        mount_sync_outbox_reconciliation(&server, &records, &[false, false]).await;

        let batch = delivery_batches(records).next().expect("one batch");
        let mut settlements = Vec::new();
        deliver_sync_outbox_batch_records(
            &client,
            "token",
            batch,
            &mut settlements,
            Instant::now() + Duration::from_secs(60),
        )
        .await;

        assert_eq!(settlements.len(), 2);
        assert!(settlements.iter().all(|settlement| matches!(
            settlement,
            astra_services::SyncOutboxDeliverySettlement::Deferred { .. }
        )));
        assert_eq!(
            server
                .received_requests()
                .await
                .expect("recorded requests")
                .len(),
            2,
            "a request-level rejection must not be replayed per record"
        );
    }

    #[tokio::test]
    async fn batch_verdicts_settle_rejections_and_ack_siblings() {
        use astra_sync_protocol::{
            SyncOutboxAck, SyncOutboxBatchResponse, SyncOutboxEventBatch,
            SyncOutboxIngestionStatus, SyncOutboxRejection,
        };
        use astra_thin_client::ThinClient;
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let client = ThinClient::new(&server.uri(), None).expect("thin client");
        let records = vec![
            delivery_record(1, "session-a"),
            delivery_record(2, "session-a"),
            delivery_record(3, "session-a"),
        ];
        Mock::given(method("POST"))
            .and(path("/sync/outbox/events"))
            .and(body_json(
                serde_json::to_value(SyncOutboxEventBatch::new(
                    records.iter().map(event_body_from_outbox_record).collect(),
                ))
                .unwrap(),
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(SyncOutboxBatchResponse::new(
                    vec![
                        SyncOutboxAck::new(
                            &records[0].record_id,
                            &records[0].payload_hash,
                            SyncOutboxIngestionStatus::Created,
                        ),
                        SyncOutboxAck::new(
                            &records[2].record_id,
                            &records[2].payload_hash,
                            SyncOutboxIngestionStatus::Created,
                        ),
                    ],
                    vec![SyncOutboxRejection::new(
                        &records[1].record_id,
                        astra_sync_protocol::SYNC_OUTBOX_RECORD_CONFLICT_ERROR_CODE,
                        "stored payload differs",
                    )],
                )),
            )
            .expect(1)
            .mount(&server)
            .await;

        let settlements = deliver_sync_outbox_records(&client, "token", records).await;
        assert_eq!(settlements.len(), 3);
        for (index, settlement) in settlements.iter().enumerate() {
            match settlement {
                astra_services::SyncOutboxDeliverySettlement::Ack { record_id, .. } => {
                    assert_ne!(index, 1);
                    assert_eq!(record_id, &format!("record-{}", index + 1));
                }
                astra_services::SyncOutboxDeliverySettlement::Failed { record_id, .. } => {
                    assert_eq!(index, 1);
                    assert_eq!(record_id, "record-2");
                }
                astra_services::SyncOutboxDeliverySettlement::Deferred { .. } => {
                    panic!("a record verdict must not be deferred");
                }
            }
        }
        server.verify().await;
        assert_eq!(
            server
                .received_requests()
                .await
                .expect("recorded requests")
                .len(),
            1,
            "record verdicts must be settled from the single batch response"
        );
    }

    #[tokio::test]
    async fn server_failures_defer_without_record_isolation() {
        use astra_thin_client::ThinClient;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        for status in [429_u16, 500] {
            let server = MockServer::start().await;
            let client = ThinClient::new(&server.uri(), None).expect("thin client");
            let records = vec![
                delivery_record(1, "session-a"),
                delivery_record(2, "session-a"),
            ];
            Mock::given(method("POST"))
                .and(path("/sync/outbox/events"))
                .respond_with(ResponseTemplate::new(status))
                .expect(1)
                .mount(&server)
                .await;
            mount_sync_outbox_reconciliation(&server, &records, &[false, false]).await;

            let settlements = deliver_sync_outbox_records(&client, "token", records).await;
            assert_eq!(settlements.len(), 2);
            assert!(settlements.iter().all(|settlement| matches!(
                settlement,
                astra_services::SyncOutboxDeliverySettlement::Deferred { .. }
            )));
            server.verify().await;
            assert_one_batch_and_reconciliation_request(&server).await;
        }
    }

    #[tokio::test]
    async fn unknown_reconciliation_defers_record_rejection() {
        use astra_thin_client::ThinClient;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let client = ThinClient::new(&server.uri(), None).expect("thin client");
        let records = vec![
            delivery_record(1, "session-a"),
            delivery_record(2, "session-a"),
        ];
        Mock::given(method("POST"))
            .and(path("/sync/outbox/events"))
            .respond_with(ResponseTemplate::new(409))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/sync/outbox/events/reconcile"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;

        let settlements = deliver_sync_outbox_records(&client, "token", records).await;
        assert_eq!(settlements.len(), 2);
        assert!(settlements.iter().all(|settlement| matches!(
            settlement,
            astra_services::SyncOutboxDeliverySettlement::Deferred { .. }
        )));
        server.verify().await;
        assert_one_batch_and_reconciliation_request(&server).await;
    }

    #[tokio::test]
    async fn lost_batch_ack_reconciles_without_reposting_committed_records() {
        use astra_thin_client::ThinClient;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let client = ThinClient::new(&server.uri(), None).expect("thin client");
        let records = vec![
            delivery_record(1, "session-a"),
            delivery_record(2, "session-a"),
        ];
        Mock::given(method("POST"))
            .and(path("/sync/outbox/events"))
            .respond_with(ResponseTemplate::new(500).set_body_string("gateway lost the response"))
            .expect(1)
            .mount(&server)
            .await;
        mount_sync_outbox_reconciliation(&server, &records, &[true, true]).await;

        let settlements = deliver_sync_outbox_records(&client, "token", records.clone()).await;
        assert_eq!(settlements.len(), records.len());
        for (settlement, record) in settlements.iter().zip(&records) {
            match settlement {
                astra_services::SyncOutboxDeliverySettlement::Ack { record_id, .. } => {
                    assert_eq!(record_id, &record.record_id);
                }
                astra_services::SyncOutboxDeliverySettlement::Failed { error, .. }
                | astra_services::SyncOutboxDeliverySettlement::Deferred { error, .. } => {
                    panic!("committed record must reconcile after a lost ACK: {error}");
                }
            }
        }
        server.verify().await;
        assert_one_batch_and_reconciliation_request(&server).await;
    }

    #[tokio::test]
    async fn session_conflict_does_not_isolate_records() {
        use astra_sync_protocol::SyncOutboxEventBatch;
        use astra_thin_client::ThinClient;
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer};

        let server = MockServer::start().await;
        let client = ThinClient::new(&server.uri(), None).expect("thin client");
        let records = vec![
            delivery_record(1, "session-a"),
            delivery_record(2, "session-a"),
        ];
        let request =
            SyncOutboxEventBatch::new(records.iter().map(event_body_from_outbox_record).collect());
        Mock::given(method("POST"))
            .and(path("/sync/outbox/events"))
            .and(body_json(serde_json::to_value(request).unwrap()))
            .respond_with(sync_outbox_conflict_response(
                astra_sync_protocol::SYNC_OUTBOX_SESSION_UNAVAILABLE_ERROR_CODE,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let settlements = deliver_sync_outbox_records(&client, "token", records).await;
        assert_eq!(settlements.len(), 2);
        assert!(settlements.iter().all(|settlement| matches!(
            settlement,
            astra_services::SyncOutboxDeliverySettlement::Failed { .. }
        )));
        server.verify().await;
        assert_eq!(
            server
                .received_requests()
                .await
                .expect("recorded requests")
                .len(),
            1,
            "a deleted session must fail the batch without reconciliation"
        );
    }

    #[tokio::test]
    async fn oversized_singleton_is_failed_without_a_request() {
        use astra_sync_protocol::SyncOutboxEventBatch;
        use astra_thin_client::ThinClient;
        use wiremock::MockServer;

        let server = MockServer::start().await;
        let client = ThinClient::new(&server.uri(), None).expect("thin client");
        let mut record = delivery_record(1, "session-a");
        record.payload = serde_json::json!({
            "blob": "x".repeat(super::SYNC_OUTBOX_BATCH_BODY_LIMIT_BYTES),
        });
        assert!(
            serde_json::to_vec(&SyncOutboxEventBatch::new(vec![
                event_body_from_outbox_record(&record)
            ]))
            .unwrap()
            .len()
                > super::SYNC_OUTBOX_BATCH_BODY_LIMIT_BYTES
        );

        let settlements = deliver_sync_outbox_records(&client, "token", vec![record]).await;
        assert!(matches!(
            settlements.as_slice(),
            [astra_services::SyncOutboxDeliverySettlement::Failed { .. }]
        ));
        assert!(
            server
                .received_requests()
                .await
                .expect("recorded requests")
                .is_empty(),
            "an oversized record must fail locally instead of sending a doomed request"
        );
    }

    #[tokio::test]
    async fn singleton_lost_ack_reconciles_without_repeating_post() {
        use astra_sync_protocol::SyncOutboxEventBatch;
        use astra_thin_client::ThinClient;
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let client = ThinClient::new(&server.uri(), None).expect("thin client");
        let record = delivery_record(1, "session-a");
        let request = SyncOutboxEventBatch::new(vec![event_body_from_outbox_record(&record)]);
        Mock::given(method("POST"))
            .and(path("/sync/outbox/events"))
            .and(body_json(serde_json::to_value(request).unwrap()))
            .respond_with(ResponseTemplate::new(500).set_body_string("gateway lost the response"))
            .expect(1)
            .mount(&server)
            .await;
        mount_sync_outbox_reconciliation(&server, std::slice::from_ref(&record), &[true]).await;

        let settlements = deliver_sync_outbox_records(&client, "token", vec![record]).await;
        assert!(
            matches!(
                settlements.as_slice(),
                [astra_services::SyncOutboxDeliverySettlement::Ack { .. }]
            ),
            "a committed record must reconcile instead of being re-posted: {settlements:?}"
        );
        server.verify().await;
        assert_one_batch_and_reconciliation_request(&server).await;
    }

    #[test]
    fn journal_ingest_retry_budget_quarantines_and_success_resets() {
        let mut budget = JournalIngestRetryBudget::default();
        for _ in 1..SYNC_OUTBOX_JOURNAL_INGEST_MAX_CONSECUTIVE_FAILURES {
            assert!(matches!(
                budget.observe_failure("session-a"),
                JournalIngestFailureDisposition::RetryAfter(_)
            ));
        }
        assert_eq!(
            budget.observe_failure("session-a"),
            JournalIngestFailureDisposition::QuarantineUntilNewHint
        );
        assert!(matches!(
            budget.observe_failure("session-a"),
            JournalIngestFailureDisposition::RetryAfter(_)
        ));
        budget.observe_success("session-a");
        assert!(matches!(
            budget.observe_failure("session-a"),
            JournalIngestFailureDisposition::RetryAfter(_)
        ));
    }

    #[serial_test::serial]
    #[test]
    fn journal_ingest_overflow_is_bounded_and_escalates_to_one_recovery_scan() {
        super::recover_mutex_lock(&SYNC_OUTBOX_JOURNAL_INGEST_OVERFLOW)
            .sessions
            .clear();
        for index in 0..SYNC_OUTBOX_JOURNAL_INGEST_OVERFLOW_CAPACITY {
            assert_eq!(
                defer_journal_ingest_hint(format!("session-{index}")),
                JournalIngestScheduleOutcome::Coalesced
            );
        }
        assert_eq!(
            defer_journal_ingest_hint("session-over-capacity".to_string()),
            JournalIngestScheduleOutcome::RecoveryScanQueued
        );
        let mut overflow = super::recover_mutex_lock(&SYNC_OUTBOX_JOURNAL_INGEST_OVERFLOW);
        assert_eq!(
            overflow.sessions.len(),
            SYNC_OUTBOX_JOURNAL_INGEST_OVERFLOW_CAPACITY
        );
        assert!(overflow.reconcile_all);
        *overflow = Default::default();
    }

    #[test]
    fn cloud_pull_result_default_not_reachable() {
        let result = CloudPullResult {
            cloud_reachable: false,
        };
        assert!(!result.cloud_reachable);
    }

    #[serial_test::serial]
    #[test]
    fn drain_schedule_guard_releases_claim_on_drop() {
        release_sync_outbox_drain_schedule();
        let guard = try_claim_sync_outbox_drain_schedule().expect("first drain claim");
        drop(guard);
        assert!(try_claim_sync_outbox_drain_schedule().is_some());
        release_sync_outbox_drain_schedule();
    }

    #[serial_test::serial]
    #[test]
    fn stale_drain_guard_cannot_release_a_new_owner() {
        release_sync_outbox_drain_schedule();
        let stale = try_claim_sync_outbox_drain_schedule().expect("stale drain owner");
        release_sync_outbox_drain_schedule();
        let current = try_claim_sync_outbox_drain_schedule().expect("current drain owner");

        drop(stale);

        assert!(
            try_claim_sync_outbox_drain_schedule().is_none(),
            "a stale guard must not clear another worker's ownership"
        );
        drop(current);
        assert!(try_claim_sync_outbox_drain_schedule().is_some());
        release_sync_outbox_drain_schedule();
    }

    #[serial_test::serial]
    #[test]
    fn retry_wake_guard_releases_matching_deadline_on_drop() {
        release_sync_outbox_retry_wake_schedule();
        let deadline = 123_456;
        let guard = claim_sync_outbox_retry_wake_deadline(deadline).expect("first wake claim");
        drop(guard);
        assert!(claim_sync_outbox_retry_wake_deadline(deadline).is_some());
        release_sync_outbox_retry_wake_schedule();
    }

    #[serial_test::serial]
    #[test]
    fn stale_retry_wake_guard_cannot_clear_an_earlier_replacement() {
        release_sync_outbox_retry_wake_schedule();
        let stale = claim_sync_outbox_retry_wake_deadline(200).expect("later wake claim");
        let current = claim_sync_outbox_retry_wake_deadline(100).expect("earlier replacement");

        drop(stale);

        assert!(
            claim_sync_outbox_retry_wake_deadline(150).is_none(),
            "dropping the replaced owner must preserve the earlier wake"
        );
        drop(current);
        assert!(claim_sync_outbox_retry_wake_deadline(150).is_some());
        release_sync_outbox_retry_wake_schedule();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_outbox_boundary_keeps_runtime_responsive_during_lock_contention() {
        let temp = tempfile::tempdir().expect("tempdir");
        let lock_path = temp.path().join("outbox.lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .expect("open lock");
        lock.lock_exclusive().expect("hold outbox lock");
        let store = SyncOutboxStore::new(temp.path());
        let status_task = tokio::spawn(run_sync_outbox_io(store, |store| store.status()));

        tokio::time::timeout(Duration::from_millis(250), async {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        })
        .await
        .expect("local file-lock contention must not block the async worker");

        lock.unlock().expect("release outbox lock");
        status_task
            .await
            .expect("join blocking status task")
            .expect("status after lock release");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn journal_projection_establishes_durability_before_advancing_its_cursor() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = ProcessJournalDirGuard::new(temp.path());
        let session_id = format!("sync-projector-{}", uuid::Uuid::new_v4());
        let writer = JournalWriter::new(&session_id).expect("journal writer");
        writer
            .append_bulk_no_sync(&[JournalEvent::config_change(
                Some(&session_id),
                "model",
                "gpt-5",
            )])
            .expect("append unsynced canonical journal event");

        assert!(
            reconcile_sync_outbox_journal(&session_id)
                .await
                .expect("project journal")
        );
        let store = SyncOutboxStore::local();
        let first_offset = store
            .journal_source_offset(&session_id)
            .expect("source offset after first projection");
        assert!(first_offset > 0);
        assert_eq!(store.status().expect("outbox status").pending, 2);

        assert!(
            !reconcile_sync_outbox_journal(&session_id)
                .await
                .expect("empty source delta is a no-op")
        );

        writer
            .append_bulk_no_sync(&[JournalEvent::config_change(
                Some(&session_id),
                "mode",
                "auto",
            )])
            .expect("append second unsynced canonical event");
        assert!(
            reconcile_sync_outbox_journal(&session_id)
                .await
                .expect("project only newly appended journal bytes")
        );
        assert!(
            store
                .journal_source_offset(&session_id)
                .expect("source offset after second projection")
                > first_offset
        );
        assert_eq!(store.status().expect("outbox status").pending, 3);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn asynchronous_journal_ingest_hint_projects_without_blocking_the_submitter() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = ProcessJournalDirGuard::new(temp.path());
        let session_id = format!("sync-async-{}", uuid::Uuid::new_v4());
        let writer = JournalWriter::new(&session_id).expect("journal writer");
        writer
            .append(&JournalEvent::config_change(
                Some(&session_id),
                "model",
                "gpt-5",
            ))
            .expect("append canonical event");

        let scheduled_from_blocking_turn_worker = {
            let session_id = session_id.clone();
            tokio::task::spawn_blocking(move || schedule_sync_outbox_journal_ingestion(&session_id))
                .await
                .expect("blocking turn worker should finish")
        };
        assert!(scheduled_from_blocking_turn_worker.accepted());
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if SyncOutboxStore::local()
                    .status()
                    .is_ok_and(|status| status.pending == 2)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("background projector should flush the durable source delta");
    }

    #[test]
    fn sync_marker_logic() {
        let empty_pull = CloudPullResult {
            cloud_reachable: true,
        };
        assert!(!cloud_pull_warrants_sync_marker(&empty_pull, &[]));

        // With pref keys
        assert!(cloud_pull_warrants_sync_marker(
            &empty_pull,
            &["explain_mode".to_string()]
        ));
    }

    #[test]
    fn journal_append_decision() {
        let unreachable = CloudPullResult {
            cloud_reachable: false,
        };
        assert!(!should_append_cloud_pull_journal(
            &unreachable,
            &[],
            "startup"
        ));

        // post_login always journals if reachable
        let reachable_empty = CloudPullResult {
            cloud_reachable: true,
        };
        assert!(should_append_cloud_pull_journal(
            &reachable_empty,
            &[],
            "post_login"
        ));
    }

    #[test]
    fn settlement_report_counts_only_real_local_ack_as_acked() {
        let mut report = SyncOutboxDrainReport::default();

        record_settlement_result(
            &mut report,
            2,
            Ok(SyncOutboxSettlementReport {
                missing: 1,
                poisoned: 1,
                ..Default::default()
            }),
        );
        record_settlement_result(
            &mut report,
            1,
            Ok(SyncOutboxSettlementReport {
                acked: 1,
                ..Default::default()
            }),
        );

        assert_eq!(report.acked, 1);
        assert_eq!(report.failed, 0);
        assert_eq!(report.terminal, 2);
        assert!(report.is_incomplete());
        assert!(!report.can_retry_automatically());
    }

    #[serial_test::serial]
    #[test]
    fn drain_scheduler_allows_only_one_worker_until_released() {
        release_sync_outbox_drain_schedule();
        release_sync_outbox_retry_wake_schedule();

        let first_drain = try_claim_sync_outbox_drain_schedule().expect("first drain owner");
        assert!(try_claim_sync_outbox_drain_schedule().is_none());
        drop(first_drain);
        let second_drain = try_claim_sync_outbox_drain_schedule().expect("replacement drain owner");
        drop(second_drain);
        release_sync_outbox_drain_schedule();
        let later = claim_sync_outbox_retry_wake_deadline(200).expect("later wake");
        let earlier = claim_sync_outbox_retry_wake_deadline(100).expect("earlier wake");
        assert!(claim_sync_outbox_retry_wake_deadline(150).is_none());
        drop(later);
        drop(earlier);
        release_sync_outbox_retry_wake_schedule();
    }

    #[test]
    fn retry_delay_uses_ready_or_next_retry_after_without_user_action() {
        let mut status = SyncOutboxStatus {
            next_retry_after_unix_ms: Some(super::unix_ms().expect("time").saturating_add(10)),
            ..Default::default()
        };

        assert!(next_sync_outbox_drain_delay(&status).is_some());
        status.claimable = 1;
        assert_eq!(next_sync_outbox_drain_delay(&status), Some(Duration::ZERO));

        status.pending_high_watermark_exceeded = true;
        assert_eq!(
            next_sync_outbox_drain_delay(&status),
            Some(super::SYNC_OUTBOX_DEGRADED_DRAIN_COOLDOWN),
            "a huge ready backlog must make progress without a zero-delay CPU/IO loop"
        );
    }
}
