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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use futures_util::FutureExt;

use crate::cli::session::session_runtime;
use crate::cli::session::session_side_effects::enqueue_ingestion_pub;
use crate::{ExplainMode, SessionState};

const SYNC_OUTBOX_DRAIN_LIMIT: usize = 64;
const SYNC_OUTBOX_DRAIN_BACKGROUND_ROUNDS: usize = 4;
const SYNC_OUTBOX_RECORD_DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);
static SYNC_OUTBOX_DRAIN_SCHEDULED: AtomicBool = AtomicBool::new(false);
static SYNC_OUTBOX_RETRY_WAKE_DEADLINE_MS: AtomicU64 = AtomicU64::new(0);

struct SyncOutboxDrainScheduleGuard;

impl Drop for SyncOutboxDrainScheduleGuard {
    fn drop(&mut self) {
        release_sync_outbox_drain_schedule();
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

/// Spawn a fire-and-forget future, logging any panic so it is not silently
/// swallowed when the returned [`JoinHandle`] is dropped.
fn spawn_tracked(fut: impl Future<Output = ()> + Send + 'static) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    handle.spawn(async move {
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
}

/// Result from cloud pull attempt at session start.
pub(crate) struct CloudPullResult {
    /// True when the server's preferences endpoint responded
    /// successfully (regardless of whether it returned data).
    pub cloud_reachable: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SyncOutboxDrainReport {
    pub cloud_configured: bool,
    pub attempted: u32,
    pub acked: u32,
    pub failed: u32,
    pub remaining_ready: u32,
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
    if !try_claim_sync_outbox_drain_schedule() {
        return;
    }
    if tokio::runtime::Handle::try_current().is_err() {
        release_sync_outbox_drain_schedule();
        return;
    }
    spawn_tracked(async {
        let schedule_guard = SyncOutboxDrainScheduleGuard;
        for _ in 0..SYNC_OUTBOX_DRAIN_BACKGROUND_ROUNDS {
            let report = try_drain_sync_outbox(SYNC_OUTBOX_DRAIN_LIMIT).await;
            if !report.cloud_configured || report.remaining_ready == 0 || report.attempted == 0 {
                break;
            }
        }
        let next_delay = SyncOutboxStore::local()
            .status()
            .ok()
            .and_then(|status| next_sync_outbox_drain_delay(&status));
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
    if !claim_sync_outbox_retry_wake_deadline(deadline) {
        return;
    }
    if tokio::runtime::Handle::try_current().is_err() {
        release_sync_outbox_retry_wake_schedule();
        return;
    }
    spawn_tracked(async move {
        let _wake_guard = SyncOutboxRetryWakeGuard { deadline };
        tokio::time::sleep(delay).await;
        if SYNC_OUTBOX_RETRY_WAKE_DEADLINE_MS
            .compare_exchange(deadline, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            schedule_sync_outbox_drain();
        }
    });
}

fn try_claim_sync_outbox_drain_schedule() -> bool {
    SYNC_OUTBOX_DRAIN_SCHEDULED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

fn release_sync_outbox_drain_schedule() {
    SYNC_OUTBOX_DRAIN_SCHEDULED.store(false, Ordering::Release);
}

fn claim_sync_outbox_retry_wake_deadline(deadline: u64) -> bool {
    let mut current = SYNC_OUTBOX_RETRY_WAKE_DEADLINE_MS.load(Ordering::Acquire);
    loop {
        if current != 0 && current <= deadline {
            return false;
        }
        match SYNC_OUTBOX_RETRY_WAKE_DEADLINE_MS.compare_exchange_weak(
            current,
            deadline,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(next) => current = next,
        }
    }
}

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
    if report.failed > 0 {
        tracing::warn!(
            target: "astra_cli::cloud_sync",
            attempted = report.attempted,
            acked = report.acked,
            failed = report.failed,
            remaining_ready = report.remaining_ready,
            "sync outbox drain completed with failures"
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
    let Some(token) =
        session_runtime::current_access_token(None).filter(|token| !token.trim().is_empty())
    else {
        tracing::debug!(
            target: "astra_cli::cloud_sync",
            "sync outbox drain skipped because no access token is available"
        );
        return report;
    };
    let client = match astra_thin_client::ThinClient::new(&cloud_base, Some(token.clone())) {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(
                target: "astra_cli::cloud_sync",
                ?error,
                "sync outbox drain skipped because cloud base URL is invalid"
            );
            return report;
        }
    };
    let store = SyncOutboxStore::local();
    let records = match store.claim_ready_records(limit) {
        Ok(records) => records,
        Err(error) => {
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
        match delivery {
            Ok(Ok(response)) if event_response_ack_matches(&record, &response) => {
                settlements.push(SyncOutboxDeliverySettlement::Ack {
                    record_id: record.record_id,
                    payload_hash: record.payload_hash,
                });
            }
            Ok(Ok(response)) => {
                settlements.push(SyncOutboxDeliverySettlement::Failed {
                    record_id: record.record_id,
                    error: format!("cloud ack mismatch for sync outbox record: {response}"),
                });
            }
            Ok(Err(error)) => {
                settlements.push(SyncOutboxDeliverySettlement::Failed {
                    record_id: record.record_id,
                    error: error.to_string(),
                });
            }
            Err(_) => {
                settlements.push(SyncOutboxDeliverySettlement::Failed {
                    record_id: record.record_id,
                    error: format!(
                        "cloud event delivery timed out after {}ms",
                        SYNC_OUTBOX_RECORD_DELIVERY_TIMEOUT.as_millis()
                    ),
                });
            }
        }
    }
    if !settlements.is_empty() {
        record_settlement_result(
            &mut report,
            settlements.len(),
            store.settle_delivery_batch(&settlements),
        );
    }
    report.remaining_ready = store
        .status()
        .map(|status| status.claimable.min(u64::from(u32::MAX)) as u32)
        .unwrap_or(0);
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
            report.failed += settlement
                .failed
                .saturating_add(settlement.missing)
                .saturating_add(settlement.poisoned);
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

fn event_response_ack_matches(record: &SyncOutboxRecord, response: &Value) -> bool {
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
        enqueue_ingestion_pub(state, &evt);
    }
}

/// Re-sync preferences from cloud after authentication.
pub(crate) async fn post_auth_cloud_resync(profile: Option<&str>, state: &mut SessionState) {
    let profile_name = profile.unwrap_or("default");
    let pull = try_cloud_pull(profile_name).await;
    let pref_keys = try_cloud_pull_preferences(state).await;
    append_cloud_pull_sync_journal(state, profile_name, "post_login", &pull, &pref_keys);
    let _ = try_drain_sync_outbox(SYNC_OUTBOX_DRAIN_LIMIT).await;
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        CloudPullResult, SyncOutboxDrainReport, SyncOutboxDrainScheduleGuard,
        SyncOutboxRetryWakeGuard, claim_sync_outbox_retry_wake_deadline,
        cloud_pull_warrants_sync_marker, next_sync_outbox_drain_delay, record_settlement_result,
        release_sync_outbox_drain_schedule, release_sync_outbox_retry_wake_schedule,
        should_append_cloud_pull_journal, try_claim_sync_outbox_drain_schedule,
    };
    use astra_services::{SyncOutboxSettlementReport, SyncOutboxStatus};

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
        assert!(try_claim_sync_outbox_drain_schedule());
        drop(SyncOutboxDrainScheduleGuard);
        assert!(try_claim_sync_outbox_drain_schedule());
        release_sync_outbox_drain_schedule();
    }

    #[serial_test::serial]
    #[test]
    fn retry_wake_guard_releases_matching_deadline_on_drop() {
        release_sync_outbox_retry_wake_schedule();
        let deadline = 123_456;
        assert!(claim_sync_outbox_retry_wake_deadline(deadline));
        drop(SyncOutboxRetryWakeGuard { deadline });
        assert!(claim_sync_outbox_retry_wake_deadline(deadline));
        release_sync_outbox_retry_wake_schedule();
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
        assert_eq!(report.failed, 2);
    }

    #[serial_test::serial]
    #[test]
    fn drain_scheduler_allows_only_one_worker_until_released() {
        release_sync_outbox_drain_schedule();
        release_sync_outbox_retry_wake_schedule();

        assert!(try_claim_sync_outbox_drain_schedule());
        assert!(!try_claim_sync_outbox_drain_schedule());
        release_sync_outbox_drain_schedule();
        assert!(try_claim_sync_outbox_drain_schedule());

        release_sync_outbox_drain_schedule();
        assert!(claim_sync_outbox_retry_wake_deadline(200));
        assert!(claim_sync_outbox_retry_wake_deadline(100));
        assert!(!claim_sync_outbox_retry_wake_deadline(150));
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
