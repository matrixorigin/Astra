//! Unified Plan lifecycle — state machine, transitions, capabilities, and metrics.
//!
//! # Architecture
//!
//! The plan system has a single state machine (`PlanPhase`) that governs all
//! plan-related interactions: decomposition, refinement, execution, and completion.
//!
//! ```text
//! Idle → Planning → Refining ⇄ Refining
//!                     ↓
//!                  Executing ⇄ Paused
//!                     ↓
//!            Completed | Failed → Idle
//! ```

pub use super::decompose::*;

use serde::{Deserialize, Serialize};
use std::fmt;

// ─── PlanPhase: Unified State Machine ────────────────────────────────────────

/// Unified state machine for the plan lifecycle.
///
/// Replaces the former 6+ independent boolean/optional fields in `ReplState`
/// (`chat_plan_only`, `plan_mode`, `executing_plan`, `plan_execution_config`,
/// `executing_plan_goal`, `current_plan_subtask_id`) with a single enum that
/// makes invalid states unrepresentable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum PlanPhase {
    /// No active plan. The user is in normal chat mode.
    #[default]
    Idle,

    /// Plan-only chat mode: tools are disabled, model produces plans as text.
    /// This is the lightweight planning mode (`/plan on`).
    PlanOnlyChat,

    /// Goal submitted, waiting for LLM decomposition.
    Planning {
        goal: String,
        context: ProjectContext,
    },

    /// Plan generated, user is interactively refining it (`plan>` prompt).
    Refining { state: PlanModeState },

    /// Plan execution in progress (foreground or background).
    Executing { state: PlanExecutionState },

    /// Execution paused — waiting for user input, approval, or error recovery.
    Paused {
        state: PlanExecutionState,
        reason: PauseReason,
    },

    /// All subtasks completed successfully.
    Completed { summary: PlanExecutionSummary },

    /// Execution failed with unrecoverable error.
    Failed {
        error: PlanError,
        partial: Option<PlanExecutionSummary>,
    },
}

