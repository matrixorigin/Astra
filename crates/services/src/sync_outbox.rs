//! Durable edge-to-cloud sync outbox.
//!
//! The session journal is the local fact log, but it is not a sync boundary by
//! itself: a local JSONL line only proves that the edge observed a fact.  This
//! module adds the missing durable queue boundary between local journal writes
//! and cloud ingestion.  Records have a stable logical event id, a separate
//! payload hash, explicit retry/backoff state, a contiguous ack watermark, and
//! poison isolation so one bad record cannot block later records forever.

use crate::session_journal::{self, JournalEvent, JournalEventType};
use crate::sync_engine::SyncDomain;
use astra_core::canonical_json_string;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const SYNC_OUTBOX_SCHEMA_VERSION: u32 = 2;
pub const SYNC_OUTBOX_MAX_ATTEMPTS: u32 = 5;
pub const SYNC_OUTBOX_IN_FLIGHT_LEASE_MS: u64 = 5 * 60 * 1000;
pub const SYNC_OUTBOX_ACKED_RETAINED_RECORDS: usize = 128;
pub const SYNC_OUTBOX_ACK_TOMBSTONE_RETAINED_RECORDS: usize = 4096;
pub const SYNC_OUTBOX_SKIPPED_RETAINED_RECORDS: usize = 128;
const SYNC_OUTBOX_LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const SYNC_OUTBOX_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncOutboxRecordState {
    Pending,
    InFlight,
    Acked,
    Poisoned,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncOutboxPoisonKind {
    PayloadHashMismatch,
    RetryExhausted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncOutboxRecord {
    pub schema_version: u32,
    pub sequence: u64,
    pub record_id: String,
    pub domain: SyncDomain,
    pub session_id: String,
    pub event_type: String,
    pub event_ts: String,
    pub payload_hash: String,
    pub payload: Value,
    pub state: SyncOutboxRecordState,
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_retry_after_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poison_kind: Option<SyncOutboxPoisonKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poison_reason: Option<String>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acked_at_unix_ms: Option<u64>,
}

impl SyncOutboxRecord {
    pub fn canonical_payload_json(&self) -> String {
        canonical_json_string(&self.payload)
    }
}

pub fn sync_outbox_canonical_payload_hash(payload: &Value) -> String {
    let payload_json = canonical_json_string(payload);
    sha256_prefixed(payload_json.as_bytes())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncOutboxFile {
    pub schema_version: u32,
    pub ack_watermark: u64,
    /// Per-source byte watermark for canonical session journals. The outbox is
    /// a derived delivery projection, so a process can recover a lost
    /// in-memory enqueue by replaying only journal bytes beyond this offset.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub journal_source_offsets: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acked_tombstones: Vec<SyncOutboxAckTombstone>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_records: Vec<SyncOutboxSkippedRecord>,
    pub records: Vec<SyncOutboxRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncOutboxAckTombstone {
    pub record_id: String,
    pub sequence: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub payload_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncOutboxSkipKind {
    MissingSessionId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncOutboxSkippedRecord {
    pub kind: SyncOutboxSkipKind,
    pub event_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_ts: Option<String>,
    pub reason: String,
    pub observed_at_unix_ms: u64,
}

impl Default for SyncOutboxFile {
    fn default() -> Self {
        Self {
            schema_version: SYNC_OUTBOX_SCHEMA_VERSION,
            ack_watermark: 0,
            journal_source_offsets: BTreeMap::new(),
            acked_tombstones: Vec::new(),
            skipped_records: Vec::new(),
            records: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutboxEnqueueOutcome {
    Inserted { record_id: String, sequence: u64 },
    Duplicate { record_id: String, sequence: u64 },
    Poisoned { record_id: String, sequence: u64 },
}

/// Result of atomically projecting one append-only journal delta into the
/// outbox. A stale source offset means another process already advanced this
/// source; callers must reread from the returned offset instead of guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutboxJournalDeltaOutcome {
    Appended {
        scanned_events: usize,
        inserted: usize,
        duplicates: usize,
        poisoned: usize,
        skipped: usize,
    },
    StaleSourceOffset {
        actual_offset: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutboxAckOutcome {
    Acked { record_id: String, sequence: u64 },
    DuplicateAck { record_id: String, sequence: u64 },
    NotFound,
    Poisoned { record_id: String, sequence: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutboxDeliverySettlement {
    Ack {
        record_id: String,
        payload_hash: String,
    },
    Failed {
        record_id: String,
        error: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncOutboxSettlementReport {
    pub acked: u32,
    pub failed: u32,
    pub missing: u32,
    pub poisoned: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncOutboxStatus {
    pub total: u64,
    pub pending: u64,
    pub ready: u64,
    pub claimable: u64,
    pub in_flight: u64,
    pub stale_in_flight: u64,
    pub acked: u64,
    #[serde(default)]
    pub ack_tombstones: u64,
    #[serde(default)]
    pub skipped: u64,
    pub poisoned: u64,
    pub retry_deferred: u64,
    pub ack_watermark: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_pending_created_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_retry_after_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_skipped_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_skipped_event_type: Option<String>,
    pub degraded: bool,
}

/// Blocking, file-backed durable outbox transaction boundary.
///
/// Every mutating method may wait for the cross-process file lock and perform
/// read/write/rename/fsync work. Async hosts must move the complete method call
/// to a blocking executor; moving only lock polling off-thread is insufficient.
#[derive(Debug, Clone)]
pub struct SyncOutboxStore {
    root: PathBuf,
}

impl SyncOutboxStore {
    pub fn local() -> Self {
        Self::new(session_journal::local_sessions_dir().join("sync"))
    }

    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn path(&self) -> PathBuf {
        self.root.join("outbox.json")
    }

    pub fn enqueue_journal_event(
        &self,
        event: &JournalEvent,
    ) -> std::io::Result<SyncOutboxEnqueueOutcome> {
        let mut outcomes = self.enqueue_journal_events(std::slice::from_ref(event))?;
        // One input produces exactly one typed outcome. Keeping the single
        // event method as a thin wrapper means callers retain its precise
        // contract while multi-event turn sidecars share one lock/fsync.
        Ok(outcomes
            .pop()
            .expect("single outbox enqueue always returns one outcome"))
    }

    /// Enqueue a journal batch as one durable outbox transaction.
    ///
    /// Each event retains the same stable-id deduplication and poison rules as
    /// [`Self::enqueue_journal_event`]. The difference is purely transactional:
    /// callers that already committed an ordered journal sidecar batch do not
    /// pay one cross-process lock, file rewrite, and directory fsync per
    /// event.
    pub fn enqueue_journal_events(
        &self,
        events: &[JournalEvent],
    ) -> std::io::Result<Vec<SyncOutboxEnqueueOutcome>> {
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let now = unix_ms()?;
        let candidates = events
            .iter()
            .map(|event| build_record(event, 0, now))
            .collect::<std::io::Result<Vec<_>>>()?;
        self.with_state(|state| {
            candidates
                .into_iter()
                .map(|candidate| enqueue_candidate(state, candidate, now))
                .collect()
        })
    }

    /// Return the durable cursor for an append-only canonical journal.
    pub fn journal_source_offset(&self, session_id: &str) -> std::io::Result<u64> {
        self.with_state_readonly(|state| {
            Ok(state
                .journal_source_offsets
                .get(session_id)
                .copied()
                .unwrap_or(0))
        })
    }

    /// Project a journal byte range into the outbox and advance that journal's
    /// cursor in the exact same durable transaction.
    ///
    /// The source reader runs outside the outbox lock. `expected_offset` is
    /// therefore compared under the lock so multiple CLI processes cannot
    /// independently project overlapping ranges and accidentally move the
    /// cursor backwards.
    pub fn append_journal_delta(
        &self,
        source_session_id: &str,
        expected_offset: u64,
        next_offset: u64,
        events: &[JournalEvent],
    ) -> std::io::Result<SyncOutboxJournalDeltaOutcome> {
        if next_offset < expected_offset {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "journal source offset cannot move backwards",
            ));
        }
        let now = unix_ms()?;
        let candidates = events
            .iter()
            .filter(|event| {
                event
                    .session_id
                    .as_deref()
                    .is_some_and(|session_id| !session_id.trim().is_empty())
            })
            .map(|event| build_record(event, 0, now))
            .collect::<std::io::Result<Vec<_>>>()?;
        self.with_state(|state| {
            let actual_offset = state
                .journal_source_offsets
                .get(source_session_id)
                .copied()
                .unwrap_or(0);
            if actual_offset != expected_offset {
                return Ok(SyncOutboxJournalDeltaOutcome::StaleSourceOffset { actual_offset });
            }

            let mut inserted = 0;
            let mut duplicates = 0;
            let mut poisoned = 0;
            for candidate in candidates {
                match enqueue_candidate(state, candidate, now)? {
                    SyncOutboxEnqueueOutcome::Inserted { .. } => inserted += 1,
                    SyncOutboxEnqueueOutcome::Duplicate { .. } => duplicates += 1,
                    SyncOutboxEnqueueOutcome::Poisoned { .. } => poisoned += 1,
                }
            }
            let skipped = events
                .len()
                .saturating_sub(inserted + duplicates + poisoned);
            for event in events.iter().filter(|event| {
                event
                    .session_id
                    .as_deref()
                    .is_none_or(|session_id| session_id.trim().is_empty())
            }) {
                state.skipped_records.push(SyncOutboxSkippedRecord {
                    kind: SyncOutboxSkipKind::MissingSessionId,
                    event_type: event_type_string(&event.event_type),
                    event_ts: Some(event.ts.clone()).filter(|ts| !ts.trim().is_empty()),
                    reason: "journal event has no session_id and cannot be delivered to /events"
                        .to_string(),
                    observed_at_unix_ms: now,
                });
            }
            state.compact_skipped_records();
            state
                .journal_source_offsets
                .insert(source_session_id.to_string(), next_offset);
            Ok(SyncOutboxJournalDeltaOutcome::Appended {
                scanned_events: events.len(),
                inserted,
                duplicates,
                poisoned,
                skipped,
            })
        })
    }

    pub fn record_skipped_journal_event(
        &self,
        event: &JournalEvent,
        kind: SyncOutboxSkipKind,
        reason: impl Into<String>,
    ) -> std::io::Result<()> {
        let reason = reason.into();
        self.with_state(|state| {
            state.skipped_records.push(SyncOutboxSkippedRecord {
                kind,
                event_type: event_type_string(&event.event_type),
                event_ts: Some(event.ts.clone()).filter(|ts| !ts.trim().is_empty()),
                reason,
                observed_at_unix_ms: unix_ms()?,
            });
            state.compact_skipped_records();
            Ok(())
        })
    }

    pub fn acknowledge(
        &self,
        record_id: &str,
        payload_hash: Option<&str>,
    ) -> std::io::Result<SyncOutboxAckOutcome> {
        self.with_state(|state| {
            let Some(index) = state
                .records
                .iter()
                .position(|record| record.record_id == record_id)
            else {
                return Ok(SyncOutboxAckOutcome::NotFound);
            };
            let outcome = acknowledge_record(&mut state.records[index], payload_hash, unix_ms()?);
            state.recompute_ack_watermark();
            Ok(outcome)
        })
    }

    pub fn settle_delivery_batch(
        &self,
        settlements: &[SyncOutboxDeliverySettlement],
    ) -> std::io::Result<SyncOutboxSettlementReport> {
        self.with_state(|state| {
            let now = unix_ms()?;
            let mut report = SyncOutboxSettlementReport::default();
            for settlement in settlements {
                match settlement {
                    SyncOutboxDeliverySettlement::Ack {
                        record_id,
                        payload_hash,
                    } => {
                        let Some(record) = state
                            .records
                            .iter_mut()
                            .find(|record| record.record_id.as_str() == record_id.as_str())
                        else {
                            report.missing = report.missing.saturating_add(1);
                            continue;
                        };
                        match acknowledge_record(record, Some(payload_hash.as_str()), now) {
                            SyncOutboxAckOutcome::Acked { .. }
                            | SyncOutboxAckOutcome::DuplicateAck { .. } => {
                                report.acked = report.acked.saturating_add(1);
                            }
                            SyncOutboxAckOutcome::Poisoned { .. } => {
                                report.poisoned = report.poisoned.saturating_add(1);
                            }
                            SyncOutboxAckOutcome::NotFound => {
                                report.missing = report.missing.saturating_add(1);
                            }
                        }
                    }
                    SyncOutboxDeliverySettlement::Failed { record_id, error } => {
                        let Some(record) = state
                            .records
                            .iter_mut()
                            .find(|record| record.record_id.as_str() == record_id.as_str())
                        else {
                            report.missing = report.missing.saturating_add(1);
                            continue;
                        };
                        if apply_delivery_failure(record, error, now) {
                            if record.state == SyncOutboxRecordState::Poisoned {
                                report.poisoned = report.poisoned.saturating_add(1);
                            } else {
                                report.failed = report.failed.saturating_add(1);
                            }
                        } else if record.state == SyncOutboxRecordState::Poisoned {
                            report.poisoned = report.poisoned.saturating_add(1);
                        }
                    }
                }
            }
            state.recompute_ack_watermark();
            Ok(report)
        })
    }

    pub fn mark_delivery_failed(
        &self,
        record_id: &str,
        error: impl Into<String>,
    ) -> std::io::Result<bool> {
        let error = error.into();
        self.with_state(|state| {
            let Some(index) = state
                .records
                .iter()
                .position(|record| record.record_id == record_id)
            else {
                return Ok(false);
            };
            let record = &mut state.records[index];
            let updated = apply_delivery_failure(record, &error, unix_ms()?);
            state.recompute_ack_watermark();
            Ok(updated)
        })
    }

    pub fn claim_ready_records(&self, limit: usize) -> std::io::Result<Vec<SyncOutboxRecord>> {
        let now = unix_ms()?;
        self.with_state(|state| {
            let mut claimed = Vec::new();
            for record in &mut state.records {
                if claimed.len() >= limit {
                    break;
                }
                if record_ready_for_claim(record, now) {
                    record.state = SyncOutboxRecordState::InFlight;
                    record.next_retry_after_unix_ms = None;
                    record.updated_at_unix_ms = now;
                    claimed.push(record.clone());
                }
            }
            Ok(claimed)
        })
    }

    pub fn mark_in_flight(&self, record_id: &str) -> std::io::Result<bool> {
        let now = unix_ms()?;
        self.with_state(|state| {
            let Some(record) = state
                .records
                .iter_mut()
                .find(|record| record.record_id == record_id)
            else {
                return Ok(false);
            };
            if !record_ready_for_claim(record, now) {
                return Ok(false);
            }
            record.state = SyncOutboxRecordState::InFlight;
            record.next_retry_after_unix_ms = None;
            record.updated_at_unix_ms = now;
            Ok(true)
        })
    }

    pub fn ready_records(&self, limit: usize) -> std::io::Result<Vec<SyncOutboxRecord>> {
        self.with_state_readonly(|state| {
            let now = unix_ms()?;
            Ok(state
                .records
                .into_iter()
                .filter(|record| record_ready_for_claim(record, now))
                .take(limit)
                .collect())
        })
    }

    pub fn retry_deferred_now(&self) -> std::io::Result<u64> {
        self.with_state(|state| {
            let now = unix_ms()?;
            let mut changed = 0;
            for record in &mut state.records {
                if record.state == SyncOutboxRecordState::Pending
                    && record.next_retry_after_unix_ms.is_some()
                {
                    changed += 1;
                    record.next_retry_after_unix_ms = None;
                    record.last_error = None;
                    record.poison_kind = None;
                    record.poison_reason = None;
                    record.updated_at_unix_ms = now;
                } else if record.state == SyncOutboxRecordState::InFlight
                    && in_flight_lease_expired(record, now)
                {
                    changed += 1;
                    record.state = SyncOutboxRecordState::Pending;
                    record.next_retry_after_unix_ms = None;
                    record.updated_at_unix_ms = now;
                }
            }
            Ok(changed)
        })
    }

    pub fn repair_retry_exhausted_poison(&self) -> std::io::Result<u64> {
        self.with_state(|state| {
            let now = unix_ms()?;
            let mut repaired = 0;
            for record in &mut state.records {
                if record.state == SyncOutboxRecordState::Poisoned
                    && record.poison_kind == Some(SyncOutboxPoisonKind::RetryExhausted)
                {
                    record.state = SyncOutboxRecordState::Pending;
                    record.attempts = 0;
                    record.next_retry_after_unix_ms = None;
                    record.last_error = None;
                    record.poison_kind = None;
                    record.poison_reason = None;
                    record.updated_at_unix_ms = now;
                    repaired += 1;
                }
            }
            state.recompute_ack_watermark();
            Ok(repaired)
        })
    }

    pub fn status(&self) -> std::io::Result<SyncOutboxStatus> {
        self.with_state_readonly(|state| Ok(status_from_state(&state, unix_ms()?)))
    }

    fn with_state<R>(
        &self,
        f: impl FnOnce(&mut SyncOutboxFile) -> std::io::Result<R>,
    ) -> std::io::Result<R> {
        let path = self.path();
        self.locked(|state| {
            let result = f(state)?;
            state.compact_acked_tail();
            state.compact_skipped_records();
            write_state(&path, state)?;
            // The directory fsync is part of the same durable transaction as
            // the rename. Keep the cross-process lock until it succeeds so no
            // writer can observe and build on a state that has not crossed its
            // own durability boundary yet.
            sync_state_dir(&path)?;
            Ok(result)
        })
    }

    fn with_state_readonly<R>(
        &self,
        f: impl FnOnce(SyncOutboxFile) -> std::io::Result<R>,
    ) -> std::io::Result<R> {
        self.locked(|state| f(state.clone()))
    }

    fn locked<R>(
        &self,
        f: impl FnOnce(&mut SyncOutboxFile) -> std::io::Result<R>,
    ) -> std::io::Result<R> {
        std::fs::create_dir_all(&self.root)?;
        let lock_path = self.root.join("outbox.lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        lock_exclusive_with_timeout(&lock, SYNC_OUTBOX_LOCK_TIMEOUT)?;
        let mut state = read_state(&self.path())?;
        state.normalize();
        let result = f(&mut state);
        let unlock_result = lock.unlock();

        // If the operation succeeded, return success even if unlock fails.
        // The operation already modified in-memory state and attempted disk write.
        // Unlock failure is a resource leak, not a transaction failure.
        match result {
            Ok(value) => {
                if let Err(unlock_err) = unlock_result {
                    tracing::warn!(
                        ?unlock_err,
                        "failed to unlock sync outbox after successful operation"
                    );
                }
                Ok(value)
            }
            Err(error) => {
                // Operation failed, unlock error is secondary
                if let Err(unlock_err) = unlock_result {
                    tracing::warn!(
                        ?unlock_err,
                        "failed to unlock sync outbox after failed operation"
                    );
                }
                Err(error)
            }
        }
    }
}

fn enqueue_candidate(
    state: &mut SyncOutboxFile,
    candidate: SyncOutboxRecord,
    now: u64,
) -> std::io::Result<SyncOutboxEnqueueOutcome> {
    if let Some(index) = state
        .records
        .iter()
        .position(|record| record.record_id == candidate.record_id)
    {
        let existing = &mut state.records[index];
        if existing.payload_hash == candidate.payload_hash {
            return Ok(SyncOutboxEnqueueOutcome::Duplicate {
                record_id: existing.record_id.clone(),
                sequence: existing.sequence,
            });
        }
        existing.state = SyncOutboxRecordState::Poisoned;
        existing.poison_kind = Some(SyncOutboxPoisonKind::PayloadHashMismatch);
        existing.poison_reason = Some(format!(
            "same stable event id has different payload hash: existing={} incoming={}",
            existing.payload_hash, candidate.payload_hash
        ));
        existing.last_error = existing.poison_reason.clone();
        existing.updated_at_unix_ms = now;
        let outcome = SyncOutboxEnqueueOutcome::Poisoned {
            record_id: existing.record_id.clone(),
            sequence: existing.sequence,
        };
        state.recompute_ack_watermark();
        return Ok(outcome);
    }
    if let Some(tombstone) = state
        .acked_tombstones
        .iter()
        .find(|tombstone| tombstone.record_id == candidate.record_id)
    {
        if !tombstone.payload_hash.is_empty() && tombstone.payload_hash == candidate.payload_hash {
            return Ok(SyncOutboxEnqueueOutcome::Duplicate {
                record_id: tombstone.record_id.clone(),
                sequence: tombstone.sequence,
            });
        }

        let mut record = candidate;
        record.sequence = state.next_sequence();
        record.state = SyncOutboxRecordState::Poisoned;
        record.poison_kind = Some(SyncOutboxPoisonKind::PayloadHashMismatch);
        record.poison_reason = Some(if tombstone.payload_hash.is_empty() {
            format!(
                "compacted ack tombstone payload hash missing: record_id={}",
                tombstone.record_id
            )
        } else {
            format!(
                "compacted ack tombstone payload hash mismatch: tombstone={} incoming={}",
                tombstone.payload_hash, record.payload_hash
            )
        });
        record.last_error = record.poison_reason.clone();
        let outcome = SyncOutboxEnqueueOutcome::Poisoned {
            record_id: record.record_id.clone(),
            sequence: record.sequence,
        };
        state.records.push(record);
        state.recompute_ack_watermark();
        return Ok(outcome);
    }

    let mut record = candidate;
    record.sequence = state.next_sequence();
    let outcome = SyncOutboxEnqueueOutcome::Inserted {
        record_id: record.record_id.clone(),
        sequence: record.sequence,
    };
    state.records.push(record);
    state.recompute_ack_watermark();
    Ok(outcome)
}

fn lock_exclusive_with_timeout(lock: &std::fs::File, timeout: Duration) -> std::io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!(
                            "timed out after {}ms waiting for the sync outbox lock",
                            timeout.as_millis()
                        ),
                    ));
                }
                std::thread::sleep(SYNC_OUTBOX_LOCK_POLL_INTERVAL.min(deadline - now));
            }
            Err(error) => return Err(error),
        }
    }
}

impl SyncOutboxFile {
    fn normalize(&mut self) {
        self.schema_version = SYNC_OUTBOX_SCHEMA_VERSION;
        self.records.sort_by_key(|record| record.sequence);
        self.compact_acked_tombstones();
        self.compact_skipped_records();
        self.recompute_ack_watermark();
    }

    fn next_sequence(&self) -> u64 {
        self.records
            .iter()
            .map(|record| record.sequence)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
    }

    fn recompute_ack_watermark(&mut self) {
        let mut watermark = self.ack_watermark;
        if let Some(first_open_sequence) = self
            .records
            .iter()
            .filter(|record| {
                record.sequence > 0
                    && record.sequence <= watermark
                    && !record_is_terminal_for_watermark(record)
            })
            .map(|record| record.sequence)
            .min()
        {
            watermark = first_open_sequence.saturating_sub(1);
        }

        let terminal_sequences: std::collections::HashSet<u64> = self
            .records
            .iter()
            .filter(|record| {
                record.sequence > watermark && record_is_terminal_for_watermark(record)
            })
            .map(|record| record.sequence)
            .chain(
                self.acked_tombstones
                    .iter()
                    .filter(|tombstone| tombstone.sequence > watermark)
                    .map(|tombstone| tombstone.sequence),
            )
            .collect();
        while terminal_sequences.contains(&(watermark + 1)) {
            watermark += 1;
        }
        self.ack_watermark = watermark;
    }

    fn compact_acked_tail(&mut self) {
        self.recompute_ack_watermark();
        let compactable: Vec<u64> = self
            .records
            .iter()
            .filter(|record| {
                record.state == SyncOutboxRecordState::Acked
                    && record.sequence <= self.ack_watermark
            })
            .map(|record| record.sequence)
            .collect();
        let remove_count = compactable
            .len()
            .saturating_sub(SYNC_OUTBOX_ACKED_RETAINED_RECORDS);
        if remove_count == 0 {
            return;
        }
        let cutoff = compactable[remove_count - 1];
        let existing_tombstones: HashSet<String> = self
            .acked_tombstones
            .iter()
            .map(|tombstone| tombstone.record_id.clone())
            .collect();
        let mut new_tombstones = Vec::new();
        let mut new_tombstone_ids = HashSet::new();
        self.records.retain(|record| {
            let remove = record.state == SyncOutboxRecordState::Acked && record.sequence <= cutoff;
            if remove
                && !existing_tombstones.contains(&record.record_id)
                && new_tombstone_ids.insert(record.record_id.clone())
            {
                new_tombstones.push(SyncOutboxAckTombstone {
                    record_id: record.record_id.clone(),
                    sequence: record.sequence,
                    payload_hash: record.payload_hash.clone(),
                });
            }
            !remove
        });
        self.acked_tombstones.extend(new_tombstones);
        self.compact_acked_tombstones();
    }

    fn compact_acked_tombstones(&mut self) {
        self.acked_tombstones
            .sort_by_key(|tombstone| tombstone.sequence);
        let mut seen = HashSet::new();
        let mut deduped = Vec::with_capacity(self.acked_tombstones.len());
        for tombstone in self.acked_tombstones.iter().rev() {
            if seen.insert(tombstone.record_id.clone()) {
                deduped.push(tombstone.clone());
            }
        }
        deduped.reverse();
        self.acked_tombstones = deduped;
        let remove_count = self
            .acked_tombstones
            .len()
            .saturating_sub(SYNC_OUTBOX_ACK_TOMBSTONE_RETAINED_RECORDS);
        if remove_count > 0 {
            self.acked_tombstones.drain(0..remove_count);
        }
    }

    fn compact_skipped_records(&mut self) {
        let remove_count = self
            .skipped_records
            .len()
            .saturating_sub(SYNC_OUTBOX_SKIPPED_RETAINED_RECORDS);
        if remove_count > 0 {
            self.skipped_records.drain(0..remove_count);
        }
    }
}

fn build_record(
    event: &JournalEvent,
    sequence: u64,
    now: u64,
) -> std::io::Result<SyncOutboxRecord> {
    let payload = serde_json::to_value(event).map_err(invalid_data)?;
    let payload_hash = sync_outbox_canonical_payload_hash(&payload);
    let record_id = sync_outbox_stable_event_id(event, &payload_hash)?;
    let event_type = event_type_string(&event.event_type);
    Ok(SyncOutboxRecord {
        schema_version: SYNC_OUTBOX_SCHEMA_VERSION,
        sequence,
        record_id,
        domain: SyncDomain::Events,
        session_id: event.session_id.clone().unwrap_or_default(),
        event_type,
        event_ts: event.ts.clone(),
        payload_hash,
        payload,
        state: SyncOutboxRecordState::Pending,
        attempts: 0,
        next_retry_after_unix_ms: None,
        last_error: None,
        poison_kind: None,
        poison_reason: None,
        created_at_unix_ms: now,
        updated_at_unix_ms: now,
        acked_at_unix_ms: None,
    })
}

pub fn sync_outbox_stable_event_id(
    event: &JournalEvent,
    payload_hash: &str,
) -> std::io::Result<String> {
    let event_type = event_type_string(&event.event_type);
    let identity = serde_json::json!({
        "session_id": event.session_id.as_deref().unwrap_or(""),
        "type": event_type,
        "ts": event.ts,
        "turn": event.turn,
        "agentic_step": event.agentic_step,
        "config_key": event.config_key,
        "trace_id": event.metadata.as_ref().and_then(|m| m.get("trace_id")).cloned(),
    });
    let identity_json = canonical_json_string(&identity);
    let id_input = format!("{identity_json}|payload:{payload_hash}");
    Ok(format!(
        "sync_evt_{}",
        sha256_hex_with_namespace(b"astra-sync-outbox-event-id", id_input.as_bytes())
    ))
}

fn event_type_string(event_type: &JournalEventType) -> String {
    serde_json::to_value(event_type)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| format!("{event_type:?}"))
}

fn read_state(path: &Path) -> std::io::Result<SyncOutboxFile> {
    let mut file = match OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SyncOutboxFile::default());
        }
        Err(error) => return Err(error),
    };
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    if text.trim().is_empty() {
        return Ok(SyncOutboxFile::default());
    }
    serde_json::from_str(&text).map_err(invalid_data)
}

fn write_state(path: &Path, state: &SyncOutboxFile) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("json.tmp.{}", fastrand::u64(..)));
    {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)?;
        let bytes = serde_json::to_vec(state).map_err(invalid_data)?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_data()?;
    }
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(())
}

