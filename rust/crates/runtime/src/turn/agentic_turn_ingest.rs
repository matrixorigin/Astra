//! Apply one `/chat/turn` stream outcome to agentic loop state (usage, response guards, factual retry).
//!
//! Hosts (CLI `stream_chat_sse`, future headless clients) build [`AgenticTurnStreamSnapshot`] and pass
//! edge-tool names via `edge_round_len` + an index closure (`usize` → owned `String`) so closure
//! signatures stay lifetime-simple.

use std::collections::HashSet;

use astra_core::agent_warn;
use serde_json::Value;

use super::chat_turn_heuristics::{
    TaskExecutionProfile, openai_factual_tool_retry_user_message, should_force_factual_tool_retry,
};
use super::chat_turn_sse_dispatch::ChatTurnSseAccum;
use super::response_guard::apply_response_guards;
use crate::pipeline::step_recorder::StepRecorder;

/// Read-only slice of [`super::chat_turn_sse_dispatch::ChatTurnSseAccum`] fields needed for ingest.
#[derive(Debug, Clone)]
pub struct AgenticTurnStreamSnapshot<'a> {
    pub ttft_ms: Option<u64>,
    pub session_id: &'a Option<String>,
    pub run_id: &'a Option<String>,
    pub full_text: &'a str,
    pub tool_calls: &'a [Value],
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub has_usage: bool,
    pub error_message: &'a Option<String>,
}

/// Build [`AgenticTurnStreamSnapshot`] from a [`ChatTurnSseAccum`] plus TTFT (CLI `TurnResult` derefs to accum).
#[must_use]
pub fn agentic_turn_stream_snapshot_from_sse_accum<'a>(
    accum: &'a ChatTurnSseAccum,
    ttft_ms: Option<u64>,
) -> AgenticTurnStreamSnapshot<'a> {
    AgenticTurnStreamSnapshot {
        ttft_ms,
        session_id: &accum.session_id,
        run_id: &accum.run_id,
        full_text: accum.full_text.as_str(),
        tool_calls: accum.tool_calls.as_slice(),
        prompt_tokens: accum.prompt_tokens,
        completion_tokens: accum.completion_tokens,
        cache_read_tokens: accum.cache_read_tokens,
        cache_creation_tokens: accum.cache_creation_tokens,
        has_usage: accum.has_usage,
        error_message: &accum.error_message,
    }
}

/// Mutable agentic-loop fields updated by [`ingest_agentic_turn_stream`].
pub struct AgenticTurnIngestMut<'a> {
    pub task_profile: TaskExecutionProfile,
    pub first_ttft_ms: &'a mut Option<u64>,
    pub current_session_id: &'a mut Option<String>,
    pub current_run_id: &'a mut Option<String>,
    pub final_text: &'a mut String,
    pub total_prompt: &'a mut u64,
    pub total_completion: &'a mut u64,
    pub total_cache_read: &'a mut u64,
    pub total_cache_creation: &'a mut u64,
    pub total_tool_calls: &'a mut u32,
    pub step_recorder: &'a mut StepRecorder,
    pub all_tools_used: &'a mut HashSet<String>,
    pub has_any_usage: &'a mut bool,
    pub forced_factual_retry: &'a mut bool,
    pub messages: &'a mut Vec<Value>,
    /// See [`crate::turn::agentic_loop_host::AgenticLoopState::last_measured_prompt_tokens`].
    pub last_measured_prompt_tokens: &'a mut Option<u64>,
    /// See [`crate::turn::agentic_loop_host::AgenticLoopState::consecutive_context_window_errors`].
    pub consecutive_context_window_errors: &'a mut u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AgenticTurnIngestOutcome {
    Break,
    Continue,
    Fatal(String),
    HasToolCalls,
}