impl PlanPhase {
    /// Whether the phase is idle (no active plan work).
    pub fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }

    /// Whether we're in plan-only chat mode.
    pub fn is_plan_only_chat(&self) -> bool {
        matches!(self, Self::PlanOnlyChat)
    }

    /// Whether we're in the interactive plan editing mode.
    pub fn is_refining(&self) -> bool {
        matches!(self, Self::Refining { .. })
    }

    /// Whether a plan is currently being executed.
    pub fn is_executing(&self) -> bool {
        matches!(self, Self::Executing { .. })
    }

    /// Whether execution is paused.
    pub fn is_paused(&self) -> bool {
        matches!(self, Self::Paused { .. })
    }

    /// Whether the plan has reached a terminal state.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed { .. } | Self::Failed { .. })
    }

    /// Whether plan-only chat is active — tools should be omitted from payloads.
    /// True for `PlanOnlyChat` and `Refining` phases.
    pub fn should_omit_tools(&self) -> bool {
        matches!(self, Self::PlanOnlyChat | Self::Refining { .. })
    }

    /// Get the plan-only chat flag (backward compat with `chat_plan_only`).
    pub fn chat_plan_only(&self) -> bool {
        matches!(self, Self::PlanOnlyChat)
    }

    /// Get the plan mode state reference, if in refining phase.
    pub fn plan_mode_state(&self) -> Option<&PlanModeState> {
        match self {
            Self::Refining { state } => Some(state),
            _ => None,
        }
    }

    /// Get mutable plan mode state reference.
    pub fn plan_mode_state_mut(&mut self) -> Option<&mut PlanModeState> {
        match self {
            Self::Refining { state } => Some(state),
            _ => None,
        }
    }

    /// Get the executing plan (backward compat with `executing_plan`).
    pub fn executing_plan(&self) -> Option<&TaskPlan> {
        match self {
            Self::Executing { state } => Some(&state.plan),
            Self::Paused { state, .. } => Some(&state.plan),
            _ => None,
        }
    }

    /// Get mutable executing plan.
    pub fn executing_plan_mut(&mut self) -> Option<&mut TaskPlan> {
        match self {
            Self::Executing { state } => Some(&mut state.plan),
            Self::Paused { state, .. } => Some(&mut state.plan),
            _ => None,
        }
    }

    /// Get the execution state.
    pub fn execution_state(&self) -> Option<&PlanExecutionState> {
        match self {
            Self::Executing { state } | Self::Paused { state, .. } => Some(state),
            _ => None,
        }
    }

    /// Get mutable execution state.
    pub fn execution_state_mut(&mut self) -> Option<&mut PlanExecutionState> {
        match self {
            Self::Executing { state } | Self::Paused { state, .. } => Some(state),
            _ => None,
        }
    }

    /// Get the current plan subtask ID being executed.
    pub fn current_subtask_id(&self) -> Option<&str> {
        match self {
            Self::Executing { state } | Self::Paused { state, .. } => {
                state.current_subtask_id.as_deref()
            }
            _ => None,
        }
    }

    /// Get the executing plan goal.
    pub fn executing_goal(&self) -> Option<&str> {
        match self {
            Self::Executing { state } | Self::Paused { state, .. } => state.goal.as_deref(),
            _ => None,
        }
    }

    /// Validate and apply a transition.
    ///
    /// Returns `Err` if the transition is not valid from the current phase.
    pub fn transition(self, action: PlanAction) -> Result<PlanPhase, PlanTransitionError> {
        use PlanAction::*;
        use PlanPhase::*;

        match (self, action) {
            // Idle → PlanOnlyChat
            (Idle, EnablePlanOnlyChat) => Ok(PlanOnlyChat),

            // PlanOnlyChat → Idle
            (PlanOnlyChat, DisablePlanOnlyChat) => Ok(Idle),

            // Idle → Planning
            (Idle, SubmitGoal { goal, context }) => Ok(Planning { goal, context }),

            // PlanOnlyChat → Planning (auto-escalate from plan-only to structured)
            (PlanOnlyChat, SubmitGoal { goal, context }) => Ok(Planning { goal, context }),

            // Planning → Refining
            (Planning { .. }, PlanGenerated { state }) => Ok(Refining { state }),

            // Planning → Idle (cancel before decomposition completes)
            (Planning { .. }, Cancel) => Ok(Idle),

            // Planning → Failed (LLM decomposition failed)
            (Planning { goal, .. }, Fail { error }) => Ok(Failed {
                error,
                partial: Some(PlanExecutionSummary::from_plan(
                    &TaskPlan::default(),
                    &goal,
                    0,
                )),
            }),

            // Refining → Refining (edit loop)
            (Refining { state: _ }, PlanEdited { state: new_state }) => {
                Ok(Refining { state: new_state })
            }

            // Refining → Planning (regenerate)
            (Refining { state }, Regenerate) => Ok(Planning {
                goal: state.goal.clone(),
                context: state.context.clone(),
            }),

            // Refining → Executing
            (Refining { state }, Execute { config }) => {
                let exec_state = PlanExecutionState {
                    plan: state.plan.clone(),
                    goal: Some(state.goal.clone()),
                    config,
                    current_subtask_id: None,
                    execution_rounds: 0,
                    corrections: Vec::new(),
                    timeline: state.timeline.clone(),
                    last_turn_interrupted: false,
                    metrics: crate::metrics::PlanMetrics::default(),
                };
                Ok(Executing { state: exec_state })
            }

            // Refining → Idle (cancel)
            (Refining { .. }, Cancel) => Ok(Idle),

            // Executing → Executing (subtask done — stay in executing)
            (Executing { state }, SubtaskDone) => Ok(Executing { state }),

            // Executing → Paused
            (Executing { state }, Pause { reason }) => Ok(Paused { state, reason }),

            // Executing → Completed
            (Executing { state }, Complete) => {
                let summary = PlanExecutionSummary::from_plan(
                    &state.plan,
                    state.goal.as_deref().unwrap_or(""),
                    state.execution_rounds,
                );
                Ok(Completed { summary })
            }

            // Executing → Failed
            (Executing { state }, Fail { error }) => {
                let partial = Some(PlanExecutionSummary::from_plan(
                    &state.plan,
                    state.goal.as_deref().unwrap_or(""),
                    state.execution_rounds,
                ));
                Ok(Failed { error, partial })
            }

            // Paused → Executing (resume)
            (Paused { state, .. }, Resume) => Ok(Executing { state }),

            // Paused → Refining (replan — preserves existing plan and timeline)
            (Paused { state, .. }, Replan) => {
                let mut plan_state =
                    PlanModeState::new(state.goal.unwrap_or_default(), ProjectContext::default());
                plan_state.plan = state.plan;
                plan_state.timeline = state.timeline;
                Ok(Refining { state: plan_state })
            }

            // Paused → Idle (abandon)
            (Paused { .. }, Abandon) => Ok(Idle),

            // Completed / Failed → Idle (dismiss)
            (Completed { .. }, Dismiss) | (Failed { .. }, Dismiss) => Ok(Idle),

            // Failed → Refining (retry)
            (Failed { .. }, RetryPlan { state }) => Ok(Refining { state }),

            // Invalid transitions
            (from, action) => Err(PlanTransitionError::Invalid {
                from_phase: from.phase_name().to_string(),
                action: action.action_name().to_string(),
            }),
        }
    }

    /// Human-readable name for this phase.
    pub fn phase_name(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::PlanOnlyChat => "plan_only_chat",
            Self::Planning { .. } => "planning",
            Self::Refining { .. } => "refining",
            Self::Executing { .. } => "executing",
            Self::Paused { .. } => "paused",
            Self::Completed { .. } => "completed",
            Self::Failed { .. } => "failed",
        }
    }
}

impl fmt::Display for PlanPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.phase_name())
    }
}

// ─── PlanExecutionState ──────────────────────────────────────────────────────

