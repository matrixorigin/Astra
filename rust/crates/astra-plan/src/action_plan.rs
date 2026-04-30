//! Typed ActionPlan — the executable face of a plan step.
//!
//! Where `TaskPlan` (see `decompose.rs`) is a free-text narrative produced by
//! LLM decomposition, `ActionPlan` is the *typed*, executor-consumable intent:
//! a dense list of `Action`s (tool + args) paired with
//! `expected_postconditions` that the executor uses to diff observed outcomes
//! against intent — without asking the LLM to re-read the plan.
//!
//! The construction-time invariants (enforced by `ActionPlan::new`) are what
//! makes the pipeline verifiable end-to-end:
//!
//! - every action is covered by at least one postcondition,
//! - no postcondition dangles on a non-existent action,
//! - action indices are dense (`0..N`) and unique,
//! - every action names a non-empty tool.
//!
//! Drift any of these and the observation diff silently misses cases.

use astra_turn_types::{ToolIdempotency, classify_tool_idempotency};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

// ─── Action ──────────────────────────────────────────────────────────────────

/// A single executable step: a tool name plus its JSON args.
///
/// Intentionally minimal. No `description`, no `narrative`, no `reasoning`.
/// Free text belongs in the decomposition phase (`TaskPlan`), not here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    index: u32,
    tool: String,
    args: serde_json::Value,
}

impl Action {
    /// Build an action. `index` must be its 0-based position in the
    /// containing `ActionPlan`; uniqueness and density are enforced by
    /// `ActionPlan::new`.
    pub fn new(index: u32, tool: impl Into<String>, args: serde_json::Value) -> Self {
        Self {
            index,
            tool: tool.into(),
            args,
        }
    }

    pub fn index(&self) -> u32 {
        self.index
    }

    pub fn tool(&self) -> &str {
        &self.tool
    }

    pub fn args(&self) -> &serde_json::Value {
        &self.args
    }
}

// ─── PostCondition ───────────────────────────────────────────────────────────

/// What the executor must observe for the plan to be considered satisfied.
///
/// Starts with a single variant on purpose — we add variants only when a real
/// consumer needs them. Adding more shapes without a consumer re-introduces
/// free-form intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PostCondition {
    /// The tool call at `action_index` finished with success semantics.
    /// (What "success" means for a given tool is the executor's contract.)
    ToolCallSucceeded { action_index: u32 },
}

impl PostCondition {
    /// The action index this postcondition covers, if any.
    pub fn action_index(&self) -> Option<u32> {
        match self {
            Self::ToolCallSucceeded { action_index } => Some(*action_index),
        }
    }

    /// Does `observed` satisfy this postcondition?
    ///
    /// Correlation is **index-strict** (same `action_index`) and
    /// **outcome-strict** (same variant with the right success flag). The
    /// `result` payload is audit data only — it does not influence the match
    /// decision. This is deliberate: we never want retrieval heuristics or
    /// string scans on tool output to leak into verification.
    pub fn matches(&self, observed: &ObservedOutcome) -> bool {
        match (self, observed) {
            (
                Self::ToolCallSucceeded {
                    action_index: expected,
                },
                ObservedOutcome::ToolCall {
                    action_index: got,
                    success,
                    ..
                },
            ) => expected == got && *success,
        }
    }
}

// ─── ObservedOutcome ─────────────────────────────────────────────────────────

/// The executor's report of what actually happened for a single `Action`.
///
/// One variant per `PostCondition` variant it needs to answer. Extend only
/// when the matcher has a real new case — not speculatively.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObservedOutcome {
    /// A tool invocation completed. `success` encodes the tool's own
    /// success-or-failure signal; `result` is opaque audit payload (must not
    /// influence matcher decisions — see `PostCondition::matches`).
    ToolCall {
        action_index: u32,
        tool: String,
        success: bool,
        result: serde_json::Value,
    },
}