/// Sync the directory after a write. Callers hold the exclusive outbox lock so
/// the file rename and its durability barrier form one ordered transaction.
fn sync_state_dir(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    OpenOptions::new().read(true).open(parent)?.sync_all()
}

fn status_from_state(state: &SyncOutboxFile, now: u64) -> SyncOutboxStatus {
    let mut status = SyncOutboxStatus {
        total: state.records.len() as u64,
        ack_watermark: state.ack_watermark,
        ack_tombstones: state.acked_tombstones.len() as u64,
        skipped: state.skipped_records.len() as u64,
        last_skipped_reason: state
            .skipped_records
            .last()
            .map(|record| record.reason.clone()),
        last_skipped_event_type: state
            .skipped_records
            .last()
            .map(|record| record.event_type.clone()),
        ..Default::default()
    };
    for record in &state.records {
        match record.state {
            SyncOutboxRecordState::Pending => {
                status.pending += 1;
                status.oldest_pending_created_at_unix_ms = Some(
                    status
                        .oldest_pending_created_at_unix_ms
                        .map_or(record.created_at_unix_ms, |old| {
                            old.min(record.created_at_unix_ms)
                        }),
                );
                if record
                    .next_retry_after_unix_ms
                    .is_some_and(|retry_at| retry_at > now)
                {
                    status.retry_deferred += 1;
                    status.next_retry_after_unix_ms = Some(
                        status
                            .next_retry_after_unix_ms
                            .map_or(record.next_retry_after_unix_ms.unwrap_or(now), |old| {
                                old.min(record.next_retry_after_unix_ms.unwrap_or(now))
                            }),
                    );
                } else {
                    status.ready += 1;
                    status.claimable += 1;
                }
            }
            SyncOutboxRecordState::InFlight => {
                status.in_flight += 1;
                if in_flight_lease_expired(record, now) {
                    status.stale_in_flight += 1;
                    status.claimable += 1;
                }
            }
            SyncOutboxRecordState::Acked => status.acked += 1,
            SyncOutboxRecordState::Poisoned => {
                status.poisoned += 1;
                status.last_error = record
                    .poison_reason
                    .clone()
                    .or_else(|| record.last_error.clone())
                    .or(status.last_error);
            }
        }
        if status.last_error.is_none() {
            status.last_error = record.last_error.clone();
        }
    }
    status.degraded = status.poisoned > 0
        || status.retry_deferred > 0
        || status.stale_in_flight > 0
        || status.skipped > 0;
    status
}

