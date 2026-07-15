//! Crash recovery and session restore from Step Protocol checkpoints + events.
//!
//! # Recovery Strategy
//!
//! 1. Load latest heavy checkpoint → full conversation state
//! 2. Replay events from JSONL → extract completed tool results
//! 3. Recover completed-tool audit history without granting replay authority
//! 4. Validate protocol version → reject incompatible checkpoints
//!
//! # Usage
//!
//! ```ignore
//! let restored = restore_session("user-123", "session-123")?;
//! if let Some(state) = restored {
//!     // Resume from checkpoint
//!     let messages = state.messages;
//!     let completed_results = state.completed_tool_results;
//!     // ... continue execution
//! }
//! ```

use std::collections::HashMap;

use crate::step_checkpoint::{FileBackedEventStore, read_latest_heavy_checkpoint};
use crate::step_protocol::{
    HeavyCheckpoint, PROTOCOL_VERSION, SlotState, StepEvent, StepEventType, VersionPolicy,
    check_protocol_version_with_policy, persisted_cache_key_is_context_bound,
};

pub const CACHE_RESTORE_REPORT_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheRestoreReport {
    pub contract_version: u32,
    /// Completed-event rows deliberately kept out of the live execution
    /// cache because they carry neither durable logical invocation identity
    /// nor a verified current context snapshot.
    pub rejected_unverified_entries: usize,
    /// Diagnostic subset of `rejected_unverified_entries` that explicitly
    /// carried a context/freshness suffix.
    pub rejected_context_bound_entries: usize,
    /// Whether this bounded audit projection covers the complete journal.
    /// Audit completeness is separate from executable replay authority.
    pub journal_complete: bool,
    pub journal_bytes_read: usize,
    pub events_examined: usize,
    pub prefix_truncated: bool,
    pub events_dropped: usize,
    pub trailing_torn_line: bool,
    pub degraded_reason: Option<String>,
}

impl Default for CacheRestoreReport {
    fn default() -> Self {
        Self {
            contract_version: CACHE_RESTORE_REPORT_VERSION,
            rejected_unverified_entries: 0,
            rejected_context_bound_entries: 0,
            journal_complete: true,
            journal_bytes_read: 0,
            events_examined: 0,
            prefix_truncated: false,
            events_dropped: 0,
            trailing_torn_line: false,
            degraded_reason: None,
        }
    }
}

/// Restored session state — everything needed to resume execution.
#[derive(Debug)]
pub struct RestoredSession {
    /// Full conversation messages (for LLM context)
    pub messages: Vec<serde_json::Value>,
    /// Remaining token budget
    pub budget_remaining_tokens: u64,
    /// Remaining turn rounds
    pub budget_remaining_rounds: u32,
    /// Tools currently blocked (from stall/health tracking)
    pub blocked_tools: Vec<String>,
    /// Recently used tools (for selection context)
    pub recent_tools: Vec<String>,
    /// Turn number to resume from
    pub resume_turn: u32,
    /// Protocol version of the checkpoint
    pub protocol_version: u32,
    /// Completed tool results extracted from events (tool_name → outputs)
    pub completed_tool_results: HashMap<String, Vec<String>>,
    /// Structured interruption record from the checkpoint that created this restore
    /// point. When present, describes why the previous run was interrupted and
    /// what the caller should do to resume (e.g., wait, compact, intervene).
    pub interruption: Option<serde_json::Value>,
    /// Serialized approval overrides restored from checkpoint.
    /// The caller should merge these into PermissionManager on resume.
    pub approval_overrides: Option<serde_json::Value>,
    /// Consecutive context-window errors counter restored from checkpoint.
    pub consecutive_context_window_errors: u32,
    /// Serialized CompactionEffectivenessTracker state for enriched resume guidance.
    pub compaction_state: Option<serde_json::Value>,
    /// Serialized context pipeline state for warm-start on resume.
    pub pipeline_state: Option<serde_json::Value>,
    /// Explicit account of completed-event rows rejected as execution cache
    /// authority during recovery.
    pub cache_restore_report: CacheRestoreReport,
}
#[derive(Debug, Clone, PartialEq)]
pub enum RestoreError {
    /// No checkpoint found for this session
    NoCheckpoint,
    /// Protocol version mismatch (checkpoint too old/new)
    VersionMismatch {
        checkpoint_version: u32,
        current_version: u32,
    },
    /// IO error reading checkpoint files
    IoError(String),
    /// Checkpoint data is corrupted or invalid
    InvalidCheckpoint(String),
}

