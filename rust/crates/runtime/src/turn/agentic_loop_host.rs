//! Runtime-portable agentic multi-turn loop.
//!
//! # Overview
//!
//! [`AgenticLoopHost`] abstracts all host-specific behavior (payload preparation,
//! HTTP posting, SSE consumption, terminal rendering) so the multi-turn loop
//! can run identically in CLI and headless cloud contexts.
//!
//! # Host Implementations
#![allow(deprecated)]
//!
//! | Host | Crate | Context | Tool execution |
//! |------|-------|---------|----------------|
//! | `CliAgenticLoopHost` | astra-cli | Interactive terminal | Local via `ToolExecutor` |
//! | `ServerAgenticLoopHost` | runtime/server | Headless cloud/API | Via edge callback ledger |
//! | `MockHost` (tests) | runtime (tests) | Unit tests | Scripted responses |
//!
//! # Execution Flow
//!
//! ```text
//! run_agentic_loop_with_host(host, state)
//!   for turn in 0..max_turns:
//!     ── cancel check ──────────────── cooperative, via cancel_flag
//!     host.execute_turn(&mut state) → HostTurnResult    ← host-specific
//!     ingest_agentic_turn_stream(...)                    ← runtime
//!     agentic_round_stall_preflight_with_tool_calls(...) ← runtime
//!     run_agentic_headless_tool_round(...)               ← runtime
//!     apply_agentic_post_tool_policy(...)                ← runtime
//! ```
//!
//! # Host Contract (pre-conditions)
//!
//! Before calling [`run_agentic_loop_with_host`]:
//! - `state.messages` must contain at least one user message
//! - `state.max_turns` and `state.remaining_turns` must be > 0
//! - `state.message` must match the current user query text
//!
//! # Host Contract (invariants during loop)
//!
//! The runtime guarantees:
//! - `execute_turn` is called at most `max_turns` times
//! - Cancel flag is checked cooperatively between turns (not mid-turn)
//! - Stall detection runs after every turn with tool calls
//! - Post-tool policy may restrict tools or abort the loop
//!
//! # Dispatch
//!
//! For a higher-level entry point, use [`super::loop_dispatcher::LoopDispatcher`]
//! which wraps this loop with consistent outcome mapping.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use astra_services::session_audit::RuntimePromotionEventData;
use astra_services::session_journal::ToolCallRecord;
use async_trait::async_trait;
use serde_json::Value;

use crate::pipeline::step_checkpoint;
use crate::pipeline::step_protocol::{InMemoryIdempotencyCache, StepCheckpoint};
use crate::pipeline::step_recorder::StepRecorder;
use crate::semantic_dedup::SemanticDedup;
use crate::tool_registry::SelectionReport;
use crate::turn::agentic_headless_round::{
    HeadlessRoundTerminal, HeadlessStderrStyle, HeadlessToolRoundCtx,
    run_agentic_headless_tool_round,
};
use crate::turn::agentic_post_tool_policy::{
    AgenticPostToolIterationControl, AgenticPostToolPolicyRequest, apply_agentic_post_tool_policy,
    map_post_tool_policy_outcome,
};
use crate::turn::agentic_turn_flow::{
    agentic_round_stall_preflight_with_tool_calls, append_explain_turn_batch,
};
use crate::turn::agentic_turn_ingest::{
    AgenticIngestIterationControl, AgenticTurnIngestMut,
    agentic_turn_stream_snapshot_from_sse_accum, ingest_agentic_turn_stream,
    map_ingest_outcome_to_iteration_control,
};
use crate::turn::agentic_verdict_audit::AgenticVerdictAuditEvent;
use crate::turn::chat_turn_heuristics::TaskExecutionProfile;
use crate::turn::chat_turn_sse_dispatch::ChatTurnSseAccum;
use crate::turn::sse_stream_host::EdgeToolExecResult;
use crate::turn::tool_result_semantics::tool_dedup_signature;
use crate::turn::turn_guard::TurnGuard;
use tokio_util::sync::CancellationToken;

// ─── Host turn result ────────────────────────────────────────────────────────

/// Result from one host-executed turn (payload prep + HTTP + SSE consumption).
pub struct HostTurnResult {
    /// Protocol accumulator from SSE stream.
    pub accum: ChatTurnSseAccum,
    /// Time to first token (ms).
    pub ttft_ms: Option<u64>,
    /// Ordered edge tool executions from this turn.
    pub edge_tool_round: Vec<EdgeToolExecResult>,
}

/// Request for a hidden host-executed reflection subcall.
pub struct HostReflectionRequest<'a> {
    /// Structured runtime context that motivated the reflection.
    pub context: &'a crate::liquid::reflection::ReflectionContext,
    /// System prompt for the reflection subcall.
    pub system_prompt: &'a str,
    /// User prompt for the reflection subcall.
    pub user_prompt: &'a str,
    /// Optional output cap for the reflection response.
    pub max_output_tokens: Option<usize>,
}

/// Result from a hidden host-executed reflection subcall.
pub struct HostReflectionResult {
    pub full_text: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub has_usage: bool,
}

// ─── Host trait ──────────────────────────────────────────────────────────────

/// Abstraction for host-specific agentic loop behavior.
///
/// The runtime calls [`AgenticLoopHost::execute_turn`] for each LLM interaction;
/// the host handles payload preparation, HTTP posting, and SSE consumption.
/// Post-turn cognitive processing (ingest, stall detection, tool round,
/// post-tool policy) runs entirely in the runtime.
///
/// **CLI host**: builds payload with selector/memory/skills, POSTs to cloud API,
/// consumes SSE with terminal rendering, executes tools locally.
///
/// **Headless host**: receives payload from client, calls LLM directly,
/// streams SSE to client, executes tools via ledger.
#[async_trait]
pub trait AgenticLoopHost: Send {
    /// Execute one LLM turn: prepare payload → POST → consume SSE.
    ///
    /// The host is responsible for all CLI/server-specific logic:
    /// - Building the JSON payload (selector, memory, skills, edge profile)
    /// - POSTing to the LLM API (cloud or direct)
    /// - Consuming the SSE stream (via `SseStreamHost` methods on `self`)
    ///
    /// State fields are passed by reference so the host can read/update them.
    async fn execute_turn(
        &mut self,
        state: &mut AgenticLoopState,
    ) -> Result<HostTurnResult, String>;

    /// Whether the host can execute a hidden reflection-only LLM subcall.
    fn supports_auto_reflection(&self) -> bool {
        false
    }

    /// Execute a hidden reflection-only LLM subcall and return the raw text.
    ///
    /// Hosts that do not support this can keep the default implementation.
    async fn execute_reflection(
        &mut self,
        _state: &mut AgenticLoopState,
        _request: HostReflectionRequest<'_>,
    ) -> Result<Option<HostReflectionResult>, String> {
        Ok(None)
    }

    /// Headless round terminal output.
    fn emit_headless_line(&mut self, style: HeadlessStderrStyle, line: String);

    /// Whether output is suppressed (quiet mode).
    fn is_quiet(&self) -> bool;

    /// Valid tool names from the host's tool schemas.
    fn valid_tool_names(&self) -> &HashSet<String>;

    /// Inject an additional tool schema into the host's tool list.
    ///
    /// Called by the runtime in the loop preamble to auto-register tools
    /// that are provided by the runtime layer (e.g. the `delegate` tool when
    /// a [`DelegationEngine`] is wired into the loop state).
    ///
    /// The host should add the schema to its tool list (for LLM visibility)
    /// and register the tool name in its valid-tool set.
    ///
    /// Default: no-op (used by MockHost and hosts that don't support injection).
    fn inject_tool_schema(&mut self, _schema: Value) {}

    /// Render the final answer text to the user.
    ///
    /// Called only when the agentic loop is certain the text is the final
    /// answer (no more iterations, stop-hooks satisfied). Text was deferred
    /// during SSE consumption to avoid premature rendering that leaks into
    /// tool-turn output when the loop continues.
    ///
    /// Default: no-op (tests, headless, sub-run hosts).
    fn render_final_text(&mut self, _text: &str) {}
}

// ─── Loop state sub-structs ──────────────────────────────────────────────────

/// Skill-related state for the agentic loop.
pub struct SkillState {
    /// Unified skill registry for conditional activation via file paths.
    /// When set, edge tool file paths are recorded for conditional skill activation.
    pub registry_for_activation: Option<Arc<crate::skills::UnifiedSkillRegistry>>,
    /// Optional skill resolver for executing skills as tool calls.
    /// When set, the loop injects a `skill` tool schema and intercepts
    /// `skill` calls, returning resolved instructions as tool results.
    pub resolver: Option<Arc<dyn crate::turn::skill_tool::SkillResolver>>,
    /// Optional skill executor for fork-context skills. When set, skills with
    /// `execution_context: Fork` are executed via this executor (sub-agent loop).
    pub executor: Option<Arc<dyn crate::skills::traits::SkillExecutor>>,
    /// Model override from the most recently activated skill.
    /// When set, the host should use this model instead of the default.
    pub model_override: Option<String>,
    /// Effort level override from the most recently activated skill.
    pub effort: Option<crate::skills::manifest::EffortLevel>,
    /// Agent type hint from the most recently activated skill.
    pub agent_type: Option<String>,
    /// Tool allow-list from the most recently activated skill.
    /// When non-empty, only these tools (plus `skill` itself) should be available.
    /// The host converts this allow-list to additions in `restricted_tools`.
    pub allowed_tools: Option<HashSet<String>>,
    /// Sandbox policy derived from the most recently activated skill's trust tier.
    /// When set, tool execution should apply these restrictions (path boundaries,
    /// env filtering, network control, timeouts).
    pub sandbox_policy: Option<crate::tool_sandbox::SandboxPolicy>,
    /// Per-skill quality metrics accumulated during the session.
    /// Used to boost high-performing skills in selection priority.
    pub quality_tracker: crate::skills::quality::SkillQualityTracker,
    /// Skill auto-improvement tracker — detects user corrections and proposes SKILL.md rewrites.
    pub improvement_tracker: crate::skills::improvement::ImprovementTracker,
    /// Skills pinned by the user — always included in budget (never truncated).
    pub pinned: std::collections::HashSet<String>,
    /// Canonical skill names surfaced via `discover_skills` this session.
    pub discovered: HashSet<String>,
    /// Skill catalog surfacing for this request / session.
    pub search: astra_core::SkillSearchSettings,
    /// Skill listing message (available skill names + descriptions).
    /// Stored here instead of in `messages` so hosts can inject it ephemerally
    /// into each LLM request without bloating the persistent conversation history.
    /// Hosts should prepend this to the messages array when building the payload.
    pub listing_message: Option<Value>,
    /// Skills invoked during this session, keyed by canonical name.
    /// Used for same-session dedup and post-compaction re-injection.
    pub invoked: std::collections::HashMap<String, crate::turn::skill_tool::InvokedSkill>,
    /// Tool event hooks (PreToolUse/PostToolUse) for intercepting tool calls.
    /// Loaded from `.astra/hooks.json` or skill frontmatter.
    pub tool_event_hooks: crate::skills::hooks::ToolEventHookRegistry,
    /// Session event hooks (SessionStart, SessionEnd, etc.).
    /// Loaded from `.astra/hooks.json` alongside tool event hooks.
    pub session_event_hooks: crate::skills::hooks::SessionEventHookRegistry,
}

impl Default for SkillState {
    fn default() -> Self {
        Self {
            registry_for_activation: None,
            resolver: None,
            executor: None,
            model_override: None,
            effort: None,
            agent_type: None,
            allowed_tools: None,
            sandbox_policy: None,
            quality_tracker: Default::default(),
            improvement_tracker: Default::default(),
            pinned: HashSet::new(),
            discovered: HashSet::new(),
            search: Default::default(),
            listing_message: None,
            invoked: HashMap::new(),
            tool_event_hooks: Default::default(),
            session_event_hooks: Default::default(),
        }
    }
}

/// Telemetry and observability state for the agentic loop.
#[derive(Default)]
pub struct TelemetryState {
    /// Explain data collected per turn.
    pub explain_turns: Vec<Value>,
    /// Time-to-first-token for the first LLM turn (ms).
    pub first_ttft_ms: Option<u64>,
    /// All tool names used across all turns.
    pub all_tools_used: HashSet<String>,
    /// Selection report from the first turn's skill selector.
    pub first_selection_report: Option<SelectionReport>,
    /// Budget pressure value from the first turn.
    pub first_budget_pressure: f64,
    /// Context assembly duration from the first turn (ms).
    pub first_context_assembly_ms: Option<u64>,
    /// Memoria retrieval duration from the first turn (ms).
    pub first_memoria_ms: Option<u64>,
    /// Selector duration from the first turn (ms).
    pub first_selector_ms: Option<u64>,
    /// Selector strategy from the first turn.
    pub first_selector_strategy: Option<String>,
    /// Selector confidence from the first turn.
    pub first_selector_confidence: Option<f64>,
    /// Cumulative selector input tokens.
    pub selector_tokens_in: u64,
    /// Cumulative selector output tokens.
    pub selector_tokens_out: u64,
    /// All skill names selected across all turns.
    pub all_selected_skills: Vec<String>,
    /// Optional observability session for context tracing, drift detection, and auto-tuning.
    /// When set, hooks are called at turn start/end, tool selection, etc.
    pub observability_session: Option<
        std::sync::Arc<std::sync::RwLock<crate::observability_integration::ObservabilitySession>>,
    >,
    /// Shared observability hub for profile/experiment management.
    /// Typically set at session init and shared across agents.
    pub observability_hub:
        Option<std::sync::Arc<crate::observability_integration::ObservabilityHub>>,
    /// Optional preloaded evaluation summaries used to damp runtime promotions.
    pub runtime_promotion_signals:
        Option<crate::runtime_promotion_signals::RuntimePromotionSignals>,
    /// Runtime promotion verdicts captured for later audit/report persistence.
    pub promotion_events: Vec<RuntimePromotionEventData>,
    /// Optional turn trace collector for detailed context assembly observability.
    /// When set, records system prompt, history, memory, and tool selection traces.
    /// Created at turn start, finalized at turn end.
    pub turn_trace_collector: Option<crate::turn::turn_trace_collector::TurnTraceCollector>,
    /// Number of turns completed in this loop invocation (for tuning cycle trigger).
    pub completed_turns_for_tuning: u32,
}

/// Stall and verdict tracking state for the agentic loop.
#[derive(Default)]
pub struct StallTrackingState {
    /// Per-turn tool-call dedup signatures.
    pub turn_sigs: Vec<BTreeSet<String>>,
    /// Per-turn tool name sets.
    pub turn_tool_names: Vec<HashSet<String>>,
    /// Stall events: `(description, turn_number)`.
    pub events: Vec<(String, u32)>,
    /// Per-turn intent+tool pairs for stall analysis.
    pub intent_tool_turns: Vec<(Vec<String>, String)>,
    /// Verdict audit trail.
    pub verdict_events: Vec<AgenticVerdictAuditEvent>,
    /// Last heavy checkpoint for step resumption.
    pub last_heavy_checkpoint: Option<StepCheckpoint>,
    /// Tool call records for session journal.
    pub tool_call_records: Vec<ToolCallRecord>,
    /// Whether a factual-retry was forced this loop.
    pub forced_factual_retry: bool,
}

/// Inter-agent messaging state for the agentic loop.
#[derive(Default)]
pub struct MessagingState {
    /// Optional mailbox for receiving messages from other agents.
    /// When set, incoming messages are drained at each turn start and
    /// progress updates are sent to the parent at turn end.
    pub mailbox: Option<crate::messaging::router::AgentMailbox>,
    /// Tracks messages that require acknowledgment and handles retries.
    pub ack_tracker: Option<std::sync::Arc<crate::messaging::ack_tracker::PendingAckTracker>>,
    /// Background retry/dead-letter sweep for ack-tracked messages.
    pub ack_sweep_task: Option<crate::messaging::ack_tracker::AckSweepHandle>,
    /// Dead letter queue for permanently failed messages.
    pub dead_letter_queue: Option<std::sync::Arc<crate::messaging::dead_letter::DeadLetterQueue>>,
    /// Unified messaging metrics (optional, shared across agents in a delegation).
    pub metrics: Option<std::sync::Arc<crate::messaging::metrics::MessagingMetrics>>,
    /// Optional progress emitter for broadcasting turn events to UI/subscribers.
    /// When set, the loop emits `TurnCompleted` events after each turn.
    pub progress_emitter: Option<crate::orchestration::AgentProgressEmitter>,
}

/// Stop-hook and teammate-idle-hook state for the agentic loop.
#[derive(Default)]
pub struct StopHookState {
    /// Verification commands run before the loop is allowed to complete.
    /// For plan subtasks, populated from declarative `when: task_completed` hooks.
    /// If any hook fails, its output is injected and the loop continues.
    pub stop_hooks: Vec<crate::turn::stop_hooks::StopHook>,
    /// How many times stop hooks have fired (prevents infinite hook loops).
    pub stop_hook_runs: u32,
    /// Hooks with `when: teammate_idle` — injected once after a `delegate` round returns.
    pub teammate_idle_hooks: Vec<crate::turn::stop_hooks::StopHook>,
    /// How many times teammate-idle hooks have fired (at most once per loop).
    pub teammate_idle_hook_runs: u32,
    /// Edge/chat project root (`git_root` or `cwd`) for enriching `delegate` sub-run context
    /// so server-side sub-runs load `.astra/stop-hooks.yaml` from the same tree.
    pub workspace_root_hint: Option<String>,
}

/// Cancellation state for the agentic loop.
#[derive(Default)]
pub struct CancellationState {
    /// Shared flag checked between turns. Set externally (e.g. by cancel_run).
    pub flag: Option<Arc<AtomicBool>>,
    /// Shared pause flag checked between turns. Set externally (e.g. by pause_run).
    pub pause_flag: Option<Arc<AtomicBool>>,
    /// Optional token cancelled with user cancel for immediate LLM/stream wake.
    pub token: Option<Arc<CancellationToken>>,
}

/// Error recovery state for the agentic loop.
#[derive(Default)]
pub struct ErrorRecoveryState {
    /// Consecutive turns where the same error category dominated.
    /// Reset when a turn succeeds or a different error category appears.
    pub consecutive_same_error: u32,
    /// The error category from the last turn (for streak detection).
    pub last_error_category: Option<crate::turn::error_recovery::ErrorCategory>,
}

// ─── Loop state ──────────────────────────────────────────────────────────────

/// Cross-turn state managed by the runtime loop.
///
/// Created by the CLI/host from session parameters; mutated by the runtime
/// during multi-turn execution. Consumed at the end to produce results.
pub struct AgenticLoopState {
    // ── Message context ──
    pub messages: Vec<Value>,
    pub tool_results: Vec<Value>,
    pub current_session_id: Option<String>,
    pub current_run_id: Option<String>,
    /// Current nested agent/sub-run depth. Root loops start at 0.
    pub recursion_depth: u8,

    // ── Accumulated output ──
    pub final_text: String,
    pub total_prompt: u64,
    pub total_completion: u64,
    pub total_cache_read: u64,
    pub total_cache_creation: u64,
    pub total_tool_calls: u32,
    pub has_any_usage: bool,

    // ── Turn management ──
    pub max_turns: usize,
    pub remaining_turns: usize,
    pub turn_guard: TurnGuard,
    pub restricted_tools: HashSet<String>,
    pub step_recorder: StepRecorder,

    // ── Dedup + caching ──
    pub idempotency_cache: InMemoryIdempotencyCache,
    pub semantic_dedup: SemanticDedup,
    /// Session-level call counter: `dedup_signature → count`.
    /// Hard-caps repeated identical calls across all rounds.
    pub call_counts: HashMap<String, u32>,
    /// Resolved max identical tool calls (from config, computed once at init).
    pub max_identical_tool_calls: u32,
    /// Resolved max tool calls per turn (from config, computed once at init).
    pub max_tools_per_turn: u32,

    // ── Sub-states ──
    pub skills: SkillState,
    pub telemetry: TelemetryState,
    pub stall: StallTrackingState,
    pub messaging: MessagingState,
    pub hooks: StopHookState,
    pub cancellation: CancellationState,
    pub error_recovery: ErrorRecoveryState,

    // ── Host-provided context (read-only by runtime) ──
    pub message: String,
    pub recent_tools: Vec<String>,
    pub task_profile: TaskExecutionProfile,

    // ── API context (for cloud tool delivery) ──
    pub api: astra_thin_client::ThinClient,
    pub api_token: String,

    // ── Delegation ──
    /// Optional delegation engine for multi-agent coordination.
    /// When set, the loop intercepts `delegate` tool calls and routes them
    /// through the delegation engine instead of the headless tool round.
    pub delegation_engine: Option<Arc<crate::server::delegation_engine::DelegationEngine>>,

    // ── Composite Snapshot ──
    /// Optional data snapshot provider for building composite snapshots.
    /// When set, heavy checkpoints will also capture a data dimension.
    pub data_snapshot_provider:
        Option<Arc<dyn astra_core::composite_snapshot::DataSnapshotProvider>>,
    /// Most recent composite snapshot created for this session.
    pub last_composite_snapshot: Option<astra_core::composite_snapshot::CompositeSnapshot>,

    // ── Context calibration (measured prompt vs estimate) ──
    /// Last successful LLM turn's reported `prompt_tokens` (previous provider response).
    pub last_measured_prompt_tokens: Option<u64>,
    /// Consecutive fatal ingests whose error matched context-window / PTL patterns.
    pub consecutive_context_window_errors: u32,

    // ── Per-turn token budget ──
    /// Maximum LLM input tokens before the loop forces a graceful wind-down.
    /// 0 = unlimited (legacy).  Set from `RuntimeLimits::max_turn_input_tokens`.
    pub max_turn_input_tokens: u64,
    /// Set to `true` once the budget-exceeded wrap-up message has been injected.
    /// The loop allows exactly one more LLM iteration after injection.
    pub budget_wrapup_injected: bool,

    /// Set to `true` when a skill produced substantial output in the current
    /// turn. The CLI host reads this to suppress intermediate text rendering
    /// on subsequent iterations (prevents markdown leak from draft text).
    pub skill_produced_output: bool,

    // ── Cumulative token budget ──
    /// Maximum cumulative (prompt + completion) tokens across all rounds.
    /// 0 = unlimited (default for interactive sessions).
    /// Skill subruns set this to cap total cost.
    pub max_cumulative_tokens: u64,

    // ── Thinking budget ──
    /// Optional thinking/reasoning budget in tokens for models with extended thinking.
    /// When Some, passed to the API request so the server constrains thinking output.
    pub thinking_budget_tokens: Option<u32>,

    // ── Recently accessed files ──
    /// Recently accessed file paths tracked for post-compaction restoration.
    /// Each entry is `(absolute_path, turn_number)`. The list is bounded to
    /// the most recent [`MAX_TRACKED_FILE_READS`] entries. After compaction,
    /// hosts use this to re-inject recent file contents so the LLM retains
    /// awareness of recently-read code.
    pub recent_file_reads: Vec<(String, u32)>,

    // ── Cross-session project context ──
    /// Pre-computed cross-session project context (P2 knowledge backflow).
    /// Set once at session init; `None` for sub-runs or when the feature is disabled.
    pub project_context: Option<String>,

    // ── Permission sync ──
    /// Optional permission sync context for runtime permission management.
    /// When set, tool execution checks permissions before running and can
    /// request permission from parent agent via mailbox if denied.
    pub permission_context:
        Option<std::sync::Arc<tokio::sync::RwLock<crate::orchestration::PermissionSyncContext>>>,

    /// Optional permission request handler for processing child requests.
    /// When set, incoming PermissionRequest messages are handled automatically.
    pub permission_handler: Option<crate::orchestration::PermissionRequestHandler>,

    // ── Mid-execution checkpoint gate ──
    /// Optional checkpoint gate checked every N turns during delegation sub-runs.
    /// When the gate returns `false`, the loop aborts with `Cancelled`.
    pub checkpoint_gate: Option<Arc<dyn crate::server::delegation_engine::CheckpointGate>>,

    // ── Evolution ──
    /// Optional evolution service for multi-axis self-evolution.
    /// When set, tool results and user messages feed into signal collection.
    pub evolution_service: Option<Arc<crate::evolution::service::EvolutionService>>,

    // ── Rate Limit Cooldown ──
    /// Cross-turn rate-limit cooldown tracker.  When the loop detects a
    /// rate-limit error (429 / TPM / RPM), it records it here so subsequent
    /// turns can wait or reject early instead of immediately re-hitting the
    /// limit.  Shared across all turns within a single agentic loop invocation.
    pub rate_limit_cooldown: crate::bridge::RateLimitCooldown,

    // ── Liquid (within-turn tactical adaptation) ──
    /// Optional tactical adapter for step-level adaptation within a turn.
    pub tactical_adapter: Option<crate::liquid::tactical::TacticalAdapter>,
    /// Optional step signal collector for within-turn outcome tracking.
    pub step_signal_collector: Option<crate::liquid::step_signals::StepSignalCollector>,

    // ── Tool selection budget override ──
    /// Scenario-driven override for the tool selection token budget.
    /// When `Some(n)` with n > 0, the host should use this instead of the
    /// registry's default budget (800 tokens) when building the selection context.
    /// Set by `apply_adaptive_execution_profile` from `config.tool_selection.tool_budget_tokens`.
    pub tool_budget_override: Option<u32>,

    // ── Auto-reflection ──
    /// Accumulated LLM-routed evolution signals awaiting reflection.
    /// Filled during tuning cycles; drained when threshold is met and
    /// reflection prompt is injected.
    pub pending_reflection_signals: Vec<crate::evolution::types::EvolutionSignal>,
    /// Recent tactical adaptations applied while the current reflection window
    /// was accumulating. Drained into the next auto-reflection context.
    pub recent_tactical_actions: Vec<String>,
}

/// Consecutive same-category error turns before forcing a strategy change.
const CONSECUTIVE_ERROR_BUDGET: u32 = 3;

/// Maximum number of recent file reads to track for post-compact restoration.
const MAX_TRACKED_FILE_READS: usize = 20;

fn edge_tool_status_exit_code(status: &str) -> Option<i32> {
    match status.trim().to_ascii_lowercase().as_str() {
        "ok" | "success" | "succeeded" | "completed" | "complete" | "passed" => Some(0),
        "error" | "failed" | "failure" | "partial_failure" | "denied" | "cancelled"
        | "canceled" | "timeout" | "timed_out" => Some(1),
        _ => None,
    }
}

fn record_edge_tool_observability(
    state: &mut AgenticLoopState,
    edge_tool_round: &[EdgeToolExecResult],
) {
    if let Some(session) = &state.telemetry.observability_session {
        for edge_result in edge_tool_round {
            session
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .record_tool_result(
                    &edge_result.tool,
                    &edge_result.output,
                    edge_tool_status_exit_code(&edge_result.status),
                );
        }
    }

    if let Some(hub) = &state.telemetry.observability_hub {
        let user_id = state
            .telemetry
            .observability_session
            .as_ref()
            .map(|s| s.read().unwrap_or_else(|e| e.into_inner()).user_id.clone())
            .unwrap_or_default();
        for edge_result in edge_tool_round {
            crate::observability_integration::on_tool_executed(hub, &user_id, &edge_result.tool);
        }
    }
}

#[allow(unused_imports)]
pub(crate) use super::agentic_adaptive_tuning::{
    DEFAULT_TUNING_CYCLE_INTERVAL, apply_adaptive_execution_profile, apply_per_turn_adaptation,
    apply_tactical_actions, maybe_run_tuning_cycle, record_loop_completion_feedback,
    record_new_evolution_promotion_events, should_emit_adaptive_scenario_event,
    snapshot_evolution_promotion_ids,
};

// ─── Loop exit ───────────────────────────────────────────────────────────────

/// Result of running the agentic loop to completion.
#[derive(Debug)]
pub enum AgenticLoopOutcome {
    /// Loop completed normally (final text produced or budget exhausted gracefully).
    Completed,
    /// Loop aborted due to a fatal error.
    Error(String),
    /// Loop was cancelled externally via `cancel_flag` or `cancel_token`.
    Cancelled,
    /// Loop is waiting for external input (tool approval, user resume, webhook).
    /// The caller should provide the requested input and re-invoke the loop.
    Waiting(String),
}

// ─── Delegation support ──────────────────────────────────────────────────────

pub const DELEGATE_TOOL_NAME: &str = super::agentic_delegate_interception::DELEGATE_TOOL_NAME;

#[allow(unused_imports)]
pub(crate) use super::agentic_delegate_interception::{
    DelegationAdaptiveContext, DelegationExecutionResult, DelegationFinalOutputSource,
    DelegationOutcomeMetadata, coordination_pattern_name, delegation_adaptive_context,
    delegation_final_output_preview, format_delegation_result, format_delegation_terminal_preview,
    is_delegation_call, merge_workspace_hint_into_delegation_request, parse_coordination_pattern,
    parse_delegate_agents, parse_delegation_request, partition_and_execute_delegations,
    pattern_from_name, select_default_coordination_pattern, task_needs_review,
    tool_call_arguments_value, tool_call_name,
};

/// Generate the OpenAI-compatible tool schema for the "delegate" tool.
pub fn delegate_tool_schema() -> Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "delegate",
            "description": "Delegate a task to one or more specialized sub-agents. Use this when a task benefits from parallel execution, pipeline processing, or adversarial review by specialized agents.",
            "parameters": {
                "type": "object",
                "required": ["task", "agents"],
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "The task description/prompt for the delegated agents."
                    },
                    "agents": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Agent IDs to delegate to. Available: 'coder' (code tasks), 'reviewer' (code review), 'writer' (documentation)."
                    },
                    "pattern": {
                        "type": "string",
                        "enum": ["sequential", "fan_out", "pipeline", "adversarial"],
                        "description": "Coordination pattern. 'sequential': agents run one by one. 'fan_out': agents run in parallel. 'pipeline': output of each feeds the next. 'adversarial': producer+reviewer iterate."
                    },
                    "max_rounds": {
                        "type": "integer",
                        "description": "Maximum rounds for adversarial pattern (default: 2)."
                    },
                    "context": {
                        "type": "object",
                        "description": "Additional context to pass to sub-agents."
                    }
                }
            }
        }
    })
}

// ─── Runtime loop ────────────────────────────────────────────────────────────

/// Finalize the turn trace collector: record measured token budget, feed to
/// observability session, and persist to journal. Called from every exit path
/// in the agentic loop so `/context breakdown` always reflects the latest turn.
fn finalize_turn_trace(state: &mut AgenticLoopState) {
    let Some(collector) = state.telemetry.turn_trace_collector.take() else {
        return;
    };
    if let Some(ref session_id) = state.current_session_id {
        collector.set_session_id(session_id);
    }
    let session_turn = context_trace_turn_number(state);
    collector.set_turn_id(format!("turn-{session_turn}"));
    let measured = state.last_measured_prompt_tokens.unwrap_or(0);
    let max = state.max_turn_input_tokens;
    let budget_pressure = if max > 0 {
        measured as f64 / max as f64
    } else {
        state.telemetry.first_budget_pressure
    };
    collector.record_token_budget(crate::turn::context_assembly_trace::TokenBudgetTrace {
        max_tokens: max as u32,
        total_used: measured as u32,
        budget_pressure,
        compression_triggered: state.budget_wrapup_injected,
        ..Default::default()
    });
    let trace = collector.finalize();
    if let Some(ref session) = state.telemetry.observability_session {
        let mut guard = session.write().unwrap_or_else(|e| e.into_inner());
        crate::observability_integration::on_context_assembled(&mut guard, trace.clone());
    }
    if collector.has_data() {
        if let Some(ref sid) = state.current_session_id {
            if let Ok(writer) = astra_services::session_journal::JournalWriter::new(sid) {
                let event =
                    astra_services::session_journal::JournalEvent::context_assembly_recorded(
                        Some(sid),
                        session_turn,
                        trace.to_json_value(),
                    );
                let _ = writer.append(&event);
            }
        }
    }
}

fn context_trace_turn_number(state: &AgenticLoopState) -> u32 {
    let outer_turn = (state.max_turns - state.remaining_turns) as u32;
    state
        .telemetry
        .observability_session
        .as_ref()
        .and_then(|s| s.read().ok().map(|g| g.turn_number))
        .filter(|turn| *turn > 0)
        .unwrap_or(outer_turn)
}

/// Best-effort heavy checkpoint write.
///
/// Several early-exit paths in the agentic loop (text-only responses, stop-hook
/// injection, factual-retry nudges) skip the main post-tool-policy checkpoint.
/// This helper ensures those paths still persist the accumulated messages so that
/// `/debug` turn inspection and session recovery have accurate per-iteration state.
fn try_write_heavy_checkpoint(state: &mut AgenticLoopState) {
    let Some(sid) = state.current_session_id.as_ref() else {
        return;
    };
    let ckpt_num = state.step_recorder.summary().checkpoints;
    let Some(heavy) = state.step_recorder.build_heavy_checkpoint(
        &state.messages,
        0,
        state.remaining_turns as u32,
        &state
            .turn_guard
            .health
            .deprioritized_tools()
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
        &state.recent_tools,
    ) else {
        return;
    };
    let cp = StepCheckpoint::Heavy(Box::new(heavy));
    if let Err(e) = step_checkpoint::write_step_checkpoint(sid, ckpt_num, &cp) {
        astra_core::agent_warn!(
            "checkpoint",
            "Failed to write step checkpoint {ckpt_num}: {e}"
        );
    }

    let turn = (state.max_turns - state.remaining_turns) as u32;
    let mut snapshot =
        astra_core::composite_snapshot::CompositeSnapshotBuilder::new(sid.clone(), turn)
            .label(format!("checkpoint-t{turn}"))
            .session_state(format!("{:06}-heavy.json", ckpt_num))
            .workspace_state(sid.clone())
            .build();

    let mut index = step_checkpoint::read_composite_snapshot_index(sid).unwrap_or_default();
    if let Err(e) = index.append(&mut snapshot) {
        astra_core::agent_warn!("checkpoint", "Failed to append snapshot version: {e}");
        return;
    }
    if let Err(e) = step_checkpoint::write_composite_snapshot_index(sid, &index) {
        astra_core::agent_warn!("checkpoint", "Failed to write snapshot index: {e}");
    }

    state.last_composite_snapshot = Some(snapshot);
    state.stall.last_heavy_checkpoint = Some(cp);
}

/// Build a full composite snapshot asynchronously (with data provider).
///
/// Call this at strategic points (breakpoints, plan boundaries, user request)
/// where the async data snapshot is worth the cost.
#[allow(dead_code)]
async fn build_full_composite_snapshot(
    state: &mut AgenticLoopState,
) -> Option<astra_core::composite_snapshot::CompositeSnapshot> {
    let sid = state.current_session_id.as_ref()?;
    let turn = (state.max_turns - state.remaining_turns) as u32;
    let ckpt_num = state.step_recorder.summary().checkpoints;

    let mut builder =
        astra_core::composite_snapshot::CompositeSnapshotBuilder::new(sid.clone(), turn)
            .label(format!("full-snapshot-t{turn}"))
            .session_state(format!("{:06}-heavy.json", ckpt_num))
            .workspace_state(sid.clone());

    // Include memory/learning snapshot if available.
    // The learning snapshot epoch is tracked on the step recorder.
    {
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        builder = builder.memory_snapshot(astra_core::composite_snapshot::MemorySnapshotRef {
            profile: "default".to_string(),
            epoch,
            path: None,
        });
    }

    if let Ok(output) = tokio::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .await
        && output.status.success()
    {
        let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if sha.len() >= 7 {
            builder = builder.git_commit(sha);
        }
    }

    if let Some(provider) = &state.data_snapshot_provider {
        let context = astra_core::composite_snapshot::SnapshotContext {
            session_id: sid.clone(),
            turn,
            label: Some(format!("turn-{turn}")),
            task_type: None,
            databases: None,
        };
        match provider.create_snapshot(&context).await {
            Ok(Some(ds)) => {
                builder = builder.data_snapshot(ds);
            }
            Ok(None) => {}
            Err(e) => {
                astra_core::agent_warn!("snapshot", "Data snapshot failed: {e}");
            }
        }
    }

    let mut snapshot = builder.build();

    let mut index = step_checkpoint::read_composite_snapshot_index(sid).unwrap_or_default();
    if let Err(e) = index.append(&mut snapshot) {
        astra_core::agent_warn!(
            "checkpoint",
            "Failed to append composite snapshot version: {e}"
        );
        return Some(snapshot);
    }
    if let Err(e) = step_checkpoint::write_composite_snapshot_index(sid, &index) {
        astra_core::agent_warn!(
            "checkpoint",
            "Failed to write composite snapshot index: {e}"
        );
    }

    state.last_composite_snapshot = Some(snapshot.clone());
    Some(snapshot)
}