/// State held during plan execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanExecutionState {
    /// The plan being executed.
    pub plan: TaskPlan,
    /// Goal text for summary generation.
    pub goal: Option<String>,
    /// Execution configuration (step-by-step, auto, etc.).
    pub config: PlanExecutionConfig,
    /// Currently executing subtask ID.
    pub current_subtask_id: Option<String>,
    /// Number of parallel execution rounds completed.
    pub execution_rounds: usize,
    /// Stacked operator corrections for upcoming subtasks.
    pub corrections: Vec<String>,
    /// Execution timeline tracking all events.
    pub timeline: ExecutionTimeline,
    /// Whether the last chat turn was interrupted by Ctrl+C.
    #[serde(default)]
    pub last_turn_interrupted: bool,
    /// Structured metrics for observability.
    #[serde(default)]
    pub metrics: crate::metrics::PlanMetrics,
}

// ─── PlanAction ──────────────────────────────────────────────────────────────

/// Actions that can trigger phase transitions.
#[derive(Debug, Clone)]
pub enum PlanAction {
    EnablePlanOnlyChat,
    DisablePlanOnlyChat,
    SubmitGoal {
        goal: String,
        context: ProjectContext,
    },
    PlanGenerated {
        state: PlanModeState,
    },
    PlanEdited {
        state: PlanModeState,
    },
    Regenerate,
    Execute {
        config: PlanExecutionConfig,
    },
    Cancel,
    SubtaskDone,
    Pause {
        reason: PauseReason,
    },
    Resume,
    Replan,
    Abandon,
    Complete,
    Fail {
        error: PlanError,
    },
    Dismiss,
    RetryPlan {
        state: PlanModeState,
    },
}

impl PlanAction {
    /// Human-readable name for this action.
    pub fn action_name(&self) -> &'static str {
        match self {
            Self::EnablePlanOnlyChat => "enable_plan_only_chat",
            Self::DisablePlanOnlyChat => "disable_plan_only_chat",
            Self::SubmitGoal { .. } => "submit_goal",
            Self::PlanGenerated { .. } => "plan_generated",
            Self::PlanEdited { .. } => "plan_edited",
            Self::Regenerate => "regenerate",
            Self::Execute { .. } => "execute",
            Self::Cancel => "cancel",
            Self::SubtaskDone => "subtask_done",
            Self::Pause { .. } => "pause",
            Self::Resume => "resume",
            Self::Replan => "replan",
            Self::Abandon => "abandon",
            Self::Complete => "complete",
            Self::Fail { .. } => "fail",
            Self::Dismiss => "dismiss",
            Self::RetryPlan { .. } => "retry_plan",
        }
    }
}

// ─── Transition Errors ───────────────────────────────────────────────────────

/// Error when a plan phase transition is invalid.
#[derive(Debug, Clone)]
pub enum PlanTransitionError {
    Invalid { from_phase: String, action: String },

    ValidationFailed { reason: String },
}

impl std::fmt::Display for PlanTransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid { from_phase, action } => {
                write!(
                    f,
                    "invalid plan transition: cannot {action} from {from_phase}"
                )
            }
            Self::ValidationFailed { reason } => {
                write!(f, "plan validation failed: {reason}")
            }
        }
    }
}

impl std::error::Error for PlanTransitionError {}

// ─── PauseReason ─────────────────────────────────────────────────────────────

/// Why execution was paused.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PauseReason {
    /// User pressed Ctrl+C or typed a pause command.
    UserRequest,
    /// Waiting for user approval of a tool call.
    ApprovalNeeded { tool: String, request_id: String },
    /// Subtask failed and requires manual intervention.
    SubtaskFailed { subtask_id: String, error: String },
    /// Step-by-step mode: waiting for user to confirm next subtask.
    StepByStep { next_subtask_id: String },
}

impl fmt::Display for PauseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UserRequest => write!(f, "user request"),
            Self::ApprovalNeeded { tool, .. } => write!(f, "approval needed for {tool}"),
            Self::SubtaskFailed { subtask_id, error } => {
                write!(f, "subtask {subtask_id} failed: {error}")
            }
            Self::StepByStep { next_subtask_id } => {
                write!(f, "step-by-step: awaiting {next_subtask_id}")
            }
        }
    }
}

// ─── PlanError ───────────────────────────────────────────────────────────────

/// Structured plan error.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlanError {
    LlmFailure { message: String },
    RetriesExhausted { subtask_id: String, attempts: u32 },
    DependencyDeadlock { blocked_ids: Vec<String> },
    ValidationFailed { reason: String },
    NetworkError { message: String },
    Other { message: String },
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LlmFailure { message } => write!(f, "LLM failure: {message}"),
            Self::RetriesExhausted {
                subtask_id,
                attempts,
            } => {
                write!(
                    f,
                    "all retries exhausted for subtask {subtask_id} ({attempts} attempts)"
                )
            }
            Self::DependencyDeadlock { blocked_ids } => {
                write!(f, "dependency deadlock: {blocked_ids:?}")
            }
            Self::ValidationFailed { reason } => write!(f, "plan validation failed: {reason}"),
            Self::NetworkError { message } => write!(f, "network error: {message}"),
            Self::Other { message } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for PlanError {}

