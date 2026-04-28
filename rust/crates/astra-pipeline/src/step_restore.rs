//! Crash recovery and session restore from Step Protocol checkpoints + events.
//!
//! # Recovery Strategy
//!
//! 1. Load latest heavy checkpoint → full conversation state
//! 2. Replay events from JSONL → extract completed tool results
//! 3. Warm idempotency cache → skip already-done tools on resume
//! 4. Validate protocol version → reject incompatible checkpoints
//!
//! # Usage
//!
//! ```ignore
//! let restored = restore_session("session-123")?;
//! if let Some(state) = restored {
//!     // Resume from checkpoint
//!     let messages = state.messages;
//!     let cache = state.idempotency_cache;
//!     // ... continue execution
//! }
//! ```

use std::collections::HashMap;

use crate::step_checkpoint::{FileBackedEventStore, read_latest_heavy_checkpoint};
use crate::step_protocol::{
    CachedToolResult, HeavyCheckpoint, IdempotencyKey, InMemoryIdempotencyCache, PROTOCOL_VERSION,
    SlotState, StepEvent, StepEventType, ValidationError, VersionPolicy,
    check_protocol_version_with_policy,
};

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
    /// Pre-warmed idempotency cache (skip already-done tools)
    pub idempotency_cache: InMemoryIdempotencyCache,
    /// Turn number to resume from
    pub resume_turn: u32,
    /// Protocol version of the checkpoint
    pub protocol_version: u32,
    /// Completed tool results extracted from events (tool_name → outputs)
    pub completed_tool_results: HashMap<String, Vec<String>>,
    /// Learning snapshot ID (for cross-session knowledge)
    pub learning_snapshot_id: Option<String>,
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
    /// Serialized runtime-owned continuity state restored from checkpoint.
    pub continuity_state: Option<serde_json::Value>,
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
pub fn restore_session(session_id: &str) -> Result<Option<RestoredSession>, RestoreError> {
    restore_session_with_policy(session_id, VersionPolicy::Compatible)
}

/// Restore with explicit version policy (no migration).
pub fn restore_session_with_policy(
    session_id: &str,
    policy: VersionPolicy,
) -> Result<Option<RestoredSession>, RestoreError> {
    restore_session_with_policy_inner(
        session_id,
        policy,
        None::<fn(&serde_json::Value) -> Result<(), String>>,
    )
}

/// Restore with an injected validator for the embedded `continuity_state` blob.
pub fn restore_session_with_continuity_validator<F>(
    session_id: &str,
    validator: F,
) -> Result<Option<RestoredSession>, RestoreError>
where
    F: FnOnce(&serde_json::Value) -> Result<(), String>,
{
    restore_session_with_policy_inner(session_id, VersionPolicy::Compatible, Some(validator))
}

fn restore_session_with_policy_inner<F>(
    session_id: &str,
    policy: VersionPolicy,
    validator: Option<F>,
) -> Result<Option<RestoredSession>, RestoreError>
where
    F: FnOnce(&serde_json::Value) -> Result<(), String>,
{
    // Step 1: Load latest heavy checkpoint
    let mut heavy = match read_latest_heavy_checkpoint(session_id) {
        Ok(Some(h)) => h,
        Ok(None) => return Ok(None),
        Err(e) => return Err(RestoreError::IoError(e.to_string())),
    };

    // Step 2: Validate protocol version (no migration)
    validate_checkpoint_version(&heavy, policy)?;
    validate_restored_continuity_state(&mut heavy, validator)?;

    // Step 3: Extract resume turn and warm cache
    build_restored_session(session_id, heavy)
}

