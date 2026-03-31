//! Apply one `/chat/turn` stream outcome to agentic loop state (usage, response guards, factual retry).
//!
//! Hosts (CLI `stream_chat_sse`, future headless clients) build [`AgenticTurnStreamSnapshot`] and pass
//! edge-tool names via `edge_round_len` + an index closure (`usize` → owned `String`) so this crate
//! stays free of `EdgeToolRoundEntry` and closure signatures stay lifetime-simple.

use std::collections::HashSet;

use mo_agent_core::agent_warn;
use serde_json::Value;

use super::chat_turn_heuristics::{
    openai_factual_tool_retry_user_message, should_force_factual_tool_retry,
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
        has_usage: accum.has_usage,
        error_message: &accum.error_message,
    }
}

/// Mutable agentic-loop fields updated by [`ingest_agentic_turn_stream`].
pub struct AgenticTurnIngestMut<'a> {
    pub first_ttft_ms: &'a mut Option<u64>,
    pub current_session_id: &'a mut Option<String>,
    pub current_run_id: &'a mut Option<String>,
    pub final_text: &'a mut String,
    pub total_prompt: &'a mut u64,
    pub total_completion: &'a mut u64,
    pub total_tool_calls: &'a mut u32,
    pub step_recorder: &'a mut StepRecorder,
    pub all_tools_used: &'a mut HashSet<String>,
    pub has_any_usage: &'a mut bool,
    pub forced_factual_retry: &'a mut bool,
    pub messages: &'a mut Vec<Value>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AgenticTurnIngestOutcome {
    Break,
    Continue,
    Fatal(String),
    HasToolCalls,
}

/// Merge streaming turn metadata into running totals and decide whether to break, retry, or run tools.
pub fn ingest_agentic_turn_stream(
    snap: &AgenticTurnStreamSnapshot<'_>,
    edge_round_len: usize,
    mut edge_tool_name: impl FnMut(usize) -> String,
    message: &str,
    recent_tools: &[String],
    quiet: bool,
    st: AgenticTurnIngestMut<'_>,
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
    if !snap.full_text.is_empty() {
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
        return AgenticTurnIngestOutcome::Fatal(err.clone());
    }

    let round_has_edge_work = !snap.tool_calls.is_empty() || edge_round_len > 0;
    if !round_has_edge_work {
        if should_force_factual_tool_retry(
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
            return AgenticTurnIngestOutcome::Continue;
        }
        return AgenticTurnIngestOutcome::Break;
    }

    AgenticTurnIngestOutcome::HasToolCalls
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
        total_tool_calls: u32,
        step_recorder: StepRecorder,
        all_tools_used: HashSet<String>,
        has_any_usage: bool,
        forced_factual_retry: bool,
        messages: Vec<Value>,
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
                total_tool_calls: 0,
                step_recorder: StepRecorder::with_persistence("s", "t"),
                all_tools_used: HashSet::new(),
                has_any_usage: false,
                forced_factual_retry: false,
                messages: Vec::new(),
            }
        }

        fn ingest_mut(&mut self) -> AgenticTurnIngestMut<'_> {
            AgenticTurnIngestMut {
                first_ttft_ms: &mut self.first_ttft_ms,
                current_session_id: &mut self.current_session_id,
                current_run_id: &mut self.current_run_id,
                final_text: &mut self.final_text,
                total_prompt: &mut self.total_prompt,
                total_completion: &mut self.total_completion,
                total_tool_calls: &mut self.total_tool_calls,
                step_recorder: &mut self.step_recorder,
                all_tools_used: &mut self.all_tools_used,
                has_any_usage: &mut self.has_any_usage,
                forced_factual_retry: &mut self.forced_factual_retry,
                messages: &mut self.messages,
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
}
