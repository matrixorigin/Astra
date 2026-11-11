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
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use futures_util::{FutureExt, StreamExt};
use tokio::sync::{broadcast, mpsc, watch};

use crate::cli::session::session_runtime;
use crate::cli::session::session_side_effects::{
    enqueue_ingestion_for_immediate_drain_pub, enqueue_ingestion_pub,
};
use crate::{ExplainMode, SessionState};

const SYNC_OUTBOX_DRAIN_LIMIT: usize = 64;
// One full server batch for each concurrent session, still bounded by bytes.
const SYNC_OUTBOX_BACKGROUND_DRAIN_LIMIT: usize =
    MAX_SYNC_OUTBOX_BATCH_EVENTS * SYNC_OUTBOX_MAX_CONCURRENT_SESSIONS;
/// Bound owned record clones retained for one drain, independently of request batches.
const SYNC_OUTBOX_DRAIN_SERIALIZED_BYTE_BUDGET: usize = 4 * 1024 * 1024;
const SYNC_OUTBOX_DRAIN_BACKGROUND_ROUNDS: usize = 4;
const SYNC_OUTBOX_RECORD_DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);
const SYNC_OUTBOX_SERVICE_RETRY_DELAY: Duration = Duration::from_secs(16);
/// Stops starting new work well before the 5 minute outbox in-flight lease so
/// the settlement write still fits inside the lease.
const SYNC_OUTBOX_DELIVERY_DEADLINE: Duration = Duration::from_secs(180);
const SYNC_OUTBOX_DEADLINE_ERROR: &str =
    "sync outbox drain deadline reached before delivery; record stays queued";
const SYNC_OUTBOX_OWNER_REPLACED_ERROR: &str = "cloud login changed during synchronization";
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
static SYNC_OUTBOX_CLOUD_ACCESS: LazyLock<
    watch::Sender<Option<crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot>>,
> = LazyLock::new(|| watch::channel(None).0);
// Notifications are hints only; the outbox remains the durable source of truth.
static SYNC_OUTBOX_ISSUE_EVENTS: LazyLock<broadcast::Sender<astra_services::OwnerScope>> =
    LazyLock::new(|| broadcast::channel(16).0);

pub(crate) fn subscribe_sync_issues() -> broadcast::Receiver<astra_services::OwnerScope> {
    SYNC_OUTBOX_ISSUE_EVENTS.subscribe()
}

pub(crate) fn notify_sync_issue(owner: &astra_services::OwnerScope) {
    let _ = SYNC_OUTBOX_ISSUE_EVENTS.send(owner.clone());
}

pub(crate) fn subscribe_sync_cloud_access()
-> watch::Receiver<Option<crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot>> {
    SYNC_OUTBOX_CLOUD_ACCESS.subscribe()
}

pub(crate) fn sync_cloud_access_is_blocked(
    snapshot: &crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot,
) -> bool {
    let blocked = SYNC_OUTBOX_CLOUD_ACCESS.borrow().as_ref() == Some(snapshot);
    if !blocked {
        SYNC_OUTBOX_CLOUD_ACCESS.send_if_modified(|current| {
            if current.as_ref().is_some_and(|old| !old.is_current()) {
                *current = None;
                true
            } else {
                false
            }
        });
    }
    blocked
}

pub(crate) fn block_sync_cloud_access(
    snapshot: &crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot,
) {
    SYNC_OUTBOX_CLOUD_ACCESS.send_if_modified(|blocked| {
        if !snapshot.is_current() || blocked.as_ref() == Some(snapshot) {
            return false;
        }
        *blocked = Some(snapshot.clone());
        true
    });
}