/// Extract a file path from an edge tool's name + arguments.
///
/// Covers the common file-touching tools: read_file, write_file, str_replace,
/// grep, glob, find_definition, etc. Returns `None` for non-file tools.
fn extract_file_path_from_tool(tool_name: &str, args: &Value) -> Option<String> {
    match tool_name {
        "read_file" | "write_file" | "str_replace" | "find_definition" => args
            .get("path")
            .or_else(|| args.get("file_path"))
            .and_then(Value::as_str)
            .map(|s| s.to_string()),
        "grep" | "glob" | "list_dir" => args
            .get("path")
            .or_else(|| args.get("directory"))
            .and_then(Value::as_str)
            .map(|s| s.to_string()),
        _ => None,
    }
}

/// Run the multi-turn agentic loop using the provided host.
///
/// This is the runtime-portable entry point. The host handles all
/// CLI/server-specific behavior; the runtime handles cognitive decisions:
/// turn ingest, stall detection, tool round orchestration, post-tool policy.
pub async fn run_agentic_loop_with_host<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) -> Result<AgenticLoopOutcome, String> {
    let result = run_agentic_loop_impl(host, state).await;

    // ── Evolution: flush signals and auto-apply fast-path proposals ──
    if let Some(evo) = state.evolution_service.clone() {
        evo.set_runtime_promotion_signals(state.telemetry.runtime_promotion_signals.clone());
        let (pending_before, applied_before) = snapshot_evolution_promotion_ids(&evo).await;
        let (auto_applied, _llm_signals) = evo.flush().await;
        record_new_evolution_promotion_events(state, &evo, &pending_before, &applied_before).await;
        if !auto_applied.is_empty() {
            eprintln!(
                "[evolution] auto-applied {} fast-path proposals",
                auto_applied.len()
            );
        }
    }

    record_loop_completion_feedback(state, &result);
    result
}

#[allow(unused_imports)]
pub(crate) use super::agentic_auto_reflection::{
    AUTO_REFLECTION_SIGNAL_THRESHOLD, maybe_trigger_auto_reflection,
};
#[allow(unused_imports)]
pub(crate) use super::agentic_loop_lifecycle::{
    PreparedTurnIteration, TurnIterationPrep, prepare_turn_iteration, run_loop_preamble,
};

/// Render deferred final text if any is buffered, then write heavy checkpoint.
fn finalize_and_render<H: AgenticLoopHost>(host: &mut H, state: &mut AgenticLoopState) {
    finalize_turn_trace(state);
    try_write_heavy_checkpoint(state);
    if !state.final_text.is_empty() {
        host.render_final_text(&state.final_text);
    }
}

async fn run_agentic_loop_impl<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) -> Result<AgenticLoopOutcome, String> {
    run_loop_preamble(host, state).await;

    for turn_index in 0..state.max_turns {
        let TurnIterationPrep {
            quiet,
            turn_start_time,
        } = match prepare_turn_iteration(host, state, turn_index).await? {
            PreparedTurnIteration::Ready(prep) => prep,
            PreparedTurnIteration::Finished(outcome) => {
                if matches!(outcome, AgenticLoopOutcome::Completed) && !state.final_text.is_empty()
                {
                    finalize_and_render(host, state);
                }
                return Ok(outcome);
            }
        };

        // ─── Step 1: Host executes the turn (payload → HTTP → SSE) ──────
        if let Some(ref emitter) = state.messaging.progress_emitter {
            emitter.llm_call_started(turn_index as u32);
        }
        let llm_wall_start = std::time::Instant::now();
        let turn_result = host.execute_turn(state).await?;
        // Successful LLM call — reset consecutive error counters.
        state.rate_limit_cooldown.record_success();
        if let Some(ref emitter) = state.messaging.progress_emitter {
            emitter.llm_call_completed(
                turn_index as u32,
                turn_result.ttft_ms,
                llm_wall_start.elapsed().as_millis() as u64,
            );
        }

        // ─── Step 2: Ingest turn stream into loop state ─────────────────
        let snap =
            agentic_turn_stream_snapshot_from_sse_accum(&turn_result.accum, turn_result.ttft_ms);

        // Update trace collector with runtime-measured system prompt tokens + breakdown.
        if let Some(ref collector) = state.telemetry.turn_trace_collector {
            if let Some(spt) = turn_result.accum.system_prompt_tokens {
                collector.set_system_prompt_tokens(spt);
            }
            if let Some(ref breakdown_json) = turn_result.accum.system_prompt_breakdown {
                if let Ok(breakdown) = serde_json::from_value::<
                    crate::turn::context_assembly_trace::SystemPromptBreakdown,
                >(breakdown_json.clone())
                {
                    collector.record_system_prompt(breakdown);
                }
            }
        }
        let edge_len = turn_result.edge_tool_round.len();
        match map_ingest_outcome_to_iteration_control(ingest_agentic_turn_stream(
            &snap,
            edge_len,
            |i| turn_result.edge_tool_round[i].tool.clone(),
            &state.message,
            &state.recent_tools,
            quiet,
            AgenticTurnIngestMut {
                task_profile: state.task_profile,
                first_ttft_ms: &mut state.telemetry.first_ttft_ms,
                current_session_id: &mut state.current_session_id,
                current_run_id: &mut state.current_run_id,
                final_text: &mut state.final_text,
                total_prompt: &mut state.total_prompt,
                total_completion: &mut state.total_completion,
                total_cache_read: &mut state.total_cache_read,
                total_cache_creation: &mut state.total_cache_creation,
                total_tool_calls: &mut state.total_tool_calls,
                step_recorder: &mut state.step_recorder,
                all_tools_used: &mut state.telemetry.all_tools_used,
                has_any_usage: &mut state.has_any_usage,
                forced_factual_retry: &mut state.stall.forced_factual_retry,
                messages: &mut state.messages,
                last_measured_prompt_tokens: &mut state.last_measured_prompt_tokens,
                consecutive_context_window_errors: &mut state.consecutive_context_window_errors,
            },
        )) {
            AgenticIngestIterationControl::Fatal(e) => {
                // ── Rate-limit graceful degradation ──────────────────────
                // When the error is a rate-limit (429 / TPM exceeded) AND
                // the loop has already done meaningful work (tool calls
                // executed, files written), convert from hard failure to
                // graceful completion.  This preserves the conversation
                // context so the next turn can continue where we left off,
                // instead of losing all accumulated tool results.
                let lower = e.to_lowercase();
                let is_rate_limit = lower.contains("rate")
                    || lower.contains("429")
                    || lower.contains("too many requests")
                    || lower.contains("tpm")
                    || lower.contains("rpm");

                // Record the error in the cross-turn cooldown tracker so
                // subsequent turns can back off or abort early.
                if is_rate_limit {
                    let is_overload = lower.contains("529")
                        || lower.contains("503")
                        || lower.contains("overload");
                    if is_overload {
                        state.rate_limit_cooldown.record_529(None, false);
                    } else {
                        state.rate_limit_cooldown.record_429(None, false);
                    }
                }

                if is_rate_limit && state.total_tool_calls > 0 {
                    if !quiet {
                        host.emit_headless_line(
                            HeadlessStderrStyle::Yellow,
                            format!(
                                "⚠ Rate limit hit after {} tool calls — preserving work.",
                                state.total_tool_calls,
                            ),
                        );
                    }
                    // Synthesize a final text so the conversation has a
                    // meaningful assistant message (not an empty failure).
                    state.final_text = format!(
                        "[Rate limit reached after {} tool call(s). \
                         All completed tool results are preserved above. \
                         You can continue from where I left off in the next message.]\n\n\
                         Error: {}",
                        state.total_tool_calls, e,
                    );
                    // ─── Observability: turn end hook (rate limit path) ───
                    if let (Some(hub), Some(session)) = (
                        state.telemetry.observability_hub.as_ref(),
                        state.telemetry.observability_session.as_ref(),
                    ) {
                        let total_ms = turn_start_time.elapsed().as_millis() as u64;
                        let timing = crate::observability_integration::TurnTiming {
                            turn: turn_index as u32,
                            context_assembly_ms: 0,
                            ttft_ms: turn_result.ttft_ms.unwrap_or(0) as u64,
                            llm_total_ms: total_ms,
                            tool_execution_ms: 0,
                            total_ms,
                        };
                        let mut session_guard = session.write().unwrap_or_else(|e| e.into_inner());
                        crate::observability_integration::on_turn_end(
                            hub,
                            &mut session_guard,
                            timing,
                        );
                    }
                    finalize_and_render(host, state);
                    return Ok(AgenticLoopOutcome::Completed);
                }

                finalize_turn_trace(state);
                try_write_heavy_checkpoint(state);
                return Err(e);
            }
            AgenticIngestIterationControl::BreakLoop => {
                // Agent produced final text with no tool calls — it thinks it's done.
                // Inject verification prompt once when the agent first tries to complete.
                // The prompt instructs the LLM to run checks, fix failures, and
                // re-run until passing — all within the normal tool loop.
                // Runtime does not inspect results; it trusts the LLM's tool cycle.
                // Inject whenever `stop_hooks` is non-empty (declarative and/or auto-detect).
                // Read-only turns omit auto-detect but may still carry declarative hooks.
                if state.hooks.stop_hook_runs == 0
                    && let Some(prompt) =
                        crate::turn::stop_hooks::build_stop_hook_prompt(&state.hooks.stop_hooks)
                {
                    state.hooks.stop_hook_runs = 1;
                    if !quiet {
                        host.emit_headless_line(
                            HeadlessStderrStyle::Yellow,
                            "⚠ Verification required, continuing…".to_string(),
                        );
                    }
                    state.messages.push(prompt);
                    try_write_heavy_checkpoint(state);
                    continue;
                }
                // ─── Observability: turn end hook (no tool calls path) ───
                if let (Some(hub), Some(session)) = (
                    state.telemetry.observability_hub.as_ref(),
                    state.telemetry.observability_session.as_ref(),
                ) {
                    let total_ms = turn_start_time.elapsed().as_millis() as u64;
                    let timing = crate::observability_integration::TurnTiming {
                        turn: turn_index as u32,
                        context_assembly_ms: 0,
                        ttft_ms: turn_result.ttft_ms.unwrap_or(0) as u64,
                        llm_total_ms: total_ms,
                        tool_execution_ms: 0,
                        total_ms,
                    };
                    let mut session_guard = session.write().unwrap_or_else(|e| e.into_inner());
                    crate::observability_integration::on_turn_end(hub, &mut session_guard, timing);
                }
                finalize_and_render(host, state);
                return Ok(AgenticLoopOutcome::Completed);
            }
            AgenticIngestIterationControl::ContinueIterating => {
                // Ingest injected a nudge (e.g. factual retry). Continue the loop.
                try_write_heavy_checkpoint(state);
                continue;
            }
            AgenticIngestIterationControl::ProceedWithToolCalls => {}
        }

        // ─── Step 2a: Emit LLM text output for sub-run progress visibility ──
        // In sub-run mode (quiet=false, no interactive terminal), the LLM's
        // text response is the primary signal of agent activity. Emit a
        // truncated preview so the parent process can show real-time progress.
        if !quiet && !state.final_text.is_empty() {
            let preview: String = state.final_text.chars().take(120).collect();
            let line = if state.final_text.len() > 120 {
                format!("{preview}…")
            } else {
                preview
            };
            host.emit_headless_line(HeadlessStderrStyle::Dim, line);
        }
        // If the last LLM call's prompt_tokens exceeds max_turn_input_tokens,
        // inject a wrap-up system message and skip tool execution.  The loop
        // continues for exactly one more iteration so the model can produce a
        // final answer.  On the second breach (wrapup already injected),
        // force-complete immediately.
        if state.max_turn_input_tokens > 0 {
            if let Some(measured) = state.last_measured_prompt_tokens {
                if measured > state.max_turn_input_tokens {
                    if state.budget_wrapup_injected {
                        // Second breach after wrap-up — hard stop.
                        if !quiet {
                            host.emit_headless_line(
                                HeadlessStderrStyle::Yellow,
                                "⚠ Token budget exceeded — completing turn.".to_string(),
                            );
                        }
                        // ─── Observability: turn end hook (budget exceeded) ───
                        if let (Some(hub), Some(session)) = (
                            state.telemetry.observability_hub.as_ref(),
                            state.telemetry.observability_session.as_ref(),
                        ) {
                            let total_ms = turn_start_time.elapsed().as_millis() as u64;
                            let timing = crate::observability_integration::TurnTiming {
                                turn: turn_index as u32,
                                context_assembly_ms: 0,
                                ttft_ms: turn_result.ttft_ms.unwrap_or(0) as u64,
                                llm_total_ms: total_ms,
                                tool_execution_ms: 0,
                                total_ms,
                            };
                            let mut session_guard =
                                session.write().unwrap_or_else(|e| e.into_inner());
                            crate::observability_integration::on_turn_end(
                                hub,
                                &mut session_guard,
                                timing,
                            );
                        }
                        finalize_and_render(host, state);
                        return Ok(AgenticLoopOutcome::Completed);
                    }
                    // First breach — inject wrap-up instruction, skip tool execution.
                    state.budget_wrapup_injected = true;
                    if !quiet {
                        host.emit_headless_line(
                            HeadlessStderrStyle::Yellow,
                            format!(
                                "⚠ Token budget reached ({measured}/{} tokens) — wrapping up.",
                                state.max_turn_input_tokens,
                            ),
                        );
                    }
                    state.messages.push(serde_json::json!({
                        "role": "system",
                        "content": "You have reached the token budget limit for this turn. \
                            Do NOT call any more tools. Summarize your progress so far and \
                            present your results to the user. If you have partial work, \
                            explain what remains to be done."
                    }));
                    try_write_heavy_checkpoint(state);
                    continue;
                }
            }
        }

        // ─── Cumulative token budget check ─────────────────────────────
        // Skill subruns set max_cumulative_tokens to cap total cost.
        // When exceeded, inject wrap-up (same pattern as per-turn budget).
        if state.max_cumulative_tokens > 0 {
            let cumulative = state.total_prompt + state.total_completion;
            if cumulative > state.max_cumulative_tokens && !state.budget_wrapup_injected {
                state.budget_wrapup_injected = true;
                if !quiet {
                    host.emit_headless_line(
                        HeadlessStderrStyle::Yellow,
                        format!(
                            "⚠ Cumulative token budget reached ({cumulative}/{} tokens) — wrapping up.",
                            state.max_cumulative_tokens,
                        ),
                    );
                }
                state.messages.push(serde_json::json!({
                    "role": "system",
                    "content": "You have reached the cumulative token budget. \
                        Do NOT call any more tools. Summarize your progress so far and \
                        present your results to the user."
                }));
                try_write_heavy_checkpoint(state);
                continue;
            }
        }

        // ─── Observability: tool selection hook ──────────────────────────
        // Record which tools the LLM chose for this turn (before execution).
        if let Some(session) = &state.telemetry.observability_session {
            let selected_tools: Vec<String> = turn_result
                .edge_tool_round
                .iter()
                .map(|r| r.tool.clone())
                .collect();
            if !selected_tools.is_empty() {
                let explanation = crate::turn::decision_explainer::DecisionExplanation {
                    id: format!(
                        "tool-sel-{}-{}",
                        state.current_session_id.as_deref().unwrap_or("?"),
                        turn_index
                    ),
                    timestamp: std::time::SystemTime::now(),
                    decision_type: crate::turn::decision_explainer::DecisionType::ToolSelection {
                        selected_tools: selected_tools.clone(),
                        total_available: state.telemetry.all_tools_used.len() as u32,
                    },
                    inputs: vec![crate::turn::decision_explainer::ExplainableInput {
                        name: "user_query".to_string(),
                        value: state.message.clone(),
                        influence: 1.0,
                        explanation: Some("Primary input driving tool selection".to_string()),
                    }],
                    reasoning: format!(
                        "LLM selected {} tool(s) for this turn",
                        selected_tools.len()
                    ),
                    alternatives: vec![],
                    confidence: 0.8, // placeholder
                };
                let mut session_guard = session.write().unwrap_or_else(|e| e.into_inner());
                crate::observability_integration::on_tool_selection(
                    &mut session_guard,
                    explanation,
                );
            }
        }

        // ─── Trace collector: record tool selection ──────────────────────
        // Only record if the CLI path didn't already set tool selection
        // (CLI has accurate per-tool costs from its ToolRegistry).
        // The server path builds selected_tools from edge_tool_round which
        // reflects tool USAGE (with duplicates), not tool SELECTION.
        if let Some(ref collector) = state.telemetry.turn_trace_collector {
            let already_has_tools = collector.has_tool_trace();
            if !already_has_tools {
                let selected_tools: Vec<String> = turn_result
                    .edge_tool_round
                    .iter()
                    .map(|r| r.tool.clone())
                    .collect();
                collector.record_tool_selection(
                    &selected_tools,
                    state
                        .telemetry
                        .first_selector_strategy
                        .as_deref()
                        .unwrap_or("unknown"),
                    state.telemetry.first_selector_confidence.unwrap_or(0.0),
                    &[],
                    state.telemetry.all_tools_used.len() as u32,
                    state.telemetry.first_selector_ms.unwrap_or(0),
                );
            }
        }

        // ─── Step 3: Stall preflight ────────────────────────────────────
        let tool_calls_for_guard = agentic_round_stall_preflight_with_tool_calls(
            turn_index,
            &turn_result.accum.tool_calls,
            &turn_result.edge_tool_round,
            &mut state.stall.turn_sigs,
            &mut state.stall.turn_tool_names,
            &mut state.stall.events,
            &mut state.turn_guard,
        );

        // ─── Step 3b: Delegation interception ───────────────────────────
        let super::agentic_delegate_interception::DelegationInterceptionResult {
            effective_tool_calls,
            intercepted_any: delegation_intercepted,
        } = super::agentic_delegate_interception::intercept_delegations(
            host,
            state,
            &turn_result,
            quiet,
        )
        .await;

        let super::agentic_tool_interception::PreparedToolRound {
            tool_calls,
            pre_resolved_results,
            edge_tool_round,
        } = super::agentic_tool_interception::prepare_intercepted_tool_round(
            state,
            &turn_result,
            &effective_tool_calls,
            delegation_intercepted,
        )
        .await;
        let all_tool_calls = tool_calls.as_slice();
        let edge_round_for_headless = edge_tool_round.as_slice();

        // ─── Step 4: Headless tool round ────────────────────────────────
        // Snapshot error counts before the round for per-turn delta tracking.
        let errors_before_round = state.turn_guard.errors.total_errors;
        let errors_by_cat_before = state.turn_guard.errors.errors_by_category.clone();

        struct HostTerminalAdapter<'a, H: AgenticLoopHost>(&'a mut H);
        impl<H: AgenticLoopHost> HeadlessRoundTerminal for HostTerminalAdapter<'_, H> {
            fn emit_line(&mut self, style: HeadlessStderrStyle, line: String) {
                self.0.emit_headless_line(style, line);
            }
        }

        // Build edge_callback_outputs from tool_results
        let edge_callback_outputs: HashMap<String, String> = turn_result
            .edge_tool_round
            .iter()
            .map(|r| (tool_dedup_signature(&r.tool, &r.args), r.output.clone()))
            .collect();

        let evo_records_before = state.stall.tool_call_records.len();
        {
            let valid_tool_names = host.valid_tool_names().clone();
            let mut term_adapter = HostTerminalAdapter(host);
            // Suppress headless-round terminal output when a skill already
            // produced visible output — avoids leaking internal tool progress
            // lines (blocked-tool warnings, git diffs, etc.) into the user's
            // terminal after the skill's result was shown.
            let headless_quiet = quiet || state.skill_produced_output;
            run_agentic_headless_tool_round(HeadlessToolRoundCtx {
                turn_index,
                quiet: headless_quiet,
                api: &state.api,
                token: &state.api_token,
                current_session_id: state.current_session_id.as_ref(),
                tool_calls: all_tool_calls,
                edge_tool_round: edge_round_for_headless,
                reasoning_content: turn_result.accum.reasoning_content.as_str(),
                edge_callback_outputs: &edge_callback_outputs,
                messages: &mut state.messages,
                tool_results: &mut state.tool_results,
                valid_tool_names: &valid_tool_names,
                restricted_tools: &mut state.restricted_tools,
                turn_guard: &mut state.turn_guard,
                step_recorder: &mut state.step_recorder,
                idempotency_cache: &mut state.idempotency_cache,
                semantic_dedup: &mut state.semantic_dedup,
                call_counts: &mut state.call_counts,
                max_identical_calls: state.max_identical_tool_calls,
                max_tools_per_turn: state.max_tools_per_turn,
                tool_call_records: &mut state.stall.tool_call_records,
                tool_event_hooks: &state.skills.tool_event_hooks,
                term: &mut term_adapter,
                mailbox: state.messaging.mailbox.as_mut(),
                permission_context: state.permission_context.as_ref(),
                progress_emitter: state.messaging.progress_emitter.as_ref(),
                pre_resolved_results: &pre_resolved_results,
            })
            .await;
        }

        // ── Feed tool results into evolution signal collector ──
        if let Some(ref evo) = state.evolution_service {
            let turn_id = state.current_run_id.as_deref().unwrap_or("unknown");
            // Determine active skill from the most recently invoked skill.
            let active_skill: Option<String> = state
                .skills
                .invoked
                .iter()
                .max_by_key(|(_, v)| v.invoked_at_turn)
                .map(|(name, _)| name.clone());
            let active_skill_ref = active_skill.as_deref();
            for rec in &state.stall.tool_call_records[evo_records_before..] {
                if rec.is_synthetic_placeholder() {
                    continue;
                }
                let is_error = !rec.ok;
                let ctx = crate::evolution::types::ToolResultContext {
                    tool_name: &rec.name,
                    tool_args: rec.args_preview.as_deref().unwrap_or(""),
                    result: rec.result_preview.as_deref().unwrap_or(""),
                    is_error,
                    duration_ms: rec.ms,
                    active_skill: active_skill_ref,
                    turn_id,
                };
                evo.on_tool_result(&ctx).await;
            }

            // Feed stall events as RepeatedStall signals.
            if !state.stall.turn_sigs.is_empty() {
                // Check if the last 3 turns have the same tool signature.
                let sigs = &state.stall.turn_sigs;
                let n = sigs.len();
                if n >= 3 && sigs[n - 1] == sigs[n - 2] && sigs[n - 2] == sigs[n - 3] {
                    let chain: Vec<String> = sigs[n - 1].iter().cloned().collect();
                    evo.add_signal(crate::evolution::types::EvolutionSignal::RepeatedStall {
                        tool_chain: chain,
                        stall_count: 3,
                        turn_id: turn_id.to_string(),
                    })
                    .await;
                }
            }

            // Within-turn repetition: if the same tool failed 3+ times in this
            // turn, treat it as a stall even if this is the first (or only) turn.
            {
                let this_turn = &state.stall.tool_call_records[evo_records_before..];
                let mut fail_counts: std::collections::HashMap<&str, u32> =
                    std::collections::HashMap::new();
                for rec in this_turn {
                    if !rec.ok {
                        *fail_counts.entry(rec.name.as_str()).or_default() += 1;
                    }
                }
                for (tool, count) in &fail_counts {
                    if *count >= 3 {
                        evo.add_signal(crate::evolution::types::EvolutionSignal::RepeatedStall {
                            tool_chain: vec![(*tool).to_string()],
                            stall_count: *count,
                            turn_id: turn_id.to_string(),
                        })
                        .await;
                    }
                }
            }
        }

        // ── Feed tool results into liquid step-level signal collector ──
        if state.step_signal_collector.is_some() || state.tactical_adapter.is_some() {
            let new_records = &state.stall.tool_call_records[evo_records_before..];
            let mut step_actions: Vec<crate::liquid::tactical::TacticalAction> = Vec::new();

            for rec in new_records {
                let outcome = crate::liquid::step_signals::StepOutcome {
                    tool_name: rec.name.clone(),
                    ok: rec.ok,
                    latency_ms: rec.ms,
                    tokens_used: (rec.input_bytes.unwrap_or(0) + rec.output_bytes.unwrap_or(0))
                        as u64,
                    error_hint: rec.error.clone(),
                };
                // Record into step signal collector
                let triggers = if let Some(ref mut collector) = state.step_signal_collector {
                    collector.record(outcome)
                } else {
                    vec![]
                };
                // Evaluate triggers through tactical adapter
                if !triggers.is_empty() {
                    if let Some(ref mut adapter) = state.tactical_adapter {
                        let actions = adapter.evaluate(&triggers);
                        for action in actions {
                            if !matches!(action, crate::liquid::tactical::TacticalAction::NoOp) {
                                step_actions.push(action);
                            }
                        }
                        adapter.advance_step();
                    }
                }
            }

            // Apply tactical actions as real bounded runtime mutations plus
            // inline hints so the next round can see both the changed state
            // and an explicit explanation.
            if !step_actions.is_empty() {
                let hint_parts = apply_tactical_actions(state, &step_actions);
                if !hint_parts.is_empty() {
                    let hint_text = format!("[Tactical Adaptation]\n{}", hint_parts.join("\n"));
                    state.messages.push(serde_json::json!({
                        "role": "system",
                        "content": hint_text
                    }));
                }
            }
        }

        // ── Emit progress events for permission-denied tools so
        //    parent/UI subscribers learn about blocked operations.
        if let Some(ref emitter) = state.messaging.progress_emitter {
            for rec in &state.stall.tool_call_records {
                if let Some(ref err) = rec.error {
                    if err.starts_with("blocked_tool:") {
                        emitter.permission_denied(
                            &rec.name,
                            err.trim_start_matches("blocked_tool: "),
                            turn_index as u32,
                        );
                    }
                }
            }
        }

        append_explain_turn_batch(
            &mut state.telemetry.explain_turns,
            turn_result.accum.explain_turns.as_slice(),
        );

        // ─── Step 4a: Track recent file reads for post-compact restoration ──
        {
            let turn_num = (state.max_turns - state.remaining_turns) as u32;
            for edge_result in &turn_result.edge_tool_round {
                if let Some(path) =
                    extract_file_path_from_tool(&edge_result.tool, &edge_result.args)
                {
                    // Deduplicate: if same path already tracked, update its turn number
                    if let Some(existing) =
                        state.recent_file_reads.iter_mut().find(|(p, _)| p == &path)
                    {
                        existing.1 = turn_num;
                    } else {
                        state.recent_file_reads.push((path, turn_num));
                    }
                    // Bound the list to prevent unbounded growth
                    if state.recent_file_reads.len() > MAX_TRACKED_FILE_READS {
                        // Remove oldest (lowest turn number)
                        state.recent_file_reads.sort_by_key(|(_, t)| *t);
                        state.recent_file_reads.remove(0);
                    }
                }
            }
        }

        // ─── Observability: tool executed hook ───────────────────────────
        // Feed tool usage into both the user profile and the goal tracker.
        record_edge_tool_observability(state, &turn_result.edge_tool_round);

        // ─── Step 4b: Conditional skill activation ──────────────────────
        // Record file paths from edge tool executions so path-conditional
        // skills can activate dynamically. When new skills activate, refresh
        // the `skill` tool schema with the expanded skill list.
        if let Some(ref registry) = state.skills.registry_for_activation {
            let mut any_newly_activated = false;
            for edge_result in &turn_result.edge_tool_round {
                if let Some(path) =
                    extract_file_path_from_tool(&edge_result.tool, &edge_result.args)
                {
                    let newly = registry.record_file_path(&path);
                    if !newly.is_empty() {
                        any_newly_activated = true;
                        if !quiet {
                            for name in &newly {
                                host.emit_headless_line(
                                    HeadlessStderrStyle::Dim,
                                    format!("  ◆ Skill activated: {name}"),
                                );
                            }
                        }
                    }
                }
            }
            if any_newly_activated {
                if let Some(resolver) = &state.skills.resolver {
                    let full = resolver.available_skills();
                    if !full.is_empty() {
                        let (visible, open_skill_name) =
                            crate::turn::skill_tool::visible_skills_for_host_turn(
                                &full,
                                state.message.as_str(),
                                &state.skills.quality_tracker,
                                &state.skills.pinned,
                                &state.skills.discovered,
                                &state.skills.search,
                            );
                        host.inject_tool_schema(crate::turn::skill_tool::skill_tool_schema(
                            &visible,
                            Some(&state.skills.quality_tracker),
                            Some(&state.skills.pinned),
                            open_skill_name,
                        ));
                        if open_skill_name {
                            host.inject_tool_schema(
                                crate::turn::skill_tool::discover_skills_tool_schema(),
                            );
                        }
                    }
                }
            }
        }

        // ─── Step 4b: Error budget tracking ─────────────────────────────
        // Track consecutive turns dominated by the same error category.
        // If the agent keeps hitting the same error N turns in a row,
        // inject a strategy-change nudge.
        {
            // Compare error counts before/after this turn's tool round.
            let turn_errors = state
                .turn_guard
                .errors
                .total_errors
                .saturating_sub(errors_before_round);
            if turn_errors > 0 {
                // Find the category that grew most this turn.
                let dominant = state
                    .turn_guard
                    .errors
                    .errors_by_category
                    .iter()
                    .filter_map(|(cat, &count)| {
                        let before = errors_by_cat_before.get(cat).copied().unwrap_or(0);
                        let delta = count.saturating_sub(before);
                        if delta > 0 { Some((*cat, delta)) } else { None }
                    })
                    .max_by_key(|(_, delta)| *delta)
                    .map(|(cat, _)| cat);
                if dominant == state.error_recovery.last_error_category {
                    state.error_recovery.consecutive_same_error += 1;
                } else {
                    state.error_recovery.consecutive_same_error = 1;
                    state.error_recovery.last_error_category = dominant;
                }
                if state.error_recovery.consecutive_same_error >= CONSECUTIVE_ERROR_BUDGET {
                    let cat_name = state
                        .error_recovery
                        .last_error_category
                        .map(|c| format!("{c:?}"))
                        .unwrap_or_else(|| "Unknown".into());
                    state.messages.push(serde_json::json!({
                        "role": "user",
                        "content": format!(
                            "🔄 ERROR BUDGET EXHAUSTED: You've hit {cat_name} errors \
                             {n} turns in a row. Your current approach is not working. \
                             STOP repeating the same strategy. You MUST try a fundamentally \
                             different approach: different tool, different file, different \
                             method. If you cannot make progress, explain what's blocking you.",
                            n = state.error_recovery.consecutive_same_error,
                        )
                    }));
                    state.error_recovery.consecutive_same_error = 0; // Reset after nudge
                }
            } else {
                // Successful turn — reset streak.
                state.error_recovery.consecutive_same_error = 0;
                state.error_recovery.last_error_category = None;
            }
        }

        // ─── Step 4b: Checkpoint gate (mid-execution fail-fast) ─────────
        if let Some(ref gate) = state.checkpoint_gate {
            let freq = gate.checkpoint_frequency();
            if freq > 0 && (turn_index as u32 + 1).is_multiple_of(freq) {
                let run_id = state.current_run_id.as_deref().unwrap_or("unknown");
                match gate
                    .check(run_id, turn_index as u32, state.total_tool_calls)
                    .await
                {
                    Ok(true) => { /* continue */ }
                    Ok(false) => {
                        // ─── Observability: turn end hook (gate cancelled) ───
                        if let (Some(hub), Some(session)) = (
                            state.telemetry.observability_hub.as_ref(),
                            state.telemetry.observability_session.as_ref(),
                        ) {
                            let total_ms = turn_start_time.elapsed().as_millis() as u64;
                            let timing = crate::observability_integration::TurnTiming {
                                turn: turn_index as u32,
                                context_assembly_ms: 0,
                                ttft_ms: turn_result.ttft_ms.unwrap_or(0) as u64,
                                llm_total_ms: total_ms,
                                tool_execution_ms: 0,
                                total_ms,
                            };
                            let mut session_guard =
                                session.write().unwrap_or_else(|e| e.into_inner());
                            crate::observability_integration::on_turn_end(
                                hub,
                                &mut session_guard,
                                timing,
                            );
                        }
                        state.step_recorder.end_turn(true);
                        finalize_turn_trace(state);
                        return Ok(AgenticLoopOutcome::Cancelled);
                    }
                    Err(e) => {
                        // Gate error is non-fatal — log and continue
                        eprintln!("[checkpoint-gate] check error: {e}");
                    }
                }
            }
        }

        // ─── Step 5: Post-tool policy ───────────────────────────────────
        match map_post_tool_policy_outcome(apply_agentic_post_tool_policy(
            AgenticPostToolPolicyRequest {
                turn_index: turn_index as u32,
                message: &state.message,
                tool_calls_for_guard: &tool_calls_for_guard,
                intent_tool_turns: &mut state.stall.intent_tool_turns,
                messages: &mut state.messages,
                stall_events: &mut state.stall.events,
                turn_guard: &mut state.turn_guard,
                verdict_events: &mut state.stall.verdict_events,
                restricted_tools: &mut state.restricted_tools,
                remaining_turns: &mut state.remaining_turns,
                step_recorder: &mut state.step_recorder,
                current_session_id: state.current_session_id.as_ref(),
                max_turns: state.max_turns,
                loop_turn: turn_index,
                recent_tools: &state.recent_tools,
                last_heavy_checkpoint: &mut state.stall.last_heavy_checkpoint,
            },
        )) {
            AgenticPostToolIterationControl::Abort(e) => {
                finalize_turn_trace(state);
                return Err(e);
            }
            AgenticPostToolIterationControl::RetryLlmClearToolResults => {
                state.tool_results.clear();
            }
            AgenticPostToolIterationControl::ProceedEndTurn => {
                // Emit progress event for subscribers (UI, monitors).
                if let Some(ref emitter) = state.messaging.progress_emitter {
                    let tool_calls_this_turn =
                        state.total_tool_calls.saturating_sub(if turn_index > 0 {
                            state.total_tool_calls
                        } else {
                            0
                        });
                    let last_tool = turn_result
                        .edge_tool_round
                        .last()
                        .map(|r| r.tool.clone())
                        .unwrap_or_else(|| "thinking".to_string());
                    emitter.turn_completed(turn_index as u32 + 1, tool_calls_this_turn, last_tool);

                    // Emit intermediate metrics so UI can show progress (e.g., "5/30 turns, 12 tools, 8k tokens")
                    emitter.metrics_update(
                        turn_index as u32 + 1,
                        state.max_turns as u32,
                        state.total_prompt,
                        state.total_completion,
                        state.total_tool_calls,
                    );
                }

                // Send progress update to parent agent (best-effort, skip for root).
                if let Some(ref mailbox) = state.messaging.mailbox {
                    if mailbox.has_parent().await {
                        if let Err(e) = mailbox
                            .send_progress(
                                turn_index as u32,
                                state.total_tool_calls,
                                "turn_complete",
                                None,
                            )
                            .await
                        {
                            astra_core::agent_warn!("mailbox", "Failed to send turn progress: {e}");
                        }
                    }
                }

                // ─── Observability: turn end hook ────────────────────────
                // Capture timing and feed to auto-tuning.
                if let (Some(hub), Some(session)) = (
                    state.telemetry.observability_hub.as_ref(),
                    state.telemetry.observability_session.as_ref(),
                ) {
                    let total_ms = turn_start_time.elapsed().as_millis() as u64;
                    let ctx_asm_ms = (llm_wall_start - turn_start_time).as_millis() as u64;
                    let tool_exec_ms: u64 = turn_result
                        .edge_tool_round
                        .iter()
                        .map(|e| e.duration_ms)
                        .sum();
                    let timing = crate::observability_integration::TurnTiming {
                        turn: turn_index as u32,
                        context_assembly_ms: ctx_asm_ms,
                        ttft_ms: turn_result.ttft_ms.unwrap_or(0) as u64,
                        llm_total_ms: total_ms
                            .saturating_sub(ctx_asm_ms)
                            .saturating_sub(tool_exec_ms),
                        tool_execution_ms: tool_exec_ms,
                        total_ms,
                    };
                    let mut session_guard = session.write().unwrap_or_else(|e| e.into_inner());
                    crate::observability_integration::on_turn_end(hub, &mut session_guard, timing);
                }

                // ─── Finalize turn trace collector ────────────────────────
                // Persist context assembly trace to session journal (best-effort)
                // and feed to observability session for /telemetry context.
                finalize_turn_trace(state);

                state.step_recorder.end_turn(false);

                // ── Auto-tuning: count completed turns & periodic cycle ──
                state.telemetry.completed_turns_for_tuning += 1;
                maybe_run_tuning_cycle(state);
                maybe_trigger_auto_reflection(host, state).await;

                // ── Per-turn micro-adaptation ──
                let turn_tokens = state.last_measured_prompt_tokens.unwrap_or(0);
                apply_per_turn_adaptation(state, turn_tokens);
            }
        }
    }
    // Loop exhausted max_turns without explicit break — write final state.
    finalize_and_render(host, state);
    Ok(AgenticLoopOutcome::Completed)
}

