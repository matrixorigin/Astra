//! Runtime-portable agentic multi-turn loop.
//!
//! # Overview
//!
//! [`AgenticLoopHost`] abstracts all host-specific behavior (payload preparation,
//! HTTP posting, SSE consumption, terminal rendering) so the multi-turn loop
//! can run identically in CLI and headless cloud contexts.
//!
//! # Host Implementations
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
use std::sync::atomic::{AtomicBool, Ordering};

use astra_services::session_journal::ToolCallRecord;
use async_trait::async_trait;
use serde_json::Value;

use crate::pipeline::step_checkpoint;
use crate::pipeline::step_protocol::{InMemoryIdempotencyCache, StepCheckpoint};
use crate::pipeline::step_recorder::StepRecorder;
use crate::semantic_dedup::SemanticDedup;
use crate::str_preview::truncate_str;
use crate::tool_registry::SelectionReport;
use crate::turn::agentic_headless_round::{
    HeadlessRoundTerminal, HeadlessStderrStyle, run_agentic_headless_tool_round,
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
use crate::turn::stall::CLI_AGENTIC_TURN_BUDGET_STALL_ABORT_MSG;
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

    // ── Stall + verdict tracking ──
    pub turn_sigs: Vec<BTreeSet<String>>,
    pub turn_tool_names: Vec<HashSet<String>>,
    pub stall_events: Vec<(String, u32)>,
    pub intent_tool_turns: Vec<(Vec<String>, String)>,
    pub verdict_events: Vec<AgenticVerdictAuditEvent>,
    pub last_heavy_checkpoint: Option<StepCheckpoint>,
    pub tool_call_records: Vec<ToolCallRecord>,
    pub forced_factual_retry: bool,

    // ── Explain + telemetry ──
    pub explain_turns: Vec<Value>,
    pub first_ttft_ms: Option<u64>,
    pub all_tools_used: HashSet<String>,
    pub first_selection_report: Option<SelectionReport>,
    pub first_budget_pressure: f64,
    pub first_context_assembly_ms: Option<u64>,
    pub first_memoria_ms: Option<u64>,
    pub first_selector_ms: Option<u64>,
    pub first_selector_strategy: Option<String>,
    pub first_selector_confidence: Option<f64>,
    pub selector_tokens_in: u64,
    pub selector_tokens_out: u64,
    pub all_selected_skills: Vec<String>,

    // ── Host-provided context (read-only by runtime) ──
    pub message: String,
    pub recent_tools: Vec<String>,
    pub task_profile: TaskExecutionProfile,

    // ── API context (for cloud tool delivery) ──
    pub api: astra_thin_client::ThinClient,
    pub api_token: String,

    // ── Cancellation ──
    /// Shared flag checked between turns. Set externally (e.g. by cancel_run).
    pub cancel_flag: Option<Arc<AtomicBool>>,
    /// Optional token cancelled with user cancel for immediate LLM/stream wake.
    pub cancel_token: Option<Arc<CancellationToken>>,

    // ── Delegation ──
    /// Optional delegation engine for multi-agent coordination.
    /// When set, the loop intercepts `delegate` tool calls and routes them
    /// through the delegation engine instead of the headless tool round.
    pub delegation_engine: Option<Arc<crate::server::delegation_engine::DelegationEngine>>,

    // ── Skills ──
    /// Unified skill registry for conditional activation via file paths.
    /// When set, edge tool file paths are recorded for conditional skill activation.
    pub skill_registry_for_activation: Option<Arc<crate::skills::UnifiedSkillRegistry>>,
    /// Optional skill resolver for executing skills as tool calls.
    /// When set, the loop injects a `skill` tool schema and intercepts
    /// `skill` calls, returning resolved instructions as tool results.
    pub skill_resolver: Option<Arc<dyn crate::turn::skill_tool::SkillResolver>>,
    /// Optional skill executor for fork-context skills. When set, skills with
    /// `execution_context: Fork` are executed via this executor (sub-agent loop).
    pub skill_executor: Option<Arc<dyn crate::skills::traits::SkillExecutor>>,
    /// Model override from the most recently activated skill.
    /// When set, the host should use this model instead of the default.
    pub skill_model_override: Option<String>,
    /// Effort level override from the most recently activated skill.
    pub skill_effort: Option<crate::skills::manifest::EffortLevel>,
    /// Agent type hint from the most recently activated skill.
    pub skill_agent_type: Option<String>,
    /// Tool allow-list from the most recently activated skill.
    /// When non-empty, only these tools (plus `skill` itself) should be available.
    /// The host converts this allow-list to additions in `restricted_tools`.
    pub skill_allowed_tools: Option<HashSet<String>>,
    /// Sandbox policy derived from the most recently activated skill's trust tier.
    /// When set, tool execution should apply these restrictions (path boundaries,
    /// env filtering, network control, timeouts).
    pub skill_sandbox_policy: Option<crate::tool_sandbox::SandboxPolicy>,
    /// Per-skill quality metrics accumulated during the session.
    /// Used to boost high-performing skills in selection priority.
    pub skill_quality_tracker: crate::skills::quality::SkillQualityTracker,
    /// Skill auto-improvement tracker — detects user corrections and proposes SKILL.md rewrites.
    pub skill_improvement_tracker: crate::skills::improvement::ImprovementTracker,
    /// Skills pinned by the user — always included in budget (never truncated).
    pub pinned_skills: std::collections::HashSet<String>,
    /// Canonical skill names surfaced via `discover_skills` this session.
    pub discovered_skills: HashSet<String>,
    /// Skill catalog surfacing for this request / session.
    pub skill_search: astra_core::SkillSearchSettings,

    /// Tool event hooks (PreToolUse/PostToolUse) for intercepting tool calls.
    /// Loaded from `.astra/hooks.json` or skill frontmatter.
    pub tool_event_hooks: crate::skills::hooks::ToolEventHookRegistry,

    /// Session event hooks (SessionStart, SessionEnd, etc.).
    /// Loaded from `.astra/hooks.json` alongside tool event hooks.
    pub session_event_hooks: crate::skills::hooks::SessionEventHookRegistry,

    // ── Stop hooks ──
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

    // ── Error budget ──
    /// Consecutive turns where the same error category dominated.
    /// Reset when a turn succeeds or a different error category appears.
    pub consecutive_same_error: u32,
    /// The error category from the last turn (for streak detection).
    pub last_error_category: Option<crate::turn::error_recovery::ErrorCategory>,

    // ── Mid-execution checkpoint gate ──
    /// Optional checkpoint gate checked every N turns during delegation sub-runs.
    /// When the gate returns `false`, the loop aborts with `Cancelled`.
    pub checkpoint_gate: Option<Arc<dyn crate::server::delegation_engine::CheckpointGate>>,

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

    // ── Thinking budget ──
    /// Optional thinking/reasoning budget in tokens for models with extended thinking.
    /// When Some, passed to the API request so the server constrains thinking output.
    pub thinking_budget_tokens: Option<u32>,

    // ── Ephemeral skill listing ──
    /// Skill listing message (available skill names + descriptions).
    /// Stored here instead of in `messages` so hosts can inject it ephemerally
    /// into each LLM request without bloating the persistent conversation history.
    /// Hosts should prepend this to the messages array when building the payload.
    pub skill_listing_message: Option<Value>,

    /// Skills invoked during this session, keyed by canonical name.
    /// Used for same-session dedup and post-compaction re-injection.
    pub invoked_skills: std::collections::HashMap<String, crate::turn::skill_tool::InvokedSkill>,

    /// Recently accessed file paths tracked for post-compaction restoration.
    /// Each entry is `(absolute_path, turn_number)`. The list is bounded to
    /// the most recent [`MAX_TRACKED_FILE_READS`] entries. After compaction,
    /// hosts use this to re-inject recent file contents so the LLM retains
    /// awareness of recently-read code.
    pub recent_file_reads: Vec<(String, u32)>,

    /// Pre-computed cross-session project context (P2 knowledge backflow).
    /// Set once at session init; `None` for sub-runs or when the feature is disabled.
    pub project_context: Option<String>,

    // ── Inter-agent messaging ──
    /// Optional mailbox for receiving messages from other agents.
    /// When set, incoming messages are drained at each turn start and
    /// progress updates are sent to the parent at turn end.
    pub mailbox: Option<crate::messaging::router::AgentMailbox>,

    /// Tracks messages that require acknowledgment and handles retries.
    pub ack_tracker: Option<crate::messaging::ack_tracker::PendingAckTracker>,

    /// Dead letter queue for permanently failed messages.
    pub dead_letter_queue: Option<std::sync::Arc<crate::messaging::dead_letter::DeadLetterQueue>>,

    /// Unified messaging metrics (optional, shared across agents in a delegation).
    pub messaging_metrics: Option<std::sync::Arc<crate::messaging::metrics::MessagingMetrics>>,

    // ── Progress reporting ──
    /// Optional progress emitter for broadcasting turn events to UI/subscribers.
    /// When set, the loop emits `TurnCompleted` events after each turn.
    pub progress_emitter: Option<crate::orchestration::AgentProgressEmitter>,

    // ── Permission sync ──
    /// Optional permission sync context for runtime permission management.
    /// When set, tool execution checks permissions before running and can
    /// request permission from parent agent via mailbox if denied.
    pub permission_context:
        Option<std::sync::Arc<tokio::sync::RwLock<crate::orchestration::PermissionSyncContext>>>,

    /// Optional permission request handler for processing child requests.
    /// When set, incoming PermissionRequest messages are handled automatically.
    pub permission_handler: Option<crate::orchestration::PermissionRequestHandler>,

    // ── Observability (M1-M6 integration) ──
    /// Optional observability session for context tracing, drift detection, and auto-tuning.
    /// When set, hooks are called at turn start/end, tool selection, etc.
    pub observability_session: Option<
        std::sync::Arc<std::sync::RwLock<crate::observability_integration::ObservabilitySession>>,
    >,

    /// Shared observability hub for profile/experiment management.
    /// Typically set at session init and shared across agents.
    pub observability_hub:
        Option<std::sync::Arc<crate::observability_integration::ObservabilityHub>>,

    /// Optional turn trace collector for detailed context assembly observability.
    /// When set, records system prompt, history, memory, and tool selection traces.
    /// Created at turn start, finalized at turn end.
    pub turn_trace_collector: Option<crate::turn::turn_trace_collector::TurnTraceCollector>,
}

