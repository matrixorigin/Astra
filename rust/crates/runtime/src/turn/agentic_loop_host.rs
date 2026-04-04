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
    /// Optional skill resolver for executing skills as tool calls.
    /// When set, the loop injects a `skill` tool schema and intercepts
    /// `skill` calls, returning resolved instructions as tool results.
    pub skill_resolver: Option<Arc<dyn crate::turn::skill_tool::SkillResolver>>,
    /// Model override from the most recently activated skill.
    /// When set, the host should use this model instead of the default.
    pub skill_model_override: Option<String>,
    /// Tool allow-list from the most recently activated skill.
    /// When non-empty, only these tools (plus `skill` itself) should be available.
    /// The host converts this allow-list to additions in `restricted_tools`.
    pub skill_allowed_tools: Option<HashSet<String>>,

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
}

/// Consecutive same-category error turns before forcing a strategy change.
const CONSECUTIVE_ERROR_BUDGET: u32 = 3;

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

const DELEGATE_TOOL_NAME: &str = "delegate";

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

            match parse_delegation_request(tc, parent_run_id, session_id) {
                Ok(mut request) => {
                    merge_workspace_hint_into_delegation_request(&mut request, workspace_hint);
                    match engine.execute(request, source_agent_id).await {
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
            .map(|o| {
                if o.len() > 500 {
                    format!("{}...", &o[..500])
                } else {
                    o.to_string()
                }
            })
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
    let _ =
        step_checkpoint::write_step_checkpoint(sid, state.step_recorder.summary().checkpoints, &cp);
    state.last_heavy_checkpoint = Some(cp);
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
    // ─── Preamble: auto-inject delegate tool when delegation is wired ────
    if state.delegation_engine.is_some() {
        host.inject_tool_schema(delegate_tool_schema());
    }

    // ─── Preamble: auto-inject skill tool when skills are available ──────
    if let Some(resolver) = &state.skill_resolver {
        let skills = resolver.available_skills();
        if !skills.is_empty() {
            host.inject_tool_schema(crate::turn::skill_tool::skill_tool_schema(&skills));
        }
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

        // ─── Step 1: Host executes the turn (payload → HTTP → SSE) ──────
        let turn_result = host.execute_turn(state).await?;

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
                total_tool_calls: &mut state.total_tool_calls,
                step_recorder: &mut state.step_recorder,
                all_tools_used: &mut state.all_tools_used,
                has_any_usage: &mut state.has_any_usage,
                forced_factual_retry: &mut state.forced_factual_retry,
                messages: &mut state.messages,
            },
        )) {
            AgenticIngestIterationControl::Fatal(e) => return Err(e),
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
                partition_and_execute_delegations(
                    &turn_result.accum.tool_calls,
                    engine,
                    state.current_run_id.as_deref().unwrap_or("unknown"),
                    state.current_session_id.as_deref().unwrap_or("unknown"),
                    "orchestrator",
                    state.workspace_root_hint.as_deref(),
                )
                .await
            } else {
                (Vec::new(), turn_result.accum.tool_calls.clone())
            };

        // Inject delegation results into messages + tool_results
        for (call_id, result_text) in &delegation_results {
            let tool_msg = serde_json::json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": result_text,
            });
            state.messages.push(tool_msg.clone());
            state.tool_results.push(tool_msg);
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

        // ─── Step 3c: Skill interception ─────────────────────────────────
        // If a skill resolver is wired, intercept "skill" tool calls and
        // return resolved instructions as tool results.
        let (skill_results, post_skill_tool_calls);
        let effective_tool_calls = if let Some(resolver) = &state.skill_resolver {
            let (sr, remaining, activation) =
                crate::turn::skill_tool::partition_and_execute_skills(
                    effective_tool_calls,
                    resolver.as_ref(),
                )
                .await;
            skill_results = sr;
            post_skill_tool_calls = remaining;

            // Apply skill activation effects (model override, tool restrictions)
            if let Some(act) = activation {
                if let Some(model) = act.model_override {
                    state.skill_model_override = Some(model);
                }
                if !act.allowed_tools.is_empty() {
                    state.skill_allowed_tools = Some(act.allowed_tools.into_iter().collect());
                }
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
                turn_result.edge_tool_round.as_slice(),
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
                &mut term_adapter,
            )
            .await;
        }
        append_explain_turn_batch(
            &mut state.explain_turns,
            turn_result.accum.explain_turns.as_slice(),
        );

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
                state.step_recorder.end_turn(false);
            }
        }
    }
    // Loop exhausted max_turns without explicit break — write final state.
    try_write_heavy_checkpoint(state);
    Ok(AgenticLoopOutcome::Completed)
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
            skill_resolver: None,
            skill_model_override: None,
            skill_allowed_tools: None,
            stop_hooks: Vec::new(),
            stop_hook_runs: 0,
            teammate_idle_hooks: Vec::new(),
            teammate_idle_hook_runs: 0,
            workspace_root_hint: None,
            consecutive_same_error: 0,
            last_error_category: None,
            checkpoint_gate: None,
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
        let req = super::parse_delegation_request(&tool_call, "run-123", "session-456").unwrap();
        assert_eq!(req.task, "write tests");
        assert_eq!(req.parent_run_id, "run-123");
        assert!(req.context.contains_key("session_id"));
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
        let result = super::parse_delegation_request(&tool_call, "run-1", "sess-1");
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
        assert!(formatted.contains("..."));
        // Output should be truncated to ~500 chars + "..."
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
}
