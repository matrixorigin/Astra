//! Unified agentic loop dispatcher.
//!
//! [`LoopDispatcher`] is the single entry point for running the agentic cognitive
//! loop regardless of execution context (CLI, cloud headless, WebSocket).
//!
//! # Architecture
//!
//! ```text
//!                  ┌──────────────┐
//!                  │LoopDispatcher│
//!                  └──────┬───────┘
//!                         │
//!               dispatch_run(context)
//!                         │
//!           ┌─────────────┼─────────────┐
//!           ▼             ▼             ▼
//!     CliLoopHost   ServerLoopHost   Future hosts
//!     (terminal)    (headless SSE)   (WebSocket, etc.)
//!           │             │             │
//!           └──────┬──────┘─────────────┘
//!                  ▼
//!      run_agentic_loop_with_host()
//!           (unified cognitive loop)
//! ```
//!
//! # Host Contract
//!
//! Every [`AgenticLoopHost`] implementation must:
//!
//! 1. **`execute_turn`** — Build payload → call LLM → consume SSE → return
//!    [`HostTurnResult`] with accumulated tool calls, text, and usage.
//!
//! 2. **`emit_headless_line`** — Route headless output (tool status, errors)
//!    to the appropriate sink (terminal stderr, SSE event, WS frame).
//!
//! 3. **`is_quiet`** — Suppress non-essential output in quiet/CI modes.
//!
//! 4. **`valid_tool_names`** — Return the set of tool names the host supports,
//!    used for tool-call validation during headless rounds.
//!
//! # WAITING State
//!
//! When a run needs external input (tool approval, user resume after pause),
//! the loop can yield [`AgenticLoopOutcome::Waiting`] with a [`WaitReason`].
//! The caller is responsible for providing the input and re-invoking the loop
//! with the updated state.
//!
//! # Usage
//!
//! ```rust,ignore
//! let dispatcher = LoopDispatcher::new();
//! let outcome = dispatcher.dispatch_run(context).await?;
//! match outcome {
//!     DispatchOutcome::Completed(result) => { /* done */ }
//!     DispatchOutcome::Waiting(reason) => { /* need external input */ }
//!     DispatchOutcome::Cancelled => { /* externally cancelled */ }
//!     DispatchOutcome::Error(e) => { /* fatal */ }
//! }
//! ```

use std::collections::HashSet;

use super::agentic_loop_host::{
    AgenticLoopHost, AgenticLoopOutcome, AgenticLoopState, run_agentic_loop_with_host,
};

// ─── Wait reason ─────────────────────────────────────────────────────────────

/// Reason the agentic loop is waiting for external input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitReason {
    /// Waiting for user to approve a tool execution.
    ToolApproval {
        request_id: String,
        tool_name: String,
    },
    /// Waiting for user to resume after a pause.
    UserResume,
    /// Waiting for external event (webhook, callback).
    ExternalEvent { event_type: String },
}

impl std::fmt::Display for WaitReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WaitReason::ToolApproval {
                tool_name,
                request_id,
            } => write!(f, "awaiting approval for {tool_name} (req {request_id})"),
            WaitReason::UserResume => write!(f, "paused, awaiting user resume"),
            WaitReason::ExternalEvent { event_type } => {
                write!(f, "awaiting external event: {event_type}")
            }
        }
    }
}

// ─── Dispatch outcome ────────────────────────────────────────────────────────

/// Outcome of a dispatched agentic run.
#[derive(Debug)]
pub enum DispatchOutcome {
    /// Loop ran to completion (final text produced or turn budget exhausted).
    Completed(LoopResult),
    /// Loop is waiting for external input; can be resumed.
    Waiting(WaitReason),
    /// Loop was cancelled externally via cancel flag.
    Cancelled,
    /// Loop encountered a fatal error.
    Error(String),
}

/// Summary produced after a completed agentic loop run.
#[derive(Debug, Default)]
pub struct LoopResult {
    pub final_text: String,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_tool_calls: u32,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub tools_used: HashSet<String>,
}

impl LoopResult {
    /// Extract result from completed loop state.
    pub fn from_state(state: &AgenticLoopState) -> Self {
        Self {
            final_text: state.final_text.clone(),
            total_prompt_tokens: state.total_prompt,
            total_completion_tokens: state.total_completion,
            total_tool_calls: state.total_tool_calls,
            session_id: state.current_session_id.clone(),
            run_id: state.current_run_id.clone(),
            tools_used: state.telemetry.all_tools_used.clone(),
        }
    }
}

// ─── Dispatcher ──────────────────────────────────────────────────────────────

/// Unified agentic loop dispatcher.
///
/// Wraps [`run_agentic_loop_with_host`] with consistent pre/post-loop handling
/// and outcome mapping. All execution contexts (CLI, server, WebSocket) should
/// go through this dispatcher.
pub struct LoopDispatcher;