/// Shared: build RestoredSession from a validated checkpoint.
fn build_restored_session(
    session_id: &str,
    heavy: HeavyCheckpoint,
) -> Result<Option<RestoredSession>, RestoreError> {
    let resume_turn = extract_resume_turn(&heavy);
    let (cache, completed_results) = warm_cache_from_events(session_id);

    Ok(Some(RestoredSession {
        messages: heavy.messages,
        budget_remaining_tokens: heavy.budget_remaining_tokens,
        budget_remaining_rounds: heavy.budget_remaining_rounds,
        blocked_tools: heavy.blocked_tools,
        recent_tools: heavy.recent_tools,
        idempotency_cache: cache,
        resume_turn,
        protocol_version: heavy.light.protocol_version,
        completed_tool_results: completed_results,
        learning_snapshot_id: heavy.learning_snapshot_id,
        interruption: heavy.interruption,
        approval_overrides: heavy.approval_overrides,
        consecutive_context_window_errors: heavy.consecutive_context_window_errors,
        compaction_state: heavy.compaction_state,
        continuity_state: heavy.continuity_state,
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

fn validate_restored_continuity_state<F>(
    heavy: &mut HeavyCheckpoint,
    validator: Option<F>,
) -> Result<(), RestoreError>
where
    F: FnOnce(&serde_json::Value) -> Result<(), String>,
{
    let Some(validator) = validator else {
        return Ok(());
    };
    match heavy.validate_with(validator) {
        Ok(()) => Ok(()),
        Err(ValidationError::ContinuityStateSchema(error)) => {
            tracing::warn!(
                error = %error,
                "dropping invalid continuity_state from restored checkpoint"
            );
            heavy.continuity_state = None;
            Ok(())
        }
        Err(ValidationError::Protocol(error)) => Err(RestoreError::InvalidCheckpoint(error)),
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

/// Replay step events from JSONL to warm the idempotency cache.
///
/// Extracts completed tool results and pre-populates the cache so that
/// on resume, already-executed tools are skipped (especially important
/// for non-idempotent tools like bash).
pub fn warm_cache_from_events(
    session_id: &str,
) -> (InMemoryIdempotencyCache, HashMap<String, Vec<String>>) {
    let mut cache = InMemoryIdempotencyCache::new();
    let mut completed_results: HashMap<String, Vec<String>> = HashMap::new();

    // Load events from JSONL
    let store = FileBackedEventStore::new(session_id);
    let events = store.all_events();

    // Extract tool completion events and populate cache
    for event in events {
        if let StepEventType::ToolCallCompleted = event.event_type
            && let Some(payload) = &event.payload
        {
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
            let content_hash = payload
                .get("idempotency_key")
                .and_then(|v| v.as_str())
                .unwrap_or_default();

            if !tool_name.is_empty() && !content_hash.is_empty() {
                let key = IdempotencyKey::semantic(
                    tool_name,
                    &serde_json::Value::String(content_hash.to_string()),
                );
                cache.record(
                    &key,
                    CachedToolResult {
                        tool_name: tool_name.to_string(),
                        output: output.to_string(),
                        is_error,
                        cached_at: event.created_at,
                    },
                );
            }

            if !tool_name.is_empty() {
                completed_results
                    .entry(tool_name.to_string())
                    .or_default()
                    .push(output.to_string());
            }
        }
    }

    (cache, completed_results)
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
    let cache_entries = restored.idempotency_cache.len();
    let tool_count: usize = restored
        .completed_tool_results
        .values()
        .map(|v| v.len())
        .sum();
    let mut s = format!(
        "Restored session: turn={}, messages={}, cache={} entries, \
         completed_tools={}, blocked={}, budget_tokens={}, budget_rounds={}",
        restored.resume_turn,
        restored.messages.len(),
        cache_entries,
        tool_count,
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
    if restored.continuity_state.is_some() {
        s.push_str(", continuity_state=yes");
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
    use crate::step_protocol::{ExecutionCursor, LightCheckpoint, epoch_ms};

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
            recent_tools: vec!["git_status".to_string()],
            learning_snapshot_id: Some("snap-123".to_string()),
            memory_context: None,
            delegation_id: None,
            delegation_pattern: None,
            delegation_sub_run_summaries: Vec::new(),
            interruption: None,
            approval_overrides: None,
            consecutive_context_window_errors: 0,
            compaction_state: None,
            continuity_state: None,
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
        // Same major (1xxx), different minor
        heavy.light.protocol_version = PROTOCOL_VERSION + 1;
        let result = validate_checkpoint_version(&heavy, VersionPolicy::Compatible);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_version_compatible_rejects_different_major() {
        let mut heavy = make_heavy_checkpoint(3, vec![], vec![]);
        heavy.light.protocol_version = 2000; // major 2, current is major 1
        let result = validate_checkpoint_version(&heavy, VersionPolicy::Compatible);
        assert!(matches!(result, Err(RestoreError::VersionMismatch { .. })));
    }

    #[test]
    fn restore_continuity_validator_drops_bad_embedded_state() {
        let mut heavy = make_heavy_checkpoint(3, vec![], vec![]);
        heavy.continuity_state = Some(serde_json::json!({"todos": "not-an-object"}));

        validate_restored_continuity_state(
            &mut heavy,
            Some(|value: &serde_json::Value| {
                value
                    .get("todos")
                    .and_then(|todos| todos.as_object())
                    .ok_or_else(|| "todos must be object".to_string())?;
                Ok(())
            }),
        )
        .unwrap();

        assert!(heavy.continuity_state.is_none());
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
            recent_tools: vec!["git_status".to_string()],
            idempotency_cache: InMemoryIdempotencyCache::new(),
            resume_turn: 3,
            protocol_version: PROTOCOL_VERSION,
            completed_tool_results: HashMap::new(),
            learning_snapshot_id: None,
            interruption: None,
            approval_overrides: None,
            consecutive_context_window_errors: 0,
            compaction_state: None,
            continuity_state: None,
        };

        let summary = restore_summary(&restored);
        assert!(summary.contains("turn=3"));
        assert!(summary.contains("messages=1"));
        assert!(summary.contains("blocked=1"));
        assert!(summary.contains("budget_tokens=50000"));
    }

    // ── Event-based cache warming ──

    #[test]
    fn warm_cache_from_empty_events() {
        // Non-existent session → empty cache
        let (cache, results) = warm_cache_from_events("nonexistent-session-xyz");
        assert!(cache.is_empty());
        assert!(results.is_empty());
    }

    // ── Tool timeline extraction ──

    #[test]
    fn extract_timeline_from_start_complete_pairs() {
        let events = vec![
            StepEvent {
                event_id: "e1".to_string(),
                step_id: "s1".to_string(),
                event_type: StepEventType::ToolCallStarted,
                agent_id: None,
                caused_by: vec![],
                payload: Some(serde_json::json!({"tool_name": "read_file"})),
                created_at: 1000,
            },
            StepEvent {
                event_id: "e2".to_string(),
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
                step_id: "s1".to_string(),
                event_type: StepEventType::ToolCallStarted,
                agent_id: None,
                caused_by: vec![],
                payload: Some(serde_json::json!({"tool_name": "bash"})),
                created_at: 2000,
            },
            StepEvent {
                event_id: "e2".to_string(),
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
                step_id: "s1".to_string(),
                event_type: StepEventType::ToolCallStarted,
                agent_id: None,
                caused_by: vec![],
                payload: Some(serde_json::json!({"tool_name": "read_file"})),
                created_at: 1000,
            },
            StepEvent {
                event_id: "e2".to_string(),
                step_id: "s1".to_string(),
                event_type: StepEventType::ToolCallStarted,
                agent_id: None,
                caused_by: vec![],
                payload: Some(serde_json::json!({"tool_name": "grep"})),
                created_at: 1010,
            },
            StepEvent {
                event_id: "e3".to_string(),
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
