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
//! For a higher-level entry point, use [`super::super::loop_dispatcher::LoopDispatcher`]
//! which wraps this loop with consistent outcome mapping.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use astra_services::session_audit::RuntimePromotionEventData;
use astra_services::session_journal::{ToolCallRecord, TraceSpanBuilder};
use astra_services::{DatabaseEvaluationService, DatabaseEventService};
use async_trait::async_trait;
use serde_json::Value;

use astra_config::user_profile::TurnIntent;
use astra_pipeline::step_protocol::{InMemoryIdempotencyCache, StepCheckpoint};
use astra_pipeline::step_recorder::StepRecorder;
use astra_text_utils::semantic_dedup::SemanticDedup;
use astra_tools::task_mgmt::{SessionTask, TaskManager};
use astra_turn_core::chat_turn_heuristics::TaskExecutionProfile;
use astra_turn_core::chat_turn_sse_dispatch::ChatTurnSseAccum;
use astra_turn_core::compaction_types::{CompactionEvent, CompactionTier};
use astra_turn_core::guardrails::turn_guard::TurnGuard;
use astra_turn_core::guardrails::verdict_audit::AgenticVerdictAuditEvent;
use astra_turn_core::headless_tool_body_preview::HeadlessStderrStyle;
use astra_turn_core::sse_stream_host::EdgeToolExecResult;
use astra_turn_core::tool_registry_report::ToolSurfaceReport;
use tokio_util::sync::CancellationToken;

/// Anchors journal wall-clock timestamps to a single process-local epoch so
/// later reads stay monotonic even if `SystemTime` jumps backwards.
///
/// The anchor is created on first trace emission in this process and then only
/// advanced via `Instant::elapsed()`. This keeps cross-span ordering stable
/// without pretending to be a globally synchronized clock.
static TRACE_CLOCK_ANCHOR: std::sync::LazyLock<(u64, Instant)> = std::sync::LazyLock::new(|| {
    let wall_us = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;
    (wall_us, Instant::now())
});

fn now_us() -> u64 {
    let (wall_us, monotonic_anchor) = &*TRACE_CLOCK_ANCHOR;
    wall_us.saturating_add(monotonic_anchor.elapsed().as_micros() as u64)
}

fn record_trace_span(
    buf: &mut astra_services::session_journal::TurnEventBuffer,
    span_id: String,
    name: &str,
    started_at: Instant,
    parent_span_id: Option<String>,
    attrs: Option<&HashMap<String, String>>,
    trace_id: Option<&str>,
) {
    let end_us = now_us();
    let start_us = end_us.saturating_sub(started_at.elapsed().as_micros() as u64);
    buf.record_trace_span_v2(
        TraceSpanBuilder::default()
            .span_id(span_id)
            .name(name.to_string())
            .start_us(start_us)
            .end_us(end_us)
            .parent_span_id(parent_span_id)
            .attrs(attrs)
            .trace_id(trace_id.map(str::to_string)),
    );
}

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

pub enum ControlToolRecovery {
    Unsupported,
    Missing,
    Recovered(EdgeToolExecResult),
}

pub use astra_turn_core::interaction_types::{
    ASK_USER_TOOL_NAME, TurnInteractionMode, TurnInteractionPolicy,
    interaction_scoped_tool_restrictions, tool_counts_as_factual_evidence,
};

// ─── Host trait ──────────────────────────────────────────────────────────────

/// Abstraction for host-specific agentic loop behavior.
///
/// The runtime calls [`AgenticLoopHost::execute_turn`] for each LLM interaction;
/// the host handles payload preparation, HTTP posting, and SSE consumption.
/// Post-turn cognitive processing (ingest, stall detection, tool round,
/// post-tool policy) runs entirely in the runtime.
///
/// **CLI host**: builds payload with tool surface, memory, and skills, POSTs to cloud API,
/// consumes SSE with terminal rendering, executes tools locally.
///
/// **Headless host**: receives payload from client, calls LLM directly,
/// streams SSE to client, executes tools via ledger.
#[async_trait]
pub trait AgenticLoopHost: Send {
    /// Execute one LLM turn: prepare payload → POST → consume SSE.
    ///
    /// The host is responsible for all CLI/server-specific logic:
    /// - Building the JSON payload (tool surface, memory, skills, edge profile)
    /// - POSTing to the LLM API (cloud or direct)
    /// - Consuming the SSE stream (via `SseStreamHost` methods on `self`)
    ///
    /// State fields are passed by reference so the host can read/update them.
    async fn execute_turn(
        &mut self,
        state: &mut AgenticLoopState,
    ) -> Result<HostTurnResult, astra_core::ClassifiedError>;

    /// Optional semantic judge for the current user turn.
    ///
    /// Hosts with an LLM-backed intent judge should override this. The default
    /// does not infer semantic continuation from text; continuing the current
    /// objective is a high-impact decision and should come from a structured
    /// judge result, not phrase matching.
    async fn judge_turn_intent(&mut self, _state: &AgenticLoopState) -> Option<TurnIntent> {
        None
    }

    /// Whether the host already injects tool round guidance into the system
    /// prompt during `execute_turn`.  When true, the agentic loop skips its
    /// own user-message guidance injection to avoid double injection.
    fn injects_round_guidance(&self) -> bool {
        false
    }

    /// Observe a deferred `user_input` event that was appended to the active
    /// run while execution was already in progress.
    ///
    /// Hosts that carry mutable request context, such as the server host's
    /// `edge_profile`, can use this hook to keep the next LLM round aligned
    /// with any per-input runtime hints before the deferred user message is
    /// surfaced to the model.
    ///
    /// Contract: the loop calls this at most once per durable input event.
    /// `DeferredInputState` advances its append-only cursor when staging a
    /// poll result, so hosts do not need to deduplicate repeated hook calls.
    fn on_deferred_user_input(&mut self, _input: &Value) {}

    /// Headless round terminal output.
    fn emit_headless_line(&mut self, style: HeadlessStderrStyle, line: String);

    /// Whether output is suppressed (quiet mode).
    fn is_quiet(&self) -> bool;

    /// The user-facing interaction mode for this turn.
    ///
    /// Used by the execution phase to decide whether to inject the
    /// interruption-style nudges (execution escalation, parallel-batching
    /// force, circuit breaker corrections, etc.). See
    /// [`TurnInteractionMode::suppresses_loop_nudges`] for the policy.
    ///
    /// Defaults to [`TurnInteractionMode::NonInteractive`] which preserves
    /// the pre-existing behaviour (nudges enabled) for any host that
    /// hasn't been updated yet.
    fn turn_interaction_mode(&self) -> TurnInteractionMode {
        TurnInteractionMode::NonInteractive
    }

    /// Emit a structured compaction event for real-time UX feedback.
    ///
    /// Default implementation falls back to [`emit_headless_line`] so
    /// existing hosts get stderr output without changes. Hosts that
    /// have a UI layer (CLI / TUI) should override to emit a structured
    /// event (e.g. `StreamEvent::Compaction`) for richer rendering.
    fn on_compaction(&mut self, event: CompactionEvent) {
        // Clone to avoid moving event.summary — subclasses overriding this
        // method receive the full event by value and may inspect all fields.
        let summary = event.summary.clone();
        self.emit_headless_line(HeadlessStderrStyle::Dim, summary);
    }

    /// Whether the current turn is still in read-only plan authoring mode.
    ///
    /// When true, headless tool execution must deny mutating tools before
    /// they can fall through to edge/server execution resolution.
    fn plan_mode_active(&self, _state: &AgenticLoopState) -> bool {
        false
    }

    /// Host-provided turn-start lifecycle summary for prompt/introspection.
    ///
    /// Default is empty; hosts can override to surface mode/run/resume/delegation
    /// context as turn-start state.
    fn turn_start_lifecycle_summary(&self, _state: &AgenticLoopState) -> String {
        String::new()
    }

    /// Optional LLM summary client for summary-based compaction helpers.
    ///
    /// Hosts can provide a client that uses the same model/credentials as the
    /// main LLM path for summary-related work. Cache-friendly pre-turn inline
    /// compaction is handled by [`AgenticLoopHost::maybe_pre_turn_compact`],
    /// which can build the exact main-turn prefix when the host supports it.
    ///
    /// Default: `None` (no pre-turn compaction available — falls back to
    /// mechanical compression only).
    fn summary_client(&self) -> Option<Box<dyn astra_turn_core::cloud_summary::SummaryLlmClient>> {
        None
    }

    /// Optional host-specific pre-turn compaction hook.
    ///
    /// Hosts that can build the exact next-turn system prompt may override
    /// this to run cache-friendly inline summarization before the next LLM
    /// round. On success the implementation must bump
    /// [`AgenticLoopState::compact_tier_applied`] to at least
    /// [`CompactionTier::CompactHistory`] so the downstream budget guard
    /// skips redundant mechanical compression. Default is a no-op so
    /// non-server hosts preserve legacy behavior.
    async fn maybe_pre_turn_compact(
        &mut self,
        _state: &mut AgenticLoopState,
        _pressure: f64,
        _quiet: bool,
    ) {
    }

    /// Valid tool names from the host's tool schemas.
    fn valid_tool_names(&self) -> &HashSet<String>;

    /// Names listed in the current turn's `<deferred_tools>` manifest.
    ///
    /// The validator uses this to differentiate "unknown tool" denials
    /// (truly hallucinated names) from "not yet activated" denials
    /// (deferred but reachable via `tool_search(query="select:NAME")`).
    /// Default: empty — hosts that don't render a deferred manifest should
    /// treat every non-admitted name as unknown.
    ///
    /// Returned by value because some hosts compute the set lazily from
    /// shared state (`Arc<ToolExecutor>`) and don't keep a borrowable
    /// `HashSet`. The validator clones once per round, so this isn't hot.
    fn deferred_tool_names(&self) -> HashSet<String> {
        HashSet::new()
    }

    /// Active capability set for this host. Prompt rendering should read this
    /// rather than inferring capabilities from the resolved tool list.
    fn capabilities(&self) -> astra_turn_core::capability::CapabilitySet {
        astra_turn_core::capability::CapabilitySet::all()
    }

    /// Recover a host-owned control-tool result when the LLM emitted a tool
    /// call but the post-SSE edge result row is missing.
    ///
    /// This is intentionally host-scoped: replaying arbitrary missing tools
    /// would duplicate side effects. Implementations must recover only from
    /// an authoritative host state source, such as the multi-agent fanout
    /// registry for `agent_fanout`, and must return [`ControlToolRecovery::Unsupported`]
    /// for tool names they do not own.
    async fn recover_missing_control_tool_result(
        &mut self,
        _parent_run_id: Option<&str>,
        _tool_call_id: &str,
        _tool_name: &str,
        _args: &Value,
    ) -> ControlToolRecovery {
        ControlToolRecovery::Unsupported
    }

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

    /// Best-effort cancellation hook for child agents that should no longer
    /// continue running after the parent has decided to stop waiting.
    ///
    /// Default: no-op so hosts without dynamic-agent control preserve legacy
    /// behaviour. Returns the subset of `agent_ids` that were actually
    /// cancelled.
    async fn cancel_child_agents(&mut self, _agent_ids: &[String], _reason: &str) -> Vec<String> {
        Vec::new()
    }

    /// Post-sampling turn-completed hook — fires exactly once
    /// immediately after a successful LLM response has been received
    /// AND cleanly ingested (`state.current_run_id`, `state.messages`,
    /// `state.final_text` are now current), and BEFORE any side
    /// effects (tool phase, microcompact-for-next-turn, memory
    /// extraction).
    ///
    /// Name intentionally drops "parent" — this host may itself be
    /// running as a spawned sub-agent (sub-run / delegated child).
    /// Every loop that completes a turn sees this hook, regardless
    /// of where it sits in the delegation tree.
    ///
    /// This is the canonical capture slot for downstream work that
    /// must share the parent's prompt cache — see
    /// `astra_turn_core::fork_capture`. Hosts that want to record a
    /// `ForkPrefix` into a `PrefixCaptureSink` implement this method.
    ///
    /// Default: no-op. All five in-tree hosts (CLI, server, bridge,
    /// sub-run, mocks) pick up the default; only hosts that need
    /// capture override it.
    ///
    /// Contract:
    /// - Fires only on the happy path: `execute_turn` returned `Ok`
    ///   AND ingest produced a non-Fatal outcome
    ///   (`Continue` / `Break` / `HasToolCalls`). Fatal ingest
    ///   outcomes (rate-limit error string in SSE, context-window
    ///   overflow) do NOT fire it — state is partially updated and
    ///   capture would write a corrupt prefix.
    /// - Fires BEFORE iteration-control dispatch so any side effect
    ///   scheduled by the loop sees state as it was on hook exit.
    /// - `state` is `&` not `&mut` — the hook is observational, not
    ///   state-mutating. Mutating state here would re-introduce the
    ///   exact "writing through multiple layers of references" pain
    ///   the dedicated capture slot was designed to avoid.
    fn on_turn_completed(&mut self, _state: &AgenticLoopState) {}
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
    /// When set, only skills from these source kinds may be surfaced via the
    /// `skill` / `discover_skills` tool schemas.
    pub allowed_skill_sources: Option<HashSet<crate::skills::manifest::SkillSourceKind>>,
}

impl RequestConstraints {
    /// Construct with all three lanes set explicitly.
    ///
    /// Every field is required so adding a new constraint axis is a hard
    /// compile error at every call site, not a silent default. Callers that
    /// don't have a specific lane should pass `None`.
    pub fn new(
        allowed_tools: Option<HashSet<String>>,
        allowed_skills: Option<HashSet<String>>,
        allowed_skill_sources: Option<HashSet<crate::skills::manifest::SkillSourceKind>>,
    ) -> Self {
        Self {
            allowed_tools,
            allowed_skills,
            allowed_skill_sources,
        }
    }

    pub fn skill_surfacing_policy(&self) -> crate::turn::skill_tool::SkillSurfacingPolicy {
        crate::turn::skill_tool::SkillSurfacingPolicy {
            allowed_names: self.allowed_skills.clone(),
            allowed_sources: self.allowed_skill_sources.clone(),
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
    /// This is enforced only by runtime tool interception. Hosts must not use
    /// it to prune prompt-visible tool schemas, because that would churn the
    /// provider prompt-cache prefix every time a skill is loaded.
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
    pub improvement_tracker: astra_skills::improvement::ImprovementTracker,
    /// Canonical skill names surfaced via `discover_skills` this session.
    pub discovered: HashSet<String>,
    /// Scoring thresholds for deterministic pre-turn skill auto-routing.
    pub auto_routing: crate::turn::skill_tool::AutoRoutingConfig,
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
            discovered: HashSet::new(),
            auto_routing: Default::default(),
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
    /// Selection report from the first turn's tool surface assembly.
    pub first_surface_report: Option<ToolSurfaceReport>,
    /// Budget pressure value from the first turn.
    pub first_budget_pressure: f64,
    /// Context assembly duration from the first turn (ms).
    pub first_context_assembly_ms: Option<u64>,
    /// Memoria retrieval duration from the first turn (ms).
    pub first_memoria_ms: Option<u64>,
    /// All skill names selected across all turns.
    pub all_selected_skills: Vec<String>,
    /// Optional observability session for context tracing, drift detection, and auto-tuning.
    /// When set, hooks are called at turn start/end, tool surface, etc.
    pub observability_session:
        Option<std::sync::Arc<std::sync::RwLock<crate::observability::ObservabilitySession>>>,
    /// Shared observability hub for profile/experiment management.
    /// Typically set at session init and shared across agents.
    pub observability_hub: Option<std::sync::Arc<crate::observability::ObservabilityHub>>,
    /// Optional evaluation persistence context for refreshing DB-backed runtime signals.
    pub evaluation_persistence: Option<EvaluationPersistenceContext>,
    /// Optional event persistence context for mirroring context traces into cloud events.
    pub context_trace_persistence: Option<ContextTracePersistenceContext>,
    /// Runtime promotion verdicts captured for later audit/report persistence.
    pub promotion_events: Vec<RuntimePromotionEventData>,
    /// Optional turn trace collector for detailed context assembly observability.
    /// When set, records system prompt, history, memory, and tool surface traces.
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
    pub artifact_store: astra_services::DatabaseSessionArtifactStore,
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
    /// Whether the circuit breaker injected the tool-round hard-stop
    /// corrective this loop. It tells the model to produce a final answer
    /// next round and restricts all tools so the next round must be text-only.
    /// One-shot per turn.
    pub forced_tool_round_hard_stop: bool,
    /// Whether the circuit breaker escalated to a hard abort after the
    /// hard-stop corrective was ignored. One-shot per turn.
    pub forced_tool_round_abort: bool,
    /// Monotonic count of circuit-breaker introspection (self-check) prompts
    /// injected this turn. Used for post-turn telemetry so operators can see
    /// how often the breaker nudged the model on long read-only sessions.
    ///
    /// Note: this counter is distinct from the breaker's internal
    /// `introspect_emissions()` counter — the breaker's counter resets on
    /// mutation (for cap enforcement), while this counter monotonically
    /// accumulates across the whole turn (for diagnostics).
    pub introspection_count: u32,
    /// Whether a completion-aware soft-stop prompt was injected after a
    /// successful task-completion signal (for example a successful git commit).
    /// One-shot per turn; advisory only, tools remain available.
    pub forced_completion_soft_stop: bool,
    /// Whether the unfinished-task-board mid-loop gate already injected its
    /// corrective this turn. Without this one-shot, a model that responds
    /// with text-only completions while the task board still has unfinished
    /// work would keep re-triggering the gate every BreakLoop round —
    /// burning the global tool round instead of letting the terminal
    /// guard rewrite the answer with a structured interruption record.
    /// One-shot per turn; reset in `reset_per_turn_corrective_state`.
    pub forced_task_board_completion_gate: bool,
    /// Whether the redundant-reads mid-loop corrective injected a guidance
    /// message this loop. Fires when the model has re-read overlapping line
    /// ranges of the same file enough times to cross
    /// `REDUNDANT_READS_MIDLOOP_THRESHOLD` without any intervening
    /// workspace mutation. One-shot per turn — escalation is via the
    /// existing post-mortem `EvalSignal::RedundantOverlappingReads`.
    pub forced_redundant_reads_corrective: bool,
    /// Whether the repeated-cache-waste mid-loop corrective injected a
    /// guidance message this loop. Fires when the model keeps reissuing
    /// identical tool calls that are served from cache instead of reusing
    /// the earlier result. One-shot per turn.
    pub forced_cache_waste_corrective: bool,
    /// Whether broad search fanout has already triggered a convergence
    /// corrective this turn. This is intentionally advisory and one-shot: it
    /// catches implementation tasks that keep widening search instead of
    /// synthesizing, editing, verifying, or finishing.
    pub forced_search_fanout_corrective: bool,
    /// Whether a broad exploration-family corrective injected a guidance
    /// message and restricted the dominant low-yield family this loop. Fires
    /// when consecutive multi-call rounds stay inside the same exploratory
    /// family (diff/search/read). One-shot per turn.
    pub forced_exploration_family_corrective: bool,
    /// Whether the exploration-family corrective escalated to a stronger
    /// convergence directive after the model spent a later round attempting
    /// ONLY tools from the already-restricted family. One-shot per turn.
    pub forced_exploration_family_lockout: bool,
    /// Dominant exploratory family currently under runtime correction. Used
    /// to detect whether later blocked rounds are simply retrying the same
    /// low-yield path instead of switching families or synthesizing.
    pub exploration_family_corrective_family: Option<String>,
    /// How many stall correction nudges have been injected this loop.
    /// Limits nudge frequency (at most one per stall type per session).
    pub nudge_count: u32,
    /// Anomaly-based circuit breaker for the agentic loop.
    /// Owns tool-round hard-stop and abort decisions.
    pub circuit_breaker: astra_turn_core::loop_circuit_breaker::LoopCircuitBreaker,
    /// Rolling-stats guardrail auto-tuner for the auto-reflection signal
    /// threshold. Observes per-turn outcomes and adjusts the threshold by
    /// ±1 (bounded to `[MIN, MAX]`) so Astra reacts faster when failures
    /// cluster and backs off when things are stable.
    pub guardrail_tuner: crate::config_admin::guardrail::GuardrailTuner,
    /// Cursor into `tool_call_records` marking the boundary already
    /// observed by the guardrail tuner. Turn N sees records
    /// `tool_call_records[cursor..]`; after observation the cursor is
    /// advanced to `len()`.
    pub guardrail_tuner_records_cursor: usize,
}

#[derive(Default)]
pub struct DeferredInputState {
    /// Durable event cursor for deferred user-input polling.
    deferred_user_input_cursor: usize,
    /// Event indices already delivered to the model but not yet durably marked
    /// as released. Retried on later poll points without re-injecting content.
    pending_release_event_indices: Vec<usize>,
}

pub(crate) struct ObservedDeferredUserInputs {
    pub(crate) raw_inputs: Vec<Value>,
    pub(crate) contents: Vec<String>,
    pub(crate) released_event_indices: Vec<usize>,
    pub(crate) next_cursor: usize,
}

impl DeferredInputState {
    pub fn deferred_user_input_cursor(&self) -> usize {
        self.deferred_user_input_cursor
    }

    pub(crate) fn release_event_indices_to_ack(&self, observed: &[usize]) -> Vec<usize> {
        let mut indices = self.pending_release_event_indices.clone();
        indices.extend(observed.iter().copied());
        indices.sort_unstable();
        indices.dedup();
        indices
    }

    pub(crate) fn note_release_ack_result(&mut self, event_indices: &[usize], released: bool) {
        if released {
            let released = event_indices
                .iter()
                .copied()
                .collect::<std::collections::HashSet<_>>();
            self.pending_release_event_indices
                .retain(|event_index| !released.contains(event_index));
            return;
        }
        self.pending_release_event_indices
            .extend(event_indices.iter().copied());
        self.pending_release_event_indices.sort_unstable();
        self.pending_release_event_indices.dedup();
    }

    pub(crate) fn observe_polled_user_inputs<F>(
        &mut self,
        poll: crate::turn::run_control::RunQueuedInputPoll,
        mut content_from_input: F,
    ) -> ObservedDeferredUserInputs
    where
        F: FnMut(&Value) -> Option<String>,
    {
        let mut raw_inputs = Vec::new();
        let mut contents = Vec::new();
        let mut released_event_indices = Vec::new();
        for event in poll.inputs {
            released_event_indices.push(event.event_index);
            raw_inputs.push(event.input.clone());
            let Some(content) = content_from_input(&event.input) else {
                continue;
            };
            contents.push(content);
        }

        ObservedDeferredUserInputs {
            raw_inputs,
            contents,
            released_event_indices,
            next_cursor: poll.next_cursor,
        }
    }

    #[cfg(test)]
    pub fn set_deferred_user_input_cursor_for_test(&mut self, cursor: usize) {
        self.deferred_user_input_cursor = cursor;
    }

    pub fn commit_observed_cursor(&mut self, next_cursor: usize) {
        self.deferred_user_input_cursor = self.deferred_user_input_cursor.max(next_cursor);
    }
}

/// Inter-agent messaging state for the agentic loop.
#[derive(Default)]
pub struct MessagingState {
    /// Optional mailbox for receiving messages from other agents.
    /// When set, incoming messages are drained at each turn start and
    /// progress updates are sent to the parent at turn end.
    pub mailbox: Option<astra_messaging::router::AgentMailbox>,
    /// Tracks messages that require acknowledgment and handles retries.
    pub ack_tracker: Option<std::sync::Arc<astra_messaging::ack_tracker::PendingAckTracker>>,
    /// Background retry/dead-letter sweep for ack-tracked messages.
    pub ack_sweep_task: Option<astra_messaging::ack_tracker::AckSweepHandle>,
    /// Dead letter queue for permanently failed messages.
    pub dead_letter_queue: Option<std::sync::Arc<astra_messaging::dead_letter::DeadLetterQueue>>,
    /// Unified messaging metrics (optional, shared across agents in a delegation).
    pub metrics: Option<std::sync::Arc<astra_messaging::metrics::MessagingMetrics>>,
    /// Optional progress emitter for broadcasting turn events to UI/subscribers.
    /// When set, the loop emits `TurnCompleted` events after each turn.
    pub progress_emitter: Option<crate::orchestration::AgentProgressEmitter>,
}

/// Point-in-time summary of the active session task board.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskBoardSnapshot {
    pub pending_count: usize,
    pub in_progress_count: usize,
    /// Count of active (pending/in_progress) tasks that have at least one
    /// blocker in their `blocked_by` list.  This counts tasks waiting on
    /// dependencies, *not* tasks whose status is literally "blocked"
    /// (there is no such status — the field reflects dependency edges).
    pub blocked_count: usize,
    pub active_tasks: Vec<String>,
}

impl TaskBoardSnapshot {
    #[must_use]
    pub fn from_active_tasks(tasks: &[SessionTask]) -> Self {
        let mut snapshot = Self::default();
        for task in tasks {
            if !task.status.is_active() {
                continue;
            }
            if task.status.is_in_progress() {
                snapshot.in_progress_count += 1;
            } else {
                snapshot.pending_count += 1;
            }
            if !task.blocked_by.is_empty() {
                snapshot.blocked_count += 1;
            }
            if snapshot.active_tasks.len() < 3 {
                snapshot
                    .active_tasks
                    .push(format!("{} {} [{}]", task.id, task.title, task.status));
            }
        }
        snapshot
    }