impl std::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoCheckpoint => write!(f, "no checkpoint found"),
            Self::VersionMismatch {
                checkpoint_version,
                current_version,
            } => write!(
                f,
                "protocol version mismatch: checkpoint={}, current={}",
                checkpoint_version, current_version
            ),
            Self::IoError(msg) => write!(f, "IO error: {}", msg),
            Self::InvalidCheckpoint(msg) => write!(f, "invalid checkpoint: {}", msg),
        }
    }
}

/// Restore a session from the latest heavy checkpoint + event history.
///
/// Returns `Ok(Some(RestoredSession))` if a valid checkpoint exists,
/// `Ok(None)` if no checkpoint found, or `Err` on version mismatch/corruption.
pub fn restore_session(
    user_id: &str,
    session_id: &str,
) -> Result<Option<RestoredSession>, RestoreError> {
    restore_session_with_policy(user_id, session_id, VersionPolicy::Compatible)
}

/// Restore with explicit version policy.
pub fn restore_session_with_policy(
    user_id: &str,
    session_id: &str,
    policy: VersionPolicy,
) -> Result<Option<RestoredSession>, RestoreError> {
    // Step 1: Load latest heavy checkpoint
    let heavy = match read_latest_heavy_checkpoint(user_id, session_id) {
        Ok(Some(h)) => h,
        Ok(None) => return Ok(None),
        Err(e) => return Err(RestoreError::IoError(e.to_string())),
    };

    // Step 2: Validate protocol version
    validate_checkpoint_version(&heavy, policy)?;

    // Step 3: Extract resume turn and completed-tool audit history.
    build_restored_session(user_id, session_id, heavy)
}

/// Shared: build RestoredSession from a validated checkpoint.
fn build_restored_session(
    user_id: &str,
    session_id: &str,
    heavy: HeavyCheckpoint,
) -> Result<Option<RestoredSession>, RestoreError> {
    let resume_turn = extract_resume_turn(&heavy);
    let (completed_results, cache_restore_report) =
        recover_completed_tool_audit_from_events(user_id, session_id);

    Ok(Some(RestoredSession {
        messages: heavy.messages,
        budget_remaining_tokens: heavy.budget_remaining_tokens,
        budget_remaining_rounds: heavy.budget_remaining_rounds,
        blocked_tools: heavy.blocked_tools,
        recent_tools: heavy.recent_tools,
        resume_turn,
        protocol_version: heavy.light.protocol_version,
        completed_tool_results: completed_results,
        interruption: heavy.interruption,
        approval_overrides: heavy.approval_overrides,
        consecutive_context_window_errors: heavy.consecutive_context_window_errors,
        compaction_state: heavy.compaction_state,
        pipeline_state: heavy.pipeline_state,
        cache_restore_report,
    }))
}

/// Validate that the checkpoint's protocol version is compatible.
fn validate_checkpoint_version(
    heavy: &HeavyCheckpoint,
    policy: VersionPolicy,
) -> Result<(), RestoreError> {
    let cp_version = heavy.light.protocol_version;

    if cp_version == 0 {
        return Err(RestoreError::InvalidCheckpoint(
            "checkpoint has zero protocol version".to_string(),
        ));
    }

    match check_protocol_version_with_policy(cp_version, policy) {
        Ok(_verdict) => Ok(()),
        Err(_) => Err(RestoreError::VersionMismatch {
            checkpoint_version: cp_version,
            current_version: PROTOCOL_VERSION,
        }),
    }
}

/// Extract the turn number to resume from (based on cursor progress).
fn extract_resume_turn(heavy: &HeavyCheckpoint) -> u32 {
    // Parse turn number from step_id formats like "session-turn-N" and
    // "session-turn-N-step-M".
    let step_id = &heavy.light.step_id;
    step_id
        .split("-turn-")
        .nth(1)
        .and_then(|suffix| suffix.split('-').next())
        .and_then(|turn| turn.parse().ok())
        .unwrap_or(0)
}