/// Maps [`AgenticTurnIngestOutcome`] to multi-turn loop control (hosts map break/continue to their enums).
#[derive(Debug, PartialEq, Eq)]
pub enum AgenticIngestIterationControl {
    Fatal(String),
    BreakLoop,
    ContinueIterating,
    ProceedWithToolCalls,
}

#[must_use]
pub fn map_ingest_outcome_to_iteration_control(
    outcome: AgenticTurnIngestOutcome,
) -> AgenticIngestIterationControl {
    match outcome {
        AgenticTurnIngestOutcome::Fatal(e) => AgenticIngestIterationControl::Fatal(e),
        AgenticTurnIngestOutcome::Break => AgenticIngestIterationControl::BreakLoop,
        AgenticTurnIngestOutcome::Continue => AgenticIngestIterationControl::ContinueIterating,
        AgenticTurnIngestOutcome::HasToolCalls => {
            AgenticIngestIterationControl::ProceedWithToolCalls
        }
    }
}

/// Merge streaming turn metadata into running totals and decide whether to break, retry, or run tools.
pub fn ingest_agentic_turn_stream(
    snap: &AgenticTurnStreamSnapshot<'_>,
    edge_round_len: usize,
    mut edge_tool_name: impl FnMut(usize) -> String,
    message: &str,
    recent_tools: &[String],
    quiet: bool,
    mut st: AgenticTurnIngestMut<'_>,
) -> AgenticTurnIngestOutcome {
    if st.first_ttft_ms.is_none() {
        *st.first_ttft_ms = snap.ttft_ms;
    }

    if let Some(sid) = snap.session_id.as_ref() {
        *st.current_session_id = Some(sid.clone());
    }
    if snap.run_id.is_some() {
        *st.current_run_id = snap.run_id.clone();
    }
    let round_has_edge_work = !snap.tool_calls.is_empty() || edge_round_len > 0;

    if !snap.full_text.is_empty() && !round_has_edge_work {
        *st.final_text = snap.full_text.to_string();

        let guard = apply_response_guards(st.final_text.as_str(), snap.tool_calls, &[], message);
        if let Some(replacement) = guard.replacement {
            agent_warn!("response_guard", "Guard triggered, replacing LLM output");
            *st.final_text = replacement;
            return AgenticTurnIngestOutcome::Break;
        }
        if guard.quality.has_fabrication_markers {
            agent_warn!(
                "response_guard",
                "Fabrication markers detected: placeholder paths in response"
            );
        }
        if guard.quality.is_echo {
            agent_warn!(
                "response_guard",
                "Echo detected: LLM repeated user query instead of answering"
            );
        }
    }

    *st.total_prompt += snap.prompt_tokens;
    *st.total_completion += snap.completion_tokens;
    *st.total_cache_read += snap.cache_read_tokens;
    *st.total_cache_creation += snap.cache_creation_tokens;
    *st.total_tool_calls += if !snap.tool_calls.is_empty() {
        snap.tool_calls.len()
    } else {
        edge_round_len
    } as u32;

    st.step_recorder
        .record_tokens(snap.prompt_tokens, snap.completion_tokens);

    for tc in snap.tool_calls {
        if let Some(name) = tc.get("name").and_then(|v| v.as_str()) {
            st.all_tools_used.insert(name.to_string());
        }
    }
    for i in 0..edge_round_len {
        st.all_tools_used.insert(edge_tool_name(i));
    }
    *st.has_any_usage = *st.has_any_usage || snap.has_usage;

    if let Some(err) = snap.error_message {
        let lower = err.to_lowercase();
        if crate::turn::llm_client::is_context_window_error(&lower) {
            *st.consecutive_context_window_errors =
                st.consecutive_context_window_errors.saturating_add(1);
        } else {
            *st.consecutive_context_window_errors = 0;
        }
        return AgenticTurnIngestOutcome::Fatal(err.clone());
    }

    if !round_has_edge_work {
        if should_force_factual_tool_retry(
            st.task_profile,
            message,
            recent_tools,
            *st.total_tool_calls,
            *st.forced_factual_retry,
        ) {
            *st.forced_factual_retry = true;
            if !quiet {
                eprintln!("  ↻ No tool call on a live-data query; forcing one corrective retry…");
            }
            st.messages
                .push(openai_factual_tool_retry_user_message(message));
            st.final_text.clear();
            record_prompt_calibration_success(snap, &mut st);
            return AgenticTurnIngestOutcome::Continue;
        }
        record_prompt_calibration_success(snap, &mut st);
        return AgenticTurnIngestOutcome::Break;
    }

    record_prompt_calibration_success(snap, &mut st);
    AgenticTurnIngestOutcome::HasToolCalls
}