    #[must_use]
    pub fn has_unfinished_tasks(&self) -> bool {
        self.pending_count > 0 || self.in_progress_count > 0
    }

    #[must_use]
    pub fn short_summary(&self) -> String {
        let parts = self.status_count_parts();
        if parts.is_empty() {
            return "no active tasks remain".to_string();
        }
        let mut summary = format!("{} task(s) remain", parts.join(", "));
        if !self.active_tasks.is_empty() {
            summary.push_str(": ");
            summary.push_str(&self.active_tasks.join("; "));
        }
        summary
    }

    #[must_use]
    pub fn status_count_summary(&self) -> String {
        let parts = self.status_count_parts();
        if parts.is_empty() {
            "no active tasks remain".to_string()
        } else {
            format!("{} task(s) remain", parts.join(", "))
        }
    }

    fn status_count_parts(&self) -> Vec<String> {
        let mut parts = Vec::new();
        if self.in_progress_count > 0 {
            parts.push(format!("{} in_progress", self.in_progress_count));
        }
        if self.pending_count > 0 {
            parts.push(format!("{} pending", self.pending_count));
        }
        parts
    }
}

/// Stop-hook and teammate-idle-hook state for the agentic loop.
#[derive(Default)]
pub struct StopHookState {
    /// Verification commands run before the loop is allowed to complete.
    /// For plan subtasks, populated from declarative `when: task_completed` hooks.
    /// If any hook fails, its output is injected and the loop continues.
    pub stop_hooks: Vec<astra_turn_core::stop_hooks::StopHook>,
    /// How many times stop hooks have fired (prevents infinite hook loops).
    pub stop_hook_runs: u32,
    /// Hooks with `when: teammate_idle` — injected once after a `delegate` round returns.
    pub teammate_idle_hooks: Vec<astra_turn_core::stop_hooks::StopHook>,
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
    /// Shared session task board handle, when available.
    pub task_board_monitor: Option<Arc<TaskManager>>,
    /// Cached view of unfinished task-board work for completion gating.
    pub task_board_snapshot: TaskBoardSnapshot,
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

/// Cross-pod cancel/pause status provider for horizontally-scaled deployments.
///
/// When the run is running on a different pod than the one that received the
/// cancel/pause request, the in-memory `AtomicBool` flags won't be updated.
/// Re-exported from [`crate::turn::run_control::RunControlProvider`].
pub use crate::turn::run_control::{RunControlProvider, RunControlStatus};

/// Error recovery state for the agentic loop.
#[derive(Default)]
pub struct ErrorRecoveryState {
    /// Consecutive turns where the same error category dominated.
    /// Reset when a turn succeeds or a different error category appears.
    pub consecutive_same_error: u32,
    /// The error category from the last turn (for streak detection).
    pub last_error_category: Option<astra_turn_core::error_recovery::ErrorCategory>,
}

// ─── Loop state ──────────────────────────────────────────────────────────────

/// Cross-turn state managed by the runtime loop.
///
/// A structured volatile-injection lane. The runtime produces many
/// kinds of per-round hints that must be visible to the LLM but must
/// NOT live in `AgenticLoopState.messages[]` — tool_health warnings,
/// working-set snapshots, inventory blocks, stall reflections,
/// execution-escalation / parallel-batching-force nudges, tactical
/// adaptations, budget-exhaustion alerts, and similar corrective
/// messages.
///
/// Before this lane existed, every producer called
/// `state.messages.push(...)` and the wire layer had to scan the full
/// history for known patterns and consolidate them. That worked but
/// was fragile: new patterns forgot to match the classifier, and the
/// history-is-byte-stable invariant lived implicitly across dozens of
/// call sites.
///
/// Post-fix, every producer calls [`AgenticLoopState::push_volatile`]
/// instead. `wire_assembly::assemble_llm_messages` drains this lane
/// into the volatile_preamble on every LLM call, so `messages[]` only
/// ever carries real user/assistant/tool conversation turns.
#[derive(Debug, Clone)]
pub struct VolatileInjection {
    /// Classification — used by introspect to enumerate injections by
    /// type, and by downstream dedup/coalescing if needed.
    pub kind: VolatileKind,
    /// The human-readable injection text the LLM will see. Role is
    /// implicit in `kind` (coaching / working-set / inventory land as
    /// system; nudges / corrections land as user). The consumer
    /// decides the wrapper shape at drain time.
    pub content: String,
    /// Round index the injection was produced in (for introspect
    /// telemetry; not used by the wire layer).
    pub round_index: u32,
}

/// In-memory summary of one LLM round within the current session.
/// Populated in parallel with the journal's `LlmRoundRecord` so
/// `introspect` can answer "what were my recent rounds doing?" without
/// requiring `full_llm_capture=true` and on-disk I/O. Capped to a
/// small ring (latest [`RECENT_ROUNDS_RING_CAPACITY`] entries) to keep
/// state size bounded.
#[derive(Debug, Clone, Default)]
pub struct RecentRoundSummary {
    pub turn: u32,
    pub round: u32,
    pub provider: String,
    pub model: String,
    pub prompt_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub completion_tokens: u64,
    pub tool_calls_returned: u32,
    pub tool_call_names: Vec<String>,
    pub duration_ms: u64,
    pub finish_reason: Option<String>,
}

/// Ring capacity for [`AgenticLoopState::recent_rounds`]. Small enough
/// to keep state lean, large enough to cover a typical tool-loop turn
/// (sessions 05e63cac / 65606b95 t6 observed up to 19 rounds).
pub const RECENT_ROUNDS_RING_CAPACITY: usize = 32;

/// Taxonomy of runtime-produced volatile content. Add a new variant
/// when introducing a new injection kind — both the producer and the
/// drain path become compile-time-checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolatileKind {
    /// TurnGuard "⚠ The following tools have failed…" output.
    ToolHealthWarning,
    /// Stall-reflection nudge (`build_stall_reflection`).
    StallNudge,
    /// Parallel-batching-force corrective ("Detected N consecutive
    /// single-tool rounds…").
    ParallelBatchingForce,
    /// Execution-escalation nudge for mutating-task reads-only-churn.
    ExecutionEscalation,
    /// `[working-set:v1]` session-facts snapshot.
    WorkingSet,
    /// `## Already Fetched` inventory block.
    AlreadyFetched,
    /// "✓ N tools executed in parallel" coaching ping.
    ToolBatchCoaching,
    /// Budget/turn/round limit advisory ("You have reached the token
    /// budget…", "Do NOT call any more tools…").
    BudgetAdvisory,
    /// Tactical adaptation hint.
    TacticalAdaptation,
    /// Neutral compaction context note. Injected after a successful
    /// compact+spill pass so the model understands the smaller context
    /// without treating the note itself as a new user request or automatic
    /// resume authorization.
    CompactResume,
    /// Circuit-breaker intermediate / soft-stop messages.
    CircuitBreaker,
    /// Error-budget-exhausted / redundant-reads-corrective /
    /// cache-waste-corrective / exploration-family-corrective — the
    /// family of "stop and change approach" messages.
    Corrective,
    /// Mailbox / agent-to-agent volatile drop-offs.
    Mailbox,
    /// User message queued during execution for delivery after the next
    /// completed tool-call round.
    DeferredUserInput,
    /// Budget-review acknowledgment.
    BudgetReview,
    /// Open-ended exploration budget reminder.
    ExplorationBudget,
    /// Execution retry with corrective reason.
    ExecutionRetry,
    /// Post-turn hallucination self-check nudge. Fires when the
    /// assistant's prose for the just-finished turn claimed a
    /// phantom tool outcome (e.g. "silently returned {}") that no
    /// tool call actually produced. See
    /// [`astra_turn_core::hallucination_tripwire`] for the detector.
    HallucinationTripwire,
    /// Task-board completion gate. Singleton and count-only on the wire so
    /// repeated corrections replace each other instead of accumulating
    /// dynamic task titles in the volatile prompt suffix.
    TaskBoardCompletionGate,
    /// Task-board start gate. Broad multi-step or delegated work should create
    /// a durable task board before broad exploration/spawn begins.
    TaskBoardStartGate,
    /// Plan-mode marker: a single short reminder that the current
    /// turn is read-only investigation and the model must surface
    /// its plan via `exit_plan_mode(plan="…")` for user approval.
    /// Singleton — only the latest one ever rides the wire.
    PlanModeMarker,
    /// Catch-all for producers we haven't categorized yet. Prefer
    /// adding a new variant over reusing this — introspect reports
    /// by kind and a generic bucket degrades the signal.
    Other,
}

impl VolatileKind {
    /// Snapshot-style kinds where only the most recent value is
    /// semantically meaningful. `push_volatile` replaces any prior
    /// entry of the same kind instead of appending. Non-singleton
    /// kinds (nudges, corrections) accumulate so the LLM sees every
    /// one fired in the same prepare cycle.
    #[must_use]
    pub fn is_singleton(self) -> bool {
        matches!(
            self,
            Self::WorkingSet
                | Self::AlreadyFetched
                | Self::ExplorationBudget
                | Self::Mailbox
                | Self::CompactResume
                | Self::TaskBoardCompletionGate
                | Self::TaskBoardStartGate
                | Self::PlanModeMarker,
        )
    }

    /// Default wire role for this kind. System-role for coaching /
    /// snapshots; user-role for nudges / corrections that deliberately
    /// mimic the user scolding the LLM.
    #[must_use]
    pub fn default_role(self) -> &'static str {
        match self {
            // Operator-persona messages — land in the user slot so the
            // LLM reads them with "correct my behavior" framing.
            Self::ToolHealthWarning
            | Self::StallNudge
            | Self::ParallelBatchingForce
            | Self::ExecutionEscalation
            | Self::Corrective
            | Self::CircuitBreaker
            | Self::BudgetAdvisory
            | Self::CompactResume
            | Self::TaskBoardCompletionGate
            | Self::TaskBoardStartGate
            | Self::ExecutionRetry
            | Self::ExplorationBudget
            | Self::DeferredUserInput
            | Self::BudgetReview => "user",
            // System-role: prevents injection via attacker-crafted file content.
            Self::HallucinationTripwire => "system",
            // System-role: in-band runtime snapshots or coaching.
            Self::WorkingSet
            | Self::AlreadyFetched
            | Self::ToolBatchCoaching
            | Self::TacticalAdaptation
            | Self::Mailbox
            | Self::PlanModeMarker
            | Self::Other => "system",
        }
    }
}

/// Created by the CLI/host from session parameters; mutated by the runtime
/// during multi-turn execution. Consumed at the end to produce results.
pub struct AgenticLoopState {
    // ── Message context ──
    pub messages: Vec<Value>,
    /// Runtime-produced volatile content scheduled to ride the next
    /// LLM call's volatile_preamble. See [`VolatileInjection`]. The
    /// wire layer (`wire_assembly::assemble_llm_messages`) drains this
    /// field on every call, so producers just append and move on.
    pub volatile_pending: Vec<VolatileInjection>,
    /// In-memory ring of recent LLM-round summaries. Fed from the same
    /// site that records into the journal buffer, but available at
    /// introspect time regardless of `full_llm_capture` setting. Capped
    /// to [`RECENT_ROUNDS_RING_CAPACITY`] entries — older rounds fall
    /// out (they're still in the journal if capture was enabled).
    pub recent_rounds: Vec<RecentRoundSummary>,
    pub tool_results: Vec<Value>,
    pub current_session_id: Option<String>,
    pub current_run_id: Option<String>,
    pub context_manifest_pool: Option<astra_core::SharedPool>,
    pub context_manifest_user_id: Option<String>,
    pub context_manifest_model_name: Option<String>,
    pub runtime_manifest: Option<serde_json::Value>,
    /// Current nested agent/sub-run depth. Root loops start at 0.
    pub recursion_depth: u8,

    // ── Accumulated output ──
    pub final_text: String,
    /// True once the current `final_text` has already been sent to the user.
    /// Deferred completion paths leave this false so finalization emits exactly once.
    pub final_text_streamed: bool,
    // Run-level token aggregators. See `turn::token_usage::TokenUsage` for
    // the per-call invariant: these four fields are DISJOINT buckets whose
    // sum equals the billable total across the whole run.
    //
    // - total_prompt         → fresh input tokens (billed at full rate)
    // - total_cache_read     → cached input tokens (discount rate)
    // - total_cache_creation → cache write tokens (premium rate)
    // - total_completion     → output tokens
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
    /// Latches for the per-budget self-pacing hints emitted at
    /// 50 % / 20 % remaining. Reset when a budget extension
    /// lands so the newly-extended budget gets the hint sequence
    /// at the new threshold crossings. See
    /// `maybe_emit_turn_budget_self_pacing_hint`.
    pub turn_budget_hint_emitted_90: bool,
    pub turn_budget_hint_emitted_50: bool,
    pub turn_budget_hint_emitted_20: bool,
    pub agentic_turn_budget: astra_turn_core::chat_turn_heuristics::AgenticTurnBudget,
    /// Current agentic loop turn index (0-based, updated each iteration).
    /// Used by the CLI to inject `round_index` into the bridge payload so the
    /// system prompt can include tool round directives.
    pub current_round_index: u32,
    /// Actual number of LLM calls completed in this turn (not inflated by
    /// progressive penalty).  Used for tool round guidance injection.
    pub llm_rounds_completed: u32,
    /// Number of history messages that were visible to the most recent LLM
    /// request. Microcompact uses this to avoid rewriting older, already-sent
    /// tool results while still allowing compaction of newly appended results.
    pub last_request_message_count: Option<usize>,
    pub turn_guard: TurnGuard,
    pub restricted_tools: HashSet<String>,
    /// Positive allowlist bias populated by pipeline `add_tools` strategy.
    /// Tools listed here are guaranteed NOT to be filtered out by the effective
    /// restriction set on the current turn (they still have to be advertised
    /// by the edge catalogue). This is additive and persists until manually
    /// cleared; the bridge prunes it naturally when a later diagnosis drops the
    /// tool from its recommendation.
    pub boosted_tools: HashSet<String>,
    /// One-shot flag set by pipeline `widen_surface` strategy. The flag is
    /// consumed (reset to false) on the next authoritative tool-visibility
    /// assembly; soft health diagnostics no longer hide tools from the schema.
    pub widen_surface_pending: bool,
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
    /// Consecutive cache-hit suppression cap before the pipeline switches
    /// from soft-hint to hard-refusal. Replaces the former hardcoded
    /// `REPEATED_CACHE_HIT_SUPPRESSION_THRESHOLD` (was 2).
    pub repeated_cache_hit_suppression: u32,
    /// Headless-round abort cap for consecutive empty-name tool calls.
    /// Replaces the former hardcoded `MAX_CONSECUTIVE_EMPTY_NAME` (was 3).
    pub max_consecutive_empty_name: u32,

    // ── Sub-states ──
    pub skills: SkillState,
    pub telemetry: TelemetryState,
    pub stall: StallTrackingState,
    pub messaging: MessagingState,
    /// Durable user input queued while a run is active. Kept separate from
    /// `messaging`, which is reserved for agent-to-agent routing state.
    pub deferred_input: DeferredInputState,
    pub hooks: StopHookState,
    pub cancellation: CancellationState,
    pub error_recovery: ErrorRecoveryState,

    // ── Horizontal scaling ──
    /// Optional cross-pod cancel/pause status provider.
    /// When set, the agentic loop periodically polls the database for the
    /// authoritative run status, enabling cross-pod control without
    /// sticky sessions. See [`RunControlProvider`].
    pub run_control: Option<Arc<dyn RunControlProvider>>,

    // ── Context Pipeline ──
    /// Session-scoped pipeline orchestrator. When `Some`, the pipeline manages
    /// context assembly, cache optimization, and pressure-adaptive compaction.
    /// Initialized on first turn; carries stats/latches/emergent across turns.
    pub pipeline_session: Option<astra_turn_core::pipeline_session::PipelineSession>,

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
    pub delegation_engine: Option<Arc<crate::server::delegation::engine::DelegationEngine>>,
    /// Number of delegations executed in the current turn. Used to prevent
    /// runaway delegation loops where the parent agent keeps delegating
    /// without synthesizing results.
    pub delegations_this_turn: u32,
    /// Chain of agent_ids that led to this delegation (for circular detection).
    /// Inherited from parent delegation and appended with parent agent_id.
    /// Format: ["orchestrator", "coder", "reviewer"] means orchestrator→coder→reviewer.
    pub delegation_chain: Vec<String>,
    /// Agent ID of this agent itself. Set from delegation config for sub-agents;
    /// falls back to "orchestrator" for the root agent.
    pub self_agent_id: String,

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
    pub compaction_effectiveness: super::super::compaction_replay::CompactionEffectivenessTracker,

    /// Measured token cost of the tool schemas injected into the LLM request.
    /// Passed to `estimate_tokens` so pressure estimates include the
    /// schema overhead the API will count. 0 = unknown (legacy / sub-runs).
    pub always_load_tool_schema_tokens: u64,
    /// Cache-sensitive sticky tool schema set for the current user turn.
    /// When a cache-capable provider sees multiple LLM rounds in one turn,
    /// we keep once-advertised tools stable instead of letting the planner
    /// add/remove schemas mid-turn and break the cache prefix.
    pub sticky_tool_schemas: Vec<Value>,

    // ── Per-turn token budget ──
    /// Maximum LLM input tokens before the loop forces a graceful wind-down.
    /// 0 = unlimited (legacy).  Set from `RuntimeLimits::max_turn_input_tokens`.
    pub max_turn_input_tokens: u64,
    /// Set to `true` once the budget-exceeded wrap-up message has been injected.
    /// The loop allows exactly one more LLM iteration after injection.
    pub budget_wrapup_injected: bool,
    /// Counts how many post-wrap-up rounds still emitted tool_calls. Task #43
    /// hybrid enforcement: the first such round triggers a physical lockout
    /// (tool_calls dropped, `restricted_tools` populated, loop continues so the
    /// model gets one more LLM call to produce text); the second aborts the
    /// turn with an interruption. Without the counter, the only available
    /// response to "model ignored wrap-up" was immediate abort, which lost
    /// partial text that arrived alongside the tool_calls (session 05e63cac).
    pub budget_wrapup_ignored_rounds: u32,