fn in_flight_lease_expired(record: &SyncOutboxRecord, now: u64) -> bool {
    // A persistent lease needs wall-clock timestamps to survive process
    // restart. If the wall clock moves backwards past the recorded claim,
    // prefer an idempotent redelivery over leaving the outbox stuck until the
    // clock catches up. Stable record ids make that the safe at-least-once
    // failure mode.
    now < record.updated_at_unix_ms
        || record
            .updated_at_unix_ms
            .saturating_add(SYNC_OUTBOX_IN_FLIGHT_LEASE_MS)
            <= now
}

fn record_ready_for_claim(record: &SyncOutboxRecord, now: u64) -> bool {
    (record.state == SyncOutboxRecordState::Pending
        && record
            .next_retry_after_unix_ms
            .is_none_or(|retry_at| retry_at <= now))
        || (record.state == SyncOutboxRecordState::InFlight && in_flight_lease_expired(record, now))
}

fn record_is_terminal_for_watermark(record: &SyncOutboxRecord) -> bool {
    matches!(
        record.state,
        SyncOutboxRecordState::Acked | SyncOutboxRecordState::Poisoned
    )
}

fn acknowledge_record(
    record: &mut SyncOutboxRecord,
    payload_hash: Option<&str>,
    now: u64,
) -> SyncOutboxAckOutcome {
    if record.state == SyncOutboxRecordState::Acked {
        return SyncOutboxAckOutcome::DuplicateAck {
            record_id: record.record_id.clone(),
            sequence: record.sequence,
        };
    }
    if let Some(hash) = payload_hash
        && hash != record.payload_hash
    {
        record.state = SyncOutboxRecordState::Poisoned;
        record.poison_kind = Some(SyncOutboxPoisonKind::PayloadHashMismatch);
        record.poison_reason = Some(format!(
            "ack payload hash mismatch: stored={} ack={}",
            record.payload_hash, hash
        ));
        record.last_error = record.poison_reason.clone();
        record.updated_at_unix_ms = now;
        return SyncOutboxAckOutcome::Poisoned {
            record_id: record.record_id.clone(),
            sequence: record.sequence,
        };
    }
    record.state = SyncOutboxRecordState::Acked;
    record.acked_at_unix_ms = Some(now);
    record.updated_at_unix_ms = now;
    record.last_error = None;
    record.next_retry_after_unix_ms = None;
    SyncOutboxAckOutcome::Acked {
        record_id: record.record_id.clone(),
        sequence: record.sequence,
    }
}

