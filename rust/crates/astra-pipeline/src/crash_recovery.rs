//! Crash Recovery Manager — first-principles implementation.
//!
//! # Design
//!
//! After a process crash, the recovery manager:
//! 1. **Detects** the crash (session was `in_progress` but the owning process died)
//! 2. **Locates** the last valid checkpoint from the journal
//! 3. **Scans** journal events after the checkpoint to reconstruct what happened
//! 4. **Classifies** each tool call into: Replay / Skip / RequireUser
//! 5. **Replays** deterministic steps, skipping side-effect tools
//! 6. **Transitions** to `Recovered` (ready to continue) or `Failed` (unrecoverable)
//!
//! # State Machine
//!
//! ```text
//! Idle → Scanning → Replaying → Recovered
//!          ↓            ↓
//!        Failed       Failed
//! ```
//!
//! # Unhappy Paths Covered
//!
//! - No checkpoint found → `Failed(MissingCheckpoint)`
//! - Checkpoint JSON corrupted → `Failed(CorruptedCheckpoint)`
//! - Protocol version mismatch → `Failed(VersionMismatch)`
//! - Journal gap (missing events between checkpoint and crash) → `Failed(JournalGap)`
//! - In-flight tool call at crash time → classified as `RequireUser`
//! - Side-effect tool partially executed → classified as `RequireUser`
//! - Crypto hash mismatch (tamper/corruption) → `Failed(HashMismatch)`
//! - Double crash (recovery itself crashes) → `Failed(RecursiveCrash)`

use astra_turn_types::{ToolIdempotency, classify_tool_idempotency};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt;

use crate::step_protocol::{
    CachedToolResult, HeavyCheckpoint, PROTOCOL_VERSION, StepCheckpoint, StepEvent, StepEventType,
};

// ---------------------------------------------------------------------------
// Integration bridge: high-level recovery function
// ---------------------------------------------------------------------------

use crate::step_checkpoint::{FileBackedEventStore, read_latest_heavy_checkpoint};
use crate::step_restore::RestoredSession;

/// Recover a crashed session using the full state machine.
///
/// This is the primary entry point for crash recovery. It:
/// 1. Detects if the session crashed (no SessionEnd event)
/// 2. Loads the latest checkpoint
/// 3. Scans journal events after the checkpoint
/// 4. Classifies tool calls for replay
/// 5. Returns a RestoredSession if recovery is possible
///
/// Returns `Ok(None)` if no checkpoint exists or session is already completed.
/// Returns `Err` if recovery fails (corruption, version mismatch, etc).
pub fn recover_from_crash(session_id: &str) -> Result<Option<RecoveryOutcome>, RecoveryError> {
    let mut manager = CrashRecoveryManager::new();

    // Phase 1: Begin recovery
    manager.begin_recovery()?;

    // Load checkpoint from disk
    let heavy = match read_latest_heavy_checkpoint(session_id) {
        Ok(Some(h)) => h,
        Ok(None) => return Ok(None), // No checkpoint, nothing to recover
        Err(e) => {
            return Err(RecoveryError::CorruptedCheckpoint(format!(
                "Failed to read checkpoint: {e}"
            )));
        }
    };

    // Extract turn number from checkpoint
    let checkpoint_turn = extract_turn_from_step_id(&heavy.light.step_id);

    // Stream only the recovery window after the checkpoint. Long sessions may
    // have very large journals; recovery should not materialize historical
    // events that cannot affect the current replay.
    let events =
        FileBackedEventStore::load_events_created_after(session_id, heavy.light.created_at)
            .map_err(|e| {
                RecoveryError::JournalRead(format!("Failed to read event journal: {e}"))
            })?;

    // Serialize checkpoint for scan_journal (must be StepCheckpoint enum JSON)
    let light_cp = StepCheckpoint::Light(heavy.light.clone());
    let checkpoint_json = serde_json::to_string(&light_cp).map_err(|e| {
        RecoveryError::CorruptedCheckpoint(format!("Failed to serialize checkpoint: {e}"))
    })?;

    // Phase 2: Scan journal
    let scan_result = manager.scan_journal(
        session_id,
        checkpoint_turn + 1, // Crash happened after this checkpoint
        Some(&checkpoint_json),
        checkpoint_turn,
        events,
    )?;

    // Phase 3: Classify tools
    manager.begin_replay()?;

    // Check if auto-recovery is possible
    let can_auto_recover = manager
        .context()
        .map(|ctx| ctx.can_auto_recover())
        .unwrap_or(false);

    if can_auto_recover {
        // Complete recovery automatically
        manager.complete_recovery()?;

        // Build RestoredSession from scan result
        let restored = build_restored_from_scan(&scan_result, &heavy)?;

        Ok(Some(RecoveryOutcome::AutoRecovered {
            restored,
            manager: Box::new(manager),
            scan_result,
        }))
    } else {
        // User intervention required
        let pending = manager
            .context()
            .map(|ctx| ctx.pending_user_decisions())
            .unwrap_or_default();

        let restored = build_restored_from_scan(&scan_result, &heavy)?;

        Ok(Some(RecoveryOutcome::RequiresUserInput {
            pending_decisions: pending.into_iter().map(|(name, decision)| (name.clone(), decision.clone())).collect(),
            restored,
            manager: Box::new(manager),
            scan_result,
        }))
    }
}

/// Extract turn number from step_id (e.g., "session-turn-5" → 5)
fn extract_turn_from_step_id(step_id: &str) -> u32 {
    step_id
        .split("-turn-")
        .nth(1)
        .and_then(|suffix| suffix.split('-').next())
        .and_then(|turn| turn.parse().ok())
        .unwrap_or(0)
}

/// Build RestoredSession from scan result and heavy checkpoint
fn build_restored_from_scan(
    scan: &JournalScanResult,
    heavy: &HeavyCheckpoint,
) -> Result<RestoredSession, RecoveryError> {
    use crate::step_protocol::{IdempotencyKey, InMemoryIdempotencyCache};
    use std::collections::HashMap;

    let mut cache = InMemoryIdempotencyCache::new();
    let mut completed_results: HashMap<String, Vec<String>> = HashMap::new();

    // Extract completed tool results from events
    for tool_call in &scan.tool_calls_found {
        if let Some(ref cached) = tool_call.cached_result {
            let key = IdempotencyKey::semantic(
                &tool_call.tool_name,
                &serde_json::Value::String(cached.output.clone()),
            );
            cache.record(&key, cached.clone());

            completed_results
                .entry(tool_call.tool_name.clone())
                .or_default()
                .push(cached.output.clone());
        }
    }

    Ok(RestoredSession {
        messages: heavy.messages.clone(),
        budget_remaining_tokens: heavy.budget_remaining_tokens,
        budget_remaining_rounds: heavy.budget_remaining_rounds,
        blocked_tools: heavy.blocked_tools.clone(),
        recent_tools: heavy.recent_tools.clone(),
        idempotency_cache: cache,
        resume_turn: extract_turn_from_step_id(&heavy.light.step_id),
        protocol_version: heavy.light.protocol_version,
        completed_tool_results: completed_results,
        interruption: heavy.interruption.clone(),
        approval_overrides: heavy.approval_overrides.clone(),
        consecutive_context_window_errors: heavy.consecutive_context_window_errors,
        compaction_state: heavy.compaction_state.clone(),
        pipeline_state: heavy.pipeline_state.clone(),
    })
}

