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
//! - **Preferences**: [`try_cloud_pull_preferences`] / [`try_cloud_push_preferences`].
//!   Both go through [`crate::preferences_client`] now.

use astra_services::session_journal;
use astra_services::state_sync::pref_keys;
use astra_services::{
    SyncOutboxDeliverySettlement, SyncOutboxRecord, SyncOutboxSettlementReport, SyncOutboxStatus,
    SyncOutboxStore,
};
use astra_turn_core::tool_health_persistence::ToolHealthEntry;
use serde_json::{Value, json};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::FutureExt;

use crate::cli::session::session_runtime;
use crate::cli::session::session_side_effects::{
    enqueue_ingestion_for_immediate_drain_pub, enqueue_ingestion_pub,
};
use crate::{ExplainMode, SessionState};

const SYNC_OUTBOX_DRAIN_LIMIT: usize = 64;
const SYNC_OUTBOX_DRAIN_BACKGROUND_ROUNDS: usize = 4;
const SYNC_OUTBOX_RECORD_DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);
static SYNC_OUTBOX_DRAIN_OWNER: AtomicU64 = AtomicU64::new(0);
static SYNC_OUTBOX_NEXT_DRAIN_OWNER: AtomicU64 = AtomicU64::new(1);
static SYNC_OUTBOX_RETRY_WAKE_DEADLINE_MS: AtomicU64 = AtomicU64::new(0);

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
    spawn_detached_tracked(&handle, async move {
        let mut blocked = false;
        for _ in 0..SYNC_OUTBOX_DRAIN_BACKGROUND_ROUNDS {
            let report = try_drain_sync_outbox(SYNC_OUTBOX_DRAIN_LIMIT).await;
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
        }
        if blocked {
            return;
        }
        let next_delay =
            match run_sync_outbox_io(SyncOutboxStore::local(), |store| store.status()).await {
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
/// is configured (offline mode). Reads `ASTRA_API_URL`.
fn resolve_cloud_base() -> Option<String> {
    std::env::var("ASTRA_API_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
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

/// Push user preferences to cloud at session end.
pub(crate) async fn try_cloud_push_preferences(state: &SessionState) {
    let Some(cloud_base) = resolve_cloud_base() else {
        return;
    };
    let token = session_runtime::current_access_token(None);

    let blocked: Vec<String> = state
        .tool_health_entries
        .iter()
        .filter(|e| e.failure_rate >= 1.0)
        .map(|e| e.name.clone())
        .collect();
    let blocked_json = serde_json::to_string(&blocked).unwrap_or_else(|_| "[]".to_string());

    let prefs = [
        (pref_keys::EXPLAIN_MODE, state.explain.to_string()),
        (pref_keys::BLOCKED_TOOLS, blocked_json),
        (
            pref_keys::AUTO_MEMORY_ENABLED,
            state.auto_memory_enabled.to_string(),
        ),
        (
            pref_keys::NOTIFICATIONS_ENABLED,
            state.notifications_enabled.to_string(),
        ),
        (
            pref_keys::NOTIFICATION_METHOD,
            state.notification_method.to_string(),
        ),
        (
            pref_keys::NOTIFICATION_THRESHOLD_SECS,
            state.notification_threshold_secs.to_string(),
        ),
    ];
    for (key, value) in &prefs {
        if let Err(e) = crate::cli::preferences_client::push_preference(
            &cloud_base,
            token.as_deref(),
            key,
            value,
        )
        .await
        {
            tracing::warn!(
                target: "astra_cli::cloud_sync",
                error = %e,
                key = %key,
                "preference push failed"
            );
        }
    }
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
            "sync outbox drain did not fully converge"
        );
    }
}

pub(crate) async fn try_drain_sync_outbox(limit: usize) -> SyncOutboxDrainReport {
    let Some(cloud_base) = resolve_cloud_base() else {
        return SyncOutboxDrainReport::default();
    };
    let mut report = SyncOutboxDrainReport {
        cloud_configured: true,
        ..Default::default()
    };
    let store = SyncOutboxStore::local();
    match run_sync_outbox_io(store.clone(), |store| store.status()).await {
        Ok(status) => {
            report.remaining_ready = status.claimable.min(u64::from(u32::MAX)) as u32;
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
    let Some(token) =
        session_runtime::current_access_token(None).filter(|token| !token.trim().is_empty())
    else {
        report.blocker = Some(SyncOutboxDrainBlocker::MissingAccessToken);
        tracing::debug!(
            target: "astra_cli::cloud_sync",
            "sync outbox drain skipped because no access token is available"
        );
        return report;
    };
    let client = match astra_thin_client::ThinClient::new(&cloud_base, Some(token.clone())) {
        Ok(client) => client,
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
        store.claim_ready_records(limit)
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
    let mut settlements = Vec::new();
    for record in records {
        report.attempted += 1;
        let body = event_body_from_outbox_record(&record);
        let delivery = tokio::time::timeout(
            SYNC_OUTBOX_RECORD_DELIVERY_TIMEOUT,
            client.post_sync_outbox_event_json(Some(token.as_str()), &body),
        )
        .await;
        let delivery_error = match delivery {
            Ok(Ok(ack)) if ack.confirms(&record.record_id, &record.payload_hash) => None,
            Ok(Ok(ack)) => Some(format!("cloud returned an uncorrelated sync ack: {ack:?}")),
            Ok(Err(error)) => Some(error.to_string()),
            Err(_) => Some(format!(
                "cloud event delivery timed out after {}ms",
                SYNC_OUTBOX_RECORD_DELIVERY_TIMEOUT.as_millis()
            )),
        };
        let delivery_confirmed = delivery_error.is_none()
            || reconcile_sync_outbox_delivery(&client, &token, &record).await;
        if delivery_confirmed {
            settlements.push(SyncOutboxDeliverySettlement::Ack {
                record_id: record.record_id,
                payload_hash: record.payload_hash,
            });
        } else {
            settlements.push(SyncOutboxDeliverySettlement::Failed {
                record_id: record.record_id,
                error: delivery_error
                    .unwrap_or_else(|| "cloud delivery could not be reconciled".to_string()),
            });
        }
    }
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

async fn reconcile_sync_outbox_delivery(
    client: &astra_thin_client::ThinClient,
    token: &str,
    record: &SyncOutboxRecord,
) -> bool {
    client
        .get_event_json(Some(token), &record.record_id)
        .await
        .ok()
        .is_some_and(|event| stored_event_matches_outbox_record(record, &event))
}

fn stored_event_matches_outbox_record(record: &SyncOutboxRecord, response: &Value) -> bool {
    response.get("event_id").and_then(Value::as_str) == Some(record.record_id.as_str())
        && response
            .get("metadata")
            .and_then(|metadata| metadata.get("sync_outbox"))
            .and_then(|sync| sync.get("payload_hash"))
            .and_then(Value::as_str)
            == Some(record.payload_hash.as_str())
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
    let Ok(writer) = session_journal::JournalWriter::new(sid) else {
        tracing::warn!(
            session_id = sid,
            context = "cloud_sync:cloud_pull_sync_marker",
            "failed to open session journal for cloud pull sync marker"
        );
        return;
    };
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
    use std::time::Duration;

    use super::{
        CloudPullResult, SyncOutboxDrainReport, claim_sync_outbox_retry_wake_deadline,
        cloud_pull_warrants_sync_marker, next_sync_outbox_drain_delay, record_settlement_result,
        release_sync_outbox_drain_schedule, release_sync_outbox_retry_wake_schedule,
        run_sync_outbox_io, should_append_cloud_pull_journal, try_claim_sync_outbox_drain_schedule,
    };
    use astra_services::{SyncOutboxSettlementReport, SyncOutboxStatus, SyncOutboxStore};
    use fs2::FileExt;

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
    }
}
