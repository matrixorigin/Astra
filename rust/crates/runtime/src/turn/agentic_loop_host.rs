//! Runtime-portable agentic multi-turn loop.
//!
//! [`AgenticLoopHost`] abstracts all host-specific behavior (payload preparation,
//! HTTP posting, SSE consumption, terminal rendering) so the multi-turn loop
//! can run identically in CLI and headless cloud contexts.
//!
//! ```text
//! run_agentic_loop_with_host(host, state)
//!   for turn in 0..max_turns:
//!     host.execute_turn(&mut state) → HostTurnResult    ← CLI: payload + HTTP + SSE
//!     ingest_agentic_turn_stream(...)                    ← runtime
//!     agentic_round_stall_preflight_with_tool_calls(...) ← runtime
//!     run_agentic_headless_tool_round(...)               ← runtime
//!     apply_agentic_post_tool_policy(...)                ← runtime
//! ```

use std::collections::{BTreeSet, HashMap, HashSet};

use async_trait::async_trait;
use mo_agent_services::session_journal::ToolCallRecord;
use serde_json::Value;

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
use crate::turn::chat_turn_sse_dispatch::ChatTurnSseAccum;
use crate::turn::sse_stream_host::EdgeToolExecResult;
use crate::turn::stall::CLI_AGENTIC_TURN_BUDGET_STALL_ABORT_MSG;
use crate::turn::tool_result_semantics::tool_dedup_signature;
use crate::turn::turn_guard::TurnGuard;

// ─── Host turn result ────────────────────────────────────────────────────────

/// Result from one host-executed turn (payload prep + HTTP + SSE consumption).
pub struct HostTurnResult {
    /// Protocol accumulator from SSE stream.
    pub accum: ChatTurnSseAccum,
    /// Time to first token (ms).
    pub ttft_ms: Option<u64>,
    /// Edge tool outputs keyed by dedup signature.
    pub edge_callback_outputs: HashMap<String, String>,
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

    // ── API context (for cloud tool delivery) ──
    pub api: mo_thin_client::ThinClient,
    pub api_token: String,
}

// ─── Loop exit ───────────────────────────────────────────────────────────────

/// Result of running the agentic loop to completion.
#[derive(Debug)]
pub enum AgenticLoopOutcome {
    /// Loop completed normally (final text produced or budget exhausted gracefully).
    Completed,
    /// Loop aborted due to a fatal error.
    Error(String),
}

// ─── Runtime loop ────────────────────────────────────────────────────────────

/// Run the multi-turn agentic loop using the provided host.
///
/// This is the runtime-portable entry point. The host handles all
/// CLI/server-specific behavior; the runtime handles cognitive decisions:
/// turn ingest, stall detection, tool round orchestration, post-tool policy.
pub async fn run_agentic_loop_with_host<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) -> Result<AgenticLoopOutcome, String> {
    for turn_index in 0..state.max_turns {
        if state.remaining_turns == 0 {
            return Err(CLI_AGENTIC_TURN_BUDGET_STALL_ABORT_MSG.to_string());
        }
        state.remaining_turns = state.remaining_turns.saturating_sub(1);
        state.step_recorder.begin_turn(turn_index as u32);

        // ─── Step 1: Host executes the turn (payload → HTTP → SSE) ──────
        let turn_result = host.execute_turn(state).await?;

        // ─── Step 2: Ingest turn stream into loop state ─────────────────
        let snap = agentic_turn_stream_snapshot_from_sse_accum(
            &turn_result.accum,
            turn_result.ttft_ms,
        );
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
                return Ok(AgenticLoopOutcome::Completed);
            }
            AgenticIngestIterationControl::ContinueIterating => {
                return Ok(AgenticLoopOutcome::Completed);
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

        // ─── Step 4: Headless tool round ────────────────────────────────
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
                &turn_result.accum.tool_calls,
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
    Ok(AgenticLoopOutcome::Completed)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A mock host that returns a canned turn result and then signals loop exit.
    struct MockLoopHost {
        turn_results: Vec<HostTurnResult>,
        current_turn: usize,
    }

    impl MockLoopHost {
        fn with_results(results: Vec<HostTurnResult>) -> Self {
            Self {
                turn_results: results,
                current_turn: 0,
            }
        }

        fn text_only_result(text: &str) -> HostTurnResult {
            HostTurnResult {
                accum: ChatTurnSseAccum {
                    full_text: text.to_string(),
                    has_tool_calls: false,
                    has_usage: true,
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    ..ChatTurnSseAccum::default()
                },
                ttft_ms: Some(42),
                edge_callback_outputs: HashMap::new(),
                edge_tool_round: Vec::new(),
            }
        }
    }

    #[async_trait]
    impl AgenticLoopHost for MockLoopHost {
        async fn execute_turn(
            &mut self,
            _state: &mut AgenticLoopState,
        ) -> Result<HostTurnResult, String> {
            if self.current_turn >= self.turn_results.len() {
                return Err("no more turns".to_string());
            }
            let result = self.turn_results.remove(0);
            self.current_turn += 1;
            Ok(result)
        }

        fn emit_headless_line(&mut self, _style: HeadlessStderrStyle, _line: String) {}

        fn is_quiet(&self) -> bool {
            true
        }

        fn valid_tool_names(&self) -> &HashSet<String> {
            static EMPTY: std::sync::LazyLock<HashSet<String>> =
                std::sync::LazyLock::new(HashSet::new);
            &EMPTY
        }
    }

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
            api: mo_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            api_token: String::new(),
        }
    }

    #[tokio::test]
    async fn single_text_turn_completes() {
        let mut host = MockLoopHost::with_results(vec![MockLoopHost::text_only_result(
            "Hello, world!",
        )]);
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
        let mut host = MockLoopHost::with_results(vec![]);
        let mut state = make_state();
        state.remaining_turns = 0;
        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_err());
        assert!(outcome
            .unwrap_err()
            .contains("budget"));
    }

    #[tokio::test]
    async fn host_error_propagates() {
        let mut host = MockLoopHost::with_results(vec![]);
        let mut state = make_state();
        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_err());
        assert_eq!(outcome.unwrap_err(), "no more turns");
    }

    #[tokio::test]
    async fn ttft_captured_from_first_turn() {
        let mut host = MockLoopHost::with_results(vec![MockLoopHost::text_only_result("hi")]);
        let mut state = make_state();
        state.first_ttft_ms = None;
        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert_eq!(state.first_ttft_ms, Some(42));
    }
}