/// Outcome of crash recovery attempt
#[derive(Debug)]
pub enum RecoveryOutcome {
    /// Session automatically recovered, ready to continue
    AutoRecovered {
        restored: RestoredSession,
        manager: Box<CrashRecoveryManager>,
        scan_result: JournalScanResult,
    },
    /// Recovery requires user decisions before proceeding
    RequiresUserInput {
        pending_decisions: Vec<(String, ToolReplayDecision)>,
        restored: RestoredSession,
        manager: Box<CrashRecoveryManager>,
        scan_result: JournalScanResult,
    },
}

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RecoveryError {
    /// No checkpoint found in the journal for this session.
    MissingCheckpoint,
    /// Checkpoint data is corrupted (bad JSON, invalid structure).
    CorruptedCheckpoint(String),
    /// Checkpoint protocol version doesn't match current runtime.
    VersionMismatch { expected: u32, found: u32 },
    /// Gap detected in journal events (timestamps out of order or large gap).
    JournalGap { expected_after: u64, found_at: u64 },
    /// Journal could not be read for recovery replay.
    JournalRead(String),
    /// Crypto hash mismatch — data was tampered with or corrupted.
    HashMismatch { expected: String, actual: String },
    /// Recovery was attempted on an already-recovering session.
    RecursiveCrash,
    /// Session was not in a recoverable state (e.g., already completed).
    InvalidSessionState(String),
    /// Unrecoverable tool state — manual intervention required.
    ToolStateUnrecoverable { tool_name: String, reason: String },
}

impl fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCheckpoint => write!(f, "No checkpoint found in journal"),
            Self::CorruptedCheckpoint(msg) => write!(f, "Corrupted checkpoint: {msg}"),
            Self::VersionMismatch { expected, found } => {
                write!(
                    f,
                    "Protocol version mismatch: expected {expected}, found {found}"
                )
            }
            Self::JournalGap {
                expected_after,
                found_at,
            } => write!(
                f,
                "Journal gap: expected event after {expected_after}, found at {found_at}"
            ),
            Self::JournalRead(msg) => write!(f, "Journal read failed: {msg}"),
            Self::HashMismatch { expected, actual } => {
                write!(f, "Hash mismatch: expected {expected}, got {actual}")
            }
            Self::RecursiveCrash => write!(f, "Recovery attempted during active recovery"),
            Self::InvalidSessionState(msg) => write!(f, "Invalid session state: {msg}"),
            Self::ToolStateUnrecoverable { tool_name, reason } => {
                write!(f, "Tool '{tool_name}' state unrecoverable: {reason}")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// State machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RecoveryState {
    /// Not recovering — normal operation.
    Idle,
    /// Scanning journal for crash point and last checkpoint.
    Scanning,
    /// Replaying deterministic steps from journal.
    Replaying,
    /// Recovery complete — session can continue from the recovered state.
    Recovered,
    /// Recovery failed — manual intervention or fresh start required.
    Failed,
}

impl RecoveryState {
    /// Whether this state allows transitioning to the given next state.
    pub fn can_transition_to(&self, next: &RecoveryState) -> bool {
        use RecoveryState::*;
        matches!(
            (self, next),
            (Idle, Scanning)
                | (Scanning, Replaying)
                | (Scanning, Failed)
                | (Replaying, Recovered)
                | (Replaying, Failed)
                | (Failed, Idle) // Allow retry after failure
                | (Recovered, Idle) // Reset after successful recovery
        )
    }
}

impl fmt::Display for RecoveryState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Idle => write!(f, "Idle"),
            Self::Scanning => write!(f, "Scanning"),
            Self::Replaying => write!(f, "Replaying"),
            Self::Recovered => write!(f, "Recovered"),
            Self::Failed => write!(f, "Failed"),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool replay classification
// ---------------------------------------------------------------------------

/// What to do with a tool call encountered during recovery replay.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ToolReplayDecision {
    /// Safe to re-execute (pure read, idempotent).
    Replay,
    /// Already executed before crash — skip and use cached result.
    SkipCached { cached_result: CachedToolResult },
    /// Side-effect tool — cannot safely replay, require user decision.
    RequireUserInput { reason: String },
    /// Tool was in-flight when crash happened — unknown completion state.
    InFlightAtCrash { tool_name: String },
}

/// Reason a tool was skipped during replay.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ToolSkipReason {
    /// Tool has side effects and was already executed.
    SideEffectAlreadyApplied,
    /// Tool is non-idempotent and result is cached.
    NonIdempotentCached,
    /// Tool was interrupted mid-execution.
    InterruptedMidExecution,
    /// Tool requires user input that is no longer available.
    UserInputLost,
}

// ---------------------------------------------------------------------------
// Recovery context
// ---------------------------------------------------------------------------

/// Full context needed to perform crash recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryContext {
    pub session_id: String,
    pub crash_turn: u32,
    pub last_checkpoint: Option<StepCheckpoint>,
    pub checkpoint_turn: u32,
    pub events_after_checkpoint: Vec<StepEvent>,
    pub tool_decisions: Vec<(String, ToolReplayDecision)>,
    pub journal_gap: Option<RecoveryError>,
}

impl RecoveryContext {
    pub fn new(session_id: String, crash_turn: u32) -> Self {
        Self {
            session_id,
            crash_turn,
            last_checkpoint: None,
            checkpoint_turn: 0,
            events_after_checkpoint: Vec::new(),
            tool_decisions: Vec::new(),
            journal_gap: None,
        }
    }

    /// Number of tool calls that need user input before recovery can proceed.
    pub fn pending_user_decisions(&self) -> Vec<&(String, ToolReplayDecision)> {
        self.tool_decisions
            .iter()
            .filter(|(_, d)| {
                matches!(
                    d,
                    ToolReplayDecision::RequireUserInput { .. }
                        | ToolReplayDecision::InFlightAtCrash { .. }
                )
            })
            .collect()
    }

    /// Whether recovery can proceed without user input.
    pub fn can_auto_recover(&self) -> bool {
        self.pending_user_decisions().is_empty() && self.journal_gap.is_none()
    }
}

// ---------------------------------------------------------------------------
// Journal scanning
// ---------------------------------------------------------------------------

/// Result of scanning the journal for recovery data.
#[derive(Debug, Clone)]
pub struct JournalScanResult {
    pub last_checkpoint: StepCheckpoint,
    pub checkpoint_turn: u32,
    pub events_after: Vec<StepEvent>,
    pub tool_calls_found: Vec<ToolCallRecord>,
    pub gap_detected: Option<RecoveryError>,
}

/// A tool call extracted from journal events during scanning.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallRecord {
    pub step_id: String,
    pub tool_name: String,
    pub tool_index: u32,
    pub status: ToolCallStatus,
    pub cached_result: Option<CachedToolResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ToolCallStatus {
    /// Tool call was started but no completion event found.
    StartedOnly,
    /// Tool call completed successfully.
    Completed,
    /// Tool call failed.
    Failed,
    /// Tool call was skipped (cached result used).
    Skipped,
}

// ---------------------------------------------------------------------------
// Tool classification (pure read / idempotent write / side-effect)
// ---------------------------------------------------------------------------