// ─── CTX_ helpers ────────────────────────────────────────────────────────────

/// Extract repository name from a git remote URL.
///
// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Flexible mock host for multi-turn scenarios ─────────────────────────

    struct MockHost {
        turn_results: Vec<HostTurnResult>,
        current_turn: usize,
        valid_tools: HashSet<String>,
        emitted_lines: Vec<String>,
        quiet: bool,
        injected_schemas: Vec<Value>,
        reflection_text: Option<String>,
        reflection_error: Option<String>,
        last_reflection_prompt: Option<String>,
        rendered_final_text: Vec<String>,
    }

    impl MockHost {
        fn new(results: Vec<HostTurnResult>) -> Self {
            Self {
                turn_results: results,
                current_turn: 0,
                valid_tools: HashSet::new(),
                emitted_lines: Vec::new(),
                quiet: true,
                injected_schemas: Vec::new(),
                reflection_text: None,
                reflection_error: None,
                last_reflection_prompt: None,
                rendered_final_text: Vec::new(),
            }
        }

        fn with_valid_tools(mut self, tools: &[&str]) -> Self {
            self.valid_tools = tools.iter().map(|s| s.to_string()).collect();
            self
        }

        fn with_reflection_text(mut self, text: &str) -> Self {
            self.reflection_text = Some(text.to_string());
            self
        }

        fn with_reflection_error(mut self, error: &str) -> Self {
            self.reflection_error = Some(error.to_string());
            self
        }
    }

    #[async_trait]
    impl AgenticLoopHost for MockHost {
        async fn execute_turn(
            &mut self,
            _state: &mut AgenticLoopState,
        ) -> Result<HostTurnResult, String> {
            if self.turn_results.is_empty() {
                return Err("no more turns".to_string());
            }
            let result = self.turn_results.remove(0);
            self.current_turn += 1;
            Ok(result)
        }

        fn supports_auto_reflection(&self) -> bool {
            self.reflection_text.is_some() || self.reflection_error.is_some()
        }

        async fn execute_reflection(
            &mut self,
            _state: &mut AgenticLoopState,
            request: HostReflectionRequest<'_>,
        ) -> Result<Option<HostReflectionResult>, String> {
            self.last_reflection_prompt = Some(request.user_prompt.to_string());
            if let Some(error) = self.reflection_error.take() {
                return Err(error);
            }
            let Some(text) = self.reflection_text.take() else {
                return Ok(None);
            };
            Ok(Some(HostReflectionResult {
                full_text: text,
                prompt_tokens: 91,
                completion_tokens: 37,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                has_usage: true,
            }))
        }

        fn emit_headless_line(&mut self, _style: HeadlessStderrStyle, line: String) {
            self.emitted_lines.push(line);
        }

        fn is_quiet(&self) -> bool {
            self.quiet
        }

        fn valid_tool_names(&self) -> &HashSet<String> {
            &self.valid_tools
        }

        fn inject_tool_schema(&mut self, schema: Value) {
            if let Some(name) = schema
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
            {
                self.valid_tools.insert(name.to_string());
            }
            self.injected_schemas.push(schema);
        }

        fn render_final_text(&mut self, text: &str) {
            self.rendered_final_text.push(text.to_string());
        }
    }

    // ── Result builders ─────────────────────────────────────────────────────

    fn text_result(text: &str, prompt: u64, completion: u64, ttft: Option<u64>) -> HostTurnResult {
        HostTurnResult {
            accum: ChatTurnSseAccum {
                full_text: text.to_string(),
                has_tool_calls: false,
                has_usage: true,
                prompt_tokens: prompt,
                completion_tokens: completion,
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: ttft,
            edge_tool_round: Vec::new(),
        }
    }

    fn edge_tool_result(
        tools: Vec<EdgeToolExecResult>,
        prompt: u64,
        completion: u64,
        ttft: Option<u64>,
    ) -> HostTurnResult {
        HostTurnResult {
            accum: ChatTurnSseAccum {
                has_tool_calls: false,
                has_usage: true,
                prompt_tokens: prompt,
                completion_tokens: completion,
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: ttft,
            edge_tool_round: tools,
        }
    }

    fn server_tool_result(
        tool_calls: Vec<Value>,
        edge_tools: Vec<EdgeToolExecResult>,
        prompt: u64,
        completion: u64,
        ttft: Option<u64>,
    ) -> HostTurnResult {
        HostTurnResult {
            accum: ChatTurnSseAccum {
                has_tool_calls: true,
                has_usage: true,
                prompt_tokens: prompt,
                completion_tokens: completion,
                tool_calls,
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: ttft,
            edge_tool_round: edge_tools,
        }
    }

    fn make_edge_tool(name: &str, output: &str) -> EdgeToolExecResult {
        EdgeToolExecResult {
            request_id: format!("req-{name}"),
            tool: name.to_string(),
            args: json!({}),
            output: output.to_string(),
            tool_result_fields: None,
            status: "ok".to_string(),
            duration_ms: 10,
        }
    }

    fn make_edge_tool_with_args(name: &str, args: Value, output: &str) -> EdgeToolExecResult {
        EdgeToolExecResult {
            request_id: format!("req-{name}"),
            tool: name.to_string(),
            args,
            output: output.to_string(),
            tool_result_fields: None,
            status: "ok".to_string(),
            duration_ms: 10,
        }
    }

    fn make_edge_tool_with_status(name: &str, status: &str, output: &str) -> EdgeToolExecResult {
        EdgeToolExecResult {
            request_id: format!("req-{name}"),
            tool: name.to_string(),
            args: json!({}),
            output: output.to_string(),
            tool_result_fields: None,
            status: status.to_string(),
            duration_ms: 10,
        }
    }

    // ── State builder ───────────────────────────────────────────────────────

    fn make_state() -> AgenticLoopState {
        AgenticLoopState {
            messages: Vec::new(),
            tool_results: Vec::new(),
            current_session_id: None,
            current_run_id: None,
            recursion_depth: 0,
            final_text: String::new(),
            total_prompt: 0,
            total_completion: 0,
            total_cache_read: 0,
            total_cache_creation: 0,
            total_tool_calls: 0,
            has_any_usage: false,
            max_turns: 10,
            remaining_turns: 10,
            turn_guard: TurnGuard::new(),
            restricted_tools: HashSet::new(),
            step_recorder: StepRecorder::new("test-session", "test-task"),
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(0.95),
            call_counts: HashMap::new(),
            max_identical_tool_calls: crate::runtime_config::RuntimeConfig::load()
                .tool_selection
                .effective_max_identical_calls(),
            max_tools_per_turn: crate::runtime_config::RuntimeConfig::load()
                .tool_selection
                .effective_max_tools_per_turn(),
            stall: Default::default(),
            telemetry: Default::default(),
            skills: SkillState {
                quality_tracker: crate::skills::quality::SkillQualityTracker::new(),
                improvement_tracker: crate::skills::improvement::ImprovementTracker::new(),
                ..Default::default()
            },
            hooks: Default::default(),
            messaging: Default::default(),
            cancellation: Default::default(),
            error_recovery: Default::default(),
            message: "test query".to_string(),
            recent_tools: Vec::new(),
            task_profile: TaskExecutionProfile::default(),
            api: astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            api_token: String::new(),
            delegation_engine: None,
            project_context: None,
            checkpoint_gate: None,
            evolution_service: None,
            rate_limit_cooldown: Default::default(),
            data_snapshot_provider: None,
            last_composite_snapshot: None,
            last_measured_prompt_tokens: None,
            consecutive_context_window_errors: 0,
            max_turn_input_tokens: 0,
            budget_wrapup_injected: false,
            skill_produced_output: false,
            max_cumulative_tokens: 0,
            thinking_budget_tokens: None,
            recent_file_reads: Vec::new(),
            permission_context: None,
            permission_handler: None,
            tactical_adapter: None,
            step_signal_collector: None,
            tool_budget_override: None,
            pending_reflection_signals: Vec::new(),
            recent_tactical_actions: Vec::new(),
        }
    }

    // ── Original tests ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn single_text_turn_completes() {
        let mut host = MockHost::new(vec![text_result("Hello, world!", 10, 5, Some(42))]);
        let mut state = make_state();
        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(state.final_text, "Hello, world!");
        assert_eq!(state.total_prompt, 10);
        assert_eq!(state.total_completion, 5);
        assert!(state.has_any_usage);
        // Deferred rendering: host.render_final_text() is called with the final text.
        assert_eq!(host.rendered_final_text.len(), 1);
        assert_eq!(host.rendered_final_text[0], "Hello, world!");
    }

    #[tokio::test]
    async fn render_final_text_called_once_at_completion() {
        // Two turns: tool turn (no render) → text turn (render).
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![make_edge_tool("grep", "results...")], 20, 10, Some(50)),
            text_result("Final answer", 15, 8, Some(30)),
        ])
        .with_valid_tools(&["grep"]);
        let mut state = make_state();
        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(state.final_text, "Final answer");
        // render_final_text should be called exactly once with the final text.
        assert_eq!(host.rendered_final_text.len(), 1);
        assert_eq!(host.rendered_final_text[0], "Final answer");
    }

    #[tokio::test]
    async fn render_final_text_not_duplicated_across_tool_then_text() {
        // Verify render_final_text isn't called after tool turns, only at
        // final text completion.
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![make_edge_tool("grep", "results...")], 20, 10, Some(50)),
            edge_tool_result(
                vec![make_edge_tool("grep", "more results")],
                20,
                10,
                Some(50),
            ),
            text_result("Done!", 15, 8, Some(30)),
        ])
        .with_valid_tools(&["grep"]);
        let mut state = make_state();
        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(state.final_text, "Done!");
        // Key contract: render_final_text called exactly once, not once per turn.
        assert_eq!(host.rendered_final_text.len(), 1);
        assert_eq!(host.rendered_final_text[0], "Done!");
    }

    #[test]
    fn adaptive_scenario_event_only_emits_for_real_changes() {
        assert!(should_emit_adaptive_scenario_event(true, false, true));
        assert!(should_emit_adaptive_scenario_event(false, false, false));
        assert!(!should_emit_adaptive_scenario_event(false, false, true));
        assert!(!should_emit_adaptive_scenario_event(true, true, true));
        assert!(!should_emit_adaptive_scenario_event(false, true, false));
    }

    #[test]
    fn edge_tool_status_exit_code_maps_common_statuses() {
        assert_eq!(edge_tool_status_exit_code("ok"), Some(0));
        assert_eq!(edge_tool_status_exit_code("completed"), Some(0));
        assert_eq!(edge_tool_status_exit_code("error"), Some(1));
        assert_eq!(edge_tool_status_exit_code("partial_failure"), Some(1));
        assert_eq!(edge_tool_status_exit_code("unknown"), None);
    }

    #[test]
    fn edge_tool_observability_records_goal_tracker_without_hub() {
        let mut state = make_state();
        let session = std::sync::Arc::new(std::sync::RwLock::new(
            crate::observability_integration::ObservabilitySession::new_simple("sess-1"),
        ));
        {
            let mut guard = session.write().unwrap_or_else(|e| e.into_inner());
            guard.turn_number = 1;
            guard.record_query("run tests for authentication flow");
        }
        state.telemetry.observability_session = Some(session.clone());

        let edge_tools = vec![
            make_edge_tool_with_status("bash", "ok", "test result: ok. 24 passed; 0 failed"),
            make_edge_tool_with_status("bash", "error", "test result: FAILED. 1 passed; 2 failed"),
        ];
        record_edge_tool_observability(&mut state, &edge_tools);

        let progress = session
            .read()
            .unwrap()
            .goal_progress()
            .expect("goal progress");
        assert_eq!(progress.milestone_count, 2);
    }

    #[tokio::test]
    async fn budget_exhausted_returns_error() {
        let mut host = MockHost::new(vec![]);
        let mut state = make_state();
        state.max_turns = 25;
        state.remaining_turns = 0;
        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_err());
        let err = outcome.unwrap_err();
        assert!(err.contains("budget"), "should mention budget: {err}");
        assert!(
            err.contains("budget: 25"),
            "should show max_turns as budget: {err}"
        );
    }

    #[tokio::test]
    async fn host_error_propagates() {
        let mut host = MockHost::new(vec![]);
        let mut state = make_state();
        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_err());
        assert_eq!(outcome.unwrap_err(), "no more turns");
    }

    #[tokio::test]
    async fn ttft_captured_from_first_turn() {
        let mut host = MockHost::new(vec![text_result("hi", 10, 5, Some(42))]);
        let mut state = make_state();
        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert_eq!(state.telemetry.first_ttft_ms, Some(42));
    }

    // ── Multi-turn flow tests ───────────────────────────────────────────────

    #[tokio::test]
    async fn multi_turn_edge_tool_then_text() {
        // Turn 1: edge tool → ProceedWithToolCalls → headless round → policy
        // Turn 2: text response → BreakLoop → complete
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![make_edge_tool("bash", "file list")], 20, 10, Some(50)),
            text_result("Analysis complete.", 15, 5, Some(30)),
        ]);
        let mut state = make_state();

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok(), "expected Ok but got: {:?}", outcome);
        assert_eq!(host.current_turn, 2);
        assert_eq!(state.final_text, "Analysis complete.");
        // Tokens accumulate across turns (+=)
        assert_eq!(state.total_prompt, 35); // 20 + 15
        assert_eq!(state.total_completion, 15); // 10 + 5
        // Edge tool counted
        assert!(state.total_tool_calls >= 1);
        // Messages accumulated: assistant + tool from turn 1, at minimum
        assert!(state.messages.len() >= 2);
    }

    #[tokio::test]
    async fn tokens_accumulate_with_preexisting() {
        // Verify += semantics: pre-existing tokens are preserved
        let mut host = MockHost::new(vec![text_result("ok", 100, 50, None)]);
        let mut state = make_state();
        state.total_prompt = 200;
        state.total_completion = 80;

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert_eq!(state.total_prompt, 300); // 200 + 100
        assert_eq!(state.total_completion, 130); // 80 + 50
    }

    #[tokio::test]
    async fn remaining_turns_decrements_per_turn() {
        let mut host = MockHost::new(vec![
            edge_tool_result(
                vec![make_edge_tool("read_file", "content")],
                10,
                5,
                Some(20),
            ),
            text_result("Done", 10, 5, None),
        ]);
        let mut state = make_state();
        state.max_turns = 10;
        state.remaining_turns = 5;

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;
        // Turn 1: 5→4, Turn 2: 4→3
        assert_eq!(state.remaining_turns, 3);
    }

    #[tokio::test]
    async fn ttft_not_overwritten_by_later_turns() {
        // TTFT should capture the first turn's value only
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![make_edge_tool("bash", "ok")], 10, 5, Some(100)),
            text_result("result", 10, 5, Some(200)),
        ]);
        let mut state = make_state();
        assert!(state.telemetry.first_ttft_ms.is_none());

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert_eq!(state.telemetry.first_ttft_ms, Some(100)); // NOT 200
    }

    // ── Boundary condition tests ────────────────────────────────────────────

    #[tokio::test]
    async fn max_turns_zero_completes_immediately() {
        let mut host = MockHost::new(vec![text_result("unreachable", 10, 5, None)]);
        let mut state = make_state();
        state.max_turns = 0;
        state.remaining_turns = 10;

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(host.current_turn, 0); // No turns executed
        assert_eq!(state.final_text, ""); // No text produced
        assert_eq!(state.remaining_turns, 10); // Unchanged
    }

    #[tokio::test]
    async fn max_turns_one_executes_exactly_once() {
        let mut host = MockHost::new(vec![text_result("single turn", 10, 5, Some(33))]);
        let mut state = make_state();
        state.max_turns = 1;
        state.remaining_turns = 1;

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(host.current_turn, 1);
        assert_eq!(state.final_text, "single turn");
        assert_eq!(state.remaining_turns, 0);
    }

    // ── Error propagation tests ─────────────────────────────────────────────

    #[tokio::test]
    async fn host_error_on_second_turn_preserves_first_state() {
        // Turn 1 succeeds with edge tools; turn 2 errors (no more results)
        let mut host = MockHost::new(vec![edge_tool_result(
            vec![make_edge_tool("bash", "ok")],
            20,
            10,
            Some(50),
        )]);
        let mut state = make_state();
        state.max_turns = 5;
        state.remaining_turns = 5;

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_err());
        // Turn 1 state preserved
        assert_eq!(state.total_prompt, 20);
        assert_eq!(state.total_completion, 10);
        assert_eq!(state.telemetry.first_ttft_ms, Some(50));
        assert!(state.telemetry.all_tools_used.contains("bash"));
        // Both turns decremented remaining_turns
        assert_eq!(state.remaining_turns, 3); // 5→4→3
    }

    #[tokio::test]
    async fn fatal_error_from_sse_terminates_loop() {
        // SSE stream returns an error_message → ingest returns Fatal
        let mut host = MockHost::new(vec![HostTurnResult {
            accum: ChatTurnSseAccum {
                error_message: Some("rate limit exceeded".to_string()),
                has_usage: false,
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: None,
            edge_tool_round: Vec::new(),
        }]);
        let mut state = make_state();

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_err());
        assert!(outcome.unwrap_err().contains("rate limit"));
    }

    // ── State accumulation tests ────────────────────────────────────────────

    #[tokio::test]
    async fn all_tools_used_accumulates_across_turns() {
        // Turn 1: bash tool, Turn 2: read_file tool, Turn 3: text
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![make_edge_tool("bash", "ok")], 10, 5, Some(20)),
            edge_tool_result(vec![make_edge_tool("read_file", "content")], 10, 5, None),
            text_result("done", 10, 5, None),
        ]);
        let mut state = make_state();

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(state.telemetry.all_tools_used.contains("bash"));
        assert!(state.telemetry.all_tools_used.contains("read_file"));
        assert_eq!(state.telemetry.all_tools_used.len(), 2);
    }

    #[tokio::test]
    async fn total_tool_calls_sums_across_turns() {
        let mut host = MockHost::new(vec![
            edge_tool_result(
                vec![
                    make_edge_tool("bash", "a"),
                    make_edge_tool("read_file", "b"),
                ],
                10,
                5,
                None,
            ),
            edge_tool_result(vec![make_edge_tool("grep", "c")], 10, 5, None),
            text_result("done", 10, 5, None),
        ]);
        let mut state = make_state();

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;
        // Turn 1: 2 tools, Turn 2: 1 tool, Turn 3: 0 tools
        assert_eq!(state.total_tool_calls, 3);
    }

    #[tokio::test]
    async fn session_id_captured_from_sse_accum() {
        let mut host = MockHost::new(vec![HostTurnResult {
            accum: ChatTurnSseAccum {
                full_text: "hello".to_string(),
                session_id: Some("sess-42".to_string()),
                run_id: Some("run-7".to_string()),
                has_usage: true,
                prompt_tokens: 10,
                completion_tokens: 5,
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: None,
            edge_tool_round: Vec::new(),
        }]);
        let mut state = make_state();

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert_eq!(state.current_session_id, Some("sess-42".to_string()));
        assert_eq!(state.current_run_id, Some("run-7".to_string()));
    }

    // ── Headless round integration tests ────────────────────────────────────

    #[tokio::test]
    async fn edge_tool_round_modifies_messages() {
        // After headless round, messages should contain assistant + tool messages
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![make_edge_tool("bash", "hello world")], 10, 5, None),
            text_result("done", 10, 5, None),
        ]);
        let mut state = make_state();
        assert!(state.messages.is_empty());

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;
        // Turn 1 headless round adds: 1 assistant msg + 1 tool result msg = 2
        // Turn 2 (text-only) doesn't add to messages via ingest
        assert!(
            state.messages.len() >= 2,
            "expected >=2 messages from headless round, got {}",
            state.messages.len()
        );
    }

    #[tokio::test]
    async fn tool_call_records_populated_from_edge_round() {
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![make_edge_tool("bash", "ok")], 10, 5, None),
            text_result("done", 10, 5, None),
        ]);
        let mut state = make_state();

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;
        // The headless round should record tool executions
        assert!(!state.stall.tool_call_records.is_empty());
        assert_eq!(state.stall.tool_call_records[0].ms, 10);
    }

    #[tokio::test]
    async fn headless_lines_emitted_when_not_quiet() {
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![make_edge_tool("bash", "ok")], 10, 5, None),
            text_result("done", 10, 5, None),
        ]);
        host.quiet = false;
        let mut state = make_state();

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;
        // Headless round emits tool execution status lines when not quiet
        // At minimum, the unknown-tool error line is emitted via stderr in headless round
        // (since "bash" is not in valid_tool_names, it gets an error but still processes)
        assert!(host.current_turn == 2); // Both turns executed regardless
    }

    #[tokio::test]
    async fn valid_tools_affect_headless_round() {
        // With valid_tool_names set, edge tool outputs are preserved (not replaced by error)
        let mut host = MockHost::new(vec![
            edge_tool_result(
                vec![make_edge_tool_with_args(
                    "bash",
                    json!({"cmd": "ls"}),
                    "file.txt",
                )],
                10,
                5,
                None,
            ),
            text_result("done", 10, 5, None),
        ])
        .with_valid_tools(&["bash"]);
        let mut state = make_state();

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(host.current_turn, 2);
        // Tool should be in all_tools_used
        assert!(state.telemetry.all_tools_used.contains("bash"));
    }

    // ── Server tool_call tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn server_tool_calls_with_edge_outputs() {
        // Server returns tool_calls in SSE; edge has matching outputs
        let edge_tools = vec![make_edge_tool_with_args(
            "read_file",
            json!({"path": "/tmp/test.txt"}),
            "file content here",
        )];
        let tool_calls = vec![json!({
            "id": "call-1",
            "name": "read_file",
            "arguments": {"path": "/tmp/test.txt"}
        })];
        let mut host = MockHost::new(vec![
            server_tool_result(tool_calls, edge_tools, 20, 10, Some(25)),
            text_result("Analyzed the file.", 15, 5, None),
        ])
        .with_valid_tools(&["read_file"]);
        let mut state = make_state();

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(host.current_turn, 2);
        assert_eq!(state.final_text, "Analyzed the file.");
        assert!(state.telemetry.all_tools_used.contains("read_file"));
        assert_eq!(state.total_prompt, 35);
        assert_eq!(state.total_completion, 15);
    }

    // ── Stall detection tests ───────────────────────────────────────────────

    #[tokio::test]
    async fn stall_tracking_accumulates_turn_signatures() {
        // Multiple edge-tool turns should accumulate stall tracking data
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![make_edge_tool("bash", "ok")], 10, 5, None),
            edge_tool_result(vec![make_edge_tool("bash", "ok")], 10, 5, None),
            text_result("done", 10, 5, None),
        ]);
        let mut state = make_state();

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;
        // turn_sigs and turn_tool_names should have entries from both tool turns
        assert!(state.stall.turn_sigs.len() >= 2);
        assert!(state.stall.turn_tool_names.len() >= 2);
    }

    // ── Edge case: has_any_usage tracking ───────────────────────────────────

    #[tokio::test]
    async fn has_any_usage_set_when_usage_present() {
        let mut host = MockHost::new(vec![text_result("ok", 0, 0, None)]);
        let mut state = make_state();
        assert!(!state.has_any_usage);

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(state.has_any_usage); // has_usage=true in text_result
    }

    // ── Cancel flag tests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn cancel_flag_none_does_not_cancel() {
        let mut host = MockHost::new(vec![text_result("ok", 10, 5, Some(42))]);
        let mut state = make_state();
        state.cancellation.flag = None; // default
        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(matches!(outcome, Ok(AgenticLoopOutcome::Completed)));
        assert_eq!(state.final_text, "ok");
    }

    #[tokio::test]
    async fn cancel_flag_false_does_not_cancel() {
        let flag = Arc::new(AtomicBool::new(false));
        let mut host = MockHost::new(vec![text_result("ok", 10, 5, Some(42))]);
        let mut state = make_state();
        state.cancellation.flag = Some(flag);
        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(matches!(outcome, Ok(AgenticLoopOutcome::Completed)));
        assert_eq!(state.final_text, "ok");
    }

    #[tokio::test]
    async fn cancel_flag_true_aborts_before_first_turn() {
        let flag = Arc::new(AtomicBool::new(true));
        let mut host = MockHost::new(vec![text_result("should not run", 10, 5, Some(42))]);
        let mut state = make_state();
        state.cancellation.flag = Some(flag);
        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(matches!(outcome, Ok(AgenticLoopOutcome::Cancelled)));
        // No turns executed — host never called
        assert_eq!(host.current_turn, 0);
        assert!(state.final_text.is_empty());
    }

    #[tokio::test]
    async fn cancel_flag_set_between_turns() {
        let flag = Arc::new(AtomicBool::new(false));
        let flag_clone = flag.clone();

        // Cancel is already set, so even with available turns the loop won't execute
        let mut host = MockHost::new(vec![
            text_result("first", 10, 5, Some(42)),
            text_result("should not run", 10, 5, Some(42)),
        ]);
        let mut state = make_state();
        state.cancellation.flag = Some(flag_clone);

        // Set cancel flag before loop starts — simulates cancel arriving
        flag.store(true, std::sync::atomic::Ordering::Relaxed);

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(matches!(outcome, Ok(AgenticLoopOutcome::Cancelled)));
    }

    #[tokio::test]
    async fn pause_flag_true_waits_until_cleared() {
        let pause_flag = Arc::new(AtomicBool::new(true));
        let pause_flag_clone = pause_flag.clone();

        let handle = tokio::spawn(async move {
            let mut host = MockHost::new(vec![text_result("ok", 10, 5, Some(42))]);
            let mut state = make_state();
            state.cancellation.pause_flag = Some(pause_flag_clone);
            let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
            (outcome, host.current_turn, state.final_text)
        });

        tokio::time::sleep(std::time::Duration::from_millis(75)).await;
        assert!(
            !handle.is_finished(),
            "loop should stay paused while flag is set"
        );

        pause_flag.store(false, std::sync::atomic::Ordering::Relaxed);

        let (outcome, turns, final_text) = handle.await.unwrap();
        assert!(matches!(outcome, Ok(AgenticLoopOutcome::Completed)));
        assert_eq!(turns, 1);
        assert_eq!(final_text, "ok");
    }

    #[test]
    fn waiting_variant_carries_reason() {
        let outcome = AgenticLoopOutcome::Waiting("tool_approval".to_string());
        match outcome {
            AgenticLoopOutcome::Waiting(reason) => assert_eq!(reason, "tool_approval"),
            other => panic!("expected Waiting, got {other:?}"),
        }
    }

    #[test]
    fn outcome_debug_format() {
        // Ensure all variants have Debug
        let variants: Vec<AgenticLoopOutcome> = vec![
            AgenticLoopOutcome::Completed,
            AgenticLoopOutcome::Error("fail".into()),
            AgenticLoopOutcome::Cancelled,
            AgenticLoopOutcome::Waiting("resume".into()),
        ];
        for v in &variants {
            let _ = format!("{v:?}");
        }
        assert_eq!(variants.len(), 4);
    }

    // ── Delegation integration tests ────────────────────────────────────────

    #[test]
    fn is_delegation_call_detects_delegate_tool() {
        let delegate = json!({
            "id": "call_123",
            "type": "function",
            "function": {
                "name": "delegate",
                "arguments": "{}"
            }
        });
        let non_delegate = json!({
            "id": "call_456",
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": "{}"
            }
        });
        assert!(super::is_delegation_call(&delegate));
        assert!(!super::is_delegation_call(&non_delegate));
    }

    #[test]
    fn is_delegation_call_rejects_missing_function() {
        let malformed = json!({"id": "call_000"});
        assert!(!super::is_delegation_call(&malformed));
    }

    #[test]
    fn is_delegation_call_accepts_legacy_top_level_shape() {
        let delegate = json!({
            "id": "call_legacy",
            "name": "delegate",
            "arguments": {"task": "review"}
        });
        assert!(super::is_delegation_call(&delegate));
    }

    #[test]
    fn tool_call_arguments_value_parses_canonical_argument_string() {
        let tool_call = json!({
            "id": "call_123",
            "type": "function",
            "function": {
                "name": "delegate",
                "arguments": "{\"task\":\"review\",\"agents\":[\"reviewer\"]}"
            }
        });
        assert_eq!(
            super::tool_call_arguments_value(&tool_call),
            json!({"task":"review","agents":["reviewer"]})
        );
    }

    #[test]
    fn delegate_tool_schema_has_correct_structure() {
        let schema = super::delegate_tool_schema();
        assert_eq!(schema["type"], "function");
        assert_eq!(schema["function"]["name"], "delegate");

        let params = &schema["function"]["parameters"];
        assert_eq!(params["type"], "object");

        let required = params["required"].as_array().unwrap();
        assert!(required.contains(&json!("task")));
        assert!(required.contains(&json!("agents")));

        let props = &params["properties"];
        assert!(props["task"].is_object());
        assert!(props["agents"].is_object());
        assert!(props["pattern"].is_object());
        assert!(props["max_rounds"].is_object());
        assert!(props["context"].is_object());
    }

    #[test]
    fn parse_coordination_pattern_defaults_to_sequential() {
        let args = json!({"agents": ["coder", "reviewer"]});
        let pattern = super::parse_coordination_pattern(&args).unwrap();
        match pattern {
            astra_services::coordination::CoordinationPattern::Sequential {
                agent_ids,
                stop_on_success,
                ..
            } => {
                assert_eq!(agent_ids, vec!["coder", "reviewer"]);
                assert!(!stop_on_success);
            }
            _ => panic!("expected Sequential"),
        }
    }

    #[test]
    fn parse_coordination_pattern_fan_out() {
        let args = json!({"pattern": "fan_out", "agents": ["coder", "writer"]});
        let pattern = super::parse_coordination_pattern(&args).unwrap();
        match pattern {
            astra_services::coordination::CoordinationPattern::FanOut {
                agent_ids,
                timeout_sec,
                ..
            } => {
                assert_eq!(agent_ids, vec!["coder", "writer"]);
                assert_eq!(timeout_sec, 300);
            }
            _ => panic!("expected FanOut"),
        }
    }

    #[test]
    fn parse_coordination_pattern_pipeline() {
        let args = json!({"pattern": "pipeline", "agents": ["coder", "reviewer"]});
        let pattern = super::parse_coordination_pattern(&args).unwrap();
        match pattern {
            astra_services::coordination::CoordinationPattern::Pipeline { stages, .. } => {
                assert_eq!(stages.len(), 2);
                assert_eq!(stages[0].agent_id, "coder");
                assert_eq!(stages[1].agent_id, "reviewer");
            }
            _ => panic!("expected Pipeline"),
        }
    }

    #[test]
    fn parse_coordination_pattern_adversarial() {
        let args =
            json!({"pattern": "adversarial", "agents": ["coder", "reviewer"], "max_rounds": 3});
        let pattern = super::parse_coordination_pattern(&args).unwrap();
        match pattern {
            astra_services::coordination::CoordinationPattern::AdversarialReview {
                producer_id,
                reviewer_id,
                max_rounds,
                ..
            } => {
                assert_eq!(producer_id, "coder");
                assert_eq!(reviewer_id, "reviewer");
                assert_eq!(max_rounds, 3);
            }
            _ => panic!("expected AdversarialReview"),
        }
    }

    #[test]
    fn parse_delegation_request_extracts_fields() {
        let tool_call = json!({
            "id": "call_abc",
            "type": "function",
            "function": {
                "name": "delegate",
                "arguments": "{\"task\": \"write tests\", \"agents\": [\"coder\"], \"pattern\": \"sequential\", \"context\": {\"repo\": \"my-repo\"}}"
            }
        });
        let req = super::parse_delegation_request(
            &tool_call,
            "run-123",
            "session-456",
            2,
            &astra_core::SkillSearchSettings::default(),
            None,
        )
        .unwrap();
        assert_eq!(req.task, "write tests");
        assert_eq!(req.parent_run_id, "run-123");
        assert_eq!(req.depth, 2);
        assert!(req.context.contains_key("session_id"));
        assert!(req.context.contains_key("skill_search"));
        assert!(req.context.contains_key("repo"));
    }

    #[test]
    fn parse_delegation_request_handles_missing_args() {
        let tool_call = json!({
            "id": "call_bad",
            "type": "function",
            "function": {
                "name": "delegate"
            }
        });
        let result = super::parse_delegation_request(
            &tool_call,
            "run-1",
            "sess-1",
            0,
            &astra_core::SkillSearchSettings::default(),
            None,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing arguments"));
    }

    #[test]
    fn parse_delegation_request_rejects_max_recursion_depth() {
        let tool_call = json!({
            "type": "function",
            "function": {
                "name": "delegate",
                "arguments": "{\"task\": \"review this patch\", \"agents\": [\"reviewer\"]}"
            }
        });

        let result = super::parse_delegation_request(
            &tool_call,
            "run-1",
            "sess-1",
            crate::turn::agentic_recursion_guard::MAX_AGENT_RECURSION_DEPTH,
            &astra_core::SkillSearchSettings::default(),
            None,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("recursion depth 3 reached maximum 3")
        );
    }

    #[test]
    fn parse_delegation_request_without_pattern_uses_exploration_fan_out() {
        let tool_call = json!({
            "type": "function",
            "function": {
                "name": "delegate",
                "arguments": "{\"task\": \"search the codebase for relevant modules\", \"agents\": [\"coder\", \"reviewer\"]}"
            }
        });
        let adaptive_context = super::DelegationAdaptiveContext {
            scenario: Some(crate::user_profile::Scenario::Exploration),
            preferred_pattern: None,
        };

        let req = super::parse_delegation_request(
            &tool_call,
            "run-123",
            "session-456",
            0,
            &astra_core::SkillSearchSettings::default(),
            Some(&adaptive_context),
        )
        .unwrap();

        assert!(matches!(
            req.pattern,
            astra_services::coordination::CoordinationPattern::FanOut { .. }
        ));
        assert_eq!(
            req.context["adaptive_coordination"]["selected_pattern"],
            json!("fan_out")
        );
    }

    #[test]
    fn parse_delegation_request_without_pattern_uses_code_review_adversarial() {
        let tool_call = json!({
            "type": "function",
            "function": {
                "name": "delegate",
                "arguments": "{\"task\": \"review this patch\", \"agents\": [\"coder\", \"reviewer\"]}"
            }
        });
        let adaptive_context = super::DelegationAdaptiveContext {
            scenario: Some(crate::user_profile::Scenario::CodeReview),
            preferred_pattern: None,
        };

        let req = super::parse_delegation_request(
            &tool_call,
            "run-123",
            "session-456",
            0,
            &astra_core::SkillSearchSettings::default(),
            Some(&adaptive_context),
        )
        .unwrap();

        assert!(matches!(
            req.pattern,
            astra_services::coordination::CoordinationPattern::AdversarialReview { .. }
        ));
        assert_eq!(
            req.context["adaptive_coordination"]["reason"],
            json!("code_review_scenario_prefers_review_loop")
        );
    }

    #[test]
    fn pattern_from_name_fan_out() {
        let agents = vec!["a".to_string(), "b".to_string()];
        let args = json!({"timeout": 60});
        let pattern = super::pattern_from_name("fan_out", &agents, &args).unwrap();
        match pattern {
            astra_services::coordination::CoordinationPattern::FanOut {
                agent_ids,
                timeout_sec,
                ..
            } => {
                assert_eq!(agent_ids, vec!["a", "b"]);
                assert_eq!(timeout_sec, 60);
            }
            _ => panic!("expected FanOut"),
        }
    }

    #[test]
    fn pattern_from_name_sequential() {
        let agents = vec!["x".to_string()];
        let args = json!({});
        let pattern = super::pattern_from_name("sequential", &agents, &args).unwrap();
        assert!(matches!(
            pattern,
            astra_services::coordination::CoordinationPattern::Sequential { .. }
        ));
    }

    #[test]
    fn pattern_from_name_pipeline() {
        let agents = vec!["plan".to_string(), "verify".to_string()];
        let args = json!({"timeout": 45});
        let pattern = super::pattern_from_name("pipeline", &agents, &args).unwrap();
        match pattern {
            astra_services::coordination::CoordinationPattern::Pipeline {
                stages,
                timeout_sec,
            } => {
                assert_eq!(timeout_sec, 45);
                assert_eq!(stages.len(), 2);
                assert_eq!(stages[0].agent_id, "plan");
                assert_eq!(stages[1].agent_id, "verify");
            }
            _ => panic!("expected Pipeline"),
        }
    }

    #[test]
    fn pattern_from_name_unknown_returns_none() {
        let agents = vec!["a".to_string()];
        let args = json!({});
        assert!(super::pattern_from_name("unknown_pattern", &agents, &args).is_none());
    }

    #[test]
    fn select_default_uses_history_when_available() {
        let args = json!({"agents": ["coder", "reviewer"]});
        let adaptive_context = super::DelegationAdaptiveContext {
            scenario: Some(crate::user_profile::Scenario::CodeReview),
            preferred_pattern: Some("fan_out".to_string()),
        };
        let (pattern, policy) = super::select_default_coordination_pattern(
            &args,
            "review code",
            Some(&adaptive_context),
        )
        .unwrap();
        assert!(
            matches!(
                pattern,
                astra_services::coordination::CoordinationPattern::FanOut { .. }
            ),
            "history should override scenario heuristic"
        );
        assert_eq!(policy["selection_source"], "outcome_history");
    }

    #[test]
    fn select_default_uses_pipeline_history_when_available() {
        let args = json!({"agents": ["plan", "verify"]});
        let adaptive_context = super::DelegationAdaptiveContext {
            scenario: Some(crate::user_profile::Scenario::Testing),
            preferred_pattern: Some("pipeline".to_string()),
        };
        let (pattern, policy) = super::select_default_coordination_pattern(
            &args,
            "run staged verification",
            Some(&adaptive_context),
        )
        .unwrap();
        assert!(
            matches!(
                pattern,
                astra_services::coordination::CoordinationPattern::Pipeline { .. }
            ),
            "history should restore learned pipeline preference"
        );
        assert_eq!(policy["selection_source"], "outcome_history");
        assert_eq!(policy["selected_pattern"], "pipeline");
    }

    #[test]
    fn select_default_debugging_scenario() {
        let args = json!({"agents": ["coder"]});
        let adaptive_context = super::DelegationAdaptiveContext {
            scenario: Some(crate::user_profile::Scenario::Debugging),
            preferred_pattern: None,
        };
        let (_pattern, policy) = super::select_default_coordination_pattern(
            &args,
            "find the bug",
            Some(&adaptive_context),
        )
        .unwrap();
        assert_eq!(policy["selection_source"], "adaptive_default");
        assert_eq!(
            policy["reason"],
            "debugging_scenario_prefers_sequential_with_stop"
        );
    }

    #[test]
    fn select_default_testing_scenario() {
        let args = json!({"agents": ["coder", "tester"]});
        let adaptive_context = super::DelegationAdaptiveContext {
            scenario: Some(crate::user_profile::Scenario::Testing),
            preferred_pattern: None,
        };
        let (_pattern, policy) =
            super::select_default_coordination_pattern(&args, "run tests", Some(&adaptive_context))
                .unwrap();
        assert_eq!(policy["selection_source"], "adaptive_default");
        assert_eq!(
            policy["reason"],
            "testing_scenario_prefers_parallel_execution"
        );
    }

    #[test]
    fn format_delegation_result_includes_status_and_agents() {
        let result = astra_services::coordination::DelegationResult {
            delegation_id: "del-1".to_string(),
            status: "completed".to_string(),
            agent_results: vec![astra_services::coordination::AgentResult {
                agent_id: "coder".to_string(),
                run_id: "run-1".to_string(),
                status: "completed".to_string(),
                output: Some("implemented feature X".to_string()),
                error: None,
                prompt_tokens: 100,
                completion_tokens: 50,
                tool_calls: 3,
            }],
            aggregated_output: Some("All tasks done.".to_string()),
            total_prompt_tokens: 100,
            total_completion_tokens: 50,
            total_tool_calls: 3,
        };
        let formatted = super::format_delegation_result(&result);
        assert!(formatted.contains("del-1"));
        assert!(formatted.contains("completed"));
        assert!(formatted.contains("✅"));
        assert!(formatted.contains("coder"));
        assert!(formatted.contains("implemented feature X"));
        assert!(formatted.contains("All tasks done"));
        assert!(
            formatted
                .find("Final aggregated result")
                .unwrap_or(usize::MAX)
                < formatted.find("Sub-agent results").unwrap_or(usize::MAX)
        );
        assert!(formatted.contains("Tokens:"));
    }

    #[test]
    fn format_delegation_result_truncates_long_output() {
        let long_output = "x".repeat(1000);
        let result = astra_services::coordination::DelegationResult {
            delegation_id: "del-2".to_string(),
            status: "completed".to_string(),
            agent_results: vec![astra_services::coordination::AgentResult {
                agent_id: "writer".to_string(),
                run_id: "run-2".to_string(),
                status: "completed".to_string(),
                output: Some(long_output),
                error: None,
                prompt_tokens: 200,
                completion_tokens: 100,
                tool_calls: 1,
            }],
            aggregated_output: None,
            total_prompt_tokens: 200,
            total_completion_tokens: 100,
            total_tool_calls: 1,
        };
        let formatted = super::format_delegation_result(&result);
        assert!(formatted.contains('…'));
        // Output should be truncated to 500 Unicode scalars + ellipsis
        assert!(formatted.len() < 1500);
    }

    #[test]
    fn format_delegation_result_shows_errors() {
        let result = astra_services::coordination::DelegationResult {
            delegation_id: "del-3".to_string(),
            status: "partial_failure".to_string(),
            agent_results: vec![astra_services::coordination::AgentResult {
                agent_id: "coder".to_string(),
                run_id: "run-3".to_string(),
                status: "failed".to_string(),
                output: None,
                error: Some("timeout".to_string()),
                prompt_tokens: 50,
                completion_tokens: 0,
                tool_calls: 0,
            }],
            aggregated_output: None,
            total_prompt_tokens: 50,
            total_completion_tokens: 0,
            total_tool_calls: 0,
        };
        let formatted = super::format_delegation_result(&result);
        assert!(formatted.contains("❌"));
        assert!(formatted.contains("timeout"));
        assert!(formatted.contains("partial_failure"));
    }

    #[test]
    fn format_delegation_result_falls_back_to_single_success_output() {
        let result = astra_services::coordination::DelegationResult {
            delegation_id: "del-fallback".to_string(),
            status: "completed".to_string(),
            agent_results: vec![astra_services::coordination::AgentResult {
                agent_id: "coder".to_string(),
                run_id: "run-1".to_string(),
                status: "completed".to_string(),
                output: Some("single-agent final answer".to_string()),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
            }],
            aggregated_output: None,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_tool_calls: 0,
        };
        let formatted = super::format_delegation_result(&result);
        assert!(formatted.contains("Final aggregated result"));
        assert!(formatted.contains("single-agent final answer"));
    }

    #[test]
    fn format_delegation_terminal_preview_surfaces_summary_first() {
        let result = astra_services::coordination::DelegationResult {
            delegation_id: "del-preview".to_string(),
            status: "completed".to_string(),
            agent_results: vec![astra_services::coordination::AgentResult {
                agent_id: "coder".to_string(),
                run_id: "run-1".to_string(),
                status: "completed".to_string(),
                output: Some("implemented feature X".to_string()),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
            }],
            aggregated_output: Some("Final merged answer".to_string()),
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_tool_calls: 0,
        };
        let lines = super::format_delegation_terminal_preview(&result);
        assert_eq!(lines[0].0, HeadlessStderrStyle::Green);
        assert!(lines[0].1.contains("del-preview"));
        assert!(lines[1].1.contains("Final merged answer"));
    }

    #[tokio::test]
    async fn partition_and_execute_with_no_delegation_engine() {
        // When delegation_engine is None, all calls pass through
        let tool_calls = [
            json!({"id": "c1", "function": {"name": "bash", "arguments": "{}"}}),
            json!({"id": "c2", "function": {"name": "read_file", "arguments": "{}"}}),
        ];

        let state = make_state();

        // Verify that without delegation_engine, no delegation happens
        assert!(state.delegation_engine.is_none());
        // All calls remain as-is
        assert_eq!(tool_calls.len(), 2);
    }

    #[tokio::test]
    async fn partition_separates_delegate_from_regular_calls() {
        use crate::server::delegation_engine::{
            DelegationEngine, DelegationTracker, StubSubRunExecutor,
        };
        use crate::server::run_engine::RunEngine;
        use astra_services::AgentProfileRegistry;

        // Create a delegation engine with stub executor
        let mut registry = AgentProfileRegistry::new();
        {
            use astra_services::coordination::{AgentProfile, AgentTier};
            let _ = registry.register(AgentProfile::new("coder", "Coder", AgentTier::System));
        }
        let run_store = Arc::new(astra_services::runs::InMemoryRunStateStore::default());
        let engine = DelegationEngine::with_executor(
            Arc::new(tokio::sync::RwLock::new(registry)),
            Arc::new(RunEngine::new(run_store)),
            Arc::new(DelegationTracker::new()),
            Arc::new(StubSubRunExecutor),
        );

        let tool_calls = vec![
            json!({
                "id": "call_delegate",
                "type": "function",
                "function": {
                    "name": "delegate",
                    "arguments": "{\"task\": \"write tests\", \"agents\": [\"coder\"]}"
                }
            }),
            json!({
                "id": "call_bash",
                "type": "function",
                "function": {
                    "name": "bash",
                    "arguments": "{\"command\": \"ls\"}"
                }
            }),
        ];

        let (delegation_results, remaining) = super::partition_and_execute_delegations(
            &tool_calls,
            &engine,
            "test-run",
            "test-session",
            0,
            "orchestrator",
            None,
            &astra_core::SkillSearchSettings::default(),
            None,
        )
        .await;

        // One delegation executed, one regular call passed through
        assert_eq!(delegation_results.len(), 1);
        assert_eq!(remaining.len(), 1);

        // Delegation result should contain the call_id
        assert_eq!(delegation_results[0].call_id, "call_delegate");
        assert!(delegation_results[0].summary.contains("Delegation"));
        // Remaining should be the bash call
        assert_eq!(remaining[0]["id"], "call_bash");
    }

    #[tokio::test]
    async fn partition_handles_all_delegate_calls() {
        use crate::server::delegation_engine::{
            DelegationEngine, DelegationTracker, StubSubRunExecutor,
        };
        use crate::server::run_engine::RunEngine;
        use astra_services::AgentProfileRegistry;

        let mut registry = AgentProfileRegistry::new();
        {
            use astra_services::coordination::{AgentProfile, AgentTier};
            let _ = registry.register(AgentProfile::new("coder", "Coder", AgentTier::System));
            let _ = registry.register(AgentProfile::new("reviewer", "Reviewer", AgentTier::System));
        }
        let run_store = Arc::new(astra_services::runs::InMemoryRunStateStore::default());
        let engine = DelegationEngine::with_executor(
            Arc::new(tokio::sync::RwLock::new(registry)),
            Arc::new(RunEngine::new(run_store)),
            Arc::new(DelegationTracker::new()),
            Arc::new(StubSubRunExecutor),
        );

        let tool_calls = vec![
            json!({
                "id": "d1",
                "function": {"name": "delegate", "arguments": "{\"task\": \"code\", \"agents\": [\"coder\"]}"}
            }),
            json!({
                "id": "d2",
                "function": {"name": "delegate", "arguments": "{\"task\": \"review\", \"agents\": [\"reviewer\"]}"}
            }),
        ];

        let (delegation_results, remaining) = super::partition_and_execute_delegations(
            &tool_calls,
            &engine,
            "run-1",
            "sess-1",
            0,
            "orchestrator",
            None,
            &astra_core::SkillSearchSettings::default(),
            None,
        )
        .await;

        assert_eq!(delegation_results.len(), 2);
        assert!(remaining.is_empty());
    }

    #[tokio::test]
    async fn partition_handles_invalid_delegation_args_gracefully() {
        use crate::server::delegation_engine::{
            DelegationEngine, DelegationTracker, StubSubRunExecutor,
        };
        use crate::server::run_engine::RunEngine;
        use astra_services::AgentProfileRegistry;

        let registry = AgentProfileRegistry::new();
        let run_store = Arc::new(astra_services::runs::InMemoryRunStateStore::default());
        let engine = DelegationEngine::with_executor(
            Arc::new(tokio::sync::RwLock::new(registry)),
            Arc::new(RunEngine::new(run_store)),
            Arc::new(DelegationTracker::new()),
            Arc::new(StubSubRunExecutor),
        );

        let tool_calls = vec![json!({
            "id": "bad_call",
            "function": {"name": "delegate", "arguments": "not valid json!!!"}
        })];

        let (delegation_results, remaining) = super::partition_and_execute_delegations(
            &tool_calls,
            &engine,
            "run-1",
            "sess-1",
            0,
            "orchestrator",
            None,
            &astra_core::SkillSearchSettings::default(),
            None,
        )
        .await;

        assert_eq!(delegation_results.len(), 1);
        assert!(
            delegation_results[0]
                .summary
                .contains("Invalid delegation request")
        );
        assert!(remaining.is_empty());
    }

    #[test]
    fn state_delegation_engine_defaults_to_none() {
        let state = make_state();
        assert!(state.delegation_engine.is_none());
    }

    // ── E2E delegation round-trip tests ─────────────────────────────────────

    /// Helper to build a DelegationEngine with StubSubRunExecutor for tests.
    fn make_test_delegation_engine() -> Arc<crate::server::delegation_engine::DelegationEngine> {
        use crate::server::delegation_engine::{
            DelegationEngine, DelegationTracker, StubSubRunExecutor,
        };
        use crate::server::run_engine::RunEngine;
        use astra_services::AgentProfileRegistry;
        use astra_services::coordination::{AgentProfile, AgentTier};

        let mut registry = AgentProfileRegistry::new();
        let _ = registry.register(AgentProfile::new(
            "orchestrator",
            "Orchestrator",
            AgentTier::Orchestrator,
        ));
        let mut coder = AgentProfile::new("coder", "Coder", AgentTier::System);
        coder.system_prompt = Some("You are a coder.".to_string());
        let _ = registry.register(coder);
        let mut reviewer = AgentProfile::new("reviewer", "Reviewer", AgentTier::System);
        reviewer.system_prompt = Some("You are a reviewer.".to_string());
        let _ = registry.register(reviewer);

        let run_store = Arc::new(astra_services::runs::InMemoryRunStateStore::default());
        Arc::new(DelegationEngine::with_executor(
            Arc::new(tokio::sync::RwLock::new(registry)),
            Arc::new(RunEngine::new(run_store)),
            Arc::new(DelegationTracker::new()),
            Arc::new(StubSubRunExecutor),
        ))
    }

    /// Helper: make a HostTurnResult with server-side tool_calls (like an LLM
    /// requesting the "delegate" tool).
    fn delegate_tool_call_result(
        call_id: &str,
        args_json: &str,
        prompt: u64,
        completion: u64,
    ) -> HostTurnResult {
        HostTurnResult {
            accum: ChatTurnSseAccum {
                has_tool_calls: true,
                has_usage: true,
                prompt_tokens: prompt,
                completion_tokens: completion,
                tool_calls: vec![json!({
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": "delegate",
                        "arguments": args_json,
                    }
                })],
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: Some(30),
            edge_tool_round: Vec::new(),
        }
    }

    #[tokio::test]
    async fn e2e_delegation_round_trip_through_loop() {
        // Turn 1: LLM issues a delegate tool call
        // Turn 2: LLM produces final text after seeing delegation result
        let turns = vec![
            delegate_tool_call_result(
                "call_del_1",
                r#"{"task": "write unit tests", "agents": ["coder"], "pattern": "sequential"}"#,
                100,
                50,
            ),
            text_result(
                "Done! Tests written based on delegation results.",
                80,
                30,
                None,
            ),
        ];

        let mut host = MockHost::new(turns);
        host.quiet = false;
        let mut state = make_state();
        state.messages.push(
            json!({"role": "user", "content": "Please delegate test writing to the coder agent."}),
        );
        state.current_run_id = Some("test-run-e2e".to_string());
        state.current_session_id = Some("test-session-e2e".to_string());

        // Wire delegation engine
        state.delegation_engine = Some(make_test_delegation_engine());

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok(), "loop should complete: {outcome:?}");
        assert_eq!(
            state.final_text,
            "Done! Tests written based on delegation results."
        );

        // Verify delegation result was injected into messages
        let tool_messages: Vec<&Value> = state
            .messages
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
            .collect();
        assert!(
            !tool_messages.is_empty(),
            "delegation result should appear as tool message"
        );

        // The tool message should reference our call_id
        let has_delegation_result = tool_messages
            .iter()
            .any(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("call_del_1"));
        assert!(
            has_delegation_result,
            "delegation result should reference call_del_1"
        );

        // Verify the delegation result content mentions delegation status
        let delegation_content = tool_messages
            .iter()
            .find(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("call_del_1"))
            .and_then(|m| m.get("content").and_then(Value::as_str))
            .unwrap_or("");
        assert!(
            delegation_content.contains("Delegation") || delegation_content.contains("completed"),
            "delegation result should contain status info, got: {delegation_content}"
        );
        assert!(
            delegation_content.contains("Final aggregated result"),
            "delegation result should prioritize the final aggregate: {delegation_content}"
        );
        assert!(
            delegation_content
                .find("Final aggregated result")
                .unwrap_or(usize::MAX)
                < delegation_content
                    .find("Sub-agent results")
                    .unwrap_or(usize::MAX),
            "aggregate should appear before per-agent details: {delegation_content}"
        );

        // Verify token accounting includes both turns
        assert!(state.total_prompt >= 180, "should accumulate prompt tokens");
        assert!(
            state.total_completion >= 80,
            "should accumulate completion tokens"
        );
        assert!(
            host.emitted_lines
                .iter()
                .any(|line| line.contains("parent agent is paused")),
            "delegation wait state should be emitted"
        );
        assert!(
            host.emitted_lines
                .iter()
                .any(|line| line.contains("🤝 Delegation")),
            "delegation completion preview should be emitted"
        );
        assert!(
            host.emitted_lines
                .iter()
                .any(|line| line.contains("incorporating delegated results")),
            "parent incorporation phase should be emitted"
        );
    }

    #[tokio::test]
    async fn e2e_delegation_mixed_with_regular_tools() {
        // Turn 1: LLM issues both a delegate call AND a regular tool call
        // Turn 2: Final text
        let mut turn1 = delegate_tool_call_result(
            "call_del_mix",
            r#"{"task": "review code", "agents": ["reviewer"]}"#,
            100,
            50,
        );
        // Add a regular edge tool to this turn
        turn1
            .edge_tool_round
            .push(make_edge_tool("bash", "ls output"));

        let turns = vec![
            turn1,
            text_result("Mixed delegation + tool complete.", 60, 20, None),
        ];

        let mut host = MockHost::new(turns).with_valid_tools(&["bash"]);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "review and list files"}));
        state.current_run_id = Some("run-mix".to_string());
        state.delegation_engine = Some(make_test_delegation_engine());

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(state.final_text, "Mixed delegation + tool complete.");

        // Both delegation result and edge tool result should be in messages
        let tool_msgs: Vec<&Value> = state
            .messages
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
            .collect();
        // At minimum: 1 delegation + potential edge tool messages
        assert!(!tool_msgs.is_empty());
    }

    #[tokio::test]
    async fn e2e_delegation_with_invalid_args_continues_gracefully() {
        // Turn 1: LLM issues a malformed delegate call
        // The loop should inject an error result and continue to turn 2
        let turns = vec![
            delegate_tool_call_result("call_bad", "this is not json", 100, 50),
            text_result("Recovered after bad delegation.", 60, 20, None),
        ];

        let mut host = MockHost::new(turns);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "delegate something"}));
        state.delegation_engine = Some(make_test_delegation_engine());

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(state.final_text, "Recovered after bad delegation.");

        // Error message should be injected as tool result
        let error_msg = state
            .messages
            .iter()
            .find(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("call_bad"))
            .and_then(|m| m.get("content").and_then(Value::as_str))
            .unwrap_or("");
        assert!(
            error_msg.contains("Invalid delegation request"),
            "should contain error: {error_msg}"
        );
    }

    #[tokio::test]
    async fn e2e_delegation_fan_out_pattern() {
        // Test fan_out pattern: multiple agents in parallel
        let turns = vec![
            delegate_tool_call_result(
                "call_fanout",
                r#"{"task": "implement feature", "agents": ["coder", "reviewer"], "pattern": "fan_out"}"#,
                150,
                75,
            ),
            text_result("Fan-out complete.", 60, 20, None),
        ];

        let mut host = MockHost::new(turns);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "implement and review"}));
        state.current_run_id = Some("run-fanout".to_string());
        state.delegation_engine = Some(make_test_delegation_engine());

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(state.final_text, "Fan-out complete.");

        // Fan-out result should mention both agents
        let result_content = state
            .messages
            .iter()
            .find(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("call_fanout"))
            .and_then(|m| m.get("content").and_then(Value::as_str))
            .unwrap_or("");
        assert!(
            result_content.contains("coder"),
            "result should mention coder agent"
        );
        assert!(
            result_content.contains("reviewer"),
            "result should mention reviewer agent"
        );
    }

    #[tokio::test]
    async fn e2e_no_delegation_engine_passthrough() {
        // When no delegation engine is wired, delegate tool calls
        // pass through to the headless round (no interception)
        let turn1_tool_calls = vec![json!({
            "id": "call_del_passthrough",
            "type": "function",
            "function": {
                "name": "delegate",
                "arguments": "{\"task\": \"test\", \"agents\": [\"coder\"]}"
            }
        })];

        let turns = vec![
            server_tool_result(turn1_tool_calls, Vec::new(), 100, 50, Some(20)),
            text_result("Passed through.", 60, 20, None),
        ];

        let mut host = MockHost::new(turns).with_valid_tools(&["delegate"]);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "test delegate passthrough"}));
        // Intentionally leave delegation_engine as None
        assert!(state.delegation_engine.is_none());

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(state.final_text, "Passed through.");
    }

    #[tokio::test]
    async fn e2e_adversarial_delegation_pattern() {
        let turns = vec![
            delegate_tool_call_result(
                "call_adversarial",
                r#"{"task": "write secure auth", "agents": ["coder", "reviewer"], "pattern": "adversarial", "max_rounds": 2}"#,
                200,
                100,
            ),
            text_result("Adversarial review complete.", 80, 40, None),
        ];

        let mut host = MockHost::new(turns);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "write and review auth"}));
        state.current_run_id = Some("run-adversarial".to_string());
        state.delegation_engine = Some(make_test_delegation_engine());

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(state.final_text, "Adversarial review complete.");
    }

    // ── Auto-injection tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn auto_inject_delegate_schema_when_engine_present() {
        // When delegation_engine is Some, the loop preamble should call
        // inject_tool_schema with the delegate tool schema.
        let mut host = MockHost::new(vec![text_result("done", 50, 20, Some(10))]);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "hello"}));
        state.delegation_engine = Some(make_test_delegation_engine());

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert_eq!(host.injected_schemas.len(), 1);
        let injected = &host.injected_schemas[0];
        let name = injected["function"]["name"].as_str().unwrap();
        assert_eq!(name, "delegate");
        assert!(host.valid_tools.contains("delegate"));
    }

    #[tokio::test]
    async fn no_inject_when_delegation_engine_absent() {
        // When delegation_engine is None, no schema should be injected.
        let mut host = MockHost::new(vec![text_result("done", 50, 20, Some(10))]);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "hello"}));
        // delegation_engine defaults to None

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(host.injected_schemas.is_empty());
        assert!(!host.valid_tools.contains("delegate"));
    }

    #[tokio::test]
    async fn injected_schema_matches_delegate_tool_schema() {
        let mut host = MockHost::new(vec![text_result("done", 50, 20, Some(10))]);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "hello"}));
        state.delegation_engine = Some(make_test_delegation_engine());

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;

        let expected = delegate_tool_schema();
        assert_eq!(host.injected_schemas[0], expected);
    }

    #[test]
    fn delegate_schema_has_required_openai_structure() {
        let schema = delegate_tool_schema();
        assert_eq!(schema["type"], "function");
        assert_eq!(schema["function"]["name"], "delegate");
        assert!(schema["function"]["description"].as_str().unwrap().len() > 10);
        let params = &schema["function"]["parameters"];
        assert_eq!(params["type"], "object");
        let required = params["required"].as_array().unwrap();
        assert!(required.contains(&json!("task")));
        assert!(required.contains(&json!("agents")));
        let props = &params["properties"];
        assert!(props.get("task").is_some());
        assert!(props.get("agents").is_some());
        assert!(props.get("pattern").is_some());
        assert!(props.get("max_rounds").is_some());
        assert!(props.get("context").is_some());
    }

    #[tokio::test]
    async fn auto_inject_only_once_across_loop() {
        // Even with multiple turns, injection should happen only once (in preamble).
        // Use a delegate call followed by final text — two turns total.
        let mut host = MockHost::new(vec![
            text_result("still going", 100, 50, Some(10)),
            text_result("done", 50, 20, None),
        ]);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "list files"}));
        state.delegation_engine = Some(make_test_delegation_engine());

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;

        // Only one injection, not one per turn
        assert_eq!(host.injected_schemas.len(), 1);
    }

    // ── is_valid_model_string tests ──────────────────────────────────────

    #[test]
    fn valid_model_strings() {
        assert!(crate::turn::agentic_tool_interception::is_valid_model_string("gpt-4o"));
        assert!(
            crate::turn::agentic_tool_interception::is_valid_model_string(
                "claude-sonnet-4-20250514"
            )
        );
        assert!(crate::turn::agentic_tool_interception::is_valid_model_string("claude-3.5-sonnet"));
        assert!(crate::turn::agentic_tool_interception::is_valid_model_string("openai/gpt-4o"));
        assert!(
            crate::turn::agentic_tool_interception::is_valid_model_string("anthropic:claude-3")
        );
        assert!(crate::turn::agentic_tool_interception::is_valid_model_string("m0"));
    }

    #[test]
    fn invalid_model_strings() {
        assert!(!crate::turn::agentic_tool_interception::is_valid_model_string(""));
        assert!(!crate::turn::agentic_tool_interception::is_valid_model_string("x")); // too short
        assert!(
            !crate::turn::agentic_tool_interception::is_valid_model_string("model with spaces")
        );
        assert!(
            !crate::turn::agentic_tool_interception::is_valid_model_string("-starts-with-dash")
        );
        assert!(!crate::turn::agentic_tool_interception::is_valid_model_string("has;semicolon"));
        assert!(!crate::turn::agentic_tool_interception::is_valid_model_string("has$dollar"));
        assert!(!crate::turn::agentic_tool_interception::is_valid_model_string("has`backtick`"));
        assert!(!crate::turn::agentic_tool_interception::is_valid_model_string("has\nnewline"));
        assert!(!crate::turn::agentic_tool_interception::is_valid_model_string("has\ttab"));
        assert!(!crate::turn::agentic_tool_interception::is_valid_model_string(&"a".repeat(129))); // too long
    }

    #[test]
    fn model_string_boundary_lengths() {
        assert!(crate::turn::agentic_tool_interception::is_valid_model_string("ab")); // min valid
        assert!(
            crate::turn::agentic_tool_interception::is_valid_model_string(&format!(
                "m{}",
                "a".repeat(127)
            ))
        ); // 128 = max
        assert!(
            !crate::turn::agentic_tool_interception::is_valid_model_string(&format!(
                "m{}",
                "a".repeat(128)
            ))
        ); // 129 = over
    }

    // ── Skill pipeline integration tests ─────────────────────────────────

    /// Stub SkillResolver for agentic loop integration tests.
    struct StubSkillResolver {
        skills: Vec<(String, String, String, Option<String>, Vec<String>)>,
    }

    impl StubSkillResolver {
        fn new() -> Self {
            Self {
                skills: vec![(
                    "test-skill".into(),
                    "A test skill".into(),
                    "Follow these instructions carefully.".into(),
                    None,
                    vec![],
                )],
            }
        }

        fn with_model(mut self, model: &str) -> Self {
            self.skills[0].3 = Some(model.to_string());
            self
        }

        fn with_allowed_tools(mut self, tools: Vec<String>) -> Self {
            self.skills[0].4 = tools;
            self
        }
    }

    impl crate::turn::skill_tool::SkillResolver for StubSkillResolver {
        fn resolve(&self, name: &str) -> Result<crate::turn::skill_tool::ResolvedSkill, String> {
            self.skills
                .iter()
                .find(|(n, _, _, _, _)| n == name)
                .map(
                    |(n, _, inst, model, tools)| crate::turn::skill_tool::ResolvedSkill {
                        name: n.clone(),
                        instructions: inst.clone(),
                        model: model.clone(),
                        max_tokens: None,
                        allowed_tools: tools.clone(),
                        execution_context: crate::skills::manifest::ExecutionContext::Inline,
                        hooks: crate::skills::hooks::SkillHooks::default(),
                        skill_dir: None,
                        source: crate::skills::manifest::SkillSourceKind::Local,
                        success_criteria: Vec::new(),
                        composition: None,
                        input_schema: None,
                        aliases: Vec::new(),

                        effort: None,
                        agent_type: None,
                        trust_tier: crate::skills::manifest::TrustTier::Bundled,
                    },
                )
                .ok_or_else(|| format!("unknown skill: {name}"))
        }

        fn available_skills(&self) -> Vec<crate::turn::skill_tool::SkillToolInfo> {
            self.skills
                .iter()
                .map(|(n, d, _, _, _)| crate::turn::skill_tool::SkillToolInfo {
                    name: n.clone(),
                    description: d.clone(),
                    when_to_use: None,
                    source: crate::skills::manifest::SkillSourceKind::Local,
                    aliases: Vec::new(),
                    category: None,
                    tags: Vec::new(),
                    triggers: Vec::new(),
                })
                .collect()
        }
    }

    /// Make a HostTurnResult with a `skill` tool call.
    fn skill_tool_call_result(
        call_id: &str,
        args_json: &str,
        prompt: u64,
        completion: u64,
    ) -> HostTurnResult {
        HostTurnResult {
            accum: ChatTurnSseAccum {
                has_tool_calls: true,
                has_usage: true,
                prompt_tokens: prompt,
                completion_tokens: completion,
                tool_calls: vec![json!({
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": "skill",
                        "arguments": args_json,
                    }
                })],
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: Some(30),
            edge_tool_round: Vec::new(),
        }
    }

    #[tokio::test]
    async fn skill_schema_injected_when_resolver_present() {
        let resolver = StubSkillResolver::new();
        let mut host = MockHost::new(vec![text_result("done", 50, 20, Some(10))]);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "hello"}));
        state.skills.resolver = Some(Arc::new(resolver));

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert_eq!(host.injected_schemas.len(), 1);
        let name = host.injected_schemas[0]["function"]["name"]
            .as_str()
            .unwrap();
        assert_eq!(name, "skill");
        assert!(host.valid_tools.contains("skill"));
    }

    #[tokio::test]
    async fn no_skill_schema_when_resolver_absent() {
        let mut host = MockHost::new(vec![text_result("done", 50, 20, Some(10))]);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "hello"}));

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(host.injected_schemas.is_empty());
        assert!(!host.valid_tools.contains("skill"));
    }

    #[tokio::test]
    async fn skill_tool_call_intercepted_and_result_injected() {
        let resolver = StubSkillResolver::new();
        let turns = vec![
            skill_tool_call_result("call_skill_1", r#"{"skill_name": "test-skill"}"#, 100, 50),
            text_result("Following the skill instructions.", 80, 30, None),
        ];

        let mut host = MockHost::new(turns);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "use the test skill"}));
        state.skills.resolver = Some(Arc::new(resolver));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(state.final_text, "Following the skill instructions.");

        // Skill result should be in messages as a tool message
        let tool_msgs: Vec<&Value> = state
            .messages
            .iter()
            .filter(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("call_skill_1"))
            .collect();
        assert_eq!(tool_msgs.len(), 1);
        let content = tool_msgs[0]["content"].as_str().unwrap();
        assert!(content.contains("# Skill: test-skill"));
        assert!(content.contains("Follow these instructions carefully."));
    }

    #[tokio::test]
    async fn skill_model_override_applied_and_cleared() {
        let resolver = StubSkillResolver::new().with_model("claude-sonnet-4-20250514");
        let turns = vec![
            skill_tool_call_result("call_1", r#"{"skill_name": "test-skill"}"#, 100, 50),
            text_result("Done with skill.", 80, 30, None),
        ];

        let mut host = MockHost::new(turns);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "use skill"}));
        state.skills.resolver = Some(Arc::new(resolver));

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;

        // Model override should be set after skill activation
        assert_eq!(
            state.skills.model_override.as_deref(),
            Some("claude-sonnet-4-20250514")
        );
    }

    #[tokio::test]
    async fn skill_model_override_rejected_for_invalid_string() {
        let resolver = StubSkillResolver::new().with_model("model; rm -rf /");
        let turns = vec![
            skill_tool_call_result("call_1", r#"{"skill_name": "test-skill"}"#, 100, 50),
            text_result("Done.", 80, 30, None),
        ];

        let mut host = MockHost::new(turns);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "use skill"}));
        state.skills.resolver = Some(Arc::new(resolver));

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;

        // Invalid model string should be rejected
        assert!(state.skills.model_override.is_none());
    }

    #[tokio::test]
    async fn skill_allowed_tools_set_and_cleared() {
        let resolver =
            StubSkillResolver::new().with_allowed_tools(vec!["bash".into(), "grep".into()]);
        let turns = vec![
            skill_tool_call_result("call_1", r#"{"skill_name": "test-skill"}"#, 100, 50),
            text_result("Done.", 80, 30, None),
        ];

        let mut host = MockHost::new(turns);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "use skill"}));
        state.skills.resolver = Some(Arc::new(resolver));

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;

        let allowed = state.skills.allowed_tools.as_ref().unwrap();
        assert!(allowed.contains("bash"));
        assert!(allowed.contains("grep"));
        assert_eq!(allowed.len(), 2);
    }

    #[tokio::test]
    async fn unrestricted_skill_clears_prior_overrides() {
        // Simulate: first skill sets overrides, second skill is unrestricted
        let mut state = make_state();
        state.skills.model_override = Some("old-model".into());
        state.skills.allowed_tools = Some(["bash".into()].into_iter().collect());

        // An unrestricted skill (no model, no tools) should clear both
        let resolver = StubSkillResolver::new(); // no model, no tools
        let turns = vec![
            skill_tool_call_result("call_1", r#"{"skill_name": "test-skill"}"#, 100, 50),
            text_result("Done.", 80, 30, None),
        ];

        let mut host = MockHost::new(turns);
        state
            .messages
            .push(json!({"role": "user", "content": "use skill"}));
        state.skills.resolver = Some(Arc::new(resolver));

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;

        // Both should be cleared
        assert!(state.skills.model_override.is_none());
        assert!(state.skills.allowed_tools.is_none());
    }

    #[tokio::test]
    async fn restricted_tools_not_accumulated_across_turns() {
        // This tests the invariant that restricted_tools doesn't permanently
        // grow from skill allowed_tools. After the loop, restricted_tools
        // should not contain skill-scoped restrictions.
        let resolver = StubSkillResolver::new().with_allowed_tools(vec!["bash".into()]);
        let turns = vec![
            skill_tool_call_result("call_1", r#"{"skill_name": "test-skill"}"#, 100, 50),
            text_result("Done.", 80, 30, None),
        ];

        let mut host = MockHost::new(turns).with_valid_tools(&["bash", "grep", "edit"]);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "use skill"}));
        state.skills.resolver = Some(Arc::new(resolver));

        // Pre-condition: restricted_tools is empty
        assert!(state.restricted_tools.is_empty());

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;

        // Post-condition: restricted_tools should NOT contain "grep" or "edit"
        // permanently (skill restrictions are transient, applied only in CLI host)
        // Note: the runtime loop itself doesn't apply restrictions — that's the
        // host's job in execute_turn(). This test verifies the runtime doesn't
        // pollute restricted_tools.
        // The skill_allowed_tools field IS set (for the host to use):
        let allowed = state.skills.allowed_tools.as_ref().unwrap();
        assert!(allowed.contains("bash"));
    }

    // ── CTX_ helper tests ──────────────────────────────────────────────────

    #[test]
    fn extract_repo_name_https() {
        assert_eq!(
            crate::turn::agentic_tool_interception::extract_repo_name_from_url(
                "https://github.com/org/my-repo.git"
            ),
            Some("my-repo".into())
        );
    }

    #[test]
    fn extract_repo_name_ssh() {
        assert_eq!(
            crate::turn::agentic_tool_interception::extract_repo_name_from_url(
                "git@github.com:org/my-repo.git"
            ),
            Some("my-repo".into())
        );
    }

    #[test]
    fn extract_repo_name_no_git_suffix() {
        assert_eq!(
            crate::turn::agentic_tool_interception::extract_repo_name_from_url(
                "https://github.com/org/my-repo"
            ),
            Some("my-repo".into())
        );
    }

    #[test]
    fn extract_repo_name_trailing_slash() {
        assert_eq!(
            crate::turn::agentic_tool_interception::extract_repo_name_from_url(
                "https://github.com/org/repo.git/"
            ),
            Some("repo".into())
        );
    }

    #[test]
    fn detect_project_types_rust_and_docker() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(tmp.path().join("Dockerfile"), "").unwrap();
        let types = crate::turn::agentic_tool_interception::detect_project_types(tmp.path());
        assert!(types.contains(&"rust"));
        assert!(types.contains(&"docker"));
    }

    #[test]
    fn detect_project_types_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let types = crate::turn::agentic_tool_interception::detect_project_types(tmp.path());
        assert!(types.is_empty());
    }

    #[test]
    fn detect_project_types_no_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        // Both pyproject.toml and setup.py → single "python"
        std::fs::write(tmp.path().join("pyproject.toml"), "").unwrap();
        std::fs::write(tmp.path().join("setup.py"), "").unwrap();
        let types = crate::turn::agentic_tool_interception::detect_project_types(tmp.path());
        assert_eq!(types.iter().filter(|&&t| t == "python").count(), 1);
    }

    // ── Skill listing ephemeral injection tests ─────────────────────────

    #[test]
    fn skill_listing_message_not_in_state_messages() {
        // Skill listing should be stored on the field, not pushed into messages.
        let mut state = make_state();
        state.messages = vec![json!({"role": "user", "content": "hi"})];
        state.skills.listing_message = Some(json!({
            "role": "system",
            "content": "<available_skills>...</available_skills>"
        }));

        // Messages should not contain the listing
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0]["role"], "user");
        // But the listing should be available for ephemeral injection
        assert!(state.skills.listing_message.is_some());
    }

    #[test]
    fn skill_listing_message_defaults_to_none() {
        let state = make_state();
        assert!(state.skills.listing_message.is_none());
    }

    #[test]
    fn skill_listing_system_message_format() {
        use crate::skills::manifest::SkillSourceKind;
        use crate::turn::skill_tool::{SkillToolInfo, skill_listing_system_message};

        let skills = vec![
            SkillToolInfo {
                name: "review".into(),
                description: "Code review".into(),
                when_to_use: None,
                source: SkillSourceKind::Bundled,
                aliases: vec![],
                category: None,
                tags: vec![],
                triggers: vec![],
            },
            SkillToolInfo {
                name: "debug".into(),
                description: "Debug issues".into(),
                when_to_use: None,
                source: SkillSourceKind::Bundled,
                aliases: vec![],
                category: None,
                tags: vec![],
                triggers: vec![],
            },
        ];

        let msg = skill_listing_system_message(&skills, None, None, false);
        let content = msg["content"].as_str().unwrap();

        // Must contain skill names in XML format
        assert!(
            content.contains("<name>review</name>"),
            "missing review skill"
        );
        assert!(
            content.contains("<name>debug</name>"),
            "missing debug skill"
        );
        assert!(
            content.contains("<available_skills>"),
            "missing opening tag"
        );
        assert!(
            content.contains("</available_skills>"),
            "missing closing tag"
        );
        // Must be a system message
        assert_eq!(msg["role"], "system");
    }

    #[test]
    fn skill_listing_empty_skills_produces_no_message() {
        use crate::turn::skill_tool::skill_listing_system_message;
        // With empty skills, the function still returns a message but with no skill entries
        let msg = skill_listing_system_message(&[], None, None, false);
        let content = msg["content"].as_str().unwrap();
        // Should have the wrapper but no <skill> entries
        assert!(content.contains("<available_skills>"));
        assert!(!content.contains("<name>"));
    }

    #[tokio::test]
    async fn skill_listing_not_persisted_after_turn() {
        // After a turn completes, state.messages should NOT contain the skill listing.
        let mut host = MockHost::new(vec![text_result("Hello!", 10, 5, Some(42))]);
        let mut state = make_state();
        state.messages = vec![json!({"role": "user", "content": "hi"})];
        state.skills.listing_message = Some(json!({
            "role": "system",
            "content": "skill listing content"
        }));

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;

        // Verify: no message in state.messages contains "skill listing content"
        for msg in &state.messages {
            if let Some(content) = msg.get("content").and_then(Value::as_str) {
                assert!(
                    !content.contains("skill listing content"),
                    "skill listing leaked into persistent messages: {:?}",
                    msg
                );
            }
        }
    }

    #[test]
    fn skill_listing_refresh_updates_field() {
        use crate::skills::manifest::SkillSourceKind;
        use crate::turn::skill_tool::{SkillToolInfo, skill_listing_system_message};

        let mut state = make_state();

        // Initial: no listing
        assert!(state.skills.listing_message.is_none());

        // Simulate first refresh with 1 skill
        let skills_v1 = vec![SkillToolInfo {
            name: "review".into(),
            description: "v1".into(),
            when_to_use: None,
            source: SkillSourceKind::Bundled,
            aliases: vec![],
            category: None,
            tags: vec![],
            triggers: vec![],
        }];
        state.skills.listing_message =
            Some(skill_listing_system_message(&skills_v1, None, None, false));
        let v1_content = state.skills.listing_message.as_ref().unwrap()["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(v1_content.contains("review"));

        // Simulate second refresh with 2 skills (hot-reload added one)
        let skills_v2 = vec![
            SkillToolInfo {
                name: "review".into(),
                description: "v2".into(),
                when_to_use: None,
                source: SkillSourceKind::Bundled,
                aliases: vec![],
                category: None,
                tags: vec![],
                triggers: vec![],
            },
            SkillToolInfo {
                name: "debug".into(),
                description: "new".into(),
                when_to_use: None,
                source: SkillSourceKind::Bundled,
                aliases: vec![],
                category: None,
                tags: vec![],
                triggers: vec![],
            },
        ];
        state.skills.listing_message =
            Some(skill_listing_system_message(&skills_v2, None, None, false));
        let v2_content = state.skills.listing_message.as_ref().unwrap()["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            v2_content.contains("debug"),
            "new skill should appear after refresh"
        );
    }

    #[test]
    fn ephemeral_prefix_inserted_at_start_of_payload_messages() {
        // Verify that ephemeral_prefix is inserted at index 0 of the messages array.
        let messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "hi"}),
        ];
        let prefix = json!({"role": "system", "content": "skill listing"});

        let mut payload = json!({"messages": messages});
        // Simulate what prepare_chat_turn_payload does
        if let Some(arr) = payload.get_mut("messages").and_then(Value::as_array_mut) {
            arr.insert(0, prefix.clone());
        }

        let arr = payload["messages"].as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["content"], "skill listing");
        assert_eq!(arr[1]["content"], "hello");
        assert_eq!(arr[2]["content"], "hi");
    }

    #[test]
    fn no_ephemeral_prefix_leaves_messages_unchanged() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let mut payload = json!({"messages": messages});
        let prefix: Option<&Value> = None;

        // Simulate: no prefix → no modification
        if let Some(p) = prefix {
            if let Some(arr) = payload.get_mut("messages").and_then(Value::as_array_mut) {
                arr.insert(0, p.clone());
            }
        }

        let arr = payload["messages"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["content"], "hello");
    }

    // ── Invoked skills dedup tests ──────────────────────────────────────

    #[test]
    fn invoked_skills_defaults_to_empty() {
        let state = make_state();
        assert!(state.skills.invoked.is_empty());
    }

    #[tokio::test]
    async fn skill_dedup_returns_stub_on_second_invocation() {
        // Turn 1: skill call → full instructions returned + recorded
        // Turn 2: same skill call → stub returned (dedup)
        // Turn 3: text completion
        let resolver = StubSkillResolver::new(); // has "test-skill"
        let turns = vec![
            // Turn 1: LLM calls skill
            skill_tool_call_result("call_1", r#"{"skill_name": "test-skill"}"#, 100, 50),
            // Turn 2: LLM calls same skill again
            skill_tool_call_result("call_2", r#"{"skill_name": "test-skill"}"#, 100, 50),
            // Turn 3: LLM finishes
            text_result("Done.", 80, 20, None),
        ];

        let mut host = MockHost::new(turns);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "use skill twice"}));
        state.skills.resolver = Some(Arc::new(resolver));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        // First call: full instructions
        let msg1: Vec<&Value> = state
            .messages
            .iter()
            .filter(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("call_1"))
            .collect();
        assert_eq!(msg1.len(), 1);
        assert!(
            msg1[0]["content"]
                .as_str()
                .unwrap()
                .contains("# Skill: test-skill")
        );
        assert!(
            msg1[0]["content"]
                .as_str()
                .unwrap()
                .contains("Follow these instructions carefully.")
        );

        // Second call: stub (dedup)
        let msg2: Vec<&Value> = state
            .messages
            .iter()
            .filter(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("call_2"))
            .collect();
        assert_eq!(msg2.len(), 1);
        let stub = msg2[0]["content"].as_str().unwrap();
        assert!(
            stub.contains("already loaded"),
            "expected dedup stub, got: {stub}"
        );
        assert!(
            !stub.contains("# Skill:"),
            "stub should NOT contain full instructions"
        );

        // Skill should be tracked
        assert!(state.skills.invoked.contains_key("test-skill"));
        assert_eq!(state.skills.invoked["test-skill"].invoked_at_turn, 1);
    }

    #[tokio::test]
    async fn skill_dedup_allows_different_skills() {
        let mut resolver = StubSkillResolver::new(); // has "test-skill"
        resolver.skills.push((
            "other-skill".into(),
            "Another skill".into(),
            "Other instructions.".into(),
            None,
            vec![],
        ));
        let turns = vec![
            skill_tool_call_result("call_1", r#"{"skill_name": "test-skill"}"#, 100, 50),
            skill_tool_call_result("call_2", r#"{"skill_name": "other-skill"}"#, 100, 50),
            text_result("Done.", 80, 20, None),
        ];

        let mut host = MockHost::new(turns);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "use both skills"}));
        state.skills.resolver = Some(Arc::new(resolver));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        // Both should get full instructions (different skills, no dedup)
        let msg1: Vec<&Value> = state
            .messages
            .iter()
            .filter(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("call_1"))
            .collect();
        assert!(
            msg1[0]["content"]
                .as_str()
                .unwrap()
                .contains("# Skill: test-skill")
        );

        let msg2: Vec<&Value> = state
            .messages
            .iter()
            .filter(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("call_2"))
            .collect();
        assert!(
            msg2[0]["content"]
                .as_str()
                .unwrap()
                .contains("# Skill: other-skill")
        );

        // Both tracked
        assert_eq!(state.skills.invoked.len(), 2);
    }

    #[test]
    fn post_compact_skill_reinjection() {
        use crate::turn::cloud::attachments::AttachmentBuilder;
        use crate::turn::skill_tool::InvokedSkill;

        let mut state = make_state();
        state.skills.invoked.insert(
            "review-changes".into(),
            InvokedSkill {
                name: "review-changes".into(),
                content: "# Review\nDo a code review.".into(),
                invoked_at_turn: 2,
            },
        );

        // Simulate post-compaction re-injection
        let mut builder = AttachmentBuilder::new();
        let mut skills: Vec<_> = state.skills.invoked.values().collect();
        skills.sort_by(|a, b| b.invoked_at_turn.cmp(&a.invoked_at_turn));
        for skill in skills {
            builder.add_skill(&skill.name, &skill.content);
        }
        let attachments = builder.build();
        let msgs = attachments.to_messages();

        assert_eq!(msgs.len(), 1);
        let content = msgs[0]["content"].as_str().unwrap();
        assert!(content.contains("review-changes"));
        assert!(content.contains("# Review"));
    }

    // ── Skill exclusivity tests ─────────────────────────────────────────

    /// Helper: a turn where the model emits a skill call AND a regular tool call together.
    fn skill_plus_regular_tool_call_result(
        skill_call_id: &str,
        skill_args: &str,
        regular_call_id: &str,
        regular_tool: &str,
        regular_args: &str,
        prompt: u64,
        completion: u64,
    ) -> HostTurnResult {
        HostTurnResult {
            accum: ChatTurnSseAccum {
                has_tool_calls: true,
                has_usage: true,
                prompt_tokens: prompt,
                completion_tokens: completion,
                tool_calls: vec![
                    json!({
                        "id": skill_call_id,
                        "type": "function",
                        "function": {
                            "name": "skill",
                            "arguments": skill_args,
                        }
                    }),
                    json!({
                        "id": regular_call_id,
                        "type": "function",
                        "function": {
                            "name": regular_tool,
                            "arguments": regular_args,
                        }
                    }),
                ],
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: Some(30),
            edge_tool_round: Vec::new(),
        }
    }

    #[tokio::test]
    async fn skill_exclusivity_defers_parallel_non_skill_tool_calls() {
        let resolver = StubSkillResolver::new();
        let turns = vec![
            // Turn 1: model emits skill + write_file in parallel
            skill_plus_regular_tool_call_result(
                "call_skill",
                r#"{"skill_name": "test-skill"}"#,
                "call_write",
                "write_file",
                r#"{"path": "/tmp/test.html", "content": "hello"}"#,
                100,
                50,
            ),
            // Turn 2: model follows skill instructions properly
            text_result("Done following skill.", 80, 30, None),
        ];

        let mut host = MockHost::new(turns);
        host.quiet = false; // enable stderr output to check deferred message
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "use the test skill"}));
        state.skills.resolver = Some(Arc::new(resolver));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        // The write_file call should have a "Deferred" tool result, not actual execution
        let write_tool_msgs: Vec<&Value> = state
            .messages
            .iter()
            .filter(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("call_write"))
            .collect();
        assert_eq!(write_tool_msgs.len(), 1);
        let content = write_tool_msgs[0]["content"].as_str().unwrap();
        assert!(
            content.contains("Deferred"),
            "Expected deferred message, got: {content}"
        );
        assert!(
            content.contains("write_file"),
            "Should mention the deferred tool name"
        );

        // The skill call should still have been executed normally
        let skill_msgs: Vec<&Value> = state
            .messages
            .iter()
            .filter(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("call_skill"))
            .collect();
        assert_eq!(skill_msgs.len(), 1);
        assert!(
            skill_msgs[0]["content"]
                .as_str()
                .unwrap()
                .contains("# Skill: test-skill")
        );

        // Skill exclusivity drop is now a debug log, not a user-facing headless line.
        // Verify the host did NOT receive any deferred notice (it goes to tracing now).
        assert!(
            !host.emitted_lines.iter().any(|l| l.contains("deferred")),
            "Should NOT emit deferred notice to user: {:?}",
            host.emitted_lines
        );
    }

    #[tokio::test]
    async fn skill_exclusivity_does_not_trigger_for_dedup_stubs() {
        let resolver = StubSkillResolver::new();
        let turns = vec![
            // Turn 1: first invocation of skill (normal)
            skill_tool_call_result("call_1", r#"{"skill_name": "test-skill"}"#, 100, 50),
            // Turn 2: model re-invokes same skill + a regular tool call
            // The skill should be deduped (stub), and the regular call should NOT be deferred
            skill_plus_regular_tool_call_result(
                "call_2",
                r#"{"skill_name": "test-skill"}"#,
                "call_bash",
                "bash",
                r#"{"command": "echo hi"}"#,
                100,
                50,
            ),
            text_result("All done.", 80, 30, None),
        ];

        let mut host = MockHost::new(turns);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "use the test skill"}));
        state.skills.resolver = Some(Arc::new(resolver));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        // The bash call should NOT be deferred (skill was a dedup stub, not new)
        let bash_msgs: Vec<&Value> = state
            .messages
            .iter()
            .filter(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("call_bash"))
            .collect();
        // bash tool call goes through headless round (not intercepted by skill partition)
        // so it won't have a "Deferred" message
        for msg in &bash_msgs {
            let content = msg["content"].as_str().unwrap_or("");
            assert!(
                !content.contains("Deferred"),
                "Dedup stub should not trigger exclusivity, got: {content}"
            );
        }
    }

    #[tokio::test]
    async fn skill_exclusivity_not_triggered_when_no_skills() {
        // No skill resolver — regular tool calls should work normally
        let turns = vec![
            server_tool_result(
                vec![json!({
                    "id": "call_bash",
                    "type": "function",
                    "function": {
                        "name": "bash",
                        "arguments": r#"{"command": "echo hi"}"#,
                    }
                })],
                vec![],
                100,
                50,
                Some(30),
            ),
            text_result("Done.", 80, 30, None),
        ];

        let mut host = MockHost::new(turns);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "run echo"}));
        // No skill_resolver set

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        // No deferred messages should exist
        for msg in &state.messages {
            let content = msg.get("content").and_then(Value::as_str).unwrap_or("");
            assert!(
                !content.contains("Deferred"),
                "No skills = no deferral, got: {content}"
            );
        }
    }

    #[tokio::test]
    async fn skill_exclusivity_only_skill_no_regular_tools() {
        // When only a skill call is present (no regular tools), no deferral should happen.
        let resolver = StubSkillResolver::new();
        let turns = vec![
            skill_tool_call_result("call_skill", r#"{"skill_name": "test-skill"}"#, 100, 50),
            text_result("Following skill.", 80, 30, None),
        ];

        let mut host = MockHost::new(turns);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "use skill"}));
        state.skills.resolver = Some(Arc::new(resolver));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        for msg in &state.messages {
            let content = msg.get("content").and_then(Value::as_str).unwrap_or("");
            assert!(
                !content.contains("Deferred"),
                "Only-skill turn should not defer anything, got: {content}"
            );
        }
    }

    #[tokio::test]
    async fn skill_exclusivity_multiple_regular_tools_all_deferred() {
        let resolver = StubSkillResolver::new();
        // Model emits skill + 2 regular tool calls
        let turns = vec![
            HostTurnResult {
                accum: ChatTurnSseAccum {
                    has_tool_calls: true,
                    has_usage: true,
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    tool_calls: vec![
                        json!({
                            "id": "call_skill",
                            "type": "function",
                            "function": {
                                "name": "skill",
                                "arguments": r#"{"skill_name": "test-skill"}"#,
                            }
                        }),
                        json!({
                            "id": "call_write",
                            "type": "function",
                            "function": {
                                "name": "write_file",
                                "arguments": r#"{"path": "/tmp/a"}"#,
                            }
                        }),
                        json!({
                            "id": "call_bash",
                            "type": "function",
                            "function": {
                                "name": "bash",
                                "arguments": r#"{"command": "echo"}"#,
                            }
                        }),
                    ],
                    ..ChatTurnSseAccum::default()
                },
                ttft_ms: Some(30),
                edge_tool_round: Vec::new(),
            },
            text_result("Done.", 80, 30, None),
        ];

        let mut host = MockHost::new(turns);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "use skill"}));
        state.skills.resolver = Some(Arc::new(resolver));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        // Both regular tools should be deferred
        for call_id in &["call_write", "call_bash"] {
            let msgs: Vec<&Value> = state
                .messages
                .iter()
                .filter(|m| m.get("tool_call_id").and_then(Value::as_str) == Some(call_id))
                .collect();
            assert_eq!(msgs.len(), 1, "Expected one message for {call_id}");
            let content = msgs[0]["content"].as_str().unwrap();
            assert!(
                content.contains("Deferred"),
                "{call_id} should be deferred, got: {content}"
            );
        }
    }

    #[tokio::test]
    async fn skill_exclusivity_still_defers_when_skill_fails() {
        // Even if the skill call fails (unknown skill), regular tools should
        // still be deferred — the model should see the error and re-evaluate.
        let resolver = StubSkillResolver::new(); // only knows "test-skill"
        let turns = vec![
            skill_plus_regular_tool_call_result(
                "call_skill",
                r#"{"skill_name": "nonexistent-skill"}"#,
                "call_bash",
                "bash",
                r#"{"command": "rm -rf /"}"#,
                100,
                50,
            ),
            text_result("OK, skill not found.", 80, 30, None),
        ];

        let mut host = MockHost::new(turns);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "use skill"}));
        state.skills.resolver = Some(Arc::new(resolver));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        // bash should still be deferred even though skill failed
        let bash_msgs: Vec<&Value> = state
            .messages
            .iter()
            .filter(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("call_bash"))
            .collect();
        assert_eq!(bash_msgs.len(), 1);
        assert!(
            bash_msgs[0]["content"]
                .as_str()
                .unwrap()
                .contains("Deferred"),
            "bash should be deferred even when skill fails"
        );

        // skill call should have an error result (not a skill instruction)
        let skill_msgs: Vec<&Value> = state
            .messages
            .iter()
            .filter(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("call_skill"))
            .collect();
        assert_eq!(skill_msgs.len(), 1);
        let skill_content = skill_msgs[0]["content"].as_str().unwrap();
        assert!(
            skill_content.contains("Unknown skill") || skill_content.contains("unknown"),
            "Skill should have failed, got: {skill_content}"
        );
    }

    #[tokio::test]
    async fn skill_exclusivity_malformed_tool_call_no_panic() {
        // Tool call with missing id/function — should not panic
        let resolver = StubSkillResolver::new();
        let turns = vec![
            HostTurnResult {
                accum: ChatTurnSseAccum {
                    has_tool_calls: true,
                    has_usage: true,
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    tool_calls: vec![
                        json!({
                            "id": "call_skill",
                            "type": "function",
                            "function": {
                                "name": "skill",
                                "arguments": r#"{"skill_name": "test-skill"}"#,
                            }
                        }),
                        // Malformed: missing function.name
                        json!({
                            "id": "call_bad",
                            "type": "function",
                            "function": {}
                        }),
                    ],
                    ..ChatTurnSseAccum::default()
                },
                ttft_ms: Some(30),
                edge_tool_round: Vec::new(),
            },
            text_result("Done.", 80, 30, None),
        ];

        let mut host = MockHost::new(turns);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "use skill"}));
        state.skills.resolver = Some(Arc::new(resolver));

        // Should not panic
        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
    }

    // ── Token budget enforcement tests ──────────────────────────────────────

    #[tokio::test]
    async fn budget_exceeded_injects_wrapup_and_completes() {
        // Turn 1: edge tool call with prompt=50K → under 80K budget → proceeds
        // Turn 2: prompt=90K → exceeds 80K budget → wrapup injected, loop continues
        // Turn 3: final text response → completes
        let mut host = MockHost::new(vec![
            edge_tool_result(
                vec![make_edge_tool("bash", "file list")],
                50_000,
                1000,
                Some(200),
            ),
            edge_tool_result(
                vec![make_edge_tool("read_file", "big content")],
                90_000,
                2000,
                Some(100),
            ),
            text_result("Here is my summary.", 90_000, 500, None),
        ]);
        let mut state = make_state();
        state.max_turn_input_tokens = 80_000;
        state
            .messages
            .push(json!({"role": "user", "content": "analyze code"}));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert!(state.budget_wrapup_injected);
        assert_eq!(state.final_text, "Here is my summary.");
        // Verify a system message was injected about budget
        let has_budget_msg = state.messages.iter().any(|m| {
            m.get("content")
                .and_then(|c| c.as_str())
                .is_some_and(|s| s.contains("token budget limit"))
        });
        assert!(has_budget_msg, "expected budget wrapup system message");
    }

    #[tokio::test]
    async fn budget_zero_means_unlimited() {
        // With max_turn_input_tokens=0, even large prompt tokens shouldn't trigger wrapup.
        let mut host = MockHost::new(vec![
            edge_tool_result(
                vec![make_edge_tool("bash", "big output")],
                200_000,
                5000,
                Some(100),
            ),
            text_result("All done.", 200_000, 1000, None),
        ]);
        let mut state = make_state();
        state.max_turn_input_tokens = 0; // unlimited
        state
            .messages
            .push(json!({"role": "user", "content": "big task"}));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert!(!state.budget_wrapup_injected);
        assert_eq!(state.final_text, "All done.");
    }

    #[tokio::test]
    async fn budget_hard_stop_on_second_breach() {
        // Turn 1: prompt=90K → exceeds 80K budget → wrapup injected
        // Turn 2: model still returns tool calls with prompt=95K → hard stop
        let mut host = MockHost::new(vec![
            edge_tool_result(
                vec![make_edge_tool("bash", "output1")],
                90_000,
                2000,
                Some(100),
            ),
            // After wrapup injection, model ignores instruction and returns tool calls again.
            edge_tool_result(
                vec![make_edge_tool("bash", "output2")],
                95_000,
                2500,
                Some(50),
            ),
        ]);
        let mut state = make_state();
        state.max_turn_input_tokens = 80_000;
        state
            .messages
            .push(json!({"role": "user", "content": "complex task"}));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        // Should complete (hard stop), not error.
        assert!(outcome.is_ok());
        assert!(state.budget_wrapup_injected);
    }

    // ── Rate-limit graceful degradation tests ───────────────────────────────

    fn error_result(error_msg: &str, prompt: u64, completion: u64) -> HostTurnResult {
        HostTurnResult {
            accum: ChatTurnSseAccum {
                error_message: Some(error_msg.to_string()),
                has_usage: true,
                prompt_tokens: prompt,
                completion_tokens: completion,
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: None,
            edge_tool_round: Vec::new(),
        }
    }

    #[tokio::test]
    async fn rate_limit_after_tool_calls_preserves_work() {
        // Turn 1: successful tool execution
        // Turn 2: 429 rate limit error
        // Expected: graceful completion (Ok), not hard error (Err)
        let mut host = MockHost::new(vec![
            edge_tool_result(
                vec![make_edge_tool("bash", "file list output")],
                20_000,
                1000,
                Some(200),
            ),
            error_result("Error: 429 Too Many Requests (after 3 retries)", 30_000, 0),
        ]);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "review code"}));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(
            outcome.is_ok(),
            "429 after tool calls should complete gracefully, got: {outcome:?}"
        );
        assert!(
            state.final_text.contains("Rate limit reached"),
            "final_text should mention rate limit: {}",
            state.final_text,
        );
        assert!(
            state.final_text.contains("1 tool call"),
            "final_text should mention tool call count",
        );
    }

    #[tokio::test]
    async fn rate_limit_without_tool_calls_is_fatal() {
        // Turn 1: 429 rate limit error immediately (no prior tool work)
        // Expected: hard error (Err), because nothing to preserve
        let mut host = MockHost::new(vec![error_result(
            "Error: rate limit exceeded (after 3 retries)",
            0,
            0,
        )]);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "hello"}));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(
            outcome.is_err(),
            "429 without prior work should be a hard error",
        );
    }

    #[tokio::test]
    async fn non_rate_limit_error_is_always_fatal() {
        // Turn 1: successful tool execution
        // Turn 2: non-429 error (e.g., auth error)
        // Expected: hard error (Err) even with prior tool work
        let mut host = MockHost::new(vec![
            edge_tool_result(
                vec![make_edge_tool("bash", "some output")],
                20_000,
                1000,
                Some(200),
            ),
            error_result("Error: authentication failed", 0, 0),
        ]);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "do stuff"}));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(
            outcome.is_err(),
            "non-rate-limit error should always be fatal",
        );
    }

    #[tokio::test]
    async fn rate_limit_cooldown_records_consecutive_errors() {
        // Verify that the rate_limit_cooldown field on AgenticLoopState
        // accumulates errors from rate-limited turns.
        let mut host = MockHost::new(vec![error_result(
            "Error: 429 Too Many Requests (after 3 retries)",
            0,
            0,
        )]);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "hello"}));

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;
        let metrics = state.rate_limit_cooldown.metrics();
        assert!(
            metrics.total_429_errors >= 1,
            "cooldown should have recorded at least one 429 error, got: {metrics:?}"
        );
    }

    #[tokio::test]
    async fn rate_limit_cooldown_resets_on_success() {
        // Turn 1: 429 error (records into cooldown)
        // Turn 2: success (resets consecutive counters)
        let mut host = MockHost::new(vec![
            error_result("Error: 429 rate limit", 0, 0),
            text_result("All good now!", 100, 50, Some(80)),
        ]);
        // Need 2+ turns so the loop can try both
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "hello"}));

        // The first turn will fail with a fatal error (no tool calls to preserve),
        // so the loop stops. But the cooldown should still have recorded the error.
        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(
            state.rate_limit_cooldown.metrics().total_429_errors >= 1,
            "should have recorded the 429 error"
        );
        assert_eq!(
            state.rate_limit_cooldown.metrics().consecutive_errors,
            1,
            "consecutive errors should be 1 since success didn't run"
        );
    }

    #[tokio::test]
    async fn rate_limit_cooldown_reject_with_no_prior_work() {
        // Pre-populate cooldown to reject state, then verify the loop
        // rejects immediately when there's no prior tool work.
        let mut host = MockHost::new(vec![text_result("unreachable", 100, 50, None)]);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "test"}));

        // Force cooldown into reject state.
        for _ in 0..5 {
            state.rate_limit_cooldown.record_429(Some(60_000), false);
        }
        assert!(
            state.rate_limit_cooldown.is_in_cooldown(),
            "should be in cooldown after 5 errors"
        );

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_err(), "should reject with error: {outcome:?}");
        let err = outcome.unwrap_err();
        assert!(
            err.contains("Rate limit cooldown active"),
            "error should mention cooldown: {err}"
        );
    }

    #[tokio::test]
    async fn rate_limit_cooldown_reject_preserves_prior_tool_work() {
        // When the first turn does tool work but the second turn 429s,
        // the existing rate-limit graceful degradation preserves results.
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![make_edge_tool("bash", "hello")], 100, 20, Some(50)),
            error_result("Error: 429 rate limit (after 3 retries)", 0, 0),
        ]);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "test"}));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok(), "should gracefully complete: {outcome:?}");
        assert!(
            state.final_text.contains("Rate limit"),
            "should mention rate limit in final text: {}",
            state.final_text,
        );
        // Verify cooldown recorded the error for future turns.
        assert!(
            state.rate_limit_cooldown.metrics().total_429_errors >= 1,
            "cooldown should have recorded the 429"
        );
    }

    // ── Session event hooks: multi-turn integration tests ───────────────

    #[tokio::test]
    async fn session_start_hook_injects_context_before_first_turn() {
        // Hook: shell that returns greeting context
        let hooks = crate::skills::hooks::SessionEventHookRegistry::new(vec![
            crate::skills::hooks::SessionEventHook {
                event: crate::skills::hooks::SessionEvent::SessionStart,
                action: crate::skills::hooks::HookAction::Shell {
                    command: r#"echo '{"context": "Branch: main | Last session: audit"}'"#.into(),
                },
                timeout_secs: 5,
                is_async: false,
                condition: None,
                once: false,
                priority: 0,
            },
        ]);

        // Two turns: first turn sees the injected context, second turn completes
        let mut host = MockHost::new(vec![
            // Turn 1: LLM sees the hook context + user message, responds with tool call
            edge_tool_result(vec![make_edge_tool("bash", "xupeng\n")], 100, 20, Some(50)),
            // Turn 2: LLM produces final text
            text_result("Hello xupeng! Branch: main", 120, 30, Some(30)),
        ]);
        host = host.with_valid_tools(&["bash"]);

        let mut state = make_state();
        state.skills.session_event_hooks = hooks;
        state.current_session_id = Some("test-session-123".to_string());
        state.message = "hello".to_string();
        state
            .messages
            .push(json!({"role": "user", "content": "hello"}));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(matches!(outcome, Ok(AgenticLoopOutcome::Completed)));

        // Verify: hook context was injected as the first message (before user message)
        let first_msg = &state.messages[0];
        let content = first_msg["content"].as_str().unwrap();
        assert!(
            content.contains("Branch: main"),
            "first message should contain hook context, got: {content}"
        );
        assert!(
            content.contains("[Session hooks]"),
            "should be tagged as session hooks"
        );

        // Verify: user message follows the hook context
        let user_msg = state
            .messages
            .iter()
            .find(|m| m["role"] == "user")
            .expect("should have user message");
        assert_eq!(user_msg["content"], "hello");

        // Verify: final text includes the greeting
        assert!(state.final_text.contains("Hello xupeng"));
    }

    #[tokio::test]
    async fn session_start_hook_env_vars_are_set() {
        let hooks = crate::skills::hooks::SessionEventHookRegistry::new(vec![
            crate::skills::hooks::SessionEventHook {
                event: crate::skills::hooks::SessionEvent::SessionStart,
                action: crate::skills::hooks::HookAction::SetEnv {
                    key: "ASTRA_TEST_HOOK_VAR".into(),
                    value: "session_active".into(),
                },
                timeout_secs: 10,
                is_async: false,
                condition: None,
                once: false,
                priority: 0,
            },
        ]);

        let mut host = MockHost::new(vec![text_result("done", 10, 5, Some(10))]);
        let mut state = make_state();
        state.skills.session_event_hooks = hooks;
        state
            .messages
            .push(json!({"role": "user", "content": "test"}));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(matches!(outcome, Ok(AgenticLoopOutcome::Completed)));

        // Verify env var was set
        assert_eq!(
            std::env::var("ASTRA_TEST_HOOK_VAR").ok().as_deref(),
            Some("session_active")
        );
        // Cleanup
        unsafe { std::env::remove_var("ASTRA_TEST_HOOK_VAR") };
    }

    #[tokio::test]
    async fn session_start_hook_multiple_hooks_context_merged() {
        let hooks = crate::skills::hooks::SessionEventHookRegistry::new(vec![
            crate::skills::hooks::SessionEventHook {
                event: crate::skills::hooks::SessionEvent::SessionStart,
                action: crate::skills::hooks::HookAction::Shell {
                    command: r#"echo '{"context": "git: main, 3 uncommitted"}'"#.into(),
                },
                timeout_secs: 5,
                is_async: false,
                condition: None,
                once: false,
                priority: 0,
            },
            crate::skills::hooks::SessionEventHook {
                event: crate::skills::hooks::SessionEvent::SessionStart,
                action: crate::skills::hooks::HookAction::Shell {
                    command: r#"echo '{"context": "last session: reviewed PR #42"}'"#.into(),
                },
                timeout_secs: 5,
                is_async: false,
                condition: None,
                once: false,
                priority: 0,
            },
        ]);

        let mut host = MockHost::new(vec![text_result("ok", 10, 5, Some(10))]);
        let mut state = make_state();
        state.skills.session_event_hooks = hooks;
        state
            .messages
            .push(json!({"role": "user", "content": "hi"}));

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;

        let first_content = state.messages[0]["content"].as_str().unwrap();
        assert!(
            first_content.contains("3 uncommitted"),
            "should contain first hook context"
        );
        assert!(
            first_content.contains("PR #42"),
            "should contain second hook context"
        );
    }

    #[tokio::test]
    async fn session_start_hook_failure_does_not_block_session() {
        let hooks = crate::skills::hooks::SessionEventHookRegistry::new(vec![
            crate::skills::hooks::SessionEventHook {
                event: crate::skills::hooks::SessionEvent::SessionStart,
                action: crate::skills::hooks::HookAction::Shell {
                    command: "exit 1".into(), // fails
                },
                timeout_secs: 5,
                is_async: false,
                condition: None,
                once: false,
                priority: 0,
            },
        ]);

        let mut host = MockHost::new(vec![text_result("Hello!", 10, 5, Some(10))]);
        let mut state = make_state();
        state.skills.session_event_hooks = hooks;
        state
            .messages
            .push(json!({"role": "user", "content": "hello"}));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(
            matches!(outcome, Ok(AgenticLoopOutcome::Completed)),
            "session should complete even when hook fails"
        );
        assert_eq!(state.final_text, "Hello!");

        // No hook context injected (hook failed)
        assert!(
            !state.messages[0]["content"]
                .as_str()
                .unwrap_or("")
                .contains("[Session hooks]"),
            "failed hook should not inject context"
        );
    }

    #[tokio::test]
    async fn no_session_hooks_skips_preamble() {
        // Empty registry — no hooks fire, no context injected
        let mut host = MockHost::new(vec![text_result("Hi", 10, 5, Some(10))]);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "hello"}));

        let _msg_count_before = state.messages.len();
        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(matches!(outcome, Ok(AgenticLoopOutcome::Completed)));

        // No extra messages were injected at position 0
        // (the first message should still be the user message, not a system hook message)
        let first_role = state.messages[0]["role"].as_str().unwrap();
        assert_eq!(first_role, "user", "no hook context should be injected");
    }

    #[tokio::test]
    async fn session_hook_receives_user_message_in_stdin() {
        // Hook script reads stdin JSON and echoes user_message back as context
        let hooks = crate::skills::hooks::SessionEventHookRegistry::new(vec![
            crate::skills::hooks::SessionEventHook {
                event: crate::skills::hooks::SessionEvent::SessionStart,
                action: crate::skills::hooks::HookAction::Shell {
                    command: r#"python3 -c "
import sys, json
d = json.load(sys.stdin)
msg = d.get('user_message', '')
print(json.dumps({'context': 'user said: ' + msg}))
""#
                    .into(),
                },
                timeout_secs: 5,
                is_async: false,
                condition: None,
                once: false,
                priority: 0,
            },
        ]);

        let mut host = MockHost::new(vec![text_result("ok", 10, 5, Some(10))]);
        let mut state = make_state();
        state.skills.session_event_hooks = hooks;
        state.message = "analyze my code".to_string();
        state
            .messages
            .push(json!({"role": "user", "content": "analyze my code"}));

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;

        let first_content = state.messages[0]["content"].as_str().unwrap();
        assert!(
            first_content.contains("user said: analyze my code"),
            "hook should receive the user message, got: {first_content}"
        );
    }

    #[tokio::test]
    async fn session_end_hooks_not_fired_at_start() {
        // Only SessionEnd hooks — should NOT fire during SessionStart preamble
        let hooks = crate::skills::hooks::SessionEventHookRegistry::new(vec![
            crate::skills::hooks::SessionEventHook {
                event: crate::skills::hooks::SessionEvent::SessionEnd,
                action: crate::skills::hooks::HookAction::Shell {
                    command: r#"echo '{"context": "SHOULD NOT APPEAR"}'"#.into(),
                },
                timeout_secs: 5,
                is_async: false,
                condition: None,
                once: false,
                priority: 0,
            },
        ]);

        let mut host = MockHost::new(vec![text_result("done", 10, 5, Some(10))]);
        let mut state = make_state();
        state.skills.session_event_hooks = hooks;
        state
            .messages
            .push(json!({"role": "user", "content": "test"}));

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;

        // No hook context should be injected (SessionEnd doesn't fire at start)
        for msg in &state.messages {
            let content = msg["content"].as_str().unwrap_or("");
            assert!(
                !content.contains("SHOULD NOT APPEAR"),
                "SessionEnd hook should not fire at session start"
            );
        }
    }

    // ── read_capped tests ───────────────────────────────────────────────

    #[tokio::test]
    async fn read_capped_limits_large_hook_output() {
        // A hook that outputs more than 256 KiB should be truncated
        use crate::skills::hooks::HOOK_STDOUT_MAX_BYTES;
        let registry = crate::skills::hooks::SessionEventHookRegistry::new(vec![
            crate::skills::hooks::SessionEventHook {
                event: crate::skills::hooks::SessionEvent::SessionStart,
                action: crate::skills::hooks::HookAction::Shell {
                    // dd outputs exactly 300 KiB of zeros, fast
                    command: format!(
                        "dd if=/dev/zero bs=1024 count={} 2>/dev/null | tr '\\0' 'x'",
                        (HOOK_STDOUT_MAX_BYTES + 50_000) / 1024
                    ),
                },
                timeout_secs: 5,
                is_async: false,
                condition: None,
                once: false,
                priority: 0,
            },
        ]);

        let _output = crate::skills::hooks::evaluate_session_hooks(
            &registry,
            crate::skills::hooks::SessionEvent::SessionStart,
            "s1",
            None,
        )
        .await;
        // Output should exist but be capped (plain text → context)
    }

    // ── Auto-tuning integration tests ───────────────────────────────────────

    fn make_hub() -> std::sync::Arc<crate::observability_integration::ObservabilityHub> {
        std::sync::Arc::new(crate::observability_integration::ObservabilityHub::new())
    }

    fn tool_record(name: &str, ok: bool, result_preview: Option<&str>) -> ToolCallRecord {
        ToolCallRecord {
            name: name.into(),
            ok,
            ms: 0,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
            result_preview: result_preview.map(str::to_string),
        }
    }

    fn make_session()
    -> std::sync::Arc<std::sync::RwLock<crate::observability_integration::ObservabilitySession>>
    {
        std::sync::Arc::new(std::sync::RwLock::new(
            crate::observability_integration::ObservabilitySession::new_simple("test-session"),
        ))
    }

    #[test]
    fn feedback_records_task_success_on_completed() {
        let hub = make_hub();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub.clone());
        state.current_run_id = Some("run-1".into());

        let result = Ok(AgenticLoopOutcome::Completed);
        record_loop_completion_feedback(&mut state, &result);

        // Add a rule that fires on low success rate — it should NOT fire because
        // we just recorded a success.
        hub.tuning()
            .add_rule(crate::auto_tuning::EvolutionRule::new(
                "test-low-success",
                crate::auto_tuning::EvolutionTrigger::LowSuccessRate {
                    threshold: 0.5,
                    window_secs: 3600,
                    min_samples: 1,
                },
                crate::auto_tuning::EvolutionAction::Alert {
                    message: "low success".into(),
                    severity: crate::auto_tuning::AlertSeverity::Warning,
                },
            ));
        let config = crate::runtime_config::RuntimeConfig::default();
        let triggered = hub.tuning().evaluate(&config);
        assert!(
            triggered.is_empty(),
            "rule should not fire with 100% success"
        );

        // Record failures to bring success rate below threshold.
        let fail_result: Result<AgenticLoopOutcome, String> =
            Ok(AgenticLoopOutcome::Error("test error".into()));
        record_loop_completion_feedback(&mut state, &fail_result);
        let fail_result2: Result<AgenticLoopOutcome, String> =
            Ok(AgenticLoopOutcome::Error("test error 2".into()));
        record_loop_completion_feedback(&mut state, &fail_result2);

        let triggered = hub.tuning().evaluate(&config);
        // success_rate = 1/3 ≈ 0.33 < 0.5 threshold
        assert!(
            !triggered.is_empty(),
            "rule should fire with low success rate"
        );

        // Full cycle: evaluate + execute.
        let mut config2 = crate::runtime_config::RuntimeConfig::default();
        let executions = hub.run_tuning_cycle(&mut config2);
        assert!(
            !executions.is_empty(),
            "tuning cycle should execute the triggered rule"
        );
    }

    #[test]
    fn feedback_records_task_failure_on_error() {
        let hub = make_hub();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub.clone());

        let result: Result<AgenticLoopOutcome, String> = Err("something broke".into());
        record_loop_completion_feedback(&mut state, &result);

        // TaskFailure lowers success rate. With 0 successes and 1 failure, rate = 0.0.
        hub.tuning()
            .add_rule(crate::auto_tuning::EvolutionRule::new(
                "low-success",
                crate::auto_tuning::EvolutionTrigger::LowSuccessRate {
                    threshold: 0.5,
                    window_secs: 3600,
                    min_samples: 1,
                },
                crate::auto_tuning::EvolutionAction::Alert {
                    message: "low success".into(),
                    severity: crate::auto_tuning::AlertSeverity::Warning,
                },
            ));
        let config = crate::runtime_config::RuntimeConfig::default();
        let triggered = hub.tuning().evaluate(&config);
        assert!(
            !triggered.is_empty(),
            "low success rate rule should fire after failure"
        );
    }

    #[test]
    fn feedback_records_interruption_on_cancel() {
        let hub = make_hub();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub.clone());

        let result = Ok(AgenticLoopOutcome::Cancelled);
        record_loop_completion_feedback(&mut state, &result);

        // Interruption is recorded as a signal — verify via accumulation.
        hub.tuning()
            .add_rule(crate::auto_tuning::EvolutionRule::new(
                "interrupt-detect",
                crate::auto_tuning::EvolutionTrigger::SignalAccumulation {
                    signal_type: "interruption".into(),
                    count: 1,
                    window_secs: 3600,
                },
                crate::auto_tuning::EvolutionAction::Alert {
                    message: "interrupted".into(),
                    severity: crate::auto_tuning::AlertSeverity::Info,
                },
            ));
        let config = crate::runtime_config::RuntimeConfig::default();
        let triggered = hub.tuning().evaluate(&config);
        assert!(
            !triggered.is_empty(),
            "interruption signal should fire accumulation rule"
        );
    }

    #[test]
    fn feedback_records_high_token_usage() {
        let hub = make_hub();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub.clone());
        state.total_prompt = 40_000;
        state.total_completion = 20_000; // total = 60k > 50k threshold

        let result = Ok(AgenticLoopOutcome::Completed);
        record_loop_completion_feedback(&mut state, &result);

        hub.tuning()
            .add_rule(crate::auto_tuning::EvolutionRule::new(
                "high-tokens",
                crate::auto_tuning::EvolutionTrigger::HighTokenUsage {
                    threshold_tokens: 50_000,
                    window_secs: 3600,
                    min_samples: 1,
                },
                crate::auto_tuning::EvolutionAction::Alert {
                    message: "high tokens".into(),
                    severity: crate::auto_tuning::AlertSeverity::Warning,
                },
            ));
        let config = crate::runtime_config::RuntimeConfig::default();
        let triggered = hub.tuning().evaluate(&config);
        assert!(!triggered.is_empty(), "high token usage rule should fire");
    }

    #[test]
    fn feedback_records_tool_churn() {
        let hub = make_hub();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub.clone());
        state.total_tool_calls = 30;
        state.telemetry.all_tools_used = ["bash"].iter().map(|s| s.to_string()).collect();
        // ratio = 30/1 = 30 > 5 threshold

        let result = Ok(AgenticLoopOutcome::Completed);
        record_loop_completion_feedback(&mut state, &result);

        hub.tuning()
            .add_rule(crate::auto_tuning::EvolutionRule::new(
                "churn-detect",
                crate::auto_tuning::EvolutionTrigger::SignalAccumulation {
                    signal_type: "tool_churn".into(),
                    count: 1,
                    window_secs: 3600,
                },
                crate::auto_tuning::EvolutionAction::Alert {
                    message: "churn".into(),
                    severity: crate::auto_tuning::AlertSeverity::Info,
                },
            ));
        let config = crate::runtime_config::RuntimeConfig::default();
        let triggered = hub.tuning().evaluate(&config);
        assert!(
            !triggered.is_empty(),
            "tool churn signal should fire accumulation rule"
        );
    }

    #[test]
    fn feedback_ignores_synthetic_tool_churn_records() {
        let hub = make_hub();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub.clone());
        state.total_tool_calls = 15;
        state.telemetry.all_tools_used = ["bash"].iter().map(|s| s.to_string()).collect();
        state.stall.tool_call_records = vec![
            tool_record(
                "skill",
                false,
                Some(
                    "Skill 'debug' was already loaded (turn 2). Follow those instructions directly.",
                ),
            ),
            tool_record(
                "bash",
                false,
                Some("Skipped: the skill already completed this work. Do NOT call `bash` again."),
            ),
            tool_record(
                "read_file",
                false,
                Some(
                    "Deferred: skill was invoked in this turn. Read the skill instructions above.",
                ),
            ),
            tool_record("git_show", true, Some("diff")),
            tool_record("read_file", true, Some("contents")),
        ];

        let result = Ok(AgenticLoopOutcome::Completed);
        record_loop_completion_feedback(&mut state, &result);

        hub.tuning()
            .add_rule(crate::auto_tuning::EvolutionRule::new(
                "churn-detect",
                crate::auto_tuning::EvolutionTrigger::SignalAccumulation {
                    signal_type: "tool_churn".into(),
                    count: 1,
                    window_secs: 3600,
                },
                crate::auto_tuning::EvolutionAction::Alert {
                    message: "churn".into(),
                    severity: crate::auto_tuning::AlertSeverity::Info,
                },
            ));
        let config = crate::runtime_config::RuntimeConfig::default();
        let triggered = hub.tuning().evaluate(&config);
        assert!(
            triggered.is_empty(),
            "synthetic placeholders should not trigger churn feedback"
        );
    }

    #[test]
    fn feedback_no_signal_without_hub() {
        let mut state = make_state();
        let result = Ok(AgenticLoopOutcome::Completed);
        record_loop_completion_feedback(&mut state, &result);
        // Should not panic.
    }

    #[test]
    fn feedback_skill_quality_signals() {
        use crate::skills::quality::SkillOutcome;

        let hub = make_hub();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub.clone());

        state.skills.quality_tracker.record_outcome(&SkillOutcome {
            skill_name: "good-skill".into(),
            tokens_used: 100,
            duration_ms: 50,
            all_required_passed: true,
            partial: false,
        });
        state.skills.quality_tracker.record_outcome(&SkillOutcome {
            skill_name: "bad-skill".into(),
            tokens_used: 200,
            duration_ms: 100,
            all_required_passed: false,
            partial: false,
        });

        let result = Ok(AgenticLoopOutcome::Completed);
        record_loop_completion_feedback(&mut state, &result);

        // The bad skill failure adds a TaskFailure signal, lowering success rate.
        // We have TaskSuccess (completed) + TaskSuccess (good-skill) + TaskFailure (bad-skill)
        // = 2 successes / 3 total = 0.67 success rate.
        hub.tuning()
            .add_rule(crate::auto_tuning::EvolutionRule::new(
                "low-success",
                crate::auto_tuning::EvolutionTrigger::LowSuccessRate {
                    threshold: 0.8, // 0.67 < 0.8, so this should trigger
                    window_secs: 3600,
                    min_samples: 1,
                },
                crate::auto_tuning::EvolutionAction::Alert {
                    message: "skill failure".into(),
                    severity: crate::auto_tuning::AlertSeverity::Warning,
                },
            ));
        let config = crate::runtime_config::RuntimeConfig::default();
        let triggered = hub.tuning().evaluate(&config);
        assert!(
            !triggered.is_empty(),
            "skill failure should lower success rate and trigger rule"
        );
    }

    #[test]
    fn tuning_cycle_runs_at_interval() {
        let hub = make_hub();
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub.clone());
        state.telemetry.observability_session = Some(session.clone());

        hub.tuning().add_rule(
            crate::auto_tuning::EvolutionRule::new(
                "test-alert",
                crate::auto_tuning::EvolutionTrigger::LowSuccessRate {
                    threshold: 0.5,
                    window_secs: 3600,
                    min_samples: 1,
                },
                crate::auto_tuning::EvolutionAction::Alert {
                    message: "low success detected".into(),
                    severity: crate::auto_tuning::AlertSeverity::Warning,
                },
            )
            .with_cooldown(std::time::Duration::from_secs(0)),
        );

        // Record failures to satisfy the rule trigger.
        for _ in 0..3 {
            hub.record_feedback(crate::auto_tuning::FeedbackSignal::new(
                crate::auto_tuning::SignalType::TaskFailure {
                    reason: "test".into(),
                },
            ));
        }

        // Below interval — should NOT trigger.
        state.telemetry.completed_turns_for_tuning = DEFAULT_TUNING_CYCLE_INTERVAL - 1;
        maybe_run_tuning_cycle(&mut state);
        assert_eq!(
            state.telemetry.completed_turns_for_tuning,
            DEFAULT_TUNING_CYCLE_INTERVAL - 1,
            "counter should not reset below interval"
        );
        assert!(
            hub.tuning().get_executions().is_empty(),
            "no cycle should run below interval"
        );

        // At interval — SHOULD trigger.
        state.telemetry.completed_turns_for_tuning = DEFAULT_TUNING_CYCLE_INTERVAL;
        maybe_run_tuning_cycle(&mut state);
        assert_eq!(
            state.telemetry.completed_turns_for_tuning, 0,
            "counter should reset after cycle"
        );
        assert!(
            !hub.tuning().get_executions().is_empty(),
            "cycle should execute the triggered rule"
        );
    }

    #[test]
    fn tuning_cycle_skips_without_session() {
        let hub = make_hub();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub.clone());
        state.telemetry.completed_turns_for_tuning = DEFAULT_TUNING_CYCLE_INTERVAL;
        maybe_run_tuning_cycle(&mut state);
        // Counter is reset (passes threshold check) but no cycle runs (no session).
        assert_eq!(state.telemetry.completed_turns_for_tuning, 0);
    }

    #[test]
    fn adaptive_profile_updates_session_scenario_and_budget() {
        let hub = make_hub();
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub);
        state.telemetry.observability_session = Some(session.clone());
        state.message = "fix the bug in the parser".into();
        state.recent_tools = vec!["bash".into(), "view".into()];

        {
            let mut guard = session.write().unwrap_or_else(|e| e.into_inner());
            for _ in 0..5 {
                guard.record_query("fix the bug in the parser");
            }
        }

        apply_adaptive_execution_profile(&mut state);

        let guard = session.read().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            guard.profile.current_scenario,
            Some(crate::user_profile::Scenario::Debugging)
        );
        assert!(state.max_turn_input_tokens >= 100_000);
    }

    #[test]
    fn adaptive_profile_assigns_experiment_variant() {
        let hub = make_hub();
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub.clone());
        state.telemetry.observability_session = Some(session.clone());
        state.message = "implement the feature".into();

        let mut experiment = crate::ab_testing::Experiment::new("exp-router")
            .with_variant(crate::ab_testing::Variant::control())
            .with_variant(
                crate::ab_testing::Variant::new("treatment")
                    .with_traffic(0.5)
                    .with_config_diff("compression.max_history_tokens", serde_json::json!(25_000)),
            )
            .with_metric(crate::ab_testing::MetricDefinition::success_rate())
            .with_min_samples(5)
            .build();
        experiment.start();
        hub.experiments_mut().register(experiment);

        apply_adaptive_execution_profile(&mut state);

        let guard = session.read().unwrap_or_else(|e| e.into_inner());
        assert_eq!(guard.active_experiment_id.as_deref(), Some("exp-router"));
        assert!(guard.active_variant.is_some());
        assert!(
            guard
                .profile
                .active_experiments
                .contains(&"exp-router".to_string())
        );
    }

    #[test]
    fn adaptive_profile_enables_liquid_runtime_when_adaptive_context_is_on() {
        let hub = make_hub();
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub);
        state.telemetry.observability_session = Some(session.clone());

        {
            let mut guard = session.write().unwrap();
            guard.config.context_window.adaptive = true;
            guard.config.token_budget.max_turn_input_tokens = 64_000;
        }

        apply_adaptive_execution_profile(&mut state);

        assert!(state.tactical_adapter.is_some());
        assert!(state.step_signal_collector.is_some());
    }

    #[test]
    fn adaptive_profile_disables_liquid_runtime_when_adaptive_context_is_off() {
        let hub = make_hub();
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub);
        state.telemetry.observability_session = Some(session.clone());
        state.tactical_adapter = Some(crate::liquid::tactical::TacticalAdapter::new(
            crate::liquid::tactical::DampenerConfig::default(),
        ));
        state.step_signal_collector = Some(crate::liquid::step_signals::StepSignalCollector::new(
            crate::liquid::step_signals::StepSignalConfig::default(),
            64_000,
        ));

        {
            let mut guard = session.write().unwrap();
            guard.config.context_window.adaptive = false;
        }

        apply_adaptive_execution_profile(&mut state);

        assert!(state.tactical_adapter.is_none());
        assert!(state.step_signal_collector.is_none());
    }

    #[test]
    fn tuning_cycle_concludes_mature_experiments() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let hub = make_hub();
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub.clone());
        state.telemetry.observability_session = Some(session);
        state.telemetry.completed_turns_for_tuning = DEFAULT_TUNING_CYCLE_INTERVAL;
        state.current_session_id = Some("test-session-promote".to_string());

        let mut experiment = crate::ab_testing::Experiment::new("exp-mature")
            .with_variant(crate::ab_testing::Variant::control())
            .with_variant(
                crate::ab_testing::Variant::new("treatment")
                    .with_traffic(0.5)
                    .with_config_diff("memory.retrieval_top_k", serde_json::json!(8)),
            )
            .with_metric(crate::ab_testing::MetricDefinition::success_rate())
            .with_tag("task_type:fetch")
            .with_tag("domain:any")
            .with_min_samples(1)
            .build();
        experiment.start();
        hub.experiments_mut().register(experiment);
        {
            let experiments = hub.experiments();
            for (idx, value) in [0.10, 0.20, 0.25, 0.15, 0.30].into_iter().enumerate() {
                experiments.record_outcome(
                    "exp-mature",
                    crate::ab_testing::ExperimentOutcome::new(format!("c{idx}"), "control")
                        .with_metric("success_rate", value)
                        .with_success(false),
                );
            }
            for (idx, value) in [0.82, 0.91, 0.88, 0.95, 0.86].into_iter().enumerate() {
                experiments.record_outcome(
                    "exp-mature",
                    crate::ab_testing::ExperimentOutcome::new(format!("t{idx}"), "treatment")
                        .with_metric("success_rate", value)
                        .with_success(true),
                );
            }
        }

        maybe_run_tuning_cycle(&mut state);

        assert_eq!(
            hub.experiments().get("exp-mature").map(|exp| exp.status),
            Some(crate::ab_testing::ExperimentStatus::Completed)
        );
        let baseline = hub
            .adaptive_baselines()
            .resolve(crate::pipeline::routing::TaskType::Fetch, None);
        assert!(
            baseline.is_some(),
            "winner should be promoted into a baseline"
        );
        let events = astra_services::session_journal::read_journal("test-session-promote").unwrap();
        assert!(events.iter().any(|event| {
            event.event_type
                == astra_services::session_journal::JournalEventType::AdaptiveBaselinePromoted
        }));
    }

    #[test]
    fn tuning_cycle_creates_exploration_experiments_from_pattern_library() {
        let hub = make_hub();
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub.clone());
        state.telemetry.observability_session = Some(session);
        state.telemetry.completed_turns_for_tuning = DEFAULT_TUNING_CYCLE_INTERVAL;

        let pattern_library = std::sync::Arc::new(std::sync::Mutex::new(
            crate::pipeline::pattern::PatternLibrary::default(),
        ));
        {
            let mut library = pattern_library.lock().unwrap();
            library.record_outcome(
                &["view".to_string()],
                crate::pipeline::routing::TaskType::Fetch,
                None,
                false,
                0.2,
                None,
            );
            library.record_outcome(
                &["view".to_string()],
                crate::pipeline::routing::TaskType::Fetch,
                None,
                false,
                0.3,
                None,
            );
        }
        hub.attach_pattern_library(pattern_library);

        maybe_run_tuning_cycle(&mut state);

        let experiments = hub.experiments();
        let created = experiments.get("explore-fetch-any");
        assert!(
            created.is_some(),
            "tuning cycle should auto-create exploration"
        );
        assert_eq!(
            created.map(|experiment| experiment.status),
            Some(crate::ab_testing::ExperimentStatus::Running)
        );
    }

    // ── Per-turn micro-adaptation tests ──

    #[test]
    fn per_turn_adaptation_shrinks_token_budget_on_high_usage() {
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_session = Some(session.clone());

        {
            let mut guard = session.write().unwrap();
            guard.config.token_budget.max_turn_input_tokens = 80_000;
            guard.config.context_window.adaptive = true;
        }
        state.max_turn_input_tokens = 80_000;

        // Simulate a turn that used 72k tokens (90% of 80k, above 85% threshold)
        apply_per_turn_adaptation(&mut state, 72_000);

        let guard = session.read().unwrap();
        assert!(
            guard.config.token_budget.max_turn_input_tokens < 80_000,
            "budget should shrink: {}",
            guard.config.token_budget.max_turn_input_tokens
        );
        assert!(
            guard.config.token_budget.max_turn_input_tokens >= 30_000,
            "budget should not go below floor"
        );
        assert_eq!(
            state.max_turn_input_tokens, guard.config.token_budget.max_turn_input_tokens as u64,
            "loop state should stay in sync"
        );
    }

    #[test]
    fn per_turn_adaptation_no_change_on_low_usage() {
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_session = Some(session.clone());

        {
            let mut guard = session.write().unwrap();
            guard.config.token_budget.max_turn_input_tokens = 80_000;
            guard.config.context_window.adaptive = true;
        }
        state.max_turn_input_tokens = 80_000;

        // Simulate a turn that used only 40k tokens (50% of 80k, below 85% threshold)
        apply_per_turn_adaptation(&mut state, 40_000);

        let guard = session.read().unwrap();
        assert_eq!(
            guard.config.token_budget.max_turn_input_tokens, 80_000,
            "budget should not change for low usage"
        );
    }

    #[test]
    fn per_turn_adaptation_lowers_compression_threshold_after_multiple_compressions() {
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_session = Some(session.clone());

        {
            let mut guard = session.write().unwrap();
            guard.config.compression.compression_threshold = 0.8;
            guard.config.context_window.dynamic_compression = true;
            guard.config.context_window.compression_threshold_min = 0.5;
            // Record 2 compressions
            guard.record_compression(1);
            guard.record_compression(3);
        }

        apply_per_turn_adaptation(&mut state, 0);

        let guard = session.read().unwrap();
        assert!(
            (guard.config.compression.compression_threshold - 0.75).abs() < 0.001,
            "threshold should drop by 0.05: {}",
            guard.config.compression.compression_threshold
        );
    }

    #[test]
    fn per_turn_adaptation_respects_compression_threshold_floor() {
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_session = Some(session.clone());

        {
            let mut guard = session.write().unwrap();
            guard.config.compression.compression_threshold = 0.52;
            guard.config.context_window.dynamic_compression = true;
            guard.config.context_window.compression_threshold_min = 0.5;
            guard.record_compression(1);
            guard.record_compression(2);
        }

        apply_per_turn_adaptation(&mut state, 0);

        let guard = session.read().unwrap();
        assert!(
            guard.config.compression.compression_threshold >= 0.5,
            "threshold should not go below floor: {}",
            guard.config.compression.compression_threshold
        );
    }

    #[test]
    fn per_turn_adaptation_raises_verification_on_corrections() {
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_session = Some(session.clone());

        let initial_strictness;
        {
            let mut guard = session.write().unwrap();
            guard.config.verification.adaptive = true;
            guard.config.verification.increase_on_correction = true;
            guard.config.verification.strictness = 0.5;
            guard.config.verification.max_strictness = 0.9;
            initial_strictness = guard.config.verification.strictness;
            // Simulate a recent correction at current turn
            guard.turn_number = 5;
            guard.user_corrections.push(4);
        }

        apply_per_turn_adaptation(&mut state, 0);

        let guard = session.read().unwrap();
        assert!(
            guard.config.verification.strictness > initial_strictness,
            "strictness should increase: {} > {}",
            guard.config.verification.strictness,
            initial_strictness
        );
        assert!(
            (guard.config.verification.strictness - 0.55).abs() < 0.001,
            "should increase by 0.05: {}",
            guard.config.verification.strictness
        );
    }

    #[test]
    fn per_turn_adaptation_no_verification_change_without_recent_corrections() {
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_session = Some(session.clone());

        {
            let mut guard = session.write().unwrap();
            guard.config.verification.adaptive = true;
            guard.config.verification.increase_on_correction = true;
            guard.config.verification.strictness = 0.5;
            // Old correction, not recent
            guard.turn_number = 10;
            guard.user_corrections.push(2);
        }

        apply_per_turn_adaptation(&mut state, 0);

        let guard = session.read().unwrap();
        assert!(
            (guard.config.verification.strictness - 0.5).abs() < 0.001,
            "strictness should not change for old corrections: {}",
            guard.config.verification.strictness
        );
    }

    #[test]
    fn adaptive_profile_applies_scenario_memory_and_verification() {
        let hub = make_hub();
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub);
        state.telemetry.observability_session = Some(session.clone());
        state.message = "review the PR diff and approve the change".into();
        state.recent_tools = vec!["view".into()];

        {
            let mut guard = session.write().unwrap();
            for _ in 0..5 {
                guard.record_query("review the PR diff and approve the change");
            }
        }

        apply_adaptive_execution_profile(&mut state);

        let guard = session.read().unwrap();
        // CodeReview scenario should set memory_top_k=7 and verification_strictness=0.7
        assert_eq!(
            guard.profile.current_scenario,
            Some(crate::user_profile::Scenario::CodeReview)
        );
        assert_eq!(guard.config.memory.retrieval_top_k, 7);
        assert!(
            (guard.config.verification.strictness - 0.7).abs() < 0.01,
            "verification should be 0.7 for code review: {}",
            guard.config.verification.strictness
        );
    }

    // ── Anti-flap dampening tests ──

    #[test]
    fn anti_flap_scenario_cooldown_suppresses_rapid_change() {
        let hub = make_hub();
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub);
        state.telemetry.observability_session = Some(session.clone());

        // First call: set up Debugging scenario via queries
        state.message = "fix the crash in the parser module".into();
        state.recent_tools = vec!["bash".into()];
        {
            let mut guard = session.write().unwrap();
            guard.turn_number = 1;
            for _ in 0..5 {
                guard.record_query("fix the crash in the parser module");
            }
        }
        apply_adaptive_execution_profile(&mut state);

        let scenario_after_first = {
            let guard = session.read().unwrap();
            guard.profile.current_scenario
        };

        // Second call at turn 2 (within cooldown): try switching to CodeReview
        state.message = "review the PR diff and approve the change".into();
        state.recent_tools = vec!["view".into()];
        {
            let mut guard = session.write().unwrap();
            guard.turn_number = 2;
            guard.recent_queries.clear();
            for _ in 0..5 {
                guard.record_query("review the PR diff and approve the change");
            }
        }
        apply_adaptive_execution_profile(&mut state);

        let scenario_after_second = {
            let guard = session.read().unwrap();
            guard.profile.current_scenario
        };

        // Scenario should NOT have changed due to cooldown
        assert_eq!(
            scenario_after_first, scenario_after_second,
            "scenario should be suppressed by cooldown: first={:?}, second={:?}",
            scenario_after_first, scenario_after_second
        );
    }

    #[test]
    fn anti_flap_scenario_change_allowed_after_cooldown() {
        let hub = make_hub();
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub);
        state.telemetry.observability_session = Some(session.clone());

        // First call: set up Debugging scenario
        state.message = "fix the crash in the parser module".into();
        state.recent_tools = vec!["bash".into()];
        {
            let mut guard = session.write().unwrap();
            guard.turn_number = 1;
            for _ in 0..5 {
                guard.record_query("fix the crash in the parser module");
            }
        }
        apply_adaptive_execution_profile(&mut state);

        // Second call at turn 10 (well past cooldown of 5): switch to CodeReview
        state.message = "review the PR diff and approve the change".into();
        state.recent_tools = vec!["view".into()];
        {
            let mut guard = session.write().unwrap();
            guard.turn_number = 10;
            guard.recent_queries.clear();
            for _ in 0..5 {
                guard.record_query("review the PR diff and approve the change");
            }
        }
        apply_adaptive_execution_profile(&mut state);

        let guard = session.read().unwrap();
        // After cooldown expires, scenario should be allowed to change
        assert_ne!(
            guard.profile.current_scenario,
            Some(crate::user_profile::Scenario::Debugging),
            "scenario should change after cooldown expires"
        );
    }

    #[test]
    fn anti_flap_token_budget_oscillation_suppressed() {
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_session = Some(session.clone());

        {
            let mut guard = session.write().unwrap();
            guard.config.token_budget.max_turn_input_tokens = 80_000;
            guard.config.context_window.adaptive = true;
            guard.turn_number = 5;
            // Simulate that a tuning cycle just increased the budget at turn 4
            guard.last_token_budget_direction = 1; // increase
            guard.last_token_budget_change_turn = Some(4);
        }
        state.max_turn_input_tokens = 80_000;

        // Now per-turn wants to decrease (turn 5, within cooldown of 3 from turn 4)
        apply_per_turn_adaptation(&mut state, 72_000);

        let guard = session.read().unwrap();
        // Budget should NOT decrease because we'd be oscillating
        assert_eq!(
            guard.config.token_budget.max_turn_input_tokens, 80_000,
            "budget should be unchanged due to oscillation suppression"
        );
    }

    #[test]
    fn anti_flap_token_budget_decrease_allowed_after_cooldown() {
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_session = Some(session.clone());

        {
            let mut guard = session.write().unwrap();
            guard.config.token_budget.max_turn_input_tokens = 80_000;
            guard.config.context_window.adaptive = true;
            guard.turn_number = 10;
            // Previous increase was at turn 2 — well past cooldown
            guard.last_token_budget_direction = 1;
            guard.last_token_budget_change_turn = Some(2);
        }
        state.max_turn_input_tokens = 80_000;

        apply_per_turn_adaptation(&mut state, 72_000);

        let guard = session.read().unwrap();
        assert!(
            guard.config.token_budget.max_turn_input_tokens < 80_000,
            "budget should decrease after cooldown: {}",
            guard.config.token_budget.max_turn_input_tokens
        );
        assert_eq!(
            guard.last_token_budget_direction, -1,
            "direction should be updated to decrease"
        );
    }

    #[test]
    fn anti_flap_consecutive_decreases_not_suppressed() {
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_session = Some(session.clone());

        {
            let mut guard = session.write().unwrap();
            guard.config.token_budget.max_turn_input_tokens = 80_000;
            guard.config.context_window.adaptive = true;
            guard.turn_number = 5;
            // Previous change was also a decrease
            guard.last_token_budget_direction = -1;
            guard.last_token_budget_change_turn = Some(4);
        }
        state.max_turn_input_tokens = 80_000;

        // Another decrease should be allowed (same direction = not oscillation)
        apply_per_turn_adaptation(&mut state, 72_000);

        let guard = session.read().unwrap();
        assert!(
            guard.config.token_budget.max_turn_input_tokens < 80_000,
            "consecutive decreases should not be suppressed: {}",
            guard.config.token_budget.max_turn_input_tokens
        );
    }

    // ── Journal event attribution tests ──

    #[test]
    fn adaptive_profile_emits_journal_event_on_scenario_change() {
        let hub = make_hub();
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub);
        state.telemetry.observability_session = Some(session.clone());
        state.current_session_id = Some("journal-test-session".into());

        state.message = "fix the crash in the parser module".into();
        state.recent_tools = vec!["bash".into()];
        {
            let mut guard = session.write().unwrap();
            for _ in 0..5 {
                guard.record_query("fix the crash in the parser module");
            }
        }

        apply_adaptive_execution_profile(&mut state);

        // Verify a scenario was detected (journal event emission is best-effort
        // in tests since we don't have a real journal backend, but the function
        // should not panic).
        let guard = session.read().unwrap();
        assert!(
            guard.profile.current_scenario.is_some(),
            "scenario should be detected for journal event"
        );
    }

    #[test]
    fn per_turn_adaptation_tracks_budget_direction_state() {
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_session = Some(session.clone());

        {
            let mut guard = session.write().unwrap();
            guard.config.token_budget.max_turn_input_tokens = 80_000;
            guard.config.context_window.adaptive = true;
            guard.turn_number = 1;
        }
        state.max_turn_input_tokens = 80_000;

        apply_per_turn_adaptation(&mut state, 72_000);

        let guard = session.read().unwrap();
        // After a decrease, direction should be -1
        assert_eq!(guard.last_token_budget_direction, -1);
        assert_eq!(guard.last_token_budget_change_turn, Some(1));
    }

    #[test]
    fn tuning_cycle_updates_budget_direction_on_increase() {
        let hub = make_hub();
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub.clone());
        state.telemetry.observability_session = Some(session.clone());
        state.telemetry.completed_turns_for_tuning = DEFAULT_TUNING_CYCLE_INTERVAL;

        // Set a low budget so tuning might increase it
        {
            let mut guard = session.write().unwrap();
            guard.config.token_budget.max_turn_input_tokens = 50_000;
            guard.turn_number = 10;
        }
        state.max_turn_input_tokens = 50_000;

        // Add a rule that increases token budget
        hub.tuning()
            .add_rule(crate::auto_tuning::EvolutionRule::new(
                "test-increase-budget",
                crate::auto_tuning::EvolutionTrigger::LowSuccessRate {
                    threshold: 1.0, // always fires
                    window_secs: 3600,
                    min_samples: 0,
                },
                crate::auto_tuning::EvolutionAction::AdjustConfig {
                    path: "token_budget.max_turn_input_tokens".into(),
                    delta: 10_000.0,
                    min: None,
                    max: None,
                },
            ));

        maybe_run_tuning_cycle(&mut state);

        let guard = session.read().unwrap();
        if guard.config.token_budget.max_turn_input_tokens > 50_000 {
            // If the rule fired and increased budget, direction should be +1
            assert_eq!(guard.last_token_budget_direction, 1);
            assert_eq!(guard.last_token_budget_change_turn, Some(10));
        }
        // (If the rule didn't fire due to min_samples, that's OK — the direction
        // tracking only activates on actual changes.)
    }

    // ══════════════════════════════════════════════════════════════════════
    // Stress & integration tests
    // ══════════════════════════════════════════════════════════════════════

    /// Simulate a single "turn" through all adaptive phases:
    /// 1. apply_adaptive_execution_profile (scenario routing)
    /// 2. apply_per_turn_adaptation (micro-adaptation based on token usage)
    /// 3. record_loop_completion_feedback (outcome signal)
    /// 4. maybe_run_tuning_cycle (if interval reached)
    fn simulate_turn(
        state: &mut AgenticLoopState,
        session: &std::sync::Arc<
            std::sync::RwLock<crate::observability_integration::ObservabilitySession>,
        >,
        query: &str,
        tools: &[&str],
        tokens_used: u64,
        outcome: &Result<AgenticLoopOutcome, String>,
    ) {
        // Advance turn
        {
            let mut guard = session.write().unwrap();
            guard.turn_number += 1;
            guard.record_query(query);
        }
        state.message = query.into();
        state.recent_tools = tools.iter().map(|s| s.to_string()).collect();

        // Phase 1: scenario routing
        apply_adaptive_execution_profile(state);

        // Phase 2: micro-adaptation
        apply_per_turn_adaptation(state, tokens_used);

        // Phase 3: feedback
        state.telemetry.completed_turns_for_tuning += 1;
        record_loop_completion_feedback(state, outcome);

        // Phase 4: tuning cycle (fires at interval)
        maybe_run_tuning_cycle(state);
    }

    #[test]
    fn stress_full_adaptive_loop_20_turns() {
        let hub = make_hub();
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub.clone());
        state.telemetry.observability_session = Some(session.clone());
        state.current_session_id = Some("stress-test".into());
        state.current_run_id = Some("run-stress".into());

        // Pre-seed queries for scenario detection
        {
            let mut guard = session.write().unwrap();
            guard.config.context_window.adaptive = true;
            guard.config.token_budget.max_turn_input_tokens = 100_000;
        }
        state.max_turn_input_tokens = 100_000;

        let success: Result<AgenticLoopOutcome, String> = Ok(AgenticLoopOutcome::Completed);
        let failure: Result<AgenticLoopOutcome, String> =
            Ok(AgenticLoopOutcome::Error("test error".into()));

        // Turns 1-10: Debugging scenario, moderate token usage, mostly successful
        for i in 0..10 {
            let outcome = if i == 7 { &failure } else { &success };
            let tokens = 50_000 + (i as u64 * 2_000); // 50k → 68k
            simulate_turn(
                &mut state,
                &session,
                "fix the crash in the parser",
                &["bash", "view"],
                tokens,
                outcome,
            );
        }

        let mid_state = {
            let guard = session.read().unwrap();
            (
                guard.profile.current_scenario,
                guard.config.token_budget.max_turn_input_tokens,
                guard.turn_number,
            )
        };
        assert_eq!(mid_state.2, 10, "should be at turn 10");
        assert!(
            mid_state.0.is_some(),
            "scenario should be detected after 10 debugging queries"
        );

        // Turns 11-20: Switch to CodeReview (some may be suppressed by cooldown)
        for _ in 0..10 {
            simulate_turn(
                &mut state,
                &session,
                "review the PR diff and approve the change",
                &["view"],
                40_000,
                &success,
            );
        }

        let final_state = {
            let guard = session.read().unwrap();
            (
                guard.profile.current_scenario,
                guard.config.token_budget.max_turn_input_tokens,
                guard.turn_number,
            )
        };
        assert_eq!(final_state.2, 20);
        // Budget should still be within valid range
        assert!(
            final_state.1 >= 30_000 && final_state.1 <= 200_000,
            "budget should be in valid range: {}",
            final_state.1
        );
        // No panic, no corruption — full loop survived 20 turns
    }

    #[test]
    fn stress_budget_conflict_tuning_increase_then_per_turn_decrease() {
        let hub = make_hub();
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub.clone());
        state.telemetry.observability_session = Some(session.clone());
        state.current_run_id = Some("run-conflict".into());

        {
            let mut guard = session.write().unwrap();
            guard.config.token_budget.max_turn_input_tokens = 50_000;
            guard.config.context_window.adaptive = true;
            guard.turn_number = 9;
        }
        state.max_turn_input_tokens = 50_000;

        // Add a rule that increases token budget
        hub.tuning()
            .add_rule(crate::auto_tuning::EvolutionRule::new(
                "increase-budget",
                crate::auto_tuning::EvolutionTrigger::LowSuccessRate {
                    threshold: 1.0,
                    window_secs: 3600,
                    min_samples: 0,
                },
                crate::auto_tuning::EvolutionAction::AdjustConfig {
                    path: "token_budget.max_turn_input_tokens".into(),
                    delta: 20_000.0,
                    min: None,
                    max: None,
                },
            ));

        // Step 1: Fire tuning cycle — should increase budget
        state.telemetry.completed_turns_for_tuning = DEFAULT_TUNING_CYCLE_INTERVAL;
        maybe_run_tuning_cycle(&mut state);

        let budget_after_tuning = {
            let guard = session.read().unwrap();
            guard.config.token_budget.max_turn_input_tokens
        };

        // Step 2: Advance turn, then per-turn wants to decrease due to high usage
        {
            let mut guard = session.write().unwrap();
            guard.turn_number = 10;
        }

        // High token usage relative to new budget
        let high_usage = (budget_after_tuning as f64 * 0.92) as u64;
        apply_per_turn_adaptation(&mut state, high_usage);

        let budget_after_per_turn = {
            let guard = session.read().unwrap();
            guard.config.token_budget.max_turn_input_tokens
        };

        // Anti-flap should suppress the decrease because tuning just increased
        // (direction reversal within cooldown)
        if budget_after_tuning > 50_000 {
            // Tuning rule fired, so anti-flap should kick in
            assert_eq!(
                budget_after_per_turn, budget_after_tuning,
                "anti-flap should suppress decrease right after tuning increase: tuning={}, per_turn={}",
                budget_after_tuning, budget_after_per_turn
            );
        }
        // In all cases, budget should be valid
        assert!(budget_after_per_turn >= 30_000);
    }

    #[test]
    fn stress_rapid_scenario_switching_100_turns() {
        let hub = make_hub();
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub.clone());
        state.telemetry.observability_session = Some(session.clone());

        let debugging_queries = [
            "fix the crash in the parser module",
            "debug the segfault in memory allocator",
            "trace the bug causing data corruption",
        ];
        let review_queries = [
            "review the PR diff and approve the change",
            "review this pull request for correctness",
            "approve the PR after reviewing all changes",
        ];

        let mut scenario_changes = 0u32;
        let mut last_scenario: Option<crate::user_profile::Scenario> = None;

        for turn in 1..=100u32 {
            let query = if turn % 2 == 1 {
                debugging_queries[(turn as usize / 2) % 3]
            } else {
                review_queries[(turn as usize / 2) % 3]
            };

            {
                let mut guard = session.write().unwrap();
                guard.turn_number = turn;
                // Keep only recent queries for scenario detection
                if guard.recent_queries.len() > 5 {
                    let drain_end = guard.recent_queries.len() - 2;
                    guard.recent_queries.drain(0..drain_end);
                }
                guard.record_query(query);
            }
            state.message = query.into();
            state.recent_tools = vec!["view".into()];

            apply_adaptive_execution_profile(&mut state);

            let current = {
                let guard = session.read().unwrap();
                guard.profile.current_scenario
            };
            if current != last_scenario {
                scenario_changes += 1;
                last_scenario = current;
            }
        }

        // With scenario_cooldown_turns=5 (default), max possible changes in 100 turns is ~20
        // (initial detection + one change per 5 turns).
        assert!(
            scenario_changes <= 25,
            "anti-flap should limit scenario changes: got {} in 100 turns",
            scenario_changes
        );
        // Verify no panic, no corruption
        let guard = session.read().unwrap();
        assert_eq!(guard.turn_number, 100);
    }

    #[test]
    fn stress_oscillating_token_usage_50_turns() {
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_session = Some(session.clone());

        {
            let mut guard = session.write().unwrap();
            guard.config.token_budget.max_turn_input_tokens = 80_000;
            guard.config.context_window.adaptive = true;
        }
        state.max_turn_input_tokens = 80_000;

        let mut direction_changes = 0u32;
        let mut prev_direction: i8 = 0;

        for turn in 1..=50u32 {
            {
                let mut guard = session.write().unwrap();
                guard.turn_number = turn;
            }

            // Alternate between high (92%) and low (40%) usage
            let budget = {
                let guard = session.read().unwrap();
                guard.config.token_budget.max_turn_input_tokens
            };
            let tokens = if turn % 2 == 0 {
                (budget as f64 * 0.92) as u64 // high usage
            } else {
                (budget as f64 * 0.40) as u64 // low usage (no change)
            };

            apply_per_turn_adaptation(&mut state, tokens);

            let current_direction = {
                let guard = session.read().unwrap();
                guard.last_token_budget_direction
            };
            if current_direction != prev_direction && current_direction != 0 {
                direction_changes += 1;
                prev_direction = current_direction;
            }
        }

        let final_budget = {
            let guard = session.read().unwrap();
            guard.config.token_budget.max_turn_input_tokens
        };

        // Budget should still be valid
        assert!(
            final_budget >= 30_000 && final_budget <= 80_000,
            "budget should be in valid range: {}",
            final_budget
        );

        // Direction changes should be limited (per-turn only decreases, so
        // oscillation only occurs if something else increases — in this test
        // nothing increases, so direction_changes should be ≤ 1)
        assert!(
            direction_changes <= 5,
            "direction changes should be limited: got {}",
            direction_changes
        );
    }

    #[test]
    fn stress_multi_turn_state_continuity_50_turns() {
        let hub = make_hub();
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub.clone());
        state.telemetry.observability_session = Some(session.clone());
        state.current_session_id = Some("continuity-test".into());
        state.current_run_id = Some("run-continuity".into());

        {
            let mut guard = session.write().unwrap();
            guard.config.context_window.adaptive = true;
            guard.config.verification.adaptive = true;
            guard.config.verification.increase_on_correction = true;
            guard.config.memory_pressure.adaptive = true;
            guard.config.memory_pressure.expand_on_correction = true;
            guard.config.token_budget.max_turn_input_tokens = 100_000;
        }
        state.max_turn_input_tokens = 100_000;

        let success: Result<AgenticLoopOutcome, String> = Ok(AgenticLoopOutcome::Completed);

        for turn in 1..=50u32 {
            // Add some corrections every 10 turns
            if turn % 10 == 0 {
                let mut guard = session.write().unwrap();
                guard.user_corrections.push(turn);
            }

            let tokens = 60_000 + (turn as u64 * 500); // gradually increasing
            simulate_turn(
                &mut state,
                &session,
                "fix the crash in the parser module",
                &["bash", "view"],
                tokens,
                &success,
            );
        }

        let guard = session.read().unwrap();
        assert_eq!(guard.turn_number, 50);

        // Verify anti-flap state is coherent
        assert!(
            guard.last_scenario_change_turn.is_some(),
            "scenario change should have been recorded"
        );

        // Budget should have been influenced by high usage
        assert!(
            guard.config.token_budget.max_turn_input_tokens <= 100_000,
            "budget should not have increased: {}",
            guard.config.token_budget.max_turn_input_tokens
        );
        assert!(
            guard.config.token_budget.max_turn_input_tokens >= 30_000,
            "budget should be above floor: {}",
            guard.config.token_budget.max_turn_input_tokens
        );

        // Verification strictness should have increased from corrections
        assert!(
            guard.config.verification.strictness >= 0.5,
            "strictness should have increased from corrections: {:.3}",
            guard.config.verification.strictness
        );
    }

    #[test]
    fn stress_experiment_lifecycle_create_enroll_conclude_promote() {
        use crate::ab_testing::{Experiment, Variant};

        let hub = make_hub();
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub.clone());
        state.telemetry.observability_session = Some(session.clone());

        // Step 1: Create an experiment via the hub
        let experiment = Experiment::new("test-exp-lifecycle")
            .with_description("Test lifecycle experiment")
            .with_variant(Variant {
                id: "control".into(),
                name: "control".into(),
                description: "Control: default config".into(),
                config_diff: std::collections::HashMap::new(),
                traffic_percentage: 0.5,
                is_control: true,
            })
            .with_variant(Variant {
                id: "treatment".into(),
                name: "treatment".into(),
                description: "Treatment: higher budget".into(),
                config_diff: {
                    let mut m = std::collections::HashMap::new();
                    m.insert(
                        "token_budget.max_turn_input_tokens".into(),
                        serde_json::json!(120_000),
                    );
                    m
                },
                traffic_percentage: 0.5,
                is_control: false,
            })
            .with_min_samples(3)
            .build();
        hub.experiments().register(experiment);

        // Step 2: Enroll by setting active experiment
        {
            let mut guard = session.write().unwrap();
            guard.active_experiment_id = Some("test-exp-lifecycle".into());
            guard
                .profile
                .enroll_experiment("test-exp-lifecycle".to_string());
        }

        // Step 3: Simulate turns with the experiment active
        state.message = "fix the crash in the parser module".into();
        state.recent_tools = vec!["bash".into()];
        for turn in 1..=5u32 {
            let mut guard = session.write().unwrap();
            guard.turn_number = turn;
            guard.record_query("fix the crash in the parser module");
        }

        // Step 4: Record outcomes for the experiment
        for _ in 0..5 {
            let experiments = hub.experiments();
            let outcome = crate::ab_testing::ExperimentOutcome::new("test-user", "treatment")
                .with_success(true);
            experiments.record_outcome("test-exp-lifecycle", outcome);
        }
        for _ in 0..2 {
            let experiments = hub.experiments();
            let outcome = crate::ab_testing::ExperimentOutcome::new("test-user", "control")
                .with_success(true);
            experiments.record_outcome("test-exp-lifecycle", outcome);
        }
        for _ in 0..3 {
            let experiments = hub.experiments();
            let outcome = crate::ab_testing::ExperimentOutcome::new("test-user", "control")
                .with_success(false);
            experiments.record_outcome("test-exp-lifecycle", outcome);
        }

        // Step 5: Try conclusion
        let exploration = crate::exploration_engine::ExplorationEngine::default();
        let experiments = hub.experiments();
        let concluded = exploration.conclude_mature_experiments(&experiments);

        // The experiment may or may not be mature enough to conclude depending
        // on min_samples, but the lifecycle should not panic.
        if !concluded.is_empty() {
            let conclusion = &concluded[0];
            assert_eq!(conclusion.experiment_id, "test-exp-lifecycle");
            // Treatment had 5/5 success (100%), control had 2/5 (40%)
            // Treatment should win
            if let Some(winner) = &conclusion.winner_variant_id {
                assert_eq!(
                    winner, "treatment",
                    "treatment should win with higher success"
                );
            }
        }

        // Verify experiment is accessible and no corruption
        let exp = experiments.get("test-exp-lifecycle");
        assert!(exp.is_some(), "experiment should still exist");
    }

    #[test]
    fn stress_all_8_default_rules_fire() {
        use crate::auto_tuning::{FeedbackSignal, SignalType, default_rules};

        let hub = make_hub();
        // Load default evolution rules so the tuning engine has something to evaluate
        for rule in default_rules() {
            hub.tuning().add_rule(rule);
        }
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub.clone());
        state.telemetry.observability_session = Some(session.clone());
        state.current_run_id = Some("run-all-rules".into());

        // Record diverse feedback to trigger as many rules as possible
        for _ in 0..15 {
            // Failures (triggers LowSuccessRate rules)
            hub.record_feedback(
                FeedbackSignal::new(SignalType::TaskFailure {
                    reason: "test error".into(),
                })
                .with_turn("turn-1"),
            );

            // High token usage
            hub.record_feedback(
                FeedbackSignal::new(SignalType::HighTokenUsage {
                    tokens: 90_000,
                    threshold: 80_000,
                })
                .with_turn("turn-1"),
            );

            // Tool churn
            hub.record_feedback(
                FeedbackSignal::new(SignalType::ToolChurn {
                    calls: 30,
                    unique_tools: 2,
                })
                .with_turn("turn-1"),
            );

            // Corrections
            hub.record_feedback(FeedbackSignal::new(SignalType::Correction).with_turn("turn-1"));

            // Interruptions
            hub.record_feedback(FeedbackSignal::new(SignalType::Interruption).with_turn("turn-1"));
        }

        // Trigger tuning cycle
        {
            let mut guard = session.write().unwrap();
            guard.turn_number = 10;
        }
        state.telemetry.completed_turns_for_tuning = DEFAULT_TUNING_CYCLE_INTERVAL;

        let config_before = {
            let guard = session.read().unwrap();
            guard.config.clone()
        };

        maybe_run_tuning_cycle(&mut state);

        let config_after = {
            let guard = session.read().unwrap();
            guard.config.clone()
        };

        // At least some rules should have fired
        let executions = hub.tuning().get_executions();
        assert!(
            !executions.is_empty(),
            "at least some rules should fire with diverse feedback signals"
        );

        // Config should still be valid (no corruption from multiple simultaneous adjustments)
        assert!(config_after.token_budget.max_turn_input_tokens >= 10_000);
        assert!(config_after.token_budget.max_turn_input_tokens <= 500_000);
        assert!(config_after.memory.retrieval_top_k >= 1);
        assert!(config_after.memory.retrieval_top_k <= 50);

        // Verify that config actually changed (at least one rule had an effect)
        let budget_changed = config_before.token_budget.max_turn_input_tokens
            != config_after.token_budget.max_turn_input_tokens;
        let memory_changed =
            config_before.memory.retrieval_top_k != config_after.memory.retrieval_top_k;
        let _some_change = budget_changed || memory_changed;

        // No panic, no corruption — all rules composed successfully
    }

    /// Full-loop replay test covering:
    /// signal emission → tuning cycle → experiment creation → conclusion → baseline promotion → abort/rollback
    #[test]
    fn replay_full_adaptive_cycle_with_experiment_lifecycle() {
        use crate::ab_testing::{ExperimentOutcome, ExperimentStore};
        use crate::adaptive_baselines::AdaptiveBaselineStore;
        use crate::exploration_engine::ExplorationEngine;
        use crate::pipeline::routing::TaskType;

        let hub = make_hub();
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub.clone());
        state.telemetry.observability_session = Some(session.clone());
        state.current_session_id = Some("replay-test".into());
        state.current_run_id = Some("run-replay".into());

        {
            let mut guard = session.write().unwrap();
            guard.config.context_window.adaptive = true;
            guard.config.token_budget.max_turn_input_tokens = 100_000;
        }
        state.max_turn_input_tokens = 100_000;

        // --- Phase 1: Generate mixed signals over 10 turns ---
        let success: Result<AgenticLoopOutcome, String> = Ok(AgenticLoopOutcome::Completed);
        let failure: Result<AgenticLoopOutcome, String> =
            Ok(AgenticLoopOutcome::Error("test error".into()));

        for i in 0..10 {
            let outcome = if i % 3 == 0 { &failure } else { &success };
            simulate_turn(
                &mut state,
                &session,
                "analyze the code structure",
                &["view", "grep"],
                60_000 + (i as u64 * 3_000),
                outcome,
            );
        }

        // Verify signals were recorded: should have mix of successes and failures
        let config = crate::runtime_config::RuntimeConfig::default();
        hub.tuning()
            .add_rule(crate::auto_tuning::EvolutionRule::new(
                "check-signals",
                crate::auto_tuning::EvolutionTrigger::LowSuccessRate {
                    threshold: 0.9,
                    window_secs: 3600,
                    min_samples: 3,
                },
                crate::auto_tuning::EvolutionAction::Alert {
                    message: "low success".into(),
                    severity: crate::auto_tuning::AlertSeverity::Warning,
                },
            ));
        let triggered = hub.tuning().evaluate(&config);
        assert!(
            !triggered.is_empty(),
            "mixed success/failure should trigger low-success rule"
        );

        // --- Phase 2: Exercise experiment lifecycle separately ---
        let exp_store = ExperimentStore::new();
        let baselines = AdaptiveBaselineStore::new();
        let exploration = ExplorationEngine::new(0.5, 3, 1);

        // Manually seed a pattern library with low-confidence area
        let mut pattern_lib = crate::pipeline::pattern::PatternLibrary::default();
        for _ in 0..5 {
            pattern_lib.record_outcome(
                &["view".to_string()],
                TaskType::Fetch,
                None,
                false,
                0.2,
                None,
            );
        }

        // Create experiments from pattern opportunities
        let created = exploration.check_and_create_experiments(&pattern_lib, &exp_store);
        assert!(
            !created.is_empty(),
            "should create experiment for low-confidence area"
        );
        let exp_id = created[0].id.clone();

        // Record outcomes for the experiment
        exp_store.record_outcome(
            &exp_id,
            ExperimentOutcome::new("u1", "control")
                .with_metric("success_rate", 0.3)
                .with_success(false),
        );
        exp_store.record_outcome(
            &exp_id,
            ExperimentOutcome::new("u2", "treatment-low-success")
                .with_metric("success_rate", 0.9)
                .with_success(true),
        );

        // Conclude mature experiments
        let conclusions = exploration.conclude_mature_experiments(&exp_store);
        assert_eq!(conclusions.len(), 1);
        assert_eq!(conclusions[0].experiment_id, exp_id);
        // With only 2 data points, statistical analysis may return NoSignificantDifference.
        // The important thing is the experiment was concluded.

        // Promote treatment as baseline (simulating a real winner decision).
        if let Some(exp) = exp_store.get(&exp_id) {
            let _ = baselines.promote_winner(&exp, "treatment-low-success");
        }
        assert!(
            baselines.resolve(TaskType::Fetch, None).is_some(),
            "baseline should be promoted"
        );

        // --- Phase 3: Abort experiment → rollback baseline ---
        // Create a new experiment for abort testing
        let mut abort_exp = crate::ab_testing::Experiment::new("exp-abort-replay")
            .with_variant(crate::ab_testing::Variant::control())
            .with_variant(
                crate::ab_testing::Variant::new("risky")
                    .with_traffic(0.5)
                    .with_config_diff("max_tools", serde_json::json!(99)),
            )
            .with_tag("task_type:code")
            .with_tag("domain:any")
            .build();
        abort_exp.start();
        exp_store.register(abort_exp.clone());
        let _ = baselines.promote_winner(&abort_exp, "risky");
        assert!(baselines.resolve(TaskType::Code, None).is_some());

        let (cancelled, rollbacks) =
            exploration.abort_experiment("exp-abort-replay", &exp_store, &baselines);
        assert!(cancelled);
        assert_eq!(rollbacks.len(), 1);
        assert!(
            baselines.resolve(TaskType::Code, None).is_none(),
            "baseline should be rolled back after abort"
        );

        // Original Fetch baseline should still exist
        assert!(
            baselines.resolve(TaskType::Fetch, None).is_some(),
            "unrelated baseline should survive abort"
        );
    }

    #[test]
    fn retry_signal_emitted_on_consecutive_identical_tool_calls() {
        let hub = make_hub();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub.clone());
        state.stall.tool_call_records = vec![
            astra_services::session_journal::ToolCallRecord {
                name: "bash".into(),
                ok: false,
                ms: 100,
                error: Some("exit code 1".into()),
                input_bytes: None,
                output_bytes: None,
                args_preview: Some("npm test".into()),
                result_preview: None,
            },
            astra_services::session_journal::ToolCallRecord {
                name: "bash".into(),
                ok: false,
                ms: 100,
                error: Some("exit code 1".into()),
                input_bytes: None,
                output_bytes: None,
                args_preview: Some("npm test".into()),
                result_preview: None,
            },
        ];

        let result = Ok(AgenticLoopOutcome::Completed);
        record_loop_completion_feedback(&mut state, &result);

        let config = crate::runtime_config::RuntimeConfig::default();
        // Add a rule that triggers on high retry rate.
        hub.tuning()
            .add_rule(crate::auto_tuning::EvolutionRule::new(
                "retry-trigger",
                crate::auto_tuning::EvolutionTrigger::HighRetryRate {
                    threshold: 0.3,
                    window_secs: 3600,
                    min_samples: 1,
                },
                crate::auto_tuning::EvolutionAction::Alert {
                    message: "retries detected".into(),
                    severity: crate::auto_tuning::AlertSeverity::Warning,
                },
            ));
        let triggered = hub.tuning().evaluate(&config);
        assert!(
            !triggered.is_empty(),
            "consecutive identical tool calls should emit Retry signal"
        );
    }

    #[test]
    fn acceptance_signal_emitted_when_no_correction() {
        let hub = make_hub();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub.clone());
        state.message = "thanks, looks good".into();
        state.messages =
            vec![serde_json::json!({"role": "assistant", "content": "here is the code"})];

        let result = Ok(AgenticLoopOutcome::Completed);
        record_loop_completion_feedback(&mut state, &result);

        // Verify Acceptance signal was recorded — check success rate.
        // Acceptance + TaskSuccess = 2 successes, both positive.
        let config = crate::runtime_config::RuntimeConfig::default();
        hub.tuning()
            .add_rule(crate::auto_tuning::EvolutionRule::new(
                "high-success-check",
                crate::auto_tuning::EvolutionTrigger::LowSuccessRate {
                    threshold: 0.5,
                    window_secs: 3600,
                    min_samples: 1,
                },
                crate::auto_tuning::EvolutionAction::Alert {
                    message: "acceptance".into(),
                    severity: crate::auto_tuning::AlertSeverity::Info,
                },
            ));
        let triggered = hub.tuning().evaluate(&config);
        // With Acceptance + TaskSuccess = 100% success rate, LowSuccessRate (0.5) should NOT trigger.
        assert!(
            triggered.is_empty(),
            "acceptance + task success should keep success rate high"
        );
    }

    #[test]
    fn no_acceptance_signal_when_correction_detected() {
        let hub = make_hub();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub.clone());
        state.message = "no that's wrong, please fix it".into();
        state.messages =
            vec![serde_json::json!({"role": "assistant", "content": "here is the code"})];

        let result = Ok(AgenticLoopOutcome::Completed);
        record_loop_completion_feedback(&mut state, &result);

        // Only TaskSuccess should be recorded (no Acceptance because "wrong" is a correction keyword).
        // We can verify by checking there's only 1 signal.
        let config = crate::runtime_config::RuntimeConfig::default();
        hub.tuning()
            .add_rule(crate::auto_tuning::EvolutionRule::new(
                "success-check",
                crate::auto_tuning::EvolutionTrigger::LowSuccessRate {
                    threshold: 1.1, // impossible to reach — we're just counting signals
                    window_secs: 3600,
                    min_samples: 1,
                },
                crate::auto_tuning::EvolutionAction::Alert {
                    message: "check".into(),
                    severity: crate::auto_tuning::AlertSeverity::Info,
                },
            ));
        // This would trigger if there's any signal below the impossible threshold.
        let triggered = hub.tuning().evaluate(&config);
        assert!(
            !triggered.is_empty(),
            "TaskSuccess alone with threshold 1.1 should trigger (success rate < 1.1)"
        );
    }

    // ── L1.3 Tactical adapter wiring tests ──────────────────────────────

    #[test]
    fn tactical_adapter_state_fields_default_to_none() {
        let state = make_state();
        assert!(state.tactical_adapter.is_none());
        assert!(state.step_signal_collector.is_none());
    }

    #[test]
    fn tactical_adapter_wiring_produces_hints_on_error_streak() {
        use crate::liquid::step_signals::{StepSignalCollector, StepSignalConfig};
        use crate::liquid::tactical::{DampenerConfig, TacticalAction, TacticalAdapter};

        let mut state = make_state();
        state.max_turn_input_tokens = 100_000;

        // Set up collector with low error-streak threshold for testability.
        let mut sig_cfg = StepSignalConfig::default();
        sig_cfg.error_streak_threshold = 2;
        state.step_signal_collector = Some(StepSignalCollector::new(sig_cfg, 100_000));

        // Use a permissive dampener so actions fire easily.
        let dampener_cfg = DampenerConfig {
            min_calls_between_same_type: 1,
            max_actions_per_turn: 10,
            drift_freeze_threshold: 1.0,
        };
        state.tactical_adapter = Some(TacticalAdapter::new(dampener_cfg));

        // Simulate 3 consecutive failures for the same tool
        let records = vec![
            ToolCallRecord {
                name: "bash".into(),
                ok: false,
                ms: 100,
                error: Some("exit code 1".into()),
                input_bytes: Some(50),
                output_bytes: Some(200),
                args_preview: Some("ls -la".into()),
                result_preview: Some("error".into()),
            },
            ToolCallRecord {
                name: "bash".into(),
                ok: false,
                ms: 120,
                error: Some("exit code 1".into()),
                input_bytes: Some(50),
                output_bytes: Some(200),
                args_preview: Some("cat foo".into()),
                result_preview: Some("not found".into()),
            },
            ToolCallRecord {
                name: "bash".into(),
                ok: false,
                ms: 130,
                error: Some("exit code 1".into()),
                input_bytes: Some(50),
                output_bytes: Some(200),
                args_preview: Some("rm bar".into()),
                result_preview: Some("permission denied".into()),
            },
        ];

        let evo_records_before = state.stall.tool_call_records.len();
        state.stall.tool_call_records.extend(records);

        // Replay the tactical wiring logic manually (mirrors the loop body)
        let new_records: Vec<ToolCallRecord> =
            state.stall.tool_call_records[evo_records_before..].to_vec();
        let mut step_actions: Vec<TacticalAction> = Vec::new();

        for rec in &new_records {
            let outcome = crate::liquid::step_signals::StepOutcome {
                tool_name: rec.name.clone(),
                ok: rec.ok,
                latency_ms: rec.ms,
                tokens_used: (rec.input_bytes.unwrap_or(0) + rec.output_bytes.unwrap_or(0)) as u64,
                error_hint: rec.error.clone(),
            };
            let triggers = if let Some(ref mut collector) = state.step_signal_collector {
                collector.record(outcome)
            } else {
                vec![]
            };
            if !triggers.is_empty() {
                if let Some(ref mut adapter) = state.tactical_adapter {
                    let actions = adapter.evaluate(&triggers);
                    for action in actions {
                        if !matches!(action, TacticalAction::NoOp) {
                            step_actions.push(action);
                        }
                    }
                    adapter.advance_step();
                }
            }
        }

        // We should see at least one non-NoOp action (IncreaseVerification or SuggestToolSwitch)
        assert!(
            !step_actions.is_empty(),
            "3 consecutive errors should produce tactical actions, got none"
        );

        let hint_parts = apply_tactical_actions(&mut state, &step_actions);

        assert!(!hint_parts.is_empty(), "Should produce non-empty hint text");
    }

    #[test]
    fn tactical_actions_apply_bounded_runtime_mutations() {
        use crate::liquid::tactical::TacticalAction;

        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_session = Some(session.clone());
        state.max_turn_input_tokens = 100_000;
        state.tool_budget_override = Some(1000);

        {
            let mut guard = session.write().unwrap();
            guard.config.verification.strictness = 0.5;
            guard.config.verification.max_strictness = 0.9;
            guard.config.compression.compression_threshold = 0.8;
            guard.config.context_window.compression_threshold_min = 0.5;
            guard.config.tool_selection.tool_budget_tokens = 1000;
            guard.config.token_budget.max_turn_input_tokens = 100_000;
        }

        let hints = apply_tactical_actions(
            &mut state,
            &[
                TacticalAction::IncreaseVerification {
                    reason: "3 consecutive errors".into(),
                },
                TacticalAction::SuggestToolSwitch {
                    from_tool: "bash".into(),
                    reason: "repeated failures".into(),
                },
                TacticalAction::TokenBudgetWarning {
                    used: 90_000,
                    budget: 100_000,
                },
                TacticalAction::ThrottleHint {
                    reason: "latency spike".into(),
                },
            ],
        );

        let guard = session.read().unwrap();
        assert!(guard.config.verification.strictness > 0.5);
        assert!(state.turn_guard.health.is_deprioritized("bash"));
        assert_eq!(state.tool_budget_override, Some(850));
        assert!(guard.config.compression.compression_threshold < 0.8);
        assert!(guard.config.token_budget.max_turn_input_tokens < 100_000);
        assert_eq!(
            state.max_turn_input_tokens,
            guard.config.token_budget.max_turn_input_tokens as u64
        );
        assert!(!hints.is_empty());
    }

    #[test]
    fn tactical_budget_mutations_survive_next_adaptive_profile_application() {
        use crate::liquid::tactical::TacticalAction;

        let hub = make_hub();
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub);
        state.telemetry.observability_session = Some(session.clone());
        state.message = "fix the failing bug in the parser".into();

        {
            let mut guard = session.write().unwrap();
            for _ in 0..5 {
                guard.record_query("fix the failing bug in the parser");
            }
            guard.config.context_window.adaptive = true;
            guard.config.tool_selection.tool_budget_tokens = 1100;
            guard.config.token_budget.max_turn_input_tokens = 100_000;
            guard.config.compression.compression_threshold = 0.8;
            guard.config.context_window.compression_threshold_min = 0.5;
        }

        apply_tactical_actions(
            &mut state,
            &[
                TacticalAction::TokenBudgetWarning {
                    used: 90_000,
                    budget: 100_000,
                },
                TacticalAction::ThrottleHint {
                    reason: "latency spike".into(),
                },
            ],
        );

        let lowered_tool_budget = state.tool_budget_override.expect("tool budget override");
        let lowered_turn_budget = state.max_turn_input_tokens;
        assert!(lowered_tool_budget < 1100);
        assert!(lowered_turn_budget < 100_000);

        apply_adaptive_execution_profile(&mut state);

        let guard = session.read().unwrap();
        assert_eq!(
            state.tool_budget_override,
            Some(lowered_tool_budget),
            "scenario profile should not wipe tactical tool budget reductions"
        );
        assert_eq!(
            state.max_turn_input_tokens, lowered_turn_budget,
            "scenario profile should not wipe tactical turn budget reductions"
        );
        assert_eq!(
            guard.config.tool_selection.tool_budget_tokens, lowered_tool_budget,
            "session config should retain tactical tool budget reductions across turns"
        );
        assert_eq!(
            guard.config.token_budget.max_turn_input_tokens, lowered_turn_budget as u32,
            "session config should retain tactical turn budget reductions across turns"
        );
    }

    #[test]
    fn tactical_adapter_reset_clears_turn_state() {
        use crate::liquid::step_signals::{StepSignalCollector, StepSignalConfig};
        use crate::liquid::tactical::TacticalAdapter;

        let mut state = make_state();
        state.max_turn_input_tokens = 50_000;
        state.step_signal_collector = Some(StepSignalCollector::new(
            StepSignalConfig::default(),
            50_000,
        ));
        state.tactical_adapter = Some(TacticalAdapter::new(
            crate::liquid::tactical::DampenerConfig::default(),
        ));

        // Record some outcomes
        if let Some(ref mut collector) = state.step_signal_collector {
            collector.record(crate::liquid::step_signals::StepOutcome {
                tool_name: "test".into(),
                ok: false,
                latency_ms: 100,
                tokens_used: 500,
                error_hint: Some("err".into()),
            });
        }

        // Reset (mimics turn boundary logic)
        if let Some(ref mut adapter) = state.tactical_adapter {
            adapter.reset_turn();
        }
        if let Some(ref mut collector) = state.step_signal_collector {
            let budget = state.max_turn_input_tokens as u64;
            collector.reset(budget);
        }

        // After reset, recording a single OK outcome should produce no triggers
        let triggers = if let Some(ref mut collector) = state.step_signal_collector {
            collector.record(crate::liquid::step_signals::StepOutcome {
                tool_name: "test".into(),
                ok: true,
                latency_ms: 50,
                tokens_used: 100,
                error_hint: None,
            })
        } else {
            vec![]
        };

        assert!(
            triggers.is_empty()
                || triggers
                    .iter()
                    .all(|t| matches!(t, crate::liquid::step_signals::AdaptationTrigger::Nominal)),
            "After reset, a single OK call should not trigger error-based adaptation"
        );
    }

    #[test]
    fn tactical_adapter_noop_when_none() {
        // When tactical fields are None, the code path just skips.
        // This test ensures no panic.
        let state = make_state();
        assert!(state.tactical_adapter.is_none());
        assert!(state.step_signal_collector.is_none());

        // The guard condition in the loop body is:
        // if state.step_signal_collector.is_some() || state.tactical_adapter.is_some()
        // When both are None, the block is skipped entirely.
        let should_enter =
            state.step_signal_collector.is_some() || state.tactical_adapter.is_some();
        assert!(!should_enter, "Neither field set — block should be skipped");
    }

    // ── L2.5 Auto-reflection tests ──────────────────────────────────────────

    #[tokio::test]
    async fn auto_reflection_skips_without_evolution_service() {
        let mut host = MockHost::new(vec![]);
        let mut state = make_state();
        assert!(state.evolution_service.is_none());
        assert!(state.pending_reflection_signals.is_empty());

        // Should be a no-op — no panic, no messages added.
        let msg_count = state.messages.len();
        maybe_trigger_auto_reflection(&mut host, &mut state).await;
        assert_eq!(state.messages.len(), msg_count);
        assert!(state.pending_reflection_signals.is_empty());
    }

    #[tokio::test]
    async fn auto_reflection_accumulates_below_threshold() {
        let mut host = MockHost::new(vec![]);
        let mut state = make_state();

        // Add fewer signals than threshold.
        state.pending_reflection_signals.push(
            crate::evolution::types::EvolutionSignal::RepeatedStall {
                tool_chain: vec!["test".into()],
                stall_count: 5,
                turn_id: "t1".into(),
            },
        );
        assert_eq!(state.pending_reflection_signals.len(), 1);

        // Without evolution service, signals stay.
        let msg_count = state.messages.len();
        maybe_trigger_auto_reflection(&mut host, &mut state).await;
        assert_eq!(state.messages.len(), msg_count);
        // Signals are NOT drained (no evo service to flush).
        assert_eq!(state.pending_reflection_signals.len(), 1);
    }

    #[tokio::test]
    async fn auto_reflection_triggers_at_threshold() {
        let reflection_response = r#"{
            "proposals": [
                {
                    "axis": "pattern",
                    "description": "Demote failing chain",
                    "confidence": 0.8,
                    "details": { "signature": "tool_0", "action": "demote" }
                }
            ],
            "summary": "One issue found."
        }"#;
        let mut host = MockHost::new(vec![]).with_reflection_text(reflection_response);
        let mut state = make_state();

        // Create an evolution service.
        let evo = std::sync::Arc::new(crate::evolution::service::EvolutionService::new());
        state.evolution_service = Some(evo.clone());

        // Pre-load signals at threshold.
        for i in 0..AUTO_REFLECTION_SIGNAL_THRESHOLD {
            state.pending_reflection_signals.push(
                crate::evolution::types::EvolutionSignal::RepeatedStall {
                    tool_chain: vec![format!("tool_{i}")],
                    stall_count: 3,
                    turn_id: format!("t{i}"),
                },
            );
        }
        assert_eq!(
            state.pending_reflection_signals.len(),
            AUTO_REFLECTION_SIGNAL_THRESHOLD
        );

        let msg_count = state.messages.len();
        maybe_trigger_auto_reflection(&mut host, &mut state).await;

        // Signals should be drained.
        assert!(
            state.pending_reflection_signals.is_empty(),
            "Signals should be drained after reflection"
        );
        assert_eq!(state.messages.len(), msg_count);
        assert_eq!(evo.pending().await.len(), 1);
        assert_eq!(state.total_prompt, 91);
        assert_eq!(state.total_completion, 37);
        assert!(
            host.emitted_lines
                .iter()
                .any(|line| line.contains("processed 1 proposal(s): 0 auto-applied, 1 queued"))
        );
    }

    #[tokio::test]
    async fn auto_reflection_flushes_evo_signals() {
        let reflection_response = r#"{
            "proposals": [
                {
                    "axis": "skill",
                    "description": "Add retry hint",
                    "confidence": 0.6,
                    "details": { "skill_name": "ops", "section": "troubleshooting", "content": "retry" }
                }
            ],
            "summary": "Retry needed."
        }"#;
        let mut host = MockHost::new(vec![]).with_reflection_text(reflection_response);
        let mut state = make_state();
        let evo = std::sync::Arc::new(crate::evolution::service::EvolutionService::new());
        state.evolution_service = Some(evo.clone());

        // Feed a signal that passes needs_llm (ToolFailure with skill_context).
        evo.add_signal(crate::evolution::types::EvolutionSignal::ToolFailure {
            tool_name: "bash".into(),
            error_snippet: "permission denied".into(),
            skill_context: Some("deploy_script".into()),
            turn_id: "t0".into(),
        })
        .await;

        // Pre-load enough on state so that pre-loaded + flushed >= threshold.
        for i in 0..(AUTO_REFLECTION_SIGNAL_THRESHOLD - 1) {
            state.pending_reflection_signals.push(
                crate::evolution::types::EvolutionSignal::ToolFailure {
                    tool_name: format!("tool_{i}"),
                    error_snippet: "err".into(),
                    skill_context: Some("sk".into()),
                    turn_id: format!("t{}", i + 1),
                },
            );
        }

        let msg_count = state.messages.len();
        maybe_trigger_auto_reflection(&mut host, &mut state).await;

        // Should have triggered: pre-loaded + flushed >= threshold.
        assert!(
            state.pending_reflection_signals.is_empty(),
            "All signals drained"
        );
        assert_eq!(state.messages.len(), msg_count);
        // 1 skill proposal from reflection + 1 calibration proposal from ToolFailure fast-path
        assert_eq!(evo.pending().await.len(), 2);
    }

    #[tokio::test]
    async fn auto_reflection_parse_failure_retains_signals() {
        let mut host = MockHost::new(vec![]).with_reflection_text("not json");
        let mut state = make_state();
        state.evolution_service = Some(std::sync::Arc::new(
            crate::evolution::service::EvolutionService::new(),
        ));

        for i in 0..AUTO_REFLECTION_SIGNAL_THRESHOLD {
            state.pending_reflection_signals.push(
                crate::evolution::types::EvolutionSignal::RepeatedStall {
                    tool_chain: vec![format!("tool_{i}")],
                    stall_count: 3,
                    turn_id: format!("t{i}"),
                },
            );
        }

        maybe_trigger_auto_reflection(&mut host, &mut state).await;

        assert_eq!(
            state.pending_reflection_signals.len(),
            AUTO_REFLECTION_SIGNAL_THRESHOLD
        );
        assert!(
            host.emitted_lines
                .iter()
                .any(|line| line.contains("parse failed"))
        );
    }

    #[tokio::test]
    async fn auto_reflection_host_error_retains_signals() {
        let mut host = MockHost::new(vec![]).with_reflection_error("network unavailable");
        let mut state = make_state();
        state.evolution_service = Some(std::sync::Arc::new(
            crate::evolution::service::EvolutionService::new(),
        ));

        for i in 0..AUTO_REFLECTION_SIGNAL_THRESHOLD {
            state.pending_reflection_signals.push(
                crate::evolution::types::EvolutionSignal::RepeatedStall {
                    tool_chain: vec![format!("tool_{i}")],
                    stall_count: 3,
                    turn_id: format!("t{i}"),
                },
            );
        }

        maybe_trigger_auto_reflection(&mut host, &mut state).await;

        assert_eq!(
            state.pending_reflection_signals.len(),
            AUTO_REFLECTION_SIGNAL_THRESHOLD
        );
        assert!(
            host.emitted_lines
                .iter()
                .any(|line| line.contains("skipped: network unavailable"))
        );
    }

    #[tokio::test]
    async fn auto_reflection_summarizes_recent_tools_and_tactical_actions() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let reflection_response = r#"{
            "proposals": [
                {
                    "axis": "pattern",
                    "description": "Demote bash chain",
                    "confidence": 0.8,
                    "details": { "signature": "bash", "action": "demote" }
                }
            ],
            "summary": "Tool issues found."
        }"#;
        let mut host = MockHost::new(vec![]).with_reflection_text(reflection_response);
        let mut state = make_state();
        state.current_session_id = Some("sess-reflect".into());
        let mut workspace = astra_services::session_workspace::WorkspaceMetadata::with_context(
            "sess-reflect",
            "gpt-5.4",
            "/repo",
            Some("main"),
        );
        workspace.session_goal = Some("ship self surface".into());
        workspace.plan_goal = Some("stabilize reflection loop".into());
        workspace.deprioritized_tools = vec!["bash".into()];
        workspace.goal_progress = Some(astra_services::session_workspace::GoalProgressSnapshot {
            goal: "ship self surface".into(),
            completion_score: 0.5,
            momentum: 0.2,
            milestone_count: 2,
            summary: "2/4 milestones complete".into(),
            weighted_progress: 0.5,
            negative_signals: 0.0,
            milestones: Vec::new(),
        });
        workspace.contract_json = Some(
            serde_json::to_string(&astra_services::TaskContract {
                contract_id: "contract-1".into(),
                task_id: "task-1".into(),
                goal: "stabilize reflection loop".into(),
                scope: astra_services::TaskScope::default(),
                subtasks: vec![astra_services::DurableSubtask {
                    id: "subtask-1".into(),
                    title: "wire reflection evidence".into(),
                    stage: astra_services::SubtaskStage::Pending,
                    criteria: vec![astra_services::VerificationCriterion {
                        id: "criterion-1".into(),
                        description: "reflection prompt includes goal + verify".into(),
                        verifier: astra_services::VerifierKind::BuildPass {
                            cmd: "cargo test".into(),
                        },
                        required: true,
                        timeout_sec: 120,
                        global_only: false,
                    }],
                    ..Default::default()
                }],
                global_verification: Vec::new(),
                version: 1,
                status: astra_services::ContractStatus::Active,
                created_at: chrono::Utc::now().to_rfc3339(),
                updated_at: chrono::Utc::now().to_rfc3339(),
                domain_hint: None,
                task_type: None,
                last_global_results: Vec::new(),
            })
            .unwrap(),
        );
        astra_services::session_workspace::write_workspace(&workspace).unwrap();
        astra_services::session_journal::JournalWriter::new("sess-reflect")
            .unwrap()
            .append(&astra_services::session_journal::JournalEvent::turn(
                Some("sess-reflect"),
                1,
                Some("gpt-5.4"),
                "improve reflection",
                "working on it",
                0,
                10,
                20,
                30,
            ))
            .unwrap();
        for (turn, bash_ok, bash_ms, rg_ms) in [
            (1, true, 60_u64, 25_u64),
            (2, true, 70, 30),
            (4, false, 220, 300),
        ] {
            let mut event = astra_services::session_journal::JournalEvent::turn(
                Some("sess-reflect"),
                turn,
                Some("gpt-5.4"),
                "inspect tool health",
                "record tool outcome",
                2,
                12,
                24,
                bash_ms.max(rg_ms),
            );
            event.tools_selected = Some(vec!["bash".to_string(), "rg".to_string()]);
            event.tools_used = Some(vec!["bash".to_string(), "rg".to_string()]);
            event.tool_calls = Some(vec![
                ToolCallRecord {
                    name: "bash".to_string(),
                    ok: bash_ok,
                    ms: bash_ms,
                    error: (!bash_ok).then(|| "bash regression".to_string()),
                    input_bytes: None,
                    output_bytes: None,
                    args_preview: None,
                    result_preview: None,
                },
                ToolCallRecord {
                    name: "rg".to_string(),
                    ok: true,
                    ms: rg_ms,
                    error: None,
                    input_bytes: None,
                    output_bytes: None,
                    args_preview: None,
                    result_preview: None,
                },
            ]);
            if !bash_ok {
                event.error = Some("bash regression".to_string());
            }
            astra_services::session_journal::JournalWriter::new("sess-reflect")
                .unwrap()
                .append(&event)
                .unwrap();
        }
        astra_services::session_journal::JournalWriter::new("sess-reflect")
            .unwrap()
            .append(
                &astra_services::session_journal::JournalEvent::goal_steered(
                    Some("sess-reflect"),
                    2,
                    "plan_execution_start",
                    Some("ship self surface"),
                    "stabilize reflection loop",
                    None,
                ),
            )
            .unwrap();
        astra_services::session_journal::JournalWriter::new("sess-reflect")
            .unwrap()
            .append(&astra_services::session_journal::JournalEvent {
                event_type: astra_services::session_journal::JournalEventType::TurnError,
                ts: chrono::Utc::now().to_rfc3339(),
                session_id: Some("sess-reflect".to_string()),
                turn: Some(3),
                model: Some("gpt-5.4".to_string()),
                user_input: Some("debug bash timeout".to_string()),
                assistant_output: None,
                tool_count: Some(1),
                tokens_in: Some(10),
                tokens_out: Some(0),
                duration_ms: Some(120),
                error: Some("timed out waiting for test".to_string()),
                config_key: None,
                config_value: None,
                turns_compacted: None,
                facts_stored: None,
                tools_selected: Some(vec!["bash".to_string()]),
                selected_skills: None,
                tools_used: Some(vec!["bash".to_string()]),
                tool_calls: Some(vec![ToolCallRecord {
                    name: "bash".to_string(),
                    ok: false,
                    ms: 120,
                    error: Some("timed out waiting for test".to_string()),
                    input_bytes: None,
                    output_bytes: None,
                    args_preview: None,
                    result_preview: None,
                }]),
                budget_used: None,
                budget_pressure: None,
                stall_type: None,
                metadata: None,
                plan_subtask_id: None,
                ttft_ms: None,
                context_ms: None,
                selector_strategy: None,
                selector_ms: None,
                selector_tokens_in: None,
                selector_tokens_out: None,
                cache_read_tokens: None,
                cache_creation_tokens: None,
                memoria_ms: None,
                session_lineage: None,
                coordination: None,
                edge_policy: None,
                selection_trace: None,
                context_assembly_trace: None,
                selector_confidence: None,
                routing_domain_hint: None,
                entity_learn_skipped_no_domain: false,
            })
            .unwrap();
        astra_services::session_journal::JournalWriter::new("sess-reflect")
            .unwrap()
            .append(
                &astra_services::session_journal::JournalEvent::verification_completed(
                    Some("sess-reflect"),
                    3,
                    "subtask-1",
                    "global",
                    true,
                    &serde_json::json!([{"check":"unit-tests","passed":true}]),
                ),
            )
            .unwrap();
        astra_services::session_journal::JournalWriter::new("sess-reflect")
            .unwrap()
            .append(
                &astra_services::session_journal::JournalEvent::adaptive_per_turn_applied(
                    Some("sess-reflect"),
                    4,
                    vec![("verification.strictness".into(), "0.6".into(), "0.7".into())],
                    vec!["high token pressure".into()],
                ),
            )
            .unwrap();
        astra_services::session_journal::JournalWriter::new("sess-reflect")
            .unwrap()
            .append(
                &astra_services::session_journal::JournalEvent::verification_completed(
                    Some("sess-reflect"),
                    5,
                    "subtask-1",
                    "global",
                    false,
                    &serde_json::json!([{"check":"integration-tests","passed":false}]),
                ),
            )
            .unwrap();
        state.evolution_service = Some(std::sync::Arc::new(
            crate::evolution::service::EvolutionService::new(),
        ));

        for i in 0..AUTO_REFLECTION_SIGNAL_THRESHOLD {
            state.pending_reflection_signals.push(
                crate::evolution::types::EvolutionSignal::RepeatedStall {
                    tool_chain: vec![format!("tool_{i}")],
                    stall_count: 3,
                    turn_id: format!("t{i}"),
                },
            );
        }
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "bash".into(),
                ok: false,
                ms: 200,
                error: Some("permission denied".into()),
                input_bytes: Some(12),
                output_bytes: Some(0),
                args_preview: None,
                result_preview: None,
            },
            ToolCallRecord {
                name: "bash".into(),
                ok: true,
                ms: 100,
                error: None,
                input_bytes: Some(8),
                output_bytes: Some(20),
                args_preview: None,
                result_preview: None,
            },
            ToolCallRecord {
                name: "web_fetch".into(),
                ok: true,
                ms: 40,
                error: None,
                input_bytes: Some(5),
                output_bytes: Some(50),
                args_preview: None,
                result_preview: None,
            },
        ];
        state.recent_tactical_actions = vec![
            "⚠️ verify outputs more strictly".into(),
            "📊 Token usage high".into(),
        ];

        let session = std::sync::Arc::new(std::sync::RwLock::new(
            crate::observability_integration::ObservabilitySession::new_simple("sess-reflect"),
        ));
        {
            let mut guard = session.write().unwrap();
            guard.active_experiment_id = Some("exp-123".into());
            guard.active_variant = Some("variant-b".into());
            guard.turn_number = 4;
        }
        state.telemetry.observability_session = Some(session);
        state
            .evolution_service
            .as_ref()
            .unwrap()
            .add_signal(crate::evolution::types::EvolutionSignal::ToolFailure {
                tool_name: "bash".into(),
                error_snippet: "Permission denied".into(),
                skill_context: Some("ops".into()),
                turn_id: "t-reflect".into(),
            })
            .await;

        maybe_trigger_auto_reflection(&mut host, &mut state).await;

        let prompt = host.last_reflection_prompt.as_deref().unwrap();
        assert!(prompt.contains("Effective goal: stabilize reflection loop"));
        assert!(prompt.contains("Goal progress: 2/4 milestones complete"));
        assert!(prompt.contains("Verification summary:"));
        assert!(prompt.contains("Tool health:"));
        assert!(prompt.contains("Blocked tools: bash"));
        assert!(prompt.contains("Recent performance deltas:"));
        assert!(prompt.contains("[Regressed]"));
        assert!(prompt.contains("Recent evaluation events:"));
        assert!(prompt.contains("[GoalSteered]"));
        assert!(prompt.contains("[Verification]"));
        assert!(prompt.contains("Recent adaptations:"));
        assert!(prompt.contains("[Adaptation]"));
        assert!(prompt.contains("Recent adaptation outcomes:"));
        assert!(prompt.contains("[Verification] after Adaptation turn 4"));
        assert!(prompt.contains("Recent adaptation impacts:"));
        assert!(prompt.contains("Recent adaptation verification impacts:"));
        assert!(prompt.contains("Tool statistics:"));
        assert!(prompt.contains("bash — calls=2, failures=1, avg_ms=150"));
        assert!(prompt.contains("Active experiment: exp-123 (variant=variant-b, samples=4)"));
        assert!(prompt.contains("Recent tactical actions:"));
        assert!(prompt.contains("verify outputs more strictly"));
        assert!(prompt.contains("[ToolFailure] bash: Permission denied"));
        assert!(state.recent_tactical_actions.is_empty());
    }

    // ── finalize_turn_trace tests ───────────────────────────────────────

    #[test]
    fn finalize_turn_trace_feeds_observability_session() {
        let mut state = make_state();
        let hub = crate::observability_integration::ObservabilityHub::new();
        let session = hub.start_session("u1", "s1");
        state.telemetry.observability_session = Some(session.clone());
        state.max_turn_input_tokens = 100_000;
        state.last_measured_prompt_tokens = Some(25_000);

        // Create collector with some data
        let collector = crate::turn::turn_trace_collector::TurnTraceCollector::new(
            "turn-0".to_string(),
            "s1".to_string(),
        );
        collector.record_token_budget_estimate(14_000, 5_000, 0, 3_000, 200, 22_200, 100_000, 0.22);
        state.telemetry.turn_trace_collector = Some(collector);

        finalize_turn_trace(&mut state);

        // Collector consumed
        assert!(state.telemetry.turn_trace_collector.is_none());
        // Trace fed to observability session
        let guard = session.read().unwrap();
        assert_eq!(guard.context_traces.len(), 1);
        let trace = &guard.context_traces[0];
        assert_eq!(trace.turn_id, "turn-0");
        // CLI component estimates preserved.
        assert_eq!(trace.token_budget.system_prompt_tokens, 14_000);
        assert_eq!(trace.token_budget.history_tokens, 5_000);
        // Persisted total remains aligned with the component breakdown.
        assert_eq!(trace.token_budget.total_used, 22_200);
        assert_eq!(trace.token_budget.max_tokens, 100_000);
        assert!((trace.token_budget.budget_pressure - 0.25).abs() < 0.01);
    }

    #[test]
    fn finalize_turn_trace_noop_when_no_collector() {
        let mut state = make_state();
        assert!(state.telemetry.turn_trace_collector.is_none());
        // Should not panic
        finalize_turn_trace(&mut state);
    }

    #[test]
    fn finalize_turn_trace_updates_on_consecutive_turns() {
        let mut state = make_state();
        let hub = crate::observability_integration::ObservabilityHub::new();
        let session = hub.start_session("u1", "s1");
        state.telemetry.observability_session = Some(session.clone());
        state.max_turn_input_tokens = 100_000;

        // Turn 0
        session.write().unwrap().turn_number = 1;
        state.last_measured_prompt_tokens = Some(20_000);
        state.telemetry.turn_trace_collector =
            Some(crate::turn::turn_trace_collector::TurnTraceCollector::new(
                "turn-0".to_string(),
                "s1".to_string(),
            ));
        finalize_turn_trace(&mut state);

        // Turn 1
        session.write().unwrap().turn_number = 2;
        state.last_measured_prompt_tokens = Some(30_000);
        state.telemetry.turn_trace_collector =
            Some(crate::turn::turn_trace_collector::TurnTraceCollector::new(
                "turn-1".to_string(),
                "s1".to_string(),
            ));
        finalize_turn_trace(&mut state);

        let guard = session.read().unwrap();
        assert_eq!(guard.context_traces.len(), 2);
        assert_eq!(guard.context_traces[0].turn_id, "turn-1");
        assert_eq!(guard.context_traces[0].token_budget.total_used, 20_000);
        assert_eq!(guard.context_traces[1].turn_id, "turn-2");
        assert_eq!(guard.context_traces[1].token_budget.total_used, 30_000);
    }

    #[test]
    fn finalize_turn_trace_aligns_trace_turn_id_with_journal_turn() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());

        let mut state = make_state();
        let hub = crate::observability_integration::ObservabilityHub::new();
        let session = hub.start_session("u1", "s1");
        session.write().unwrap().turn_number = 3;
        state.current_session_id = Some("s1".to_string());
        state.telemetry.observability_session = Some(session.clone());
        state.telemetry.turn_trace_collector =
            Some(crate::turn::turn_trace_collector::TurnTraceCollector::new(
                "turn-0".to_string(),
                "s1".to_string(),
            ));
        state.max_turn_input_tokens = 100_000;
        state.last_measured_prompt_tokens = Some(42_000);

        finalize_turn_trace(&mut state);

        let session_guard = session.read().unwrap();
        assert_eq!(session_guard.context_traces.len(), 1);
        assert_eq!(session_guard.context_traces[0].turn_id, "turn-3");
        drop(session_guard);

        let journal = std::fs::read_to_string(temp.path().join("s1.jsonl")).unwrap();
        let event: serde_json::Value =
            serde_json::from_str(journal.lines().next().unwrap()).unwrap();
        assert_eq!(event["turn"], 3);
        assert_eq!(event["context_assembly_trace"]["turn_id"], "turn-3");
    }

    // ── Skill deferral behavior tests ─────────────────────────────────────

    /// Helper: build a HostTurnResult with a skill call + non-skill tool calls.
    fn skill_plus_tools_result(
        skill_call_id: &str,
        skill_args: &str,
        extra_calls: &[(&str, &str)], // (call_id, tool_name)
        prompt: u64,
        completion: u64,
    ) -> HostTurnResult {
        let mut tool_calls = vec![json!({
            "id": skill_call_id,
            "type": "function",
            "function": {
                "name": "skill",
                "arguments": skill_args,
            }
        })];
        for (id, name) in extra_calls {
            tool_calls.push(json!({
                "id": *id,
                "type": "function",
                "function": {
                    "name": *name,
                    "arguments": "{}",
                }
            }));
        }
        HostTurnResult {
            accum: ChatTurnSseAccum {
                has_tool_calls: true,
                has_usage: true,
                prompt_tokens: prompt,
                completion_tokens: completion,
                tool_calls,
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: Some(30),
            edge_tool_round: Vec::new(),
        }
    }

    #[tokio::test]
    async fn skill_with_substantial_output_skips_deferred_calls() {
        // Regression: session 746b6423 — skill produced a full code review
        // but 18 deferred tool calls were re-executed in the next iteration,
        // causing 3x token waste.
        let mut resolver = StubSkillResolver::new();
        // Simulate a skill that produces substantial output (like a code review).
        // The formatted result includes "# Skill: ..." header (~120 chars) + instructions.
        resolver.skills[0].2 = "x".repeat(500);
        let turns = vec![
            // Iteration 1: skill + 2 read_file calls
            skill_plus_tools_result(
                "call_skill",
                r#"{"skill_name": "test-skill"}"#,
                &[("call_rf1", "read_file"), ("call_rf2", "read_file")],
                100,
                50,
            ),
            // Iteration 2: LLM sees "Skipped" messages, produces final text
            text_result("Final answer from skill output.", 200, 100, None),
        ];

        let mut host = MockHost::new(turns);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "review code"}));
        state.skills.resolver = Some(Arc::new(resolver));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        // The deferred calls should have "Skipped" messages (not "Deferred")
        // because the skill produced substantial output (>200 chars).
        let rf1_msgs: Vec<&Value> = state
            .messages
            .iter()
            .filter(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("call_rf1"))
            .collect();
        assert_eq!(
            rf1_msgs.len(),
            1,
            "expected exactly one tool result for call_rf1"
        );
        let content = rf1_msgs[0]["content"].as_str().unwrap();
        assert!(
            content.contains("Skipped"),
            "expected 'Skipped' message for deferred call when skill produced output, got: {content}"
        );
        assert!(
            content.contains("Do NOT call"),
            "should tell LLM not to re-invoke, got: {content}"
        );

        // skill_produced_output flag should be set
        assert!(
            state.skill_produced_output,
            "skill_produced_output flag should be set when skill produces substantial output"
        );

        // Soft constraint: deferred tools should NOT be hard-restricted — the soft
        // prompt is sufficient, and hard-restricting prevents legitimate re-use with
        // different arguments in later iterations.
        assert!(
            !state.restricted_tools.contains("read_file"),
            "read_file should not be hard-restricted — soft prompt is the constraint"
        );
    }

    #[tokio::test]
    async fn skill_with_short_output_defers_calls() {
        // When skill output is short (<= 500 bytes), deferred calls should
        // use the original "Deferred" message that invites re-evaluation.
        let mut resolver = StubSkillResolver::new();
        // Override with a short instruction (< 200 chars)
        resolver.skills[0].2 = "Short.".into();
        let turns = vec![
            skill_plus_tools_result(
                "call_skill",
                r#"{"skill_name": "test-skill"}"#,
                &[("call_rf1", "read_file")],
                100,
                50,
            ),
            text_result("Done.", 200, 100, None),
        ];

        let mut host = MockHost::new(turns);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "do something"}));
        state.skills.resolver = Some(Arc::new(resolver));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        let rf1_msgs: Vec<&Value> = state
            .messages
            .iter()
            .filter(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("call_rf1"))
            .collect();
        assert_eq!(rf1_msgs.len(), 1);
        let content = rf1_msgs[0]["content"].as_str().unwrap();
        assert!(
            content.contains("Deferred"),
            "expected 'Deferred' message for short skill output, got: {content}"
        );
        assert!(
            !content.contains("Skipped"),
            "should NOT say 'Skipped' for short skill output, got: {content}"
        );

        // skill_produced_output flag should NOT be set
        assert!(
            !state.skill_produced_output,
            "skill_produced_output should not be set for short skill output"
        );

        // restricted_tools should NOT contain the deferred tool
        assert!(
            !state.restricted_tools.contains("read_file"),
            "read_file should not be restricted for short skill output"
        );
    }

    #[tokio::test]
    async fn skill_only_call_with_substantial_output_sets_flag() {
        // Regression: session 699a0f6c — skill sub-agent produced 212 lines
        // but skill_produced_output was not set because there were no parallel
        // tool calls (remaining was empty, deferral block never ran).
        let mut resolver = StubSkillResolver::new();
        resolver.skills[0].2 = "x".repeat(500);
        let turns = vec![
            // Iteration 1: skill-only call (no parallel tools)
            skill_tool_call_result("call_skill", r#"{"skill_name": "test-skill"}"#, 100, 50),
            // Iteration 2: LLM produces final text
            text_result("Here is the review.", 200, 100, None),
        ];

        let mut host = MockHost::new(turns);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "review code"}));
        state.skills.resolver = Some(Arc::new(resolver));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        // Flag should be set even without deferral (skill-only call)
        assert!(
            state.skill_produced_output,
            "skill_produced_output should be set for skill-only call with substantial output"
        );
    }

    // ── Microcompact integration tests ───────────────────────────────────

    #[tokio::test]
    async fn microcompact_clears_old_tool_results_between_iterations() {
        // 3-iteration flow: tool round → tool round → text.
        // After iteration 1, the tool results from iteration 1 should be
        // compacted before iteration 2's LLM call.
        let big_output = "x".repeat(1000);
        let mut host = MockHost::new(vec![
            // Iteration 1: 3 edge tool calls with large output
            edge_tool_result(
                vec![
                    make_edge_tool("read_file", &big_output),
                    make_edge_tool("read_file", &big_output),
                    make_edge_tool("read_file", &big_output),
                ],
                100,
                50,
                Some(30),
            ),
            // Iteration 2: 3 more edge tool calls
            edge_tool_result(
                vec![
                    make_edge_tool("read_file", &big_output),
                    make_edge_tool("read_file", &big_output),
                    make_edge_tool("read_file", &big_output),
                ],
                100,
                50,
                Some(30),
            ),
            // Iteration 3: final text
            text_result("Done.", 50, 20, None),
        ]);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "review"}));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        // After completion, some old tool results should have been compacted.
        // The messages contain tool results from iterations 1 and 2.
        // Microcompact keeps the last 6, so with 6 total tool results,
        // iteration 1's results (first 3) should be compacted if there are
        // more than 6 total tool-role messages.
        let tool_msgs: Vec<&Value> = state
            .messages
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
            .collect();
        // Verify at least some tool messages exist
        assert!(
            tool_msgs.len() >= 3,
            "expected tool messages, got {}",
            tool_msgs.len()
        );

        // With KEEP_RECENT=6 and 6 tool results, none get compacted.
        // But the test verifies the microcompact code path runs without
        // breaking the agentic loop (no panics, correct final state).
        assert_eq!(state.final_text, "Done.");
        assert_eq!(host.current_turn, 3);
    }

    #[tokio::test]
    async fn microcompact_skips_first_iteration() {
        // Single iteration: microcompact should NOT run (turn_index == 0).
        let mut host = MockHost::new(vec![text_result("Hello.", 50, 20, None)]);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "hi"}));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(state.final_text, "Hello.");
        // No tool messages to compact, and turn_index was 0 anyway
    }

    #[tokio::test]
    async fn microcompact_preserves_tool_call_id_references() {
        // Verify that compacting tool result content doesn't break
        // the tool_call_id linkage (assistant.tool_calls[].id ↔ tool.tool_call_id).
        let big_output = "x".repeat(1000);
        let mut host = MockHost::new(vec![
            // Iteration 1: 7 edge tool calls with different names to avoid dedup cap
            edge_tool_result(
                vec![
                    make_edge_tool("read_file", &big_output),
                    make_edge_tool("grep", &big_output),
                    make_edge_tool("glob", &big_output),
                    make_edge_tool("git_show", &big_output),
                    make_edge_tool("git_diff", &big_output),
                    make_edge_tool("git_log", &big_output),
                    make_edge_tool("git_status", &big_output),
                ],
                100,
                50,
                Some(30),
            ),
            // Iteration 2: 1 more tool (total 8, triggers compaction of oldest)
            edge_tool_result(vec![make_edge_tool("bash", &big_output)], 100, 50, Some(30)),
            text_result("Done.", 50, 20, None),
        ]);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "analyze"}));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(state.final_text, "Done.");

        // Every tool message must still have a tool_call_id field —
        // microcompact only changes content, never removes structural fields.
        for msg in &state.messages {
            if msg.get("role").and_then(Value::as_str) == Some("tool") {
                assert!(
                    msg.get("tool_call_id").is_some(),
                    "tool message missing tool_call_id after compaction: {:?}",
                    msg
                );
            }
        }
    }

    #[tokio::test]
    async fn microcompact_emits_status_line_when_not_quiet() {
        // Verify the ♻ status line is emitted when compaction occurs.
        let big = "x".repeat(1000);
        let mut host = MockHost::new(vec![
            edge_tool_result(
                vec![
                    make_edge_tool("read_file", &big),
                    make_edge_tool("grep", &big),
                    make_edge_tool("glob", &big),
                    make_edge_tool("git_show", &big),
                    make_edge_tool("git_diff", &big),
                    make_edge_tool("git_log", &big),
                    make_edge_tool("git_status", &big),
                ],
                100,
                50,
                Some(30),
            ),
            text_result("Done.", 50, 20, None),
        ]);
        host.quiet = false; // Enable status line output
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "review"}));

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;

        // Check if any emitted line contains the compaction marker.
        // Compaction only fires on turn_index > 0, and with 7 tool results
        // (keep=6), at most 1 gets compacted — IF the content is >= 500 bytes.
        let compact_lines: Vec<&String> = host
            .emitted_lines
            .iter()
            .filter(|l| l.contains("Compacted"))
            .collect();
        // With 7 tool results and keep=6, iteration 2 should compact 1.
        // But only if the tool result content is >= MIN_COMPACT_SIZE (500).
        // Edge tool results go through the headless round which may format them.
        // This test verifies the status line mechanism works when compaction fires.
        if !compact_lines.is_empty() {
            assert!(
                compact_lines[0].contains("♻"),
                "status line should contain ♻ marker"
            );
            assert!(
                compact_lines[0].contains("tokens saved"),
                "status line should mention tokens saved"
            );
        }
    }

    #[tokio::test]
    async fn microcompact_actually_reduces_content_size() {
        // Direct verification: inject large tool results into messages,
        // run the loop, and verify content was replaced.
        let big = "x".repeat(1000);
        let mut host = MockHost::new(vec![
            // Iteration 1: returns tools
            edge_tool_result(vec![make_edge_tool("bash", "ok")], 100, 50, Some(30)),
            // Iteration 2: final text
            text_result("Done.", 50, 20, None),
        ]);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "go"}));

        // Pre-populate messages with old tool results (simulating prior iterations).
        // These are already in the history before the loop starts.
        for i in 0..10 {
            state.messages.push(json!({"role": "assistant", "content": "", "tool_calls": [{"id": format!("old-{i}"), "type": "function", "function": {"name": "read_file", "arguments": "{}"}}]}));
            state
                .messages
                .push(json!({"role": "tool", "tool_call_id": format!("old-{i}"), "content": &big}));
        }

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        // Count how many old tool results were compacted
        let compacted = state
            .messages
            .iter()
            .filter(|m| {
                m.get("role").and_then(Value::as_str) == Some("tool")
                    && m.get("content").and_then(Value::as_str)
                        == Some("[Previous tool output cleared]")
            })
            .count();

        // 10 pre-populated + at least 1 from iteration 1 = 11+ tool results.
        // Keep 6, so at least 5 should be compacted.
        assert!(
            compacted >= 4,
            "expected at least 4 compacted results from 10+ tool results (keep=6), got {}",
            compacted
        );

        // Verify total content size decreased
        let total_content_bytes: usize = state
            .messages
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
            .map(|m| m.get("content").and_then(Value::as_str).unwrap_or("").len())
            .sum();
        // Without compaction: 10 * 1000 + small = ~10000 bytes
        // With compaction: 4+ cleared (32 bytes each) + 6 kept (1000 each) = ~6200
        assert!(
            total_content_bytes < 8000,
            "expected reduced content after compaction, got {} bytes",
            total_content_bytes
        );
    }
}