/// Stream completed tool events from JSONL for recovery audit state.
///
/// Event payloads historically carried a name/arguments-derived cache-key
/// string. That is neither logical invocation identity nor proof that the
/// current workspace still matches the observation context. Promoting it into
/// a live execution cache can collapse distinct intent or serve stale reads.
/// Completed results remain available for audit and recovery classification;
/// executable replay authority comes from the invocation ledger/checkpoint
/// state machine instead.
pub fn recover_completed_tool_audit_from_events(
    user_id: &str,
    session_id: &str,
) -> (HashMap<String, Vec<String>>, CacheRestoreReport) {
    recover_completed_tool_audit_from_events_with_bounds(
        user_id,
        session_id,
        crate::step_checkpoint::STEP_EVENT_RECOVERY_MAX_BYTES,
        crate::step_checkpoint::STEP_EVENT_RECOVERY_MAX_EVENTS,
    )
}

fn recover_completed_tool_audit_from_events_with_bounds(
    user_id: &str,
    session_id: &str,
    max_bytes: usize,
    max_events: usize,
) -> (HashMap<String, Vec<String>>, CacheRestoreReport) {
    let mut completed_results: HashMap<String, Vec<String>> = HashMap::new();
    let mut cache_restore_report = CacheRestoreReport::default();

    let window = match FileBackedEventStore::load_recent_events_bounded(
        user_id, session_id, max_bytes, max_events,
    ) {
        Ok(window) => window,
        Err(error) => {
            cache_restore_report.journal_complete = false;
            cache_restore_report.degraded_reason = Some(error.to_string());
            tracing::warn!(
                user_id,
                session_id,
                error = %error,
                "completed-tool audit recovery failed without returning a partial projection"
            );
            return (completed_results, cache_restore_report);
        }
    };
    cache_restore_report.journal_bytes_read = window.bytes_read;
    cache_restore_report.events_examined = window.events.len();
    cache_restore_report.prefix_truncated = window.prefix_truncated;
    cache_restore_report.events_dropped = window.events_dropped;
    cache_restore_report.trailing_torn_line = window.trailing_torn_line;
    cache_restore_report.journal_complete =
        !window.prefix_truncated && window.events_dropped == 0 && !window.trailing_torn_line;
    if !cache_restore_report.journal_complete {
        cache_restore_report.degraded_reason = Some(format!(
            "bounded event audit window: prefix_truncated={}, events_dropped={}, trailing_torn_line={}",
            window.prefix_truncated, window.events_dropped, window.trailing_torn_line
        ));
    }

    for event in &window.events {
        if let StepEventType::ToolCallCompleted = &event.event_type {
            let Some(payload) = &event.payload else {
                continue;
            };
            let tool_name = payload
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let output = payload
                .get("output")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let is_error = payload
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let cache_key = payload
                .get("idempotency_key")
                .and_then(|v| v.as_str())
                .unwrap_or_default();

            if !tool_name.is_empty() && !cache_key.is_empty() && !is_error {
                cache_restore_report.rejected_unverified_entries += 1;
                if persisted_cache_key_is_context_bound(cache_key) {
                    cache_restore_report.rejected_context_bound_entries += 1;
                }
            }

            if !tool_name.is_empty() {
                completed_results
                    .entry(tool_name.to_string())
                    .or_default()
                    .push(output.to_string());
            }
        }
    }
    if cache_restore_report.rejected_unverified_entries > 0 {
        tracing::warn!(
            user_id,
            session_id,
            rejected_unverified_entries = cache_restore_report.rejected_unverified_entries,
            rejected_context_bound_entries = cache_restore_report.rejected_context_bound_entries,
            "completed tool-event results were retained for audit but rejected as executable replay authority"
        );
    }

    (completed_results, cache_restore_report)
}

/// Determine which execution slots are already complete and can be skipped.
///
/// Returns a set of slot indices that were Completed in the checkpoint cursor.
pub fn completed_slots(heavy: &HeavyCheckpoint) -> Vec<usize> {
    heavy
        .light
        .cursor
        .slots
        .iter()
        .enumerate()
        .filter(|(_, slot)| slot.state == SlotState::Completed)
        .map(|(i, _)| i)
        .collect()
}