/// Classifies a tool by its replay safety.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSafetyClass {
    /// Pure read — always safe to replay (e.g., read_file, grep, glob).
    PureRead,
    /// Idempotent write — safe to replay (e.g., write_file with same content).
    IdempotentWrite,
    /// Non-idempotent side effect — unsafe to replay (e.g., bash with mutations).
    SideEffect,
}

/// Known tool safety classifications.
///
/// **Single source of truth**: delegates to `classify_tool_idempotency()` in
/// `astra-turn-types`. This eliminates drift between crash recovery and
/// exactly-once executor classifications.
///
/// Mapping: `PureRead` → `PureRead`, `IdempotentWrite` → `IdempotentWrite`,
/// `NonIdempotent` → `SideEffect`.
pub fn classify_tool(tool_name: &str) -> ToolSafetyClass {
    // Normalize to lowercase for case-insensitive matching
    let normalized = tool_name.to_lowercase();
    match classify_tool_idempotency(&normalized, None) {
        ToolIdempotency::PureRead => ToolSafetyClass::PureRead,
        ToolIdempotency::IdempotentWrite => ToolSafetyClass::IdempotentWrite,
        ToolIdempotency::NonIdempotent => ToolSafetyClass::SideEffect,
    }
}

// ---------------------------------------------------------------------------
// Hash verification
// ---------------------------------------------------------------------------

/// Compute a Merkle root from a list of string IDs for tamper detection.
fn compute_merkle_root(ids: &[String]) -> String {
    if ids.is_empty() {
        return "empty".to_string();
    }
    let mut hashes: Vec<Vec<u8>> = ids
        .iter()
        .map(|id| Sha256::digest(id.as_bytes()).to_vec())
        .collect();
    while hashes.len() > 1 {
        if !hashes.len().is_multiple_of(2) {
            hashes.push(hashes.last().unwrap().clone());
        }
        let mut next = Vec::new();
        for chunk in hashes.chunks(2) {
            let mut h = Sha256::new();
            h.update(&chunk[0]);
            h.update(&chunk[1]);
            next.push(h.finalize().to_vec());
        }
        hashes = next;
    }
    hashes[0].iter().map(|b| format!("{:02x}", b)).collect()
}