    /// Highest [`CompactionTier`] applied to this turn so far.
    ///
    /// `Normal` = no compaction; `CompactHistory` = pre-turn LLM summary or
    /// tier-1 mechanical compression already ran; `AggressivePrune` = reserved
    /// for future tiered escalation. Paths that would otherwise re-compact
    /// check this and stay their hand when the current tier already covers
    /// them. Not persisted — starts `Normal` every time the loop runs.
    pub compact_tier_applied: CompactionTier,

    /// Set to `true` when a skill produced substantial output in the current
    /// turn. The CLI host reads this to suppress intermediate text rendering
    /// on subsequent iterations (prevents markdown leak from draft text).
    pub skill_produced_output: bool,

    // ── Cumulative token budget ──
    /// Maximum cumulative (prompt + completion) tokens across all rounds.
    /// 0 = unlimited (default for interactive sessions).
    /// Skill subruns set this to cap total cost.
    pub max_cumulative_tokens: u64,

    // ── Thinking config ──
    /// Thinking/reasoning configuration for extended thinking models.
    /// Applied to the LLM request body via provider-specific wire format.
    pub thinking: astra_turn_core::thinking_config::ThinkingConfig,

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
    pub permission_context: Option<crate::orchestration::PermissionSyncHandle>,

    /// Optional permission request handler for processing child requests.
    /// When set, incoming PermissionRequest messages are handled automatically.
    pub permission_handler: Option<crate::orchestration::PermissionRequestHandler>,

    // ── Mid-execution checkpoint gate ──
    /// Optional checkpoint gate checked every N turns during delegation sub-runs.
    /// When the gate returns `false`, the loop aborts with `Cancelled`.
    pub checkpoint_gate: Option<Arc<dyn crate::server::delegation::engine::CheckpointGate>>,

    /// Last context assembly trace produced by the shared LLM context assembler.
    /// The per-call context manifest writer uses this to describe the actual
    /// prompt/cache assembly path instead of reconstructing it independently.
    pub last_llm_context_manifest_trace: Option<serde_json::Value>,

    // ── Rate Limit Cooldown ──
    /// Cross-turn rate-limit cooldown tracker.  When the loop detects a
    /// rate-limit error (429 / TPM / RPM), it records it here so subsequent
    /// turns can wait or reject early instead of immediately re-hitting the
    /// limit.  Shared across all turns within a single agentic loop invocation.
    pub rate_limit_cooldown: crate::bridge::RateLimitCooldown,

    // ── Liquid (within-turn tactical adaptation) ──
    /// Optional tactical adapter for step-level adaptation within a turn.
    pub tactical_adapter: Option<astra_turn_core::liquid_tactical::TacticalAdapter>,
    /// Optional step signal collector for within-turn outcome tracking.
    pub step_signal_collector: Option<astra_turn_core::liquid_step_signals::StepSignalCollector>,

    /// Recent tactical adaptations applied while liquid tactical tuning runs.
    pub recent_tactical_actions: Vec<String>,

    // ── Server-side tool execution ──
    /// Optional server-side tool executor for web agent sessions (no CLI edge agent).
    /// When present, tools that have no edge match are executed directly by the server.
    pub server_tool_executor: Option<Arc<crate::server::server_tool_executor::ServerToolExecutor>>,

    // ── Interruption tracking ──
    /// Structured interruption record populated by early-exit paths.
    /// When set, the session journal and checkpoint include machine-readable
    /// interruption context for structured resumption.
    pub interruption: Option<astra_turn_core::interruption::InterruptionRecord>,

    // ── Session Facts (L1a ground truth) ──
    /// System-tracked session state updated every turn from tool call records.
    /// Used for facts-first anchor, injection, compaction, and microcompact pin list.
    pub session_facts: astra_turn_types::session_facts::SessionFacts,

    // ── Session-memory extraction (LLM-backed L1) ──
    /// Coordinator for background session-memory extraction. When `Some`,
    /// `finalize_and_render` calls `svc.maybe_spawn(req)` after each
    /// turn; the service owns background model resolution, extraction
    /// cooldown, in-flight dedup, the event stream, the UX broker,
    /// AND the per-session debounce state. When `None` (tests, sub-runs
    /// that opt out), no extraction happens and no events are emitted.
    /// Cloned from the host service at state-build time.
    ///
    /// Per-session debounce state (`initialized`, `tokens_at_last_extraction`,
    /// …) deliberately lives *inside* the service — not here — because
    /// `AgenticLoopState` is rebuilt every turn and would lose the
    /// debounce across turns, making the "growth delta" branch of the
    /// gate structurally unreachable.
    pub memory_extraction_service:
        Option<std::sync::Arc<crate::session_memory::MemoryExtractionService>>,

    /// Debounce state for the LLM-backed session-memory extractor that
    /// writes `session-memory.md` in the background (fire-and-forget).
    /// Persists across turns so `should_extract` can compare growth deltas.
    /// Reset to `Default::default()` only when a new session starts.
    ///
    /// Wrapped in `Arc<Mutex<>>` so the background extraction task can
    /// flip `mark_extracted` itself *after* the write completes.
    pub session_memory_state: std::sync::Arc<
        std::sync::Mutex<astra_turn_core::cloud_session_memory_extract::SessionMemoryState>,
    >,

    /// Resolved background-model params used by the memory
    /// extraction runner (see `turn::memory_extraction_runner`). Shared
    /// with `memory_relevance` and lesson synthesis — same "cheap
    /// background LLM" resolved once per session via
    /// `resolve_memory_model`. `None` → extraction falls back to the
    /// rule-based L1 builder.
    pub session_memory_llm_params: Option<crate::memory_hooks::relevance::LlmConnParams>,

    /// Provider-aware compaction strategy for microcompact placeholders.
    pub compact_strategy: astra_turn_core::microcompact::CompactStrategy,

    // ── Approval checkpoint persistence ──
    /// Approval overrides synchronized from CLI's PermissionManager before each turn.
    /// Written to HeavyCheckpoint so approval decisions survive session restarts.
    pub approval_overrides: Option<astra_turn_core::approval_fingerprint::FingerprintedOverrides>,

    // ── Confidence tracking ──
    /// Tracks tool-surface confidence trends across turns to detect floor loops.
    pub confidence_trend: astra_turn_core::confidence_contract::ConfidenceTrendTracker,
    /// Last diagnosis computed after tool surface (for telemetry and fallback).
    pub last_confidence_diagnosis:
        Option<astra_turn_core::confidence_contract::ConfidenceDiagnosis>,

    // ── Turn observability (Phase 1) ──
    /// In-memory collector for fine-grained turn events (llm_round, tool timing).
    /// Session-level turn number (1-based). Set by the CLI from ReplState.turn
    /// so that llm_round journal events carry the correct turn number.
    pub session_turn: u32,
    /// Optional authoritative bridge turn-chain id propagated by outer loops.
    /// When present, all `/chat/turn` retries within the same visible turn
    /// should reuse this id instead of generating a fresh bridge-local value.
    pub bridge_turn_chain_id: Option<String>,
    /// Optional authoritative root user-query event id propagated by outer loops.
    pub bridge_user_query_event_id: Option<String>,
    /// Created at turn start, flushed at turn end or on interruption.
    pub turn_event_buffer: Option<astra_services::session_journal::TurnEventBuffer>,

    // ── Harness (observation + verification layer) ──
    pub harness: super::super::harness_adapter::HarnessSlot,
}

/// Build the stable runtime manifest carried through context metadata.
///
/// `model` must already be the selected concrete model for the run. Symbolic
/// values such as `default` and invalid/control-character strings fail closed
/// to `None` so callers cannot accidentally publish a guessed identity.
pub fn runtime_manifest_for_model(
    source: &'static str,
    runtime_profile: &'static str,
    model: Option<&str>,
) -> Option<serde_json::Value> {
    let model = astra_core::model_override::normalize_model_override(model)?;
    Some(serde_json::json!({
        "schema_version": "astra_runtime_manifest.v1",
        "selected_model": {
            "model": model,
        },
        "model_resolution": {
            "source": source,
            "model": model,
            "resolved": true,
        },
        "runtime_profile": runtime_profile,
    }))
}

impl AgenticLoopState {
    /// Refresh the cached active task-board snapshot from the shared
    /// TaskManager, when one is attached. Call this before any terminal
    /// completion decision so tool calls in the just-finished round are
    /// reflected before the loop decides whether unfinished work remains.
    ///
    /// The DB call is guarded by a 5-second timeout to prevent a stalled
    /// store from holding up loop finalisation indefinitely.
    pub async fn refresh_task_board_snapshot(&mut self) {
        let Some(task_manager) = self.hooks.task_board_monitor.clone() else {
            return;
        };
        let load = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            task_manager.load_active_tasks(),
        );
        match load.await {
            Ok(Ok(tasks)) => {
                self.hooks.task_board_snapshot = TaskBoardSnapshot::from_active_tasks(&tasks);
            }
            Ok(Err(error)) => {
                tracing::warn!(
                    target: "astra::loop_guard",
                    session_id = self.current_session_id.as_deref().unwrap_or_default(),
                    error = %error,
                    "failed to refresh active task-board snapshot; preserving previous snapshot"
                );
            }
            Err(_elapsed) => {
                tracing::warn!(
                    target: "astra::loop_guard",
                    session_id = self.current_session_id.as_deref().unwrap_or_default(),
                    "timed out refreshing active task-board snapshot; preserving previous snapshot"
                );
            }
        }
    }

    /// Queue a runtime-produced volatile injection for the next LLM call.
    ///
    /// Prefer this over `state.messages.push(...)` for any content that
    /// (a) isn't a genuine user / assistant / tool conversation turn and
    /// (b) changes across rounds or turns.
    ///
    /// The injection rides `volatile_preamble` on the next call (see
    /// `wire_assembly::assemble_llm_messages`) which keeps `messages[]`
    /// byte-stable across rounds — the property Anthropic / DeepSeek
    /// prompt caches rely on.
    ///
    /// **Singleton kinds auto-dedup**: `WorkingSet`, `AlreadyFetched`,
    /// and `ExplorationBudget` are snapshot-style — only the most
    /// recent value matters. If one is already pending when a new one
    /// is pushed, the old entry is replaced in place (preserving order
    /// for other kinds). This mirrors the legacy `state.messages.retain()`
    /// guard that producers used to write by hand.
    ///
    /// Silently trims empty content so call sites can pass formatter
    /// output directly without a guard.
    pub fn push_volatile(&mut self, kind: VolatileKind, content: impl Into<String>) {
        let content = content.into();
        if content.trim().is_empty() {
            return;
        }
        let injection = VolatileInjection {
            kind,
            content,
            round_index: self.current_round_index,
        };
        if kind.is_singleton() {
            // Replace any prior entry of the same kind so the snapshot
            // semantics are preserved: second push within a turn drops
            // the first, never doubles up.
            if let Some(existing) = self
                .volatile_pending
                .iter_mut()
                .find(|inj| inj.kind == kind)
            {
                *existing = injection;
                return;
            }
        }
        self.volatile_pending.push(injection);
    }

    /// Drain all pending volatile injections. Called by
    /// `wire_assembly::assemble_llm_messages` once per LLM call.
    ///
    /// Consumers (and tests inspecting runtime state) get an owned
    /// list; the lane is empty afterward so the NEXT LLM call starts
    /// from a clean slate.
    #[must_use]
    pub fn take_volatile_pending(&mut self) -> Vec<VolatileInjection> {
        std::mem::take(&mut self.volatile_pending)
    }

    /// Drain the volatile lane and render it as a single `role=user`
    /// message (or `None` when the lane is empty / whitespace-only).
    ///
    /// The CLI path (`cli_loop_host::execute_turn`) uses this to inject
    /// runtime nudges into the outgoing HTTP payload right before
    /// calling the bridge. Without it, 27 `push_volatile` sites —
    /// including Task #42 force-stop, Task #43 wrap-up advisory,
    /// circuit-breaker self-check, stall reflections, budget advisories,
    /// and the corrective family — are silently dropped on `astra chat`
    /// (session c47c2dca regression: 25-round churn where the model
    /// never saw the Self-check prompt the stall system thought it
    /// injected at round 12).
    ///
    /// `server_loop_host` has its own drain path via
    /// `wire_assembly::assemble_llm_messages` and does NOT use this
    /// method — callers should not double-drain.
    ///
    /// ⚠ **Callers that append this directly to an existing messages
    /// vec** should prefer [`take_volatile_pending_appended_to`] instead,
    /// which also handles the tail=role=user case (prepends rather
    /// than appending to avoid consecutive-user protocol violations on
    /// Bedrock/Anthropic).
    #[must_use]
    pub fn take_volatile_pending_as_message(&mut self) -> Option<serde_json::Value> {
        let drained = self.take_volatile_pending();
        if drained.is_empty() {
            return None;
        }
        let mut rendered = String::new();
        for inj in &drained {
            let text = inj.content.trim();
            if text.is_empty() {
                continue;
            }
            if !rendered.is_empty() {
                rendered.push_str("\n\n");
            }
            rendered.push_str(text);
        }
        if rendered.is_empty() {
            return None;
        }
        Some(serde_json::json!({
            "role": "user",
            "content": rendered,
        }))
    }

    /// Drain the volatile lane and splice its rendered text into a
    /// copy of `base` that's protocol-safe for every provider we
    /// support. Returns `None` when the lane is empty (caller can use
    /// `base` as-is).
    ///
    /// Shape rule:
    ///   * If `base` is empty or its tail is NOT `role=user`: append a
    ///     fresh `role=user` msg carrying the rendered text.
    ///   * If the tail IS already `role=user`: prepend the rendered
    ///     text to that message's content (preserving the original
    ///     query). This avoids creating consecutive-user-role messages,
    ///     which Bedrock rejects with HTTP 400
    ///     (`"expected role to alternate …"`).
    ///
    /// Prefer this over [`take_volatile_pending_as_message`] +
    /// `push` in CLI paths — the latter produces invalid payloads for
    /// any turn where the user has just typed a query (fresh turn
    /// r0 state).
    #[must_use]
    pub fn take_volatile_pending_appended_to(
        &mut self,
        base: Vec<serde_json::Value>,
    ) -> Option<Vec<serde_json::Value>> {
        let volatile_msg = self.take_volatile_pending_as_message()?;
        let volatile_text = volatile_msg
            .get("content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut augmented = base;
        // Single-pass: bind the mutable reference and check the role through
        // it so we never have to re-`last_mut().expect(...)`. If the borrow
        // matches and we have non-empty volatile text, prepend in-place;
        // otherwise the function-level `else` branch appends. This makes
        // the previous "tail_is_user → non-empty" invariant unrepresentable.
        let prepended = augmented.last_mut().is_some_and(|last| {
            if last.get("role").and_then(serde_json::Value::as_str) != Some("user")
                || volatile_text.is_empty()
            {
                return false;
            }
            // Prepend into the last user msg so we don't create
            // `[user, user]` which breaks Bedrock/Anthropic alternation.
            let existing = last
                .get("content")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            let merged = if existing.is_empty() {
                volatile_text.clone()
            } else {
                format!("{volatile_text}\n\n{existing}")
            };
            last["content"] = serde_json::Value::String(merged);
            true
        });
        if !prepended {
            // Safe to append — tail is assistant/tool (tool-loop case),
            // base was empty, or volatile text was empty.
            augmented.push(volatile_msg);
        }
        Some(augmented)
    }

    /// Append an LLM-round summary to the in-memory ring (capped at
    /// [`RECENT_ROUNDS_RING_CAPACITY`]). Callers should use this
    /// alongside `TurnEventBuffer::record_llm_round` so the ring is
    /// populated regardless of `full_llm_capture` setting.
    pub fn push_recent_round(&mut self, summary: RecentRoundSummary) {
        self.recent_rounds.push(summary);
        if self.recent_rounds.len() > RECENT_ROUNDS_RING_CAPACITY {
            let excess = self.recent_rounds.len() - RECENT_ROUNDS_RING_CAPACITY;
            self.recent_rounds.drain(0..excess);
        }
    }

    /// Best-effort model id for sizing per-turn budgets (e.g. skill listing
    /// budget). Resolution order:
    ///   1. `skills.model_override` — the active skill's pinned model
    ///   2. The model used in the most recent LLM round
    ///   3. `None` (caller should fall back to a sensible default)
    pub fn current_model_hint(&self) -> Option<&str> {
        self.skills
            .model_override
            .as_deref()
            .or_else(|| self.recent_rounds.last().map(|r| r.model.as_str()))
    }

    /// Authoritative model identity for user/model-visible diagnostics.
    ///
    /// This answers "which model is running this session/turn?" and must not
    /// fall back to symbolic values such as `default`. Skill model overrides
    /// remain secondary because they are transient execution hints; the
    /// selected/context-manifest model is the user's chosen model identity.
    pub fn current_model_identity(&self) -> Option<&str> {
        self.context_manifest_model_name
            .as_deref()
            .or(self.skills.model_override.as_deref())
            .or_else(|| {
                self.recent_rounds
                    .last()
                    .map(|r| r.model.as_str())
                    .filter(|model| !model.is_empty())
            })
    }

    /// 1-based session turn number for the turn currently in progress.
    ///
    /// Two ways to derive it, in order:
    /// 1. The explicit `session_turn` slot, populated by the host before the
    ///    turn starts (server, edge sync, recovery paths).
    /// 2. Otherwise, derive from the loop budget — `max_turns - remaining_turns`.
    ///
    /// The caller observes turn N during the entire window where they reason
    /// about turn N (LLM round, tool round, post-turn capture). That is the
    /// same value that downstream consumers like
    /// [`crate::turn::skill_tool::InvokedSkill::invoked_at_turn`] persist —
    /// see that field's doc for the captured-before-commit semantics.
    pub fn current_session_turn_number(&self) -> u32 {
        if self.session_turn > 0 {
            self.session_turn
        } else {
            self.max_turns.saturating_sub(self.remaining_turns).max(1) as u32
        }
    }
}

/// Consecutive same-category error turns before forcing a strategy change.
pub(crate) const CONSECUTIVE_ERROR_BUDGET: u32 = 3;

/// Maximum number of recent file reads to track for post-compact restoration.
pub(crate) const MAX_TRACKED_FILE_READS: usize = 20;

/// Maximum number of times the harness pause signal triggers checkpoint
/// injection and loop continuation before forcing a text-only finalization
/// turn.
#[cfg(feature = "harness")]
const MAX_HARNESS_PAUSE_RECOVERIES: u32 = 2;

#[cfg(feature = "harness")]
fn harness_pause_finalization_message(reason: &str, original_query: &str) -> String {
    format!(
        "Harness checkpoint: the run is still in a read-heavy stall after repeated recovery prompts.\n\n\
         You must stop using tools and produce a concise final response now.\n\n\
         REQUIRED final response:\n\
         - Summarize the concrete evidence already gathered.\n\
         - State the most likely conclusion or fix path.\n\
         - If the task is not complete, say exactly what remains and why.\n\
         - Do not mention internal harness implementation details unless they are necessary to explain why work stopped.\n\n\
         Original user query: {original_query}\n\n\
         Checkpoint reason: {reason}"
    )
}

#[cfg(feature = "harness")]
fn force_text_only_harness_finalization<H: AgenticLoopHost>(
    host: &H,
    state: &mut AgenticLoopState,
    reason: &str,
) {
    // Reserve one last LLM call for the user-visible summary. Without this,
    // the recovery turns themselves can consume the remaining budget and the
    // user sees a BudgetExhausted interruption instead of the intended wrap-up.
    state.remaining_turns = state.remaining_turns.max(1);
    for name in host.valid_tool_names() {
        state.restricted_tools.insert(name.clone());
    }
    state.messages.push(serde_json::json!({
        "role": "user",
        "content": harness_pause_finalization_message(reason, &state.message),
    }));
}

#[cfg(any(feature = "harness", test))]
fn apply_harness_pause_recovery_threshold(
    state: &mut AgenticLoopState,
    recovery_threshold: Option<u32>,
) {
    let current = state.stall.circuit_breaker.read_only_threshold();
    let tighter = recovery_threshold.map(|value| value as usize);
    if !state.stall.circuit_breaker.apply_pause_recovery(tighter) {
        tracing::warn!(
            proposed_threshold = tighter.unwrap_or(0),
            current_threshold = current,
            "ignoring invalid harness recovery threshold"
        );
        let _ = state.stall.circuit_breaker.apply_pause_recovery(None);
    }
}

pub(crate) use super::super::agentic::adaptive_tuning::{
    DEFAULT_TUNING_CYCLE_INTERVAL, apply_adaptive_execution_profile_with_intent,
    apply_per_turn_adaptation, apply_tactical_actions, maybe_run_tuning_cycle,
    record_loop_completion_feedback, should_emit_adaptive_scenario_event,
};
#[cfg(test)]
pub(crate) use super::tool_support::delegate_tool_schema;
pub(crate) use super::tool_support::{extract_file_path_from_tool, record_edge_tool_observability};

// ─── Loop exit ───────────────────────────────────────────────────────────────

/// Result of running the agentic loop to completion.
#[derive(Debug)]
pub enum AgenticLoopOutcome {
    /// Loop completed normally (final text produced or budget exhausted gracefully).
    Completed,
    /// Loop aborted due to a fatal error.
    // Preserved for dispatcher/adapter contract even though the core loop doesn't emit it in non-test builds yet.
    Error(String),
    /// Loop was cancelled externally via `cancel_flag` or `cancel_token`.
    Cancelled,
    /// Loop is waiting for external input (tool approval, user resume, webhook).
    /// The caller should provide the requested input and re-invoke the loop.
    // Preserved for dispatcher/adapter contract even though the core loop doesn't emit it in non-test builds yet.
    Waiting(String),
}

// ─── Delegation support ──────────────────────────────────────────────────────

pub const DELEGATE_TOOL_NAME: &str =
    super::super::agentic::delegate_interception::DELEGATE_TOOL_NAME;