pub(crate) fn clear_sync_cloud_access() {
    // A new login must also wake workers using the previous credentials, even
    // if the old credentials had not yet received an access rejection.
    let _ = SYNC_OUTBOX_CLOUD_ACCESS.send_replace(None);
}

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
                let mut terminal = false;
                for (session_id, outcome) in batch_outcome.outcomes {
                    match outcome {
                        SyncOutboxJournalDeltaOutcome::Appended {
                            poisoned, skipped, ..
                        } => {
                            terminal |= poisoned > 0 || skipped > 0;
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
                if terminal {
                    notify_sync_issue(owner_scope);
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
    read_sync_outbox_status_for_owner(&astra_services::local_owner_scope()).await
}

pub(crate) async fn read_sync_outbox_status_for_owner(
    owner: &astra_services::OwnerScope,
) -> std::io::Result<SyncOutboxStatus> {
    run_sync_outbox_io(SyncOutboxStore::for_owner(owner)?, |store| store.status()).await
}

pub(crate) fn terminal_sync_items(status: &SyncOutboxStatus) -> u32 {
    status
        .poisoned
        .saturating_add(status.skipped)
        .min(u64::from(u32::MAX)) as u32
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
    pub blocker: Option<SyncOutboxDrainBlocker>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncOutboxDrainBlocker {
    MissingAccessToken,
    CloudAccessRequired,
    InvalidCloudBaseUrl,
    LocalOutboxRead,
    LocalSettlementWrite,
    LocalStatusRead,
}

impl SyncOutboxDrainBlocker {
    fn retry_delay(self) -> Option<Duration> {
        match self {
            Self::LocalOutboxRead | Self::LocalSettlementWrite | Self::LocalStatusRead => {
                Some(SYNC_OUTBOX_SERVICE_RETRY_DELAY)
            }
            Self::MissingAccessToken | Self::CloudAccessRequired | Self::InvalidCloudBaseUrl => {
                None
            }
        }
    }
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
            Some(SyncOutboxDrainBlocker::CloudAccessRequired) => {
                "cloud authentication or access was rejected".to_string()
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
                "{} item(s) will be retried and {} could not be synchronized",
                self.failed, self.terminal
            ),
            None if self.failed > 0 => format!(
                "{} queued record(s) could not yet be confirmed by the server",
                self.failed
            ),
            None => format!("{} item(s) could not be synchronized", self.terminal),
        };
        let recovery = if self.can_retry_automatically() && self.terminal > 0 {
            "Retryable items remain queued. Run `astra doctor` for details about the others."
        } else if self.can_retry_automatically() {
            "They remain queued for background retry."
        } else if matches!(
            self.blocker,
            Some(
                SyncOutboxDrainBlocker::MissingAccessToken
                    | SyncOutboxDrainBlocker::CloudAccessRequired
            )
        ) {
            "Check cloud access if needed, then use /login in this session to resume synchronization."
        } else if self.blocker == Some(SyncOutboxDrainBlocker::InvalidCloudBaseUrl) {
            "Check the configured server address."
        } else if self.blocker.is_some() {
            "Astra will retry. Run `astra doctor` if the problem continues."
        } else {
            "Run `astra doctor` for details."
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
    let auth_snapshot = crate::cli::cli_config::cli_utils::cli_owner_auth_snapshot();
    if sync_cloud_access_is_blocked(&auth_snapshot) {
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
    spawn_detached_tracked(&handle, async move {
        let mut blocker = None;
        for _ in 0..SYNC_OUTBOX_DRAIN_BACKGROUND_ROUNDS {
            if !auth_snapshot.is_current() {
                break;
            }
            let report = try_drain_sync_outbox_for_snapshot(
                SYNC_OUTBOX_BACKGROUND_DRAIN_LIMIT,
                &auth_snapshot,
            )
            .await;
            blocker = report.blocker;
            if blocker.is_some_and(|blocker| blocker.retry_delay().is_some()) {
                notify_sync_issue(&auth_snapshot.owner_scope);
            }
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
            if !auth_snapshot.is_current()
                || blocker.is_some()
                || !report.cloud_configured
                || report.remaining_ready == 0
                || report.attempted == 0
            {
                break;
            }
        }
        let next_delay = if !auth_snapshot.is_current() {
            None
        } else if let Some(blocker) = blocker {
            blocker.retry_delay()
        } else {
            match SyncOutboxStore::for_owner(&auth_snapshot.owner_scope) {
                Ok(store) => match run_sync_outbox_io(store, |store| store.status()).await {
                    Ok(status) => next_sync_outbox_drain_delay(&status),
                    Err(error) => {
                        tracing::warn!(target: "astra_cli::cloud_sync", ?error, "failed to read sync outbox status while scheduling the next drain");
                        Some(SYNC_OUTBOX_SERVICE_RETRY_DELAY)
                    }
                },
                Err(error) => {
                    tracing::warn!(target: "astra_cli::cloud_sync", ?error, "failed to resolve owner-scoped sync outbox while scheduling the next drain");
                    None
                }
            }
        };
        drop(schedule_guard);
        if !auth_snapshot.is_current() {
            // A newer login may have observed these records while they were
            // still claimed by this worker. Hand them back to its identity.
            schedule_sync_outbox_drain();
        } else if let Some(delay) = next_delay {
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
    if sync_cloud_access_is_blocked(auth_snapshot) {
        report.blocker = Some(SyncOutboxDrainBlocker::CloudAccessRequired);
        return report;
    }
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
            report.terminal = terminal_sync_items(&status);
            if report.terminal > 0 {
                notify_sync_issue(&auth_snapshot.owner_scope);
            }
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
    let token = sync_access_token(auth_snapshot).await;
    let Some(token) = token else {
        if auth_snapshot.is_current() {
            report.blocker = Some(SyncOutboxDrainBlocker::MissingAccessToken);
            tracing::debug!(
                target: "astra_cli::cloud_sync",
                "sync outbox drain skipped because no access token is available"
            );
        }
        return report;
    };
    if !auth_snapshot.is_current() {
        return report;
    }
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
    let (settlements, cloud_access_required, service_retry_after) =
        deliver_sync_outbox_records_within(
            &client,
            &token,
            records,
            Instant::now() + SYNC_OUTBOX_DELIVERY_DEADLINE,
            Some(auth_snapshot),
        )
        .await;
    if !settlements.is_empty() {
        let attempted_settlements = settlements.len();
        let settlement_result = run_sync_outbox_io(store.clone(), move |store| {
            store.settle_delivery_batch(&settlements, service_retry_after)
        })
        .await;
        let newly_terminal = settlement_result
            .as_ref()
            .is_ok_and(|outcome| outcome.poisoned > 0);
        let previously_terminal = report.terminal;
        record_settlement_result(&mut report, attempted_settlements, settlement_result);
        if newly_terminal && previously_terminal == 0 {
            notify_sync_issue(&auth_snapshot.owner_scope);
        }
    }
    if cloud_access_required {
        block_sync_cloud_access(auth_snapshot);
        report
            .blocker
            .get_or_insert(SyncOutboxDrainBlocker::CloudAccessRequired);
    }
    if !auth_snapshot.is_current() {
        return report;
    }
    match run_sync_outbox_io(store, |store| store.status()).await {
        Ok(status) => {
            report.remaining_ready = status.claimable.min(u64::from(u32::MAX)) as u32;
            report.terminal = report.terminal.max(terminal_sync_items(&status));
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

struct SyncOutboxRetryableFailure {
    error: String,
    retry_after: Option<Duration>,
    cloud_access_required: bool,
}

#[derive(Default)]
struct SyncOutboxDeliveryControl {
    access_blocked: AtomicBool,
    retry_delay_ms: AtomicU64,
    unresolved_sessions: AtomicUsize,
}

/// One HTTP batch attempt: per-record verdicts, a missing session, a size or
/// request rejection, or an unknown commit outcome.
enum SyncOutboxBatchAttempt {
    Verdicts(SyncOutboxBatchResponse),
    SessionUnavailable,
    PayloadTooLarge(String),
    RequestRetryable {
        failure: SyncOutboxRetryableFailure,
        service_wide: bool,
    },
    RequestFailed {
        error: String,
        pause_delivery: bool,
        retry_after: Option<Duration>,
    },
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
        Ok(Ok(response)) => SyncOutboxBatchAttempt::RequestFailed {
            error: format!(
                "cloud returned an unsupported sync batch response version: {}",
                response.schema_version
            ),
            pause_delivery: true,
            retry_after: None,
        },
        Ok(Err(astra_thin_client::ThinClientError::Api { status, body }))
            if status.as_u16() == 409
                && sync_outbox_error_code(&body).as_deref()
                    == Some(SYNC_OUTBOX_SESSION_UNAVAILABLE_ERROR_CODE) =>
        {
            SyncOutboxBatchAttempt::SessionUnavailable
        }
        Ok(Err(astra_thin_client::ThinClientError::Api { status, body }))
            if sync_outbox_cloud_access_required(status) =>
        {
            SyncOutboxBatchAttempt::RequestRetryable {
                failure: SyncOutboxRetryableFailure {
                    error: format!("cloud rejected sync batch with HTTP {status}: {body}"),
                    retry_after: Some(Duration::ZERO),
                    cloud_access_required: true,
                },
                service_wide: true,
            }
        }
        Ok(Err(astra_thin_client::ThinClientError::SyncOutboxBackpressure {
            status,
            body,
            retry_after_seconds,
        })) => {
            let error = format!("cloud rejected sync batch with HTTP {status}: {body}");
            let retry_after = retry_after_seconds.map(Duration::from_secs);
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                SyncOutboxBatchAttempt::RequestRetryable {
                    failure: SyncOutboxRetryableFailure {
                        error,
                        retry_after,
                        cloud_access_required: false,
                    },
                    service_wide: true,
                }
            } else {
                // A server error may follow a committed write, so confirm it first.
                SyncOutboxBatchAttempt::RequestFailed {
                    error,
                    retry_after,
                    pause_delivery: true,
                }
            }
        }
        Ok(Err(astra_thin_client::ThinClientError::Api { status, body }))
            if status == reqwest::StatusCode::PAYLOAD_TOO_LARGE =>
        {
            SyncOutboxBatchAttempt::PayloadTooLarge(format!(
                "cloud rejected sync batch with HTTP {status}: {body}"
            ))
        }
        // A request-level error is not an authoritative verdict on its records.
        // A route or deployment can be repaired without losing local work.
        Ok(Err(astra_thin_client::ThinClientError::Api { status, body }))
            if status.is_client_error() && status.as_u16() != 409 =>
        {
            SyncOutboxBatchAttempt::RequestRetryable {
                failure: SyncOutboxRetryableFailure {
                    error: format!("cloud rejected sync batch with HTTP {status}: {body}"),
                    retry_after: Some(SYNC_OUTBOX_SERVICE_RETRY_DELAY * 2),
                    cloud_access_required: false,
                },
                service_wide: false,
            }
        }
        Ok(Err(error)) => {
            let pause_delivery = error.is_transport()
                || matches!(&error,
                astra_thin_client::ThinClientError::Api { status, .. } if status.is_server_error());
            SyncOutboxBatchAttempt::RequestFailed {
                error: error.to_string(),
                pause_delivery,
                retry_after: None,
            }
        }
        Err(_) => SyncOutboxBatchAttempt::RequestFailed {
            error: format!(
                "cloud event batch delivery timed out after {}ms",
                SYNC_OUTBOX_RECORD_DELIVERY_TIMEOUT.as_millis()
            ),
            pause_delivery: true,
            retry_after: None,
        },
    }
}

fn sync_outbox_cloud_access_required(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    )
}

fn sync_outbox_error_code(body: &str) -> Option<String> {
    serde_json::from_str::<astra_core::ErrorResponse>(body)
        .ok()
        .and_then(|response| response.error_code)
}

#[cfg(test)]
async fn deliver_sync_outbox_records(
    client: &astra_thin_client::ThinClient,
    token: &str,
    records: Vec<SyncOutboxRecord>,
) -> (Vec<SyncOutboxDeliverySettlement>, bool) {
    let (settlements, access_required, _) = deliver_sync_outbox_records_within(
        client,
        token,
        records,
        Instant::now() + SYNC_OUTBOX_DELIVERY_DEADLINE,
        None,
    )
    .await;
    (settlements, access_required)
}

/// Delivers sessions concurrently within a fixed bound. Batches of one session
/// stay ordered, and no request starts after `deadline`, so a slow session
/// cannot make the whole drain outlive the outbox in-flight lease.
async fn deliver_sync_outbox_records_within(
    client: &astra_thin_client::ThinClient,
    token: &str,
    records: Vec<SyncOutboxRecord>,
    deadline: Instant,
    auth_snapshot: Option<&crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot>,
) -> (Vec<SyncOutboxDeliverySettlement>, bool, Option<Duration>) {
    let control = SyncOutboxDeliveryControl::default();
    let outcomes = futures_util::stream::iter(
        sync_outbox_session_batches(records, SYNC_OUTBOX_BATCH_BODY_LIMIT_BYTES).map(|batches| {
            let control = &control;
            async move {
                if auth_snapshot.is_some_and(|snapshot| !snapshot.is_current()) {
                    return (
                        deferred_settlements_with_policy(
                            batches.into_remaining(),
                            SYNC_OUTBOX_OWNER_REPLACED_ERROR,
                            Some(Duration::ZERO),
                        )
                        .collect(),
                        false,
                    );
                }
                let retry_ms = control.retry_delay_ms.load(Ordering::Acquire);
                if control.access_blocked.load(Ordering::Acquire) || retry_ms > 0 {
                    let access_required = control.access_blocked.load(Ordering::Acquire);
                    return (
                        deferred_settlements_with_policy(
                            batches.into_remaining(),
                            if access_required {
                                "cloud access was rejected"
                            } else {
                                "cloud requested a delivery pause"
                            },
                            Some(Duration::from_millis(retry_ms)),
                        )
                        .collect(),
                        access_required,
                    );
                }
                deliver_sync_outbox_session(
                    client,
                    token,
                    batches,
                    deadline,
                    auth_snapshot,
                    control,
                )
                .await
            }
        }),
    )
    .buffer_unordered(SYNC_OUTBOX_MAX_CONCURRENT_SESSIONS)
    .collect::<Vec<_>>()
    .await;
    let mut settlements = Vec::new();
    let mut cloud_access_required = false;
    for (batch_settlements, blocked) in outcomes {
        settlements.extend(batch_settlements);
        cloud_access_required |= blocked;
    }
    let retry_ms = control.retry_delay_ms.load(Ordering::Acquire);
    (
        settlements,
        cloud_access_required,
        (retry_ms > 0).then(|| Duration::from_millis(retry_ms)),
    )
}

fn pause_service_delivery(retry_delay_ms: &AtomicU64, delay: Option<Duration>) {
    let millis = delay
        .unwrap_or(SYNC_OUTBOX_SERVICE_RETRY_DELAY)
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    retry_delay_ms.fetch_max(millis.max(1), Ordering::AcqRel);
}

async fn sync_access_token(
    snapshot: &crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot,
) -> Option<String> {
    tokio::select! {
        biased;
        token = async {
            if let Some(binding) = &snapshot.native_binding {
                binding.access_token().await.ok()
            } else {
                snapshot.access_token().await
            }
        } => token,
        () = wait_for_sync_owner_replacement(Some(snapshot)) => None,
    }
}

async fn wait_for_sync_owner_replacement(
    snapshot: Option<&crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot>,
) {
    let Some(snapshot) = snapshot else {
        std::future::pending::<()>().await;
        return;
    };
    let mut changes = subscribe_sync_cloud_access();
    while snapshot.is_current() {
        if changes.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

async fn deliver_sync_outbox_session(
    client: &astra_thin_client::ThinClient,
    token: &str,
    mut batches: SyncOutboxSessionBatches,
    deadline: Instant,
    auth_snapshot: Option<&crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot>,
    control: &SyncOutboxDeliveryControl,
) -> (Vec<SyncOutboxDeliverySettlement>, bool) {
    let mut settlements = Vec::new();
    let mut remaining_error = SYNC_OUTBOX_DEADLINE_ERROR;
    let mut cloud_access_required = false;
    let mut owner_replaced = false;
    let owner_replacement = wait_for_sync_owner_replacement(auth_snapshot);
    tokio::pin!(owner_replacement);
    for batch in batches.by_ref() {
        let retry_ms = control.retry_delay_ms.load(Ordering::Acquire);
        if control.access_blocked.load(Ordering::Acquire) || retry_ms > 0 {
            cloud_access_required = control.access_blocked.load(Ordering::Acquire);
            settlements.extend(deferred_settlements_with_policy(
                batch.records.into_iter(),
                "cloud delivery is paused",
                Some(Duration::from_millis(retry_ms)),
            ));
            remaining_error = "cloud delivery is paused";
            break;
        }
        if auth_snapshot.is_some_and(|snapshot| !snapshot.is_current()) {
            settlements.extend(deferred_settlements_with_policy(
                batch.records.into_iter(),
                SYNC_OUTBOX_OWNER_REPLACED_ERROR,
                Some(Duration::ZERO),
            ));
            owner_replaced = true;
            remaining_error = SYNC_OUTBOX_OWNER_REPLACED_ERROR;
            break;
        }
        if Instant::now() >= deadline {
            settlements.extend(deferred_settlements(
                batch.records.into_iter(),
                SYNC_OUTBOX_DEADLINE_ERROR,
            ));
            break;
        }
        let previous_len = settlements.len();
        let batch_ids = batch
            .records
            .iter()
            .map(|record| record.record_id.clone())
            .collect::<Vec<_>>();
        cloud_access_required = tokio::select! {
            biased;
            blocked = deliver_sync_outbox_batch_records(client, token, batch, &mut settlements, deadline, control) => blocked,
            () = &mut owner_replacement => {
                settlements.extend(batch_ids.into_iter().map(|record_id| SyncOutboxDeliverySettlement::Deferred {
                    record_id,
                    error: SYNC_OUTBOX_OWNER_REPLACED_ERROR.to_string(),
                    retry_after: Some(Duration::ZERO),
                }));
                owner_replaced = true;
                remaining_error = SYNC_OUTBOX_OWNER_REPLACED_ERROR;
                break;
            }
        };
        if settlements[previous_len..]
            .iter()
            .any(|result| matches!(result, SyncOutboxDeliverySettlement::Deferred { .. }))
        {
            remaining_error = "an earlier batch for this session is still pending";
            break;
        }
    }
    settlements.extend(deferred_settlements_with_policy(
        batches.into_remaining(),
        remaining_error,
        if owner_replaced || cloud_access_required {
            Some(Duration::ZERO)
        } else {
            let retry_ms = control.retry_delay_ms.load(Ordering::Acquire);
            (retry_ms > 0).then(|| Duration::from_millis(retry_ms))
        },
    ));
    (settlements, cloud_access_required)
}

/// Settles one batch. Only unknown commit outcomes need reconciliation; request
/// rejections preserve records, and size rejections split the batch in order.
async fn deliver_sync_outbox_batch_records(
    client: &astra_thin_client::ThinClient,
    token: &str,
    SyncOutboxDeliveryBatch { records, request }: SyncOutboxDeliveryBatch,
    settlements: &mut Vec<SyncOutboxDeliverySettlement>,
    deadline: Instant,
    control: &SyncOutboxDeliveryControl,
) -> bool {
    let mut pending = VecDeque::from([SyncOutboxDeliveryBatch { records, request }]);
    while let Some(SyncOutboxDeliveryBatch { records, request }) = pending.pop_front() {
        let retry_ms = control.retry_delay_ms.load(Ordering::Acquire);
        if control.access_blocked.load(Ordering::Acquire) || retry_ms > 0 {
            let access_required = control.access_blocked.load(Ordering::Acquire);
            settlements.extend(deferred_settlements_with_policy(
                records.into_iter(),
                "cloud delivery is paused",
                Some(Duration::from_millis(retry_ms)),
            ));
            for batch in pending {
                settlements.extend(deferred_settlements_with_policy(
                    batch.records.into_iter(),
                    "cloud delivery is paused",
                    Some(Duration::from_millis(retry_ms)),
                ));
            }
            return access_required;
        }
        if Instant::now() >= deadline {
            settlements.extend(deferred_settlements(
                records.into_iter(),
                SYNC_OUTBOX_DEADLINE_ERROR,
            ));
            break;
        }
        // The builder admits an oversized singleton so the record is never lost.
        // It can never fit the server body limit, so settle it here instead of
        // sending a request that is guaranteed to fail.
        if records.len() == 1
            && serde_json::to_vec(&request)
                .is_ok_and(|body| body.len() > SYNC_OUTBOX_BATCH_BODY_LIMIT_BYTES)
        {
            settlements.push(SyncOutboxDeliverySettlement::Rejected {
            record_id: records[0].record_id.clone(),
            error: format!(
                "record exceeds the {SYNC_OUTBOX_BATCH_BODY_LIMIT_BYTES} byte sync batch body limit"
            ),
        });
            continue;
        }
        let previous_len = settlements.len();
        match attempt_sync_outbox_batch(client, token, &request).await {
            SyncOutboxBatchAttempt::Verdicts(response) => {
                settle_sync_outbox_verdicts(records, response, settlements);
            }
            SyncOutboxBatchAttempt::SessionUnavailable => {
                settlements.extend(records.into_iter().map(|record| {
                    SyncOutboxDeliverySettlement::Rejected {
                        record_id: record.record_id,
                        error: "cloud session is unavailable for event ingestion".to_string(),
                    }
                }));
            }
            SyncOutboxBatchAttempt::PayloadTooLarge(error) => {
                if records.len() == 1 {
                    settlements.extend(deferred_settlements(records.into_iter(), &error));
                    break;
                }
                let mid = records.len() / 2;
                let mut left_records = records;
                let right_records = left_records.split_off(mid);
                let mut left_events = request.events;
                let right_events = left_events.split_off(mid);
                pending.push_front(SyncOutboxDeliveryBatch {
                    records: right_records,
                    request: SyncOutboxEventBatch::new(right_events),
                });
                pending.push_front(SyncOutboxDeliveryBatch {
                    records: left_records,
                    request: SyncOutboxEventBatch::new(left_events),
                });
            }
            SyncOutboxBatchAttempt::RequestRetryable {
                failure,
                service_wide,
            } => {
                if failure.cloud_access_required {
                    control.access_blocked.store(true, Ordering::Release);
                } else if service_wide {
                    pause_service_delivery(&control.retry_delay_ms, failure.retry_after);
                } else if control.unresolved_sessions.fetch_add(1, Ordering::AcqRel) + 1
                    >= SYNC_OUTBOX_MAX_CONCURRENT_SESSIONS
                {
                    pause_service_delivery(&control.retry_delay_ms, None);
                }
                settlements.extend(deferred_settlements_with_policy(
                    records.into_iter(),
                    &failure.error,
                    failure.retry_after,
                ));
                for batch in pending {
                    settlements.extend(deferred_settlements_with_policy(
                        batch.records.into_iter(),
                        &failure.error,
                        failure.retry_after,
                    ));
                }
                return failure.cloud_access_required;
            }
            SyncOutboxBatchAttempt::RequestFailed {
                error,
                pause_delivery,
                retry_after,
            } => {
                if Instant::now() >= deadline {
                    settlements.extend(deferred_settlements(
                        records.into_iter(),
                        SYNC_OUTBOX_DEADLINE_ERROR,
                    ));
                    if pause_delivery {
                        pause_service_delivery(&control.retry_delay_ms, retry_after);
                    }
                    break;
                }
                let previous_len = settlements.len();
                let access = settle_sync_outbox_reconciliation(
                    client,
                    token,
                    records,
                    error,
                    settlements,
                    if pause_delivery {
                        Some(SYNC_OUTBOX_SERVICE_RETRY_DELAY * 2)
                    } else {
                        None
                    },
                    control,
                )
                .await;
                let unresolved = settlements[previous_len..]
                    .iter()
                    .any(|result| matches!(result, SyncOutboxDeliverySettlement::Deferred { .. }));
                // A lost response that reconciles successfully is not an outage.
                if pause_delivery && retry_after.is_some() {
                    pause_service_delivery(&control.retry_delay_ms, retry_after);
                } else if pause_delivery
                    && unresolved
                    && !access
                    && control.unresolved_sessions.fetch_add(1, Ordering::AcqRel) + 1
                        >= SYNC_OUTBOX_MAX_CONCURRENT_SESSIONS
                {
                    pause_service_delivery(&control.retry_delay_ms, None);
                }
                if access || unresolved {
                    for batch in pending {
                        settlements.extend(deferred_settlements(
                            batch.records.into_iter(),
                            "an earlier batch for this session is still pending",
                        ));
                    }
                    return access;
                }
            }
        }
        if settlements[previous_len..]
            .iter()
            .any(|result| matches!(result, SyncOutboxDeliverySettlement::Deferred { .. }))
        {
            break;
        }
    }
    for batch in pending {
        settlements.extend(deferred_settlements(
            batch.records.into_iter(),
            "an earlier batch for this session is still pending",
        ));
    }
    false
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
                settlements.push(SyncOutboxDeliverySettlement::Rejected {
                    record_id: record.record_id,
                    error: "cloud acknowledged a different payload for this record".to_string(),
                });
            }
        } else if let Some(rejection) = rejections.get(&record.record_id) {
            settlements.push(SyncOutboxDeliverySettlement::Rejected {
                record_id: record.record_id,
                error: format!("{}: {}", rejection.error_code, rejection.detail),
            });
        } else {
            settlements.push(SyncOutboxDeliverySettlement::Deferred {
                record_id: record.record_id,
                error: "cloud accepted the batch without a verdict for this record".to_string(),
                retry_after: None,
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
    unresolved_retry_after: Option<Duration>,
    control: &SyncOutboxDeliveryControl,
) -> bool {
    let reconciliations = match reconcile_sync_outbox_deliveries(client, token, &records).await {
        Ok(reconciliations) => reconciliations,
        Err(failure) => {
            if failure.cloud_access_required {
                control.access_blocked.store(true, Ordering::Release);
            } else {
                pause_service_delivery(&control.retry_delay_ms, failure.retry_after);
            }
            let error = format!("{delivery_error}; {}", failure.error);
            settlements.extend(deferred_settlements_with_policy(
                records.into_iter(),
                &error,
                failure.retry_after,
            ));
            return failure.cloud_access_required;
        }
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
                settlements.push(SyncOutboxDeliverySettlement::Rejected {
                    record_id: record.record_id,
                    error,
                });
            }
            SyncOutboxReconciliation::Missing => {
                settlements.push(SyncOutboxDeliverySettlement::Deferred {
                    record_id: record.record_id,
                    error: delivery_error.clone(),
                    retry_after: unresolved_retry_after,
                });
            }
        }
    }
    false
}

/// Records skipped for time remain pending for a later drain.
fn deferred_settlements(
    records: impl Iterator<Item = SyncOutboxRecord>,
    error: &str,
) -> impl Iterator<Item = SyncOutboxDeliverySettlement> {
    deferred_settlements_with_policy(records, error, None)
}

fn deferred_settlements_with_policy(
    records: impl Iterator<Item = SyncOutboxRecord>,
    error: &str,
    retry_after: Option<Duration>,
) -> impl Iterator<Item = SyncOutboxDeliverySettlement> {
    let error = error.to_string();
    records.map(move |record| SyncOutboxDeliverySettlement::Deferred {
        record_id: record.record_id,
        error: error.clone(),
        retry_after,
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
    json!({
        "event_id": record.record_id,
        "session_id": record.session_id,
        "event_type": record.event_type,
        "content": record.canonical_payload_json(),
        "agent_id": "edge_sync",
        "agent_version": env!("CARGO_PKG_VERSION"),
        "metadata": {
            "sync_outbox": {
                "schema_version": record.schema_version,
                "record_id": record.record_id,
                "sequence": record.sequence,
                "payload_hash": record.payload_hash,
                "event_ts": record.event_ts,
            }
        },
    })
}

/// Read-only reconciliation for a batch whose commit outcome is unknown.
/// An error means the response cannot be trusted; callers must defer, using the
/// server-directed delay when one was provided.
async fn reconcile_sync_outbox_deliveries(
    client: &astra_thin_client::ThinClient,
    token: &str,
    records: &[SyncOutboxRecord],
) -> Result<Vec<SyncOutboxReconciliation>, SyncOutboxRetryableFailure> {
    if records.is_empty() {
        return Ok(Vec::new());
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
        Ok(Err(astra_thin_client::ThinClientError::Api { status, body }))
            if sync_outbox_cloud_access_required(status) =>
        {
            return Err(SyncOutboxRetryableFailure {
                error: format!("cloud rejected sync reconciliation with HTTP {status}: {body}"),
                retry_after: Some(Duration::ZERO),
                cloud_access_required: true,
            });
        }
        Ok(Err(astra_thin_client::ThinClientError::SyncOutboxBackpressure {
            status,
            body,
            retry_after_seconds,
        })) => {
            return Err(SyncOutboxRetryableFailure {
                error: format!("cloud rejected sync reconciliation with HTTP {status}: {body}"),
                retry_after: retry_after_seconds.map(Duration::from_secs),
                cloud_access_required: false,
            });
        }
        Ok(Err(error)) => {
            return Err(SyncOutboxRetryableFailure {
                error: format!("cloud reconciliation failed: {error}"),
                retry_after: None,
                cloud_access_required: false,
            });
        }
        Err(_) => {
            return Err(SyncOutboxRetryableFailure {
                error: "cloud reconciliation timed out".to_string(),
                retry_after: None,
                cloud_access_required: false,
            });
        }
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
        return Err(SyncOutboxRetryableFailure {
            error: "cloud returned an uncorrelated sync reconciliation response".to_string(),
            retry_after: None,
            cloud_access_required: false,
        });
    }
    Ok(records
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
        .collect())
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
    clear_sync_cloud_access();
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
    if report.blocker.is_none() {
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
        cloud_pull_warrants_sync_marker, defer_journal_ingest_hint, deliver_sync_outbox_records,
        deliver_sync_outbox_records_within, deliver_sync_outbox_session,
        event_body_from_outbox_record, next_sync_outbox_drain_delay, reconcile_sync_outbox_journal,
        record_settlement_result, release_sync_outbox_drain_schedule,
        release_sync_outbox_retry_wake_schedule, run_sync_outbox_io,
        schedule_sync_outbox_journal_ingestion, should_append_cloud_pull_journal,
        sync_outbox_session_batches, try_claim_sync_outbox_drain_schedule,
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
        SyncDomain, SyncOutboxDeliverySettlement, SyncOutboxRecord, SyncOutboxRecordState,
        SyncOutboxSettlementReport, SyncOutboxStatus, SyncOutboxStore,
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
            next_retry_after_unix_ms: None,
            last_error: None,
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

    #[test]
    fn sync_notice_describes_available_recovery() {
        let retry = SyncOutboxDrainReport {
            failed: 1,
            ..Default::default()
        };
        assert!(retry.user_notice().unwrap().contains("background retry"));
        let terminal = SyncOutboxDrainReport {
            terminal: 1,
            ..Default::default()
        };
        let notice = terminal.user_notice().unwrap();
        assert!(notice.contains("astra doctor"));
        assert!(!notice.contains("/sync"));
        let missing_auth = SyncOutboxDrainReport {
            blocker: Some(super::SyncOutboxDrainBlocker::MissingAccessToken),
            ..Default::default()
        };
        assert!(missing_auth.user_notice().unwrap().contains("/login"));
        for blocker in [
            super::SyncOutboxDrainBlocker::LocalOutboxRead,
            super::SyncOutboxDrainBlocker::LocalSettlementWrite,
            super::SyncOutboxDrainBlocker::LocalStatusRead,
        ] {
            assert!(blocker.retry_delay().is_some());
            assert!(
                SyncOutboxDrainReport {
                    blocker: Some(blocker),
                    ..Default::default()
                }
                .user_notice()
                .unwrap()
                .contains("retry")
            );
        }
    }

    #[tokio::test]
    async fn slow_session_does_not_block_other_sessions_from_starting() {
        use astra_sync_protocol::{
            SyncOutboxAck, SyncOutboxBatchResponse, SyncOutboxIngestionStatus,
        };
        use astra_thin_client::ThinClient;
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let client = ThinClient::new(&server.uri(), None).unwrap();
        let records = (0..9)
            .map(|index| delivery_record(index, &format!("session-{index:02}")))
            .collect::<Vec<_>>();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let started_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(started_tx)));
        for (index, record) in records.iter().enumerate() {
            let request = astra_sync_protocol::SyncOutboxEventBatch::new(vec![
                event_body_from_outbox_record(record),
            ]);
            let response = ResponseTemplate::new(200).set_body_json(SyncOutboxBatchResponse::new(
                vec![SyncOutboxAck::new(
                    &record.record_id,
                    &record.payload_hash,
                    SyncOutboxIngestionStatus::Created,
                )],
                Vec::new(),
            ));
            let mock = Mock::given(method("POST"))
                .and(path("/sync/outbox/events"))
                .and(body_json(serde_json::to_value(request).unwrap()));
            if index == 0 {
                mock.respond_with(response.set_delay(Duration::from_secs(5)))
                    .mount(&server)
                    .await;
            } else if index == 8 {
                let started_tx = std::sync::Arc::clone(&started_tx);
                mock.respond_with(move |_: &wiremock::Request| {
                    if let Some(sender) = started_tx.lock().unwrap().take() {
                        let _ = sender.send(());
                    }
                    response.clone()
                })
                .mount(&server)
                .await;
            } else {
                mock.respond_with(response).mount(&server).await;
            }
        }
        let delivery =
            tokio::spawn(
                async move { deliver_sync_outbox_records(&client, "token", records).await },
            );
        let ninth_started = tokio::time::timeout(Duration::from_secs(2), started_rx).await;
        delivery.abort();
        let _ = delivery.await;
        assert!(matches!(ninth_started, Ok(Ok(()))));
    }

    #[tokio::test]
    async fn unresolved_batch_defers_later_batches_for_the_same_session() {
        use astra_thin_client::ThinClient;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let client = ThinClient::new(&server.uri(), None).unwrap();
        let records = (1..=3)
            .map(|sequence| {
                let mut record = delivery_record(sequence, "session-a");
                record.payload = serde_json::json!({"blob": "x".repeat(900)});
                record
            })
            .collect::<Vec<_>>();
        Mock::given(method("POST"))
            .and(path("/sync/outbox/events"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;
        mount_sync_outbox_reconciliation(&server, &records[..1], &[false]).await;
        let batches = sync_outbox_session_batches(records, 2_048).next().unwrap();
        let (settlements, _) = deliver_sync_outbox_session(
            &client,
            "token",
            batches,
            Instant::now() + Duration::from_secs(10),
            None,
            &super::SyncOutboxDeliveryControl::default(),
        )
        .await;
        assert_eq!(settlements.len(), 3);
        assert!(settlements.iter().all(|settlement| matches!(
            settlement,
            astra_services::SyncOutboxDeliverySettlement::Deferred { .. }
        )));
        server.verify().await;
        assert_one_batch_and_reconciliation_request(&server).await;
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

        let (settlements, _, _) = deliver_sync_outbox_records_within(
            &client,
            "token",
            records.clone(),
            Instant::now() - Duration::from_millis(1),
            None,
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
                    "record_id": record.record_id,
                    "payload_hash": "sha256:someone-else",
                    "ingestion_status": "idempotent_replay"
                }],
                "rejections": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        let (settlements, _) = deliver_sync_outbox_records(&client, "token", vec![record]).await;
        assert!(
            matches!(
                settlements.as_slice(),
                [astra_services::SyncOutboxDeliverySettlement::Rejected { .. }]
            ),
            "a permanent payload conflict must be rejected instead of deferred: {settlements:?}"
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn request_level_rejection_preserves_records_and_session_order() {
        use astra_thin_client::ThinClient;
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let client = ThinClient::new(&server.uri(), None).expect("thin client");
        let records = (1..=MAX_SYNC_OUTBOX_BATCH_EVENTS + 1)
            .map(|sequence| delivery_record(sequence as u64, "session-a"))
            .collect::<Vec<_>>();
        let batches = delivery_batches(records.clone()).collect::<Vec<_>>();
        assert_eq!(batches.len(), 2);
        Mock::given(method("POST"))
            .and(path("/sync/outbox/events"))
            .and(body_json(
                serde_json::to_value(&batches[0].request).unwrap(),
            ))
            .respond_with(ResponseTemplate::new(400).set_body_string("unsupported batch schema"))
            .expect(1)
            .mount(&server)
            .await;
        let (settlements, _) = deliver_sync_outbox_records(&client, "token", records).await;
        let first_batch_len = batches[0].records.len();
        assert!(settlements[..first_batch_len].iter().all(|settlement| {
            matches!(
                settlement,
                astra_services::SyncOutboxDeliverySettlement::Deferred { error, .. }
                    if error.contains("HTTP 400")
            )
        }));
        assert!(matches!(
            &settlements[first_batch_len],
            astra_services::SyncOutboxDeliverySettlement::Deferred { .. }
        ));
        server.verify().await;
        assert!(
            server
                .received_requests()
                .await
                .expect("recorded requests")
                .iter()
                .all(|request| request.url.path() == "/sync/outbox/events"),
            "a request-level rejection must not trigger reconciliation or send later records"
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

        let (settlements, _) = deliver_sync_outbox_records(&client, "token", records).await;
        assert_eq!(settlements.len(), 3);
        for (index, settlement) in settlements.iter().enumerate() {
            match settlement {
                astra_services::SyncOutboxDeliverySettlement::Ack { record_id, .. } => {
                    assert_ne!(index, 1);
                    assert_eq!(record_id, &format!("record-{}", index + 1));
                }
                astra_services::SyncOutboxDeliverySettlement::Rejected { record_id, .. } => {
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

        let server = MockServer::start().await;
        let client = ThinClient::new(&server.uri(), None).expect("thin client");
        let records = vec![
            delivery_record(1, "session-a"),
            delivery_record(2, "session-a"),
        ];
        Mock::given(method("POST"))
            .and(path("/sync/outbox/events"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;
        mount_sync_outbox_reconciliation(&server, &records, &[false, false]).await;

        let (settlements, _) = deliver_sync_outbox_records(&client, "token", records).await;
        assert_eq!(settlements.len(), 2);
        assert!(settlements.iter().all(|settlement| matches!(
            settlement,
            astra_services::SyncOutboxDeliverySettlement::Deferred { .. }
        )));
        server.verify().await;
        assert_one_batch_and_reconciliation_request(&server).await;
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

        let (settlements, _) = deliver_sync_outbox_records(&client, "token", records).await;
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

        let (settlements, _) = deliver_sync_outbox_records(&client, "token", records.clone()).await;
        assert_eq!(settlements.len(), records.len());
        for (settlement, record) in settlements.iter().zip(&records) {
            match settlement {
                astra_services::SyncOutboxDeliverySettlement::Ack { record_id, .. } => {
                    assert_eq!(record_id, &record.record_id);
                }
                astra_services::SyncOutboxDeliverySettlement::Rejected { error, .. }
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

        let (settlements, _) = deliver_sync_outbox_records(&client, "token", records).await;
        assert_eq!(settlements.len(), 2);
        assert!(settlements.iter().all(|settlement| matches!(
            settlement,
            astra_services::SyncOutboxDeliverySettlement::Rejected { .. }
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

    #[test]
    fn event_body_keeps_journal_metadata_only_in_content() {
        use astra_sync_protocol::SyncOutboxEventBatch;

        let mut record = delivery_record(1, "session-a");
        record.payload = serde_json::json!({
            "metadata": {"large": "x".repeat(2_200_000)},
        });
        let event = event_body_from_outbox_record(&record);
        assert_eq!(event["metadata"].as_object().unwrap().len(), 1);
        assert!(event["metadata"].get("large").is_none());
        let content: serde_json::Value =
            serde_json::from_str(event["content"].as_str().unwrap()).unwrap();
        assert_eq!(
            content["metadata"]["large"].as_str().unwrap().len(),
            2_200_000
        );

        let batch = SyncOutboxEventBatch::new(vec![event]);
        assert!(
            serde_json::to_vec(&batch).unwrap().len() < super::SYNC_OUTBOX_BATCH_BODY_LIMIT_BYTES,
            "metadata should not be duplicated into the outer request envelope"
        );
    }

    #[tokio::test]
    async fn retryable_client_errors_defer_without_reconciliation() {
        use astra_thin_client::ThinClient;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        for status in [401, 402, 403, 407, 408, 423, 425, 429] {
            let server = MockServer::start().await;
            let client = ThinClient::new(&server.uri(), None).expect("thin client");
            let expected_retry_after = match status {
                401 | 403 => Some(Duration::ZERO),
                429 => Some(Duration::from_secs(3_600)),
                _ => Some(super::SYNC_OUTBOX_SERVICE_RETRY_DELAY * 2),
            };
            let mut response = ResponseTemplate::new(status).set_body_string("retry later");
            if status == 429 {
                response = response.insert_header("retry-after", "3600");
            }
            Mock::given(method("POST"))
                .and(path("/sync/outbox/events"))
                .respond_with(response)
                .expect(1)
                .mount(&server)
                .await;

            let record = delivery_record(u64::from(status), &format!("session-{status}"));
            let (settlements, blocked) =
                deliver_sync_outbox_records(&client, "token", vec![record]).await;
            assert_eq!(blocked, matches!(status, 401 | 403));
            assert!(matches!(
                settlements.as_slice(),
                [astra_services::SyncOutboxDeliverySettlement::Deferred {
                    error,
                    retry_after,
                    ..
                }] if error.contains(&format!("HTTP {status}"))
                    && *retry_after == expected_retry_after
            ));
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                1,
                "retryable HTTP {status} must not trigger reconciliation"
            );
            server.verify().await;
        }
    }

    #[tokio::test]
    async fn access_denial_stops_starting_more_session_requests() {
        use astra_thin_client::ThinClient;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/sync/outbox/events"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let client = ThinClient::new(&server.uri(), None).unwrap();
        let records = (1..=20)
            .map(|sequence| delivery_record(sequence, &format!("session-{sequence}")))
            .collect();
        let (settlements, blocked) = deliver_sync_outbox_records(&client, "token", records).await;
        assert!(blocked);
        assert_eq!(settlements.len(), 20);
        assert!(settlements.iter().all(|settlement| matches!(
            settlement,
            astra_services::SyncOutboxDeliverySettlement::Deferred {
                retry_after: Some(delay), ..
            } if *delay == Duration::ZERO
        )));
        let requests = server.received_requests().await.unwrap().len();
        assert!((1..=8).contains(&requests), "sent {requests} requests");
    }

    #[tokio::test]
    async fn access_denial_keeps_later_session_batches_immediately_retryable() {
        use astra_thin_client::ThinClient;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/sync/outbox/events"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&server)
            .await;
        let client = ThinClient::new(&server.uri(), None).unwrap();
        let records = (1..=3)
            .map(|sequence| {
                let mut record = delivery_record(sequence, "session-a");
                record.payload = serde_json::json!({"blob": "x".repeat(900)});
                record
            })
            .collect();
        let batches = sync_outbox_session_batches(records, 2_048).next().unwrap();
        let (settlements, blocked) = deliver_sync_outbox_session(
            &client,
            "token",
            batches,
            Instant::now() + Duration::from_secs(10),
            None,
            &super::SyncOutboxDeliveryControl::default(),
        )
        .await;
        assert!(blocked);
        assert_eq!(settlements.len(), 3);
        assert!(settlements.iter().all(|settlement| matches!(
            settlement,
            astra_services::SyncOutboxDeliverySettlement::Deferred {
                retry_after: Some(delay), ..
            } if *delay == Duration::ZERO
        )));
        server.verify().await;
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn new_login_interrupts_network_wait_without_losing_completed_ack() {
        use crate::cli::cli_config::cli_utils::{
            cli_owner_auth_snapshot, install_cli_profile_identity_for_test,
        };
        use astra_services::SyncOutboxDeliverySettlement;
        use astra_sync_protocol::{
            SyncOutboxAck, SyncOutboxBatchResponse, SyncOutboxEventBatch, SyncOutboxIngestionStatus,
        };
        use astra_thin_client::ThinClient;
        use std::sync::{Arc, Mutex};
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _credentials = crate::tests::isolate_credentials();
        for waiting_on_reconcile in [false, true] {
            let _owner = install_cli_profile_identity_for_test("a", Some("account-a")).unwrap();
            let snapshot = cli_owner_auth_snapshot();
            let server = MockServer::start().await;
            let client = ThinClient::new(&server.uri(), None).unwrap();
            let records = (1..=3)
                .map(|sequence| {
                    let mut record = delivery_record(sequence, "session-a");
                    record.payload = serde_json::json!({"blob": "x".repeat(900)});
                    record
                })
                .collect::<Vec<_>>();
            let first_id = records[0].record_id.clone();
            let body = |record: &SyncOutboxRecord| {
                serde_json::to_value(SyncOutboxEventBatch::new(vec![
                    event_body_from_outbox_record(record),
                ]))
                .unwrap()
            };
            Mock::given(method("POST"))
                .and(path("/sync/outbox/events"))
                .and(body_json(body(&records[0])))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    SyncOutboxBatchResponse::new(
                        vec![SyncOutboxAck::new(
                            &records[0].record_id,
                            &records[0].payload_hash,
                            SyncOutboxIngestionStatus::Created,
                        )],
                        Vec::new(),
                    ),
                ))
                .expect(1)
                .mount(&server)
                .await;
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let started_tx = Arc::new(Mutex::new(Some(started_tx)));
            let waiting = move |_: &wiremock::Request| {
                if let Some(started) = started_tx.lock().unwrap().take() {
                    let _ = started.send(());
                }
                ResponseTemplate::new(200).set_delay(Duration::from_secs(30))
            };
            let second = Mock::given(method("POST"))
                .and(path("/sync/outbox/events"))
                .and(body_json(body(&records[1])));
            if waiting_on_reconcile {
                second
                    .respond_with(ResponseTemplate::new(503))
                    .expect(1)
                    .mount(&server)
                    .await;
                Mock::given(method("POST"))
                    .and(path("/sync/outbox/events/reconcile"))
                    .respond_with(waiting)
                    .expect(1)
                    .mount(&server)
                    .await;
            } else {
                second.respond_with(waiting).expect(1).mount(&server).await;
            }
            let batches = sync_outbox_session_batches(records, 2_048).next().unwrap();
            let delivery = tokio::spawn(async move {
                deliver_sync_outbox_session(
                    &client,
                    "token",
                    batches,
                    Instant::now() + Duration::from_secs(60),
                    Some(&snapshot),
                    &super::SyncOutboxDeliveryControl::default(),
                )
                .await
            });
            tokio::time::timeout(Duration::from_secs(5), started_rx)
                .await
                .unwrap()
                .unwrap();
            let _new_owner = install_cli_profile_identity_for_test("b", Some("account-b")).unwrap();
            super::clear_sync_cloud_access();
            let (settlements, blocked) = tokio::time::timeout(Duration::from_secs(5), delivery)
                .await
                .unwrap()
                .unwrap();
            assert!(!blocked);
            assert!(
                matches!(&settlements[0], SyncOutboxDeliverySettlement::Ack { record_id, .. } if record_id == &first_id)
            );
            assert_eq!(settlements.len(), 3);
            assert!(settlements[1..].iter().all(|settlement| matches!(
                settlement,
                SyncOutboxDeliverySettlement::Deferred { retry_after: Some(delay), .. } if *delay == Duration::ZERO
            )));
            server.verify().await;
        }
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn new_login_interrupts_wait_for_old_credentials() {
        use crate::cli::auth_flow::{AuthTokenPayload, save_profile_auth_tokens};
        use crate::cli::cli_config::cli_utils::{
            cli_owner_auth_snapshot, install_cli_profile_identity_for_test,
        };
        use std::future::Future;

        let _credentials = crate::tests::isolate_credentials();
        let _owner = install_cli_profile_identity_for_test("sync-test", Some("account")).unwrap();
        let tokens = |access: &str| AuthTokenPayload {
            user_id: "account".into(),
            access_token: access.into(),
            refresh_token: "refresh".into(),
        };
        save_profile_auth_tokens(Some("sync-test"), "user", &tokens("old")).unwrap();
        let snapshot = cli_owner_auth_snapshot();
        let binding = snapshot.legacy_binding.as_ref().unwrap().clone();
        let old_pair = binding.pair.lock().await;
        let (waiting_tx, waiting_rx) = tokio::sync::oneshot::channel();
        let resolver = tokio::spawn(async move {
            let mut token = Box::pin(super::sync_access_token(&snapshot));
            let mut waiting_tx = Some(waiting_tx);
            std::future::poll_fn(|cx| {
                let result = token.as_mut().poll(cx);
                if result.is_pending()
                    && let Some(waiting) = waiting_tx.take()
                {
                    let _ = waiting.send(());
                }
                result
            })
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), waiting_rx)
            .await
            .unwrap()
            .unwrap();
        save_profile_auth_tokens(Some("sync-test"), "user", &tokens("new")).unwrap();
        super::clear_sync_cloud_access();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), resolver)
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );
        drop(old_pair);
    }

    #[tokio::test]
    async fn reconciliation_access_denial_remains_pending_for_relogin() {
        use astra_thin_client::ThinClient;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        for status in [401, 403] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/sync/outbox/events"))
                .respond_with(ResponseTemplate::new(502))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/sync/outbox/events/reconcile"))
                .respond_with(ResponseTemplate::new(status))
                .expect(1)
                .mount(&server)
                .await;
            let client = ThinClient::new(&server.uri(), None).unwrap();
            let (settlements, blocked) =
                deliver_sync_outbox_records(&client, "token", vec![delivery_record(1, "a")]).await;
            assert!(blocked);
            assert!(matches!(settlements.as_slice(), [
                astra_services::SyncOutboxDeliverySettlement::Deferred {
                    retry_after: Some(delay), error, ..
                }
            ] if *delay == Duration::ZERO && error.contains(&format!("HTTP {status}"))));
            server.verify().await;
        }
    }

    #[tokio::test]
    async fn access_denial_does_not_discard_another_sessions_ack() {
        use astra_sync_protocol::{
            SyncOutboxAck, SyncOutboxBatchResponse, SyncOutboxIngestionStatus,
        };
        use astra_thin_client::ThinClient;
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let denied = delivery_record(1, "denied");
        let accepted = delivery_record(2, "accepted");
        let request = |record: &SyncOutboxRecord| {
            serde_json::to_value(astra_sync_protocol::SyncOutboxEventBatch::new(vec![
                event_body_from_outbox_record(record),
            ]))
            .unwrap()
        };
        Mock::given(method("POST"))
            .and(path("/sync/outbox/events"))
            .and(body_json(request(&denied)))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/sync/outbox/events"))
            .and(body_json(request(&accepted)))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(SyncOutboxBatchResponse::new(
                    vec![SyncOutboxAck::new(
                        &accepted.record_id,
                        &accepted.payload_hash,
                        SyncOutboxIngestionStatus::Created,
                    )],
                    Vec::new(),
                )),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = ThinClient::new(&server.uri(), None).unwrap();
        let (settlements, blocked) =
            deliver_sync_outbox_records(&client, "token", vec![denied, accepted]).await;
        assert!(blocked);
        assert!(settlements.iter().any(|result| matches!(
            result, astra_services::SyncOutboxDeliverySettlement::Ack { record_id, .. } if record_id == "record-2"
        )));
        assert!(settlements.iter().any(|result| matches!(
            result, astra_services::SyncOutboxDeliverySettlement::Deferred { record_id, .. } if record_id == "record-1"
        )));
        server.verify().await;
    }

    #[tokio::test]
    async fn reconciliation_rate_limit_preserves_server_retry_delay() {
        use astra_thin_client::ThinClient;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let client = ThinClient::new(&server.uri(), None).expect("thin client");
        Mock::given(method("POST"))
            .and(path("/sync/outbox/events"))
            .respond_with(ResponseTemplate::new(500).set_body_string("unavailable"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/sync/outbox/events/reconcile"))
            .respond_with(
                ResponseTemplate::new(429)
                    .set_body_string("slow down")
                    .insert_header("retry-after", "19"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let (settlements, _) =
            deliver_sync_outbox_records(&client, "token", vec![delivery_record(1, "session-a")])
                .await;
        assert!(matches!(
            settlements.as_slice(),
            [astra_services::SyncOutboxDeliverySettlement::Deferred {
                error,
                retry_after: Some(retry_after),
                ..
            }] if error.contains("HTTP 500") && *retry_after == Duration::from_secs(19)
        ));
        server.verify().await;
    }

    #[tokio::test]
    async fn request_level_client_errors_remain_pending_without_reconciliation() {
        use astra_thin_client::ThinClient;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        for status in [404, 405, 410, 413, 422] {
            let server = MockServer::start().await;
            let client = ThinClient::new(&server.uri(), None).expect("thin client");
            Mock::given(method("POST"))
                .and(path("/sync/outbox/events"))
                .respond_with(ResponseTemplate::new(status).set_body_string("invalid request"))
                .expect(1)
                .mount(&server)
                .await;

            let record = delivery_record(u64::from(status), &format!("session-{status}"));
            let (settlements, _) =
                deliver_sync_outbox_records(&client, "token", vec![record]).await;
            assert!(matches!(
                settlements.as_slice(),
                [astra_services::SyncOutboxDeliverySettlement::Deferred { error, .. }]
                    if error.contains(&format!("HTTP {status}"))
            ));
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                1,
                "HTTP {status} must not trigger reconciliation"
            );
            server.verify().await;
        }
    }

    #[tokio::test]
    async fn proxy_body_limit_splits_batch_without_losing_valid_records() {
        use astra_sync_protocol::{
            SyncOutboxAck, SyncOutboxBatchResponse, SyncOutboxEventBatch, SyncOutboxIngestionStatus,
        };
        use astra_thin_client::ThinClient;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/sync/outbox/events"))
            .respond_with(|request: &wiremock::Request| {
                if request.body.len() > 3_000 {
                    return ResponseTemplate::new(413);
                }
                let body: SyncOutboxEventBatch = serde_json::from_slice(&request.body).unwrap();
                let acks = body
                    .events
                    .into_iter()
                    .map(|event| {
                        SyncOutboxAck::new(
                            event["event_id"].as_str().unwrap(),
                            event["metadata"]["sync_outbox"]["payload_hash"]
                                .as_str()
                                .unwrap(),
                            SyncOutboxIngestionStatus::Created,
                        )
                    })
                    .collect();
                ResponseTemplate::new(200).set_body_json(SyncOutboxBatchResponse::new(acks, vec![]))
            })
            .expect(3)
            .mount(&server)
            .await;
        let client = ThinClient::new(&server.uri(), None).unwrap();
        let records = (1..=2)
            .map(|sequence| {
                let mut record = delivery_record(sequence, "session-a");
                record.payload = serde_json::json!({"blob": "x".repeat(2_000)});
                record
            })
            .collect::<Vec<_>>();
        let (settlements, _) = deliver_sync_outbox_records(&client, "token", records).await;
        assert!(matches!(settlements.as_slice(), [
            SyncOutboxDeliverySettlement::Ack { record_id: a, .. },
            SyncOutboxDeliverySettlement::Ack { record_id: b, .. }
        ] if a == "record-1" && b == "record-2"));
        server.verify().await;
    }

    #[tokio::test]
    async fn service_failure_stops_other_session_requests_and_preserves_retry_after() {
        use astra_thin_client::ThinClient;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        for status in [400, 429, 500, 503] {
            let server = MockServer::start().await;
            let response = if matches!(status, 400 | 500) {
                ResponseTemplate::new(status)
            } else {
                ResponseTemplate::new(status).insert_header("retry-after", "60")
            };
            Mock::given(method("POST"))
                .and(path("/sync/outbox/events"))
                .respond_with(response)
                .mount(&server)
                .await;
            let reconciliation = if status == 500 {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "schema_version": 1, "acks": [], "rejections": []
                }))
            } else {
                ResponseTemplate::new(503).insert_header("retry-after", "90")
            };
            Mock::given(method("POST"))
                .and(path("/sync/outbox/events/reconcile"))
                .respond_with(reconciliation)
                .mount(&server)
                .await;
            let client = ThinClient::new(&server.uri(), None).unwrap();
            let records = (0..1_000)
                .map(|sequence| delivery_record(sequence, &format!("s-{sequence:04}")))
                .collect();
            let (settlements, access, retry_after) = deliver_sync_outbox_records_within(
                &client,
                "token",
                records,
                Instant::now() + Duration::from_secs(20),
                None,
            )
            .await;
            assert!(!access);
            assert_eq!(
                retry_after,
                Some(Duration::from_secs(match status {
                    400 | 500 => 16,
                    429 => 60,
                    _ => 90,
                }))
            );
            assert_eq!(settlements.len(), 1_000);
            assert!(
                settlements
                    .iter()
                    .all(|result| matches!(result, SyncOutboxDeliverySettlement::Deferred { .. }))
            );
            if matches!(status, 400 | 500) {
                assert!(settlements.iter().any(|result| matches!(result,
                    SyncOutboxDeliverySettlement::Deferred { retry_after: Some(delay), error, .. }
                        if *delay == super::SYNC_OUTBOX_SERVICE_RETRY_DELAY * 2 && error.contains(&format!("HTTP {status}"))
                )));
            }
            let requests = server.received_requests().await.unwrap();
            let writes = requests
                .iter()
                .filter(|request| request.url.path() == "/sync/outbox/events")
                .count();
            let reads = requests.len() - writes;
            let max_writes = if matches!(status, 400 | 500) {
                2 * super::SYNC_OUTBOX_MAX_CONCURRENT_SESSIONS - 1
            } else {
                super::SYNC_OUTBOX_MAX_CONCURRENT_SESSIONS
            };
            assert!(
                (1..=max_writes).contains(&writes),
                "{status}: {writes} writes"
            );
            assert!(
                reads <= writes,
                "{status}: {reads} reconciliations for {writes} writes"
            );
        }
    }

    #[tokio::test]
    async fn one_request_failure_does_not_pause_healthy_sessions() {
        use astra_sync_protocol::{
            SyncOutboxAck, SyncOutboxBatchResponse, SyncOutboxEventBatch, SyncOutboxIngestionStatus,
        };
        use astra_thin_client::ThinClient;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        for status in [400, 500] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/sync/outbox/events"))
                .respond_with(move |request: &wiremock::Request| {
                    let batch: SyncOutboxEventBatch =
                        serde_json::from_slice(&request.body).unwrap();
                    let event = &batch.events[0];
                    if event["session_id"] == "s-0000" {
                        return ResponseTemplate::new(status);
                    }
                    ResponseTemplate::new(200).set_body_json(SyncOutboxBatchResponse::new(
                        vec![SyncOutboxAck::new(
                            event["event_id"].as_str().unwrap(),
                            event["metadata"]["sync_outbox"]["payload_hash"]
                                .as_str()
                                .unwrap(),
                            SyncOutboxIngestionStatus::Created,
                        )],
                        vec![],
                    ))
                })
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/sync/outbox/events/reconcile"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "schema_version": 1, "acks": [], "rejections": []
                })))
                .expect(if status == 500 { 1 } else { 0 })
                .mount(&server)
                .await;
            let client = ThinClient::new(&server.uri(), None).unwrap();
            let records = (0..100)
                .map(|sequence| delivery_record(sequence, &format!("s-{sequence:04}")))
                .collect();
            let (settlements, access, retry) = deliver_sync_outbox_records_within(
                &client,
                "token",
                records,
                Instant::now() + Duration::from_secs(20),
                None,
            )
            .await;
            assert!(!access && retry.is_none());
            assert_eq!(
                settlements
                    .iter()
                    .filter(|result| matches!(result, SyncOutboxDeliverySettlement::Ack { .. }))
                    .count(),
                99
            );
            assert_eq!(
                settlements
                    .iter()
                    .filter(|result| matches!(
                        result,
                        SyncOutboxDeliverySettlement::Deferred { .. }
                    ))
                    .count(),
                1
            );
            server.verify().await;
        }
    }

    #[tokio::test]
    async fn reconciled_server_errors_still_honor_retry_after() {
        use astra_thin_client::ThinClient;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        for status in [500, 502, 503, 504] {
            let server = MockServer::start().await;
            let client = ThinClient::new(&server.uri(), None).unwrap();
            let records = vec![delivery_record(1, "session-a")];
            Mock::given(method("POST"))
                .and(path("/sync/outbox/events"))
                .respond_with(ResponseTemplate::new(status).insert_header("retry-after", "17"))
                .expect(1)
                .mount(&server)
                .await;
            mount_sync_outbox_reconciliation(&server, &records, &[true]).await;
            let (settlements, access, retry) = deliver_sync_outbox_records_within(
                &client,
                "token",
                records,
                Instant::now() + Duration::from_secs(20),
                None,
            )
            .await;
            assert!(!access);
            assert_eq!(retry, Some(Duration::from_secs(17)));
            assert!(matches!(
                settlements.as_slice(),
                [SyncOutboxDeliverySettlement::Ack { .. }]
            ));
            server.verify().await;
        }
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

        let (settlements, _) = deliver_sync_outbox_records(&client, "token", vec![record]).await;
        assert!(matches!(
            settlements.as_slice(),
            [astra_services::SyncOutboxDeliverySettlement::Rejected { .. }]
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

        let (settlements, _) = deliver_sync_outbox_records(&client, "token", vec![record]).await;
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
    fn blocked_access_stops_new_drains_and_an_existing_retry_wake() {
        use crate::cli::cli_config::cli_utils::{
            cli_owner_auth_snapshot, install_cli_profile_identity_for_test,
        };
        let _credentials = crate::tests::isolate_credentials();
        let _owner = install_cli_profile_identity_for_test("sync-test", Some("account")).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        temp_env::with_var("ASTRA_API_URL", Some("http://127.0.0.1:9"), || {
            runtime.block_on(async {
                super::clear_sync_cloud_access();
                release_sync_outbox_drain_schedule();
                release_sync_outbox_retry_wake_schedule();
                super::schedule_sync_outbox_retry_wake(Duration::from_secs(30));
                tokio::task::yield_now().await;
                let snapshot = cli_owner_auth_snapshot();
                super::block_sync_cloud_access(&snapshot);

                super::schedule_sync_outbox_drain();
                drop(
                    try_claim_sync_outbox_drain_schedule()
                        .expect("blocked hint must not schedule a drain"),
                );
                tokio::time::advance(Duration::from_secs(30)).await;
                tokio::task::yield_now().await;
                assert_eq!(
                    super::SYNC_OUTBOX_RETRY_WAKE_DEADLINE_MS.load(super::Ordering::Acquire),
                    0
                );
                drop(
                    try_claim_sync_outbox_drain_schedule()
                        .expect("old wake must not schedule a drain"),
                );
                super::clear_sync_cloud_access();
            });
        });
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
            Some(Duration::ZERO),
            "a healthy service must drain an existing backlog without a fixed cooldown"
        );
    }
}