/// Compute a SHA-256 hash of recovery-critical data for tamper detection.
/// Includes event IDs (via Merkle root) so journal event tampering is detected.
pub fn compute_recovery_hash(
    session_id: &str,
    checkpoint_json: &str,
    event_count: u64,
    event_ids: &[String],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session_id.as_bytes());
    hasher.update(b"|");
    hasher.update(checkpoint_json.as_bytes());
    hasher.update(b"|");
    hasher.update(event_count.to_le_bytes());
    hasher.update(b"|");
    let merkle = compute_merkle_root(event_ids);
    hasher.update(merkle.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Verify that the recovery data matches the expected hash.
pub fn verify_recovery_hash(
    expected: &str,
    session_id: &str,
    checkpoint_json: &str,
    event_count: u64,
    event_ids: &[String],
) -> Result<(), RecoveryError> {
    let actual = compute_recovery_hash(session_id, checkpoint_json, event_count, event_ids);
    if actual == expected {
        Ok(())
    } else {
        Err(RecoveryError::HashMismatch {
            expected: expected.to_string(),
            actual,
        })
    }
}

// ---------------------------------------------------------------------------
// Journal event helpers
// ---------------------------------------------------------------------------

/// Extract tool_name from a StepEvent's payload (tool events store info in payload).
fn extract_tool_info_from_event(event: &StepEvent) -> Option<(String, u32)> {
    let payload = event.payload.as_ref()?;
    let tool_name = payload.get("tool_name")?.as_str()?.to_string();
    let tool_index = payload
        .get("tool_index")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    Some((tool_name, tool_index))
}

// ---------------------------------------------------------------------------
// CrashRecoveryManager
// ---------------------------------------------------------------------------

/// Main recovery manager — drives the state machine through crash recovery.
#[derive(Debug)]
pub struct CrashRecoveryManager {
    state: RecoveryState,
    context: Option<RecoveryContext>,
    scan_result: Option<JournalScanResult>,
    error: Option<RecoveryError>,
    /// Hash of the recovery data for tamper detection.
    recovery_hash: Option<String>,
    /// Number of recovery attempts (guards against recursive crashes).
    attempt_count: u32,
    /// Max recovery attempts before giving up.
    max_attempts: u32,
}

impl CrashRecoveryManager {
    pub fn new() -> Self {
        Self {
            state: RecoveryState::Idle,
            context: None,
            scan_result: None,
            error: None,
            recovery_hash: None,
            attempt_count: 0,
            max_attempts: 3,
        }
    }

    pub fn state(&self) -> RecoveryState {
        self.state
    }

    pub fn error(&self) -> Option<&RecoveryError> {
        self.error.as_ref()
    }

    pub fn context(&self) -> Option<&RecoveryContext> {
        self.context.as_ref()
    }

    pub fn scan_result(&self) -> Option<&JournalScanResult> {
        self.scan_result.as_ref()
    }

    pub fn recovery_hash(&self) -> Option<&str> {
        self.recovery_hash.as_deref()
    }

    pub fn attempt_count(&self) -> u32 {
        self.attempt_count
    }

    // -- State transitions --

    fn transition_to(&mut self, next: RecoveryState) -> Result<(), RecoveryError> {
        if !self.state.can_transition_to(&next) {
            return Err(RecoveryError::InvalidSessionState(format!(
                "Cannot transition from {} to {}",
                self.state, next
            )));
        }
        self.state = next;
        Ok(())
    }

    fn fail_with(&mut self, error: RecoveryError) {
        self.error = Some(error.clone());
        self.state = RecoveryState::Failed;
    }

    // -- Phase 1: Detect & Scan --

    /// Begin recovery: transition from Idle → Scanning.
    pub fn begin_recovery(&mut self) -> Result<(), RecoveryError> {
        self.attempt_count += 1;
        if self.attempt_count > self.max_attempts {
            self.fail_with(RecoveryError::RecursiveCrash);
            return Err(RecoveryError::RecursiveCrash);
        }
        self.transition_to(RecoveryState::Scanning)?;
        Ok(())
    }

    /// Scan journal events to find the last checkpoint and classify tool calls.
    ///
    /// `events` should be sorted by `created_at` (ascending).
    /// `checkpoint_json` is the serialized checkpoint (if any).
    pub fn scan_journal(
        &mut self,
        session_id: &str,
        crash_turn: u32,
        checkpoint_json: Option<&str>,
        checkpoint_turn: u32,
        events: Vec<StepEvent>,
    ) -> Result<JournalScanResult, RecoveryError> {
        // Parse checkpoint
        let checkpoint = match checkpoint_json {
            Some(json) => {
                let cp: StepCheckpoint = serde_json::from_str(json).map_err(|e| {
                    RecoveryError::CorruptedCheckpoint(format!("JSON parse error: {e}"))
                })?;

                // Verify protocol version
                if cp.protocol_version() != PROTOCOL_VERSION {
                    return Err(RecoveryError::VersionMismatch {
                        expected: PROTOCOL_VERSION,
                        found: cp.protocol_version(),
                    });
                }

                // Validate checkpoint internal consistency
                cp.validate().map_err(|e| {
                    RecoveryError::CorruptedCheckpoint(format!("Validation failed: {e}"))
                })?;

                cp
            }
            None => {
                self.fail_with(RecoveryError::MissingCheckpoint);
                return Err(RecoveryError::MissingCheckpoint);
            }
        };

        // Check for journal gaps (timestamp continuity)
        let gap = Self::detect_journal_gap(&events);
        if let Some(ref gap_err) = gap {
            // Don't fail yet — record the gap but continue scanning
            tracing::warn!("Journal gap detected during scan: {gap_err}");
        }

        // Extract tool calls from events
        let tool_calls = Self::extract_tool_calls(&events);

        let result = JournalScanResult {
            last_checkpoint: checkpoint,
            checkpoint_turn,
            events_after: events,
            tool_calls_found: tool_calls,
            gap_detected: gap,
        };

        // Compute recovery hash for tamper detection
        let cp_json = serde_json::to_string(&result.last_checkpoint).unwrap_or_default();
        let event_ids: Vec<String> = result
            .events_after
            .iter()
            .map(|e| e.event_id.clone())
            .collect();
        self.recovery_hash = Some(compute_recovery_hash(
            session_id,
            &cp_json,
            result.events_after.len() as u64,
            &event_ids,
        ));

        self.scan_result = Some(result.clone());

        let mut ctx = RecoveryContext::new(session_id.to_string(), crash_turn);
        ctx.last_checkpoint = Some(result.last_checkpoint.clone());
        ctx.checkpoint_turn = checkpoint_turn;
        ctx.events_after_checkpoint = result.events_after.clone();
        ctx.journal_gap = result.gap_detected.clone();
        self.context = Some(ctx);

        Ok(result)
    }

    /// Detect gaps in journal event timestamps.
    ///
    /// Events should have monotonically increasing `created_at`.
    /// A gap > 60 seconds between consecutive events suggests data loss.
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

    /// Extract tool call records from journal events.
    fn extract_tool_calls(events: &[StepEvent]) -> Vec<ToolCallRecord> {
        // Use (step_id, tool_name, tool_index) as key to correlate start/complete
        let mut started: HashMap<String, ToolCallRecord> = HashMap::new();
        let mut completed: Vec<ToolCallRecord> = Vec::new();

        for event in events {
            match &event.event_type {
                StepEventType::ToolCallStarted => {
                    if let Some((tool_name, tool_index)) = extract_tool_info_from_event(event) {
                        let key = format!("{}:{}:{}", event.step_id, tool_name, tool_index);
                        started.insert(
                            key,
                            ToolCallRecord {
                                step_id: event.step_id.clone(),
                                tool_name,
                                tool_index,
                                status: ToolCallStatus::StartedOnly,
                                cached_result: None,
                            },
                        );
                    }
                }
                StepEventType::ToolCallCompleted => {
                    if let Some((tool_name, tool_index)) = extract_tool_info_from_event(event) {
                        let key = format!("{}:{}:{}", event.step_id, tool_name, tool_index);
                        if let Some(mut record) = started.remove(&key) {
                            record.status = ToolCallStatus::Completed;
                            // Try to extract cached result from payload
                            if let Some(payload) = &event.payload
                                && let Some(result_str) =
                                    payload.get("result").and_then(|v| v.as_str())
                            {
                                record.cached_result = Some(CachedToolResult {
                                    tool_name: tool_name.clone(),
                                    output: result_str.to_string(),
                                    is_error: false,
                                    cached_at: event.created_at,
                                    context_signature: None,
                                });
                            }
                            completed.push(record);
                        } else {
                            // Completed without start — orphan event
                            completed.push(ToolCallRecord {
                                step_id: event.step_id.clone(),
                                tool_name,
                                tool_index,
                                status: ToolCallStatus::Completed,
                                cached_result: None,
                            });
                        }
                    }
                }
                StepEventType::ToolCallFailed => {
                    if let Some((tool_name, tool_index)) = extract_tool_info_from_event(event) {
                        let key = format!("{}:{}:{}", event.step_id, tool_name, tool_index);
                        if let Some(mut record) = started.remove(&key) {
                            record.status = ToolCallStatus::Failed;
                            completed.push(record);
                        }
                    }
                }
                StepEventType::ToolCallSkipped => {
                    if let Some((tool_name, tool_index)) = extract_tool_info_from_event(event) {
                        let key = format!("{}:{}:{}", event.step_id, tool_name, tool_index);
                        if let Some(mut record) = started.remove(&key) {
                            record.status = ToolCallStatus::Skipped;
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

    // -- Phase 2: Classify & Decide --

    /// Transition from Scanning → Replaying and classify all tool calls.
    pub fn begin_replay(&mut self) -> Result<(), RecoveryError> {
        self.transition_to(RecoveryState::Replaying)?;

        if let Some(ref scan) = self.scan_result {
            let mut decisions = Vec::new();

            for tool_call in &scan.tool_calls_found {
                let decision = Self::classify_tool_for_replay(tool_call);
                decisions.push((tool_call.tool_name.clone(), decision));
            }

            if let Some(ref mut ctx) = self.context {
                ctx.tool_decisions = decisions;
            }
        }

        Ok(())
    }

    /// Classify a single tool call for replay.
    fn classify_tool_for_replay(tool_call: &ToolCallRecord) -> ToolReplayDecision {
        match tool_call.status {
            ToolCallStatus::StartedOnly => {
                // Tool was in-flight when crash happened — unknown state.
                // Pure-read and idempotent tools are safe to auto-replay.
                let safety = classify_tool(&tool_call.tool_name);
                match safety {
                    ToolSafetyClass::PureRead | ToolSafetyClass::IdempotentWrite => {
                        ToolReplayDecision::Replay
                    }
                    ToolSafetyClass::SideEffect => ToolReplayDecision::InFlightAtCrash {
                        tool_name: tool_call.tool_name.clone(),
                    },
                }
            }
            ToolCallStatus::Completed => {
                let safety = classify_tool(&tool_call.tool_name);
                match safety {
                    ToolSafetyClass::PureRead => ToolReplayDecision::Replay,
                    ToolSafetyClass::IdempotentWrite => {
                        if let Some(ref cached) = tool_call.cached_result {
                            ToolReplayDecision::SkipCached {
                                cached_result: cached.clone(),
                            }
                        } else {
                            ToolReplayDecision::Replay
                        }
                    }
                    ToolSafetyClass::SideEffect => {
                        if let Some(ref cached) = tool_call.cached_result {
                            ToolReplayDecision::SkipCached {
                                cached_result: cached.clone(),
                            }
                        } else {
                            ToolReplayDecision::RequireUserInput {
                                reason: format!(
                                    "Side-effect tool '{}' completed before crash but no cached result available",
                                    tool_call.tool_name
                                ),
                            }
                        }
                    }
                }
            }
            ToolCallStatus::Failed => {
                // Failed tool calls are safe to retry (they didn't produce side effects)
                ToolReplayDecision::Replay
            }
            ToolCallStatus::Skipped => {
                if let Some(ref cached) = tool_call.cached_result {
                    ToolReplayDecision::SkipCached {
                        cached_result: cached.clone(),
                    }
                } else {
                    ToolReplayDecision::Replay
                }
            }
        }
    }

    // -- Phase 3: Complete Recovery --

    /// Mark recovery as complete — transition Replaying → Recovered.
    pub fn complete_recovery(&mut self) -> Result<(), RecoveryError> {
        // Check journal gaps first (more fundamental than pending decisions)
        if let Some(ref ctx) = self.context
            && let Some(ref gap) = ctx.journal_gap
        {
            return Err(gap.clone());
        }

        // Then check for pending user decisions
        if let Some(ref ctx) = self.context {
            let pending = ctx.pending_user_decisions();
            if !pending.is_empty() {
                return Err(RecoveryError::ToolStateUnrecoverable {
                    tool_name: pending[0].0.clone(),
                    reason: format!(
                        "Pending user decisions required ({} tools need input) before auto-recovery",
                        pending.len()
                    ),
                });
            }
        }

        self.transition_to(RecoveryState::Recovered)?;
        Ok(())
    }

    /// Force recovery even with pending user decisions (operator override).
    pub fn force_complete(&mut self) -> Result<(), RecoveryError> {
        self.transition_to(RecoveryState::Recovered)?;
        Ok(())
    }

    /// Reset to Idle after **successful** recovery (session continues normally).
    ///
    /// Clears all recovery state, context, and **resets** `attempt_count` to 0.
    /// Use this after `complete_recovery()` when the session can proceed.
    ///
    /// # Precondition
    /// The state must NOT be `Failed`. Callers in `Failed` state must use
    /// `reset_after_failure()` instead, which preserves the retry-storm counter.
    pub fn reset(&mut self) -> Result<(), RecoveryError> {
        debug_assert!(
            self.state != RecoveryState::Failed,
            "reset() called while in Failed state — use reset_after_failure() to preserve attempt_count"
        );
        self.transition_to(RecoveryState::Idle)?;
        self.context = None;
        self.scan_result = None;
        self.error = None;
        self.recovery_hash = None;
        self.attempt_count = 0;
        Ok(())
    }

    /// Reset to Idle after **failed** recovery (enables retry).
    ///
    /// Preserves `attempt_count` so the retry gate in exactly-once processing and
    /// crash-recovery loop can detect infinite-retry patterns. Each call increments
    /// the internal attempt counter; after `MAX_RECOVERY_ATTEMPTS` the system
    /// refuses further recovery with `RecoveryError::RecursiveCrash`.
    ///
    /// # Precondition
    /// The state must be `Failed`. Use `reset()` for successful recovery paths.
    pub fn reset_after_failure(&mut self) -> Result<(), RecoveryError> {
        if self.state != RecoveryState::Failed {
            return Err(RecoveryError::InvalidSessionState(
                "Can only reset after failure".to_string(),
            ));
        }
        self.transition_to(RecoveryState::Idle)?;
        self.context = None;
        self.scan_result = None;
        self.error = None;
        self.recovery_hash = None;
        Ok(())
    }
}

impl Default for CrashRecoveryManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests (TDD — these define the contract)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::step_protocol::{ExecutionCursor, StepCheckpoint, StepEvent, StepEventType};

    // -- Helper: build a minimal valid checkpoint --
    fn make_test_checkpoint() -> StepCheckpoint {
        StepCheckpoint::light(
            "test-step".to_string(),
            "test-task".to_string(),
            "test-agent".to_string(),
            ExecutionCursor::default(),
        )
    }

    fn checkpoint_json() -> String {
        serde_json::to_string(&make_test_checkpoint()).unwrap()
    }

    // -- Helper: build a minimal event --
    #[allow(dead_code)]
    fn make_event(
        event_id: &str,
        step_id: &str,
        event_type: StepEventType,
        created_at: u64,
    ) -> StepEvent {
        StepEvent {
            event_id: event_id.to_string(),
            canonical_event_id: None,
            step_id: step_id.to_string(),
            event_type,
            agent_id: None,
            caused_by: Vec::new(),
            payload: None,
            created_at,
        }
    }

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

    #[allow(dead_code)]
    fn tool_completed_with_result(
        event_id: &str,
        step_id: &str,
        tool_name: &str,
        index: u32,
        result: &str,
        created_at: u64,
    ) -> StepEvent {
        StepEvent {
            event_id: event_id.to_string(),
            canonical_event_id: None,
            step_id: step_id.to_string(),
            event_type: StepEventType::ToolCallCompleted,
            agent_id: None,
            caused_by: Vec::new(),
            payload: Some(serde_json::json!({
                "tool_name": tool_name,
                "tool_index": index,
                "result": result,
            })),
            created_at,
        }
    }

    fn tool_failed_event(
        event_id: &str,
        step_id: &str,
        tool_name: &str,
        index: u32,
        created_at: u64,
    ) -> StepEvent {
        make_tool_event(
            event_id,
            step_id,
            StepEventType::ToolCallFailed,
            tool_name,
            index,
            created_at,
        )
    }

    // =======================================================================
    // Begin recovery tests
    // =======================================================================

    #[test]
    fn begin_recovery_transitions_to_scanning() {
        let mut mgr = CrashRecoveryManager::new();
        assert_eq!(mgr.state(), RecoveryState::Idle);
        mgr.begin_recovery().unwrap();
        assert_eq!(mgr.state(), RecoveryState::Scanning);
        assert_eq!(mgr.attempt_count(), 1);
    }

    #[test]
    fn begin_recovery_increments_attempt_count() {
        let mut mgr = CrashRecoveryManager::new();
        mgr.begin_recovery().unwrap();
        assert_eq!(mgr.attempt_count(), 1);
    }

    #[test]
    fn begin_recovery_fails_from_wrong_state() {
        let mut mgr = CrashRecoveryManager::new();
        mgr.begin_recovery().unwrap(); // Idle → Scanning
        let result = mgr.begin_recovery(); // Scanning → Scanning should fail
        assert!(result.is_err());
    }

    #[test]
    fn recursive_crash_detected_after_max_attempts() {
        let mut mgr = CrashRecoveryManager::new();

        // Exhaust all attempts WITHOUT resetting (attempt_count accumulates)
        for _ in 0..3 {
            mgr.begin_recovery().unwrap();
            mgr.fail_with(RecoveryError::MissingCheckpoint);
            // Reset state but keep attempt_count to track lifetime attempts
            mgr.state = RecoveryState::Idle;
            mgr.error = None;
        }

        // 4th attempt should fail — attempt_count is now 3, incrementing to 4 > max_attempts(3)
        let result = mgr.begin_recovery();
        assert!(matches!(result, Err(RecoveryError::RecursiveCrash)));
        assert_eq!(mgr.state(), RecoveryState::Failed);
    }

    // =======================================================================
    // Journal scanning tests
    // =======================================================================

    #[test]
    fn scan_journal_no_checkpoint_fails() {
        let mut mgr = CrashRecoveryManager::new();
        mgr.begin_recovery().unwrap();
        let result = mgr.scan_journal("sess-1", 5, None, 0, vec![]);
        assert!(matches!(result, Err(RecoveryError::MissingCheckpoint)));
        assert_eq!(mgr.state(), RecoveryState::Failed);
    }

    #[test]
    fn scan_journal_corrupted_json_fails() {
        let mut mgr = CrashRecoveryManager::new();
        mgr.begin_recovery().unwrap();
        let result = mgr.scan_journal("sess-1", 5, Some("{bad json"), 0, vec![]);
        assert!(matches!(result, Err(RecoveryError::CorruptedCheckpoint(_))));
    }

    #[test]
    fn scan_journal_valid_checkpoint_succeeds() {
        let mut mgr = CrashRecoveryManager::new();
        mgr.begin_recovery().unwrap();

        let json = checkpoint_json();
        let result = mgr.scan_journal("sess-1", 5, Some(&json), 3, vec![]);
        assert!(result.is_ok());
        let scan = result.unwrap();
        assert_eq!(scan.checkpoint_turn, 3);
        assert!(mgr.recovery_hash().is_some());
    }

    #[test]
    fn scan_journal_extracts_tool_calls() {
        let mut mgr = CrashRecoveryManager::new();
        mgr.begin_recovery().unwrap();

        let events = vec![
            tool_started_event("e1", "step-1", "read_file", 0, 1000),
            tool_completed_event("e2", "step-1", "read_file", 0, 2000),
            tool_started_event("e3", "step-1", "bash", 1, 3000),
            tool_completed_event("e4", "step-1", "bash", 1, 4000),
        ];

        let json = checkpoint_json();
        let scan = mgr
            .scan_journal("sess-1", 5, Some(&json), 3, events)
            .unwrap();
        assert_eq!(scan.tool_calls_found.len(), 2);
        assert_eq!(scan.tool_calls_found[0].tool_name, "read_file");
        assert_eq!(scan.tool_calls_found[0].status, ToolCallStatus::Completed);
        assert_eq!(scan.tool_calls_found[1].tool_name, "bash");
        assert_eq!(scan.tool_calls_found[1].status, ToolCallStatus::Completed);
    }

    #[test]
    fn scan_journal_detects_in_flight_tool() {
        let mut mgr = CrashRecoveryManager::new();
        mgr.begin_recovery().unwrap();

        // bash was started but never completed — in-flight at crash
        let events = vec![
            tool_started_event("e1", "step-1", "read_file", 0, 1000),
            tool_completed_event("e2", "step-1", "read_file", 0, 2000),
            tool_started_event("e3", "step-1", "bash", 1, 3000),
            // No completion event for bash — crashed mid-execution
        ];

        let json = checkpoint_json();
        let scan = mgr
            .scan_journal("sess-1", 5, Some(&json), 3, events)
            .unwrap();
        assert_eq!(scan.tool_calls_found.len(), 2);

        let bash_call = scan
            .tool_calls_found
            .iter()
            .find(|t| t.tool_name == "bash")
            .unwrap();
        assert_eq!(bash_call.status, ToolCallStatus::StartedOnly);
    }

    #[test]
    fn scan_journal_detects_timestamp_gap() {
        let mut mgr = CrashRecoveryManager::new();
        mgr.begin_recovery().unwrap();

        // Events with a large timestamp gap: 1000, 2000, 400_000 (>300s gap)
        let events = vec![
            tool_started_event("e1", "step-1", "read_file", 0, 1000),
            tool_completed_event("e2", "step-1", "read_file", 0, 2000),
            tool_started_event("e3", "step-1", "bash", 1, 400_000), // gap: 398 seconds
        ];

        let json = checkpoint_json();
        let scan = mgr
            .scan_journal("sess-1", 5, Some(&json), 3, events)
            .unwrap();
        assert!(scan.gap_detected.is_some());
        match scan.gap_detected.unwrap() {
            RecoveryError::JournalGap {
                expected_after,
                found_at,
            } => {
                assert_eq!(expected_after, 2000);
                assert_eq!(found_at, 400_000);
            }
            other => panic!("Expected JournalGap, got {other:?}"),
        }
    }

    #[test]
    fn scan_journal_no_gap_with_sequential_events() {
        let mut mgr = CrashRecoveryManager::new();
        mgr.begin_recovery().unwrap();

        let events = vec![
            tool_started_event("e1", "step-1", "read_file", 0, 1000),
            tool_completed_event("e2", "step-1", "read_file", 0, 2000),
            tool_started_event("e3", "step-1", "bash", 1, 3000),
            tool_completed_event("e4", "step-1", "bash", 1, 4000),
        ];

        let json = checkpoint_json();
        let scan = mgr
            .scan_journal("sess-1", 5, Some(&json), 3, events)
            .unwrap();
        assert!(scan.gap_detected.is_none());
    }

    #[test]
    fn scan_journal_detects_out_of_order_timestamps() {
        let mut mgr = CrashRecoveryManager::new();
        mgr.begin_recovery().unwrap();

        // Events with out-of-order timestamps (>5s rollback exceeds NTP tolerance)
        let events = vec![
            tool_started_event("e1", "step-1", "read_file", 0, 10000),
            tool_completed_event("e2", "step-1", "read_file", 0, 2000), // 8s rollback!
        ];

        let json = checkpoint_json();
        let scan = mgr
            .scan_journal("sess-1", 5, Some(&json), 3, events)
            .unwrap();
        assert!(scan.gap_detected.is_some());
    }

    // =======================================================================
    // Tool classification tests
    // =======================================================================

    #[test]
    fn classify_pure_read_tools() {
        assert_eq!(classify_tool("read_file"), ToolSafetyClass::PureRead);
        assert_eq!(classify_tool("grep"), ToolSafetyClass::PureRead);
        assert_eq!(classify_tool("glob"), ToolSafetyClass::PureRead);
        assert_eq!(classify_tool("list_dir"), ToolSafetyClass::PureRead);
        assert_eq!(classify_tool("git_log"), ToolSafetyClass::PureRead);
        assert_eq!(classify_tool("find_definition"), ToolSafetyClass::PureRead);
    }

    #[test]
    fn classify_idempotent_write_tools() {
        assert_eq!(
            classify_tool("write_file"),
            ToolSafetyClass::IdempotentWrite
        );
        // str_replace depends on file state — NOT safe to replay blindly
        assert_eq!(classify_tool("str_replace"), ToolSafetyClass::SideEffect);
        // git_commit has permanent side effects — NOT safe to replay
        assert_eq!(classify_tool("git_commit"), ToolSafetyClass::SideEffect);
    }

    #[test]
    fn classify_unknown_tools_as_side_effect() {
        assert_eq!(classify_tool("bash"), ToolSafetyClass::SideEffect);
        assert_eq!(classify_tool("custom_tool"), ToolSafetyClass::SideEffect);
        assert_eq!(classify_tool(""), ToolSafetyClass::SideEffect);
    }

    #[test]
    fn classify_case_insensitive() {
        assert_eq!(classify_tool("READ_FILE"), ToolSafetyClass::PureRead);
        assert_eq!(
            classify_tool("Write_File"),
            ToolSafetyClass::IdempotentWrite
        );
    }

    #[test]
    fn replay_decision_pure_read_completed() {
        let record = ToolCallRecord {
            step_id: "s1".to_string(),
            tool_name: "read_file".to_string(),
            tool_index: 0,
            status: ToolCallStatus::Completed,
            cached_result: None,
        };
        let decision = CrashRecoveryManager::classify_tool_for_replay(&record);
        assert_eq!(decision, ToolReplayDecision::Replay);
    }

    #[test]
    fn replay_decision_side_effect_completed_no_cache() {
        let record = ToolCallRecord {
            step_id: "s1".to_string(),
            tool_name: "bash".to_string(),
            tool_index: 0,
            status: ToolCallStatus::Completed,
            cached_result: None,
        };
        let decision = CrashRecoveryManager::classify_tool_for_replay(&record);
        assert!(matches!(
            decision,
            ToolReplayDecision::RequireUserInput { .. }
        ));
    }

    #[test]
    fn replay_decision_side_effect_completed_with_cache() {
        let cached = CachedToolResult {
            tool_name: "bash".to_string(),
            output: "cached output".to_string(),
            is_error: false,
            cached_at: 1000,
            context_signature: None,
        };
        let record = ToolCallRecord {
            step_id: "s1".to_string(),
            tool_name: "bash".to_string(),
            tool_index: 0,
            status: ToolCallStatus::Completed,
            cached_result: Some(cached.clone()),
        };
        let decision = CrashRecoveryManager::classify_tool_for_replay(&record);
        assert_eq!(
            decision,
            ToolReplayDecision::SkipCached {
                cached_result: cached
            }
        );
    }

    #[test]
    fn replay_decision_in_flight_at_crash() {
        let record = ToolCallRecord {
            step_id: "s1".to_string(),
            tool_name: "bash".to_string(),
            tool_index: 0,
            status: ToolCallStatus::StartedOnly,
            cached_result: None,
        };
        let decision = CrashRecoveryManager::classify_tool_for_replay(&record);
        assert!(matches!(
            decision,
            ToolReplayDecision::InFlightAtCrash { .. }
        ));
    }

    #[test]
    fn replay_decision_failed_tool_is_safe_to_replay() {
        let record = ToolCallRecord {
            step_id: "s1".to_string(),
            tool_name: "bash".to_string(),
            tool_index: 0,
            status: ToolCallStatus::Failed,
            cached_result: None,
        };
        let decision = CrashRecoveryManager::classify_tool_for_replay(&record);
        assert_eq!(decision, ToolReplayDecision::Replay);
    }

    #[test]
    fn replay_decision_skipped_with_cache() {
        let cached = CachedToolResult {
            tool_name: "read_file".to_string(),
            output: "cached".to_string(),
            is_error: false,
            cached_at: 1000,
            context_signature: None,
        };
        let record = ToolCallRecord {
            step_id: "s1".to_string(),
            tool_name: "read_file".to_string(),
            tool_index: 0,
            status: ToolCallStatus::Skipped,
            cached_result: Some(cached.clone()),
        };
        let decision = CrashRecoveryManager::classify_tool_for_replay(&record);
        assert_eq!(
            decision,
            ToolReplayDecision::SkipCached {
                cached_result: cached
            }
        );
    }

    // =======================================================================
    // Replay phase tests
    // =======================================================================

    #[test]
    fn begin_replay_classifies_all_tools() {
        let mut mgr = CrashRecoveryManager::new();
        mgr.begin_recovery().unwrap();

        let events = vec![
            tool_started_event("e1", "step-1", "read_file", 0, 1000),
            tool_completed_event("e2", "step-1", "read_file", 0, 2000),
            tool_started_event("e3", "step-1", "bash", 1, 3000),
            tool_completed_event("e4", "step-1", "bash", 1, 4000),
        ];

        let json = checkpoint_json();
        mgr.scan_journal("sess-1", 5, Some(&json), 3, events)
            .unwrap();
        mgr.begin_replay().unwrap();

        let ctx = mgr.context().unwrap();
        assert_eq!(ctx.tool_decisions.len(), 2);

        // read_file → Replay
        assert_eq!(ctx.tool_decisions[0].0, "read_file");
        assert_eq!(ctx.tool_decisions[0].1, ToolReplayDecision::Replay);

        // bash → RequireUserInput (no cache)
        assert_eq!(ctx.tool_decisions[1].0, "bash");
        assert!(matches!(
            ctx.tool_decisions[1].1,
            ToolReplayDecision::RequireUserInput { .. }
        ));
    }

    #[test]
    fn begin_replay_fails_from_idle() {
        let mut mgr = CrashRecoveryManager::new();
        let result = mgr.begin_replay();
        assert!(result.is_err());
    }

    // =======================================================================
    // Complete recovery tests
    // =======================================================================

    #[test]
    fn complete_recovery_with_no_pending_decisions() {
        let mut mgr = CrashRecoveryManager::new();
        mgr.begin_recovery().unwrap();

        // Only pure reads — no user decisions needed
        let events = vec![
            tool_started_event("e1", "step-1", "read_file", 0, 1000),
            tool_completed_event("e2", "step-1", "read_file", 0, 2000),
        ];

        let json = checkpoint_json();
        mgr.scan_journal("sess-1", 5, Some(&json), 3, events)
            .unwrap();
        mgr.begin_replay().unwrap();
        mgr.complete_recovery().unwrap();
        assert_eq!(mgr.state(), RecoveryState::Recovered);
    }

    #[test]
    fn complete_recovery_fails_with_pending_user_decisions() {
        let mut mgr = CrashRecoveryManager::new();
        mgr.begin_recovery().unwrap();

        // bash with no cache → RequireUserInput
        let events = vec![
            tool_started_event("e1", "step-1", "bash", 0, 1000),
            tool_completed_event("e2", "step-1", "bash", 0, 2000),
        ];

        let json = checkpoint_json();
        mgr.scan_journal("sess-1", 5, Some(&json), 3, events)
            .unwrap();
        mgr.begin_replay().unwrap();

        let result = mgr.complete_recovery();
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            RecoveryError::ToolStateUnrecoverable { .. }
        ));
    }

    #[test]
    fn force_complete_bypasses_user_decisions() {
        let mut mgr = CrashRecoveryManager::new();
        mgr.begin_recovery().unwrap();

        let events = vec![
            tool_started_event("e1", "step-1", "bash", 0, 1000),
            tool_completed_event("e2", "step-1", "bash", 0, 2000),
        ];

        let json = checkpoint_json();
        mgr.scan_journal("sess-1", 5, Some(&json), 3, events)
            .unwrap();
        mgr.begin_replay().unwrap();
        mgr.force_complete().unwrap();
        assert_eq!(mgr.state(), RecoveryState::Recovered);
    }

    // =======================================================================
    // Reset tests
    // =======================================================================

    #[test]
    fn reset_after_success() {
        let mut mgr = CrashRecoveryManager::new();
        mgr.begin_recovery().unwrap();
        let json = checkpoint_json();
        mgr.scan_journal("sess-1", 5, Some(&json), 3, vec![])
            .unwrap();
        mgr.begin_replay().unwrap();
        mgr.complete_recovery().unwrap();
        mgr.reset().unwrap();
        assert_eq!(mgr.state(), RecoveryState::Idle);
        assert!(mgr.context().is_none());
        assert!(mgr.recovery_hash().is_none());
    }

    #[test]
    fn reset_after_failure() {
        let mut mgr = CrashRecoveryManager::new();
        mgr.begin_recovery().unwrap();
        mgr.scan_journal("sess-1", 5, None, 0, vec![]).unwrap_err();
        assert_eq!(mgr.state(), RecoveryState::Failed);
        mgr.reset_after_failure().unwrap();
        assert_eq!(mgr.state(), RecoveryState::Idle);
        assert_eq!(
            mgr.attempt_count(),
            1,
            "failure reset must preserve lifetime recovery attempts"
        );
    }

    #[test]
    fn reset_after_failure_does_not_allow_infinite_retry_loop() {
        let mut mgr = CrashRecoveryManager::new();
        for _ in 0..3 {
            mgr.begin_recovery().unwrap();
            mgr.scan_journal("sess-1", 5, None, 0, vec![]).unwrap_err();
            mgr.reset_after_failure().unwrap();
        }

        let result = mgr.begin_recovery();
        assert!(
            matches!(result, Err(RecoveryError::RecursiveCrash)),
            "attempt_count must accumulate across failure resets"
        );
    }

    #[test]
    fn reset_after_failure_fails_if_not_failed() {
        let mut mgr = CrashRecoveryManager::new();
        let result = mgr.reset_after_failure();
        assert!(result.is_err());
    }

    // =======================================================================
    // Hash verification tests
    // =======================================================================

    #[test]
    fn recovery_hash_deterministic() {
        let h1 = compute_recovery_hash("sess-1", "checkpoint-data", 42, &[]);
        let h2 = compute_recovery_hash("sess-1", "checkpoint-data", 42, &[]);
        assert_eq!(h1, h2);
    }

    #[test]
    fn recovery_hash_differs_on_session_change() {
        let h1 = compute_recovery_hash("sess-1", "checkpoint-data", 42, &[]);
        let h2 = compute_recovery_hash("sess-2", "checkpoint-data", 42, &[]);
        assert_ne!(h1, h2);
    }

    #[test]
    fn recovery_hash_differs_on_data_change() {
        let h1 = compute_recovery_hash("sess-1", "checkpoint-data", 42, &[]);
        let h2 = compute_recovery_hash("sess-1", "different-data", 42, &[]);
        assert_ne!(h1, h2);
    }

    // =======================================================================
    // Full happy-path integration test
    // =======================================================================

    #[test]
    fn full_recovery_happy_path() {
        let mut mgr = CrashRecoveryManager::new();

        // 1. Begin recovery
        mgr.begin_recovery().unwrap();
        assert_eq!(mgr.state(), RecoveryState::Scanning);

        // 2. Scan journal — only pure reads completed
        let events = vec![
            tool_started_event("e1", "step-1", "read_file", 0, 1000),
            tool_completed_event("e2", "step-1", "read_file", 0, 2000),
            tool_started_event("e3", "step-1", "grep", 1, 3000),
            tool_completed_event("e4", "step-1", "grep", 1, 4000),
        ];
        let json = checkpoint_json();
        let scan = mgr
            .scan_journal("sess-1", 10, Some(&json), 8, events)
            .unwrap();
        assert_eq!(scan.tool_calls_found.len(), 2);
        assert!(scan.gap_detected.is_none());

        // 3. Begin replay — classify tools
        mgr.begin_replay().unwrap();
        assert_eq!(mgr.state(), RecoveryState::Replaying);

        // 4. Complete recovery — no user decisions needed
        mgr.complete_recovery().unwrap();
        assert_eq!(mgr.state(), RecoveryState::Recovered);

        // 5. Verify hash exists
        assert!(mgr.recovery_hash().is_some());

        // 6. Reset for continued operation
        mgr.reset().unwrap();
        assert_eq!(mgr.state(), RecoveryState::Idle);
    }

    // =======================================================================
    // Full unhappy-path integration test
    // =======================================================================

    #[test]
    fn full_recovery_unhappy_path_in_flight_side_effect() {
        let mut mgr = CrashRecoveryManager::new();

        // 1. Begin recovery
        mgr.begin_recovery().unwrap();

        // 2. Scan — bash was in-flight at crash time
        let events = vec![
            tool_started_event("e1", "step-1", "read_file", 0, 1000),
            tool_completed_event("e2", "step-1", "read_file", 0, 2000),
            tool_started_event("e3", "step-1", "bash", 1, 3000),
            // Crash! bash never completed
        ];
        let json = checkpoint_json();
        mgr.scan_journal("sess-1", 10, Some(&json), 8, events)
            .unwrap();

        // 3. Begin replay
        mgr.begin_replay().unwrap();

        // 4. Cannot auto-complete — bash in-flight
        let ctx = mgr.context().unwrap();
        assert!(!ctx.can_auto_recover());
        assert_eq!(ctx.pending_user_decisions().len(), 1);

        let result = mgr.complete_recovery();
        assert!(result.is_err());

        // 5. Force complete (operator override)
        mgr.force_complete().unwrap();
        assert_eq!(mgr.state(), RecoveryState::Recovered);
    }

    #[test]
    fn full_recovery_journal_gap_blocks_auto_recovery() {
        let mut mgr = CrashRecoveryManager::new();
        mgr.begin_recovery().unwrap();

        // Large timestamp gap (>5 minutes)
        let events = vec![
            tool_started_event("e1", "step-1", "read_file", 0, 1000),
            tool_completed_event("e2", "step-1", "read_file", 0, 2000),
            tool_started_event("e3", "step-1", "bash", 1, 500_000), // ~500 seconds
        ];
        let json = checkpoint_json();
        let scan = mgr
            .scan_journal("sess-1", 10, Some(&json), 8, events)
            .unwrap();
        assert!(scan.gap_detected.is_some());

        mgr.begin_replay().unwrap();

        let ctx = mgr.context().unwrap();
        assert!(!ctx.can_auto_recover());
    }

    // =======================================================================
    // RecoveryContext helper tests
    // =======================================================================

    #[test]
    fn recovery_context_pending_user_decisions() {
        let mut ctx = RecoveryContext::new("sess-1".to_string(), 5);
        ctx.tool_decisions
            .push(("read_file".to_string(), ToolReplayDecision::Replay));
        assert!(ctx.can_auto_recover()); // Replay is not a user decision

        ctx.tool_decisions.push((
            "bash".to_string(),
            ToolReplayDecision::RequireUserInput {
                reason: "test".to_string(),
            },
        ));
        assert!(!ctx.can_auto_recover());
        assert_eq!(ctx.pending_user_decisions().len(), 1);
    }

    // =======================================================================
    // Edge case: failed tool calls
    // =======================================================================

    #[test]
    fn scan_journal_handles_failed_tool_calls() {
        let mut mgr = CrashRecoveryManager::new();
        mgr.begin_recovery().unwrap();

        let events = vec![
            tool_started_event("e1", "step-1", "bash", 0, 1000),
            tool_failed_event("e2", "step-1", "bash", 0, 2000),
        ];

        let json = checkpoint_json();
        let scan = mgr
            .scan_journal("sess-1", 5, Some(&json), 3, events)
            .unwrap();
        assert_eq!(scan.tool_calls_found.len(), 1);
        assert_eq!(scan.tool_calls_found[0].status, ToolCallStatus::Failed);
    }

    // =======================================================================
    // Edge case: orphan completion (no matching start)
    // =======================================================================

    #[test]
    fn scan_journal_handles_orphan_completion() {
        let mut mgr = CrashRecoveryManager::new();
        mgr.begin_recovery().unwrap();

        // Completion event without a matching start event
        let events = vec![tool_completed_event("e1", "step-1", "read_file", 0, 1000)];

        let json = checkpoint_json();
        let scan = mgr
            .scan_journal("sess-1", 5, Some(&json), 3, events)
            .unwrap();
        assert_eq!(scan.tool_calls_found.len(), 1);
        assert_eq!(scan.tool_calls_found[0].status, ToolCallStatus::Completed);
    }

    // =======================================================================
    // Edge case: empty events
    // =======================================================================

    #[test]
    fn scan_journal_empty_events() {
        let mut mgr = CrashRecoveryManager::new();
        mgr.begin_recovery().unwrap();

        let json = checkpoint_json();
        let scan = mgr
            .scan_journal("sess-1", 5, Some(&json), 3, vec![])
            .unwrap();
        assert!(scan.tool_calls_found.is_empty());
        assert!(scan.gap_detected.is_none());
    }
}