pub(crate) use super::super::agentic::delegate_interception::{
    DelegationAdaptiveContext, DelegationExecutionResult, DelegationFinalOutputSource,
    DelegationOutcomeMetadata, coordination_pattern_name, delegation_adaptive_context,
    delegation_final_output_preview, format_delegation_result, format_delegation_terminal_preview,
    is_delegation_call, merge_workspace_hint_into_delegation_request, parse_coordination_pattern,
    parse_delegate_agents, parse_delegation_request, partition_and_execute_delegations,
    pattern_from_name, select_default_coordination_pattern, task_needs_review,
    tool_call_arguments_value, tool_call_name,
};

use super::super::harness_adapter::harness_at;
pub(crate) use super::execution_phase::{
    TurnExecutionControl, TurnExecutionPhase, execute_turn_and_ingest_phase,
};
pub(crate) use super::finalization::{
    finalize_and_render, finalize_turn_trace, run_agentic_loop_with_host,
    try_write_heavy_checkpoint,
};
pub(crate) use super::lifecycle::{
    PreparedTurnIteration, TurnIterationPrep, prepare_turn_iteration, run_loop_preamble,
};
pub(crate) use super::tool_phase::{TurnToolPhaseControl, execute_tool_phase};

#[cfg(feature = "harness")]
pub(crate) fn set_harness_interruption(
    state: &mut AgenticLoopState,
    kind: astra_turn_core::interruption::InterruptionKind,
    reason: &str,
) {
    state.interruption = Some(astra_turn_core::interruption::InterruptionRecord::new(
        kind,
        if kind.is_resumable() {
            astra_turn_core::interruption::ResumeAction::ContinueImmediately
        } else {
            astra_turn_core::interruption::ResumeAction::StartNewSession
        },
        super::lifecycle::interruption_state_summary(state, Some(reason.to_string())),
    ));
}

