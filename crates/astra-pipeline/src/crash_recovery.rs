//! Restore a local checkpoint and classify uncertain tool effects.
//!
//! Recovery does not execute tools or grant replay authority. The checkpoint
//! owner validates the restored state; the strict journal window supplies
//! completed-result audit and identifies side effects needing confirmation.

use crate::step_checkpoint::{FileBackedEventStore, read_latest_heavy_checkpoint};
use crate::step_protocol::{CachedToolResult, StepEvent, StepEventType};
use crate::step_restore::{RestoreError, RestoredSession, restore_checkpoint};
use astra_turn_types::{ToolIdempotency, classify_tool_idempotency};
use std::collections::HashMap;

#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("Corrupted checkpoint: {0}")]
    CorruptedCheckpoint(String),
    #[error("Protocol version mismatch: expected {expected}, found {found}")]
    VersionMismatch { expected: u32, found: u32 },
    #[error("Journal gap: expected event after {expected_after}, found at {found_at}")]
    JournalGap { expected_after: u64, found_at: u64 },
    #[error("Journal read failed: {0}")]
    JournalRead(String),
}

#[derive(Debug)]
pub enum RecoveryOutcome {
    AutoRecovered {
        restored: RestoredSession,
    },
    RequiresUserInput {
        /// Accepting uncertainty continues the conversation; it does not
        /// execute or skip any of these tools.
        pending_decisions: Vec<(String, String)>,
        restored: RestoredSession,
    },
}

/// Restore the latest owner-scoped checkpoint, if one exists. Journal damage
/// fails before any uncertainty can be accepted. This reads each input once
/// and retains no recovery lifecycle or executable result cache.
pub fn recover_from_crash(
    user_id: &str,
    session_id: &str,
) -> Result<Option<RecoveryOutcome>, RecoveryError> {
    let Some(heavy) = read_latest_heavy_checkpoint(user_id, session_id)
        .map_err(|error| RecoveryError::CorruptedCheckpoint(error.to_string()))?
    else {
        return Ok(None);
    };
    let created_at = heavy.light.created_at;
    let mut restored = restore_checkpoint(session_id, heavy).map_err(|error| match error {
        RestoreError::VersionMismatch {
            checkpoint_version,
            current_version,
        } => RecoveryError::VersionMismatch {
            expected: current_version,
            found: checkpoint_version,
        },
        error => RecoveryError::CorruptedCheckpoint(error.to_string()),
    })?;
    let events =
        FileBackedEventStore::load_events_created_at_or_after(user_id, session_id, created_at)
            .map_err(|error| RecoveryError::JournalRead(error.to_string()))?;
    if let Some(error) = detect_journal_gap(&events) {
        return Err(error);
    }
    let records = extract_tool_calls(&events);
    let pending_decisions: Vec<_> = records
        .iter()
        .filter_map(|record| {
            if classify_tool(&record.tool_name) != ToolSafetyClass::SideEffect {
                return None;
            }
            let reason = match record.status {
                ToolCallStatus::StartedOnly | ToolCallStatus::Failed => format!(
                    "Tool '{}' may have produced effects before interruption",
                    record.tool_name
                ),
                ToolCallStatus::Completed if record.cached_result.is_none() => format!(
                    "Tool '{}' completed but its result is unavailable",
                    record.tool_name
                ),
                _ => return None,
            };
            Some((record.tool_name.clone(), reason))
        })
        .collect();
    restored.cache_restore_report.events_examined = events.len();
    for record in records {
        if let Some(cached) = record.cached_result {
            if let Some(key) = record.idempotency_key.as_deref() {
                restored.cache_restore_report.rejected_unverified_entries += 1;
                if crate::step_protocol::persisted_cache_key_is_context_bound(key) {
                    restored.cache_restore_report.rejected_context_bound_entries += 1;
                }
            }
            restored
                .completed_tool_results
                .entry(record.tool_name)
                .or_default()
                .push(cached.output);
        }
    }
    Ok(Some(if pending_decisions.is_empty() {
        RecoveryOutcome::AutoRecovered { restored }
    } else {
        RecoveryOutcome::RequiresUserInput {
            pending_decisions,
            restored,
        }
    }))
}

#[derive(Debug, Clone, PartialEq)]
struct ToolCallRecord {
    step_id: String,
    tool_name: String,
    tool_index: u32,
    idempotency_key: Option<String>,
    status: ToolCallStatus,
    cached_result: Option<CachedToolResult>,
}

#[derive(Debug, Clone, PartialEq)]
enum ToolCallStatus {
    /// Tool call was started but no completion event found.
    StartedOnly,
    /// Tool call completed successfully.
    Completed,
    /// Tool call failed.
    Failed,
    /// Tool call was skipped (cached result used).
    Skipped,
}

#[derive(Debug, Clone, PartialEq)]
enum ToolSafetyClass {
    /// Pure read — always safe to replay (e.g., read_file, grep, glob).
    PureRead,
    /// Idempotent write — safe to replay (e.g., write_file with same content).
    IdempotentWrite,
    /// Non-idempotent side effect — unsafe to replay (e.g., bash with mutations).
    SideEffect,
}