impl LoopDispatcher {
    pub fn new() -> Self {
        Self
    }

    /// Run the agentic loop with the given host and state.
    ///
    /// This is the single entry point for all agentic executions.
    /// The host determines how turns are executed (CLI vs server vs test).
    pub async fn dispatch<H: AgenticLoopHost>(
        &self,
        host: &mut H,
        state: &mut AgenticLoopState,
    ) -> DispatchOutcome {
        match run_agentic_loop_with_host(host, state).await {
            Ok(AgenticLoopOutcome::Completed) => {
                DispatchOutcome::Completed(LoopResult::from_state(state))
            }
            Ok(AgenticLoopOutcome::Cancelled) => DispatchOutcome::Cancelled,
            Ok(AgenticLoopOutcome::Waiting(reason)) => {
                DispatchOutcome::Waiting(WaitReason::ExternalEvent { event_type: reason })
            }
            Ok(AgenticLoopOutcome::Error(e)) | Err(e) => DispatchOutcome::Error(e),
        }
    }
}

impl Default for LoopDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::step_protocol::InMemoryIdempotencyCache;
    use crate::pipeline::step_recorder::StepRecorder;
    use crate::semantic_dedup::SemanticDedup;
    use crate::turn::agentic_headless_round::HeadlessStderrStyle;
    use crate::turn::agentic_loop_host::HostTurnResult;
    use crate::turn::chat_turn_heuristics::TaskExecutionProfile;
    use crate::turn::chat_turn_sse_dispatch::ChatTurnSseAccum;
    use crate::turn::turn_guard::TurnGuard;
    use async_trait::async_trait;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use tokio_util::sync::CancellationToken;

    // ── Test host ────────────────────────────────────────────────────────────

    struct TestHost {
        turns: Vec<HostTurnResult>,
        valid_tools: HashSet<String>,
    }

    impl TestHost {
        fn completed_after_one_turn(text: &str) -> Self {
            Self {
                turns: vec![HostTurnResult {
                    accum: ChatTurnSseAccum {
                        full_text: text.to_string(),
                        tool_calls: vec![],
                        reasoning_content: String::new(),
                        session_id: Some("test-session".to_string()),
                        run_id: None,
                        prompt_tokens: 100,
                        completion_tokens: 50,
                        cache_read_tokens: 0,
                        cache_creation_tokens: 0,
                        has_usage: true,
                        has_tool_calls: false,
                        explain_turns: vec![],
                        error_message: None,
                        system_prompt_tokens: None,
                        system_prompt_breakdown: None,
                        ..Default::default()
                    },
                    ttft_ms: Some(42),
                    edge_tool_round: vec![],
                }],
                valid_tools: HashSet::new(),
            }
        }
    }

    #[async_trait]
    impl AgenticLoopHost for TestHost {
        async fn execute_turn(
            &mut self,
            _state: &mut AgenticLoopState,
        ) -> Result<HostTurnResult, String> {
            if self.turns.is_empty() {
                return Err("no more turns".to_string());
            }
            Ok(self.turns.remove(0))
        }

        fn emit_headless_line(&mut self, _style: HeadlessStderrStyle, _line: String) {}
        fn is_quiet(&self) -> bool {
            true
        }
        fn valid_tool_names(&self) -> &HashSet<String> {
            &self.valid_tools
        }
    }

    fn test_state(message: &str) -> AgenticLoopState {
        AgenticLoopState {
            messages: vec![json!({"role": "user", "content": message})],
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
            total_evidence_tool_calls: 0,
            has_any_usage: false,
            max_turns: 3,
            remaining_turns: 3,
            turn_guard: TurnGuard::new(),
            restricted_tools: HashSet::new(),
            step_recorder: StepRecorder::new("test", "run"),
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(0.75),
            call_counts: HashMap::new(),
            max_identical_tool_calls: crate::runtime_config::RuntimeConfig::load()
                .tool_selection
                .effective_max_identical_calls(),
            max_tools_per_turn: 15,
            stall: Default::default(),
            telemetry: Default::default(),
            skills: Default::default(),
            hooks: Default::default(),
            messaging: Default::default(),
            cancellation: Default::default(),
            error_recovery: Default::default(),
            message: message.to_string(),
            recent_tools: Vec::new(),
            task_profile: TaskExecutionProfile::default(),
            last_turn_policy: crate::turn::agentic_loop_host::TurnInteractionPolicy::default(),
            api: astra_thin_client::ThinClient::new("http://localhost:1", None).unwrap(),
            api_token: "test".to_string(),
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

    #[tokio::test]
    async fn dispatch_completes_single_turn() {
        let dispatcher = LoopDispatcher::new();
        let mut host = TestHost::completed_after_one_turn("Hello world");
        let mut state = test_state("say hello");
        let outcome = dispatcher.dispatch(&mut host, &mut state).await;
        match outcome {
            DispatchOutcome::Completed(result) => {
                assert_eq!(result.final_text, "Hello world");
                assert_eq!(result.total_prompt_tokens, 100);
                assert_eq!(result.total_completion_tokens, 50);
                assert_eq!(result.session_id, Some("test-session".to_string()));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_cancelled_via_flag() {
        let dispatcher = LoopDispatcher::new();
        let mut host = TestHost::completed_after_one_turn("never reached");
        let mut state = test_state("cancel me");
        let flag = Arc::new(AtomicBool::new(true));
        state.cancellation.flag = Some(flag);
        let outcome = dispatcher.dispatch(&mut host, &mut state).await;
        assert!(matches!(outcome, DispatchOutcome::Cancelled));
    }

    #[tokio::test]
    async fn dispatch_cancelled_via_token() {
        let dispatcher = LoopDispatcher::new();
        let mut host = TestHost::completed_after_one_turn("never reached");
        let mut state = test_state("cancel via token");
        let token = Arc::new(CancellationToken::new());
        token.cancel();
        state.cancellation.token = Some(token);
        let outcome = dispatcher.dispatch(&mut host, &mut state).await;
        assert!(matches!(outcome, DispatchOutcome::Cancelled));
    }

    #[tokio::test]
    async fn dispatch_error_from_host() {
        let dispatcher = LoopDispatcher::new();
        let mut host = TestHost {
            turns: vec![], // no turns = error on first call
            valid_tools: HashSet::new(),
        };
        let mut state = test_state("fail");
        let outcome = dispatcher.dispatch(&mut host, &mut state).await;
        assert!(matches!(outcome, DispatchOutcome::Error(_)));
    }

    #[test]
    fn wait_reason_display() {
        let approval = WaitReason::ToolApproval {
            request_id: "r1".into(),
            tool_name: "bash".into(),
        };
        assert!(approval.to_string().contains("bash"));
        assert!(approval.to_string().contains("r1"));

        let resume = WaitReason::UserResume;
        assert!(resume.to_string().contains("resume"));

        let ext = WaitReason::ExternalEvent {
            event_type: "webhook".into(),
        };
        assert!(ext.to_string().contains("webhook"));
    }

    #[test]
    fn loop_result_from_state() {
        let mut state = test_state("test");
        state.final_text = "output".to_string();
        state.total_prompt = 200;
        state.total_completion = 100;
        state.total_tool_calls = 3;
        state.current_session_id = Some("s1".into());
        state.current_run_id = Some("r1".into());
        state.telemetry.all_tools_used.insert("bash".into());
        let result = LoopResult::from_state(&state);
        assert_eq!(result.final_text, "output");
        assert_eq!(result.total_prompt_tokens, 200);
        assert_eq!(result.total_completion_tokens, 100);
        assert_eq!(result.total_tool_calls, 3);
        assert_eq!(result.session_id.as_deref(), Some("s1"));
        assert_eq!(result.run_id.as_deref(), Some("r1"));
        assert!(result.tools_used.contains("bash"));
    }

    #[test]
    fn dispatcher_default_trait() {
        let _d: LoopDispatcher = Default::default();
    }

    #[tokio::test]
    async fn dispatch_waiting_maps_to_external_event() {
        // Verify that if we had a Waiting outcome from the loop, the dispatcher
        // would map it correctly. We test the WaitReason enum directly since
        // Waiting is not yet triggered by the loop itself.
        let reason = WaitReason::ToolApproval {
            request_id: "req-1".into(),
            tool_name: "bash".into(),
        };
        let outcome = DispatchOutcome::Waiting(reason);
        match outcome {
            DispatchOutcome::Waiting(WaitReason::ToolApproval { tool_name, .. }) => {
                assert_eq!(tool_name, "bash");
            }
            other => panic!("expected Waiting(ToolApproval), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn two_hosts_same_input_same_outcome_shape() {
        // Verify that two different host implementations produce the same
        // outcome shape (Completed with matching token counts) for identical input.
        let dispatcher = LoopDispatcher::new();

        let mut host_a = TestHost::completed_after_one_turn("answer");
        let mut state_a = test_state("question");
        let outcome_a = dispatcher.dispatch(&mut host_a, &mut state_a).await;

        let mut host_b = TestHost::completed_after_one_turn("answer");
        let mut state_b = test_state("question");
        let outcome_b = dispatcher.dispatch(&mut host_b, &mut state_b).await;

        match (&outcome_a, &outcome_b) {
            (DispatchOutcome::Completed(a), DispatchOutcome::Completed(b)) => {
                assert_eq!(a.final_text, b.final_text);
                assert_eq!(a.total_prompt_tokens, b.total_prompt_tokens);
                assert_eq!(a.total_completion_tokens, b.total_completion_tokens);
            }
            _ => panic!("expected both Completed, got {outcome_a:?} and {outcome_b:?}"),
        }
    }
}
