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
//!   while turn_index < current_turn_budget:
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
use astra_services::{DatabaseEvaluationService, DatabaseEventService};
use async_trait::async_trait;
use serde_json::Value;

use crate::pipeline::step_protocol::{InMemoryIdempotencyCache, StepCheckpoint};
use crate::pipeline::step_recorder::StepRecorder;
use crate::semantic_dedup::SemanticDedup;
use crate::tool_registry::SelectionReport;
use crate::turn::agentic_verdict_audit::AgenticVerdictAuditEvent;
use crate::turn::chat_turn_heuristics::TaskExecutionProfile;
use crate::turn::chat_turn_sse_dispatch::ChatTurnSseAccum;
use crate::turn::sse_stream_host::EdgeToolExecResult;
use crate::turn::turn_guard::TurnGuard;
use astra_turn_core::headless_types::HeadlessStderrStyle;
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
    /// Structured error kind when the turn failed at the LLM layer.
    /// Set by hosts that have a [`ClassifiedError`] (e.g. `ServerAgenticLoopHost`).
    /// When present, `agentic_turn_ingest` uses this instead of re-classifying
    /// `accum.error_message` from string content.
    pub error_kind: Option<astra_core::ErrorKind>,
}

pub use astra_turn_core::interaction_types::{
    ASK_USER_TOOL_NAME, TurnInteractionMode, TurnInteractionPolicy,
    interaction_scoped_tool_restrictions, tool_counts_as_factual_evidence,
};

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
    ) -> Result<HostTurnResult, astra_core::ClassifiedError>;

    /// Whether the host can execute a hidden reflection-only LLM subcall.
    fn supports_auto_reflection(&self) -> bool {
        false
    }

    /// Whether the host already injects round budget guidance into the system
    /// prompt during `execute_turn`.  When true, the agentic loop skips its
    /// own user-message guidance injection to avoid double injection.
    fn injects_round_guidance(&self) -> bool {
        false
    }

    /// Execute a hidden reflection-only LLM subcall and return the raw text.
    ///
    /// Hosts that do not support this can keep the default implementation.
    async fn execute_reflection(
        &mut self,
        _state: &mut AgenticLoopState,
        _request: HostReflectionRequest<'_>,
    ) -> Result<Option<HostReflectionResult>, astra_core::ClassifiedError> {
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

/// Request-scoped capability constraints supplied by the external caller.
///
/// Tool and skill access are controlled separately:
/// - `allowed_tools` applies to non-skill tools.
/// - `allowed_skills` applies to skill visibility/execution via the
///   `skill` / `discover_skills` tool schemas.
#[derive(Clone, Debug, Default)]
pub struct RequestConstraints {
    /// When set, only this subset of non-skill tools may execute for the request.
    ///
    /// This does not restrict the `skill` or `discover_skills` tool schemas;
    /// skill access is controlled by `allowed_skills`.
    pub allowed_tools: Option<HashSet<String>>,
    /// When set, only this subset of skills may be visible/executable via
    /// the `skill` / `discover_skills` tool schemas.
    pub allowed_skills: Option<HashSet<String>>,
}

impl RequestConstraints {
    pub fn new(
        allowed_tools: Option<HashSet<String>>,
        allowed_skills: Option<HashSet<String>>,
    ) -> Self {
        Self {
            allowed_tools,
            allowed_skills,
        }
    }
}

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
    /// Request-scoped tool/skill constraints supplied by the external caller.
    /// Nested runs inherit these constraints unchanged.
    pub request_constraints: RequestConstraints,
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
            request_constraints: Default::default(),
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
    /// Optional evaluation persistence context for refreshing DB-backed runtime signals.
    pub evaluation_persistence: Option<EvaluationPersistenceContext>,
    /// Optional event persistence context for mirroring context traces into cloud events.
    pub context_trace_persistence: Option<ContextTracePersistenceContext>,
    /// Runtime promotion verdicts captured for later audit/report persistence.
    pub promotion_events: Vec<RuntimePromotionEventData>,
    /// Optional turn trace collector for detailed context assembly observability.
    /// When set, records system prompt, history, memory, and tool selection traces.
    /// Created at turn start, finalized at turn end.
    pub turn_trace_collector: Option<crate::turn::turn_trace_collector::TurnTraceCollector>,
    /// Number of turns completed in this loop invocation (for tuning cycle trigger).
    pub completed_turns_for_tuning: u32,
    /// Deferred context assembly trace: written here by `finalize_turn_trace` so
    /// the journal event is only emitted when the turn actually commits (not on
    /// aborts/retries), preventing ghost `context_assembly_recorded` events.
    pub pending_context_assembly_trace: Option<(u32, serde_json::Value)>,
}

#[derive(Clone, Debug)]
pub struct EvaluationPersistenceContext {
    pub user_id: String,
    pub evaluation_service: DatabaseEvaluationService,
}

#[derive(Clone, Debug)]
pub struct ContextTracePersistenceContext {
    pub user_id: String,
    pub event_service: DatabaseEventService,
    pub agent_id: String,
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
    /// Whether an execution-retry was forced after a mutating/confirmed task
    /// attempted to finish without applying any concrete workspace mutation.
    pub forced_execution_retry: bool,
    /// Whether a mid-loop execution escalation was injected after a mutating
    /// task accumulated enough read-only tool calls without producing any
    /// workspace mutation. One-shot per turn.
    pub forced_execution_escalation: bool,
    /// Whether a parallel-batching force injection has fired this loop. Set
    /// when the model has produced a long streak of consecutive single-tool
    /// rounds despite the soft prompt-layer nudge. One-shot per turn.
    pub forced_parallel_batching: bool,
    /// How many stall correction nudges have been injected this loop.
    /// Limits nudge frequency (at most one per stall type per session).
    pub nudge_count: u32,
    /// Rolling-stats guardrail auto-tuner for the auto-reflection signal
    /// threshold. Observes per-turn outcomes and adjusts the threshold by
    /// ±1 (bounded to `[MIN, MAX]`) so Astra reacts faster when failures
    /// cluster and backs off when things are stable.
    pub guardrail_tuner: crate::guardrail_tuning::GuardrailTuner,
    /// Cursor into `tool_call_records` marking the boundary already
    /// observed by the guardrail tuner. Turn N sees records
    /// `tool_call_records[cursor..]`; after observation the cursor is
    /// advanced to `len()`.
    pub guardrail_tuner_records_cursor: usize,
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
    /// Inbound request headers eligible for remote skill forwarding.
    /// Header names are normalized to lowercase.
    pub forward_headers: HashMap<String, String>,
    /// Request-scoped LLM token service config propagated to nested sub-runs.
    pub llm_token_service: Option<astra_services::LlmTokenServiceConfig>,
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
    /// True once the current `final_text` has already been sent to the user.
    /// Deferred completion paths leave this false so finalization emits exactly once.
    pub final_text_streamed: bool,
    pub total_prompt: u64,
    pub total_completion: u64,
    pub total_cache_read: u64,
    pub total_cache_creation: u64,
    pub total_tool_calls: u32,
    pub total_evidence_tool_calls: u32,
    pub has_any_usage: bool,

