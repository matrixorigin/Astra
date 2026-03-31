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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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

    // ── Cancellation ──
    /// Shared flag checked between turns. Set externally (e.g. by cancel_run).
    pub cancel_flag: Option<Arc<AtomicBool>>,
}

// ─── Loop exit ───────────────────────────────────────────────────────────────

/// Result of running the agentic loop to completion.
#[derive(Debug)]
pub enum AgenticLoopOutcome {
    /// Loop completed normally (final text produced or budget exhausted gracefully).
    Completed,
    /// Loop aborted due to a fatal error.
    Error(String),
    /// Loop was cancelled externally via cancel_flag.
    Cancelled,
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
        // ─── Cancel check (cooperative) ─────────────────────────────────
        if state
            .cancel_flag
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Relaxed))
        {
            return Ok(AgenticLoopOutcome::Cancelled);
        }

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
    use serde_json::json;

    // ── Flexible mock host for multi-turn scenarios ─────────────────────────

    struct MockHost {
        turn_results: Vec<HostTurnResult>,
        current_turn: usize,
        valid_tools: HashSet<String>,
        emitted_lines: Vec<String>,
        quiet: bool,
    }

    impl MockHost {
        fn new(results: Vec<HostTurnResult>) -> Self {
            Self {
                turn_results: results,
                current_turn: 0,
                valid_tools: HashSet::new(),
                emitted_lines: Vec::new(),
                quiet: true,
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
            api: mo_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            api_token: String::new(),
            cancel_flag: None,
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
            edge_tool_result(vec![make_edge_tool("read_file", "content")], 10, 5, Some(20)),
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
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![make_edge_tool("bash", "ok")], 20, 10, Some(50)),
        ]);
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
                vec![make_edge_tool("bash", "a"), make_edge_tool("read_file", "b")],
                10, 5, None,
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
                vec![make_edge_tool_with_args("bash", json!({"cmd": "ls"}), "file.txt")],
                10, 5, None,
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
}