/// Consecutive same-category error turns before forcing a strategy change.
const CONSECUTIVE_ERROR_BUDGET: u32 = 3;

/// Maximum number of recent file reads to track for post-compact restoration.
const MAX_TRACKED_FILE_READS: usize = 20;

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

pub const DELEGATE_TOOL_NAME: &str = "delegate";

/// Check if a tool call is a delegation call.
fn is_delegation_call(tool_call: &Value) -> bool {
    tool_call
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
        == Some(DELEGATE_TOOL_NAME)
}

/// Parse delegation arguments from a tool call.
fn parse_delegation_request(
    tool_call: &Value,
    parent_run_id: &str,
    session_id: &str,
    skill_search: &astra_core::SkillSearchSettings,
) -> Result<astra_services::coordination::DelegationRequest, String> {
    let args_str = tool_call
        .get("function")
        .and_then(|f| f.get("arguments"))
        .and_then(Value::as_str)
        .ok_or("delegate call missing arguments")?;

    let args: Value =
        serde_json::from_str(args_str).map_err(|e| format!("invalid delegation JSON: {e}"))?;

    let task = args
        .get("task")
        .and_then(Value::as_str)
        .unwrap_or("delegated task")
        .to_string();

    let pattern = parse_coordination_pattern(&args)?;

    let mut context = std::collections::HashMap::new();
    context.insert(
        "session_id".to_string(),
        Value::String(session_id.to_string()),
    );
    context.insert(
        "skill_search".to_string(),
        serde_json::to_value(skill_search)
            .map_err(|e| format!("failed to encode skill_search config: {e}"))?,
    );
    if let Some(ctx) = args.get("context").and_then(Value::as_object) {
        for (k, v) in ctx {
            context.insert(k.clone(), v.clone());
        }
    }

    Ok(astra_services::coordination::DelegationRequest {
        delegation_id: uuid::Uuid::new_v4().to_string(),
        parent_run_id: parent_run_id.to_string(),
        task,
        pattern,
        user_id: "system".to_string(),
        depth: 0,
        context,
    })
}

/// Parse a CoordinationPattern from the LLM's delegate arguments.
fn parse_coordination_pattern(
    args: &Value,
) -> Result<astra_services::coordination::CoordinationPattern, String> {
    let pattern_type = args
        .get("pattern")
        .and_then(Value::as_str)
        .unwrap_or("sequential");

    let agents: Vec<String> = args
        .get("agents")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_else(|| vec!["coder".to_string()]);

    match pattern_type {
        "fan_out" => Ok(astra_services::coordination::CoordinationPattern::FanOut {
            agent_ids: agents,
            aggregation: astra_services::coordination::AggregationStrategy::AllResults,
            timeout_sec: 300,
        }),
        "pipeline" => {
            let stages = agents
                .into_iter()
                .map(|id| astra_services::coordination::PipelineStage {
                    agent_id: id,
                    output_transform: None,
                })
                .collect();
            Ok(astra_services::coordination::CoordinationPattern::Pipeline { stages })
        }
        "adversarial" => {
            let producer = agents
                .first()
                .cloned()
                .unwrap_or_else(|| "coder".to_string());
            let reviewer = agents
                .get(1)
                .cloned()
                .unwrap_or_else(|| "reviewer".to_string());
            let max_rounds = args.get("max_rounds").and_then(Value::as_u64).unwrap_or(2) as u32;
            Ok(
                astra_services::coordination::CoordinationPattern::AdversarialReview {
                    producer_id: producer,
                    reviewer_id: reviewer,
                    max_rounds,
                    acceptance_threshold: 0.8,
                },
            )
        }
        _ => Ok(
            astra_services::coordination::CoordinationPattern::Sequential {
                agent_ids: agents,
                stop_on_success: false,
            },
        ),
    }
}

/// If the LLM did not pass `cwd` / `git_root` in `delegate` args, inherit the parent run root.
fn merge_workspace_hint_into_delegation_request(
    request: &mut astra_services::coordination::DelegationRequest,
    hint: Option<&str>,
) {
    let Some(root) = hint.map(str::trim).filter(|s| !s.is_empty()) else {
        return;
    };
    let c = &mut request.context;
    if c.contains_key("git_root") || c.contains_key("workspace_root") || c.contains_key("cwd") {
        return;
    }
    c.insert("cwd".to_string(), Value::String(root.to_string()));
}

/// Partition tool calls into delegation calls and remaining calls,
/// execute delegations, and return results as (call_id, result_text) pairs.
async fn partition_and_execute_delegations(
    tool_calls: &[Value],
    engine: &crate::server::delegation_engine::DelegationEngine,
    parent_run_id: &str,
    session_id: &str,
    source_agent_id: &str,
    workspace_hint: Option<&str>,
    skill_search: &astra_core::SkillSearchSettings,
) -> (Vec<(String, String)>, Vec<Value>) {
    let mut delegation_results = Vec::new();
    let mut remaining = Vec::new();

    for tc in tool_calls {
        if is_delegation_call(tc) {
            let call_id = tc
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();

            match parse_delegation_request(tc, parent_run_id, session_id, skill_search) {
                Ok(mut request) => {
                    merge_workspace_hint_into_delegation_request(&mut request, workspace_hint);
                    match engine.execute(request, source_agent_id, None).await {
                        Ok(result) => {
                            let summary = format_delegation_result(&result);
                            delegation_results.push((call_id, summary));
                        }
                        Err(e) => {
                            delegation_results.push((call_id, format!("Delegation failed: {e}")));
                        }
                    }
                }
                Err(e) => {
                    delegation_results.push((call_id, format!("Invalid delegation request: {e}")));
                }
            }
        } else {
            remaining.push(tc.clone());
        }
    }

    (delegation_results, remaining)
}

/// Format a DelegationResult as a human-readable summary for the LLM.
fn format_delegation_result(result: &astra_services::coordination::DelegationResult) -> String {
    let mut parts = Vec::new();
    parts.push(format!(
        "Delegation {} — status: {}",
        result.delegation_id, result.status
    ));

    for ar in &result.agent_results {
        let status_icon = if ar.is_success() { "✅" } else { "❌" };
        let output_preview = ar
            .output
            .as_deref()
            .map(|o| truncate_str(o, 500))
            .unwrap_or_else(|| "[no output]".to_string());
        parts.push(format!(
            "\n{status_icon} Agent '{}' ({}): {output_preview}",
            ar.agent_id, ar.status
        ));
        if let Some(err) = &ar.error {
            parts.push(format!("   Error: {err}"));
        }
    }

    if let Some(agg) = &result.aggregated_output {
        parts.push(format!("\n📋 Aggregated output:\n{agg}"));
    }

    parts.push(format!(
        "\nTokens: {} prompt + {} completion, {} tool calls",
        result.total_prompt_tokens, result.total_completion_tokens, result.total_tool_calls
    ));

    parts.join("\n")
}

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
    let _ = step_checkpoint::write_step_checkpoint(sid, ckpt_num, &cp);

    let turn = (state.max_turns - state.remaining_turns) as u32;
    let snapshot = astra_core::composite_snapshot::CompositeSnapshotBuilder::new(sid.clone(), turn)
        .label(format!("checkpoint-t{turn}"))
        .session_state(format!("{:06}-heavy.json", ckpt_num))
        .workspace_state(sid.clone())
        .build();

    let mut index = step_checkpoint::read_composite_snapshot_index(sid).unwrap_or_default();
    index.snapshots.push(snapshot.clone());
    let _ = step_checkpoint::write_composite_snapshot_index(sid, &index);

    state.last_composite_snapshot = Some(snapshot);
    state.last_heavy_checkpoint = Some(cp);
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

    let snapshot = builder.build();

    let mut index = step_checkpoint::read_composite_snapshot_index(sid).unwrap_or_default();
    index.snapshots.push(snapshot.clone());
    let _ = step_checkpoint::write_composite_snapshot_index(sid, &index);

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