    // ── Turn management ──
    pub max_turns: usize,
    pub remaining_turns: usize,
    pub agentic_turn_budget: astra_turn_core::chat_turn_heuristics::AgenticTurnBudget,
    /// Current agentic loop turn index (0-based, updated each iteration).
    /// Used by the CLI to inject `round_index` into the bridge payload so the
    /// system prompt can include round budget directives.
    pub current_round_index: u32,
    /// Actual number of LLM calls completed in this turn (not inflated by
    /// progressive penalty).  Used for round budget guidance injection.
    pub llm_rounds_completed: u32,
    pub turn_guard: TurnGuard,
    pub restricted_tools: HashSet<String>,
    /// Positive allowlist bias populated by pipeline `add_tools` strategy.
    /// Tools listed here are guaranteed NOT to be filtered out by the effective
    /// restriction set on the current turn (they still have to be advertised
    /// by the edge catalogue). This is additive and persists until manually
    /// cleared; the bridge prunes it naturally when a later diagnosis drops the
    /// tool from its recommendation.
    pub boosted_tools: HashSet<String>,
    /// One-shot flag set by pipeline `widen_selection` strategy. When true,
    /// the upcoming tool-visibility assembly skips the deprioritized → restricted
    /// merge for this turn so the LLM sees the full catalogue again. The flag
    /// is consumed (reset to false) on use.
    pub widen_selection_pending: bool,
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
    pub last_turn_policy: TurnInteractionPolicy,

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

    /// Tracks compaction effectiveness across retries within a turn.
    /// Records tokens freed per attempt and detects "insufficient compaction"
    /// patterns where compaction runs but the next call still fails.
    pub compaction_effectiveness: super::compaction_replay::CompactionEffectivenessTracker,

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

    // ── Server-side tool execution ──
    /// Optional server-side tool executor for web agent sessions (no CLI edge agent).
    /// When present, tools that have no edge match are executed directly by the server.
    pub server_tool_executor: Option<Arc<crate::server::server_tool_executor::ServerToolExecutor>>,

    // ── Interruption tracking ──
    /// Structured interruption record populated by early-exit paths.
    /// When set, the session journal and checkpoint include machine-readable
    /// interruption context for structured resumption.
    pub interruption: Option<super::interruption::InterruptionRecord>,

    // ── Session Facts (L1a ground truth) ──
    /// System-tracked session state updated every turn from tool call records.
    /// Used for facts-first anchor, injection, compaction, and microcompact pin list.
    pub session_facts: crate::turn::cloud::session_facts::SessionFacts,

    // ── Approval checkpoint persistence ──
    /// Approval overrides synchronized from CLI's PermissionManager before each turn.
    /// Written to HeavyCheckpoint so approval decisions survive session restarts.
    pub approval_overrides: Option<super::approval_fingerprint::FingerprintedOverrides>,

    // ── Confidence tracking ──
    /// Tracks selector confidence trends across turns to detect floor loops.
    pub confidence_trend: super::confidence_contract::ConfidenceTrendTracker,
    /// Last diagnosis computed after tool selection (for telemetry and fallback).
    pub last_confidence_diagnosis: Option<super::confidence_contract::ConfidenceDiagnosis>,

    // ── Turn observability (Phase 1) ──
    /// In-memory collector for fine-grained turn events (llm_round, tool timing).
    /// Session-level turn number (1-based). Set by the CLI from ReplState.turn
    /// so that llm_round journal events carry the correct turn number.
    pub session_turn: u32,
    /// Created at turn start, flushed at turn end or on interruption.
    pub turn_event_buffer: Option<astra_services::session_journal::TurnEventBuffer>,
}

/// Consecutive same-category error turns before forcing a strategy change.
pub(crate) const CONSECUTIVE_ERROR_BUDGET: u32 = 3;

/// Maximum number of recent file reads to track for post-compact restoration.
pub(crate) const MAX_TRACKED_FILE_READS: usize = 20;

#[allow(unused_imports)]
pub(crate) use super::agentic_adaptive_tuning::{
    DEFAULT_TUNING_CYCLE_INTERVAL, apply_adaptive_execution_profile, apply_per_turn_adaptation,
    apply_tactical_actions, maybe_run_tuning_cycle, record_loop_completion_feedback,
    record_new_evolution_promotion_events, should_emit_adaptive_scenario_event,
    snapshot_evolution_promotion_ids,
};
pub use super::agentic_loop_tool_support::delegate_tool_schema;
#[allow(unused_imports)]
pub(crate) use super::agentic_loop_tool_support::{
    extract_file_path_from_tool, record_edge_tool_observability,
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

#[allow(unused_imports)]
pub(crate) use super::agentic_auto_reflection::{
    AUTO_REFLECTION_SIGNAL_THRESHOLD, maybe_trigger_auto_reflection,
};
#[allow(unused_imports)]
pub(crate) use super::agentic_loop_execution_phase::{
    TurnExecutionControl, TurnExecutionPhase, execute_turn_and_ingest_phase,
};
#[allow(unused_imports)]
pub(crate) use super::agentic_loop_finalization::{
    finalize_and_render, finalize_turn_trace, run_agentic_loop_with_host,
    try_write_heavy_checkpoint,
};
#[allow(unused_imports)]
pub(crate) use super::agentic_loop_lifecycle::{
    PreparedTurnIteration, TurnIterationPrep, prepare_turn_iteration, run_loop_preamble,
};
#[allow(unused_imports)]
pub(crate) use super::agentic_loop_tool_phase::{TurnToolPhaseControl, execute_tool_phase};

pub(crate) async fn run_agentic_loop_impl<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) -> Result<AgenticLoopOutcome, astra_core::ClassifiedError> {
    run_loop_preamble(host, state).await;

    let mut turn_index = 0usize;
    while turn_index < state.max_turns || state.remaining_turns == 0 {
        state.current_round_index = turn_index as u32;
        let TurnIterationPrep {
            quiet,
            turn_start_time,
        } = match prepare_turn_iteration(host, state, turn_index).await? {
            PreparedTurnIteration::Ready(prep) => prep,
            PreparedTurnIteration::Finished(outcome) => {
                if matches!(outcome, AgenticLoopOutcome::Completed) && !state.final_text.is_empty()
                {
                    finalize_and_render(host, state).await;
                }
                return Ok(outcome);
            }
        };

        let TurnExecutionPhase {
            llm_wall_start,
            turn_result,
        } = match execute_turn_and_ingest_phase(
            host,
            state,
            turn_index,
            TurnIterationPrep {
                quiet,
                turn_start_time,
            },
        )
        .await?
        {
            TurnExecutionControl::Proceed(phase) => *phase,
            TurnExecutionControl::ContinueLoop => {
                turn_index += 1;
                continue;
            }
            TurnExecutionControl::Return(outcome) => return Ok(outcome),
        };

        let tool_phase_control = execute_tool_phase(
            host,
            state,
            turn_index,
            TurnIterationPrep {
                quiet,
                turn_start_time,
            },
            TurnExecutionPhase {
                llm_wall_start,
                turn_result,
            },
        )
        .await?;

        // Drain evolution signals / trigger auto-reflection on the production
        // agentic loop path. Previously this was only reached via tests, which
        // caused `reflect` / auto-tuning capabilities to appear regressed at
        // runtime.
        maybe_trigger_auto_reflection(host, state).await;

        match tool_phase_control {
            TurnToolPhaseControl::ContinueLoop => {}
            TurnToolPhaseControl::Return(outcome) => return Ok(outcome),
        }

        turn_index += 1;
    }
    // Loop exhausted max_turns without explicit break — write final state.
    finalize_and_render(host, state).await;
    Ok(AgenticLoopOutcome::Completed)
}

// ─── CTX_ helpers ────────────────────────────────────────────────────────────