impl ObservedOutcome {
    pub fn action_index(&self) -> u32 {
        match self {
            Self::ToolCall { action_index, .. } => *action_index,
        }
    }
}

// ─── ActionPlanError ─────────────────────────────────────────────────────────

/// Construction errors — each one names a specific invariant violation so the
/// caller can repair the plan, not just "it failed".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionPlanError {
    /// An empty action list has no executable intent.
    EmptyActions,
    /// An action carries an empty tool name — no way to classify or audit.
    EmptyToolName { action_index: u32 },
    /// Two actions share the same index — observations can't correlate.
    DuplicateActionIndex { index: u32 },
    /// Action indices aren't `0..N`; observation diff requires dense keys.
    NonDenseIndices { observed: Vec<u32> },
    /// A postcondition references an action that doesn't exist.
    DanglingPostCondition { action_index: u32 },
    /// An action has no postcondition covering it — it would execute without
    /// any verifiable outcome, which is the exact drift this type prevents.
    UncoveredAction { action_index: u32 },
}

impl std::fmt::Display for ActionPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyActions => write!(f, "action plan has no actions"),
            Self::EmptyToolName { action_index } => {
                write!(f, "action {action_index} has an empty tool name")
            }
            Self::DuplicateActionIndex { index } => {
                write!(f, "duplicate action index {index}")
            }
            Self::NonDenseIndices { observed } => {
                write!(f, "action indices must be 0..N, got {observed:?}")
            }
            Self::DanglingPostCondition { action_index } => {
                write!(
                    f,
                    "postcondition references unknown action {action_index}"
                )
            }
            Self::UncoveredAction { action_index } => {
                write!(
                    f,
                    "action {action_index} has no postcondition covering it"
                )
            }
        }
    }
}

impl std::error::Error for ActionPlanError {}

// ─── ActionPlan ──────────────────────────────────────────────────────────────

/// A dense, typed, verifier-ready plan step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionPlan {
    actions: Vec<Action>,
    expected_postconditions: Vec<PostCondition>,
}

// ─── Executor ────────────────────────────────────────────────────────────────

/// Strategy for driving an `ActionPlan`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionPolicy {
    /// Drive every action regardless of failures; full diff at the end.
    RunAll,
    /// Stop after the first action whose outcome doesn't satisfy the matching
    /// postcondition. Remaining postconditions are reported as unmet.
    StopOnFailure,
}

/// Produces an `ObservedOutcome` for a given `Action`. Implementors are the
/// bridge between the typed plan layer and the real tool-call machinery.
///
/// The trait is synchronous on purpose: the plan layer stays pure; async
/// tool invocation lives in the caller and is awaited before `handle`. This
/// keeps the unit tests free of executor plumbing.
pub trait ActionHandler {
    fn handle(&self, action: &Action) -> ObservedOutcome;
}

/// Post-run report. `observations` is ordered by action dispatch;
/// `met`/`unmet` partition the plan's `expected_postconditions`;
/// `audit` records what actually ran in the exact order it ran.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionResult {
    pub observations: Vec<ObservedOutcome>,
    pub met: Vec<PostCondition>,
    pub unmet: Vec<PostCondition>,
    #[serde(default)]
    pub audit: Vec<ActionAuditEntry>,
}

/// One audit entry per executed action. `audit[i]` corresponds to
/// `observations[i]` one-to-one; both end at the same index when
/// `ExecutionPolicy::StopOnFailure` halts execution early.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionAuditEntry {
    pub action_index: u32,
    pub tool: String,
    pub idempotency: ToolIdempotency,
    /// Hex-encoded SHA-256 over the action's args, serialized with
    /// stable JSON object key order. Stable hashes are what make
    /// dedup, replay, and rollback correlation possible.
    pub args_hash: String,
    /// Hex-encoded SHA-256 over the observed tool result payload
    /// (success and failure payloads alike). Present for every
    /// executed action, success or not.
    pub result_hash: String,
    pub success: bool,
    pub recorded_at_unix_ms: u128,
}