// ─── PlanCommand ─────────────────────────────────────────────────────────────

/// Structured commands parsed from user input in plan mode.
///
/// Replaces ad-hoc string matching (`is_execute_command`, `is_resume_command`,
/// `starts_with("step")`, etc.) with a single typed parser.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanCommand {
    /// Create a new plan with the given goal.
    Create { goal: String },
    /// Edit the current plan with natural-language instruction.
    Edit { instruction: String },
    /// Start executing the plan.
    Execute { step_by_step: bool },
    /// Pause execution.
    Pause,
    /// Resume paused execution.
    Resume,
    /// Cancel the current plan.
    Cancel,
    /// Show current plan status.
    Status,
    /// Show plan diff between versions.
    Diff { from: Option<u32>, to: Option<u32> },
    /// Rollback to a specific plan version.
    Rollback { version: u32 },
    /// Show plan timeline.
    Timeline,
    /// Show plan metrics.
    Metrics,
    /// Add operator correction for upcoming subtasks.
    Correct { guidance: String },
    /// Clear stacked corrections.
    ClearCorrections,
    /// Rewind to a specific subtask.
    Rewind { anchor: String },
    /// Enable plan-only chat mode.
    EnablePlanOnly,
    /// Disable plan-only chat mode.
    DisablePlanOnly,
    /// List saved plans.
    List,
    /// Show version history.
    History,
    /// Show the current plan in detail.
    Show,
    /// Show available commands.
    Help,
}

impl PlanCommand {
    /// Parse user input into a plan command.
    ///
    /// Returns `None` if the input is not a recognized command (i.e., it's a
    /// natural-language plan edit or goal description).
    pub fn parse(input: &str) -> Option<PlanCommand> {
        let trimmed = input.trim();
        // Strip only emphatic/question punctuation used for command-like inputs.
        // Avoid ASCII '.' and ',' because paused-plan parsing runs on all input.
        let stripped = trimmed.trim_end_matches(['!', '！', '?', '？', '。']);
        let lower = stripped.to_lowercase();

        // Help
        if matches!(lower.as_str(), "help" | "?" | "帮助" | "commands") {
            return Some(PlanCommand::Help);
        }

        // Execute commands
        if matches!(
            lower.as_str(),
            "execute" | "go" | "start" | "done" | "run" | "开始" | "执行" | "运行"
        ) {
            return Some(PlanCommand::Execute {
                step_by_step: false,
            });
        }

        // Step-by-step execute
        if matches!(
            lower.as_str(),
            "step" | "step-by-step" | "stepbystep" | "逐步执行"
        ) {
            return Some(PlanCommand::Execute { step_by_step: true });
        }

        // Resume commands
        if matches!(lower.as_str(), "continue" | "resume" | "继续" | "next") {
            return Some(PlanCommand::Resume);
        }

        // Status
        if lower == "status" {
            return Some(PlanCommand::Status);
        }

        // Exit / cancel
        if matches!(lower.as_str(), "exit" | "quit" | "cancel" | "取消") {
            return Some(PlanCommand::Cancel);
        }

        // Pause
        if lower == "pause" || lower == "暂停" {
            return Some(PlanCommand::Pause);
        }

        // Timeline
        if lower == "timeline" || lower == "时间线" {
            return Some(PlanCommand::Timeline);
        }

        // Metrics
        if lower == "metrics" || lower == "指标" || lower == "cost" {
            return Some(PlanCommand::Metrics);
        }

        // History
        if lower == "history" || lower == "历史" {
            return Some(PlanCommand::History);
        }

        // Show
        if lower == "show" || lower == "detail" || lower == "详情" {
            return Some(PlanCommand::Show);
        }

        // List
        if lower == "list" || lower == "列表" {
            return Some(PlanCommand::List);
        }

        // Diff
        if lower.starts_with("diff") {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            let from = parts.get(1).and_then(|s| s.parse::<u32>().ok());
            let to = parts.get(2).and_then(|s| s.parse::<u32>().ok());
            return Some(PlanCommand::Diff { from, to });
        }

        // Rollback
        if lower.starts_with("rollback") || lower.starts_with("undo") {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if let Some(version) = parts.get(1).and_then(|s| s.parse::<u32>().ok()) {
                return Some(PlanCommand::Rollback { version });
            }
        }

        // Corrections (prefixed commands)
        if let Some(rest) = strip_prefix_ci_local(trimmed, "correct ") {
            if rest == "clear" {
                return Some(PlanCommand::ClearCorrections);
            }
            if !rest.is_empty() {
                return Some(PlanCommand::Correct {
                    guidance: rest.to_string(),
                });
            }
        }
        if let Some(rest) = strip_prefix_ci_local(trimmed, "note ") {
            if rest == "clear" {
                return Some(PlanCommand::ClearCorrections);
            }
            if !rest.is_empty() {
                return Some(PlanCommand::Correct {
                    guidance: rest.to_string(),
                });
            }
        }
        if let Some(rest) = strip_prefix_ci_local(trimmed, "adjust ") {
            if rest == "clear" {
                return Some(PlanCommand::ClearCorrections);
            }
            if !rest.is_empty() {
                return Some(PlanCommand::Correct {
                    guidance: rest.to_string(),
                });
            }
        }

        // Rewind
        for prefix in [
            "rewind ",
            "restart from ",
            "redo from ",
            "restart ",
            "redo ",
        ] {
            if let Some(rest) = strip_prefix_ci_local(trimmed, prefix).filter(|r| !r.is_empty()) {
                return Some(PlanCommand::Rewind {
                    anchor: rest.to_string(),
                });
            }
        }

        // Plan-only mode
        if lower == "on" {
            return Some(PlanCommand::EnablePlanOnly);
        }
        if lower == "off" {
            return Some(PlanCommand::DisablePlanOnly);
        }

        None
    }
}