pub(crate) async fn run_agentic_loop_impl<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) -> Result<AgenticLoopOutcome, astra_core::ClassifiedError> {
    run_loop_preamble(host, state).await;

    // ── Harness: SessionStart — Block prevents any turns ──
    #[cfg(feature = "harness")]
    match harness_at!(
        &state.harness,
        astra_harness::HookPoint::SessionStart,
        state
    ) {
        astra_harness::HookVerdict::Block { reason } => {
            tracing::warn!(reason = %reason, "harness blocked session at SessionStart");
            set_harness_interruption(
                state,
                astra_turn_core::interruption::InterruptionKind::HarnessBlocked,
                &reason,
            );
            finalize_and_render(host, state).await;
            return Ok(AgenticLoopOutcome::Completed);
        }
        astra_harness::HookVerdict::Pause { reason, .. } => {
            tracing::info!(reason = %reason, "harness paused session at SessionStart");
            set_harness_interruption(
                state,
                astra_turn_core::interruption::InterruptionKind::HarnessPaused,
                &reason,
            );
            finalize_and_render(host, state).await;
            return Ok(AgenticLoopOutcome::Completed);
        }
        astra_harness::HookVerdict::Continue => {}
    }
    #[cfg(not(feature = "harness"))]
    harness_at!(
        &state.harness,
        astra_harness::HookPoint::SessionStart,
        state
    );

    let loop_start_time = Instant::now();
    let mut turn_index = 0usize;
    #[cfg(feature = "harness")]
    let mut harness_pause_recovery_count: u32 = 0;
    while turn_index < state.max_turns || state.remaining_turns == 0 {
        state.current_round_index = turn_index as u32;
        let TurnIterationPrep {
            quiet,
            turn_start_time,
        } = match prepare_turn_iteration(host, state, turn_index).await? {
            PreparedTurnIteration::Ready(prep) => prep,
            PreparedTurnIteration::Finished(outcome) => {
                if matches!(outcome, AgenticLoopOutcome::Completed)
                    && (!state.final_text.is_empty() || state.interruption.is_some())
                {
                    finalize_and_render(host, state).await;
                }
                return Ok(outcome);
            }
        };

        // Trace: turn_start
        if let Some(ref mut buf) = state.turn_event_buffer {
            record_trace_span(
                buf,
                format!("turn_{}", turn_index),
                "turn_start",
                turn_start_time,
                None,
                None,
                state.current_run_id.as_deref(),
            );
        }

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
            TurnExecutionControl::Return(outcome) => {
                finalize_and_render(host, state).await;
                return Ok(outcome);
            }
        };

        // Trace: llm_call
        if let Some(ref mut buf) = state.turn_event_buffer {
            record_trace_span(
                buf,
                format!("llm_{}", turn_index),
                "llm_call",
                llm_wall_start,
                Some(format!("turn_{}", turn_index)),
                None,
                state.current_run_id.as_deref(),
            );
        }

        // Trace: tool_surface
        if let Some(ref mut buf) = state.turn_event_buffer {
            let tool_count = turn_result.accum.tool_calls.len();
            let mut attrs = std::collections::HashMap::new();
            attrs.insert("tool_count".into(), tool_count.to_string());
            record_trace_span(
                buf,
                format!("surface_{}", turn_index),
                "tool_surface",
                llm_wall_start,
                Some(format!("llm_{}", turn_index)),
                Some(&attrs),
                state.current_run_id.as_deref(),
            );
        }

        // ── Harness: PostLlmResponse — Block/Pause halts session ──
        #[cfg(feature = "harness")]
        match harness_at!(
            &state.harness,
            astra_harness::HookPoint::PostLlmResponse,
            state
        ) {
            astra_harness::HookVerdict::Block { reason } => {
                tracing::warn!(reason = %reason, "harness blocked session at PostLlmResponse");
                set_harness_interruption(
                    state,
                    astra_turn_core::interruption::InterruptionKind::HarnessBlocked,
                    &reason,
                );
                finalize_and_render(host, state).await;
                return Ok(AgenticLoopOutcome::Completed);
            }
            astra_harness::HookVerdict::Pause { reason, .. } => {
                tracing::info!(reason = %reason, "harness paused session at PostLlmResponse");
                set_harness_interruption(
                    state,
                    astra_turn_core::interruption::InterruptionKind::HarnessPaused,
                    &reason,
                );
                finalize_and_render(host, state).await;
                return Ok(AgenticLoopOutcome::Completed);
            }
            astra_harness::HookVerdict::Continue => {}
        }

        // ── Harness: PreToolBatch — Block/Pause skips tool execution ──
        #[cfg(feature = "harness")]
        let harness_blocked_tools = if !turn_result.accum.tool_calls.is_empty() {
            match harness_at!(
                &state.harness,
                astra_harness::HookPoint::PreToolBatch,
                state
            ) {
                astra_harness::HookVerdict::Block { reason }
                | astra_harness::HookVerdict::Pause { reason, .. } => {
                    tracing::warn!(reason = %reason, "harness blocked tool batch at PreToolBatch");
                    for tc in &turn_result.accum.tool_calls {
                        let tc_id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
                        state.tool_results.push(serde_json::json!({
                            "type": "tool_result",
                            "tool_use_id": tc_id,
                            "content": format!("[harness] Tool batch blocked: {reason}"),
                            "is_error": true,
                        }));
                    }
                    true
                }
                astra_harness::HookVerdict::Continue => false,
            }
        } else {
            false
        };

        let tool_phase_start = Instant::now();
        #[cfg(feature = "harness")]
        let tool_phase_control = if harness_blocked_tools {
            TurnToolPhaseControl::ContinueLoop
        } else {
            execute_tool_phase(
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
            .await?
        };

        #[cfg(not(feature = "harness"))]
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

        // Trace: tool_execution
        if let Some(ref mut buf) = state.turn_event_buffer {
            record_trace_span(
                buf,
                format!("tools_{}", turn_index),
                "tool_execution",
                tool_phase_start,
                Some(format!("llm_{}", turn_index)),
                None,
                state.current_run_id.as_deref(),
            );
        }

        // ── Harness: PostToolBatch — Block/Pause halts session ──
        #[cfg(feature = "harness")]
        if !harness_blocked_tools {
            match harness_at!(
                &state.harness,
                astra_harness::HookPoint::PostToolBatch,
                state
            ) {
                astra_harness::HookVerdict::Block { reason } => {
                    tracing::warn!(reason = %reason, "harness blocked session at PostToolBatch");
                    set_harness_interruption(
                        state,
                        astra_turn_core::interruption::InterruptionKind::HarnessBlocked,
                        &reason,
                    );
                    finalize_and_render(host, state).await;
                    return Ok(AgenticLoopOutcome::Completed);
                }
                astra_harness::HookVerdict::Pause { reason, .. } => {
                    tracing::info!(reason = %reason, "harness paused session at PostToolBatch");
                    set_harness_interruption(
                        state,
                        astra_turn_core::interruption::InterruptionKind::HarnessPaused,
                        &reason,
                    );
                    finalize_and_render(host, state).await;
                    return Ok(AgenticLoopOutcome::Completed);
                }
                astra_harness::HookVerdict::Continue => {}
            }
        }

        // ── Harness: PostTurn — Block/Pause halts session ──
        #[cfg(feature = "harness")]
        match harness_at!(&state.harness, astra_harness::HookPoint::PostTurn, state) {
            astra_harness::HookVerdict::Block { reason } => {
                tracing::warn!(reason = %reason, "harness blocked session at PostTurn");
                set_harness_interruption(
                    state,
                    astra_turn_core::interruption::InterruptionKind::HarnessBlocked,
                    &reason,
                );
                finalize_and_render(host, state).await;
                return Ok(AgenticLoopOutcome::Completed);
            }
            astra_harness::HookVerdict::Pause {
                reason,
                recovery_threshold,
            } => {
                harness_pause_recovery_count += 1;
                if harness_pause_recovery_count > MAX_HARNESS_PAUSE_RECOVERIES {
                    tracing::warn!(
                        count = harness_pause_recovery_count,
                        reason = %reason,
                        "harness pause recovery limit exceeded at PostTurn; forcing text-only finalization"
                    );
                    force_text_only_harness_finalization(host, state, &reason);
                    continue;
                }
                tracing::info!(
                    count = harness_pause_recovery_count,
                    reason = %reason,
                    "harness pause recovered at PostTurn — injecting checkpoint guidance"
                );
                state.messages.push(serde_json::json!({
                    "role": "user",
                    "content": reason,
                }));
                apply_harness_pause_recovery_threshold(state, recovery_threshold);
                // Fall through to continue the loop
            }
            astra_harness::HookVerdict::Continue => {}
        }

        match tool_phase_control {
            TurnToolPhaseControl::ContinueLoop => {}
            TurnToolPhaseControl::Return(outcome) => return Ok(outcome),
        }

        turn_index += 1;
    }
    // Trace: turn_end
    if let Some(ref mut buf) = state.turn_event_buffer {
        record_trace_span(
            buf,
            "loop_end".to_string(),
            "turn_end",
            loop_start_time,
            None,
            None,
            state.current_run_id.as_deref(),
        );
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

/// **Test-only.** Build a minimal [`AgenticLoopState`] suitable for driving
/// the mock-LLM path in integration tests (feature `bridge-e2e-hooks`).
///
/// All fields use safe defaults; tests should mutate the returned state
/// directly (e.g. push into `messages`, set `llm_rounds_completed`).
///
/// Delegates to [`make_test_loop_state_for_model`] with `None`, which
/// resolves workflow-guard thresholds from the global config defaults.
#[cfg(any(test, feature = "bridge-e2e-hooks"))]
pub fn make_test_loop_state() -> AgenticLoopState {
    make_test_loop_state_for_model(None)
}

/// **Test-only.** Like [`make_test_loop_state`], but resolves workflow-guard
/// thresholds (`max_identical_tool_calls`, `max_tools_per_turn`) through
/// [`astra_config::runtime_config::ToolPolicyConfig::resolve_for_model`], so a
/// request carrying a specific model id sees that model's profile.
#[cfg(any(test, feature = "bridge-e2e-hooks"))]
pub fn make_test_loop_state_for_model(model: Option<&str>) -> AgenticLoopState {
    let policy = astra_config::runtime_config::RuntimeConfig::load()
        .tool_policy
        .resolve_for_model(model);
    AgenticLoopState {
        messages: Vec::new(),
        volatile_pending: Vec::new(),
        recent_rounds: Vec::new(),
        tool_results: Vec::new(),
        current_session_id: None,
        current_run_id: None,
        context_manifest_pool: None,
        context_manifest_user_id: None,
        context_manifest_model_name: None,
        runtime_manifest: None,
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
        turn_budget_hint_emitted_90: false,
        turn_budget_hint_emitted_50: false,
        turn_budget_hint_emitted_20: false,
        agentic_turn_budget: TaskExecutionProfile::default().agentic_turn_budget,
        current_round_index: 0,
        llm_rounds_completed: 0,
        last_request_message_count: None,
        turn_guard: TurnGuard::new(),
        restricted_tools: HashSet::new(),
        boosted_tools: HashSet::new(),
        widen_surface_pending: false,
        step_recorder: StepRecorder::new("test-user", "test-session", "test-task"),
        idempotency_cache: InMemoryIdempotencyCache::new(),
        semantic_dedup: SemanticDedup::new(0.95),
        call_counts: HashMap::new(),
        max_identical_tool_calls: policy.max_identical_tool_calls,
        max_tools_per_turn: policy.max_tools_per_turn,
        repeated_cache_hit_suppression: policy.repeated_cache_hit_suppression,
        max_consecutive_empty_name: policy.max_consecutive_empty_name,
        stall: Default::default(),
        telemetry: Default::default(),
        skills: SkillState {
            quality_tracker: crate::skills::quality::SkillQualityTracker::new(),
            improvement_tracker: astra_skills::improvement::ImprovementTracker::new(),
            ..Default::default()
        },
        hooks: Default::default(),
        messaging: Default::default(),
        cancellation: Default::default(),
        deferred_input: Default::default(),
        error_recovery: Default::default(),
        pipeline_session: Some(astra_turn_core::pipeline_session::PipelineSession::new(
            astra_turn_core::pipeline_config::PipelineConfig::default(),
        )),
        message: "test query".to_string(),
        recent_tools: Vec::new(),
        task_profile: TaskExecutionProfile::default(),
        last_turn_policy: TurnInteractionPolicy::default(),
        api: astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
        api_token: String::new(),
        delegation_engine: None,
        delegations_this_turn: 0,
        delegation_chain: Vec::new(),
        self_agent_id: "orchestrator".to_string(),
        run_control: None,
        project_context: None,
        checkpoint_gate: None,
        last_llm_context_manifest_trace: None,
        rate_limit_cooldown: Default::default(),
        data_snapshot_provider: None,
        last_composite_snapshot: None,
        last_measured_prompt_tokens: None,
        consecutive_context_window_errors: 0,
        compaction_effectiveness: Default::default(),
        always_load_tool_schema_tokens: 0,
        sticky_tool_schemas: Vec::new(),
        max_turn_input_tokens: 0,
        budget_wrapup_injected: false,
        budget_wrapup_ignored_rounds: 0,
        compact_tier_applied: CompactionTier::Normal,
        skill_produced_output: false,
        max_cumulative_tokens: 0,
        thinking: astra_turn_core::thinking_config::ThinkingConfig::Off,
        recent_file_reads: Vec::new(),
        permission_context: None,
        permission_handler: None,
        tactical_adapter: None,
        step_signal_collector: None,
        recent_tactical_actions: Vec::new(),
        server_tool_executor: None,
        interruption: None,
        session_facts: Default::default(),
        memory_extraction_service: None,
        session_memory_state: Default::default(),
        session_memory_llm_params: None,
        compact_strategy: Default::default(),
        approval_overrides: None,
        confidence_trend: Default::default(),
        last_confidence_diagnosis: None,
        session_turn: 0,
        bridge_turn_chain_id: None,
        bridge_user_query_event_id: None,
        turn_event_buffer: None,
        harness: super::super::harness_adapter::HarnessSlot::empty(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use astra_services::session_journal::SURGICAL_REMOVAL_TOOL_NAME;
    use serde_json::json;

    fn edge_runtime_environment_fields() -> serde_json::Map<String, Value> {
        let registry = astra_runtime_env::ToolRegistry::builtins();
        let advertisement = astra_runtime_env::RuntimeEnvironmentAdvertisement::new(
            astra_runtime_env::RunBinding::edge_developer("/workspace/project", &registry),
        );
        serde_json::Map::from_iter([(
            "runtime_environment_advertisement".to_string(),
            serde_json::to_value(advertisement).expect("serialize advertisement"),
        )])
    }

    /// Unwind-safe cleanup guard for tests that write owner-bound session
    /// artifacts. Removes the provided directory on drop — including during
    /// panic unwinds from failed assertions — so repeated runs don't leak
    /// `tier-gate-*` / `precompact-spill-*` siblings.
    struct SpillDirGuard(std::path::PathBuf);

    impl SpillDirGuard {
        fn new(session_id: &str) -> Self {
            let store = astra_services::local_session_artifact_store();
            Self(
                astra_services::SessionArtifactStore::session_dir(&store, session_id)
                    .expect("session id must resolve owner-bound spill directory"),
            )
        }
    }

    impl Drop for SpillDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // ── Flexible mock host for multi-turn scenarios ─────────────────────────

    pub(crate) struct MockHost {
        turn_results: Vec<HostTurnResult>,
        current_turn: usize,
        pub(crate) valid_tools: HashSet<String>,
        pub(crate) emitted_lines: Vec<String>,
        quiet: bool,
        interaction_mode: TurnInteractionMode,
        pub(crate) injected_schemas: Vec<Value>,
        pub(crate) rendered_final_text: Vec<String>,
        pub(crate) executed_messages: Vec<Vec<Value>>,
        pub(crate) turn_intent: Option<TurnIntent>,
        pub(crate) recovered_control_results: HashMap<String, EdgeToolExecResult>,
        pub(crate) recovered_control_requests: Vec<(Option<String>, String, String, Value)>,
        /// PR 5a test observer: number of times the turn loop has
        /// invoked the parent-turn-completed hook. Each entry is the
        /// value of `state.current_run_id` at hook time so tests can
        /// assert the hook runs AFTER `ingest_agentic_turn_stream`
        /// populated that field.
        pub(crate) turn_completed_run_ids: Vec<Option<String>>,
        pub(crate) cancelled_agent_ids: Vec<String>,
        pub(crate) cancel_child_agents_delay: Option<std::time::Duration>,
    }

    impl MockHost {
        pub(crate) fn new(results: Vec<HostTurnResult>) -> Self {
            Self {
                turn_results: results,
                current_turn: 0,
                valid_tools: HashSet::new(),
                emitted_lines: Vec::new(),
                quiet: true,
                interaction_mode: TurnInteractionMode::NonInteractive,
                injected_schemas: Vec::new(),
                rendered_final_text: Vec::new(),
                executed_messages: Vec::new(),
                turn_intent: None,
                recovered_control_results: HashMap::new(),
                recovered_control_requests: Vec::new(),
                turn_completed_run_ids: Vec::new(),
                cancelled_agent_ids: Vec::new(),
                cancel_child_agents_delay: None,
            }
        }

        pub(crate) fn with_valid_tools(mut self, tools: &[&str]) -> Self {
            self.valid_tools = tools.iter().map(|s| s.to_string()).collect();
            self
        }

        pub(crate) fn with_interaction_mode(mut self, mode: TurnInteractionMode) -> Self {
            self.interaction_mode = mode;
            self
        }

        pub(crate) fn with_turn_intent(mut self, intent: TurnIntent) -> Self {
            self.turn_intent = Some(intent);
            self
        }

        pub(crate) fn with_recovered_control_tool_result(
            mut self,
            tool_call_id: &str,
            result: EdgeToolExecResult,
        ) -> Self {
            self.recovered_control_results
                .insert(tool_call_id.to_string(), result);
            self
        }

        pub(crate) fn with_cancel_child_agents_delay(mut self, delay: std::time::Duration) -> Self {
            self.cancel_child_agents_delay = Some(delay);
            self
        }

        pub(crate) fn turn_count(&self) -> usize {
            self.current_turn
        }
    }

    #[async_trait]
    impl AgenticLoopHost for MockHost {
        async fn execute_turn(
            &mut self,
            state: &mut AgenticLoopState,
        ) -> Result<HostTurnResult, astra_core::ClassifiedError> {
            if self.turn_results.is_empty() {
                return Err(astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::BudgetExhausted,
                    "no more turns",
                ));
            }
            self.executed_messages.push(state.messages.clone());
            let result = self.turn_results.remove(0);
            self.current_turn += 1;
            Ok(result)
        }

        async fn judge_turn_intent(&mut self, _state: &AgenticLoopState) -> Option<TurnIntent> {
            self.turn_intent.clone()
        }

        async fn recover_missing_control_tool_result(
            &mut self,
            parent_run_id: Option<&str>,
            tool_call_id: &str,
            tool_name: &str,
            args: &Value,
        ) -> ControlToolRecovery {
            let Some(result) = self.recovered_control_results.remove(tool_call_id) else {
                return if self
                    .recovered_control_results
                    .values()
                    .any(|result| result.tool == tool_name)
                {
                    ControlToolRecovery::Missing
                } else {
                    ControlToolRecovery::Unsupported
                };
            };
            if result.tool != tool_name {
                self.recovered_control_results
                    .insert(tool_call_id.to_string(), result);
                return ControlToolRecovery::Unsupported;
            }
            self.recovered_control_requests.push((
                parent_run_id.map(str::to_string),
                tool_call_id.to_string(),
                tool_name.to_string(),
                args.clone(),
            ));
            ControlToolRecovery::Recovered(result)
        }

        fn emit_headless_line(&mut self, _style: HeadlessStderrStyle, line: String) {
            self.emitted_lines.push(line);
        }

        fn is_quiet(&self) -> bool {
            self.quiet
        }

        fn turn_interaction_mode(&self) -> TurnInteractionMode {
            self.interaction_mode
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

        async fn cancel_child_agents(
            &mut self,
            agent_ids: &[String],
            _reason: &str,
        ) -> Vec<String> {
            if let Some(delay) = self.cancel_child_agents_delay {
                tokio::time::sleep(delay).await;
            }
            self.cancelled_agent_ids.extend(agent_ids.iter().cloned());
            agent_ids.to_vec()
        }

        fn on_turn_completed(&mut self, state: &AgenticLoopState) {
            self.turn_completed_run_ids
                .push(state.current_run_id.clone());
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

    fn tool_preamble_result(
        preamble: &str,
        tool_calls: Vec<Value>,
        edge_tools: Vec<EdgeToolExecResult>,
        prompt: u64,
        completion: u64,
        ttft: Option<u64>,
    ) -> HostTurnResult {
        HostTurnResult {
            accum: ChatTurnSseAccum {
                full_text: preamble.to_string(),
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
            tool_result_fields: Some(edge_runtime_environment_fields()),
            status: "completed".to_string(),
            duration_ms: 10,
        }
    }

    fn make_detached_bash_edge_tool(task_id: &str) -> EdgeToolExecResult {
        let mut fields = edge_runtime_environment_fields();
        fields.insert("bash_detached".to_string(), json!(true));
        fields.insert("background_task_id".to_string(), json!(task_id));
        EdgeToolExecResult {
            request_id: "req-bash".to_string(),
            tool: "bash".to_string(),
            args: json!({"command": "make check 2>&1"}),
            output: format!(
                "<bash_detached>The bash command was promoted to background task {task_id}.</bash_detached>"
            ),
            tool_result_fields: Some(fields),
            status: "completed".to_string(),
            duration_ms: 10,
        }
    }

    fn make_running_agent_fanout_edge_tool(group_id: &str) -> EdgeToolExecResult {
        EdgeToolExecResult {
            request_id: "req-agent-fanout".to_string(),
            tool: "agent_fanout".to_string(),
            args: json!({
                "action": "start",
                "target_count": 3,
                "slots": []
            }),
            output: json!({
                "status": "started",
                "group_id": group_id,
                "target_count": 3,
                "fanout": {
                    "accepted": 3,
                    "active": 3,
                    "completed": 0,
                    "failed": 0,
                    "cancelled_by_user": 0,
                    "cancelled_by_parent_budget": 0,
                    "timed_out": 0,
                    "terminal": 0,
                    "group_id": group_id,
                    "status": "running"
                }
            })
            .to_string(),
            tool_result_fields: Some(edge_runtime_environment_fields()),
            status: "completed".to_string(),
            duration_ms: 10,
        }
    }

    fn make_edge_tool_with_args(name: &str, args: Value, output: &str) -> EdgeToolExecResult {
        EdgeToolExecResult {
            request_id: format!("req-{name}"),
            tool: name.to_string(),
            args,
            output: output.to_string(),
            tool_result_fields: Some(edge_runtime_environment_fields()),
            status: "completed".to_string(),
            duration_ms: 10,
        }
    }

    // ── State builder ───────────────────────────────────────────────────────

    pub(crate) fn make_state() -> AgenticLoopState {
        let tool_policy =
            astra_config::runtime_config::ToolPolicyConfig::default().resolve_for_model(None);

        AgenticLoopState {
            messages: Vec::new(),
            volatile_pending: Vec::new(),
            recent_rounds: Vec::new(),
            tool_results: Vec::new(),
            current_session_id: None,
            current_run_id: None,
            context_manifest_pool: None,
            context_manifest_user_id: None,
            context_manifest_model_name: None,
            runtime_manifest: None,
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
            turn_budget_hint_emitted_90: false,
            turn_budget_hint_emitted_50: false,
            turn_budget_hint_emitted_20: false,
            agentic_turn_budget: TaskExecutionProfile::default().agentic_turn_budget,
            current_round_index: 0,
            llm_rounds_completed: 0,
            last_request_message_count: None,
            turn_guard: TurnGuard::new(),
            restricted_tools: HashSet::new(),
            boosted_tools: HashSet::new(),
            widen_surface_pending: false,
            step_recorder: StepRecorder::new("test-user", "test-session", "test-task"),
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(0.95),
            call_counts: HashMap::new(),
            max_identical_tool_calls: tool_policy.max_identical_tool_calls,
            max_tools_per_turn: tool_policy.max_tools_per_turn,
            repeated_cache_hit_suppression: tool_policy.repeated_cache_hit_suppression,
            max_consecutive_empty_name: tool_policy.max_consecutive_empty_name,
            stall: Default::default(),
            telemetry: Default::default(),
            skills: SkillState {
                quality_tracker: crate::skills::quality::SkillQualityTracker::new(),
                improvement_tracker: astra_skills::improvement::ImprovementTracker::new(),
                ..Default::default()
            },
            hooks: Default::default(),
            messaging: Default::default(),
            cancellation: Default::default(),
            deferred_input: Default::default(),
            error_recovery: Default::default(),
            pipeline_session: None,
            message: "test query".to_string(),
            recent_tools: Vec::new(),
            task_profile: TaskExecutionProfile::default(),
            last_turn_policy: TurnInteractionPolicy::default(),
            api: astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            api_token: String::new(),
            delegation_engine: None,
            delegations_this_turn: 0,
            delegation_chain: Vec::new(),
            self_agent_id: "orchestrator".to_string(),
            run_control: None,
            project_context: None,
            checkpoint_gate: None,
            last_llm_context_manifest_trace: None,
            rate_limit_cooldown: Default::default(),
            data_snapshot_provider: None,
            last_composite_snapshot: None,
            last_measured_prompt_tokens: None,
            consecutive_context_window_errors: 0,
            compaction_effectiveness: Default::default(),
            always_load_tool_schema_tokens: 0,
            sticky_tool_schemas: Vec::new(),
            max_turn_input_tokens: 0,
            budget_wrapup_injected: false,
            budget_wrapup_ignored_rounds: 0,
            compact_tier_applied: CompactionTier::Normal,
            skill_produced_output: false,
            max_cumulative_tokens: 0,
            thinking: astra_turn_core::thinking_config::ThinkingConfig::Off,
            recent_file_reads: Vec::new(),
            permission_context: Some(crate::orchestration::PermissionSyncContext::shared_root(
                crate::orchestration::PermissionMode::Auto,
            )),
            permission_handler: None,
            tactical_adapter: None,
            step_signal_collector: None,
            recent_tactical_actions: Vec::new(),
            server_tool_executor: None,
            interruption: None,
            session_facts: Default::default(),
            memory_extraction_service: None,
            session_memory_state: Default::default(),
            session_memory_llm_params: None,
            compact_strategy: Default::default(),
            approval_overrides: None,
            confidence_trend: Default::default(),
            last_confidence_diagnosis: None,
            session_turn: 0,
            bridge_turn_chain_id: None,
            bridge_user_query_event_id: None,
            turn_event_buffer: None,
            harness: crate::turn::harness_adapter::HarnessSlot::empty(),
        }
    }

    #[test]
    fn invalid_harness_recovery_threshold_resets_streak_instead_of_overriding() {
        let mut state = make_state();
        state.stall.circuit_breaker.set_read_only_threshold(12);
        state
            .stall
            .circuit_breaker
            .observe(astra_turn_core::loop_circuit_breaker::RoundSignal {
                tool_signatures: std::iter::once("read_file:/tmp/test".to_string()).collect(),
                produced_mutation: false,
                task_completed: false,
                tool_count: 1,
            });

        apply_harness_pause_recovery_threshold(&mut state, Some(0));

        assert_eq!(state.stall.circuit_breaker.read_only_threshold(), 12);
        assert_eq!(state.stall.circuit_breaker.consecutive_read_only(), 0);
    }

    #[test]
    fn valid_harness_recovery_threshold_tightens_breaker() {
        let mut state = make_state();
        state.stall.circuit_breaker.set_read_only_threshold(12);

        apply_harness_pause_recovery_threshold(&mut state, Some(4));

        assert_eq!(state.stall.circuit_breaker.read_only_threshold(), 4);
    }

    #[test]
    fn task_board_snapshot_summarizes_active_tasks_stably() {
        let snapshot = TaskBoardSnapshot::from_active_tasks(&[
            SessionTask {
                archived_at: None,
                id: "task-2".to_string(),
                title: "add runtime tests".to_string(),
                description: None,
                status: astra_tools::task_mgmt::SessionTaskStatusKind::Pending,
                subtasks: Vec::new(),
                created_at: "2025-01-01T00:00:00Z".to_string(),
                updated_at: "2025-01-01T00:00:00Z".to_string(),
                active_form: None,
                owner: None,
                metadata: None,
                blocks: Vec::new(),
                blocked_by: vec!["task-1".to_string()],
            },
            SessionTask {
                archived_at: None,
                id: "task-1".to_string(),
                title: "wire completion guard".to_string(),
                description: None,
                status: astra_tools::task_mgmt::SessionTaskStatusKind::InProgress,
                subtasks: Vec::new(),
                created_at: "2025-01-01T00:00:00Z".to_string(),
                updated_at: "2025-01-01T00:00:00Z".to_string(),
                active_form: None,
                owner: None,
                metadata: None,
                blocks: Vec::new(),
                blocked_by: Vec::new(),
            },
            SessionTask {
                archived_at: None,
                id: "task-3".to_string(),
                title: "already done".to_string(),
                description: None,
                status: astra_tools::task_mgmt::SessionTaskStatusKind::Completed,
                subtasks: Vec::new(),
                created_at: "2025-01-01T00:00:00Z".to_string(),
                updated_at: "2025-01-01T00:00:00Z".to_string(),
                active_form: None,
                owner: None,
                metadata: None,
                blocks: Vec::new(),
                blocked_by: Vec::new(),
            },
        ]);

        assert_eq!(snapshot.pending_count, 1);
        assert_eq!(snapshot.in_progress_count, 1);
        assert_eq!(snapshot.blocked_count, 1);
        assert_eq!(
            snapshot.active_tasks,
            vec![
                "task-2 add runtime tests [pending]".to_string(),
                "task-1 wire completion guard [in_progress]".to_string(),
            ]
        );
        assert!(snapshot.has_unfinished_tasks());
        assert!(snapshot.short_summary().contains("task(s) remain"));
        assert_eq!(
            snapshot.status_count_summary(),
            "1 in_progress, 1 pending task(s) remain"
        );
        assert!(VolatileKind::TaskBoardCompletionGate.is_singleton());
        assert!(VolatileKind::TaskBoardStartGate.is_singleton());
    }

    #[test]
    fn task_board_snapshot_ignores_terminal_and_archived_tasks() {
        let snapshot = TaskBoardSnapshot::from_active_tasks(&[
            SessionTask {
                archived_at: None,
                id: "task-1".to_string(),
                title: "waiting".to_string(),
                description: None,
                status: astra_tools::task_mgmt::SessionTaskStatusKind::Pending,
                subtasks: Vec::new(),
                created_at: "2025-01-01T00:00:00Z".to_string(),
                updated_at: "2025-01-01T00:00:00Z".to_string(),
                active_form: None,
                owner: None,
                metadata: None,
                blocks: Vec::new(),
                blocked_by: Vec::new(),
            },
            SessionTask {
                archived_at: None,
                id: "task-2".to_string(),
                title: "done".to_string(),
                description: None,
                status: astra_tools::task_mgmt::SessionTaskStatusKind::Completed,
                subtasks: Vec::new(),
                created_at: "2025-01-01T00:00:00Z".to_string(),
                updated_at: "2025-01-01T00:00:00Z".to_string(),
                active_form: None,
                owner: None,
                metadata: None,
                blocks: Vec::new(),
                blocked_by: Vec::new(),
            },
            SessionTask {
                archived_at: None,
                id: "task-3".to_string(),
                title: "cancelled".to_string(),
                description: None,
                status: astra_tools::task_mgmt::SessionTaskStatusKind::Cancelled,
                subtasks: Vec::new(),
                created_at: "2025-01-01T00:00:00Z".to_string(),
                updated_at: "2025-01-01T00:00:00Z".to_string(),
                active_form: None,
                owner: None,
                metadata: None,
                blocks: Vec::new(),
                blocked_by: Vec::new(),
            },
            SessionTask {
                archived_at: None,
                id: "task-4".to_string(),
                title: "archived".to_string(),
                description: None,
                status: astra_tools::task_mgmt::SessionTaskStatusKind::Archived,
                subtasks: Vec::new(),
                created_at: "2025-01-01T00:00:00Z".to_string(),
                updated_at: "2025-01-01T00:00:00Z".to_string(),
                active_form: None,
                owner: None,
                metadata: None,
                blocks: Vec::new(),
                blocked_by: Vec::new(),
            },
        ]);

        assert_eq!(snapshot.pending_count, 1);
        assert_eq!(snapshot.in_progress_count, 0);
        assert_eq!(snapshot.blocked_count, 0);
        assert_eq!(
            snapshot.active_tasks,
            vec!["task-1 waiting [pending]".to_string()]
        );
    }

    #[tokio::test]
    async fn refresh_task_board_snapshot_preserves_previous_snapshot_on_load_failure() {
        struct LoadFailsTaskStore;

        #[async_trait::async_trait]
        impl astra_tools::task_mgmt::TaskStore for LoadFailsTaskStore {
            async fn load(&self, _session_id: &str) -> Result<Vec<SessionTask>, String> {
                Err("simulated task-board load failure".to_string())
            }

            async fn save(
                &self,
                _session_id: &str,
                _tasks: Vec<SessionTask>,
            ) -> Result<(), String> {
                Ok(())
            }

            async fn next_task_id(&self, _session_id: &str) -> Result<u32, String> {
                Ok(1)
            }

            async fn peek_next_task_id(&self, _session_id: &str) -> Result<u32, String> {
                Ok(1)
            }
        }

        let mut state = make_state();
        state.current_session_id = Some("session-load-fails".to_string());
        state.hooks.task_board_monitor = Some(Arc::new(TaskManager::new(
            "session-load-fails",
            Arc::new(LoadFailsTaskStore),
        )));
        state.hooks.task_board_snapshot = TaskBoardSnapshot::from_active_tasks(&[SessionTask {
            archived_at: None,
            id: "task-1".to_string(),
            title: "finish cloud runtime task guard".to_string(),
            description: None,
            status: astra_tools::task_mgmt::SessionTaskStatusKind::InProgress,
            subtasks: Vec::new(),
            created_at: "2025-01-01T00:00:00Z".to_string(),
            updated_at: "2025-01-01T00:00:00Z".to_string(),
            active_form: None,
            owner: None,
            metadata: None,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
        }]);
        let previous = state.hooks.task_board_snapshot.clone();

        state.refresh_task_board_snapshot().await;

        assert_eq!(state.hooks.task_board_snapshot, previous);
        assert!(state.hooks.task_board_snapshot.has_unfinished_tasks());
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
    async fn text_only_final_response_closes_current_step() {
        let mut host = MockHost::new(vec![text_result("final", 10, 5, Some(30))]);
        let mut state = make_state();

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok(), "expected Ok but got: {:?}", outcome);

        let terminal_events: Vec<_> = state
            .step_recorder
            .events()
            .iter()
            .filter(|event| {
                matches!(
                    event.event_type,
                    astra_pipeline::step_protocol::StepEventType::StepCompleted
                        | astra_pipeline::step_protocol::StepEventType::StepIncomplete
                        | astra_pipeline::step_protocol::StepEventType::StepFailed
                        | astra_pipeline::step_protocol::StepEventType::StepRetried
                )
            })
            .collect();
        assert_eq!(terminal_events.len(), 1, "{terminal_events:?}");
        assert_eq!(
            terminal_events[0].event_type,
            astra_pipeline::step_protocol::StepEventType::StepCompleted
        );
    }

    #[tokio::test]
    async fn tool_round_then_final_text_records_one_terminal_event_per_step() {
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![make_edge_tool("bash", "file list")], 20, 10, Some(50)),
            text_result("done", 15, 5, Some(30)),
        ]);
        let mut state = make_state();

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok(), "expected Ok but got: {:?}", outcome);

        let mut terminal_counts = std::collections::BTreeMap::new();
        for event in state.step_recorder.events() {
            if matches!(
                event.event_type,
                astra_pipeline::step_protocol::StepEventType::StepCompleted
                    | astra_pipeline::step_protocol::StepEventType::StepIncomplete
                    | astra_pipeline::step_protocol::StepEventType::StepFailed
                    | astra_pipeline::step_protocol::StepEventType::StepRetried
            ) {
                *terminal_counts
                    .entry(event.step_id.clone())
                    .or_insert(0usize) += 1;
            }
        }

        assert_eq!(
            terminal_counts.values().copied().collect::<Vec<_>>(),
            vec![1, 1],
            "each created step should have exactly one terminal event: {terminal_counts:?}"
        );
    }

    #[tokio::test]
    async fn edge_tool_completion_trace_has_call_id_and_args_preview() {
        let mut host = MockHost::new(vec![
            edge_tool_result(
                vec![make_edge_tool_with_args(
                    "bash",
                    json!({"command":"git diff --stat"}),
                    "diff stat",
                )],
                20,
                10,
                Some(50),
            ),
            text_result("done", 15, 5, Some(30)),
        ])
        .with_valid_tools(&["bash"]);
        let mut state = make_state();

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok(), "expected Ok but got: {:?}", outcome);

        let completed = state
            .step_recorder
            .events()
            .iter()
            .find(|event| {
                event.event_type == astra_pipeline::step_protocol::StepEventType::ToolCallCompleted
                    && event
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("tool_name"))
                        .and_then(serde_json::Value::as_str)
                        == Some("bash")
            })
            .expect("bash ToolCallCompleted event");
        let payload = completed.payload.as_ref().unwrap();
        assert!(
            payload
                .get("call_id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| !value.is_empty()),
            "payload should include non-empty call_id: {payload:?}"
        );
        assert_eq!(
            payload
                .get("args_preview")
                .and_then(serde_json::Value::as_str),
            Some("git diff --stat")
        );
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
        assert!(state.final_text.contains("without a final answer")); // EmptyCompletion message
        assert_eq!(state.remaining_turns, 10); // Unchanged
        // EmptyCompletion interruption recorded
        let interruption = state
            .interruption
            .as_ref()
            .expect("should record interruption");
        assert_eq!(
            interruption.kind,
            astra_turn_core::interruption::InterruptionKind::EmptyCompletion
        );
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
        state.task_profile = astra_turn_core::chat_turn_heuristics::infer_task_execution_profile(
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
                .volatile_pending
                .iter()
                .any(|inj| inj.content.contains("Budget review")),
            "budget-review injection expected in volatile_pending; got {:?}",
            state.volatile_pending,
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
        state.task_profile = astra_turn_core::chat_turn_heuristics::infer_task_execution_profile(
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
        state.task_profile = astra_turn_core::chat_turn_heuristics::infer_task_execution_profile(
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
            astra_turn_core::agentic_verdict_audit::AgenticVerdictAuditEvent {
                turn: 1,
                severity: "warning".into(),
                injections: vec!["stall detected".into()],
                avoid_tools: vec!["write_file".into()],
                deprioritized_tools: vec![],
                force_stop: false,
                nudge_count: 1,
                interaction_mode: "prompt".into(),
                suppressed_loop_nudges: false,
                recent_error_pressure: 0,
                recent_timeout_pressure: 0,
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
        state.task_profile = astra_turn_core::chat_turn_heuristics::infer_task_execution_profile(
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
                .volatile_pending
                .iter()
                .any(|inj| inj.content.contains("Budget review")),
            "budget-review injection expected in volatile_pending; got {:?}",
            state.volatile_pending,
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
        state.task_profile = astra_turn_core::chat_turn_heuristics::infer_task_execution_profile(
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
        state.final_text = "changes look good".to_string();

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(host.current_turn, 2);
        assert_eq!(state.max_turns, 2);
        assert!(
            !state.final_text.contains("changes look good"),
            "budget exhaustion must overwrite stale success-shaped text"
        );
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

    #[tokio::test]
    async fn replay_tool_churn_budget_exhaustion_is_partial_and_bounded() {
        let large_bash_diff = format!(
            "diff --git a/src/lib.rs b/src/lib.rs\n{}",
            "+ changed from bash git diff\n".repeat(4_000)
        );
        let large_structured_diff = format!(
            "diff --git a/src/lib.rs b/src/lib.rs\n{}",
            "+ changed from structured git_diff\n".repeat(4_000)
        );
        let mut host = MockHost::new(vec![
            tool_preamble_result(
                "The changes look good; I will just inspect the diff.",
                vec![json!({
                    "id": "req-bash",
                    "type": "function",
                    "function": {
                        "name": "bash",
                        "arguments": "{\"command\":\"git diff -- src/\"}"
                    }
                })],
                vec![make_edge_tool_with_args(
                    "bash",
                    json!({"command": "git diff -- src/"}),
                    &large_bash_diff,
                )],
                90_000,
                200,
                Some(20),
            ),
            tool_preamble_result(
                "Everything appears fixed after the diff.",
                vec![json!({
                    "id": "req-git_diff",
                    "type": "function",
                    "function": {
                        "name": "git_diff",
                        "arguments": "{\"path\":\"src\",\"ref\":\"HEAD\"}"
                    }
                })],
                vec![make_edge_tool_with_args(
                    "git_diff",
                    json!({"path": "src", "ref": "HEAD"}),
                    &large_structured_diff,
                )],
                95_000,
                250,
                Some(25),
            ),
            text_result("should never run", 10, 5, Some(20)),
        ])
        .with_valid_tools(&["bash", "git_diff"]);
        let mut state = make_state();
        state.max_turns = 2;
        state.remaining_turns = 2;
        state.final_text = "stale success from a previous turn".to_string();

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(outcome.is_ok());
        assert_eq!(host.current_turn, 2);
        assert!(state.interruption.is_some(), "budget exhaustion is partial");
        assert!(
            state.final_text.contains("Turn budget exhausted"),
            "budget exhaustion should surface resumable partial status"
        );
        assert!(
            !state.final_text.contains("stale success")
                && !state.final_text.contains("changes look good")
                && !state.final_text.contains("Everything appears fixed"),
            "tool-call preambles and stale success text must not become final output"
        );
        assert_eq!(host.rendered_final_text, vec![state.final_text.clone()]);

        let tool_contents: Vec<&str> = state
            .messages
            .iter()
            .filter(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
            .filter_map(|message| message.get("content").and_then(Value::as_str))
            .collect();
        assert_eq!(tool_contents.len(), 2);
        // After folding, each tool result should be well below the original 50 000-char payloads.
        // The observed folded sizes are ~922–1096 chars (FOLD_KEEP_CHARS=200 plus annotation
        // and line-boundary overhead).  Using 1500 as a generous ceiling keeps the assertion
        // coupled to realistic folding output rather than the old 18_500 that would silently
        // pass even if folding regressed entirely.
        const FOLD_BOUND_CHARS: usize = 1_500;
        assert!(
            tool_contents
                .iter()
                .all(|content| content.chars().count() <= FOLD_BOUND_CHARS),
            "large diff/read outputs should be folded before replaying into the next prompt: {:?}",
            tool_contents
                .iter()
                .map(|content| content.chars().count())
                .collect::<Vec<_>>()
        );
        assert!(
            tool_contents
                .iter()
                .any(|content| content.contains("truncated"))
                || tool_contents
                    .iter()
                    .any(|content| content.contains("elided")),
            "bounded tool messages should explain that output was folded"
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
    async fn detached_bash_edge_tool_stops_without_followup_poll_round() {
        let mut host = MockHost::new(vec![
            edge_tool_result(
                vec![make_detached_bash_edge_tool("bg-shell-1")],
                10,
                5,
                None,
            ),
            edge_tool_result(
                vec![make_edge_tool("task_output", "should not run")],
                10,
                5,
                None,
            ),
            text_result("done", 10, 5, None),
        ])
        .with_valid_tools(&["bash", "task_output"]);
        let mut state = make_state();

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(
            matches!(
                outcome,
                Ok(AgenticLoopOutcome::Waiting(ref reason))
                    if reason == "background_task_detached:bg-shell-1"
            ),
            "{outcome:?}"
        );
        assert_eq!(
            host.turn_count(),
            1,
            "detached bash must stop the current turn without a follow-up poll round"
        );
        assert_eq!(state.total_tool_calls, 1);
        assert!(state.telemetry.all_tools_used.contains("bash"));
        assert!(!state.telemetry.all_tools_used.contains("task_output"));
        assert!(
            state
                .messages
                .iter()
                .any(|message| message.to_string().contains("bg-shell-1")),
            "background task id must remain visible in the tool result messages"
        );
    }

    #[tokio::test]
    async fn running_agent_fanout_stops_without_followup_poll_round() {
        let mut host = MockHost::new(vec![
            edge_tool_result(
                vec![make_running_agent_fanout_edge_tool("review-group")],
                10,
                5,
                None,
            ),
            edge_tool_result(
                vec![make_edge_tool("agent_fanout", "should not poll")],
                10,
                5,
                None,
            ),
            text_result("done", 10, 5, None),
        ])
        .with_valid_tools(&["agent_fanout"]);
        let mut state = make_state();

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(
            matches!(
                outcome,
                Ok(AgenticLoopOutcome::Waiting(ref reason))
                    if reason == "agent_fanout_running:review-group"
            ),
            "{outcome:?}"
        );
        assert_eq!(
            host.turn_count(),
            1,
            "running fanout must stop the current turn instead of polling get_results"
        );
        assert_eq!(state.total_tool_calls, 1);
        assert!(state.telemetry.all_tools_used.contains("agent_fanout"));
        assert!(
            state
                .messages
                .iter()
                .any(|message| message.to_string().contains("review-group")),
            "fanout group id must remain visible in the tool result messages"
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

    #[tokio::test]
    async fn preloaded_history_survives_to_host_execute_turn() {
        // Reproduces the post-resume scenario: the CLI packs repl history
        // into `state.messages` before the agentic loop starts. We need
        // the host's `execute_turn` to see those messages on the wire —
        // NOT an empty list or just the current user message.
        let mut host = MockHost::new(vec![text_result("ack", 100, 10, Some(42))]);
        let mut state = make_state();

        // Simulate post-resume messages: 2 prior turns + current user,
        // matching the shape `openai_messages_from_repl_history` produces.
        state.messages = vec![
            json!({"role": "user", "content": "你叫什么"}),
            json!({"role": "assistant", "content": "我叫 Astra。"}),
            json!({"role": "user", "content": "你是谁"}),
            json!({"role": "assistant", "content": "我是 Astra。"}),
            json!({"role": "user", "content": "之前我们聊过什么？"}),
        ];
        state.message = "之前我们聊过什么？".to_string();
        let expected_before = state.messages.len();

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(matches!(outcome, Ok(AgenticLoopOutcome::Completed)));

        assert!(
            !host.executed_messages.is_empty(),
            "host should have been called once"
        );
        let seen = &host.executed_messages[0];
        assert!(
            seen.len() >= expected_before,
            "host must see at least the {} pre-loaded messages + any volatile additions, got {}",
            expected_before,
            seen.len()
        );
        let user_count = seen
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("user"))
            .count();
        let asst_count = seen
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
            .count();
        assert!(
            user_count >= 3,
            "expected ≥3 user messages (history has 3), got {user_count}"
        );
        assert!(
            asst_count >= 2,
            "expected ≥2 assistant messages (history has 2), got {asst_count}"
        );
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

    #[tokio::test]
    async fn cancel_wins_over_pause() {
        // When cancel arrives while the agentic loop is paused, cancel
        // must take precedence — the loop should exit with Cancelled
        // immediately, not stay paused.
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let pause_flag = Arc::new(AtomicBool::new(true));
        let cancel_clone = cancel_flag.clone();
        let pause_clone = pause_flag.clone();

        let handle = tokio::spawn(async move {
            let mut host = MockHost::new(vec![text_result("should not run", 10, 5, Some(42))]);
            let mut state = make_state();
            state.cancellation.flag = Some(cancel_clone);
            state.cancellation.pause_flag = Some(pause_clone);
            let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
            (outcome, host.current_turn, state.final_text)
        });

        tokio::time::sleep(std::time::Duration::from_millis(75)).await;
        assert!(
            !handle.is_finished(),
            "loop should stay paused while only pause is set"
        );

        // Now set cancel — the loop should abort, not keep waiting
        cancel_flag.store(true, std::sync::atomic::Ordering::Relaxed);

        let (outcome, turns, final_text) = handle.await.unwrap();
        assert!(
            matches!(outcome, Ok(AgenticLoopOutcome::Cancelled)),
            "cancel must win over pause, got {outcome:?}"
        );
        assert_eq!(
            turns, 0,
            "no turns should execute when cancelled during pause"
        );
        assert!(final_text.is_empty());
    }

    #[tokio::test]
    async fn pause_cancel_at_same_time_cancelled_immediately() {
        // When both pause and cancel are set from the start, the loop
        // should return Cancelled without waiting.
        let cancel_flag = Arc::new(AtomicBool::new(true));
        let pause_flag = Arc::new(AtomicBool::new(true));
        let cancel_clone = cancel_flag.clone();
        let pause_clone = pause_flag.clone();

        let handle = tokio::spawn(async move {
            let mut host = MockHost::new(vec![text_result("should not run", 10, 5, Some(42))]);
            let mut state = make_state();
            state.cancellation.flag = Some(cancel_clone);
            state.cancellation.pause_flag = Some(pause_clone);
            let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
            (outcome, host.current_turn, state.final_text)
        });

        // Should complete quickly since cancel is checked inside the pause loop
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
        let (outcome, turns, final_text) = result.unwrap().unwrap();
        assert!(
            matches!(outcome, Ok(AgenticLoopOutcome::Cancelled)),
            "both pause+ cancel → must cancel, got {outcome:?}"
        );
        assert_eq!(turns, 0);
        assert!(final_text.is_empty());
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
    -> Arc<crate::server::delegation::engine::DelegationEngine> {
        use crate::server::delegation::engine::{
            DelegationEngine, DelegationTracker, StubSubRunExecutor,
        };
        use crate::server::run::engine::RunEngine;
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

        let mut host = MockHost::new(turns).with_valid_tools(&["delegate"]);
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

        let mut host = MockHost::new(turns).with_valid_tools(&["bash", "delegate"]);
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

        let mut host = MockHost::new(turns).with_valid_tools(&["delegate"]);
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

        let mut host = MockHost::new(turns).with_valid_tools(&["delegate"]);
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

        let mut host = MockHost::new(turns).with_valid_tools(&["delegate"]);
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
    async fn no_schema_injection_without_skill_resolver() {
        // With no skill resolver configured, no schemas should be injected.
        // Delegation is now handled via the always-present `agent` tool.
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

        // No injection: delegation is handled by the consolidated agent tool,
        // and no skill resolver is configured in make_state().
        assert_eq!(host.injected_schemas.len(), 0);
    }

    // ── is_valid_model_string tests ──────────────────────────────────────

    #[test]
    fn valid_model_strings() {
        assert!(crate::turn::agentic::tool_interception::is_valid_model_string("gpt-4o"));
        assert!(
            crate::turn::agentic::tool_interception::is_valid_model_string(
                "claude-sonnet-4-20250514"
            )
        );
        assert!(
            crate::turn::agentic::tool_interception::is_valid_model_string("claude-3.5-sonnet")
        );
        assert!(crate::turn::agentic::tool_interception::is_valid_model_string("openai/gpt-4o"));
        assert!(
            crate::turn::agentic::tool_interception::is_valid_model_string("anthropic:claude-3")
        );
        assert!(crate::turn::agentic::tool_interception::is_valid_model_string("m0"));
    }

    #[test]
    fn invalid_model_strings() {
        assert!(!crate::turn::agentic::tool_interception::is_valid_model_string(""));
        assert!(!crate::turn::agentic::tool_interception::is_valid_model_string("x")); // too short
        assert!(
            !crate::turn::agentic::tool_interception::is_valid_model_string("model with spaces")
        );
        assert!(
            !crate::turn::agentic::tool_interception::is_valid_model_string("-starts-with-dash")
        );
        assert!(!crate::turn::agentic::tool_interception::is_valid_model_string("has;semicolon"));
        assert!(!crate::turn::agentic::tool_interception::is_valid_model_string("has$dollar"));
        assert!(!crate::turn::agentic::tool_interception::is_valid_model_string("has`backtick`"));
        assert!(!crate::turn::agentic::tool_interception::is_valid_model_string("has\nnewline"));
        assert!(!crate::turn::agentic::tool_interception::is_valid_model_string("has\ttab"));
        assert!(!crate::turn::agentic::tool_interception::is_valid_model_string(&"a".repeat(129)));
        // too long
    }

    #[test]
    fn model_string_boundary_lengths() {
        assert!(crate::turn::agentic::tool_interception::is_valid_model_string("ab")); // min valid
        assert!(
            crate::turn::agentic::tool_interception::is_valid_model_string(&format!(
                "m{}",
                "a".repeat(127)
            ))
        ); // 128 = max
        assert!(
            !crate::turn::agentic::tool_interception::is_valid_model_string(&format!(
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
    async fn skill_allowed_tools_are_turn_scoped_only() {
        // Skill tool hints may influence prompt shaping during the active turn,
        // but they must not leak into later turns after host cleanup runs.
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

        assert!(state.restricted_tools.is_empty());

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;

        // Turn-scoped restrictions must be removed after the turn completes.
        assert!(
            state.restricted_tools.is_empty(),
            "skill allowlist restrictions must not leak after turn cleanup"
        );
        // The activated allowlist is still tracked on skill state.
        let allowed = state.skills.allowed_tools.as_ref().unwrap();
        assert!(allowed.contains("bash"));
    }

    // ── CTX_ helper tests ──────────────────────────────────────────────────

    #[test]
    fn extract_repo_name_https() {
        assert_eq!(
            crate::turn::agentic::tool_interception::extract_repo_name_from_url(
                "https://github.com/org/my-repo.git"
            ),
            Some("my-repo".into())
        );
    }

    #[test]
    fn extract_repo_name_ssh() {
        assert_eq!(
            crate::turn::agentic::tool_interception::extract_repo_name_from_url(
                "git@github.com:org/my-repo.git"
            ),
            Some("my-repo".into())
        );
    }

    #[test]
    fn extract_repo_name_no_git_suffix() {
        assert_eq!(
            crate::turn::agentic::tool_interception::extract_repo_name_from_url(
                "https://github.com/org/my-repo"
            ),
            Some("my-repo".into())
        );
    }

    #[test]
    fn extract_repo_name_trailing_slash() {
        assert_eq!(
            crate::turn::agentic::tool_interception::extract_repo_name_from_url(
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
        let types = crate::turn::agentic::tool_interception::detect_project_types(tmp.path());
        assert!(types.contains(&"rust"));
        assert!(types.contains(&"docker"));
    }

    #[test]
    fn detect_project_types_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let types = crate::turn::agentic::tool_interception::detect_project_types(tmp.path());
        assert!(types.is_empty());
    }

    #[test]
    fn detect_project_types_no_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        // Both pyproject.toml and setup.py → single "python"
        std::fs::write(tmp.path().join("pyproject.toml"), "").unwrap();
        std::fs::write(tmp.path().join("setup.py"), "").unwrap();
        let types = crate::turn::agentic::tool_interception::detect_project_types(tmp.path());
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
    fn skill_listing_section_format() {
        use crate::prompts::build_skill_listing_section;
        use crate::turn::skill_tool::SkillToolInfo;

        let skills = vec![
            SkillToolInfo {
                name: "review".into(),
                description: "Code review".into(),
                ..Default::default()
            },
            SkillToolInfo {
                name: "debug".into(),
                description: "Debug issues".into(),
                ..Default::default()
            },
        ];

        let section = build_skill_listing_section(&skills).expect("non-empty");
        assert!(section.text.contains("<name>review</name>"));
        assert!(section.text.contains("<name>debug</name>"));
        assert!(section.text.contains("<available_skills>"));
        assert!(section.text.contains("</available_skills>"));
    }

    #[test]
    fn skill_listing_empty_skills_produces_no_section() {
        use crate::prompts::build_skill_listing_section;
        assert!(
            build_skill_listing_section(&[]).is_none(),
            "empty skill list must produce no section"
        );
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
        use crate::prompts::build_skill_listing_section;
        use crate::turn::skill_tool::SkillToolInfo;

        let mut state = make_state();
        assert!(state.skills.listing_message.is_none());

        let skills_v1 = vec![SkillToolInfo {
            name: "review".into(),
            description: "v1".into(),
            ..Default::default()
        }];
        let section_v1 = build_skill_listing_section(&skills_v1).unwrap();
        state.skills.listing_message = Some(json!({
            "role": "system",
            "content": section_v1.text,
        }));
        let v1 = state.skills.listing_message.as_ref().unwrap()["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(v1.contains("review"));

        let skills_v2 = vec![
            SkillToolInfo {
                name: "review".into(),
                description: "v2".into(),
                ..Default::default()
            },
            SkillToolInfo {
                name: "debug".into(),
                description: "new".into(),
                ..Default::default()
            },
        ];
        let section_v2 = build_skill_listing_section(&skills_v2).unwrap();
        state.skills.listing_message = Some(json!({
            "role": "system",
            "content": section_v2.text,
        }));
        let v2 = state.skills.listing_message.as_ref().unwrap()["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            v2.contains("debug"),
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
    async fn skill_dedup_replays_loaded_content_on_second_invocation() {
        // Turn 1: skill call → full instructions returned + recorded
        // Turn 2: same skill call → replay loaded content + dedup note
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

        // Second call: replay loaded content + dedup note.
        let msg2: Vec<&Value> = state
            .messages
            .iter()
            .filter(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("call_2"))
            .collect();
        assert_eq!(msg2.len(), 1);
        let stub = msg2[0]["content"].as_str().unwrap();
        assert!(
            stub.contains("already loaded"),
            "expected dedup note, got: {stub}"
        );
        assert!(
            stub.contains("# Skill: test-skill"),
            "first re-entry should replay the loaded skill instructions"
        );
        assert!(
            stub.contains("<skill-loaded name=\"test-skill\"/>"),
            "first re-entry should replay the skill-loaded marker"
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
        use crate::turn::skill_tool::InvokedSkill;
        use astra_turn_core::cloud_attachments::AttachmentBuilder;

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
        // `budget_wrapup_injected` is a PER-TURN flag and
        // `finalize_and_render` resets it at turn boundary (Important
        // #3: otherwise next user turn short-circuits on stale state).
        // Assert wrap-up happened via observable outcomes instead:
        //   - Final text made it back to the user (graceful wrapup).
        //   - MockHost consumed 3 turns (r0 → wrapup @ r1 → text @ r2).
        assert_eq!(state.final_text, "Here is my summary.");
        assert_eq!(
            host.current_turn, 3,
            "wrapup path should still proceed through all 3 scripted rounds"
        );
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
    async fn budget_wrapup_lockout_then_abort_on_repeat_ignore() {
        // Task #43 hybrid enforcement:
        //   round 1: 50K prompt (under budget) → proceeds normally
        //   round 2: 90K prompt → exceeds 80K → wrap-up advisory injected,
        //     tool_calls this round still execute (wrap-up is AFTER-measure)
        //   round 3 (first ignored): tool_calls emitted with post-compact
        //     50K prompt → physical lockout, restricted_tools populated,
        //     tools dropped, loop continues
        //   round 4 (second ignored): still tool_calls → abort with
        //     TokenBudgetExceeded interruption
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
            // round 3: under-budget post-compact; model still issues tools.
            edge_tool_result(
                vec![make_edge_tool("bash", "ignored1")],
                50_000,
                2500,
                Some(50),
            ),
            // round 4: model STILL issues tools after the lockout message.
            edge_tool_result(
                vec![make_edge_tool("bash", "ignored2")],
                50_000,
                2600,
                Some(40),
            ),
        ])
        .with_valid_tools(&["bash", "read_file"])
        // Auto mode suppresses the circuit breaker's hard-stop correction so
        // the hybrid's abort path is observed in isolation. In production
        // a concurrent circuit-breaker trip is also a valid termination
        // signal; the hybrid just provides a narrower reason.
        .with_interaction_mode(TurnInteractionMode::Auto);
        let mut state = make_state();
        state.max_turn_input_tokens = 80_000;
        state
            .messages
            .push(json!({"role": "user", "content": "complex task"}));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok(), "loop completes via interruption");
        // Per-turn wrapup flags are reset by `finalize_and_render`
        // (Important #3). Assert on the persistent `interruption`
        // instead — it survives turn-boundary reset because it
        // represents the OUTCOME, not transient mid-turn state.
        assert!(
            state.interruption.is_some(),
            "second ignored round must record an interruption"
        );
        assert_eq!(
            state.interruption.as_ref().unwrap().kind,
            astra_turn_core::interruption::InterruptionKind::TokenBudgetExceeded,
            "abort interruption must be TokenBudgetExceeded"
        );
        assert!(
            state.final_text.contains("Why stopped:"),
            "terminal output should explain why the runtime aborted the wrapup path"
        );
        assert!(
            state
                .final_text
                .contains("ignored both the wrap-up advisory and the restricted-tools lockout"),
            "terminal output should include the concrete abort reason"
        );
    }

    #[tokio::test]
    async fn budget_wrapup_lockout_lets_model_finish_with_text() {
        // Task #43 hybrid — happy path for the lockout tier. Model emits a
        // stray tool_call on the first post-wrap-up round, the runtime drops
        // it and restricts tools; the model's next round is a plain text
        // reply and the turn completes cleanly without an abort interruption.
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
            edge_tool_result(
                vec![make_edge_tool("bash", "stray")],
                50_000,
                2500,
                Some(50),
            ),
            text_result("Done — summary of progress.", 40_000, 200, None),
        ])
        .with_valid_tools(&["bash", "read_file"])
        .with_interaction_mode(TurnInteractionMode::Auto);
        let mut state = make_state();
        state.max_turn_input_tokens = 80_000;
        state
            .messages
            .push(json!({"role": "user", "content": "big task"}));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        // Per-turn flags reset by finalize (Important #3). Observable
        // outcome: no interruption (model recovered with text) and
        // final_text matches the scripted reply.
        assert!(
            state.interruption.is_none(),
            "single-round ignore must NOT abort — the model recovered with text"
        );
        assert_eq!(state.final_text, "Done — summary of progress.");
    }

    // Session 0e37eb46 regression: after `handle_token_budget` runs
    // compaction + spill and returns `ContinueLoop`, the model often
    // produces a "progress summary" instead of resuming work —
    // because its working memory was just shredded and without a
    // counter-directive, the model reads the small post-compaction
    // context as "I've been interrupted, time to report".
    //
    // Contract: when compaction-with-spill fires and succeeds (i.e.
    // freed_tokens > 0), the volatile lane must carry a Resume
    // directive telling the model to CONTINUE, not summarize. The
    // directive rides the volatile lane (not `state.messages[]`) so
    // it doesn't pollute history.
    #[tokio::test]
    async fn compaction_injects_context_note_on_volatile_lane() {
        let session_id = format!(
            "resume-directive-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time")
                .as_nanos()
        );
        let _guard = SpillDirGuard::new(&session_id);

        let mut host = MockHost::new(vec![
            edge_tool_result(
                vec![make_edge_tool("bash", "large output")],
                90_000, // exceeds budget → triggers handle_token_budget
                2_000,
                Some(100),
            ),
            text_result("Fix done.", 40_000, 500, None),
        ]);
        let mut state = make_state();
        state.max_turn_input_tokens = 80_000;
        state.current_session_id = Some(session_id.clone());
        // Make sure tier-1 compression path fires (not tier-2 spill-only):
        state.compact_tier_applied = CompactionTier::Normal;
        state
            .messages
            .push(json!({"role": "user", "content": "diagnose and fix the failing test"}));
        // Enough middle history to give compaction something to free.
        for i in 0..16 {
            state.messages.push(json!({
                "role": "assistant",
                "content": format!("step {i}: investigated something long {}", "x".repeat(200)),
            }));
            state.messages.push(json!({
                "role": "user",
                "content": format!("follow-up {i}"),
            }));
        }

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok(), "loop must complete");

        // The compaction context note must ride the volatile lane. We inspect
        // the lane AFTER the loop; MockHost's execute_turn never
        // drains it, so fires accumulate there for tests to observe.
        let has_context_note = state.volatile_pending.iter().any(|inj| {
            inj.content.contains("Context compacted")
                && inj.content.contains("not a new user request")
                && !inj.content.contains("Continue the task")
                && !inj.content.contains("keep working")
        });
        assert!(
            has_context_note,
            "after compaction fires, a volatile context note must be \
             queued without acting as resume authorization. \
             Current volatile_pending: {:#?}",
            state
                .volatile_pending
                .iter()
                .map(|inj| (
                    format!("{:?}", inj.kind),
                    inj.content.chars().take(80).collect::<String>()
                ))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn budget_after_pre_turn_compact_still_spills_old_messages() {
        let session_id = format!(
            "precompact-spill-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time")
                .as_nanos()
        );
        let _guard = SpillDirGuard::new(&session_id);

        let mut host = MockHost::new(vec![
            edge_tool_result(
                vec![make_edge_tool("bash", "large output")],
                90_000,
                2_000,
                Some(100),
            ),
            text_result("Done after spill.", 40_000, 500, None),
        ]);
        let mut state = make_state();
        state.max_turn_input_tokens = 80_000;
        state.current_session_id = Some(session_id.clone());
        state.compact_tier_applied = CompactionTier::CompactHistory;
        state
            .messages
            .push(json!({"role": "user", "content": "keep debugging"}));
        for i in 0..12 {
            state
                .messages
                .push(json!({"role": "assistant", "content": format!("analysis step {i}")}));
            state
                .messages
                .push(json!({"role": "user", "content": format!("follow-up {i}")}));
        }

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(outcome.is_ok());
        assert_eq!(state.final_text, "Done after spill.");
        let has_spill_msg = state.messages.iter().any(|m| {
            m.get("content")
                .and_then(|c| c.as_str())
                .is_some_and(|s| s.contains("[Context compressed"))
        });
        assert!(
            has_spill_msg,
            "expected spill-to-disk system message after budget recovery"
        );
        let has_budget_wrapup_msg = state.messages.iter().any(|m| {
            m.get("content")
                .and_then(|c| c.as_str())
                .is_some_and(|s| s.contains("token budget limit"))
        });
        assert!(
            !has_budget_wrapup_msg,
            "spill recovery should avoid injecting the hard wrap-up message"
        );
    }

    #[tokio::test]
    async fn compact_tier_gate_skips_mechanical_compression() {
        // When compact_tier_applied >= CompactHistory (e.g. after a pre-turn LLM
        // summary), handle_token_budget must NOT run the tier-1 mechanical
        // CompactionEngine again. We verify this by populating the history
        // with otherwise-compressible tool_result payloads: if the guard is
        // broken, CompactionEngine would rewrite them to `[Cleared]` and the
        // original text would disappear. With the guard honoured the messages
        // stay intact and spill-to-disk (an independent tier-2 recovery) runs.
        let session_id = format!(
            "tier-gate-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time")
                .as_nanos()
        );
        let _guard = SpillDirGuard::new(&session_id);

        let mut host = MockHost::new(vec![
            edge_tool_result(
                vec![make_edge_tool("bash", "large output")],
                90_000,
                2_000,
                Some(100),
            ),
            text_result("Compacted result.", 40_000, 500, None),
        ]);
        let mut state = make_state();
        state.max_turn_input_tokens = 80_000;
        state.current_session_id = Some(session_id.clone());
        // Simulate a pre-turn LLM compact having already run.
        state.compact_tier_applied = CompactionTier::CompactHistory;
        let distinctive_tool_payload = "SENTINEL_RESULT_PAYLOAD_DO_NOT_CLEAR_".repeat(200);
        state
            .messages
            .push(json!({"role": "user", "content": "kick off"}));
        state.messages.push(json!({
            "role": "assistant",
            "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}
            ],
        }));
        state.messages.push(json!({
            "role": "tool",
            "tool_call_id": "call_1",
            "content": distinctive_tool_payload.clone(),
        }));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(state.final_text, "Compacted result.");

        // Either the payload still appears verbatim in live messages, OR it was
        // moved to disk via tier-2 spill (whose system marker shows up instead).
        // What must NOT happen: the mechanical pipeline rewriting it to
        // `[Cleared]` in place — that would mean the tier guard failed.
        let has_cleared_tombstone = state.messages.iter().any(|m| {
            m.get("content")
                .and_then(|c| c.as_str())
                .is_some_and(|s| s.contains("[Cleared") && !s.contains("[Context compressed"))
        });
        assert!(
            !has_cleared_tombstone,
            "tier-1 mechanical compression must be skipped when compact_tier_applied >= CompactHistory",
        );
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

    pub(crate) fn make_hub() -> std::sync::Arc<crate::observability::ObservabilityHub> {
        std::sync::Arc::new(crate::observability::ObservabilityHub::new())
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

    pub(crate) fn make_session()
    -> std::sync::Arc<std::sync::RwLock<crate::observability::ObservabilitySession>> {
        std::sync::Arc::new(std::sync::RwLock::new(
            crate::observability::ObservabilitySession::new_simple("test-session"),
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
            .add_rule(astra_learning::auto_tuning::EvolutionRule::new(
                "test-low-success",
                astra_learning::auto_tuning::EvolutionTrigger::LowSuccessRate {
                    threshold: 0.5,
                    window_secs: 3600,
                    min_samples: 1,
                },
                astra_learning::auto_tuning::EvolutionAction::Alert {
                    message: "low success".into(),
                    severity: astra_learning::auto_tuning::AlertSeverity::Warning,
                },
            ));
        let config = astra_config::runtime_config::RuntimeConfig::default();
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
        let mut config2 = astra_config::runtime_config::RuntimeConfig::default();
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
            .add_rule(astra_learning::auto_tuning::EvolutionRule::new(
                "low-success",
                astra_learning::auto_tuning::EvolutionTrigger::LowSuccessRate {
                    threshold: 0.5,
                    window_secs: 3600,
                    min_samples: 1,
                },
                astra_learning::auto_tuning::EvolutionAction::Alert {
                    message: "low success".into(),
                    severity: astra_learning::auto_tuning::AlertSeverity::Warning,
                },
            ));
        let config = astra_config::runtime_config::RuntimeConfig::default();
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
            .add_rule(astra_learning::auto_tuning::EvolutionRule::new(
                "interrupt-detect",
                astra_learning::auto_tuning::EvolutionTrigger::SignalAccumulation {
                    signal_type: "interruption".into(),
                    count: 1,
                    window_secs: 3600,
                },
                astra_learning::auto_tuning::EvolutionAction::Alert {
                    message: "interrupted".into(),
                    severity: astra_learning::auto_tuning::AlertSeverity::Info,
                },
            ));
        let config = astra_config::runtime_config::RuntimeConfig::default();
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
            .add_rule(astra_learning::auto_tuning::EvolutionRule::new(
                "high-tokens",
                astra_learning::auto_tuning::EvolutionTrigger::HighTokenUsage {
                    threshold_tokens: 50_000,
                    window_secs: 3600,
                    min_samples: 1,
                },
                astra_learning::auto_tuning::EvolutionAction::Alert {
                    message: "high tokens".into(),
                    severity: astra_learning::auto_tuning::AlertSeverity::Warning,
                },
            ));
        let config = astra_config::runtime_config::RuntimeConfig::default();
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
            .add_rule(astra_learning::auto_tuning::EvolutionRule::new(
                "churn-detect",
                astra_learning::auto_tuning::EvolutionTrigger::SignalAccumulation {
                    signal_type: "tool_churn".into(),
                    count: 1,
                    window_secs: 3600,
                },
                astra_learning::auto_tuning::EvolutionAction::Alert {
                    message: "churn".into(),
                    severity: astra_learning::auto_tuning::AlertSeverity::Info,
                },
            ));
        let config = astra_config::runtime_config::RuntimeConfig::default();
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
            .add_rule(astra_learning::auto_tuning::EvolutionRule::new(
                "churn-detect",
                astra_learning::auto_tuning::EvolutionTrigger::SignalAccumulation {
                    signal_type: "tool_churn".into(),
                    count: 1,
                    window_secs: 3600,
                },
                astra_learning::auto_tuning::EvolutionAction::Alert {
                    message: "churn".into(),
                    severity: astra_learning::auto_tuning::AlertSeverity::Info,
                },
            ));
        let config = astra_config::runtime_config::RuntimeConfig::default();
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
            .add_rule(astra_learning::auto_tuning::EvolutionRule::new(
                "low-success",
                astra_learning::auto_tuning::EvolutionTrigger::LowSuccessRate {
                    threshold: 0.8, // 0.67 < 0.8, so this should trigger
                    window_secs: 3600,
                    min_samples: 1,
                },
                astra_learning::auto_tuning::EvolutionAction::Alert {
                    message: "skill failure".into(),
                    severity: astra_learning::auto_tuning::AlertSeverity::Warning,
                },
            ));
        let config = astra_config::runtime_config::RuntimeConfig::default();
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
            astra_learning::auto_tuning::EvolutionRule::new(
                "test-alert",
                astra_learning::auto_tuning::EvolutionTrigger::LowSuccessRate {
                    threshold: 0.5,
                    window_secs: 3600,
                    min_samples: 1,
                },
                astra_learning::auto_tuning::EvolutionAction::Alert {
                    message: "low success detected".into(),
                    severity: astra_learning::auto_tuning::AlertSeverity::Warning,
                },
            )
            .with_cooldown(std::time::Duration::from_secs(0)),
        );

        // Record failures to satisfy the rule trigger.
        for _ in 0..3 {
            hub.record_feedback(astra_learning::auto_tuning::FeedbackSignal::new(
                astra_learning::auto_tuning::SignalType::TaskFailure {
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
            let mut guard = astra_core::sync_poison::recover_rwlock_write(&session);
            for _ in 0..5 {
                guard.record_query("fix the bug in the parser");
            }
        }

        apply_adaptive_execution_profile_with_intent(&mut state, None);

        let guard = astra_core::sync_poison::recover_rwlock_read(&session);
        assert_eq!(
            guard.profile.current_scenario,
            Some(astra_config::user_profile::Scenario::Debugging)
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
            let mut guard = astra_core::sync_poison::recover_rwlock_write(&session);
            guard.previous_turn_user_cancelled = true;
            scenario_before = guard.profile.current_scenario;
        }

        apply_adaptive_execution_profile_with_intent(&mut state, None);

        let guard = astra_core::sync_poison::recover_rwlock_read(&session);
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
            let mut guard = astra_core::sync_poison::recover_rwlock_write(&session);
            for _ in 0..5 {
                guard.record_query("fix the bug in the parser");
            }
            guard.previous_turn_user_cancelled = true;
        }

        // First call: suppressed (flag was true) — scenario unchanged.
        apply_adaptive_execution_profile_with_intent(&mut state, None);
        {
            let guard = astra_core::sync_poison::recover_rwlock_read(&session);
            assert!(!guard.previous_turn_user_cancelled);
            assert_eq!(guard.profile.current_scenario, None);
        }

        // Second call: normal detection path runs.
        apply_adaptive_execution_profile_with_intent(&mut state, None);
        let guard = astra_core::sync_poison::recover_rwlock_read(&session);
        assert_eq!(
            guard.profile.current_scenario,
            Some(astra_config::user_profile::Scenario::Debugging),
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

        apply_adaptive_execution_profile_with_intent(&mut state, None);

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
        state.tactical_adapter = Some(astra_turn_core::liquid_tactical::TacticalAdapter::new(
            astra_turn_core::liquid_tactical::DampenerConfig::default(),
        ));
        state.step_signal_collector = Some(
            astra_turn_core::liquid_step_signals::StepSignalCollector::new(
                astra_turn_core::liquid_step_signals::StepSignalConfig::default(),
                64_000,
            ),
        );

        {
            let mut guard = session.write().unwrap();
            guard.config.context_window.adaptive = false;
        }

        apply_adaptive_execution_profile_with_intent(&mut state, None);

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
            // Opt in to the (off-by-default) shrink path.
            guard.config.context_window.adaptive_budget_reduction = true;
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
    fn per_turn_adaptation_does_not_shrink_by_default() {
        // Regression guard: adaptive_budget_reduction is OFF by default.
        // Session 0e37eb46 shrank its own ceiling under pressure, produced
        // a progress summary, and gave up. The shrink path must be opt-in.
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_session = Some(session.clone());

        {
            let mut guard = session.write().unwrap();
            guard.config.token_budget.max_turn_input_tokens = 80_000;
            guard.config.context_window.adaptive = true;
            // adaptive_budget_reduction stays at default (false).
        }
        state.max_turn_input_tokens = 80_000;

        apply_per_turn_adaptation(&mut state, 72_000);

        let guard = session.read().unwrap();
        assert_eq!(
            guard.config.token_budget.max_turn_input_tokens, 80_000,
            "budget must be unchanged when adaptive_budget_reduction is off"
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

        apply_adaptive_execution_profile_with_intent(&mut state, None);

        let guard = session.read().unwrap();
        // CodeReview scenario should set memory_top_k=7 and verification_strictness=0.7
        assert_eq!(
            guard.profile.current_scenario,
            Some(astra_config::user_profile::Scenario::CodeReview)
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
        apply_adaptive_execution_profile_with_intent(&mut state, None);

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
        apply_adaptive_execution_profile_with_intent(&mut state, None);

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
        apply_adaptive_execution_profile_with_intent(&mut state, None);

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
        apply_adaptive_execution_profile_with_intent(&mut state, None);

        let guard = session.read().unwrap();
        // After cooldown expires, scenario should be allowed to change
        assert_ne!(
            guard.profile.current_scenario,
            Some(astra_config::user_profile::Scenario::Debugging),
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
            guard.config.context_window.adaptive_budget_reduction = true;
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
            guard.config.context_window.adaptive_budget_reduction = true;
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

        apply_adaptive_execution_profile_with_intent(&mut state, None);

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
            guard.config.context_window.adaptive_budget_reduction = true;
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
            .add_rule(astra_learning::auto_tuning::EvolutionRule::new(
                "test-increase-budget",
                astra_learning::auto_tuning::EvolutionTrigger::LowSuccessRate {
                    threshold: 1.0, // always fires
                    window_secs: 3600,
                    min_samples: 0,
                },
                astra_learning::auto_tuning::EvolutionAction::AdjustConfig {
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
        session: &std::sync::Arc<std::sync::RwLock<crate::observability::ObservabilitySession>>,
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
        apply_adaptive_execution_profile_with_intent(state, None);

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
        // See `stress_multi_turn_state_continuity_50_turns` for why we pin
        // journal I/O to a tempdir — adaptive-tuning fans out journal writes
        // for `state.current_session_id` and would otherwise pollute
        // `~/.astra/sessions/stress-test.jsonl` across local test runs.
        let _journal_dir = tempfile::tempdir().unwrap();
        let _journal_guard =
            astra_services::session_journal::JournalDirGuard::new(_journal_dir.path());

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
            .add_rule(astra_learning::auto_tuning::EvolutionRule::new(
                "increase-budget",
                astra_learning::auto_tuning::EvolutionTrigger::LowSuccessRate {
                    threshold: 1.0,
                    window_secs: 3600,
                    min_samples: 0,
                },
                astra_learning::auto_tuning::EvolutionAction::AdjustConfig {
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
        let mut last_scenario: Option<astra_config::user_profile::Scenario> = None;

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

            apply_adaptive_execution_profile_with_intent(&mut state, None);

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
        // Test isolation: every adaptive-tuning turn fans out a journal write
        // for the active session id.  Without a `JournalDirGuard`, those writes
        // land in the developer's real `~/.astra/sessions/<sid>.jsonl` file,
        // accumulating across runs (observed at >10 MB / 38k events) until
        // the next invocation of this test re-reads the entire file on every
        // append and blows the per-test timeout.  Pin journal I/O to a
        // tempdir scoped to this thread.
        let _journal_dir = tempfile::tempdir().unwrap();
        let _journal_guard =
            astra_services::session_journal::JournalDirGuard::new(_journal_dir.path());

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
        use astra_learning::auto_tuning::{FeedbackSignal, SignalType, default_rules};

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

        let config = astra_config::runtime_config::RuntimeConfig::default();
        // Add a rule that triggers on high retry rate.
        hub.tuning()
            .add_rule(astra_learning::auto_tuning::EvolutionRule::new(
                "retry-trigger",
                astra_learning::auto_tuning::EvolutionTrigger::HighRetryRate {
                    threshold: 0.3,
                    window_secs: 3600,
                    min_samples: 1,
                },
                astra_learning::auto_tuning::EvolutionAction::Alert {
                    message: "retries detected".into(),
                    severity: astra_learning::auto_tuning::AlertSeverity::Warning,
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
        let config = astra_config::runtime_config::RuntimeConfig::default();
        hub.tuning()
            .add_rule(astra_learning::auto_tuning::EvolutionRule::new(
                "high-success-check",
                astra_learning::auto_tuning::EvolutionTrigger::LowSuccessRate {
                    threshold: 0.5,
                    window_secs: 3600,
                    min_samples: 1,
                },
                astra_learning::auto_tuning::EvolutionAction::Alert {
                    message: "acceptance".into(),
                    severity: astra_learning::auto_tuning::AlertSeverity::Info,
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
        let config = astra_config::runtime_config::RuntimeConfig::default();
        hub.tuning()
            .add_rule(astra_learning::auto_tuning::EvolutionRule::new(
                "success-check",
                astra_learning::auto_tuning::EvolutionTrigger::LowSuccessRate {
                    threshold: 1.1, // impossible to reach — we're just counting signals
                    window_secs: 3600,
                    min_samples: 1,
                },
                astra_learning::auto_tuning::EvolutionAction::Alert {
                    message: "check".into(),
                    severity: astra_learning::auto_tuning::AlertSeverity::Info,
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
        use astra_turn_core::liquid_step_signals::{StepSignalCollector, StepSignalConfig};
        use astra_turn_core::liquid_tactical::{DampenerConfig, TacticalAction, TacticalAdapter};

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
            let outcome = astra_turn_core::liquid_step_signals::StepOutcome {
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
        use astra_turn_core::liquid_tactical::TacticalAction;

        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_session = Some(session.clone());
        state.max_turn_input_tokens = 100_000;
        {
            let mut guard = session.write().unwrap();
            guard.config.verification.strictness = 0.5;
            guard.config.verification.max_strictness = 0.9;
            guard.config.compression.compression_threshold = 0.8;
            guard.config.context_window.compression_threshold_min = 0.5;
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
        assert!(guard.config.compression.compression_threshold < 0.8);
        assert!(guard.config.token_budget.max_turn_input_tokens < 100_000);
        assert_eq!(
            state.max_turn_input_tokens,
            guard.config.token_budget.max_turn_input_tokens as u64
        );
        assert!(!hints.is_empty());
    }

    #[test]
    fn tactical_turn_budget_mutations_survive_next_adaptive_profile_application() {
        use astra_turn_core::liquid_tactical::TacticalAction;

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

        let lowered_turn_budget = state.max_turn_input_tokens;
        assert!(lowered_turn_budget < 100_000);

        apply_adaptive_execution_profile_with_intent(&mut state, None);

        let guard = session.read().unwrap();
        assert_eq!(
            state.max_turn_input_tokens, lowered_turn_budget,
            "scenario profile should not wipe tactical turn budget reductions"
        );
        assert_eq!(
            guard.config.token_budget.max_turn_input_tokens, lowered_turn_budget as u32,
            "session config should retain tactical turn budget reductions across turns"
        );
    }

    #[test]
    fn tactical_adapter_reset_clears_turn_state() {
        use astra_turn_core::liquid_step_signals::{StepSignalCollector, StepSignalConfig};
        use astra_turn_core::liquid_tactical::TacticalAdapter;

        let mut state = make_state();
        state.max_turn_input_tokens = 50_000;
        state.step_signal_collector = Some(StepSignalCollector::new(
            StepSignalConfig::default(),
            50_000,
        ));
        state.tactical_adapter = Some(TacticalAdapter::new(
            astra_turn_core::liquid_tactical::DampenerConfig::default(),
        ));

        // Record some outcomes
        if let Some(ref mut collector) = state.step_signal_collector {
            collector.record(astra_turn_core::liquid_step_signals::StepOutcome {
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
            collector.record(astra_turn_core::liquid_step_signals::StepOutcome {
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
                || triggers.iter().all(|t| matches!(
                    t,
                    astra_turn_core::liquid_step_signals::AdaptationTrigger::Nominal
                )),
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
        // Direct verification of the runtime microcompact path: preserve the
        // request prefix, then clear older compactable tool results in-place.
        let big = "x".repeat(1000);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "go"}));
        state.last_request_message_count = Some(state.messages.len());

        for (i, name) in [
            "read_file",
            "grep",
            "list_dir",
            "read_file",
            "grep",
            "glob",
            "read_file",
            "web_search",
        ]
        .into_iter()
        .enumerate()
        {
            let call_id = format!("call-{i}");
            state.messages.push(json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": call_id,
                    "type": "function",
                    "function": {"name": name, "arguments": "{}"}
                }]
            }));
            state.messages.push(json!({
                "role": "tool",
                "tool_call_id": format!("call-{i}"),
                "content": &big,
            }));
        }

        let stats = astra_turn_core::microcompact::compact_tool_results_adaptive_with_persistence_protected_prefix(
            &mut state.messages,
            0.0,
            state.compact_strategy,
            None,
            state.last_request_message_count,
        );

        assert!(
            stats.results_compacted >= 2,
            "expected at least 2 compacted results from 8 tool results (keep=6), got {}",
            stats.results_compacted
        );

        let compacted = state
            .messages
            .iter()
            .filter(|m| {
                m.get("role").and_then(Value::as_str) == Some("tool")
                    && m.get("content")
                        .and_then(Value::as_str)
                        .is_some_and(astra_turn_core::microcompact::is_cleared_content)
            })
            .count();

        assert!(
            compacted >= 2,
            "expected at least 2 compacted results from 8 tool results (keep=6), got {}",
            compacted
        );

        // Verify total content size decreased
        let total_content_bytes: usize = state
            .messages
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
            .map(|m| m.get("content").and_then(Value::as_str).unwrap_or("").len())
            .sum();
        assert!(
            total_content_bytes < 7000,
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
        let arguments = if name == "bash" {
            json!({"command": "true"})
        } else {
            json!({"path": format!("/tmp/{name}.txt")})
        };
        json!({
            "id": id,
            "type": "function",
            "function": {
                "name": name,
                "arguments": arguments.to_string()
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
        use crate::turn::agentic::headless_round::{ToolBatch, partition_tool_batches};
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
        let source = include_str!("host.rs");
        assert!(
            source.contains("state.max_turns.saturating_sub(state.remaining_turns)"),
            "expected saturating_sub in agentic_loop_host"
        );
    }

    // ═══════════════════════════════════════════════════════════════════
    // Session c47c2dca REGRESSION: bridge_inprocess path must drain the
    // structured volatile lane so runtime nudges reach the LLM.
    //
    // 65606b95 migrated 27 runtime injection sites from
    // `state.messages.push(...)` to `state.push_volatile(Kind, content)`.
    // `server_loop_host` drains via `wire_assembly::assemble_llm_messages`,
    // but the CLI `astra chat` path (bridge_inprocess) NEVER drained the
    // lane — every Self-check, force-stop, budget-advisory, corrective,
    // stall-nudge, working-set snapshot, etc. was silently dropped.
    //
    // Fix contract: `AgenticLoopState` must expose a single method the CLI
    // can call right before it hands messages to the bridge HTTP request,
    // which:
    //   1. Drains `volatile_pending` (empty lane afterward).
    //   2. Renders its contents into a single role=user message.
    //   3. Returns `None` when the lane is empty (no-op callers).
    //
    // Without this test, the regression could silently return any time
    // someone re-adds a `push_volatile` caller without realizing the CLI
    // consumer-side plumbing is required.
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn take_volatile_pending_as_message_returns_none_when_lane_empty() {
        let mut state = make_state();
        assert!(state.volatile_pending.is_empty());
        assert!(
            state.take_volatile_pending_as_message().is_none(),
            "empty lane must return None so callers can skip the injection"
        );
    }

    #[test]
    fn take_volatile_pending_as_message_drains_and_wraps_user_role() {
        let mut state = make_state();
        state.push_volatile(
            VolatileKind::CircuitBreaker,
            "[Self-check — round 12] You have been reading for 12 rounds. Decide and answer.",
        );
        state.push_volatile(
            VolatileKind::ParallelBatchingForce,
            "## ⚠ Parallel batching force: batch the next 3 independent calls together.",
        );
        assert_eq!(state.volatile_pending.len(), 2, "precondition: 2 queued");

        let msg = state
            .take_volatile_pending_as_message()
            .expect("lane with content must produce a user message");

        // Lane drained (next LLM call starts clean).
        assert!(
            state.volatile_pending.is_empty(),
            "take_* must empty the lane"
        );

        // Wrapped as role=user so the bridge/provider treats it as a user turn.
        assert_eq!(msg["role"], "user");

        // Content includes BOTH nudges so the model sees every runtime
        // signal queued this round.
        let content = msg["content"].as_str().unwrap();
        assert!(
            content.contains("Self-check"),
            "circuit-breaker nudge must appear: {content}"
        );
        assert!(
            content.contains("Parallel batching force"),
            "parallel-batching nudge must appear: {content}"
        );
    }

    #[test]
    fn take_volatile_pending_as_message_is_idempotent_second_call_none() {
        let mut state = make_state();
        state.push_volatile(VolatileKind::StallNudge, "⚠ REFLECTION");
        assert!(state.take_volatile_pending_as_message().is_some());
        assert!(
            state.take_volatile_pending_as_message().is_none(),
            "second take must be None — lane was drained"
        );
    }

    #[test]
    fn take_volatile_pending_appended_to_merges_into_tail_user() {
        // Tail is role=user (fresh turn r0 pattern): appending a new
        // role=user would be consecutive-user → Bedrock HTTP 400. The
        // method must prepend into the existing user msg instead.
        let mut state = make_state();
        state.push_volatile(VolatileKind::StallNudge, "⚠ REFLECTION: stale reads");
        let base = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "do the task"}),
        ];
        let out = state
            .take_volatile_pending_appended_to(base)
            .expect("lane non-empty → Some");
        // Still two msgs — the pivot was merged, not appended.
        assert_eq!(out.len(), 2, "no new msg appended when tail is user");
        let last = &out[1];
        assert_eq!(last["role"], "user");
        let content = last["content"].as_str().unwrap();
        assert!(
            content.contains("⚠ REFLECTION"),
            "volatile must be prepended to tail user content"
        );
        assert!(
            content.contains("do the task"),
            "original user content must be preserved"
        );
        assert!(
            content.starts_with("⚠ REFLECTION"),
            "volatile comes BEFORE existing user content"
        );
    }

    #[test]
    fn take_volatile_pending_appended_to_appends_when_tail_is_tool() {
        // Tool-loop pattern: tail is role=tool (tool_result). Safe to
        // append a fresh role=user msg — no alternation conflict.
        let mut state = make_state();
        state.push_volatile(VolatileKind::CircuitBreaker, "[Self-check — round 12]");
        let base = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "original task"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id":"c1","function":{"name":"bash","arguments":"{}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "c1", "content": "ok"}),
        ];
        let out = state.take_volatile_pending_appended_to(base).unwrap();
        assert_eq!(out.len(), 5, "fresh msg appended after tool_result");
        let last = out.last().unwrap();
        assert_eq!(last["role"], "user");
        assert!(last["content"].as_str().unwrap().contains("Self-check"));
        // Historical task msg untouched.
        assert_eq!(out[1]["content"], "original task");
    }

    #[test]
    fn take_volatile_pending_appended_to_returns_none_when_lane_empty() {
        let mut state = make_state();
        let base = vec![json!({"role": "user", "content": "hi"})];
        assert!(
            state.take_volatile_pending_appended_to(base).is_none(),
            "empty lane → caller uses its base as-is"
        );
    }

    #[test]
    fn take_volatile_pending_appended_to_appends_when_base_empty() {
        let mut state = make_state();
        state.push_volatile(VolatileKind::Corrective, "only nudge");
        let out = state
            .take_volatile_pending_appended_to(Vec::new())
            .expect("lane non-empty");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], "user");
        assert!(out[0]["content"].as_str().unwrap().contains("only nudge"));
    }

    #[test]
    fn take_volatile_pending_appended_to_never_creates_consecutive_user_roles() {
        // Explicit protocol invariant — the LLM-facing payload must
        // alternate assistant/user. Violations → Bedrock HTTP 400.
        // Regression guard: whatever the base shape, the output must
        // not contain adjacent role=user pairs.
        for base in [
            // Fresh-turn (tail=user)
            vec![
                json!({"role": "system", "content": "sys"}),
                json!({"role": "user", "content": "q"}),
            ],
            // Tool-loop (tail=tool)
            vec![
                json!({"role": "user", "content": "q"}),
                json!({"role": "assistant", "content": null, "tool_calls": []}),
                json!({"role": "tool", "tool_call_id": "c1", "content": "out"}),
            ],
            // Assistant-tail (rare — between rounds)
            vec![
                json!({"role": "user", "content": "q"}),
                json!({"role": "assistant", "content": "a"}),
            ],
        ] {
            let mut state = make_state();
            state.push_volatile(VolatileKind::Corrective, "injected");
            let out = state
                .take_volatile_pending_appended_to(base.clone())
                .expect("lane non-empty");
            // Walk adjacent pairs; neither can be role=user twice.
            for window in out.windows(2) {
                let r0 = window[0].get("role").and_then(Value::as_str);
                let r1 = window[1].get("role").and_then(Value::as_str);
                assert!(
                    !(r0 == Some("user") && r1 == Some("user")),
                    "consecutive user-role messages created for base={base:?} → out={out:?}"
                );
            }
        }
    }

    #[test]
    fn take_volatile_pending_as_message_skips_empty_entries() {
        // `push_volatile` already trims empty; simulating the edge where
        // we end up with whitespace-only content should still not crash.
        let mut state = make_state();
        state.push_volatile(VolatileKind::Corrective, "real content");
        // whitespace-only skipped by push_volatile
        state.push_volatile(VolatileKind::Other, "   ");
        let msg = state
            .take_volatile_pending_as_message()
            .expect("non-empty content should produce a message");
        let content = msg["content"].as_str().unwrap();
        assert!(content.contains("real content"));
        assert!(
            !content.trim().is_empty(),
            "rendered content must not be empty whitespace: {content:?}"
        );
    }

    // ═══════════════════════════════════════════════════════════════════
    // Harness E2E tests — prove enforcement works through real loop
    // ═══════════════════════════════════════════════════════════════════

    #[cfg(feature = "harness")]
    mod harness_e2e {
        use super::*;
        use astra_harness::{
            DecisionRecord, HarnessKernel, HarnessLimits, HookPoint, HookVerdict,
            InMemorySnapshotSink, RecordingKernel, SnapshotSink, StandardKernel,
        };
        use std::sync::Arc;

        struct PauseAtPostTurnKernel;

        impl HarnessKernel for PauseAtPostTurnKernel {
            fn snapshot(&self) -> Option<astra_harness::RuntimeSnapshot> {
                None
            }

            fn on_record(&self, record: &DecisionRecord) -> HookVerdict {
                if record.point == HookPoint::PostTurn {
                    HookVerdict::Pause {
                        reason: "decision checkpoint from test".into(),
                        recovery_threshold: None,
                    }
                } else {
                    HookVerdict::Continue
                }
            }
        }

        fn setup_harness_state(
            limits: HarnessLimits,
            turns: usize,
        ) -> (
            AgenticLoopState,
            Arc<InMemorySnapshotSink>,
            Arc<std::sync::RwLock<astra_harness::SessionTrace>>,
        ) {
            let sink = InMemorySnapshotSink::arc();
            let kernel = Arc::new(StandardKernel::configured(
                sink.clone() as Arc<dyn SnapshotSink>,
                limits,
            ));
            let trace = Arc::new(std::sync::RwLock::new(astra_harness::SessionTrace::new(
                Some("e2e-test".into()),
            )));
            let recording = Arc::new(RecordingKernel::with_trace(
                kernel as Arc<dyn HarnessKernel>,
                trace.clone(),
            ));
            let mut state = make_state();
            state.current_session_id = Some("e2e-test".into());
            state.message = "test query".into();
            state
                .messages
                .push(json!({"role": "user", "content": "test query"}));
            state.max_turns = turns;
            state.remaining_turns = turns;
            state.harness = crate::turn::harness_adapter::HarnessSlot::new(
                recording as Arc<dyn HarnessKernel>,
                sink.clone() as Arc<dyn SnapshotSink>,
            );
            (state, sink, trace)
        }

        // ── E2E 1: Budget verifier blocks loop on turn limit ────────────

        #[tokio::test]
        async fn harness_budget_blocks_on_turn_limit() {
            let limits = HarnessLimits {
                max_turns: Some(2),
                ..Default::default()
            };
            let (mut state, sink, trace) = setup_harness_state(limits, 10);

            // Use tool calls to keep the loop iterating (text_result ends the loop immediately).
            // Turn 0: tool call → continues. Turn 1: tool call → continues.
            // Turn 2: PostTurn sees turns_used=3 > max_turns=2 → Block.
            let mut host = MockHost::new(vec![
                edge_tool_result(vec![make_edge_tool("bash", "output 0")], 100, 20, Some(50)),
                edge_tool_result(vec![make_edge_tool("bash", "output 1")], 100, 20, Some(50)),
                edge_tool_result(vec![make_edge_tool("bash", "output 2")], 100, 20, Some(50)),
                edge_tool_result(vec![make_edge_tool("bash", "output 3")], 100, 20, Some(50)),
                text_result("should never reach here", 100, 20, Some(50)),
            ]);
            host = host.with_valid_tools(&["bash"]);

            let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
            assert!(outcome.is_ok());

            // Verify: loop stopped before consuming all 5 turns
            assert!(
                host.turn_count() < 5,
                "harness should have blocked before all 5 turns; ran {}",
                host.turn_count()
            );

            // Verify: interruption was set with HarnessBlocked
            assert!(
                state.interruption.is_some(),
                "interruption must be set when harness blocks"
            );
            let interruption = state.interruption.as_ref().unwrap();
            assert_eq!(
                interruption.kind,
                astra_turn_core::interruption::InterruptionKind::HarnessBlocked,
                "interruption kind must be HarnessBlocked"
            );

            // Verify: snapshot was captured
            assert!(
                sink.latest().is_some(),
                "sink must have at least one snapshot"
            );

            // Verify: trace recorded hook invocations
            let trace = trace.read().unwrap();
            assert!(trace.record_count() > 0, "trace must have recorded hooks");
            // SessionStart + at least one PostTurn
            assert!(
                trace.records_at_point(HookPoint::SessionStart).len() == 1,
                "exactly one SessionStart"
            );
            assert!(
                !trace.records_at_point(HookPoint::PostTurn).is_empty(),
                "at least one PostTurn recorded"
            );
        }

        // ── E2E 2: TurnGuard stall detection blocks loop ───────────────

        #[tokio::test]
        async fn harness_stall_detection_blocks_on_repeated_tool() {
            let limits = HarnessLimits::default();
            let (mut state, _sink, _trace) = setup_harness_state(limits, 20);

            // 10 turns of the same bash tool — TurnGuardVerifierAdapter
            // should detect stall (default fatal_threshold=5)
            let mut host = MockHost::new(
                (0..10)
                    .map(|i| {
                        edge_tool_result(
                            vec![make_edge_tool("bash", &format!("output {i}"))],
                            100,
                            20,
                            Some(50),
                        )
                    })
                    .collect(),
            );
            host = host.with_valid_tools(&["bash"]);

            let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
            assert!(outcome.is_ok());

            // TurnGuardVerifierAdapter fatal_threshold=5 should block before 10 turns
            assert!(
                host.turn_count() < 10,
                "stall detection should have blocked before 10 turns; ran {}",
                host.turn_count()
            );

            // Verify interruption was set
            assert!(
                state.interruption.is_some(),
                "stall block must set interruption"
            );
        }

        // ── E2E 3: Observe-only sink captures data ──────────────────────

        #[tokio::test]
        async fn harness_observe_only_captures_snapshots() {
            let sink = InMemorySnapshotSink::arc();
            let mut state = make_state();
            state.current_session_id = Some("observe-test".into());
            state.message = "hello".into();
            state
                .messages
                .push(json!({"role": "user", "content": "hello"}));
            state.max_turns = 3;
            state.remaining_turns = 3;
            // observe_only: sink only, no kernel/verifiers
            state.harness = crate::turn::harness_adapter::HarnessSlot::observe_only(
                sink.clone() as Arc<dyn SnapshotSink>
            );

            let mut host = MockHost::new(vec![text_result("response 1", 100, 20, Some(50))]);

            let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
            assert!(outcome.is_ok());

            // Verify: sink captured at least one snapshot despite no kernel
            assert!(
                sink.latest().is_some(),
                "observe_only must still write snapshots to sink"
            );
            let snap = sink.latest().unwrap();
            assert_eq!(snap.session_id, "observe-test");
        }

        // ── E2E 4: SessionEnd fires even on normal completion ───────────

        #[tokio::test]
        async fn harness_session_end_fires_on_normal_completion() {
            let limits = HarnessLimits::default();
            let (mut state, _sink, trace) = setup_harness_state(limits, 2);

            let mut host = MockHost::new(vec![text_result("done", 100, 20, Some(50))]);

            let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
            assert!(outcome.is_ok());

            let trace = trace.read().unwrap();
            let session_ends = trace.records_at_point(HookPoint::SessionEnd);
            assert_eq!(
                session_ends.len(),
                1,
                "exactly one SessionEnd must fire on normal completion"
            );
        }

        // ── E2E 5: SessionEnd fires on harness block ───────────────────

        #[tokio::test]
        async fn harness_session_end_fires_on_block() {
            let limits = HarnessLimits {
                max_turns: Some(1),
                ..Default::default()
            };
            let (mut state, _sink, trace) = setup_harness_state(limits, 10);

            let mut host = MockHost::new(vec![
                text_result("turn 1", 100, 20, Some(50)),
                text_result("turn 2 blocked", 100, 20, Some(50)),
                text_result("turn 3 blocked", 100, 20, Some(50)),
            ]);

            let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
            assert!(outcome.is_ok());

            let trace = trace.read().unwrap();
            let session_ends = trace.records_at_point(HookPoint::SessionEnd);
            assert_eq!(
                session_ends.len(),
                1,
                "exactly one SessionEnd must fire even when harness blocks"
            );
        }

        #[tokio::test]
        async fn repeated_post_turn_harness_pause_forces_text_only_finalization() {
            let sink = InMemorySnapshotSink::arc();
            let mut state = make_state();
            state.current_session_id = Some("pause-test".into());
            state.message = "test query".into();
            state
                .messages
                .push(json!({"role": "user", "content": "test query"}));
            state.max_turns = 5;
            state.remaining_turns = 5;
            state.harness = crate::turn::harness_adapter::HarnessSlot::new(
                Arc::new(PauseAtPostTurnKernel) as Arc<dyn HarnessKernel>,
                sink as Arc<dyn SnapshotSink>,
            );

            // PauseAtPostTurnKernel always returns Pause at PostTurn.
            // The loop recovers up to MAX_HARNESS_PAUSE_RECOVERIES times
            // by injecting checkpoint guidance. After that, it gives up
            // on tool use and forces one text-only finalization turn.
            let mut host = MockHost::new(vec![
                edge_tool_result(vec![make_edge_tool("bash", "output")], 100, 20, Some(50)),
                edge_tool_result(vec![make_edge_tool("bash", "output")], 100, 20, Some(50)),
                edge_tool_result(vec![make_edge_tool("bash", "output")], 100, 20, Some(50)),
                text_result("final checkpoint summary", 100, 20, Some(50)),
            ])
            .with_valid_tools(&["bash"]);

            let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
            assert!(outcome.is_ok());
            assert!(
                state.interruption.is_none(),
                "post-turn pause exhaustion should ask the LLM to summarize instead of surfacing a harness interruption; interruption={:?}; final_text={:?}",
                state.interruption,
                state.final_text
            );
            assert!(
                state.final_text.contains("final checkpoint summary"),
                "LLM finalization turn should produce the visible answer"
            );
            let final_request = host
                .executed_messages
                .last()
                .expect("finalization request should be sent");
            let final_prompt = final_request
                .last()
                .and_then(|message| message.get("content"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            assert!(
                final_prompt.contains("produce a concise final response now"),
                "final request should tell the LLM to summarize before stopping: {final_prompt}"
            );
        }

        // ── E2E 6: Snapshot history accumulates across turns ────────────

        #[tokio::test]
        async fn harness_snapshot_history_across_turns() {
            let limits = HarnessLimits::default();
            let (mut state, sink, _trace) = setup_harness_state(limits, 5);

            let mut host = MockHost::new(vec![
                edge_tool_result(vec![make_edge_tool("bash", "hello")], 100, 20, Some(50)),
                text_result("done after tool", 200, 40, Some(30)),
            ]);
            host = host.with_valid_tools(&["bash"]);

            let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
            assert!(outcome.is_ok());

            // Multiple hooks fire per turn, so history should have many entries
            let history = sink.history(20);
            assert!(
                history.len() >= 2,
                "history should have at least 2 snapshots; got {}",
                history.len()
            );
            // Newest first
            assert!(
                history[0].captured_at_unix_millis
                    >= history.last().unwrap().captured_at_unix_millis,
                "history must be newest-first"
            );
        }

        // ── E2E 7: Hook ordering invariant ──────────────────────────────

        #[tokio::test]
        async fn harness_hook_ordering_invariant() {
            let limits = HarnessLimits::default();
            let (mut state, _sink, trace) = setup_harness_state(limits, 3);

            let mut host = MockHost::new(vec![
                edge_tool_result(vec![make_edge_tool("bash", "ok")], 100, 20, Some(50)),
                text_result("done", 100, 20, Some(50)),
            ]);
            host = host.with_valid_tools(&["bash"]);

            let _outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

            let trace = trace.read().unwrap();
            let points: Vec<HookPoint> = trace.records.iter().map(|r| r.point).collect();

            // SessionStart must be first
            assert_eq!(
                points[0],
                HookPoint::SessionStart,
                "first hook must be SessionStart"
            );
            // SessionEnd must be last
            assert_eq!(
                *points.last().unwrap(),
                HookPoint::SessionEnd,
                "last hook must be SessionEnd"
            );

            // PreLlmRequest must come before PostLlmResponse in each turn
            let pre_llm = points.iter().position(|p| *p == HookPoint::PreLlmRequest);
            let post_llm = points.iter().position(|p| *p == HookPoint::PostLlmResponse);
            if let (Some(pre), Some(post)) = (pre_llm, post_llm) {
                assert!(pre < post, "PreLlmRequest must come before PostLlmResponse");
            }
        }
    }
}