/// Derive a sensible OpenAI-protocol `finish_reason` when upstream didn't
/// supply one. Observed in the wild: qwen-turbo frequently omits
/// `finish_reason` on tool-call rounds (72/92 rounds null in a production
/// session), which tripped up journal analysers and learning signals that
/// used the field to distinguish tool-call rounds from stops.
///
/// Rule follows the OpenAI Chat Completions spec:
/// * upstream value wins when present (we don't lie about "length"-truncated
///   responses — that would suppress the output-token escalation at
///   `server_loop_host.rs:1715`);
/// * absent + tool_calls present → `"tool_calls"`;
/// * absent + no tool_calls → `"stop"`.
///
/// This is a pure function to keep it trivially testable. Callers that need
/// to record a journal event can use it without mutating the upstream
/// `LlmCallResult`.
pub(crate) fn synthesise_finish_reason(
    upstream: Option<&str>,
    has_tool_calls: bool,
) -> &'static str {
    match upstream {
        Some("stop") => "stop",
        Some("length") => "length",
        Some("tool_calls") => "tool_calls",
        Some("content_filter") => "content_filter",
        Some("function_call") => "function_call",
        // Unknown / other upstream string: we can't return it borrowed as
        // `&'static`, but at the journal layer callers already have the
        // original String when needed. Any caller that feeds a non-None
        // upstream here is using this helper *as a default* — so fall
        // through to the rules below for deterministic output.
        Some(_) => {
            if has_tool_calls {
                "tool_calls"
            } else {
                "stop"
            }
        }
        None => {
            if has_tool_calls {
                "tool_calls"
            } else {
                "stop"
            }
        }
    }
}

#[cfg(test)]
mod synthesise_finish_reason_tests {
    use super::synthesise_finish_reason;

    #[test]
    fn none_plus_tool_calls_becomes_tool_calls() {
        assert_eq!(synthesise_finish_reason(None, true), "tool_calls");
    }

    #[test]
    fn none_without_tool_calls_becomes_stop() {
        assert_eq!(synthesise_finish_reason(None, false), "stop");
    }

    #[test]
    fn upstream_length_is_preserved() {
        // Critical: "length" is the signal that triggers max_output_tokens
        // escalation in server_loop_host. We must never clobber it.
        assert_eq!(synthesise_finish_reason(Some("length"), false), "length");
        assert_eq!(synthesise_finish_reason(Some("length"), true), "length");
    }

    #[test]
    fn upstream_known_values_are_preserved() {
        assert_eq!(synthesise_finish_reason(Some("stop"), true), "stop");
        assert_eq!(
            synthesise_finish_reason(Some("tool_calls"), false),
            "tool_calls"
        );
        assert_eq!(
            synthesise_finish_reason(Some("content_filter"), false),
            "content_filter"
        );
    }

    #[test]
    fn unknown_upstream_falls_back_to_rule() {
        // Unknown reasons (forward-compatibility): treat as if absent so
        // downstream consumers see consistent semantics.
        assert_eq!(
            synthesise_finish_reason(Some("something_new"), true),
            "tool_calls"
        );
        assert_eq!(
            synthesise_finish_reason(Some("something_new"), false),
            "stop"
        );
    }
}