fn apply_delivery_failure(record: &mut SyncOutboxRecord, error: &str, now: u64) -> bool {
    if matches!(
        record.state,
        SyncOutboxRecordState::Acked | SyncOutboxRecordState::Poisoned
    ) {
        return false;
    }
    record.attempts = record.attempts.saturating_add(1);
    record.last_error = Some(error.to_string());
    record.updated_at_unix_ms = now;
    if record.attempts >= SYNC_OUTBOX_MAX_ATTEMPTS {
        record.state = SyncOutboxRecordState::Poisoned;
        record.poison_kind = Some(SyncOutboxPoisonKind::RetryExhausted);
        record.poison_reason = Some(format!(
            "delivery failed after {} attempts: {}",
            record.attempts, error
        ));
        record.next_retry_after_unix_ms = None;
    } else {
        record.state = SyncOutboxRecordState::Pending;
        record.next_retry_after_unix_ms =
            Some(now.saturating_add(retry_backoff_ms(record.attempts)));
    }
    true
}

fn retry_backoff_ms(attempts: u32) -> u64 {
    let shift = attempts.saturating_sub(1).min(6);
    1_000_u64.saturating_mul(1_u64 << shift)
}

fn unix_ms() -> std::io::Result<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("system clock is before UNIX epoch: {error}"),
            )
        })
}