impl ActionAuditEntry {
    fn from_run(action: &Action, outcome: &ObservedOutcome) -> Self {
        let (success, result_value) = match outcome {
            ObservedOutcome::ToolCall {
                success, result, ..
            } => (*success, result),
        };
        Self {
            action_index: action.index(),
            tool: action.tool().to_string(),
            idempotency: classify_tool_idempotency(action.tool()),
            args_hash: canonical_sha256_hex(action.args()),
            result_hash: canonical_sha256_hex(result_value),
            success,
            recorded_at_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
        }
    }
}

/// Canonical SHA-256 of a JSON value: serialized with object keys sorted
/// recursively so semantically-equal payloads hash equal regardless of
/// wire-level field order.
fn canonical_sha256_hex(value: &serde_json::Value) -> String {
    let canonical = canonicalize(value);
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn canonicalize(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::{Map, Value};
    match value {
        Value::Object(m) => {
            let mut sorted: Vec<(&String, &Value)> = m.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            let mut out = Map::with_capacity(sorted.len());
            for (k, v) in sorted {
                out.insert(k.clone(), canonicalize(v));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(canonicalize).collect()),
        _ => value.clone(),
    }
}

impl ExecutionResult {
    pub fn is_fully_satisfied(&self) -> bool {
        self.unmet.is_empty()
    }
}

/// Drives an `ActionPlan` against an `ActionHandler` and classifies the
/// postcondition diff. Holds no mutable state between runs.
pub struct Executor {
    policy: ExecutionPolicy,
}

impl Executor {
    pub fn new(policy: ExecutionPolicy) -> Self {
        Self { policy }
    }

    pub fn run<H: ActionHandler>(&self, plan: &ActionPlan, handler: &H) -> ExecutionResult {
        let mut observations: Vec<ObservedOutcome> = Vec::with_capacity(plan.actions.len());
        let mut audit: Vec<ActionAuditEntry> = Vec::with_capacity(plan.actions.len());

        for action in &plan.actions {
            let outcome = handler.handle(action);
            audit.push(ActionAuditEntry::from_run(action, &outcome));
            let outcome_satisfies_this_action = plan
                .expected_postconditions
                .iter()
                .filter(|pc| pc.action_index() == Some(action.index()))
                .all(|pc| pc.matches(&outcome));
            observations.push(outcome);
            if self.policy == ExecutionPolicy::StopOnFailure && !outcome_satisfies_this_action {
                break;
            }
        }

        // Classify every declared postcondition.
        let mut met = Vec::new();
        let mut unmet = Vec::new();
        for pc in &plan.expected_postconditions {
            let is_met = match pc.action_index() {
                Some(idx) => observations
                    .iter()
                    .find(|o| o.action_index() == idx)
                    .is_some_and(|obs| pc.matches(obs)),
                None => false,
            };
            if is_met {
                met.push(pc.clone());
            } else {
                unmet.push(pc.clone());
            }
        }

        ExecutionResult {
            observations,
            met,
            unmet,
            audit,
        }
    }
}

// ─── ExecutionDriver ─────────────────────────────────────────────────────────

/// Protocol error returned by `ExecutionDriver`. Every violation of the
/// `next_action → record` contract is a typed error, never a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverError {
    /// `record` was called without a matching `next_action`.
    NoPendingAction,
    /// The outcome's `action_index` does not match the pending action.
    OutcomeIndexMismatch { expected: u32, got: u32 },
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPendingAction => {
                write!(f, "record() called with no pending action; call next_action() first")
            }
            Self::OutcomeIndexMismatch { expected, got } => write!(
                f,
                "outcome index mismatch: pending action was {expected}, got outcome for {got}"
            ),
        }
    }
}

impl std::error::Error for DriverError {}