/// Case-insensitive ASCII prefix strip.
fn strip_prefix_ci_local(s: &str, prefix: &str) -> Option<String> {
    let s = s.trim_start();
    if s.len() < prefix.len() {
        return None;
    }
    let head = s.get(..prefix.len())?;
    if !head.eq_ignore_ascii_case(prefix) {
        return None;
    }
    Some(s.get(prefix.len()..)?.trim_start().to_string())
}

// ─── PlanCapabilities ────────────────────────────────────────────────────────

/// Capability-based permission model for plan phases.
///
/// Each plan phase has different capabilities. During planning/refining, tools
/// are disabled. During execution, the full tool set is available but may
/// require approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanCapabilities {
    /// Whether the agent can read files (context gathering).
    pub can_read_files: bool,
    /// Whether tools can be executed (shell, write, etc.).
    pub can_execute_tools: bool,
    /// Whether files can be modified.
    pub can_modify_files: bool,
    /// Whether network access is allowed.
    pub can_access_network: bool,
    /// Maximum number of subtasks allowed.
    pub max_subtasks: usize,
    /// Maximum number of execution rounds.
    pub max_execution_rounds: usize,
    /// Approval policy for tool execution.
    pub requires_approval: ApprovalPolicy,
}

impl Default for PlanCapabilities {
    fn default() -> Self {
        Self {
            can_read_files: true,
            can_execute_tools: true,
            can_modify_files: true,
            can_access_network: true,
            max_subtasks: 20,
            max_execution_rounds: 50,
            requires_approval: ApprovalPolicy::Destructive,
        }
    }
}

impl PlanCapabilities {
    /// Capabilities for plan-only chat and refining phases (no tools).
    pub fn planning() -> Self {
        Self {
            can_read_files: true,
            can_execute_tools: false,
            can_modify_files: false,
            can_access_network: false,
            max_subtasks: 20,
            max_execution_rounds: 0,
            requires_approval: ApprovalPolicy::All,
        }
    }

    /// Capabilities for auto-execution (full tools, approve destructive only).
    pub fn auto_execute() -> Self {
        Self::default()
    }

    /// Capabilities for step-by-step execution (full tools, approve each subtask).
    pub fn step_by_step() -> Self {
        Self {
            requires_approval: ApprovalPolicy::PerSubtask,
            ..Self::default()
        }
    }

    /// Derive capabilities from the current plan phase.
    pub fn for_phase(phase: &PlanPhase) -> Self {
        match phase {
            PlanPhase::Idle => Self::default(),
            PlanPhase::PlanOnlyChat | PlanPhase::Planning { .. } | PlanPhase::Refining { .. } => {
                Self::planning()
            }
            PlanPhase::Executing { state } => {
                if state.config.step_by_step {
                    Self::step_by_step()
                } else {
                    Self::auto_execute()
                }
            }
            PlanPhase::Paused { .. } => Self::planning(),
            PlanPhase::Completed { .. } | PlanPhase::Failed { .. } => Self::default(),
        }
    }
}

// ─── Error Recovery ─────────────────────────────────────────────────────────

/// Circuit breaker state for plan execution.
///
/// Trips after `max_consecutive_failures` consecutive subtask failures,
/// pausing execution and requiring user intervention to resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreaker {
    /// Maximum consecutive failures before tripping.
    pub max_consecutive_failures: u32,
    /// Current consecutive failure count.
    #[serde(default)]
    pub consecutive_failures: u32,
    /// Whether the circuit breaker has tripped.
    #[serde(default)]
    pub tripped: bool,
    /// IDs of subtasks that caused the trip.
    #[serde(default)]
    pub failed_subtask_ids: Vec<String>,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self {
            max_consecutive_failures: 3,
            consecutive_failures: 0,
            tripped: false,
            failed_subtask_ids: Vec::new(),
        }
    }
}