fn sha256_prefixed(bytes: &[u8]) -> String {
    format!("sha256:{}", sha256_hex_with_namespace(b"", bytes))
}

fn sha256_hex_with_namespace(namespace: &[u8], bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    if !namespace.is_empty() {
        hasher.update((namespace.len() as u64).to_be_bytes());
        hasher.update(namespace);
    }
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn invalid_data(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_journal::JournalDirGuard;

    fn test_store() -> (tempfile::TempDir, JournalDirGuard, SyncOutboxStore) {
        let temp = tempfile::tempdir().expect("tempdir");
        let guard = JournalDirGuard::new(temp.path());
        let store = SyncOutboxStore::local();
        (temp, guard, store)
    }

    fn event(value: &str) -> JournalEvent {
        let mut event = JournalEvent::config_change(Some("sess-1"), "model", value);
        event.ts = "2026-07-08T00:00:00Z".to_string();
        event
    }

    #[test]
    fn store_lock_contention_times_out_and_recovers_after_release() {
        let (_temp, _guard, store) = test_store();
        std::fs::create_dir_all(&store.root).expect("create outbox root");
        let lock_path = store.root.join("outbox.lock");
        let held_lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("open lock handle");
        held_lock.try_lock_exclusive().expect("acquire held lock");

        let error = store
            .status()
            .expect_err("contended store lock must time out");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);

        held_lock.unlock().expect("release held lock");
        let status = store.status().expect("store must recover after release");
        assert_eq!(status.total, 0);
    }

    #[test]
    fn directory_sync_open_failure_is_reported() {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing_path = temp.path().join("missing").join("outbox.json");
        let error = sync_state_dir(&missing_path).expect_err("missing parent must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn enqueue_is_durable_and_idempotent_by_stable_event_id_and_payload_hash() {
        let (_temp, _guard, store) = test_store();
        let first = event("gpt-5");
        let inserted = store.enqueue_journal_event(&first).expect("enqueue");
        let (record_id, sequence) = match inserted {
            SyncOutboxEnqueueOutcome::Inserted {
                record_id,
                sequence,
            } => (record_id, sequence),
            other => panic!("expected insert, got {other:?}"),
        };
        assert_eq!(sequence, 1);

        let duplicate = store.enqueue_journal_event(&first).expect("dedupe");
        assert_eq!(
            duplicate,
            SyncOutboxEnqueueOutcome::Duplicate {
                record_id: record_id.clone(),
                sequence
            }
        );

        let reloaded = SyncOutboxStore::local();
        let status = reloaded.status().expect("status");
        assert_eq!(status.pending, 1);
        assert_eq!(status.ready, 1);
        assert_eq!(status.ack_watermark, 0);
    }

    #[test]
    fn batch_enqueue_preserves_event_order_and_single_event_deduplication() {
        let (_temp, _guard, store) = test_store();
        let first = event("first");
        let mut second = event("second");
        second.ts = "2026-07-08T00:00:01Z".to_string();

        let outcomes = store
            .enqueue_journal_events(&[first.clone(), second.clone(), first.clone()])
            .expect("batch enqueue");

        assert!(matches!(
            outcomes.as_slice(),
            [
                SyncOutboxEnqueueOutcome::Inserted { sequence: 1, .. },
                SyncOutboxEnqueueOutcome::Inserted { sequence: 2, .. },
                SyncOutboxEnqueueOutcome::Duplicate { sequence: 1, .. },
            ]
        ));
        let status = store.status().expect("status after batch enqueue");
        assert_eq!(status.total, 2);
        assert_eq!(status.pending, 2);
    }

    #[test]
    fn journal_delta_projection_advances_source_cursor_atomically() {
        let (_temp, _guard, store) = test_store();
        let first = event("first");
        let mut second = event("second");
        second.ts = "2026-07-08T00:00:01Z".to_string();

        let outcome = store
            .append_journal_delta("session-a", 0, 256, &[first.clone(), second.clone()])
            .expect("append journal delta");
        assert!(matches!(
            outcome,
            SyncOutboxJournalDeltaOutcome::Appended {
                scanned_events: 2,
                inserted: 2,
                duplicates: 0,
                poisoned: 0,
                skipped: 0,
            }
        ));
        assert_eq!(
            store
                .journal_source_offset("session-a")
                .expect("source offset"),
            256
        );

        let stale = store
            .append_journal_delta("session-a", 0, 512, &[first, second])
            .expect("stale source offset is a normal outcome");
        assert_eq!(
            stale,
            SyncOutboxJournalDeltaOutcome::StaleSourceOffset { actual_offset: 256 }
        );
        assert_eq!(store.status().expect("status").pending, 2);
    }

    #[test]
    fn enqueue_write_failure_does_not_commit_or_report_success() {
        let (_temp, _guard, store) = test_store();
        std::fs::create_dir_all(store.path()).expect("block outbox file path");

        let error = store
            .enqueue_journal_event(&event("write-fails"))
            .expect_err("durable write failure must be reported");
        assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::AlreadyExists
                    | std::io::ErrorKind::IsADirectory
                    | std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::Other
            ),
            "unexpected write failure kind: {error:?}"
        );

        std::fs::remove_dir_all(store.path()).expect("remove blocked outbox path");
        let status = store.status().expect("status after failed write");
        assert_eq!(status.total, 0);
        assert_eq!(status.pending, 0);
    }

    #[test]
    fn ack_watermark_advances_only_contiguously() {
        let (_temp, _guard, store) = test_store();
        let mut first = event("one");
        let mut second = event("two");
        second.ts = "2026-07-08T00:00:01Z".to_string();
        let SyncOutboxEnqueueOutcome::Inserted {
            record_id: first_id,
            ..
        } = store.enqueue_journal_event(&first).expect("first")
        else {
            panic!("first insert");
        };
        first.ts = "2026-07-08T00:00:02Z".to_string();
        let SyncOutboxEnqueueOutcome::Inserted {
            record_id: second_id,
            ..
        } = store.enqueue_journal_event(&second).expect("second")
        else {
            panic!("second insert");
        };

        store
            .acknowledge(&second_id, None)
            .expect("ack second first");
        assert_eq!(store.status().expect("status").ack_watermark, 0);
        store.acknowledge(&first_id, None).expect("ack first");
        assert_eq!(store.status().expect("status").ack_watermark, 2);
    }

    #[test]
    fn same_weak_identity_with_different_payload_gets_distinct_record_id() {
        let (_temp, _guard, store) = test_store();
        let first = event("gpt-5");
        let mut changed = event("gpt-5-mini");
        changed.ts = first.ts.clone();

        let first_outcome = store.enqueue_journal_event(&first).expect("enqueue first");
        let outcome = store
            .enqueue_journal_event(&changed)
            .expect("enqueue changed");
        let SyncOutboxEnqueueOutcome::Inserted {
            record_id: first_id,
            ..
        } = first_outcome
        else {
            panic!("first insert");
        };
        let SyncOutboxEnqueueOutcome::Inserted {
            record_id: changed_id,
            ..
        } = outcome
        else {
            panic!("changed insert");
        };
        assert_ne!(
            first_id, changed_id,
            "local journal entries do not have a durable offset/id, so payload hash must disambiguate otherwise identical event identities"
        );

        let status = store.status().expect("status");
        assert_eq!(status.total, 2);
        assert_eq!(status.pending, 2);
        assert_eq!(status.poisoned, 0);
        assert!(!status.degraded);
    }

    #[test]
    fn retry_backoff_and_repair_isolate_poison_records() {
        let (_temp, _guard, store) = test_store();
        let SyncOutboxEnqueueOutcome::Inserted { record_id, .. } = store
            .enqueue_journal_event(&event("gpt-5"))
            .expect("enqueue")
        else {
            panic!("insert");
        };

        store
            .mark_delivery_failed(&record_id, "offline")
            .expect("fail once");
        let status = store.status().expect("status");
        assert_eq!(status.pending, 1);
        assert_eq!(status.retry_deferred, 1);
        assert!(status.degraded);

        for _ in 1..SYNC_OUTBOX_MAX_ATTEMPTS {
            store
                .mark_delivery_failed(&record_id, "offline")
                .expect("fail until poison");
        }
        let poisoned = store.status().expect("poisoned");
        assert_eq!(poisoned.poisoned, 1);
        assert_eq!(poisoned.pending, 0);

        assert_eq!(store.repair_retry_exhausted_poison().expect("repair"), 1);
        let repaired = store.status().expect("repaired");
        assert_eq!(repaired.pending, 1);
        assert_eq!(repaired.poisoned, 0);
        assert_eq!(
            repaired.ack_watermark, 0,
            "repair reopens the record, so the terminal-prefix watermark must move back"
        );
        assert!(!repaired.degraded);
    }

    #[test]
    fn ready_records_excludes_deferred_and_in_flight_records() {
        let (_temp, _guard, store) = test_store();
        let mut first = event("ready");
        let mut second = event("deferred");
        second.ts = "2026-07-08T00:00:01Z".to_string();
        let mut third = event("in-flight");
        third.ts = "2026-07-08T00:00:02Z".to_string();

        let SyncOutboxEnqueueOutcome::Inserted {
            record_id: ready_id,
            ..
        } = store.enqueue_journal_event(&first).expect("ready")
        else {
            panic!("ready insert");
        };
        first.ts = "2026-07-08T00:00:03Z".to_string();
        let SyncOutboxEnqueueOutcome::Inserted {
            record_id: deferred_id,
            ..
        } = store.enqueue_journal_event(&second).expect("deferred")
        else {
            panic!("deferred insert");
        };
        let SyncOutboxEnqueueOutcome::Inserted {
            record_id: in_flight_id,
            ..
        } = store.enqueue_journal_event(&third).expect("in-flight")
        else {
            panic!("in-flight insert");
        };

        store
            .mark_delivery_failed(&deferred_id, "offline")
            .expect("defer");
        assert!(store.mark_in_flight(&in_flight_id).expect("mark"));

        let ready = store.ready_records(10).expect("ready records");
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].record_id, ready_id);
    }

    #[test]
    fn stale_in_flight_records_are_reclaimed_by_ready_scan() {
        let (_temp, _guard, store) = test_store();
        let SyncOutboxEnqueueOutcome::Inserted { record_id, .. } = store
            .enqueue_journal_event(&event("stale"))
            .expect("enqueue")
        else {
            panic!("insert");
        };
        assert!(store.mark_in_flight(&record_id).expect("mark"));

        store
            .with_state(|state| {
                let record = state
                    .records
                    .iter_mut()
                    .find(|record| record.record_id == record_id)
                    .expect("record");
                record.updated_at_unix_ms = unix_ms()
                    .expect("time")
                    .saturating_sub(SYNC_OUTBOX_IN_FLIGHT_LEASE_MS)
                    .saturating_sub(1);
                Ok(())
            })
            .expect("stale record");

        let status = store.status().expect("status");
        assert_eq!(status.in_flight, 1);
        assert_eq!(status.stale_in_flight, 1);
        assert_eq!(status.claimable, 1);
        assert!(status.degraded);

        let ready = store.ready_records(10).expect("ready");
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].record_id, record_id);
        assert!(store.mark_in_flight(&record_id).expect("reclaim"));
    }

    #[test]
    fn wall_clock_rollback_reclaims_in_flight_record_for_idempotent_redelivery() {
        let (_temp, _guard, store) = test_store();
        let SyncOutboxEnqueueOutcome::Inserted { record_id, .. } = store
            .enqueue_journal_event(&event("clock-rollback"))
            .expect("enqueue")
        else {
            panic!("insert");
        };
        assert!(store.mark_in_flight(&record_id).expect("mark"));

        let observed_now = unix_ms().expect("time");
        store
            .with_state(|state| {
                let record = state
                    .records
                    .iter_mut()
                    .find(|record| record.record_id == record_id)
                    .expect("record");
                record.updated_at_unix_ms = observed_now.saturating_add(60_000);
                Ok(())
            })
            .expect("future-dated claim");

        let status = store.status().expect("status");
        assert_eq!(status.stale_in_flight, 1);
        let ready = store.ready_records(10).expect("ready");
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].record_id, record_id);
    }

    #[test]
    fn retry_deferred_now_does_not_steal_active_in_flight_delivery() {
        let (_temp, _guard, store) = test_store();
        let SyncOutboxEnqueueOutcome::Inserted { record_id, .. } = store
            .enqueue_journal_event(&event("active-in-flight"))
            .expect("enqueue")
        else {
            panic!("insert");
        };
        assert!(store.mark_in_flight(&record_id).expect("mark"));

        assert_eq!(store.retry_deferred_now().expect("retry"), 0);
        let status = store.status().expect("status");
        assert_eq!(status.in_flight, 1);
        assert_eq!(status.stale_in_flight, 0);
        assert_eq!(status.ready, 0);
        assert!(store.ready_records(10).expect("ready").is_empty());
    }

    #[test]
    fn retry_deferred_now_recovers_stale_in_flight_delivery() {
        let (_temp, _guard, store) = test_store();
        let SyncOutboxEnqueueOutcome::Inserted { record_id, .. } = store
            .enqueue_journal_event(&event("stale-in-flight"))
            .expect("enqueue")
        else {
            panic!("insert");
        };
        assert!(store.mark_in_flight(&record_id).expect("mark"));
        store
            .with_state(|state| {
                let record = state
                    .records
                    .iter_mut()
                    .find(|record| record.record_id == record_id)
                    .expect("record");
                record.updated_at_unix_ms = unix_ms()
                    .expect("time")
                    .saturating_sub(SYNC_OUTBOX_IN_FLIGHT_LEASE_MS)
                    .saturating_sub(1);
                Ok(())
            })
            .expect("stale record");

        assert_eq!(store.retry_deferred_now().expect("retry"), 1);
        let status = store.status().expect("status");
        assert_eq!(status.in_flight, 0);
        assert_eq!(status.pending, 1);
        assert_eq!(status.ready, 1);
    }

    #[test]
    fn claim_ready_records_marks_batch_in_flight() {
        let (_temp, _guard, store) = test_store();
        for value in ["one", "two"] {
            store.enqueue_journal_event(&event(value)).expect("enqueue");
        }

        let claimed = store.claim_ready_records(10).expect("claim");
        assert_eq!(claimed.len(), 2);
        let status = store.status().expect("status");
        assert_eq!(status.in_flight, 2);
        assert_eq!(status.ready, 0);
        assert!(
            store
                .claim_ready_records(10)
                .expect("claim again")
                .is_empty()
        );
    }

    #[test]
    fn settle_delivery_batch_applies_ack_and_retry_in_one_transition() {
        let (_temp, _guard, store) = test_store();
        let SyncOutboxEnqueueOutcome::Inserted {
            record_id: ack_id, ..
        } = store
            .enqueue_journal_event(&event("ack"))
            .expect("ack enqueue")
        else {
            panic!("ack insert");
        };
        let SyncOutboxEnqueueOutcome::Inserted {
            record_id: fail_id, ..
        } = store
            .enqueue_journal_event(&event("fail"))
            .expect("fail enqueue")
        else {
            panic!("fail insert");
        };
        let claimed = store.claim_ready_records(10).expect("claim");
        assert_eq!(claimed.len(), 2);
        let ack_hash = claimed
            .iter()
            .find(|record| record.record_id == ack_id)
            .expect("claimed ack")
            .payload_hash
            .clone();

        let report = store
            .settle_delivery_batch(&[
                SyncOutboxDeliverySettlement::Ack {
                    record_id: ack_id,
                    payload_hash: ack_hash,
                },
                SyncOutboxDeliverySettlement::Failed {
                    record_id: fail_id,
                    error: "temporary outage".to_string(),
                },
            ])
            .expect("settle");

        assert_eq!(report.acked, 1);
        assert_eq!(report.failed, 1);
        let status = store.status().expect("status");
        assert_eq!(status.acked, 1);
        assert_eq!(status.pending, 1);
        assert_eq!(status.retry_deferred, 1);
    }

    #[test]
    fn settle_delivery_batch_isolates_record_failure_without_replaying_sibling_ack() {
        let (_temp, _guard, store) = test_store();
        let SyncOutboxEnqueueOutcome::Inserted {
            record_id: ack_id, ..
        } = store
            .enqueue_journal_event(&event("ack-stays-acked"))
            .expect("ack enqueue")
        else {
            panic!("ack insert");
        };
        let SyncOutboxEnqueueOutcome::Inserted {
            record_id: fail_id, ..
        } = store
            .enqueue_journal_event(&event("failure-isolated"))
            .expect("fail enqueue")
        else {
            panic!("fail insert");
        };
        let claimed = store.claim_ready_records(10).expect("claim");
        assert_eq!(claimed.len(), 2);
        let ack_hash = claimed
            .iter()
            .find(|record| record.record_id == ack_id)
            .expect("claimed ack")
            .payload_hash
            .clone();

        for _ in 1..SYNC_OUTBOX_MAX_ATTEMPTS {
            store
                .mark_delivery_failed(&fail_id, "previous temporary outage")
                .expect("preload retry attempts");
        }

        let report = store
            .settle_delivery_batch(&[
                SyncOutboxDeliverySettlement::Ack {
                    record_id: ack_id,
                    payload_hash: ack_hash,
                },
                SyncOutboxDeliverySettlement::Failed {
                    record_id: fail_id,
                    error: "final temporary outage".to_string(),
                },
            ])
            .expect("settle");

        assert_eq!(report.acked, 1);
        assert_eq!(report.failed, 0);
        assert_eq!(report.poisoned, 1);
        let status = store.status().expect("status");
        assert_eq!(status.acked, 1);
        assert_eq!(status.poisoned, 1);
        assert_eq!(
            status.ack_watermark, 2,
            "a sibling failure must not cause the already-settled ack to replay"
        );
    }

    #[test]
    fn poisoned_gap_does_not_block_terminal_watermark() {
        let (_temp, _guard, store) = test_store();
        let SyncOutboxEnqueueOutcome::Inserted {
            record_id: poisoned_id,
            ..
        } = store
            .enqueue_journal_event(&event("poisoned"))
            .expect("poison enqueue")
        else {
            panic!("poison insert");
        };
        let SyncOutboxEnqueueOutcome::Inserted {
            record_id: acked_id,
            ..
        } = store
            .enqueue_journal_event(&event("acked-after-poison"))
            .expect("ack enqueue")
        else {
            panic!("ack insert");
        };

        for _ in 0..SYNC_OUTBOX_MAX_ATTEMPTS {
            store
                .mark_delivery_failed(&poisoned_id, "permanent failure")
                .expect("fail until poison");
        }
        store.acknowledge(&acked_id, None).expect("ack second");

        let status = store.status().expect("status");
        assert_eq!(status.poisoned, 1);
        assert_eq!(
            status.ack_watermark, 2,
            "terminal poison isolation must not block later acked records"
        );
    }

    #[test]
    fn repairing_poisoned_prefix_recomputes_watermark_from_open_record() {
        let (_temp, _guard, store) = test_store();
        let SyncOutboxEnqueueOutcome::Inserted {
            record_id: poisoned_id,
            ..
        } = store
            .enqueue_journal_event(&event("poisoned"))
            .expect("poison enqueue")
        else {
            panic!("poison insert");
        };
        let SyncOutboxEnqueueOutcome::Inserted {
            record_id: acked_id,
            ..
        } = store
            .enqueue_journal_event(&event("acked-after-poison"))
            .expect("ack enqueue")
        else {
            panic!("ack insert");
        };

        for _ in 0..SYNC_OUTBOX_MAX_ATTEMPTS {
            store
                .mark_delivery_failed(&poisoned_id, "permanent failure")
                .expect("fail until poison");
        }
        store.acknowledge(&acked_id, None).expect("ack second");
        assert_eq!(store.status().expect("poisoned").ack_watermark, 2);

        assert_eq!(store.repair_retry_exhausted_poison().expect("repair"), 1);
        let repaired = store.status().expect("repaired");
        assert_eq!(repaired.pending, 1);
        assert_eq!(repaired.acked, 1);
        assert_eq!(
            repaired.ack_watermark, 0,
            "a repaired prefix record invalidates the contiguous terminal prefix even when later records remain acked"
        );
    }

    #[test]
    fn acked_compaction_preserves_watermark_and_bounds_tail() {
        let (_temp, _guard, store) = test_store();
        let total = SYNC_OUTBOX_ACKED_RETAINED_RECORDS + 3;
        let mut ids = Vec::new();
        let mut first_event = None;
        for index in 0..total {
            let mut item = event(&format!("value-{index}"));
            item.ts = format!("2026-07-08T00:{:02}:{:02}Z", index / 60, index % 60);
            if index == 0 {
                first_event = Some(item.clone());
            }
            let SyncOutboxEnqueueOutcome::Inserted { record_id, .. } =
                store.enqueue_journal_event(&item).expect("enqueue")
            else {
                panic!("insert");
            };
            ids.push(record_id);
        }

        for record_id in &ids {
            store.acknowledge(record_id, None).expect("acknowledge");
        }

        let status = store.status().expect("status");
        assert_eq!(status.ack_watermark, total as u64);
        assert_eq!(status.acked, SYNC_OUTBOX_ACKED_RETAINED_RECORDS as u64);
        assert_eq!(status.total, SYNC_OUTBOX_ACKED_RETAINED_RECORDS as u64);
        assert_eq!(
            status.ack_tombstones,
            (total - SYNC_OUTBOX_ACKED_RETAINED_RECORDS) as u64
        );
        let tombstones = store
            .with_state_readonly(|state| Ok(state.acked_tombstones))
            .expect("tombstones");
        assert_eq!(tombstones.len(), total - SYNC_OUTBOX_ACKED_RETAINED_RECORDS);
        assert!(
            tombstones
                .iter()
                .all(|tombstone| !tombstone.payload_hash.is_empty())
        );

        let replay = store
            .enqueue_journal_event(&first_event.expect("first event"))
            .expect("replay compacted ack");
        assert_eq!(
            replay,
            SyncOutboxEnqueueOutcome::Duplicate {
                record_id: ids[0].clone(),
                sequence: 1,
            }
        );
        let replay_status = store.status().expect("replay status");
        assert_eq!(
            replay_status.total,
            SYNC_OUTBOX_ACKED_RETAINED_RECORDS as u64
        );
        assert_eq!(replay_status.pending, 0);
    }

    #[test]
    fn skipped_local_only_event_is_durable_diagnostic_not_cloud_record() {
        let (_temp, _guard, store) = test_store();
        let mut missing_session = event("local-only");
        missing_session.session_id = None;

        store
            .record_skipped_journal_event(
                &missing_session,
                SyncOutboxSkipKind::MissingSessionId,
                "journal event has no session_id and cannot be delivered to /events",
            )
            .expect("record skipped");

        let status = store.status().expect("status");
        assert_eq!(status.total, 0);
        assert_eq!(status.pending, 0);
        assert_eq!(status.skipped, 1);
        assert_eq!(
            status.last_skipped_event_type.as_deref(),
            Some("config_change")
        );
        assert!(status.degraded);
    }

    #[test]
    fn compacted_tombstone_payload_hash_mismatch_is_poisoned() {
        let (_temp, _guard, store) = test_store();
        let item = event("future-migration-collision");
        let candidate = build_record(&item, 0, unix_ms().expect("time")).expect("candidate");
        store
            .with_state(|state| {
                state.acked_tombstones.push(SyncOutboxAckTombstone {
                    record_id: candidate.record_id.clone(),
                    sequence: 1,
                    payload_hash: "sha256:different".to_string(),
                });
                Ok(())
            })
            .expect("seed tombstone");

        let outcome = store.enqueue_journal_event(&item).expect("enqueue");
        assert!(matches!(outcome, SyncOutboxEnqueueOutcome::Poisoned { .. }));
        let status = store.status().expect("status");
        assert_eq!(status.poisoned, 1);
        assert!(status.degraded);
        assert!(
            status
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("tombstone payload hash mismatch"))
        );
    }

    #[test]
    fn compacted_tombstone_missing_payload_hash_is_poisoned() {
        let (_temp, _guard, store) = test_store();
        let item = event("legacy-tombstone-collision");
        let candidate = build_record(&item, 0, unix_ms().expect("time")).expect("candidate");
        store
            .with_state(|state| {
                state.acked_tombstones.push(SyncOutboxAckTombstone {
                    record_id: candidate.record_id.clone(),
                    sequence: 1,
                    payload_hash: String::new(),
                });
                Ok(())
            })
            .expect("seed tombstone");

        let outcome = store.enqueue_journal_event(&item).expect("enqueue");
        assert!(matches!(outcome, SyncOutboxEnqueueOutcome::Poisoned { .. }));
        let status = store.status().expect("status");
        assert_eq!(status.poisoned, 1);
        assert!(status.degraded);
        assert!(
            status
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("tombstone payload hash missing"))
        );
    }
}