/// Step-wise, synchronous driver for an `ActionPlan`.
///
/// The async boundary lives in the caller: they `await` their tool pipeline
/// between `next_action()` and `record()`. This keeps the plan crate free of
/// async traits while letting production code drive execution against a
/// real tool-call machinery.
///
/// Protocol:
/// 1. `next_action()` → returns the next `Action` (or `None` when the plan
///    is exhausted or `StopOnFailure` has halted the run);
/// 2. caller executes the tool;
/// 3. `record(outcome)` → stores the observation + audit entry, advances;
/// 4. repeat until `next_action()` returns `None`;
/// 5. `finish()` → consumes the driver and returns the full `ExecutionResult`.
pub struct ExecutionDriver<'a> {
    plan: &'a ActionPlan,
    policy: ExecutionPolicy,
    cursor: usize,
    pending: Option<u32>,
    halted: bool,
    observations: Vec<ObservedOutcome>,
    audit: Vec<ActionAuditEntry>,
}

impl<'a> ExecutionDriver<'a> {
    pub fn new(plan: &'a ActionPlan, policy: ExecutionPolicy) -> Self {
        Self {
            plan,
            policy,
            cursor: 0,
            pending: None,
            halted: false,
            observations: Vec::with_capacity(plan.actions.len()),
            audit: Vec::with_capacity(plan.actions.len()),
        }
    }

    /// Yield the next action to execute. Returns the same pending action on
    /// repeated calls (idempotent read) until `record` advances the cursor.
    /// Returns `None` once the plan is exhausted or execution has halted.
    pub fn next_action(&mut self) -> Option<&'a Action> {
        if self.halted {
            return None;
        }
        if let Some(idx) = self.pending {
            return self.plan.actions.get(idx as usize);
        }
        let action = self.plan.actions.get(self.cursor)?;
        self.pending = Some(action.index());
        Some(action)
    }

    /// Record the outcome for the current pending action. Errors if the
    /// protocol is violated.
    pub fn record(&mut self, outcome: ObservedOutcome) -> Result<(), DriverError> {
        let pending_idx = self.pending.ok_or(DriverError::NoPendingAction)?;
        if outcome.action_index() != pending_idx {
            return Err(DriverError::OutcomeIndexMismatch {
                expected: pending_idx,
                got: outcome.action_index(),
            });
        }

        let action = &self.plan.actions[self.cursor];
        self.audit.push(ActionAuditEntry::from_run(action, &outcome));

        // Halt decision uses the same rule as Executor::run.
        let satisfies_this_action = self
            .plan
            .expected_postconditions
            .iter()
            .filter(|pc| pc.action_index() == Some(pending_idx))
            .all(|pc| pc.matches(&outcome));
        self.observations.push(outcome);

        if self.policy == ExecutionPolicy::StopOnFailure && !satisfies_this_action {
            self.halted = true;
        }
        self.cursor += 1;
        self.pending = None;
        Ok(())
    }

    /// Consume the driver, classifying every postcondition as met/unmet
    /// based on the observations recorded so far. Identical semantics to
    /// `Executor::run` — calling `finish` before any step yields a result
    /// where every postcondition is unmet.
    pub fn finish(self) -> ExecutionResult {
        let mut met = Vec::new();
        let mut unmet = Vec::new();
        for pc in &self.plan.expected_postconditions {
            let is_met = match pc.action_index() {
                Some(idx) => self
                    .observations
                    .iter()
                    .find(|o| o.action_index() == idx)
                    .is_some_and(|obs| pc.matches(obs)),
                None => false,
            };
            if is_met {
                met.push(pc.clone());
            } else {
                unmet.push(pc.clone());
            }
        }
        ExecutionResult {
            observations: self.observations,
            met,
            unmet,
            audit: self.audit,
        }
    }
}

// ─── ExecutionLedger ─────────────────────────────────────────────────────────