/// Build a summary of what was recovered (for logging/explain mode).
pub fn restore_summary(restored: &RestoredSession) -> String {
    let tool_count: usize = restored
        .completed_tool_results
        .values()
        .map(|v| v.len())
        .sum();
    let mut s = format!(
        "Restored session: turn={}, messages={}, completed_tools={}, \
         unverified_cache_entries_rejected={}, audit_complete={}, blocked={}, budget_tokens={}, budget_rounds={}",
        restored.resume_turn,
        restored.messages.len(),
        tool_count,
        restored.cache_restore_report.rejected_unverified_entries,
        restored.cache_restore_report.journal_complete,
        restored.blocked_tools.len(),
        restored.budget_remaining_tokens,
        restored.budget_remaining_rounds,
    );
    if restored.consecutive_context_window_errors > 0 {
        s.push_str(&format!(
            ", ctx_window_errors={}",
            restored.consecutive_context_window_errors
        ));
    }
    if restored.approval_overrides.is_some() {
        s.push_str(", approval_overrides=yes");
    }
    if restored.compaction_state.is_some() {
        s.push_str(", compaction_state=yes");
    }
    s
}

/// Validate and convert raw event payloads into structured tool completion records.
/// Used for post-mortem analysis and debugging.
pub fn extract_tool_timeline(events: &[StepEvent]) -> Vec<ToolTimelineEntry> {
    let mut timeline = Vec::new();
    let mut pending_starts: HashMap<String, u64> = HashMap::new();

    for event in events {
        match event.event_type {
            StepEventType::ToolCallStarted => {
                if let Some(payload) = &event.payload {
                    let tool_name = payload
                        .get("tool_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    if !tool_name.is_empty() {
                        pending_starts.insert(tool_name, event.created_at);
                    }
                }
            }
            StepEventType::ToolCallCompleted | StepEventType::ToolCallFailed => {
                if let Some(payload) = &event.payload {
                    let tool_name = payload
                        .get("tool_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let is_error = event.event_type == StepEventType::ToolCallFailed
                        || payload
                            .get("is_error")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                    let started_at = pending_starts.remove(&tool_name).unwrap_or(0);
                    let duration_ms = if started_at > 0 {
                        event.created_at.saturating_sub(started_at)
                    } else {
                        0
                    };

                    timeline.push(ToolTimelineEntry {
                        tool_name,
                        started_at,
                        completed_at: event.created_at,
                        duration_ms,
                        is_error,
                        step_id: event.step_id.clone(),
                    });
                }
            }
            _ => {}
        }
    }

    timeline
}

/// A single entry in the tool execution timeline.
#[derive(Debug, Clone)]
pub struct ToolTimelineEntry {
    pub tool_name: String,
    pub started_at: u64,
    pub completed_at: u64,
    pub duration_ms: u64,
    pub is_error: bool,
    pub step_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::step_protocol::{
        ExecutionCursor, IdempotencyKey, LightCheckpoint, StepEventStore, epoch_ms,
    };

    const TEST_USER_ID: &str = "test-user";

    // ── Helper: build a heavy checkpoint for testing ──

    fn make_heavy_checkpoint(
        turn: u32,
        messages: Vec<serde_json::Value>,
        blocked_tools: Vec<String>,
    ) -> HeavyCheckpoint {
        HeavyCheckpoint {
            light: LightCheckpoint {
                protocol_version: PROTOCOL_VERSION,
                cursor: ExecutionCursor::default(),
                step_id: format!("session-turn-{}", turn),
                task_id: "task-1".to_string(),
                agent_id: "agent-1".to_string(),
                progress: 0.5,
                total_tokens: 1000,
                created_at: epoch_ms(),
            },
            messages,
            budget_remaining_tokens: 50000,
            budget_remaining_rounds: 5,
            blocked_tools,
            recent_tools: vec!["git".to_string()],
            memory_context: None,
            delegation_id: None,
            delegation_pattern: None,
            delegation_sub_run_summaries: Vec::new(),
            interruption: None,
            approval_overrides: None,
            consecutive_context_window_errors: 0,
            compaction_state: None,
            pipeline_state: None,
            config_version_id: None,
        }
    }

    // ── Version validation tests ──

    #[test]
    fn validate_version_accepts_current() {
        let heavy = make_heavy_checkpoint(3, vec![], vec![]);
        let result = validate_checkpoint_version(&heavy, VersionPolicy::Strict);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_version_rejects_zero() {
        let mut heavy = make_heavy_checkpoint(3, vec![], vec![]);
        heavy.light.protocol_version = 0;
        let result = validate_checkpoint_version(&heavy, VersionPolicy::Strict);
        assert!(matches!(result, Err(RestoreError::InvalidCheckpoint(_))));
    }

    #[test]
    fn validate_version_strict_rejects_mismatch() {
        let mut heavy = make_heavy_checkpoint(3, vec![], vec![]);
        heavy.light.protocol_version = 999; // different version
        let result = validate_checkpoint_version(&heavy, VersionPolicy::Strict);
        assert!(matches!(result, Err(RestoreError::VersionMismatch { .. })));
    }

    #[test]
    fn validate_version_compatible_accepts_same_major() {
        let mut heavy = make_heavy_checkpoint(3, vec![], vec![]);
        // Same major (2xxx), different minor
        heavy.light.protocol_version = PROTOCOL_VERSION + 1;
        let result = validate_checkpoint_version(&heavy, VersionPolicy::Compatible);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_version_compatible_rejects_different_major() {
        let mut heavy = make_heavy_checkpoint(3, vec![], vec![]);
        heavy.light.protocol_version = 1000; // major 1, current is major 2
        let result = validate_checkpoint_version(&heavy, VersionPolicy::Compatible);
        assert!(matches!(result, Err(RestoreError::VersionMismatch { .. })));
    }

    // ── Resume turn extraction ──

    #[test]
    fn extract_resume_turn_from_step_id() {
        let heavy = make_heavy_checkpoint(7, vec![], vec![]);
        assert_eq!(extract_resume_turn(&heavy), 7);
    }

    #[test]
    fn extract_resume_turn_from_unique_step_id() {
        let mut heavy = make_heavy_checkpoint(7, vec![], vec![]);
        heavy.light.step_id = "session-turn-7-step-42".to_string();
        assert_eq!(extract_resume_turn(&heavy), 7);
    }

    #[test]
    fn extract_resume_turn_defaults_to_zero() {
        let mut heavy = make_heavy_checkpoint(0, vec![], vec![]);
        heavy.light.step_id = "no-turn-info".to_string();
        // "info" isn't a number, so it should default to 0
        assert_eq!(extract_resume_turn(&heavy), 0);
    }

    // ── Completed slots ──

    #[test]
    fn completed_slots_empty_cursor() {
        let heavy = make_heavy_checkpoint(3, vec![], vec![]);
        assert!(completed_slots(&heavy).is_empty());
    }

    #[test]
    fn completed_slots_with_mixed_states() {
        let mut heavy = make_heavy_checkpoint(3, vec![], vec![]);
        heavy.light.cursor = ExecutionCursor::for_act(3);
        heavy.light.cursor.slots[0].state = SlotState::Completed;
        heavy.light.cursor.slots[1].state = SlotState::Failed;
        heavy.light.cursor.slots[2].state = SlotState::Completed;

        let done = completed_slots(&heavy);
        assert_eq!(done, vec![0, 2]);
    }

    // ── Restore summary ──

    #[test]
    fn restore_summary_format() {
        let restored = RestoredSession {
            messages: vec![serde_json::json!({"role": "user", "content": "hello"})],
            budget_remaining_tokens: 50000,
            budget_remaining_rounds: 5,
            blocked_tools: vec!["bash".to_string()],
            recent_tools: vec!["git".to_string()],
            resume_turn: 3,
            protocol_version: PROTOCOL_VERSION,
            completed_tool_results: HashMap::new(),
            interruption: None,
            approval_overrides: None,
            consecutive_context_window_errors: 0,
            compaction_state: None,
            pipeline_state: None,
            cache_restore_report: CacheRestoreReport::default(),
        };

        let summary = restore_summary(&restored);
        assert!(summary.contains("turn=3"));
        assert!(summary.contains("messages=1"));
        assert!(summary.contains("blocked=1"));
        assert!(summary.contains("budget_tokens=50000"));
    }

    // ── Event-based cache warming ──

    #[test]
    fn completed_tool_audit_from_empty_events_is_empty() {
        let (results, report) =
            recover_completed_tool_audit_from_events(TEST_USER_ID, "nonexistent-session-xyz");
        assert!(results.is_empty());
        assert_eq!(report, CacheRestoreReport::default());
    }

    #[test]
    fn restore_rejects_unscoped_semantic_result_as_replay_authority() {
        let session_id = format!("warm-cache-key-{}-{}", std::process::id(), epoch_ms());
        let args = serde_json::json!({"path": "src/lib.rs"});
        let key = IdempotencyKey::semantic("read_file", &args);
        let mut store = FileBackedEventStore::empty(TEST_USER_ID, &session_id);
        let _ = store.append(StepEvent {
            event_id: "completed-read".to_string(),
            canonical_event_id: None,
            step_id: "step-1".to_string(),
            event_type: StepEventType::ToolCallCompleted,
            agent_id: None,
            caused_by: vec![],
            payload: Some(serde_json::json!({
                "tool_name": "read_file",
                "idempotency_key": key.cache_key(),
                "output": "module contents",
                "is_error": false,
            })),
            created_at: 1000,
        });
        drop(store);

        let (results, report) = recover_completed_tool_audit_from_events(TEST_USER_ID, &session_id);
        assert_eq!(
            results.get("read_file").cloned().unwrap_or_default(),
            vec!["module contents".to_string()]
        );
        assert_eq!(report.rejected_unverified_entries, 1);
        assert_eq!(report.rejected_context_bound_entries, 0);

        let _ = std::fs::remove_dir_all(
            crate::step_checkpoint::session_dir_for(TEST_USER_ID, &session_id)
                .expect("valid session id for test cleanup"),
        );
    }

    #[test]
    fn restore_rejects_context_bound_observation_without_current_freshness_proof() {
        let session_id = format!("warm-cache-context-{}-{}", std::process::id(), epoch_ms());
        let args = serde_json::json!({"path": "src/lib.rs"});
        let key = IdempotencyKey::semantic("read_file", &args).with_context(
            crate::step_protocol::ContextSignature {
                workspace_version: Some("workspace_epoch:0".to_string()),
                memory_snapshot_id: None,
            },
        );
        let mut store = FileBackedEventStore::empty(TEST_USER_ID, &session_id);
        store
            .append(StepEvent {
                event_id: "completed-stale-read".to_string(),
                canonical_event_id: None,
                step_id: "step-1".to_string(),
                event_type: StepEventType::ToolCallCompleted,
                agent_id: None,
                caused_by: vec![],
                payload: Some(serde_json::json!({
                    "tool_name": "read_file",
                    "idempotency_key": key.cache_key(),
                    "output": "content before workspace mutation",
                    "is_error": false,
                })),
                created_at: 1000,
            })
            .unwrap();
        drop(store);

        let (results, report) = recover_completed_tool_audit_from_events(TEST_USER_ID, &session_id);
        assert_eq!(report.rejected_unverified_entries, 1);
        assert_eq!(report.rejected_context_bound_entries, 1);
        assert_eq!(
            results.get("read_file"),
            Some(&vec!["content before workspace mutation".to_string()]),
            "audit history remains available even though the optimization entry is rejected"
        );

        let _ = std::fs::remove_dir_all(
            crate::step_checkpoint::session_dir_for(TEST_USER_ID, &session_id)
                .expect("valid session id for test cleanup"),
        );
    }

    #[test]
    fn bounded_completed_tool_audit_reports_degraded_tail_instead_of_full_history() {
        let session_id = format!("bounded-audit-{}-{}", std::process::id(), epoch_ms());
        let mut store = FileBackedEventStore::empty(TEST_USER_ID, &session_id);
        for idx in 0..4 {
            store
                .append(StepEvent {
                    event_id: format!("completed-{idx}"),
                    canonical_event_id: None,
                    step_id: "step-1".to_string(),
                    event_type: StepEventType::ToolCallCompleted,
                    agent_id: None,
                    caused_by: vec![],
                    payload: Some(serde_json::json!({
                        "tool_name": "read_file",
                        "output": format!("content-{idx}"),
                        "is_error": false,
                    })),
                    created_at: 1_000 + idx,
                })
                .unwrap();
        }
        drop(store);

        let (results, report) = recover_completed_tool_audit_from_events_with_bounds(
            TEST_USER_ID,
            &session_id,
            crate::step_checkpoint::STEP_EVENT_RECOVERY_MAX_BYTES,
            2,
        );
        assert_eq!(
            results.get("read_file"),
            Some(&vec!["content-2".to_string(), "content-3".to_string()])
        );
        assert!(!report.journal_complete);
        assert!(report.prefix_truncated);
        assert_eq!(report.events_dropped, 2);
        assert_eq!(report.events_examined, 2);
        assert!(report.degraded_reason.is_some());

        let _ = std::fs::remove_dir_all(
            crate::step_checkpoint::session_dir_for(TEST_USER_ID, &session_id)
                .expect("valid session id for test cleanup"),
        );
    }

    // ── Tool timeline extraction ──

    #[test]
    fn extract_timeline_from_start_complete_pairs() {
        let events = vec![
            StepEvent {
                event_id: "e1".to_string(),
                canonical_event_id: None,
                step_id: "s1".to_string(),
                event_type: StepEventType::ToolCallStarted,
                agent_id: None,
                caused_by: vec![],
                payload: Some(serde_json::json!({"tool_name": "read_file"})),
                created_at: 1000,
            },
            StepEvent {
                event_id: "e2".to_string(),
                canonical_event_id: None,
                step_id: "s1".to_string(),
                event_type: StepEventType::ToolCallCompleted,
                agent_id: None,
                caused_by: vec!["e1".to_string()],
                payload: Some(serde_json::json!({
                    "tool_name": "read_file",
                    "output": "file contents"
                })),
                created_at: 1050,
            },
        ];

        let timeline = extract_tool_timeline(&events);
        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline[0].tool_name, "read_file");
        assert_eq!(timeline[0].duration_ms, 50);
        assert!(!timeline[0].is_error);
    }

    #[test]
    fn extract_timeline_handles_failures() {
        let events = vec![
            StepEvent {
                event_id: "e1".to_string(),
                canonical_event_id: None,
                step_id: "s1".to_string(),
                event_type: StepEventType::ToolCallStarted,
                agent_id: None,
                caused_by: vec![],
                payload: Some(serde_json::json!({"tool_name": "bash"})),
                created_at: 2000,
            },
            StepEvent {
                event_id: "e2".to_string(),
                canonical_event_id: None,
                step_id: "s1".to_string(),
                event_type: StepEventType::ToolCallFailed,
                agent_id: None,
                caused_by: vec!["e1".to_string()],
                payload: Some(serde_json::json!({
                    "tool_name": "bash",
                    "output": "command not found"
                })),
                created_at: 2100,
            },
        ];

        let timeline = extract_tool_timeline(&events);
        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline[0].tool_name, "bash");
        assert_eq!(timeline[0].duration_ms, 100);
        assert!(timeline[0].is_error);
    }

    #[test]
    fn extract_timeline_interleaved_tools() {
        let events = vec![
            StepEvent {
                event_id: "e1".to_string(),
                canonical_event_id: None,
                step_id: "s1".to_string(),
                event_type: StepEventType::ToolCallStarted,
                agent_id: None,
                caused_by: vec![],
                payload: Some(serde_json::json!({"tool_name": "read_file"})),
                created_at: 1000,
            },
            StepEvent {
                event_id: "e2".to_string(),
                canonical_event_id: None,
                step_id: "s1".to_string(),
                event_type: StepEventType::ToolCallStarted,
                agent_id: None,
                caused_by: vec![],
                payload: Some(serde_json::json!({"tool_name": "grep"})),
                created_at: 1010,
            },
            StepEvent {
                event_id: "e3".to_string(),
                canonical_event_id: None,
                step_id: "s1".to_string(),
                event_type: StepEventType::ToolCallCompleted,
                agent_id: None,
                caused_by: vec!["e2".to_string()],
                payload: Some(serde_json::json!({
                    "tool_name": "grep",
                    "output": "match found"
                })),
                created_at: 1030,
            },
            StepEvent {
                event_id: "e4".to_string(),
                canonical_event_id: None,
                step_id: "s1".to_string(),
                event_type: StepEventType::ToolCallCompleted,
                agent_id: None,
                caused_by: vec!["e1".to_string()],
                payload: Some(serde_json::json!({
                    "tool_name": "read_file",
                    "output": "file data"
                })),
                created_at: 1050,
            },
        ];

        let timeline = extract_tool_timeline(&events);
        assert_eq!(timeline.len(), 2);
        // grep completes first
        assert_eq!(timeline[0].tool_name, "grep");
        assert_eq!(timeline[0].duration_ms, 20);
        // read_file completes second
        assert_eq!(timeline[1].tool_name, "read_file");
        assert_eq!(timeline[1].duration_ms, 50);
    }

    // ── RestoreError display ──

    #[test]
    fn restore_error_display_format() {
        let err = RestoreError::VersionMismatch {
            checkpoint_version: 999,
            current_version: 1000,
        };
        let msg = err.to_string();
        assert!(msg.contains("999"));
        assert!(msg.contains("1000"));

        assert_eq!(
            RestoreError::NoCheckpoint.to_string(),
            "no checkpoint found"
        );
    }
}