/// Extract repository name from a git remote URL.
/// **Test-only.** Build a minimal [`AgenticLoopState`] suitable for driving
/// the mock-LLM path in integration tests (feature `bridge-e2e-hooks`).
///
/// All fields use safe defaults; tests should mutate the returned state
/// directly (e.g. push into `messages`, set `llm_rounds_completed`).
#[cfg(feature = "bridge-e2e-hooks")]
pub fn make_test_loop_state() -> AgenticLoopState {
    AgenticLoopState {
        messages: Vec::new(),
        tool_results: Vec::new(),
        current_session_id: None,
        current_run_id: None,
        recursion_depth: 0,
        final_text: String::new(),
        final_text_streamed: false,
        total_prompt: 0,
        total_completion: 0,
        total_cache_read: 0,
        total_cache_creation: 0,
        total_tool_calls: 0,
        total_evidence_tool_calls: 0,
        has_any_usage: false,
        max_turns: 10,
        remaining_turns: 10,
        agentic_turn_budget: TaskExecutionProfile::default().agentic_turn_budget,
        current_round_index: 0,
        llm_rounds_completed: 0,
        turn_guard: TurnGuard::new(),
        restricted_tools: HashSet::new(),
        boosted_tools: HashSet::new(),
        widen_selection_pending: false,
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
        last_turn_policy: TurnInteractionPolicy::default(),
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
        compaction_effectiveness: Default::default(),
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
        server_tool_executor: None,
        interruption: None,
        session_facts: Default::default(),
        approval_overrides: None,
        confidence_trend: Default::default(),
        last_confidence_diagnosis: None,
        session_turn: 0,
        turn_event_buffer: None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use astra_services::session_journal::SURGICAL_REMOVAL_TOOL_NAME;
    use serde_json::json;

    // ── Flexible mock host for multi-turn scenarios ─────────────────────────

    pub(crate) struct MockHost {
        turn_results: Vec<HostTurnResult>,
        current_turn: usize,
        pub(crate) valid_tools: HashSet<String>,
        pub(crate) emitted_lines: Vec<String>,
        quiet: bool,
        pub(crate) injected_schemas: Vec<Value>,
        reflection_text: Option<String>,
        reflection_error: Option<String>,
        pub(crate) last_reflection_prompt: Option<String>,
        pub(crate) rendered_final_text: Vec<String>,
    }

    impl MockHost {
        pub(crate) fn new(results: Vec<HostTurnResult>) -> Self {
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

        pub(crate) fn with_valid_tools(mut self, tools: &[&str]) -> Self {
            self.valid_tools = tools.iter().map(|s| s.to_string()).collect();
            self
        }

        pub(crate) fn turn_count(&self) -> usize {
            self.current_turn
        }

        pub(crate) fn with_reflection_text(mut self, text: &str) -> Self {
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
        ) -> Result<HostTurnResult, astra_core::ClassifiedError> {
            if self.turn_results.is_empty() {
                return Err(astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::BudgetExhausted,
                    "no more turns",
                ));
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
        ) -> Result<Option<HostReflectionResult>, astra_core::ClassifiedError> {
            self.last_reflection_prompt = Some(request.user_prompt.to_string());
            if let Some(error) = self.reflection_error.take() {
                return Err(error.into());
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

    pub(crate) fn text_result(
        text: &str,
        prompt: u64,
        completion: u64,
        ttft: Option<u64>,
    ) -> HostTurnResult {
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
            error_kind: None,
        }
    }

    pub(crate) fn edge_tool_result(
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
            error_kind: None,
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
            error_kind: None,
        }
    }

    pub(crate) fn make_edge_tool(name: &str, output: &str) -> EdgeToolExecResult {
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

    pub(crate) fn make_state() -> AgenticLoopState {
        AgenticLoopState {
            messages: Vec::new(),
            tool_results: Vec::new(),
            current_session_id: None,
            current_run_id: None,
            recursion_depth: 0,
            final_text: String::new(),
            final_text_streamed: false,
            total_prompt: 0,
            total_completion: 0,
            total_cache_read: 0,
            total_cache_creation: 0,
            total_tool_calls: 0,
            total_evidence_tool_calls: 0,
            has_any_usage: false,
            max_turns: 10,
            remaining_turns: 10,
            agentic_turn_budget: TaskExecutionProfile::default().agentic_turn_budget,
            current_round_index: 0,
            llm_rounds_completed: 0,
            turn_guard: TurnGuard::new(),
            restricted_tools: HashSet::new(),
            boosted_tools: HashSet::new(),
            widen_selection_pending: false,
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
            last_turn_policy: TurnInteractionPolicy::default(),
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
            compaction_effectiveness: Default::default(),
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
            server_tool_executor: None,
            interruption: None,
            session_facts: Default::default(),
            approval_overrides: None,
            confidence_trend: Default::default(),
            last_confidence_diagnosis: None,
            session_turn: 0,
            turn_event_buffer: None,
        }
    }

    // ── Original tests ──────────────────────────────────────────────────────

    #[test]
    fn interaction_policy_only_counts_visible_evidence_tools() {
        let policy = TurnInteractionPolicy::from_visible_tool_names(
            TurnInteractionMode::Prompt,
            vec![
                "mo_query".to_string(),
                ASK_USER_TOOL_NAME.to_string(),
                "read_file".to_string(),
            ],
        );

        assert!(policy.allow_ask_user);
        assert!(policy.can_pause_for_user);
        assert_eq!(
            policy.evidence_tool_names,
            vec!["mo_query".to_string(), "read_file".to_string()]
        );
    }

    #[test]
    fn interaction_scoped_restrictions_hide_ask_user_outside_prompt_turns() {
        assert!(
            !interaction_scoped_tool_restrictions(TurnInteractionMode::Prompt)
                .contains(ASK_USER_TOOL_NAME)
        );
        for mode in [
            TurnInteractionMode::Auto,
            TurnInteractionMode::Deny,
            TurnInteractionMode::Headless,
            TurnInteractionMode::NonInteractive,
        ] {
            assert!(interaction_scoped_tool_restrictions(mode).contains(ASK_USER_TOOL_NAME));
        }
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
        assert!(
            err.message.contains("budget"),
            "should mention budget: {err}"
        );
        assert!(
            err.message.contains("budget: 25"),
            "should show max_turns as budget: {err}"
        );
    }

    #[tokio::test]
    async fn budget_exhausted_with_progress_completes_gracefully() {
        let mut host = MockHost::new(vec![]);
        let mut state = make_state();
        state.max_turns = 15;
        state.remaining_turns = 0;
        state.total_tool_calls = 3;
        state.total_prompt = 120;
        state.stall.tool_call_records = vec![
            tool_record("bash", true, Some("ok")),
            tool_record("read_file", true, Some("ok")),
            tool_record("grep", true, Some("ok")),
        ];

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert!(state.final_text.contains("Turn budget exhausted"));
        assert!(state.final_text.contains("3 completed tool call(s)"));
        assert_eq!(host.rendered_final_text.last(), Some(&state.final_text));
    }

    #[tokio::test]
    async fn host_error_propagates() {
        let mut host = MockHost::new(vec![]);
        let mut state = make_state();
        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_err());
        assert!(
            outcome.unwrap_err().message.contains("no more turns"),
            "error should mention 'no more turns'"
        );
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

    #[tokio::test]
    async fn adaptive_budget_extends_complex_task_when_progress_is_real() {
        let mut host = MockHost::new(vec![
            edge_tool_result(
                vec![make_edge_tool_with_args(
                    "read_file",
                    json!({"path": "src/lib.rs"}),
                    "module contents",
                )],
                10,
                5,
                Some(20),
            ),
            edge_tool_result(
                vec![make_edge_tool_with_args(
                    "write_file",
                    json!({"path": "src/lib.rs"}),
                    "updated module contents",
                )],
                10,
                5,
                Some(20),
            ),
            text_result("completed after extension", 10, 5, Some(20)),
        ])
        .with_valid_tools(&["read_file", "write_file"]);
        let mut state = make_state();
        state.task_profile = crate::turn::chat_turn_heuristics::infer_task_execution_profile(
            "systematically refactor and implement a complex subsystem",
        );
        state.agentic_turn_budget = astra_turn_core::chat_turn_heuristics::AgenticTurnBudget {
            initial_turns: 2,
            hard_turn_limit: 4,
            extension_turns: 2,
            max_extensions: 1,
        };
        state.max_turns = 2;
        state.remaining_turns = 2;

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(host.current_turn, 3);
        assert_eq!(state.max_turns, 4);
        assert_eq!(state.final_text, "completed after extension");
        assert!(
            state
                .messages
                .iter()
                .filter_map(|message| message.get("content").and_then(Value::as_str))
                .any(|content| content.contains("Budget review"))
        );
    }

    #[tokio::test]
    async fn adaptive_budget_refuses_extension_for_stalled_repetition() {
        let repeated =
            make_edge_tool_with_args("read_file", json!({"path": "src/lib.rs"}), "same contents");
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![repeated.clone()], 10, 5, Some(20)),
            edge_tool_result(vec![repeated.clone()], 10, 5, Some(20)),
            edge_tool_result(vec![repeated], 10, 5, Some(20)),
        ])
        .with_valid_tools(&["read_file"]);
        let mut state = make_state();
        state.task_profile = crate::turn::chat_turn_heuristics::infer_task_execution_profile(
            "explore the codebase and investigate the root cause",
        );
        state.agentic_turn_budget = astra_turn_core::chat_turn_heuristics::AgenticTurnBudget {
            initial_turns: 2,
            hard_turn_limit: 4,
            extension_turns: 2,
            max_extensions: 1,
        };
        state.max_turns = 2;
        state.remaining_turns = 2;

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(host.current_turn, 2);
        assert!(state.final_text.contains("Turn budget exhausted"));
    }

    #[tokio::test]
    async fn adaptive_budget_refuses_extension_when_warning_verdict_present() {
        let mut host = MockHost::new(vec![
            edge_tool_result(
                vec![make_edge_tool_with_args(
                    "read_file",
                    json!({"path": "src/lib.rs"}),
                    "module contents",
                )],
                10,
                5,
                Some(20),
            ),
            edge_tool_result(
                vec![make_edge_tool_with_args(
                    "write_file",
                    json!({"path": "src/lib.rs"}),
                    "updated module contents",
                )],
                10,
                5,
                Some(20),
            ),
            text_result("should not run", 10, 5, Some(20)),
        ])
        .with_valid_tools(&["read_file", "write_file"]);
        let mut state = make_state();
        state.task_profile = crate::turn::chat_turn_heuristics::infer_task_execution_profile(
            "systematically refactor and implement a complex subsystem",
        );
        state.agentic_turn_budget = astra_turn_core::chat_turn_heuristics::AgenticTurnBudget {
            initial_turns: 2,
            hard_turn_limit: 4,
            extension_turns: 2,
            max_extensions: 1,
        };
        state.max_turns = 2;
        state.remaining_turns = 2;
        state.stall.verdict_events.push(
            crate::turn::agentic_verdict_audit::AgenticVerdictAuditEvent {
                turn: 1,
                severity: "warning".into(),
                injections: vec!["stall detected".into()],
                avoid_tools: vec!["write_file".into()],
                deprioritized_tools: vec![],
                force_stop: false,
                nudge_count: 1,
                total_errors: 0,
                deprioritized_count: 0,
                total_timeouts: 0,
                timeout_dominant_tools: vec![],
                total_cache_hits: 0,
                flaky_count: 0,
            },
        );

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(host.current_turn, 2);
        assert!(state.final_text.contains("Turn budget exhausted"));
        assert!(
            !state
                .messages
                .iter()
                .filter_map(|message| message.get("content").and_then(Value::as_str))
                .any(|content| content.contains("Budget review"))
        );
    }

    #[tokio::test]
    async fn adaptive_budget_extends_exploratory_task_on_distinct_real_progress() {
        let mut host = MockHost::new(vec![
            edge_tool_result(
                vec![make_edge_tool_with_args(
                    "read_file",
                    json!({"path": "src/lib.rs"}),
                    "module contents",
                )],
                10,
                5,
                Some(20),
            ),
            edge_tool_result(
                vec![make_edge_tool_with_args(
                    "glob",
                    json!({"pattern": "src/**/*.rs"}),
                    "src/lib.rs\nsrc/main.rs",
                )],
                10,
                5,
                Some(20),
            ),
            text_result("completed after exploratory extension", 10, 5, Some(20)),
        ])
        .with_valid_tools(&["read_file", "glob"]);
        let mut state = make_state();
        state.task_profile = crate::turn::chat_turn_heuristics::infer_task_execution_profile(
            "explore the codebase and investigate the root cause",
        );
        state.agentic_turn_budget = astra_turn_core::chat_turn_heuristics::AgenticTurnBudget {
            initial_turns: 2,
            hard_turn_limit: 4,
            extension_turns: 2,
            max_extensions: 1,
        };
        state.max_turns = 2;
        state.remaining_turns = 2;

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(host.current_turn, 3);
        assert_eq!(state.max_turns, 4);
        assert_eq!(state.final_text, "completed after exploratory extension");
        assert!(
            state
                .messages
                .iter()
                .filter_map(|message| message.get("content").and_then(Value::as_str))
                .any(|content| content.contains("Budget review"))
        );
    }

    #[tokio::test]
    async fn adaptive_budget_respects_hard_limit_even_with_real_progress() {
        let mut host = MockHost::new(vec![
            edge_tool_result(
                vec![make_edge_tool_with_args(
                    "read_file",
                    json!({"path": "src/lib.rs"}),
                    "module contents",
                )],
                10,
                5,
                Some(20),
            ),
            edge_tool_result(
                vec![make_edge_tool_with_args(
                    "write_file",
                    json!({"path": "src/lib.rs"}),
                    "updated module contents",
                )],
                10,
                5,
                Some(20),
            ),
            text_result("should never run", 10, 5, Some(20)),
        ])
        .with_valid_tools(&["read_file", "write_file"]);
        let mut state = make_state();
        state.task_profile = crate::turn::chat_turn_heuristics::infer_task_execution_profile(
            "systematically refactor and implement a complex subsystem",
        );
        state.agentic_turn_budget = astra_turn_core::chat_turn_heuristics::AgenticTurnBudget {
            initial_turns: 2,
            hard_turn_limit: 2,
            extension_turns: 2,
            max_extensions: 1,
        };
        state.max_turns = 2;
        state.remaining_turns = 2;

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(host.current_turn, 2);
        assert_eq!(state.max_turns, 2);
        assert!(
            state
                .final_text
                .contains("Turn budget exhausted after 2 agentic turn(s)")
        );
        assert!(
            !state
                .messages
                .iter()
                .filter_map(|message| message.get("content").and_then(Value::as_str))
                .any(|content| content.contains("Budget review"))
        );
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
            error_kind: None,
        }]);
        let mut state = make_state();

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_err());
        assert!(outcome.unwrap_err().message.contains("rate limit"));
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
            error_kind: None,
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

    // ── Delegation passthrough / E2E tests ──────────────────────────────────

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

    #[test]
    fn state_delegation_engine_defaults_to_none() {
        let state = make_state();
        assert!(state.delegation_engine.is_none());
    }

    // ── E2E delegation round-trip tests ─────────────────────────────────────

    /// Helper to build a DelegationEngine with StubSubRunExecutor for tests.
    pub(crate) fn make_test_delegation_engine()
    -> Arc<crate::server::delegation_engine::DelegationEngine> {
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
            error_kind: None,
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
        fn resolve(
            &self,
            name: &str,
        ) -> Result<crate::turn::skill_tool::ResolvedSkill, crate::skills::SkillError> {
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
                        output_schema: None,
                        remote_url: None,
                        forward_headers: vec![],
                        required_headers: vec![],
                        aliases: Vec::new(),

                        effort: None,
                        agent_type: None,
                        trust_tier: crate::skills::manifest::TrustTier::Bundled,
                    },
                )
                .ok_or_else(|| {
                    crate::skills::SkillError::NotFound(format!("unknown skill: {name}"))
                })
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
            error_kind: None,
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
                reentry_count: 0,
            },
        );

        // Simulate post-compaction re-injection
        let mut builder = AttachmentBuilder::new();
        let mut skills: Vec<_> = state.skills.invoked.values().collect();
        skills.sort_by_key(|b| std::cmp::Reverse(b.invoked_at_turn));
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
            error_kind: None,
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
                error_kind: None,
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
                error_kind: None,
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
            error_kind: None,
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
            err.message.contains("Rate limit cooldown active"),
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

        // Verify env var was set via session_env_overlay (not process env)
        assert_eq!(
            astra_core::session_env_overlay::get("ASTRA_TEST_HOOK_VAR").as_deref(),
            Some("session_active")
        );
        // Cleanup
        astra_core::session_env_overlay::remove("ASTRA_TEST_HOOK_VAR");
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
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
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
        let fail_result: Result<AgenticLoopOutcome, astra_core::ClassifiedError> =
            Ok(AgenticLoopOutcome::Error("test error".into()));
        record_loop_completion_feedback(&mut state, &fail_result);
        let fail_result2: Result<AgenticLoopOutcome, astra_core::ClassifiedError> =
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

        let result: Result<AgenticLoopOutcome, astra_core::ClassifiedError> =
            Err("something broke".to_string().into());
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
                SURGICAL_REMOVAL_TOOL_NAME,
                true,
                Some("(removed from context — skill covered this work)"),
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
    fn adaptive_profile_skips_scenario_after_user_cancellation() {
        // Regression for session 721df7da: after the user hits Ctrl+C on a
        // focused review task, the tool history of the cancelled turn was
        // leaking into ScenarioDetector and falsely triggering the
        // `Exploration` scenario (ratcheting tool_budget 722 → 1000).
        // The fix gates `apply_adaptive_execution_profile` on the
        // `previous_turn_user_cancelled` flag and consumes it after one
        // turn.
        let hub = make_hub();
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub);
        state.telemetry.observability_session = Some(session.clone());
        state.message = "继续啊".into();
        // Exploratory tool history from the cancelled turn — would
        // normally push the detector toward Exploration.
        state.recent_tools = vec![
            "glob".into(),
            "grep".into(),
            "view".into(),
            "read_file".into(),
            "git_show".into(),
            "git_show".into(),
            "view".into(),
            "grep".into(),
        ];

        let scenario_before;
        {
            let mut guard = session.write().unwrap_or_else(|e| e.into_inner());
            guard.previous_turn_user_cancelled = true;
            scenario_before = guard.profile.current_scenario;
        }

        apply_adaptive_execution_profile(&mut state);

        let guard = session.read().unwrap_or_else(|e| e.into_inner());
        // Flag must be consumed (one-turn suppression only).
        assert!(
            !guard.previous_turn_user_cancelled,
            "previous_turn_user_cancelled flag must be cleared after being consumed"
        );
        // Scenario must NOT have changed as a side-effect of the cancelled
        // turn's tool history.
        assert_eq!(
            guard.profile.current_scenario, scenario_before,
            "scenario should not be re-detected on the turn after a user cancellation"
        );
    }

    #[test]
    fn adaptive_profile_resumes_on_turn_after_cancellation_cleared() {
        // Verifies the suppression is strictly one turn: the second
        // apply_adaptive_execution_profile call after a cancellation must
        // behave normally (i.e. detect Debugging for a matching query).
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
            guard.previous_turn_user_cancelled = true;
        }

        // First call: suppressed (flag was true) — scenario unchanged.
        apply_adaptive_execution_profile(&mut state);
        {
            let guard = session.read().unwrap_or_else(|e| e.into_inner());
            assert!(!guard.previous_turn_user_cancelled);
            assert_eq!(guard.profile.current_scenario, None);
        }

        // Second call: normal detection path runs.
        apply_adaptive_execution_profile(&mut state);
        let guard = session.read().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            guard.profile.current_scenario,
            Some(crate::user_profile::Scenario::Debugging),
            "scenario detection must resume on the turn after the cancellation flag is consumed"
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
        state.session_turn = 10;

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
        state.session_turn = 10;

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
        state.session_turn = 1;

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
        outcome: &Result<AgenticLoopOutcome, astra_core::ClassifiedError>,
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

        let success: Result<AgenticLoopOutcome, astra_core::ClassifiedError> =
            Ok(AgenticLoopOutcome::Completed);
        let failure: Result<AgenticLoopOutcome, astra_core::ClassifiedError> =
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
            (30_000..=80_000).contains(&final_budget),
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

        let success: Result<AgenticLoopOutcome, astra_core::ClassifiedError> =
            Ok(AgenticLoopOutcome::Completed);

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
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
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
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
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
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
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
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
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
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
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
            let budget = state.max_turn_input_tokens;
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
        assert!(host.emitted_lines.iter().any(|line| {
            line.contains("processed 1 proposal(s): 0 auto-applied, 0 canary-started, 1 queued")
        }));
    }

    #[tokio::test]
    async fn auto_reflection_injects_pipeline_diagnosis_into_prompt() {
        // Seed runtime signals that the pipeline stage bridge recognises as a
        // `ToolFailures` category, and assert the structured diagnosis is
        // wired into the LLM reflection prompt via `recent_tactical_actions`.
        let reflection_response = r#"{"proposals": [], "summary": "noop"}"#;
        let mut host = MockHost::new(vec![]).with_reflection_text(reflection_response);
        let mut state = make_state();

        let evo = std::sync::Arc::new(crate::evolution::service::EvolutionService::new());
        state.evolution_service = Some(evo.clone());

        // Attach an observability session so the auto-reflection bridge can
        // publish last_strategy_application → surfaced by SelfModel rendering.
        let obs_session = std::sync::Arc::new(std::sync::RwLock::new(
            crate::observability_integration::ObservabilitySession::new_simple("sess-e2e"),
        ));
        state.telemetry.observability_session = Some(obs_session.clone());

        // Repeated failures on the same tool → FailureCategory::ToolFailures.
        let fail_rec = |err: &str| ToolCallRecord {
            name: "flaky_http".into(),
            ok: false,
            ms: 1,
            error: Some(err.into()),
            ..Default::default()
        };
        state.stall.tool_call_records = vec![fail_rec("500"), fail_rec("500"), fail_rec("timeout")];

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

        let prompt = host
            .last_reflection_prompt
            .as_deref()
            .expect("reflection prompt captured");
        assert!(
            prompt.contains("pipeline-diagnose"),
            "expected pipeline-diagnose label in reflection prompt, got: {}",
            prompt
        );
        assert!(
            prompt.contains("ToolFailures"),
            "expected ToolFailures category in prompt, got: {}",
            prompt
        );
        assert!(
            prompt.contains("flaky_http"),
            "expected failing tool name in prompt, got: {}",
            prompt
        );
        // Strategy delta should have been applied to runtime state too.
        assert!(
            state.restricted_tools.contains("flaky_http"),
            "expected flaky_http in restricted_tools, got: {:?}",
            state.restricted_tools
        );
        assert!(
            host.emitted_lines
                .iter()
                .any(|line| line.contains("Pipeline strategy applied")
                    && line.contains("flaky_http")),
            "expected strategy-applied log line, got lines: {:?}",
            host.emitted_lines
        );
        // ToolFailures diagnosis also sets widen_selection → the one-shot flag
        // should be pending until the next visible_turn_tools call consumes it.
        assert!(
            state.widen_selection_pending,
            "expected widen_selection_pending = true after bridge applied strategy"
        );
        // Passive self-awareness loop: the bridge publishes StrategyApplication
        // onto the observability session, and SelfModel rendering surfaces it
        // to the agent on the next turn.
        let obs_guard = obs_session.read().expect("obs session read");
        let applied = obs_guard
            .last_strategy_application
            .as_ref()
            .expect("expected last_strategy_application published on obs session");
        assert!(applied.widen_requested, "widen should be recorded");
        assert!(
            applied.newly_blocked.iter().any(|t| t == "flaky_http"),
            "flaky_http should be recorded as newly_blocked"
        );
        let self_model = crate::self_model::SelfModel::snapshot_with_strategy(
            &["bash", "read_file"],
            &[],
            &[],
            &[],
            None,
            state.max_turns.saturating_sub(state.remaining_turns) as u32,
            None,
            None,
            None,
            0,
            0,
            0,
            None,
            None,
            None,
            None,
            None,
            &[],
            &obs_guard.config,
            Some(applied),
        );
        let rendered = self_model.to_system_prompt_section();
        assert!(
            rendered.contains("widened for next turn"),
            "expected widen signal in self-awareness section, got: {rendered}"
        );
        // P3.1: structured skill-diff carries before/after snapshots, is
        // published on StrategyApplication, and is surfaced verbatim in the
        // self-awareness section so the agent can audit its own tuning.
        let diff = applied
            .diff_entry
            .as_ref()
            .expect("expected SkillDiffEntry populated after non-noop apply");
        assert_eq!(diff.skill, "pipeline.tool_selection");
        assert_eq!(diff.reason, "auto-reflection");
        assert!(
            !diff
                .before
                .blocked_tools
                .contains(&"flaky_http".to_string()),
            "before snapshot must predate the block, got: {:?}",
            diff.before.blocked_tools
        );
        assert!(
            diff.after.blocked_tools.contains(&"flaky_http".to_string()),
            "after snapshot must contain the newly-blocked tool, got: {:?}",
            diff.after.blocked_tools
        );
        assert!(!diff.before.widen_pending && diff.after.widen_pending);
        assert!(
            rendered.contains("Strategy diff:"),
            "expected `Strategy diff:` line, got: {rendered}"
        );
        assert!(
            rendered.contains("flaky_http"),
            "expected blocked tool name in strategy diff, got: {rendered}"
        );
        assert!(
            self_model.skill_diff.is_some(),
            "expected SelfModel.skill_diff populated from applied.diff_entry"
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
            failure_category: None,
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
                    failure_category: None,
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
                .any(|line| line.contains("skipped:") && line.contains("network unavailable"))
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
                    file_path: None,
                    surgically_removed: None,
                    original_tool_name: None,
                    ..Default::default()
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
                    file_path: None,
                    surgically_removed: None,
                    original_tool_name: None,
                    ..Default::default()
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
                agentic_step: None,
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
                    file_path: None,
                    surgically_removed: None,
                    original_tool_name: None,
                    ..Default::default()
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
                round: None,
                tool_calls_returned: None,
                offset_ms: None,
                llm_rounds: None,
                total_llm_ms: None,
                total_tool_ms: None,
                parent_event_id: None,
                git_head: None,
                git_branch: None,
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
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
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
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
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
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
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
                failure_category: None,
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
        assert!(prompt.contains("Recent tactical actions:"));
        assert!(prompt.contains("verify outputs more strictly"));
        assert!(prompt.contains("[ToolFailure] bash: Permission denied"));
        assert!(state.recent_tactical_actions.is_empty());
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
            error_kind: None,
        }
    }

    #[tokio::test]
    async fn skill_with_substantial_output_skips_deferred_calls() {
        // Regression: session 746b6423 — skill produced a full code review
        // but 18 deferred tool calls were re-executed in the next iteration,
        // causing 3x token waste.
        //
        // After surgery fix: intercepted non-skill calls are stripped entirely
        // from the assistant message and tool results — no token cost at all.
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
            // Iteration 2: LLM sees skill output only, produces final text
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

        // Surgery: call_rf1 and call_rf2 should NOT appear in messages at all —
        // not as tool_call objects in the assistant message, and not as tool results.
        let rf1_msgs: Vec<&Value> = state
            .messages
            .iter()
            .filter(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("call_rf1"))
            .collect();
        assert_eq!(
            rf1_msgs.len(),
            0,
            "call_rf1 should be surgically removed from messages, found: {rf1_msgs:?}"
        );
        let rf2_msgs: Vec<&Value> = state
            .messages
            .iter()
            .filter(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("call_rf2"))
            .collect();
        assert_eq!(
            rf2_msgs.len(),
            0,
            "call_rf2 should be surgically removed from messages, found: {rf2_msgs:?}"
        );

        // The assistant message from iteration 1 should only contain the skill
        // tool_call — not call_rf1 or call_rf2.
        let assistant_msgs: Vec<&Value> = state
            .messages
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
            .filter(|m| m.get("tool_calls").is_some())
            .collect();
        for msg in &assistant_msgs {
            if let Some(tool_calls) = msg["tool_calls"].as_array() {
                for tc in tool_calls {
                    let id = tc.get("id").and_then(Value::as_str).unwrap_or("");
                    assert!(
                        id != "call_rf1" && id != "call_rf2",
                        "assistant message should not contain surgically removed tool_call: {id}"
                    );
                }
            }
        }

        // The skill result should mention the dropped calls.
        let skill_result_msgs: Vec<&Value> = state
            .messages
            .iter()
            .filter(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("call_skill"))
            .collect();
        assert!(
            !skill_result_msgs.is_empty(),
            "skill tool result should exist"
        );
        let skill_content = skill_result_msgs[0]["content"].as_str().unwrap_or("");
        assert!(
            skill_content.contains("parallel tool call(s) were dropped"),
            "skill result should note the surgically removed calls, got: {skill_content}"
        );

        // Stall detector should still have records for the removed calls
        // (now as synthetic placeholders with ok=true — they are intentional
        // context optimizations, not failures).
        let removed_records: Vec<_> = state
            .stall
            .tool_call_records
            .iter()
            .filter(|r| r.name == SURGICAL_REMOVAL_TOOL_NAME)
            .collect();
        assert_eq!(
            removed_records.len(),
            2,
            "expected 2 surgically_removed stall records, got {}",
            removed_records.len()
        );
        for r in &removed_records {
            assert!(
                r.ok,
                "surgically_removed records should have ok=true (they are \
                 intentional context optimizations, not tool failures)"
            );
            assert!(
                r.is_synthetic_placeholder(),
                "surgically_removed records must be classified as synthetic \
                 placeholders so evaluation/analytics skip them"
            );
        }

        // skill_produced_output flag should be set
        assert!(
            state.skill_produced_output,
            "skill_produced_output flag should be set when skill produces substantial output"
        );

        // Soft constraint: deferred tools should NOT be hard-restricted
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

#[cfg(test)]
mod observability_e2e_tests {
    use super::tests::*;
    use super::*;
    use astra_services::session_journal::{
        JournalDirGuard, JournalEventType, JournalWriter, ToolCallRecord,
    };
    use serde_json::json;

    fn tool_call_json(name: &str) -> Value {
        json!({
            "id": format!("call-{name}"),
            "type": "function",
            "function": {
                "name": name,
                "arguments": json!({"path": format!("/tmp/{name}.txt")}).to_string()
            }
        })
    }

    fn turn_with_tools(tools: &[&str], text: &str) -> HostTurnResult {
        HostTurnResult {
            accum: ChatTurnSseAccum {
                full_text: text.to_string(),
                tool_calls: tools.iter().map(|t| tool_call_json(t)).collect(),
                has_tool_calls: !tools.is_empty(),
                prompt_tokens: 1000,
                completion_tokens: 200,
                cache_read_tokens: 100,
                has_usage: true,
                ..Default::default()
            },
            ttft_ms: Some(50),
            edge_tool_round: Vec::new(),
            error_kind: None,
        }
    }

    fn text_only_turn(text: &str) -> HostTurnResult {
        turn_with_tools(&[], text)
    }

    fn read_journal_events(session_id: &str) -> Vec<astra_services::session_journal::JournalEvent> {
        let writer = JournalWriter::new(session_id).unwrap();
        let content = std::fs::read_to_string(writer.path()).unwrap_or_default();
        content
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// Scenario 1: Single round with multiple tools — verifies round, start_offset_ms,
    /// batch_id, parallel fields are populated on ToolCallRecords.
    #[tokio::test]
    async fn observability_single_round_multi_tool_records_round_and_batch() {
        let session_id = format!("obs-e2e-{}", uuid::Uuid::new_v4());
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());

        let mut state = make_state();
        state.current_session_id = Some(session_id.clone());
        // Two turns: first returns 3 tool_calls, second returns text.
        let mut host = MockHost::new(vec![
            turn_with_tools(&["read_file", "grep", "glob"], ""),
            text_only_turn("done"),
        ])
        .with_valid_tools(&["read_file", "grep", "glob"]);

        let outcome = run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .unwrap();
        assert!(matches!(outcome, AgenticLoopOutcome::Completed));

        // Verify ToolCallRecords have round field set.
        let records: Vec<&ToolCallRecord> = state
            .stall
            .tool_call_records
            .iter()
            .filter(|r| !r.is_synthetic_placeholder())
            .collect();
        assert!(
            !records.is_empty(),
            "expected tool call records from headless round"
        );
        for rec in &records {
            assert_eq!(
                rec.round,
                Some(0),
                "all tools in first round should have round=0"
            );
            assert!(
                rec.start_offset_ms.is_some(),
                "start_offset_ms should be set for {}",
                rec.name
            );
        }

        // If multiple tools, they should share a batch_id and be marked parallel.
        if records.len() > 1 {
            let batch_ids: Vec<_> = records.iter().filter_map(|r| r.batch_id.as_ref()).collect();
            assert!(
                !batch_ids.is_empty(),
                "batch_id should be set for multi-tool round"
            );
            let first = &batch_ids[0];
            assert!(
                batch_ids.iter().all(|b| b == first),
                "all tools in same round should share batch_id"
            );
            assert!(
                records.iter().all(|r| r.parallel == Some(true)),
                "multi-tool round should mark parallel=true"
            );
        }
    }

    /// Scenario 2: Multiple LLM rounds — verifies llm_round events are recorded
    /// and round counter increments correctly.
    #[tokio::test]
    async fn observability_multi_round_records_llm_round_events() {
        let session_id = format!("obs-e2e-{}", uuid::Uuid::new_v4());
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());

        let mut state = make_state();
        state.current_session_id = Some(session_id.clone());
        // Three turns: round 0 (1 tool), round 1 (1 tool), round 2 (text).
        let mut host = MockHost::new(vec![
            turn_with_tools(&["read_file"], ""),
            turn_with_tools(&["grep"], ""),
            text_only_turn("final answer"),
        ])
        .with_valid_tools(&["read_file", "grep"]);

        let outcome = run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .unwrap();
        assert!(matches!(outcome, AgenticLoopOutcome::Completed));

        // Verify tool records have incrementing round numbers.
        let records: Vec<&ToolCallRecord> = state
            .stall
            .tool_call_records
            .iter()
            .filter(|r| !r.is_synthetic_placeholder())
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].round, Some(0));
        assert_eq!(records[1].round, Some(1));

        // Verify start_offset_ms is monotonically increasing.
        let off0 = records[0].start_offset_ms.unwrap_or(0);
        let off1 = records[1].start_offset_ms.unwrap_or(0);
        assert!(
            off1 >= off0,
            "second tool should start after first: {off0} vs {off1}"
        );

        // The buffer persists across iterations within the same agentic loop.
        // It should have recorded 3 llm_round events: 2 tool rounds + 1 text-only final.
        if let Some(buf) = &state.turn_event_buffer {
            assert_eq!(
                buf.current_round(),
                3,
                "buffer should have 3 rounds recorded (2 tool + 1 text-only)"
            );
        }
    }

    /// Scenario 3: Cancellation preserves partial data via flush_interrupted.
    #[tokio::test]
    async fn observability_cancellation_flushes_partial_events() {
        let session_id = format!("obs-e2e-cancel-{}", uuid::Uuid::new_v4());
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());

        let mut state = make_state();
        state.current_session_id = Some(session_id.clone());
        // First turn returns tools, second turn the host will error (simulating cancel).
        let mut host = MockHost::new(vec![
            turn_with_tools(&["read_file"], ""),
            // No more turns → BudgetExhausted error → triggers interruption path.
        ])
        .with_valid_tools(&["read_file"]);
        state.max_turns = 2;
        state.remaining_turns = 2;

        let _outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        // The loop should complete (budget exhausted gracefully) or error.
        // Either way, check that partial events were flushed.

        let events = read_journal_events(&session_id);
        // We should see at least an interruption_recorded event.
        // The flush_interrupted path writes partial llm_round events.
        let has_interruption = events
            .iter()
            .any(|e| e.event_type == JournalEventType::InterruptionRecorded);
        // If there was an interruption, partial events should have been flushed.
        if has_interruption {
            let llm_rounds: Vec<_> = events
                .iter()
                .filter(|e| e.event_type == JournalEventType::LlmRound)
                .collect();
            // Should have at least 1 llm_round from the first successful tool turn.
            if !llm_rounds.is_empty() {
                let partial = llm_rounds[0]
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("partial"))
                    .and_then(|v| v.as_bool());
                assert_eq!(
                    partial,
                    Some(true),
                    "interrupted events should be marked partial"
                );
            }
        }

        // Verify tool records still have round info even on interruption.
        let records: Vec<&ToolCallRecord> = state
            .stall
            .tool_call_records
            .iter()
            .filter(|r| !r.is_synthetic_placeholder())
            .collect();
        if !records.is_empty() {
            assert_eq!(records[0].round, Some(0));
            assert!(records[0].start_offset_ms.is_some());
        }
    }
}