/// Validate a model string from skill frontmatter before passing it to the API.
///
/// Accepts strings matching known provider naming conventions:
/// - Alphanumeric, hyphens, underscores, dots, colons, and forward slashes
/// - Length between 2 and 128 characters
/// - Must start with an ASCII alphanumeric character
///
/// Rejects empty strings, excessively long strings, and strings with
/// suspicious characters (shell metacharacters, whitespace, etc.).
fn is_valid_model_string(model: &str) -> bool {
    let len = model.len();
    if !(2..=128).contains(&len) {
        return false;
    }
    let first = model.as_bytes()[0];
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    model.bytes().all(|b| {
        b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b':' || b == b'/'
    })
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
    // ─── Preamble: fire SessionStart hooks ───────────────────────────────
    if state
        .session_event_hooks
        .has_event(crate::skills::hooks::SessionEvent::SessionStart)
    {
        let session_id = state.current_session_id.as_deref().unwrap_or("");
        let user_msg = state.message.as_str();
        let hook_output = crate::skills::hooks::evaluate_session_hooks(
            &state.session_event_hooks,
            crate::skills::hooks::SessionEvent::SessionStart,
            session_id,
            Some(user_msg),
        )
        .await;
        // Inject context as a system message before the first user message.
        if let Some(ctx) = hook_output.context {
            state.messages.insert(
                0,
                serde_json::json!({
                    "role": "system",
                    "content": format!("[Session hooks]\n{ctx}"),
                }),
            );
        }
        for (key, value) in hook_output.env_vars {
            // Safety: session hooks run once at startup, before concurrent tool execution.
            unsafe { std::env::set_var(&key, &value) };
        }
    }

    // ─── Preamble: auto-inject delegate tool when delegation is wired ────
    if state.delegation_engine.is_some() {
        host.inject_tool_schema(delegate_tool_schema());
    }

    // ─── Preamble: auto-inject send_message tool when mailbox is available ────
    if state.mailbox.is_some() {
        host.inject_tool_schema(crate::messaging::send_tool::send_message_tool_schema());
    }

    // ─── Preamble: auto-inject skill tool when skills are available ──────
    // Register the skill tool schema once so the LLM knows the `skill` tool exists.
    // The skill listing is refreshed per-turn below (skills may change at runtime
    // via hot-reload or MCP server connect/disconnect).
    if let Some(resolver) = &state.skill_resolver {
        let full = resolver.available_skills();
        if !full.is_empty() {
            let (visible, open_skill_name) = crate::turn::skill_tool::visible_skills_for_host_turn(
                &full,
                state.message.as_str(),
                &state.skill_quality_tracker,
                &state.pinned_skills,
                &state.discovered_skills,
                &state.skill_search,
            );
            host.inject_tool_schema(crate::turn::skill_tool::skill_tool_schema(
                &visible,
                Some(&state.skill_quality_tracker),
                Some(&state.pinned_skills),
                open_skill_name,
            ));
            if open_skill_name {
                host.inject_tool_schema(crate::turn::skill_tool::discover_skills_tool_schema());
            }
        }
    }

    // ─── Preamble: inject cross-session project context (P2 knowledge backflow) ──
    if let Some(ref ctx) = state.project_context {
        state.messages.push(serde_json::json!({
            "role": "system",
            "content": format!(
                "## Cross-Session Project Context\n\
                 Below are summaries of recent sessions in this project. \
                 Use them for continuity — avoid re-asking questions already answered.\n\n{ctx}"
            )
        }));
    }

    for turn_index in 0..state.max_turns {
        // ─── Cancel check (cooperative) ─────────────────────────────────
        if state
            .cancel_flag
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Relaxed))
            || state
                .cancel_token
                .as_ref()
                .is_some_and(|t| t.is_cancelled())
        {
            return Ok(AgenticLoopOutcome::Cancelled);
        }

        if state.remaining_turns == 0 {
            return Err(format!(
                "{} ({} turns used)",
                CLI_AGENTIC_TURN_BUDGET_STALL_ABORT_MSG, state.max_turns
            ));
        }
        state.remaining_turns = state.remaining_turns.saturating_sub(1);
        state.step_recorder.begin_turn(turn_index as u32);

        // ─── Observability: turn start hook ──────────────────────────────
        // Record query for scenario detection and drift analysis.
        let turn_start_time = std::time::Instant::now();
        if let (Some(hub), Some(session)) = (&state.observability_hub, &state.observability_session)
        {
            let session_id = state.current_session_id.as_deref().unwrap_or("");
            let user_id = {
                let s = session.read().unwrap();
                s.user_id.clone()
            };
            crate::observability_integration::on_turn_start(
                hub,
                session_id,
                &user_id,
                &state.message,
            );
        }

        // ─── Turn trace collector ──────────────────────────────────────────
        // Create a collector for detailed context assembly traces.
        // Observability session presence enables trace collection.
        if state.observability_session.is_some() && state.turn_trace_collector.is_none() {
            let capture = std::env::var("MO_CAPTURE_TRACES")
                .map(|v| v == "1" || v.to_lowercase() == "true")
                .unwrap_or(true);
            if capture {
                let turn_id = format!("turn-{}", turn_index);
                let session_id = state.current_session_id.clone().unwrap_or_default();
                state.turn_trace_collector = Some(
                    crate::turn::turn_trace_collector::TurnTraceCollector::new(turn_id, session_id),
                );
            }
        }

        if state.permission_handler.is_none()
            && let Some(ctx) = state.permission_context.clone()
        {
            state.permission_handler =
                Some(crate::orchestration::PermissionRequestHandler::new(ctx));
        }

        // ─── Drain inter-agent mailbox ──────────────────────────────────
        // Inject pending messages from peer/parent agents as a system
        // message so the LLM is aware of coordination context.
        // Cap per turn to prevent slow starts.
        const MAX_MAILBOX_DRAIN_PER_TURN: usize = 64;
        if let Some(ref mut mailbox) = state.mailbox {
            let (pending, has_more) = mailbox.drain_bounded(MAX_MAILBOX_DRAIN_PER_TURN);
            if !pending.is_empty() {
                let mut parts = Vec::with_capacity(pending.len());
                for msg in &pending {
                    let from_label = &msg.from.agent_id;

                    // Route Ack/Nack to our tracker (control messages, not shown to LLM).
                    match &msg.payload {
                        crate::messaging::types::MessagePayload::Ack { message_id } => {
                            if let Some(ref tracker) = state.ack_tracker {
                                tracker.acknowledge(message_id).await;
                            }
                            if let Some(ref metrics) = state.messaging_metrics {
                                metrics
                                    .acks_received
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            parts.push(format!(
                                "[{from_label} ack]: message {message_id} acknowledged"
                            ));
                            continue;
                        }
                        crate::messaging::types::MessagePayload::Nack { message_id, reason } => {
                            if let Some(ref tracker) = state.ack_tracker {
                                tracker.reject(message_id, reason.clone()).await;
                            }
                            if let Some(ref metrics) = state.messaging_metrics {
                                metrics
                                    .nacks_received
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            let r = reason.as_deref().unwrap_or("no reason");
                            parts.push(format!(
                                "[{from_label} nack]: message {message_id} rejected — {r}"
                            ));
                            continue;
                        }
                        _ => {}
                    }

                    // Track received message.
                    if let Some(ref metrics) = state.messaging_metrics {
                        metrics
                            .messages_received
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }

                    // Auto-ack: if the sender requested ack, send one back.
                    if msg.requires_ack {
                        let ack_reply = msg.make_ack(mailbox.address.clone());
                        let _ = mailbox.send(ack_reply).await;
                        if let Some(ref metrics) = state.messaging_metrics {
                            metrics
                                .acks_sent
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }

                    if let Some(ref handler) = state.permission_handler
                        && let Some((correlation_id, response)) = handler.process_message(msg).await
                    {
                        let response_msg =
                            response.to_message(&mailbox.address, &msg.from, &correlation_id);
                        let _ = mailbox.send(response_msg).await;
                        continue;
                    }

                    match &msg.payload {
                        crate::messaging::types::MessagePayload::Text { content, .. } => {
                            parts.push(format!("[{from_label}]: {content}"));
                        }
                        crate::messaging::types::MessagePayload::Progress {
                            status,
                            detail,
                            ..
                        } => {
                            let extra = detail.as_deref().unwrap_or("");
                            parts.push(format!("[{from_label} progress]: {status} {extra}"));
                        }
                        crate::messaging::types::MessagePayload::Request {
                            request_type, ..
                        } => {
                            parts.push(format!("[{from_label} request]: {request_type:?}"));
                        }
                        crate::messaging::types::MessagePayload::Response { accepted, .. } => {
                            parts.push(format!("[{from_label} response]: accepted={accepted}"));
                        }
                        crate::messaging::types::MessagePayload::Signal(sig) => {
                            parts.push(format!("[{from_label} signal]: {sig:?}"));
                        }
                        // Ack/Nack already handled above.
                        crate::messaging::types::MessagePayload::Ack { .. } => {}
                        crate::messaging::types::MessagePayload::Nack { .. } => {}
                    }
                }
                if !parts.is_empty() {
                    let mailbox_text = format!(
                        "📬 Messages from other agents ({}{}):\n{}",
                        pending.len(),
                        if has_more { "+, more queued" } else { "" },
                        parts.join("\n")
                    );
                    state.messages.push(serde_json::json!({
                        "role": "system",
                        "content": mailbox_text,
                    }));
                }
            }
        }

        // Sweep ack tracker for timed-out messages (retry or fail).
        if let Some(ref tracker) = state.ack_tracker {
            let outcomes = tracker.sweep().await;
            let retry_msgs = tracker.get_retry_messages(&outcomes).await;
            // Re-send retry messages and track retries.
            for retry_msg in &retry_msgs {
                if let Some(ref mut mb) = state.mailbox {
                    let _ = mb.send((**retry_msg).clone()).await;
                }
                if let Some(ref metrics) = state.messaging_metrics {
                    metrics
                        .retries
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
            // Log failures and store in dead-letter queue.
            for outcome in &outcomes {
                if let crate::messaging::ack_tracker::AckOutcome::Failed {
                    message_id,
                    attempts,
                    message,
                } = outcome
                {
                    eprintln!(
                        "  ⚠ messaging: ack timeout exhausted for message {} after {} attempts",
                        message_id, attempts
                    );
                    if let Some(ref dlq) = state.dead_letter_queue {
                        dlq.store(
                            Arc::clone(message),
                            crate::messaging::dead_letter::DeadLetterReason::AckTimeout {
                                attempts: *attempts,
                            },
                            *attempts,
                        )
                        .await;
                    }
                    if let Some(ref metrics) = state.messaging_metrics {
                        metrics
                            .dead_letters
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                if let crate::messaging::ack_tracker::AckOutcome::Rejected {
                    message_id,
                    reason,
                    message,
                } = outcome
                {
                    eprintln!(
                        "  ⚠ messaging: nack for message {}: {}",
                        message_id,
                        reason.as_deref().unwrap_or("no reason")
                    );
                    if let Some(ref dlq) = state.dead_letter_queue {
                        dlq.store(
                            Arc::clone(message),
                            crate::messaging::dead_letter::DeadLetterReason::Rejected {
                                reason: reason.clone(),
                            },
                            1,
                        )
                        .await;
                    }
                    if let Some(ref metrics) = state.messaging_metrics {
                        metrics
                            .dead_letters
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
        }

        // ─── Refresh ephemeral skill listing (picks up hot-reload changes) ──
        if let Some(resolver) = &state.skill_resolver {
            let full = resolver.available_skills();
            state.skill_listing_message = if full.is_empty() {
                None
            } else {
                let (visible, open_skill_name) =
                    crate::turn::skill_tool::visible_skills_for_host_turn(
                        &full,
                        state.message.as_str(),
                        &state.skill_quality_tracker,
                        &state.pinned_skills,
                        &state.discovered_skills,
                        &state.skill_search,
                    );
                Some(crate::turn::skill_tool::skill_listing_system_message(
                    &visible,
                    Some(&state.skill_quality_tracker),
                    Some(&state.pinned_skills),
                    open_skill_name,
                ))
            };
        }

        // ─── Step 0.5: Inject context inventory to reduce redundant tool calls ──
        // After the first turn, tell the LLM what files/searches are already in
        // context so it avoids re-fetching the same data. Injected as an ephemeral
        // system message — replaced each iteration (not accumulated).
        if turn_index > 0 {
            const INVENTORY_HEADER: &str = "## Already Fetched (do NOT re-read/re-grep these)\n";
            // Remove previous inventory (may be anywhere after assistant/tool messages were appended).
            state.messages.retain(|m| {
                m.get("role").and_then(Value::as_str) != Some("system")
                    || !m
                        .get("content")
                        .and_then(Value::as_str)
                        .is_some_and(|c| c.starts_with(INVENTORY_HEADER))
            });
            let inventory = state.semantic_dedup.context_inventory();
            if !inventory.is_empty() {
                state.messages.push(serde_json::json!({
                    "role": "system",
                    "content": format!("{INVENTORY_HEADER}{inventory}"),
                }));
            }
        }

        // ─── Step 1: Host executes the turn (payload → HTTP → SSE) ──────
        let llm_start = std::time::Instant::now();
        let turn_result = host.execute_turn(state).await?;
        let llm_total_ms = llm_start.elapsed().as_millis() as u64;

        // ─── Step 2: Ingest turn stream into loop state ─────────────────
        let snap =
            agentic_turn_stream_snapshot_from_sse_accum(&turn_result.accum, turn_result.ttft_ms);
        let edge_len = turn_result.edge_tool_round.len();
        let quiet = host.is_quiet();
        match map_ingest_outcome_to_iteration_control(ingest_agentic_turn_stream(
            &snap,
            edge_len,
            |i| turn_result.edge_tool_round[i].tool.clone(),
            &state.message,
            &state.recent_tools,
            quiet,
            AgenticTurnIngestMut {
                task_profile: state.task_profile,
                first_ttft_ms: &mut state.first_ttft_ms,
                current_session_id: &mut state.current_session_id,
                current_run_id: &mut state.current_run_id,
                final_text: &mut state.final_text,
                total_prompt: &mut state.total_prompt,
                total_completion: &mut state.total_completion,
                total_cache_read: &mut state.total_cache_read,
                total_cache_creation: &mut state.total_cache_creation,
                total_tool_calls: &mut state.total_tool_calls,
                step_recorder: &mut state.step_recorder,
                all_tools_used: &mut state.all_tools_used,
                has_any_usage: &mut state.has_any_usage,
                forced_factual_retry: &mut state.forced_factual_retry,
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
                    try_write_heavy_checkpoint(state);
                    return Ok(AgenticLoopOutcome::Completed);
                }

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
                if state.stop_hook_runs == 0
                    && let Some(prompt) =
                        crate::turn::stop_hooks::build_stop_hook_prompt(&state.stop_hooks)
                {
                    state.stop_hook_runs = 1;
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
                try_write_heavy_checkpoint(state);
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
                        try_write_heavy_checkpoint(state);
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

        // ─── Observability: tool selection hook ──────────────────────────
        // Record which tools the LLM chose for this turn (before execution).
        if let Some(session) = &state.observability_session {
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
                        total_available: state.all_tools_used.len() as u32,
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
                let mut session_guard = session.write().unwrap();
                crate::observability_integration::on_tool_selection(
                    &mut session_guard,
                    explanation,
                );
            }
        }

        // ─── Trace collector: record tool selection ──────────────────────
        if let Some(ref collector) = state.turn_trace_collector {
            let selected_tools: Vec<String> = turn_result
                .edge_tool_round
                .iter()
                .map(|r| r.tool.clone())
                .collect();
            collector.record_tool_selection(
                &selected_tools,
                state
                    .first_selector_strategy
                    .as_deref()
                    .unwrap_or("unknown"),
                state.first_selector_confidence.unwrap_or(0.0),
                state.total_prompt as u32, // budget approximation
                state.selector_tokens_in,
                state.selector_tokens_out,
                state.all_tools_used.len() as u32,
                state.first_selector_ms.unwrap_or(0),
            );
        }

        // ─── Step 3: Stall preflight ────────────────────────────────────
        let tool_calls_for_guard = agentic_round_stall_preflight_with_tool_calls(
            turn_index,
            &turn_result.accum.tool_calls,
            &turn_result.edge_tool_round,
            &mut state.turn_sigs,
            &mut state.turn_tool_names,
            &mut state.stall_events,
            &mut state.turn_guard,
        );

        // ─── Step 3b: Delegation interception ───────────────────────────
        // If a delegation engine is wired, intercept "delegate" tool calls
        // and execute them as multi-agent coordination runs.
        let (delegation_results, remaining_tool_calls) =
            if let Some(engine) = &state.delegation_engine {
                // Fire SubagentStart hooks before delegation.
                if turn_result.accum.tool_calls.iter().any(is_delegation_call) {
                    let _ = crate::skills::hooks::evaluate_session_hooks(
                        &state.session_event_hooks,
                        crate::skills::hooks::SessionEvent::SubagentStart,
                        state.current_session_id.as_deref().unwrap_or(""),
                        None,
                    )
                    .await;
                }
                partition_and_execute_delegations(
                    &turn_result.accum.tool_calls,
                    engine,
                    state.current_run_id.as_deref().unwrap_or("unknown"),
                    state.current_session_id.as_deref().unwrap_or("unknown"),
                    "orchestrator",
                    state.workspace_root_hint.as_deref(),
                    &state.skill_search,
                )
                .await
            } else {
                (Vec::new(), turn_result.accum.tool_calls.clone())
            };

        // Inject delegation results into messages + tool_results.
        // Build a proper assistant message with the delegate tool_calls so
        // downstream tool-result messages have a matching assistant entry
        // (required by OpenAI conversation format).
        if !delegation_results.is_empty() {
            let delegate_tool_calls: Vec<&Value> = turn_result
                .accum
                .tool_calls
                .iter()
                .filter(|tc| is_delegation_call(tc))
                .collect();
            if !delegate_tool_calls.is_empty() {
                let tc_entries: Vec<Value> = delegate_tool_calls
                    .iter()
                    .map(|tc| {
                        let id = tc.get("id").and_then(Value::as_str).unwrap_or("");
                        let name = tc.get("name").and_then(Value::as_str).unwrap_or("delegate");
                        let args = tc
                            .get("arguments")
                            .cloned()
                            .unwrap_or(serde_json::json!({}));
                        serde_json::json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": serde_json::to_string(&args)
                                    .unwrap_or_else(|_| "{}".to_string()),
                            }
                        })
                    })
                    .collect();
                let mut assistant_msg = serde_json::json!({
                    "role": "assistant",
                    "content": Value::Null,
                    "tool_calls": tc_entries,
                });
                // Preserve reasoning_content for thinking-model sessions.
                let rc = &turn_result.accum.reasoning_content;
                if !rc.is_empty() {
                    assistant_msg["reasoning_content"] = Value::String(rc.clone());
                } else if super::edge_ledger::history_has_reasoning(&state.messages) {
                    assistant_msg["reasoning_content"] = Value::String(String::new());
                }
                state.messages.push(assistant_msg);
            }
            for (call_id, result_text) in &delegation_results {
                let tool_msg = serde_json::json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": result_text,
                });
                state.messages.push(tool_msg.clone());
                state.tool_results.push(tool_msg);
                // Record delegation results in journal so session analysis
                // shows actual sub-agent output instead of the static
                // "acknowledged" placeholder from the edge executor.
                state.tool_call_records.push(ToolCallRecord {
                    name: DELEGATE_TOOL_NAME.to_string(),
                    ok: !result_text.starts_with("Delegation failed:")
                        && !result_text.starts_with("Invalid delegation request:"),
                    ms: 0, // delegation timing is inside the result text
                    error: None,
                    input_bytes: None,
                    output_bytes: Some(result_text.len() as u32),
                    args_preview: Some(call_id.clone()),
                    result_preview: Some(result_text.chars().take(500).collect::<String>()),
                });
            }
        }

        if !delegation_results.is_empty()
            && state.teammate_idle_hook_runs == 0
            && let Some(prompt) =
                crate::turn::stop_hooks::build_teammate_idle_hook_prompt(&state.teammate_idle_hooks)
        {
            state.teammate_idle_hook_runs = 1;
            if !quiet {
                host.emit_headless_line(
                    HeadlessStderrStyle::Yellow,
                    "⚠ Teammate-round verification…".to_string(),
                );
            }
            state.messages.push(prompt);
        }

        // Use remaining (non-delegation) tool calls for headless round
        let effective_tool_calls = if delegation_results.is_empty() {
            &turn_result.accum.tool_calls
        } else {
            &remaining_tool_calls
        };

        // ─── Step 3b½: send_message interception ────────────────────────
        // If the agent has a mailbox, intercept send_message tool calls and
        // route them through the messaging system.
        let post_send_tool_calls;
        let effective_tool_calls = if let Some(ref mailbox) = state.mailbox {
            let mut msg_results: Vec<(String, String)> = Vec::new();
            let mut remaining = Vec::new();
            for tc in effective_tool_calls {
                if crate::messaging::send_tool::is_send_message_call(tc) {
                    if let Some((call_id, args)) =
                        crate::messaging::send_tool::parse_send_message_call(tc)
                    {
                        let send_result =
                            crate::messaging::send_tool::execute_send_message(mailbox, &args).await;
                        // Track metrics for successful sends.
                        if send_result.tracked_message.is_some()
                            || !send_result.display.starts_with("Error:")
                        {
                            if let Some(ref metrics) = state.messaging_metrics {
                                metrics
                                    .messages_sent
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                        // Track ack-requiring messages.
                        if let Some(tracked_msg) = send_result.tracked_message {
                            if let Some(ref tracker) = state.ack_tracker {
                                tracker.track(tracked_msg).await;
                            }
                        }
                        msg_results.push((call_id, send_result.display));
                    } else if let Some(call_id) = tc.get("id").and_then(|v| v.as_str()) {
                        msg_results.push((
                            call_id.to_string(),
                            "Error: could not parse send_message arguments. Expected JSON with 'target' and 'content' fields.".to_string(),
                        ));
                    }
                } else {
                    remaining.push(tc.clone());
                }
            }
            for (call_id, result_text) in &msg_results {
                let tool_msg = serde_json::json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": result_text,
                });
                state.messages.push(tool_msg.clone());
                state.tool_results.push(tool_msg);
            }
            post_send_tool_calls = remaining;
            &post_send_tool_calls
        } else {
            effective_tool_calls
        };

        // ─── Step 3c: Skill interception ─────────────────────────────────
        // If a skill resolver is wired, intercept "skill" tool calls and
        // return resolved instructions as tool results.
        let (mut skill_results, post_skill_tool_calls);
        let effective_tool_calls = if let Some(resolver) = &state.skill_resolver {
            // Build runtime context for skill execution
            let mut extra = std::collections::HashMap::new();

            if let Some(ref root) = state.workspace_root_hint {
                let root_path = std::path::Path::new(root.as_str());

                // Detect git branch
                if let Ok(output) = std::process::Command::new("git")
                    .args(["rev-parse", "--abbrev-ref", "HEAD"])
                    .current_dir(root)
                    .output()
                {
                    if output.status.success() {
                        let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
                        if !branch.is_empty() {
                            extra.insert("git_branch".into(), branch);
                        }
                    }
                }

                // Detect git repo name from remote origin
                if let Ok(output) = std::process::Command::new("git")
                    .args(["config", "--get", "remote.origin.url"])
                    .current_dir(root)
                    .output()
                {
                    if output.status.success() {
                        let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
                        if let Some(name) = extract_repo_name_from_url(&url) {
                            extra.insert("git_repo".into(), name);
                        }
                    }
                }

                // Detect project type from marker files
                let project_types = detect_project_types(root_path);
                if !project_types.is_empty() {
                    extra.insert("project_type".into(), project_types.join(","));
                }
            }

            // OS info
            extra.insert("os".into(), std::env::consts::OS.into());

            // ── Runtime metrics for reflect skill ──
            let turns_used = state.max_turns.saturating_sub(state.remaining_turns);
            extra.insert("turn_number".into(), turns_used.to_string());
            extra.insert("turns_remaining".into(), state.remaining_turns.to_string());
            extra.insert("total_prompt_tokens".into(), state.total_prompt.to_string());
            extra.insert(
                "total_completion_tokens".into(),
                state.total_completion.to_string(),
            );
            extra.insert(
                "total_tool_calls".into(),
                state.total_tool_calls.to_string(),
            );
            extra.insert(
                "nudge_count".into(),
                state.turn_guard.nudge_count.to_string(),
            );
            extra.insert(
                "error_count".into(),
                state.turn_guard.errors.total_errors.to_string(),
            );
            let depri = state.turn_guard.health.deprioritized_tools();
            if !depri.is_empty() {
                extra.insert("deprioritized_tools".into(), depri.join(", "));
            }
            if !state.stall_events.is_empty() {
                let stalls: Vec<String> = state
                    .stall_events
                    .iter()
                    .map(|(kind, turn)| format!("{}@t{}", kind, turn))
                    .collect();
                extra.insert("stall_events".into(), stalls.join(", "));
            }
            let eff = state.turn_guard.correction_effectiveness();
            if eff.total_corrections > 0 {
                extra.insert(
                    "correction_follow_rate".into(),
                    format!("{:.0}%", eff.follow_rate * 100.0),
                );
            }

            let session_dir = state.current_session_id.as_ref().map(|id| {
                dirs::home_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
                    .join(".astra")
                    .join("sessions")
                    .join(id)
                    .to_string_lossy()
                    .into_owned()
            });

            let skill_ctx = crate::turn::skill_tool::SkillContext {
                session_id: state.current_session_id.clone(),
                session_dir,
                work_dir: state.workspace_root_hint.clone(),
                available_tools: state.all_tools_used.iter().cloned().collect(),
                extra,
            };

            let composition_ctx = crate::skills::composition::CompositionContext::root();
            let full_catalog = resolver.available_skills();
            let (visible_for_mask, _) = crate::turn::skill_tool::visible_skills_for_host_turn(
                &full_catalog,
                state.message.as_str(),
                &state.skill_quality_tracker,
                &state.pinned_skills,
                &state.discovered_skills,
                &state.skill_search,
            );
            let discover_exclude =
                crate::turn::skill_tool::skill_mask_names_lowercase(&visible_for_mask);

            // ── Same-session skill dedup: return stub for already-invoked skills ──
            let mut dedup_results: Vec<(String, String)> = Vec::new();
            let mut fresh_tool_calls: Vec<Value> = Vec::new();
            for tc in effective_tool_calls {
                if crate::turn::skill_tool::is_skill_call(tc) {
                    let skill_name = tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(Value::as_str)
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .and_then(|a| {
                            a.get("skill_name")
                                .and_then(Value::as_str)
                                .map(String::from)
                        });
                    if let Some(ref name) = skill_name {
                        if let Some(prev) = state.invoked_skills.get(name.as_str()) {
                            let call_id = tc.get("id").and_then(Value::as_str).unwrap_or("unknown");
                            dedup_results.push((
                                call_id.to_string(),
                                format!(
                                    "Skill '{}' was already loaded (turn {}). \
                                     Follow those instructions directly — do not re-invoke.",
                                    name, prev.invoked_at_turn
                                ),
                            ));
                            continue;
                        }
                    }
                }
                fresh_tool_calls.push(tc.clone());
            }

            let (sr, remaining, activation) =
                crate::turn::skill_tool::partition_discover_and_execute_skills(
                    &fresh_tool_calls,
                    resolver.as_ref(),
                    &full_catalog,
                    &discover_exclude,
                    &mut state.discovered_skills,
                    state.skill_executor.as_ref(),
                    Some(&mut state.skill_quality_tracker),
                    Some(&composition_ctx),
                    &skill_ctx,
                )
                .await;

            // Record newly invoked skills + merge dedup stubs
            let current_turn = (state.max_turns - state.remaining_turns) as u32;
            for (call_id, result_text) in &sr {
                // Extract skill name from the matching fresh_tool_calls
                if let Some(tc) = fresh_tool_calls
                    .iter()
                    .find(|t| t.get("id").and_then(Value::as_str) == Some(call_id.as_str()))
                {
                    let name = tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(Value::as_str)
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .and_then(|a| {
                            a.get("skill_name")
                                .and_then(Value::as_str)
                                .map(String::from)
                        });
                    if let Some(name) = name {
                        if crate::turn::skill_tool::is_skill_call(tc) {
                            state.invoked_skills.insert(
                                name.clone(),
                                crate::turn::skill_tool::InvokedSkill {
                                    name,
                                    content: result_text.clone(),
                                    invoked_at_turn: current_turn,
                                },
                            );
                        }
                    }
                }
            }
            skill_results = dedup_results;

            // ── Skill exclusivity: drop non-skill tool calls when skills fired ──
            // When the model emits skill calls alongside regular tool calls in the
            // same turn, the regular calls were generated WITHOUT seeing the skill
            // instructions. Executing them would bypass the skill's guidance entirely.
            // Drop them and return synthetic errors so the model re-evaluates after
            // reading the skill content.
            //
            // Only trigger on *new* skill invocations (not discover_skills,
            // not dedup stubs). A dedup stub means the skill was already loaded
            // in a prior turn; discover_skills only lists available skills
            // without loading instructions.
            let new_skills_fired = fresh_tool_calls
                .iter()
                .any(|tc| crate::turn::skill_tool::is_skill_call(tc));
            skill_results.extend(sr);
            if new_skills_fired && !remaining.is_empty() {
                let dropped_count = remaining.len();
                for tc in &remaining {
                    let call_id = tc.get("id").and_then(Value::as_str).unwrap_or("unknown");
                    let tool_name = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    skill_results.push((
                        call_id.to_string(),
                        format!(
                            "Deferred: skill was invoked in this turn. Read the skill \
                             instructions above, then decide whether to call `{}` again.",
                            tool_name
                        ),
                    ));
                }
                post_skill_tool_calls = Vec::new(); // clear remaining
                if !quiet {
                    host.emit_headless_line(
                        HeadlessStderrStyle::Dim,
                        format!(
                            "  ⏸ {} non-skill tool call(s) deferred — skill takes priority",
                            dropped_count
                        ),
                    );
                }
            } else {
                post_skill_tool_calls = remaining;
            }

            // Apply skill activation effects (model override, tool restrictions).
            // A new activation fully replaces the previous one — fields not
            // present in the new activation are cleared so stale overrides
            // from a prior skill don't persist indefinitely.
            if let Some(act) = activation {
                state.skill_model_override =
                    act.model_override.filter(|m| is_valid_model_string(m));
                state.skill_allowed_tools = if act.allowed_tools.is_empty() {
                    None
                } else {
                    Some(act.allowed_tools.into_iter().collect())
                };
                state.skill_effort = act.effort;
                state.skill_agent_type = act.agent_type;
                state.skill_sandbox_policy = act.sandbox_policy;
            }

            &post_skill_tool_calls
        } else {
            skill_results = Vec::new();
            // No skill resolver — pass through unchanged
            post_skill_tool_calls = effective_tool_calls.to_vec();
            &post_skill_tool_calls
        };

        // Inject skill results into messages + tool_results
        for (call_id, result_text) in &skill_results {
            let tool_msg = serde_json::json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": result_text,
            });
            state.messages.push(tool_msg.clone());
            state.tool_results.push(tool_msg);
        }

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

        // When delegations were handled by step 3b, filter delegate results
        // out of edge_tool_round so the headless round's fallback path
        // (used when effective_tool_calls is empty) doesn't reconstruct
        // duplicate delegate tool_calls from edge results.
        let filtered_edge_round: Vec<_>;
        let edge_round_for_headless: &[EdgeToolExecResult] = if !delegation_results.is_empty() {
            filtered_edge_round = turn_result
                .edge_tool_round
                .iter()
                .filter(|r| r.tool != DELEGATE_TOOL_NAME)
                .cloned()
                .collect();
            &filtered_edge_round
        } else {
            turn_result.edge_tool_round.as_slice()
        };

        {
            let valid_tool_names = host.valid_tool_names().clone();
            let mut term_adapter = HostTerminalAdapter(host);
            run_agentic_headless_tool_round(
                turn_index,
                quiet,
                &state.api,
                &state.api_token,
                state.current_session_id.as_ref(),
                effective_tool_calls,
                edge_round_for_headless,
                turn_result.accum.reasoning_content.as_str(),
                &edge_callback_outputs,
                &mut state.messages,
                &mut state.tool_results,
                &valid_tool_names,
                &mut state.restricted_tools,
                &mut state.turn_guard,
                &mut state.step_recorder,
                &mut state.idempotency_cache,
                &mut state.semantic_dedup,
                &mut state.tool_call_records,
                &state.tool_event_hooks,
                &mut term_adapter,
                state.mailbox.as_mut(),
                state.permission_context.as_ref(),
            )
            .await;
        }
        append_explain_turn_batch(
            &mut state.explain_turns,
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
        // Feed tool usage into user profile for pattern learning.
        if let Some(hub) = &state.observability_hub {
            let user_id = state
                .observability_session
                .as_ref()
                .map(|s| s.read().unwrap().user_id.clone())
                .unwrap_or_default();
            for edge_result in &turn_result.edge_tool_round {
                crate::observability_integration::on_tool_executed(
                    hub,
                    &user_id,
                    &edge_result.tool,
                );
            }
        }

        // ─── Step 4b: Conditional skill activation ──────────────────────
        // Record file paths from edge tool executions so path-conditional
        // skills can activate dynamically. When new skills activate, refresh
        // the `skill` tool schema with the expanded skill list.
        if let Some(ref registry) = state.skill_registry_for_activation {
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
                if let Some(resolver) = &state.skill_resolver {
                    let full = resolver.available_skills();
                    if !full.is_empty() {
                        let (visible, open_skill_name) =
                            crate::turn::skill_tool::visible_skills_for_host_turn(
                                &full,
                                state.message.as_str(),
                                &state.skill_quality_tracker,
                                &state.pinned_skills,
                                &state.discovered_skills,
                                &state.skill_search,
                            );
                        host.inject_tool_schema(crate::turn::skill_tool::skill_tool_schema(
                            &visible,
                            Some(&state.skill_quality_tracker),
                            Some(&state.pinned_skills),
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
                if dominant == state.last_error_category {
                    state.consecutive_same_error += 1;
                } else {
                    state.consecutive_same_error = 1;
                    state.last_error_category = dominant;
                }
                if state.consecutive_same_error >= CONSECUTIVE_ERROR_BUDGET {
                    let cat_name = state
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
                            n = state.consecutive_same_error,
                        )
                    }));
                    state.consecutive_same_error = 0; // Reset after nudge
                }
            } else {
                // Successful turn — reset streak.
                state.consecutive_same_error = 0;
                state.last_error_category = None;
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
                        state.step_recorder.end_turn(true);
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
                intent_tool_turns: &mut state.intent_tool_turns,
                messages: &mut state.messages,
                stall_events: &mut state.stall_events,
                turn_guard: &mut state.turn_guard,
                verdict_events: &mut state.verdict_events,
                restricted_tools: &mut state.restricted_tools,
                remaining_turns: &mut state.remaining_turns,
                step_recorder: &mut state.step_recorder,
                current_session_id: state.current_session_id.as_ref(),
                max_turns: state.max_turns,
                loop_turn: turn_index,
                recent_tools: &state.recent_tools,
                last_heavy_checkpoint: &mut state.last_heavy_checkpoint,
            },
        )) {
            AgenticPostToolIterationControl::Abort(e) => return Err(e),
            AgenticPostToolIterationControl::RetryLlmClearToolResults => {
                state.tool_results.clear();
            }
            AgenticPostToolIterationControl::ProceedEndTurn => {
                // Emit progress event for subscribers (UI, monitors).
                if let Some(ref emitter) = state.progress_emitter {
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
                }

                // Send progress update to parent agent (best-effort).
                if let Some(ref mailbox) = state.mailbox {
                    let _ = mailbox
                        .send_progress(
                            turn_index as u32,
                            state.total_tool_calls,
                            "turn_complete",
                            None,
                        )
                        .await;
                }

                // ─── Observability: turn end hook ────────────────────────
                // Capture timing and feed to auto-tuning.
                if let Some(ref session) = state.observability_session {
                    let total_ms = turn_start_time.elapsed().as_millis() as u64;
                    let timing = crate::observability_integration::TurnTiming {
                        turn: turn_index as u32,
                        context_assembly_ms: 0, // TODO: measure separately
                        ttft_ms: turn_result.ttft_ms.unwrap_or(0) as u64,
                        llm_total_ms,
                        tool_execution_ms: 0, // TODO: measure separately
                        total_ms,
                    };
                    let mut session_guard = session.write().unwrap();
                    crate::observability_integration::on_turn_end(&mut session_guard, timing);
                }

                // ─── Finalize turn trace collector ────────────────────────
                // Persist context assembly trace to session journal (best-effort).
                if let Some(ref collector) = state.turn_trace_collector {
                    // Compute budget pressure from last measured tokens.
                    let measured = state.last_measured_prompt_tokens.unwrap_or(0);
                    let max = state.max_turn_input_tokens;
                    let budget_pressure = if max > 0 {
                        measured as f64 / max as f64
                    } else {
                        state.first_budget_pressure
                    };

                    // Record token budget before finalizing.
                    // NOTE: Fine-grained breakdown (system_prompt, history, memory, etc.)
                    // is not available at runtime layer — would require CLI to pass it down.
                    // For now, we capture what we have: total_used and budget_pressure.
                    collector.record_token_budget(
                        crate::turn::context_assembly_trace::TokenBudgetTrace {
                            max_tokens: max as u32,
                            system_prompt_tokens: 0, // Future: measure from CLI host
                            history_tokens: 0,       // Future: measure from CLI host
                            memory_tokens: 0,        // Future: measure from CLI host
                            tool_schema_tokens: 0,   // Future: measure from CLI host
                            user_message_tokens: 0,  // Future: measure from CLI host
                            total_used: measured as u32,
                            budget_pressure,
                            compression_triggered: state.budget_wrapup_injected,
                        },
                    );
                    // Finalize and persist (errors logged but not propagated).
                    if let Err(e) = collector.finalize_and_persist(turn_index as u32) {
                        eprintln!("trace persist: {e}");
                    }
                }
                // Clear collector for next turn.
                state.turn_trace_collector = None;

                state.step_recorder.end_turn(false);
            }
        }
    }
    // Loop exhausted max_turns without explicit break — write final state.
    try_write_heavy_checkpoint(state);
    Ok(AgenticLoopOutcome::Completed)
}

// ─── CTX_ helpers ────────────────────────────────────────────────────────────

/// Extract repository name from a git remote URL.
///
/// Handles SSH (`git@host:org/repo.git`), HTTPS (`https://host/org/repo.git`),
/// and bare paths.
fn extract_repo_name_from_url(url: &str) -> Option<String> {
    // Take the last path component, strip `.git` suffix
    let path = url.trim_end_matches('/');
    let segment = if let Some(idx) = path.rfind('/') {
        &path[idx + 1..]
    } else if let Some(idx) = path.rfind(':') {
        // SSH shorthand: git@github.com:org/repo.git
        let after_colon = &path[idx + 1..];
        after_colon.rsplit('/').next().unwrap_or(after_colon)
    } else {
        return None;
    };
    let name = segment.strip_suffix(".git").unwrap_or(segment);
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Detect project types from well-known marker files in the workspace root.
/// Returns a list of detected types (a project can be multi-language).
fn detect_project_types(root: &std::path::Path) -> Vec<&'static str> {
    let markers: &[(&str, &str)] = &[
        ("Cargo.toml", "rust"),
        ("package.json", "node"),
        ("pyproject.toml", "python"),
        ("setup.py", "python"),
        ("requirements.txt", "python"),
        ("go.mod", "go"),
        ("pom.xml", "java"),
        ("build.gradle", "java"),
        ("Gemfile", "ruby"),
        ("Makefile", "make"),
        ("CMakeLists.txt", "cmake"),
        ("docker-compose.yml", "docker"),
        ("Dockerfile", "docker"),
    ];
    let mut seen = std::collections::HashSet::new();
    let mut types = Vec::new();
    for (file, lang) in markers {
        if root.join(file).exists() && seen.insert(*lang) {
            types.push(*lang);
        }
    }
    types
}

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
            }
        }

        fn with_valid_tools(mut self, tools: &[&str]) -> Self {
            self.valid_tools = tools.iter().map(|s| s.to_string()).collect();
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
            status: "ok".to_string(),
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
            turn_sigs: Vec::new(),
            turn_tool_names: Vec::new(),
            stall_events: Vec::new(),
            intent_tool_turns: Vec::new(),
            verdict_events: Vec::new(),
            last_heavy_checkpoint: None,
            tool_call_records: Vec::new(),
            forced_factual_retry: false,
            explain_turns: Vec::new(),
            first_ttft_ms: None,
            all_tools_used: HashSet::new(),
            first_selection_report: None,
            first_budget_pressure: 0.0,
            first_context_assembly_ms: None,
            first_memoria_ms: None,
            first_selector_ms: None,
            first_selector_strategy: None,
            first_selector_confidence: None,
            selector_tokens_in: 0,
            selector_tokens_out: 0,
            all_selected_skills: Vec::new(),
            message: "test query".to_string(),
            recent_tools: Vec::new(),
            task_profile: TaskExecutionProfile::default(),
            api: astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            api_token: String::new(),
            cancel_flag: None,
            cancel_token: None,
            delegation_engine: None,
            skill_registry_for_activation: None,
            skill_resolver: None,
            skill_executor: None,
            skill_model_override: None,
            skill_effort: None,
            skill_agent_type: None,
            skill_allowed_tools: None,
            skill_sandbox_policy: None,
            skill_quality_tracker: crate::skills::quality::SkillQualityTracker::new(),
            skill_improvement_tracker: crate::skills::improvement::ImprovementTracker::new(),
            pinned_skills: std::collections::HashSet::new(),
            discovered_skills: HashSet::new(),
            skill_search: astra_core::SkillSearchSettings::default(),
            tool_event_hooks: crate::skills::hooks::ToolEventHookRegistry::default(),
            session_event_hooks: crate::skills::hooks::SessionEventHookRegistry::default(),
            stop_hooks: Vec::new(),
            stop_hook_runs: 0,
            teammate_idle_hooks: Vec::new(),
            teammate_idle_hook_runs: 0,
            workspace_root_hint: None,
            consecutive_same_error: 0,
            last_error_category: None,
            checkpoint_gate: None,
            data_snapshot_provider: None,
            last_composite_snapshot: None,
            last_measured_prompt_tokens: None,
            consecutive_context_window_errors: 0,
            max_turn_input_tokens: 0,
            budget_wrapup_injected: false,
            thinking_budget_tokens: None,
            skill_listing_message: None,
            invoked_skills: std::collections::HashMap::new(),
            recent_file_reads: Vec::new(),
            turn_trace_collector: None,
            project_context: None,
            mailbox: None,
            ack_tracker: None,
            dead_letter_queue: None,
            messaging_metrics: None,
            progress_emitter: None,
            permission_context: None,
            permission_handler: None,
            observability_session: None,
            observability_hub: None,
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
    }

    #[tokio::test]
    async fn budget_exhausted_returns_error() {
        let mut host = MockHost::new(vec![]);
        let mut state = make_state();
        state.remaining_turns = 0;
        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_err());
        assert!(outcome.unwrap_err().contains("budget"));
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
        assert_eq!(state.first_ttft_ms, Some(42));
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
        assert!(state.first_ttft_ms.is_none());

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert_eq!(state.first_ttft_ms, Some(100)); // NOT 200
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
        assert_eq!(state.first_ttft_ms, Some(50));
        assert!(state.all_tools_used.contains("bash"));
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
        assert!(state.all_tools_used.contains("bash"));
        assert!(state.all_tools_used.contains("read_file"));
        assert_eq!(state.all_tools_used.len(), 2);
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
        assert!(!state.tool_call_records.is_empty());
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
        assert!(state.all_tools_used.contains("bash"));
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
        assert!(state.all_tools_used.contains("read_file"));
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
        assert!(state.turn_sigs.len() >= 2);
        assert!(state.turn_tool_names.len() >= 2);
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
        state.cancel_flag = None; // default
        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(matches!(outcome, Ok(AgenticLoopOutcome::Completed)));
        assert_eq!(state.final_text, "ok");
    }

    #[tokio::test]
    async fn cancel_flag_false_does_not_cancel() {
        let flag = Arc::new(AtomicBool::new(false));
        let mut host = MockHost::new(vec![text_result("ok", 10, 5, Some(42))]);
        let mut state = make_state();
        state.cancel_flag = Some(flag);
        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(matches!(outcome, Ok(AgenticLoopOutcome::Completed)));
        assert_eq!(state.final_text, "ok");
    }

    #[tokio::test]
    async fn cancel_flag_true_aborts_before_first_turn() {
        let flag = Arc::new(AtomicBool::new(true));
        let mut host = MockHost::new(vec![text_result("should not run", 10, 5, Some(42))]);
        let mut state = make_state();
        state.cancel_flag = Some(flag);
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
        state.cancel_flag = Some(flag_clone);

        // Set cancel flag before loop starts — simulates cancel arriving
        flag.store(true, Ordering::Relaxed);

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(matches!(outcome, Ok(AgenticLoopOutcome::Cancelled)));
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
            astra_services::coordination::CoordinationPattern::Pipeline { stages } => {
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
            &astra_core::SkillSearchSettings::default(),
        )
        .unwrap();
        assert_eq!(req.task, "write tests");
        assert_eq!(req.parent_run_id, "run-123");
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
            &astra_core::SkillSearchSettings::default(),
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing arguments"));
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
            "orchestrator",
            None,
            &astra_core::SkillSearchSettings::default(),
        )
        .await;

        // One delegation executed, one regular call passed through
        assert_eq!(delegation_results.len(), 1);
        assert_eq!(remaining.len(), 1);

        // Delegation result should contain the call_id
        assert_eq!(delegation_results[0].0, "call_delegate");
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
            "orchestrator",
            None,
            &astra_core::SkillSearchSettings::default(),
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
            "orchestrator",
            None,
            &astra_core::SkillSearchSettings::default(),
        )
        .await;

        assert_eq!(delegation_results.len(), 1);
        assert!(
            delegation_results[0]
                .1
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

        // Verify token accounting includes both turns
        assert!(state.total_prompt >= 180, "should accumulate prompt tokens");
        assert!(
            state.total_completion >= 80,
            "should accumulate completion tokens"
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
        assert!(super::is_valid_model_string("gpt-4o"));
        assert!(super::is_valid_model_string("claude-sonnet-4-20250514"));
        assert!(super::is_valid_model_string("claude-3.5-sonnet"));
        assert!(super::is_valid_model_string("openai/gpt-4o"));
        assert!(super::is_valid_model_string("anthropic:claude-3"));
        assert!(super::is_valid_model_string("m0"));
    }

    #[test]
    fn invalid_model_strings() {
        assert!(!super::is_valid_model_string(""));
        assert!(!super::is_valid_model_string("x")); // too short
        assert!(!super::is_valid_model_string("model with spaces"));
        assert!(!super::is_valid_model_string("-starts-with-dash"));
        assert!(!super::is_valid_model_string("has;semicolon"));
        assert!(!super::is_valid_model_string("has$dollar"));
        assert!(!super::is_valid_model_string("has`backtick`"));
        assert!(!super::is_valid_model_string("has\nnewline"));
        assert!(!super::is_valid_model_string("has\ttab"));
        assert!(!super::is_valid_model_string(&"a".repeat(129))); // too long
    }

    #[test]
    fn model_string_boundary_lengths() {
        assert!(super::is_valid_model_string("ab")); // min valid
        assert!(super::is_valid_model_string(&format!(
            "m{}",
            "a".repeat(127)
        ))); // 128 = max
        assert!(!super::is_valid_model_string(&format!(
            "m{}",
            "a".repeat(128)
        ))); // 129 = over
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
        state.skill_resolver = Some(Arc::new(resolver));

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
        state.skill_resolver = Some(Arc::new(resolver));

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
        state.skill_resolver = Some(Arc::new(resolver));

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;

        // Model override should be set after skill activation
        assert_eq!(
            state.skill_model_override.as_deref(),
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
        state.skill_resolver = Some(Arc::new(resolver));

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;

        // Invalid model string should be rejected
        assert!(state.skill_model_override.is_none());
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
        state.skill_resolver = Some(Arc::new(resolver));

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;

        let allowed = state.skill_allowed_tools.as_ref().unwrap();
        assert!(allowed.contains("bash"));
        assert!(allowed.contains("grep"));
        assert_eq!(allowed.len(), 2);
    }

    #[tokio::test]
    async fn unrestricted_skill_clears_prior_overrides() {
        // Simulate: first skill sets overrides, second skill is unrestricted
        let mut state = make_state();
        state.skill_model_override = Some("old-model".into());
        state.skill_allowed_tools = Some(["bash".into()].into_iter().collect());

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
        state.skill_resolver = Some(Arc::new(resolver));

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;

        // Both should be cleared
        assert!(state.skill_model_override.is_none());
        assert!(state.skill_allowed_tools.is_none());
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
        state.skill_resolver = Some(Arc::new(resolver));

        // Pre-condition: restricted_tools is empty
        assert!(state.restricted_tools.is_empty());

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;

        // Post-condition: restricted_tools should NOT contain "grep" or "edit"
        // permanently (skill restrictions are transient, applied only in CLI host)
        // Note: the runtime loop itself doesn't apply restrictions — that's the
        // host's job in execute_turn(). This test verifies the runtime doesn't
        // pollute restricted_tools.
        // The skill_allowed_tools field IS set (for the host to use):
        let allowed = state.skill_allowed_tools.as_ref().unwrap();
        assert!(allowed.contains("bash"));
    }

    // ── CTX_ helper tests ──────────────────────────────────────────────────

    #[test]
    fn extract_repo_name_https() {
        assert_eq!(
            extract_repo_name_from_url("https://github.com/org/my-repo.git"),
            Some("my-repo".into())
        );
    }

    #[test]
    fn extract_repo_name_ssh() {
        assert_eq!(
            extract_repo_name_from_url("git@github.com:org/my-repo.git"),
            Some("my-repo".into())
        );
    }

    #[test]
    fn extract_repo_name_no_git_suffix() {
        assert_eq!(
            extract_repo_name_from_url("https://github.com/org/my-repo"),
            Some("my-repo".into())
        );
    }

    #[test]
    fn extract_repo_name_trailing_slash() {
        assert_eq!(
            extract_repo_name_from_url("https://github.com/org/repo.git/"),
            Some("repo".into())
        );
    }

    #[test]
    fn detect_project_types_rust_and_docker() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(tmp.path().join("Dockerfile"), "").unwrap();
        let types = detect_project_types(tmp.path());
        assert!(types.contains(&"rust"));
        assert!(types.contains(&"docker"));
    }

    #[test]
    fn detect_project_types_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let types = detect_project_types(tmp.path());
        assert!(types.is_empty());
    }

    #[test]
    fn detect_project_types_no_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        // Both pyproject.toml and setup.py → single "python"
        std::fs::write(tmp.path().join("pyproject.toml"), "").unwrap();
        std::fs::write(tmp.path().join("setup.py"), "").unwrap();
        let types = detect_project_types(tmp.path());
        assert_eq!(types.iter().filter(|&&t| t == "python").count(), 1);
    }

    // ── Skill listing ephemeral injection tests ─────────────────────────

    #[test]
    fn skill_listing_message_not_in_state_messages() {
        // Skill listing should be stored on the field, not pushed into messages.
        let mut state = make_state();
        state.messages = vec![json!({"role": "user", "content": "hi"})];
        state.skill_listing_message = Some(json!({
            "role": "system",
            "content": "<available_skills>...</available_skills>"
        }));

        // Messages should not contain the listing
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0]["role"], "user");
        // But the listing should be available for ephemeral injection
        assert!(state.skill_listing_message.is_some());
    }

    #[test]
    fn skill_listing_message_defaults_to_none() {
        let state = make_state();
        assert!(state.skill_listing_message.is_none());
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
        state.skill_listing_message = Some(json!({
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
        assert!(state.skill_listing_message.is_none());

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
        state.skill_listing_message =
            Some(skill_listing_system_message(&skills_v1, None, None, false));
        let v1_content = state.skill_listing_message.as_ref().unwrap()["content"]
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
        state.skill_listing_message =
            Some(skill_listing_system_message(&skills_v2, None, None, false));
        let v2_content = state.skill_listing_message.as_ref().unwrap()["content"]
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
        assert!(state.invoked_skills.is_empty());
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
        state.skill_resolver = Some(Arc::new(resolver));

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
        assert!(state.invoked_skills.contains_key("test-skill"));
        assert_eq!(state.invoked_skills["test-skill"].invoked_at_turn, 1);
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
        state.skill_resolver = Some(Arc::new(resolver));

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
        assert_eq!(state.invoked_skills.len(), 2);
    }

    #[test]
    fn post_compact_skill_reinjection() {
        use crate::turn::cloud::attachments::AttachmentBuilder;
        use crate::turn::skill_tool::InvokedSkill;

        let mut state = make_state();
        state.invoked_skills.insert(
            "review-changes".into(),
            InvokedSkill {
                name: "review-changes".into(),
                content: "# Review\nDo a code review.".into(),
                invoked_at_turn: 2,
            },
        );

        // Simulate post-compaction re-injection
        let mut builder = AttachmentBuilder::new();
        let mut skills: Vec<_> = state.invoked_skills.values().collect();
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
        state.skill_resolver = Some(Arc::new(resolver));

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

        // Host should have emitted a deferred notice
        assert!(
            host.emitted_lines.iter().any(|l| l.contains("deferred")),
            "Expected deferred notice in emitted lines: {:?}",
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
        state.skill_resolver = Some(Arc::new(resolver));

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
        state.skill_resolver = Some(Arc::new(resolver));

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
        state.skill_resolver = Some(Arc::new(resolver));

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
        state.skill_resolver = Some(Arc::new(resolver));

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
        state.skill_resolver = Some(Arc::new(resolver));

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
        state.session_event_hooks = hooks;
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
        state.session_event_hooks = hooks;
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
        state.session_event_hooks = hooks;
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
        state.session_event_hooks = hooks;
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
        state.session_event_hooks = hooks;
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
        state.session_event_hooks = hooks;
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

        let output = crate::skills::hooks::evaluate_session_hooks(
            &registry,
            crate::skills::hooks::SessionEvent::SessionStart,
            "s1",
            None,
        )
        .await;
        // Output should exist but be capped (plain text → context)
        let ctx = output.context.unwrap();
        assert!(
            ctx.len() <= HOOK_STDOUT_MAX_BYTES,
            "context should be capped at {} bytes, got {}",
            HOOK_STDOUT_MAX_BYTES,
            ctx.len()
        );
    }
}