/// After a non-fatal ingest: clear PTL streak and remember provider prompt size when available.
fn record_prompt_calibration_success(
    snap: &AgenticTurnStreamSnapshot<'_>,
    st: &mut AgenticTurnIngestMut<'_>,
) {
    *st.consecutive_context_window_errors = 0;
    if snap.has_usage && snap.prompt_tokens > 0 {
        *st.last_measured_prompt_tokens = Some(snap.prompt_tokens);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Pack {
        first_ttft_ms: Option<u64>,
        current_session_id: Option<String>,
        current_run_id: Option<String>,
        final_text: String,
        total_prompt: u64,
        total_completion: u64,
        total_cache_read: u64,
        total_cache_creation: u64,
        total_tool_calls: u32,
        step_recorder: StepRecorder,
        all_tools_used: HashSet<String>,
        has_any_usage: bool,
        forced_factual_retry: bool,
        messages: Vec<Value>,
        last_measured_prompt_tokens: Option<u64>,
        consecutive_context_window_errors: u32,
    }

    impl Pack {
        fn new() -> Self {
            Self {
                first_ttft_ms: None,
                current_session_id: None,
                current_run_id: None,
                final_text: String::new(),
                total_prompt: 0,
                total_completion: 0,
                total_cache_read: 0,
                total_cache_creation: 0,
                total_tool_calls: 0,
                step_recorder: StepRecorder::with_persistence("s", "t"),
                all_tools_used: HashSet::new(),
                has_any_usage: false,
                forced_factual_retry: false,
                messages: Vec::new(),
                last_measured_prompt_tokens: None,
                consecutive_context_window_errors: 0,
            }
        }

        fn ingest_mut(&mut self) -> AgenticTurnIngestMut<'_> {
            AgenticTurnIngestMut {
                task_profile: TaskExecutionProfile::default(),
                first_ttft_ms: &mut self.first_ttft_ms,
                current_session_id: &mut self.current_session_id,
                current_run_id: &mut self.current_run_id,
                final_text: &mut self.final_text,
                total_prompt: &mut self.total_prompt,
                total_completion: &mut self.total_completion,
                total_cache_read: &mut self.total_cache_read,
                total_cache_creation: &mut self.total_cache_creation,
                total_tool_calls: &mut self.total_tool_calls,
                step_recorder: &mut self.step_recorder,
                all_tools_used: &mut self.all_tools_used,
                has_any_usage: &mut self.has_any_usage,
                forced_factual_retry: &mut self.forced_factual_retry,
                messages: &mut self.messages,
                last_measured_prompt_tokens: &mut self.last_measured_prompt_tokens,
                consecutive_context_window_errors: &mut self.consecutive_context_window_errors,
            }
        }
    }

    #[test]
    fn snapshot_from_sse_accum_matches_fields() {
        let accum = ChatTurnSseAccum {
            session_id: Some("s1".into()),
            run_id: Some("r1".into()),
            full_text: "hi".into(),
            tool_calls: vec![json!({"name": "bash"})],
            prompt_tokens: 3,
            completion_tokens: 4,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            has_usage: true,
            error_message: Some("e".into()),
            ..Default::default()
        };
        let snap = agentic_turn_stream_snapshot_from_sse_accum(&accum, Some(99));
        assert_eq!(snap.ttft_ms, Some(99));
        assert_eq!(snap.session_id.as_deref(), Some("s1"));
        assert_eq!(snap.run_id.as_deref(), Some("r1"));
        assert_eq!(snap.full_text, "hi");
        assert_eq!(snap.tool_calls.len(), 1);
        assert_eq!(snap.prompt_tokens, 3);
        assert_eq!(snap.completion_tokens, 4);
        assert!(snap.has_usage);
        assert_eq!(snap.error_message.as_deref(), Some("e"));
    }

    #[test]
    fn map_ingest_outcome_control() {
        assert_eq!(
            map_ingest_outcome_to_iteration_control(AgenticTurnIngestOutcome::Fatal("x".into())),
            AgenticIngestIterationControl::Fatal("x".into())
        );
        assert_eq!(
            map_ingest_outcome_to_iteration_control(AgenticTurnIngestOutcome::Break),
            AgenticIngestIterationControl::BreakLoop
        );
        assert_eq!(
            map_ingest_outcome_to_iteration_control(AgenticTurnIngestOutcome::Continue),
            AgenticIngestIterationControl::ContinueIterating
        );
        assert_eq!(
            map_ingest_outcome_to_iteration_control(AgenticTurnIngestOutcome::HasToolCalls),
            AgenticIngestIterationControl::ProceedWithToolCalls
        );
    }

    #[test]
    fn fatal_on_error_message() {
        let err = Some("boom".to_string());
        let snap = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: "",
            tool_calls: &[],
            prompt_tokens: 0,
            completion_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            has_usage: false,
            error_message: &err,
        };
        let mut pack = Pack::new();
        let out = ingest_agentic_turn_stream(
            &snap,
            0,
            |_| String::new(),
            "hi",
            &[],
            true,
            pack.ingest_mut(),
        );
        assert_eq!(out, AgenticTurnIngestOutcome::Fatal("boom".to_string()));
        assert_eq!(pack.consecutive_context_window_errors, 0);
    }

    #[test]
    fn fatal_context_window_increments_ptl_streak() {
        let err = Some("prompt is too long".to_string());
        let snap = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: "",
            tool_calls: &[],
            prompt_tokens: 5,
            completion_tokens: 1,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            has_usage: true,
            error_message: &err,
        };
        let mut pack = Pack::new();
        pack.consecutive_context_window_errors = 1;
        let out = ingest_agentic_turn_stream(
            &snap,
            0,
            |_| String::new(),
            "hi",
            &[],
            true,
            pack.ingest_mut(),
        );
        assert_eq!(
            out,
            AgenticTurnIngestOutcome::Fatal("prompt is too long".to_string())
        );
        assert_eq!(pack.consecutive_context_window_errors, 2);
    }

    #[test]
    fn fatal_non_context_resets_ptl_streak() {
        let err = Some("rate limited".to_string());
        let snap = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: "",
            tool_calls: &[],
            prompt_tokens: 0,
            completion_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            has_usage: false,
            error_message: &err,
        };
        let mut pack = Pack::new();
        pack.consecutive_context_window_errors = 4;
        let out = ingest_agentic_turn_stream(
            &snap,
            0,
            |_| String::new(),
            "hi",
            &[],
            true,
            pack.ingest_mut(),
        );
        assert_eq!(
            out,
            AgenticTurnIngestOutcome::Fatal("rate limited".to_string())
        );
        assert_eq!(pack.consecutive_context_window_errors, 0);
    }

    #[test]
    fn break_when_no_tools_and_no_edge() {
        let snap = AgenticTurnStreamSnapshot {
            ttft_ms: Some(12),
            session_id: &None,
            run_id: &None,
            full_text: "ok",
            tool_calls: &[],
            prompt_tokens: 1,
            completion_tokens: 2,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            has_usage: true,
            error_message: &None,
        };
        let mut pack = Pack::new();
        let out = ingest_agentic_turn_stream(
            &snap,
            0,
            |_| String::new(),
            "hi",
            &[],
            true,
            pack.ingest_mut(),
        );
        assert_eq!(out, AgenticTurnIngestOutcome::Break);
        assert_eq!(pack.first_ttft_ms, Some(12u64));
        assert_eq!(pack.total_prompt, 1);
        assert_eq!(pack.total_completion, 2);
        assert!(pack.has_any_usage);
        assert_eq!(pack.last_measured_prompt_tokens, Some(1));
        assert_eq!(pack.consecutive_context_window_errors, 0);
    }

    #[test]
    fn has_tool_calls_when_edge_round_nonzero() {
        let snap = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: "",
            tool_calls: &[],
            prompt_tokens: 0,
            completion_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            has_usage: false,
            error_message: &None,
        };
        let mut pack = Pack::new();
        let out = ingest_agentic_turn_stream(
            &snap,
            1,
            |_| "bash".to_string(),
            "hi",
            &[],
            true,
            pack.ingest_mut(),
        );
        assert_eq!(out, AgenticTurnIngestOutcome::HasToolCalls);
        assert!(pack.all_tools_used.contains("bash"));
        assert_eq!(pack.total_tool_calls, 1);
    }

    #[test]
    fn has_tool_calls_from_server_tool_calls() {
        let tcs = vec![json!({"name": "read_file", "arguments": {}})];
        let snap = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: "",
            tool_calls: &tcs,
            prompt_tokens: 0,
            completion_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            has_usage: false,
            error_message: &None,
        };
        let mut pack = Pack::new();
        let out = ingest_agentic_turn_stream(
            &snap,
            0,
            |_| String::new(),
            "hi",
            &[],
            true,
            pack.ingest_mut(),
        );
        assert_eq!(out, AgenticTurnIngestOutcome::HasToolCalls);
        assert!(pack.all_tools_used.contains("read_file"));
    }

    #[test]
    fn tool_turn_draft_text_does_not_replace_final_text() {
        let tcs = vec![json!({"name": "bash", "arguments": {}})];
        let snap = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: "draft summary that should not stick",
            tool_calls: &tcs,
            prompt_tokens: 1,
            completion_tokens: 2,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            has_usage: true,
            error_message: &None,
        };
        let mut pack = Pack::new();
        pack.final_text = "previous stable answer".to_string();
        let out = ingest_agentic_turn_stream(
            &snap,
            0,
            |_| String::new(),
            "hi",
            &[],
            true,
            pack.ingest_mut(),
        );
        assert_eq!(out, AgenticTurnIngestOutcome::HasToolCalls);
        assert_eq!(pack.final_text, "previous stable answer");
    }

    // ─── Factual retry injection tests ──────────────────────────────────────

    #[test]
    fn factual_retry_injects_nudge_and_clears_text() {
        // When LLM answers a live-data query with zero tool calls,
        // ingest should inject a retry nudge and return Continue.
        let snap = AgenticTurnStreamSnapshot {
            ttft_ms: Some(50),
            session_id: &None,
            run_id: &None,
            full_text: "Here are your recent PRs: ...",
            tool_calls: &[],
            prompt_tokens: 100,
            completion_tokens: 200,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            has_usage: true,
            error_message: &None,
        };
        let mut pack = Pack::new();
        let out = ingest_agentic_turn_stream(
            &snap,
            0,
            |_| String::new(),
            "show me the latest PR",
            &[],
            true,
            pack.ingest_mut(),
        );
        assert_eq!(out, AgenticTurnIngestOutcome::Continue);
        assert!(pack.forced_factual_retry, "flag should be set");
        assert!(pack.final_text.is_empty(), "text should be cleared");
        assert_eq!(pack.last_measured_prompt_tokens, Some(100));
        assert_eq!(pack.messages.len(), 1, "nudge message should be injected");
        let nudge = &pack.messages[0];
        assert_eq!(nudge["role"], "user");
        assert!(
            nudge["content"]
                .as_str()
                .unwrap()
                .contains("Runtime correction"),
        );
    }

    #[test]
    fn factual_retry_does_not_fire_twice() {
        let snap = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: "fabricated answer",
            tool_calls: &[],
            prompt_tokens: 10,
            completion_tokens: 20,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            has_usage: true,
            error_message: &None,
        };
        let mut pack = Pack::new();
        pack.forced_factual_retry = true; // already retried once
        let out = ingest_agentic_turn_stream(
            &snap,
            0,
            |_| String::new(),
            "show me the latest PR",
            &[],
            true,
            pack.ingest_mut(),
        );
        // Should break, not retry again
        assert_eq!(out, AgenticTurnIngestOutcome::Break);
        assert!(pack.messages.is_empty(), "no nudge on second attempt");
    }

    #[test]
    fn factual_retry_skipped_when_tools_were_called() {
        let snap = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: "Based on the PR data...",
            tool_calls: &[],
            prompt_tokens: 10,
            completion_tokens: 20,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            has_usage: true,
            error_message: &None,
        };
        let mut pack = Pack::new();
        pack.total_tool_calls = 3; // tools were called in a previous round
        let out = ingest_agentic_turn_stream(
            &snap,
            0,
            |_| String::new(),
            "show me the latest PR",
            &[],
            true,
            pack.ingest_mut(),
        );
        assert_eq!(out, AgenticTurnIngestOutcome::Break);
        assert!(!pack.forced_factual_retry);
        assert!(pack.messages.is_empty());
    }

    #[test]
    fn factual_retry_skipped_for_non_live_queries() {
        let snap = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: "Rust is a systems programming language...",
            tool_calls: &[],
            prompt_tokens: 10,
            completion_tokens: 50,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            has_usage: true,
            error_message: &None,
        };
        let mut pack = Pack::new();
        let out = ingest_agentic_turn_stream(
            &snap,
            0,
            |_| String::new(),
            "what is Rust?",
            &[],
            true,
            pack.ingest_mut(),
        );
        // General knowledge question — no retry
        assert_eq!(out, AgenticTurnIngestOutcome::Break);
        assert!(!pack.forced_factual_retry);
        assert!(pack.messages.is_empty());
    }

    #[test]
    fn factual_retry_works_for_chinese_workspace_queries() {
        let snap = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: "代码看起来很好...",
            tool_calls: &[],
            prompt_tokens: 50,
            completion_tokens: 100,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            has_usage: true,
            error_message: &None,
        };
        let mut pack = Pack::new();
        let out = ingest_agentic_turn_stream(
            &snap,
            0,
            |_| String::new(),
            "评审当前修改",
            &[],
            true,
            pack.ingest_mut(),
        );
        assert_eq!(out, AgenticTurnIngestOutcome::Continue);
        assert!(pack.forced_factual_retry);
        assert!(pack.final_text.is_empty());
        assert_eq!(pack.messages.len(), 1);
    }

    // ── Cache token accumulation tests ───────────────────────────────────

    #[test]
    fn snapshot_captures_nonzero_cache_tokens() {
        let accum = ChatTurnSseAccum {
            prompt_tokens: 1000,
            completion_tokens: 500,
            cache_read_tokens: 250,
            cache_creation_tokens: 100,
            has_usage: true,
            ..Default::default()
        };
        let snap = agentic_turn_stream_snapshot_from_sse_accum(&accum, None);
        assert_eq!(snap.cache_read_tokens, 250);
        assert_eq!(snap.cache_creation_tokens, 100);
    }

    #[test]
    fn cache_tokens_accumulate_across_turns() {
        let snap1 = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: "turn1",
            tool_calls: &[],
            prompt_tokens: 100,
            completion_tokens: 50,
            cache_read_tokens: 80,
            cache_creation_tokens: 20,
            has_usage: true,
            error_message: &None,
        };
        let mut pack = Pack::new();
        let out = ingest_agentic_turn_stream(
            &snap1,
            0,
            |_| String::new(),
            "q1",
            &[],
            true,
            pack.ingest_mut(),
        );
        assert_eq!(out, AgenticTurnIngestOutcome::Break);
        assert_eq!(pack.total_cache_read, 80);
        assert_eq!(pack.total_cache_creation, 20);
        assert_eq!(pack.total_prompt, 100);
        assert_eq!(pack.total_completion, 50);

        // Second turn: cache tokens accumulate
        let snap2 = AgenticTurnStreamSnapshot {
            full_text: "turn2",
            prompt_tokens: 200,
            completion_tokens: 100,
            cache_read_tokens: 150,
            cache_creation_tokens: 30,
            ..snap1
        };
        let out2 = ingest_agentic_turn_stream(
            &snap2,
            0,
            |_| String::new(),
            "q2",
            &[],
            true,
            pack.ingest_mut(),
        );
        assert_eq!(out2, AgenticTurnIngestOutcome::Break);
        assert_eq!(pack.total_cache_read, 230);       // 80 + 150
        assert_eq!(pack.total_cache_creation, 50);     // 20 + 30
        assert_eq!(pack.total_prompt, 300);            // 100 + 200
        assert_eq!(pack.total_completion, 150);        // 50 + 100
    }

    #[test]
    fn zero_cache_tokens_dont_affect_accumulation() {
        let snap = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: "test",
            tool_calls: &[],
            prompt_tokens: 500,
            completion_tokens: 200,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            has_usage: true,
            error_message: &None,
        };
        let mut pack = Pack::new();
        ingest_agentic_turn_stream(
            &snap,
            0,
            |_| String::new(),
            "q",
            &[],
            true,
            pack.ingest_mut(),
        );
        assert_eq!(pack.total_cache_read, 0);
        assert_eq!(pack.total_cache_creation, 0);
        assert_eq!(pack.total_prompt, 500);
        assert_eq!(pack.total_completion, 200);
    }

    #[test]
    fn has_any_usage_propagated_with_cache_tokens() {
        let snap = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: "response",
            tool_calls: &[],
            prompt_tokens: 500,
            completion_tokens: 200,
            cache_read_tokens: 400,
            cache_creation_tokens: 50,
            has_usage: true,
            error_message: &None,
        };
        let mut pack = Pack::new();
        assert!(!pack.has_any_usage);
        ingest_agentic_turn_stream(
            &snap,
            0,
            |_| String::new(),
            "q",
            &[],
            true,
            pack.ingest_mut(),
        );
        assert!(pack.has_any_usage);
        assert_eq!(pack.total_cache_read, 400);
        assert_eq!(pack.total_cache_creation, 50);
    }

    #[test]
    fn cache_tokens_accumulate_independently_of_prompt_completion() {
        // First turn: only cache_read, no cache_creation
        let snap1 = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: "t1",
            tool_calls: &[],
            prompt_tokens: 100,
            completion_tokens: 50,
            cache_read_tokens: 90,
            cache_creation_tokens: 0,
            has_usage: true,
            error_message: &None,
        };
        let mut pack = Pack::new();
        ingest_agentic_turn_stream(
            &snap1, 0, |_| String::new(), "q", &[], true, pack.ingest_mut(),
        );
        // Second turn: cache_creation but no cache_read
        let snap2 = AgenticTurnStreamSnapshot {
            full_text: "t2",
            cache_read_tokens: 0,
            cache_creation_tokens: 75,
            ..snap1
        };
        ingest_agentic_turn_stream(
            &snap2, 0, |_| String::new(), "q", &[], true, pack.ingest_mut(),
        );
        assert_eq!(pack.total_cache_read, 90);        // only from turn 1
        assert_eq!(pack.total_cache_creation, 75);     // only from turn 2
    }
}