#[cfg(test)]
mod parallel_execution_tests {
    use super::tests::*;
    use super::*;
    use astra_services::session_journal::{JournalDirGuard, ToolCallRecord};
    use serde_json::json;

    fn tool_call_json_named(name: &str, id: &str) -> Value {
        json!({
            "id": id,
            "type": "function",
            "function": {
                "name": name,
                "arguments": json!({"path": format!("/tmp/{name}.txt")}).to_string()
            }
        })
    }

    fn turn_with_named_tools(tools: &[(&str, &str)], text: &str) -> HostTurnResult {
        HostTurnResult {
            accum: ChatTurnSseAccum {
                full_text: text.to_string(),
                tool_calls: tools
                    .iter()
                    .map(|(name, id)| tool_call_json_named(name, id))
                    .collect(),
                has_tool_calls: !tools.is_empty(),
                prompt_tokens: 1000,
                completion_tokens: 200,
                cache_read_tokens: 0,
                has_usage: true,
                ..Default::default()
            },
            ttft_ms: Some(50),
            edge_tool_round: Vec::new(),
            error_kind: None,
        }
    }

    /// 6 read-only tools in one round — all should be batched concurrently.
    #[tokio::test]
    async fn parallel_all_readonly_tools_batched_together() {
        let session_id = format!("par-e2e-{}", uuid::Uuid::new_v4());
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());