fn classify_tool(tool_name: &str) -> ToolSafetyClass {
    // Normalize to lowercase for case-insensitive matching
    let normalized = tool_name.to_lowercase();
    match classify_tool_idempotency(&normalized, None) {
        ToolIdempotency::PureRead => ToolSafetyClass::PureRead,
        ToolIdempotency::IdempotentWrite => ToolSafetyClass::IdempotentWrite,
        ToolIdempotency::NonIdempotent => ToolSafetyClass::SideEffect,
    }
}

fn extract_tool_info_from_event(event: &StepEvent) -> Option<(String, u32)> {
    let payload = event.payload.as_ref()?;
    let tool_name = payload.get("tool_name")?.as_str()?.to_string();
    let tool_index = payload
        .get("tool_index")
        .or_else(|| payload.get("slot_index"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    Some((tool_name, tool_index))
}

fn tool_call_correlation_key(event: &StepEvent, tool_name: &str, tool_index: u32) -> String {
    if let Some(call_id) = event
        .payload
        .as_ref()
        .and_then(|payload| payload.get("call_id"))
        .and_then(serde_json::Value::as_str)
        .filter(|call_id| !call_id.is_empty())
    {
        return format!("step:{}:call:{call_id}", event.step_id);
    }
    format!("legacy:{}:{tool_name}:{tool_index}", event.step_id)
}

fn extract_idempotency_key_from_event(event: &StepEvent) -> Option<String> {
    event
        .payload
        .as_ref()?
        .get("idempotency_key")?
        .as_str()
        .filter(|key| !key.is_empty())
        .map(ToString::to_string)
}

fn extract_cached_result_from_event(
    event: &StepEvent,
    tool_name: &str,
) -> Option<CachedToolResult> {
    let payload = event.payload.as_ref()?;
    let output = payload
        .get("result")
        .or_else(|| payload.get("output"))
        .and_then(|v| v.as_str())?;

    Some(CachedToolResult {
        tool_name: tool_name.to_string(),
        output: output.to_string(),
        is_error: payload
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        cached_at: event.created_at,
        context_signature: None,
    })
}

fn detect_journal_gap(events: &[StepEvent]) -> Option<RecoveryError> {
    if events.len() < 2 {
        return None;
    }

    /// NTP rollback tolerance — small clock corrections (<5s) are normal.
    const NTP_TOLERANCE_MS: u64 = 5_000;

    // Check for out-of-order timestamps (with NTP tolerance)
    for window in events.windows(2) {
        let prev = &window[0];
        let curr = &window[1];
        if curr.created_at < prev.created_at {
            let rollback = prev.created_at - curr.created_at;
            if rollback > NTP_TOLERANCE_MS {
                return Some(RecoveryError::JournalGap {
                    expected_after: prev.created_at,
                    found_at: curr.created_at,
                });
            }
            tracing::warn!(
                rollback_ms = rollback,
                "Small NTP rollback detected in journal, tolerating"
            );
        }
    }

    // Check for large timestamp gaps (> 5 minutes between events is suspicious)
    const MAX_GAP_MS: u64 = 300_000;
    for window in events.windows(2) {
        let prev = &window[0];
        let curr = &window[1];
        let gap = curr.created_at.saturating_sub(prev.created_at);
        if gap > MAX_GAP_MS {
            return Some(RecoveryError::JournalGap {
                expected_after: prev.created_at,
                found_at: curr.created_at,
            });
        }
    }

    None
}

fn extract_tool_calls(events: &[StepEvent]) -> Vec<ToolCallRecord> {
    // Prefer the executor-issued call id. Step/slot is retained only for
    // legacy events: concurrent runs may share a session but never a
    // logical tool-call identity.
    let mut started: HashMap<String, ToolCallRecord> = HashMap::new();
    let mut completed: Vec<ToolCallRecord> = Vec::new();

    for event in events {
        match &event.event_type {
            StepEventType::ToolCallStarted => {
                if let Some((tool_name, tool_index)) = extract_tool_info_from_event(event) {
                    let key = tool_call_correlation_key(event, &tool_name, tool_index);
                    started.insert(
                        key,
                        ToolCallRecord {
                            step_id: event.step_id.clone(),
                            tool_name,
                            tool_index,
                            idempotency_key: extract_idempotency_key_from_event(event),
                            status: ToolCallStatus::StartedOnly,
                            cached_result: None,
                        },
                    );
                }
            }
            StepEventType::ToolCallCompleted => {
                if let Some((tool_name, tool_index)) = extract_tool_info_from_event(event) {
                    let key = tool_call_correlation_key(event, &tool_name, tool_index);
                    if let Some(mut record) = started.remove(&key) {
                        record.status = ToolCallStatus::Completed;
                        if record.idempotency_key.is_none() {
                            record.idempotency_key = extract_idempotency_key_from_event(event);
                        }
                        record.cached_result = extract_cached_result_from_event(event, &tool_name);
                        completed.push(record);
                    } else {
                        // Completed without start — orphan event
                        let cached_result = extract_cached_result_from_event(event, &tool_name);
                        completed.push(ToolCallRecord {
                            step_id: event.step_id.clone(),
                            tool_name,
                            tool_index,
                            idempotency_key: extract_idempotency_key_from_event(event),
                            status: ToolCallStatus::Completed,
                            cached_result,
                        });
                    }
                }
            }
            StepEventType::ToolCallFailed => {
                if let Some((tool_name, tool_index)) = extract_tool_info_from_event(event) {
                    let key = tool_call_correlation_key(event, &tool_name, tool_index);
                    if let Some(mut record) = started.remove(&key) {
                        record.status = ToolCallStatus::Failed;
                        if record.idempotency_key.is_none() {
                            record.idempotency_key = extract_idempotency_key_from_event(event);
                        }
                        completed.push(record);
                    }
                }
            }
            StepEventType::ToolCallSkipped => {
                if let Some((tool_name, tool_index)) = extract_tool_info_from_event(event) {
                    let key = tool_call_correlation_key(event, &tool_name, tool_index);
                    if let Some(mut record) = started.remove(&key) {
                        record.status = ToolCallStatus::Skipped;
                        if record.idempotency_key.is_none() {
                            record.idempotency_key = extract_idempotency_key_from_event(event);
                        }
                        completed.push(record);
                    }
                }
            }
            _ => {}
        }
    }

    // Remaining started entries were in-flight at crash time
    for (_, record) in started {
        completed.push(record);
    }

    completed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tool_event(
        event_id: &str,
        step_id: &str,
        event_type: StepEventType,
        tool_name: &str,
        tool_index: u32,
        created_at: u64,
    ) -> StepEvent {
        StepEvent {
            event_id: event_id.to_string(),
            run_id: "test-run".into(),
            canonical_event_id: None,
            step_id: step_id.to_string(),
            event_type,
            agent_id: None,
            caused_by: Vec::new(),
            payload: Some(serde_json::json!({
                "tool_name": tool_name,
                "tool_index": tool_index,
            })),
            created_at,
        }
    }

    fn tool_started_event(
        event_id: &str,
        step_id: &str,
        tool_name: &str,
        index: u32,
        created_at: u64,
    ) -> StepEvent {
        make_tool_event(
            event_id,
            step_id,
            StepEventType::ToolCallStarted,
            tool_name,
            index,
            created_at,
        )
    }

    fn tool_completed_event(
        event_id: &str,
        step_id: &str,
        tool_name: &str,
        index: u32,
        created_at: u64,
    ) -> StepEvent {
        make_tool_event(
            event_id,
            step_id,
            StepEventType::ToolCallCompleted,
            tool_name,
            index,
            created_at,
        )
    }

    #[test]

    fn concurrent_runs_with_colliding_legacy_slots_correlate_by_call_id() {
        let mut first_start = tool_started_event("e1", "shared-step", "bash", 0, 1000);
        first_start.payload.as_mut().unwrap()["call_id"] = serde_json::json!("call-a");
        let mut second_start = tool_started_event("e2", "shared-step", "bash", 0, 1001);
        second_start.payload.as_mut().unwrap()["call_id"] = serde_json::json!("call-b");
        let mut first_complete = tool_completed_event("e3", "shared-step", "bash", 0, 1002);
        first_complete.payload.as_mut().unwrap()["call_id"] = serde_json::json!("call-a");

        let records = extract_tool_calls(&[first_start, second_start, first_complete]);

        assert_eq!(records.len(), 2);
        assert!(records.iter().any(|record| {
            record.status == ToolCallStatus::Completed && record.step_id == "shared-step"
        }));
        assert!(records.iter().any(|record| {
            record.status == ToolCallStatus::StartedOnly && record.step_id == "shared-step"
        }));
    }

    #[test]

    fn provider_local_call_ids_remain_scoped_to_run_step_identity() {
        let mut root_start = tool_started_event("e1", "root-run-step", "bash", 0, 1000);
        root_start.payload.as_mut().unwrap()["call_id"] = serde_json::json!("call-1");
        let mut child_start = tool_started_event("e2", "child-run-step", "bash", 0, 1001);
        child_start.payload.as_mut().unwrap()["call_id"] = serde_json::json!("call-1");
        let mut root_complete = tool_completed_event("e3", "root-run-step", "bash", 0, 1002);
        root_complete.payload.as_mut().unwrap()["call_id"] = serde_json::json!("call-1");

        let records = extract_tool_calls(&[root_start, child_start, root_complete]);

        assert!(records.iter().any(|record| {
            record.step_id == "root-run-step" && record.status == ToolCallStatus::Completed
        }));
        assert!(records.iter().any(|record| {
            record.step_id == "child-run-step" && record.status == ToolCallStatus::StartedOnly
        }));
    }
}