impl CircuitBreaker {
    /// Record a subtask success — resets the failure counter.
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
    }

    /// Record a subtask failure — may trip the breaker.
    /// Returns `true` if the breaker just tripped.
    pub fn record_failure(&mut self, subtask_id: &str) -> bool {
        self.consecutive_failures += 1;
        self.failed_subtask_ids.push(subtask_id.to_string());
        if self.consecutive_failures >= self.max_consecutive_failures && !self.tripped {
            self.tripped = true;
            return true;
        }
        false
    }

    /// Reset the breaker (user chose to resume despite failures).
    pub fn reset(&mut self) {
        self.consecutive_failures = 0;
        self.tripped = false;
        self.failed_subtask_ids.clear();
    }

    /// Whether execution should be paused.
    pub fn should_pause(&self) -> bool {
        self.tripped
    }
}

/// Configurable retry policy for subtask execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Maximum retry attempts per subtask.
    pub max_retries: u32,
    /// Base delay between retries (ms). Doubles on each attempt (exponential backoff).
    pub base_delay_ms: u64,
    /// Maximum delay cap (ms).
    pub max_delay_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            base_delay_ms: 1000,
            max_delay_ms: 30_000,
        }
    }
}

impl RetryPolicy {
    /// Calculate the delay for a given retry attempt (0-indexed).
    pub fn delay_for_attempt(&self, attempt: u32) -> std::time::Duration {
        let delay = self.base_delay_ms.saturating_mul(1u64 << attempt.min(10));
        std::time::Duration::from_millis(delay.min(self.max_delay_ms))
    }

    /// Whether more retries are allowed.
    pub fn can_retry(&self, attempt: u32) -> bool {
        attempt < self.max_retries
    }
}