/// Construction errors for `ExecutionLedger`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionLedgerError {
    /// A ledger with capacity 0 is a silent memory hole — every `record`
    /// would be dropped, leaving `latest()` forever `None`. Reject loudly.
    ZeroCapacity,
}

impl std::fmt::Display for ExecutionLedgerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroCapacity => write!(f, "ExecutionLedger capacity must be > 0"),
        }
    }
}

impl std::error::Error for ExecutionLedgerError {}

/// Bounded, append-only history of `ExecutionResult`s. Oldest entries are
/// evicted when capacity is reached — new runs are never dropped.
///
/// Iteration is oldest → newest so renderers and replay both see a stable
/// direction. `latest()` is the load-bearing accessor: the self-model reads
/// it (and only it) to surface unmet postconditions to the next turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionLedger {
    capacity: usize,
    entries: std::collections::VecDeque<ExecutionResult>,
}

impl ExecutionLedger {
    pub fn new(capacity: usize) -> Result<Self, ExecutionLedgerError> {
        if capacity == 0 {
            return Err(ExecutionLedgerError::ZeroCapacity);
        }
        Ok(Self {
            capacity,
            entries: std::collections::VecDeque::with_capacity(capacity),
        })
    }

    pub fn record(&mut self, result: ExecutionResult) {
        // Enforce the bound *before* push so we never momentarily exceed it.
        while self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(result);
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn latest(&self) -> Option<&ExecutionResult> {
        self.entries.back()
    }

    /// Oldest → newest iteration.
    pub fn iter(&self) -> impl Iterator<Item = &ExecutionResult> {
        self.entries.iter()
    }

    /// `Some(list)` when at least one run has been recorded, even if the list
    /// is empty (everything was met). `None` only when the ledger is empty,
    /// so the caller can distinguish "never ran" from "ran and passed".
    pub fn latest_unmet(&self) -> Option<Vec<PostCondition>> {
        self.entries.back().map(|r| r.unmet.clone())
    }
}

impl ActionPlan {
    /// Construct an `ActionPlan`, enforcing every schema invariant up front.
    pub fn new(
        actions: Vec<Action>,
        expected_postconditions: Vec<PostCondition>,
    ) -> Result<Self, ActionPlanError> {
        if actions.is_empty() {
            return Err(ActionPlanError::EmptyActions);
        }

        // Empty tool names.
        for a in &actions {
            if a.tool.is_empty() {
                return Err(ActionPlanError::EmptyToolName {
                    action_index: a.index,
                });
            }
        }

        // Duplicate indices.
        let mut seen: BTreeSet<u32> = BTreeSet::new();
        for a in &actions {
            if !seen.insert(a.index) {
                return Err(ActionPlanError::DuplicateActionIndex { index: a.index });
            }
        }

        // Dense 0..N.
        let n = actions.len() as u32;
        let expected: BTreeSet<u32> = (0..n).collect();
        if seen != expected {
            let observed: Vec<u32> = seen.into_iter().collect();
            return Err(ActionPlanError::NonDenseIndices { observed });
        }

        // Dangling postconditions.
        for pc in &expected_postconditions {
            if let Some(idx) = pc.action_index()
                && idx >= n
            {
                return Err(ActionPlanError::DanglingPostCondition { action_index: idx });
            }
        }

        // Full coverage: every action index must appear in at least one postcondition.
        let covered: BTreeSet<u32> = expected_postconditions
            .iter()
            .filter_map(|pc| pc.action_index())
            .collect();
        for a in &actions {
            if !covered.contains(&a.index) {
                return Err(ActionPlanError::UncoveredAction {
                    action_index: a.index,
                });
            }
        }

        Ok(Self {
            actions,
            expected_postconditions,
        })
    }

    pub fn actions(&self) -> &[Action] {
        &self.actions
    }

    pub fn expected_postconditions(&self) -> &[PostCondition] {
        &self.expected_postconditions
    }
}