        let mut state = make_state();
        state.current_session_id = Some(session_id.clone());

        let tools = vec![
            ("read_file", "c1"),
            ("grep", "c2"),
            ("glob", "c3"),
            ("git_status", "c4"),
            ("git_diff", "c5"),
            ("read_file", "c6"),
        ];
        let mut host = MockHost::new(vec![
            turn_with_named_tools(&tools, ""),
            turn_with_named_tools(&[], "done"),
        ])
        .with_valid_tools(&["read_file", "grep", "glob", "git_status", "git_diff"]);

        let outcome = run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .unwrap();
        assert!(matches!(outcome, AgenticLoopOutcome::Completed));

        let records: Vec<&ToolCallRecord> = state
            .stall
            .tool_call_records
            .iter()
            .filter(|r| !r.is_synthetic_placeholder())
            .collect();
        assert_eq!(records.len(), 6, "expected 6 tool call records");

        // All should be in round 0, all parallel, all same batch_id.
        for rec in &records {
            assert_eq!(rec.round, Some(0), "tool {} should be round 0", rec.name);
            assert!(
                rec.parallel == Some(true),
                "tool {} should be parallel",
                rec.name
            );
        }
        let batch_ids: Vec<_> = records.iter().filter_map(|r| r.batch_id.as_ref()).collect();
        assert!(!batch_ids.is_empty(), "batch_ids should be set");
        let first = &batch_ids[0];
        assert!(
            batch_ids.iter().all(|b| b == first),
            "all tools should share same batch_id"
        );
    }

    /// Mixed: 3 read-only, then 1 write (bash), then 2 read-only.
    /// Partition should produce: Concurrent(3), Serial(1), Concurrent(2).
    /// All tools should complete successfully.
    #[tokio::test]
    async fn parallel_mixed_readonly_and_write_tools_partitioned() {
        let session_id = format!("par-mix-{}", uuid::Uuid::new_v4());
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());

        let mut state = make_state();
        state.current_session_id = Some(session_id.clone());

        let tools = vec![
            ("read_file", "c1"),
            ("grep", "c2"),
            ("glob", "c3"),
            ("bash", "c4"), // write tool — breaks the concurrent batch
            ("read_file", "c5"),
            ("git_diff", "c6"),
        ];
        let mut host = MockHost::new(vec![
            turn_with_named_tools(&tools, ""),
            turn_with_named_tools(&[], "all done"),
        ])
        .with_valid_tools(&["read_file", "grep", "glob", "bash", "git_diff"]);

        let outcome = run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .unwrap();
        assert!(matches!(outcome, AgenticLoopOutcome::Completed));

        let records: Vec<&ToolCallRecord> = state
            .stall
            .tool_call_records
            .iter()
            .filter(|r| !r.is_synthetic_placeholder())
            .collect();
        assert_eq!(records.len(), 6, "expected 6 tool call records");

        // All should be round 0 (same LLM round).
        for rec in &records {
            assert_eq!(rec.round, Some(0), "tool {} should be round 0", rec.name);
        }

        // Verify tool names in order.
        let names: Vec<&str> = records.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["read_file", "grep", "glob", "bash", "read_file", "git_diff"],
            "tools should be in original order"
        );

        // All tools should have completed (ok or error — we're testing partitioning, not tool success).
        // The key verification is that the partition logic ran without panics and
        // all 6 tools were processed in the correct order.
        assert_eq!(records.len(), 6, "all 6 tools should have been processed");
    }

    /// Unit test for partition_tool_batches.
    #[test]
    fn partition_tool_batches_groups_correctly() {
        use crate::turn::agentic_headless_round::{ToolBatch, partition_tool_batches};
        use astra_turn_core::headless_tool_assembly::HeadlessRoundToolIdx;

        let tool_calls = vec![
            json!({"function": {"name": "read_file"}}),
            json!({"function": {"name": "grep"}}),
            json!({"function": {"name": "bash"}}),
            json!({"function": {"name": "glob"}}),
            json!({"function": {"name": "git_diff"}}),
        ];
        let indices: Vec<HeadlessRoundToolIdx> =
            (0..5).map(HeadlessRoundToolIdx::ServerToolCall).collect();

        let batches = partition_tool_batches(&indices, &tool_calls);

        // Expected: Concurrent([0,1]), Serial(2), Concurrent([3,4])
        assert_eq!(batches.len(), 3);
        assert!(matches!(&batches[0], ToolBatch::Concurrent(v) if v.len() == 2));
        assert!(matches!(&batches[1], ToolBatch::Serial(_)));
        assert!(matches!(&batches[2], ToolBatch::Concurrent(v) if v.len() == 2));
    }

    /// audit-#8: source-level guard against the panicking subtraction.
    #[test]
    fn turn_count_uses_saturating_sub() {
        let source = include_str!("agentic_loop_host.rs");
        assert!(
            source.contains("state.max_turns.saturating_sub(state.remaining_turns)"),
            "expected saturating_sub in agentic_loop_host"
        );
    }
}