/// Approval policy for tool execution during plan execution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    /// No approval required — auto-execute everything.
    None,
    /// Approve each subtask before starting it.
    PerSubtask,
    /// Only approve destructive operations (file writes, shell commands).
    #[default]
    Destructive,
    /// Approve every single tool call.
    All,
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_to_plan_only_chat() {
        let phase = PlanPhase::Idle;
        let next = phase.transition(PlanAction::EnablePlanOnlyChat).unwrap();
        assert!(next.is_plan_only_chat());
    }

    #[test]
    fn plan_only_chat_to_idle() {
        let phase = PlanPhase::PlanOnlyChat;
        let next = phase.transition(PlanAction::DisablePlanOnlyChat).unwrap();
        assert!(next.is_idle());
    }

    #[test]
    fn idle_to_planning() {
        let phase = PlanPhase::Idle;
        let next = phase
            .transition(PlanAction::SubmitGoal {
                goal: "test".into(),
                context: ProjectContext::default(),
            })
            .unwrap();
        assert!(matches!(next, PlanPhase::Planning { .. }));
    }

    #[test]
    fn invalid_transition_returns_error() {
        let phase = PlanPhase::Idle;
        let err = phase.transition(PlanAction::Resume);
        assert!(err.is_err());
    }

    #[test]
    fn refining_to_executing() {
        let state = PlanModeState::new("test goal".into(), ProjectContext::default());
        let phase = PlanPhase::Refining { state };
        let next = phase
            .transition(PlanAction::Execute {
                config: PlanExecutionConfig {
                    step_by_step: false,
                    auto_execute: true,
                },
            })
            .unwrap();
        assert!(next.is_executing());
    }

    #[test]
    fn executing_to_paused_to_resumed() {
        let exec_state = PlanExecutionState {
            plan: TaskPlan::default(),
            goal: Some("test".into()),
            config: PlanExecutionConfig::default(),
            current_subtask_id: None,
            execution_rounds: 0,
            corrections: vec![],
            timeline: ExecutionTimeline::default(),
            last_turn_interrupted: false,
            metrics: crate::metrics::PlanMetrics::default(),
        };
        let phase = PlanPhase::Executing { state: exec_state };

        let paused = phase
            .transition(PlanAction::Pause {
                reason: PauseReason::UserRequest,
            })
            .unwrap();
        assert!(paused.is_paused());

        let resumed = paused.transition(PlanAction::Resume).unwrap();
        assert!(resumed.is_executing());
    }

    #[test]
    fn plan_command_parse_execute() {
        assert_eq!(
            PlanCommand::parse("go"),
            Some(PlanCommand::Execute {
                step_by_step: false
            })
        );
        assert_eq!(
            PlanCommand::parse("执行"),
            Some(PlanCommand::Execute {
                step_by_step: false
            })
        );
        assert_eq!(
            PlanCommand::parse("step"),
            Some(PlanCommand::Execute { step_by_step: true })
        );
    }

    #[test]
    fn plan_command_parse_status() {
        assert_eq!(PlanCommand::parse("status"), Some(PlanCommand::Status));
    }

    #[test]
    fn plan_command_parse_returns_none_for_freeform() {
        assert_eq!(PlanCommand::parse("add a new subtask for testing"), None);
        assert_eq!(PlanCommand::parse("simplify the plan"), None);
    }

    #[test]
    fn plan_command_parse_corrections() {
        assert_eq!(
            PlanCommand::parse("correct use async instead"),
            Some(PlanCommand::Correct {
                guidance: "use async instead".to_string()
            })
        );
        assert_eq!(
            PlanCommand::parse("correct clear"),
            Some(PlanCommand::ClearCorrections)
        );
    }

    #[test]
    fn plan_command_parse_rewind() {
        assert_eq!(
            PlanCommand::parse("rewind 3"),
            Some(PlanCommand::Rewind {
                anchor: "3".to_string()
            })
        );
        assert_eq!(
            PlanCommand::parse("restart from setup"),
            Some(PlanCommand::Rewind {
                anchor: "setup".to_string()
            })
        );
    }

    #[test]
    fn plan_command_parse_strips_trailing_punctuation() {
        // Chinese punctuation
        assert_eq!(PlanCommand::parse("继续!"), Some(PlanCommand::Resume));
        assert_eq!(PlanCommand::parse("继续！"), Some(PlanCommand::Resume));
        assert_eq!(
            PlanCommand::parse("执行。"),
            Some(PlanCommand::Execute {
                step_by_step: false
            })
        );
        // English punctuation
        assert_eq!(PlanCommand::parse("continue!"), Some(PlanCommand::Resume));
        assert_eq!(
            PlanCommand::parse("go!"),
            Some(PlanCommand::Execute {
                step_by_step: false
            })
        );
        assert_eq!(PlanCommand::parse("status?"), Some(PlanCommand::Status));
        // Multiple punctuation
        assert_eq!(PlanCommand::parse("继续!!"), Some(PlanCommand::Resume));
        // No punctuation still works
        assert_eq!(PlanCommand::parse("继续"), Some(PlanCommand::Resume));
        // ASCII sentence punctuation stays literal to avoid accidental matches.
        assert_eq!(PlanCommand::parse("done."), None);
        assert_eq!(PlanCommand::parse("continue,"), None);
    }

    #[test]
    fn capabilities_for_phases() {
        let idle_caps = PlanCapabilities::for_phase(&PlanPhase::Idle);
        assert!(idle_caps.can_execute_tools);

        let plan_caps = PlanCapabilities::for_phase(&PlanPhase::PlanOnlyChat);
        assert!(!plan_caps.can_execute_tools);
        assert!(!plan_caps.can_modify_files);

        let refine_caps = PlanCapabilities::for_phase(&PlanPhase::Refining {
            state: PlanModeState::new("test".into(), ProjectContext::default()),
        });
        assert!(!refine_caps.can_execute_tools);
    }

    #[test]
    fn phase_display() {
        assert_eq!(format!("{}", PlanPhase::Idle), "idle");
        assert_eq!(format!("{}", PlanPhase::PlanOnlyChat), "plan_only_chat");
    }

    // ── Circuit breaker tests ───────────────────────────────────────────

    #[test]
    fn circuit_breaker_trips_after_max_failures() {
        let mut cb = CircuitBreaker {
            max_consecutive_failures: 3,
            ..Default::default()
        };
        assert!(!cb.record_failure("s1"));
        assert!(!cb.record_failure("s2"));
        assert!(cb.record_failure("s3")); // trips
        assert!(cb.should_pause());
    }

    #[test]
    fn circuit_breaker_resets_on_success() {
        let mut cb = CircuitBreaker {
            max_consecutive_failures: 3,
            ..Default::default()
        };
        cb.record_failure("s1");
        cb.record_failure("s2");
        cb.record_success();
        assert_eq!(cb.consecutive_failures, 0);
        assert!(!cb.should_pause());
    }

    #[test]
    fn circuit_breaker_reset_clears_state() {
        let mut cb = CircuitBreaker::default();
        cb.record_failure("s1");
        cb.record_failure("s2");
        cb.record_failure("s3");
        assert!(cb.tripped);
        cb.reset();
        assert!(!cb.tripped);
        assert_eq!(cb.consecutive_failures, 0);
        assert!(cb.failed_subtask_ids.is_empty());
    }

    // ── Retry policy tests ──────────────────────────────────────────────

    #[test]
    fn retry_policy_exponential_backoff() {
        let policy = RetryPolicy::default();
        let d0 = policy.delay_for_attempt(0);
        let d1 = policy.delay_for_attempt(1);
        let d2 = policy.delay_for_attempt(2);
        assert!(d1 > d0);
        assert!(d2 > d1);
    }

    #[test]
    fn retry_policy_caps_at_max() {
        let policy = RetryPolicy {
            base_delay_ms: 1000,
            max_delay_ms: 5000,
            ..Default::default()
        };
        let d10 = policy.delay_for_attempt(10);
        assert!(d10.as_millis() <= 5000);
    }

    #[test]
    fn retry_policy_can_retry_within_limit() {
        let policy = RetryPolicy {
            max_retries: 3,
            ..Default::default()
        };
        assert!(policy.can_retry(0));
        assert!(policy.can_retry(2));
        assert!(!policy.can_retry(3));
    }

    // ── Additional state transition tests ────────────────────────────────

    fn make_exec_state() -> PlanExecutionState {
        PlanExecutionState {
            plan: TaskPlan::default(),
            goal: Some("test".into()),
            config: PlanExecutionConfig::default(),
            current_subtask_id: None,
            execution_rounds: 0,
            corrections: vec![],
            timeline: ExecutionTimeline::default(),
            last_turn_interrupted: false,
            metrics: crate::metrics::PlanMetrics::default(),
        }
    }

    #[test]
    fn planning_to_idle_on_cancel() {
        let phase = PlanPhase::Planning {
            goal: "test".into(),
            context: ProjectContext::default(),
        };
        let next = phase.transition(PlanAction::Cancel).unwrap();
        assert!(next.is_idle());
    }

    #[test]
    fn planning_to_failed_preserves_error_and_partial_summary() {
        let phase = PlanPhase::Planning {
            goal: "build auth system".into(),
            context: ProjectContext::default(),
        };
        let next = phase
            .transition(PlanAction::Fail {
                error: PlanError::LlmFailure {
                    message: "timeout after 30s".into(),
                },
            })
            .unwrap();
        match next {
            PlanPhase::Failed {
                ref error,
                ref partial,
            } => {
                assert!(
                    matches!(error, PlanError::LlmFailure { message } if message.contains("timeout"))
                );
                assert!(
                    partial.is_some(),
                    "Planning->Failed should produce a partial summary"
                );
                let summary = partial.as_ref().unwrap();
                assert_eq!(summary.goal, "build auth system");
            }
            other => panic!("expected Failed, got {:?}", other.phase_name()),
        }
    }

    #[test]
    fn executing_to_completed() {
        let phase = PlanPhase::Executing {
            state: make_exec_state(),
        };
        let next = phase.transition(PlanAction::Complete).unwrap();
        assert!(matches!(next, PlanPhase::Completed { .. }));
    }

    #[test]
    fn executing_to_failed() {
        let phase = PlanPhase::Executing {
            state: make_exec_state(),
        };
        let next = phase
            .transition(PlanAction::Fail {
                error: PlanError::Other {
                    message: "boom".into(),
                },
            })
            .unwrap();
        assert!(matches!(next, PlanPhase::Failed { .. }));
    }

    #[test]
    fn paused_to_refining_on_replan_preserves_plan_and_timeline() {
        let mut exec = make_exec_state();
        exec.plan
            .subtasks
            .push(astra_services::task_orchestrator::SubtaskPlan {
                id: "sub-1".into(),
                title: "Existing subtask".into(),
                description: Some("Already in progress".into()),
                status: TaskStatus::InProgress,
                ..Default::default()
            });
        exec.timeline
            .record(crate::plan::TimelineEventKind::PlanCreated { subtask_count: 1 });

        let original_subtask_count = exec.plan.subtasks.len();
        let original_timeline_len = exec.timeline.events.len();

        let phase = PlanPhase::Paused {
            state: exec,
            reason: PauseReason::UserRequest,
        };
        let next = phase.transition(PlanAction::Replan).unwrap();

        match next {
            PlanPhase::Refining { state } => {
                assert_eq!(
                    state.plan.subtasks.len(),
                    original_subtask_count,
                    "Replan must preserve existing subtasks"
                );
                assert_eq!(
                    state.timeline.events.len(),
                    original_timeline_len,
                    "Replan must preserve timeline events"
                );
                assert_eq!(state.plan.subtasks[0].title, "Existing subtask");
            }
            other => panic!("expected Refining, got {:?}", other.phase_name()),
        }
    }

    #[test]
    fn paused_to_idle_on_abandon() {
        let phase = PlanPhase::Paused {
            state: make_exec_state(),
            reason: PauseReason::UserRequest,
        };
        let next = phase.transition(PlanAction::Abandon).unwrap();
        assert!(next.is_idle());
    }

    #[test]
    fn completed_to_idle_on_dismiss() {
        let phase = PlanPhase::Completed {
            summary: PlanExecutionSummary::from_plan(&TaskPlan::default(), "test", 1),
        };
        let next = phase.transition(PlanAction::Dismiss).unwrap();
        assert!(next.is_idle());
    }

    #[test]
    fn failed_to_idle_on_dismiss() {
        let phase = PlanPhase::Failed {
            error: PlanError::Other {
                message: "err".into(),
            },
            partial: None,
        };
        let next = phase.transition(PlanAction::Dismiss).unwrap();
        assert!(next.is_idle());
    }

    #[test]
    fn failed_to_refining_on_retry() {
        let phase = PlanPhase::Failed {
            error: PlanError::Other {
                message: "err".into(),
            },
            partial: None,
        };
        let state = PlanModeState::new("retry".into(), ProjectContext::default());
        let next = phase.transition(PlanAction::RetryPlan { state }).unwrap();
        assert!(next.is_refining());
    }

    #[test]
    fn step_command_does_not_match_freeform() {
        assert_eq!(PlanCommand::parse("step back"), None);
        assert_eq!(PlanCommand::parse("steps needed"), None);
        assert_eq!(
            PlanCommand::parse("step"),
            Some(PlanCommand::Execute { step_by_step: true })
        );
        assert_eq!(
            PlanCommand::parse("step-by-step"),
            Some(PlanCommand::Execute { step_by_step: true })
        );
    }
}
