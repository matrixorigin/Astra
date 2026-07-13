//! Apply one `/chat/turn` stream outcome to agentic loop state (usage and response guards).
//!
//! Hosts (CLI `stream_chat_sse`, future headless clients) build [`AgenticTurnStreamSnapshot`] and pass
//! edge-tool names via `edge_round_len` + an index closure (`usize` → owned `String`) so closure
//! signatures stay lifetime-simple.

use std::collections::HashSet;

use astra_core::{agent_warn, canonical_names::normalize_optional_name};
use serde_json::Value;

use crate::chat_turn_sse_dispatch::ChatTurnSseAccum;
use crate::interaction_types::tool_counts_as_external_observation;
use crate::response_guard::{RESPONSE_GUARD_REDACTED_FINISH_REASON, apply_response_guards};
use crate::tool::args::shape::tool_call_name;
use astra_pipeline::step_recorder::StepRecorder;

/// Read-only slice of [`crate::chat_turn_sse_dispatch::ChatTurnSseAccum`] fields needed for ingest.
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
    /// Pre-classified error kind from the host. When `Some`, skip string re-classification.
    pub error_kind: Option<astra_core::ErrorKind>,
}

/// Build [`AgenticTurnStreamSnapshot`] from a [`ChatTurnSseAccum`] plus TTFT (CLI `TurnResult` derefs to accum).
#[must_use]
pub fn agentic_turn_stream_snapshot_from_sse_accum<'a>(
    accum: &'a ChatTurnSseAccum,
    ttft_ms: Option<u64>,
) -> AgenticTurnStreamSnapshot<'a> {
    agentic_turn_stream_snapshot_with_kind(accum, ttft_ms, accum.error_kind)
}

/// Build snapshot with an optional pre-classified error kind.
#[must_use]
pub fn agentic_turn_stream_snapshot_with_kind<'a>(
    accum: &'a ChatTurnSseAccum,
    ttft_ms: Option<u64>,
    error_kind: Option<astra_core::ErrorKind>,
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
        error_kind,
    }
}

/// Mutable agentic-loop fields updated by [`ingest_agentic_turn_stream`].
pub struct AgenticTurnIngestMut<'a> {
    pub first_ttft_ms: &'a mut Option<u64>,
    pub current_session_id: &'a mut Option<String>,
    pub current_run_id: &'a mut Option<String>,
    pub final_text: &'a mut String,
    pub last_finish_reason: &'a mut Option<String>,
    pub total_prompt: &'a mut u64,
    pub total_completion: &'a mut u64,
    pub total_cache_read: &'a mut u64,
    pub total_cache_creation: &'a mut u64,
    pub total_tool_calls: &'a mut u32,
    pub total_observation_tool_calls: &'a mut u32,
    pub step_recorder: &'a mut StepRecorder,
    pub all_tools_used: &'a mut HashSet<String>,
    pub has_any_usage: &'a mut bool,
    pub messages: &'a mut Vec<Value>,
    /// See [`crate::agentic_loop_host_types::AgenticLoopState::last_measured_prompt_tokens`].
    pub last_measured_prompt_tokens: &'a mut Option<u64>,
    /// See [`crate::agentic_loop_host_types::AgenticLoopState::consecutive_context_window_errors`].
    pub consecutive_context_window_errors: &'a mut u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AgenticTurnIngestOutcome {
    Break,
    Continue,
    Fatal(astra_core::ClassifiedError),
    HasToolCalls,
}

/// Maps [`AgenticTurnIngestOutcome`] to multi-turn loop control (hosts map break/continue to their enums).
#[derive(Debug, PartialEq, Eq)]
pub enum AgenticIngestIterationControl {
    Fatal(astra_core::ClassifiedError),
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

/// Merge streaming turn metadata into running totals and decide whether to finish or run tools.
pub fn ingest_agentic_turn_stream(
    snap: &AgenticTurnStreamSnapshot<'_>,
    edge_round_len: usize,
    mut edge_tool_name: impl FnMut(usize) -> String,
    message: &str,
    _recent_tools: &[String],
    _quiet: bool,
    mut st: AgenticTurnIngestMut<'_>,
) -> AgenticTurnIngestOutcome {
    if st.first_ttft_ms.is_none() {
        *st.first_ttft_ms = snap.ttft_ms;
    }

    if let Some(sid) = snap.session_id.as_ref() {
        *st.current_session_id = Some(sid.clone());
        st.step_recorder.attach_persistence_if_configured(sid);
    }
    if snap.run_id.is_some() {
        *st.current_run_id = snap.run_id.clone();
    }
    let round_has_edge_work = !snap.tool_calls.is_empty() || edge_round_len > 0;
    let preserve_prior_final_after_runtime_scaffolding_retry = !snap.full_text.is_empty()
        && !round_has_edge_work
        && should_preserve_prior_final_after_runtime_scaffolding_retry(
            st.messages,
            st.final_text.as_str(),
        );

    // Only text-only responses are final answers. Text that accompanies tool
    // calls is an intermediate preamble; surfacing it as `final_text` makes
    // interrupted/budget-exhausted turns look successfully completed.
    if !snap.full_text.is_empty()
        && !round_has_edge_work
        && !preserve_prior_final_after_runtime_scaffolding_retry
    {
        *st.final_text = snap.full_text.to_string();
    }

    if !snap.full_text.is_empty()
        && !round_has_edge_work
        && !preserve_prior_final_after_runtime_scaffolding_retry
    {
        let guard = apply_response_guards(st.final_text.as_str(), snap.tool_calls, &[], message);
        if let Some(replacement) = guard.replacement {
            agent_warn!("response_guard", "Guard triggered, replacing LLM output");
            *st.final_text = replacement;
            *st.last_finish_reason = Some(RESPONSE_GUARD_REDACTED_FINISH_REASON.to_string());
            persist_final_assistant_message(st.messages, st.final_text.as_str());
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
        if guard.quality.has_repetition_loop {
            agent_warn!(
                "response_guard",
                "Repeated-token loop detected and preserved as advisory evidence"
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
    let observation_tool_calls_this_round = if !snap.tool_calls.is_empty() {
        snap.tool_calls
            .iter()
            .filter_map(tool_call_name)
            .filter(|name| tool_counts_as_external_observation(name))
            .count()
    } else {
        let mut count = 0usize;
        for i in 0..edge_round_len {
            if tool_counts_as_external_observation(&edge_tool_name(i)) {
                count += 1;
            }
        }
        count
    };
    *st.total_observation_tool_calls += observation_tool_calls_this_round as u32;

    st.step_recorder
        .record_tokens(snap.prompt_tokens, snap.completion_tokens);

    for tc in snap.tool_calls {
        if let Some(name) = tool_call_name(tc) {
            insert_tool_used(st.all_tools_used, name.to_string());
        }
    }
    for i in 0..edge_round_len {
        insert_tool_used(st.all_tools_used, edge_tool_name(i));
    }
    *st.has_any_usage = *st.has_any_usage || snap.has_usage;

    // Reset context window error counter on successful turn — prevents
    // stale counter from escalating compaction on a later unrelated error.
    if snap.error_message.is_none() {
        *st.consecutive_context_window_errors = 0;
    }

    if let Some(err) = snap.error_message {
        // Use pre-classified error_kind when available (from HostTurnResult),
        // falling back to string-based classification for SSE-path errors.
        let kind = if let Some(k) = snap.error_kind {
            if k == astra_core::ErrorKind::ContextWindow {
                *st.consecutive_context_window_errors =
                    st.consecutive_context_window_errors.saturating_add(1);
            } else {
                *st.consecutive_context_window_errors = 0;
            }
            k
        } else {
            if astra_core::is_llm_context_window_error(err) {
                *st.consecutive_context_window_errors =
                    st.consecutive_context_window_errors.saturating_add(1);
                astra_core::ErrorKind::ContextWindow
            } else {
                *st.consecutive_context_window_errors = 0;
                astra_core::classify_llm_error_message(err)
            }
        };
        return AgenticTurnIngestOutcome::Fatal(astra_core::ClassifiedError::new(
            kind,
            err.clone(),
        ));
    }

    if !round_has_edge_work {
        if !snap.full_text.is_empty() && !preserve_prior_final_after_runtime_scaffolding_retry {
            persist_final_assistant_message(st.messages, st.final_text.as_str());
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
    let billable_input = snap
        .prompt_tokens
        .saturating_add(snap.cache_read_tokens)
        .saturating_add(snap.cache_creation_tokens);
    if snap.has_usage && billable_input > 0 {
        *st.last_measured_prompt_tokens = Some(billable_input);
    }
}

fn insert_tool_used(target: &mut HashSet<String>, name: String) {
    if let Some(name) = normalize_optional_name(Some(name)) {
        target.insert(name);
    }
}

fn persist_final_assistant_message(messages: &mut Vec<Value>, final_text: &str) {
    if final_text.is_empty() {
        return;
    }
    messages.push(serde_json::json!({
        "role": "assistant",
        "content": final_text,
    }));
}

fn should_preserve_prior_final_after_runtime_scaffolding_retry(
    messages: &[Value],
    prior_final_text: &str,
) -> bool {
    if prior_final_text.trim().is_empty() {
        return false;
    }

    let mut saw_runtime_scaffolding = false;
    for msg in messages.iter().rev() {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        let content = msg
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if crate::runtime_scaffolding::is_continuation_scaffolding_for_role(role, content) {
            saw_runtime_scaffolding = true;
            continue;
        }
        return saw_runtime_scaffolding && role == "assistant" && !content.trim().is_empty();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_USER_ID: &str = "test-user";
    use serde_json::json;

    struct Pack {
        first_ttft_ms: Option<u64>,
        current_session_id: Option<String>,
        current_run_id: Option<String>,
        final_text: String,
        last_finish_reason: Option<String>,
        total_prompt: u64,
        total_completion: u64,
        total_cache_read: u64,
        total_cache_creation: u64,
        total_tool_calls: u32,
        total_observation_tool_calls: u32,
        step_recorder: StepRecorder,
        all_tools_used: HashSet<String>,
        has_any_usage: bool,
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
                last_finish_reason: None,
                total_prompt: 0,
                total_completion: 0,
                total_cache_read: 0,
                total_cache_creation: 0,
                total_tool_calls: 0,
                total_observation_tool_calls: 0,
                step_recorder: StepRecorder::with_persistence(TEST_USER_ID, "s", "t"),
                all_tools_used: HashSet::new(),
                has_any_usage: false,
                messages: Vec::new(),
                last_measured_prompt_tokens: None,
                consecutive_context_window_errors: 0,
            }
        }

        fn ingest_mut(&mut self) -> AgenticTurnIngestMut<'_> {
            AgenticTurnIngestMut {
                first_ttft_ms: &mut self.first_ttft_ms,
                current_session_id: &mut self.current_session_id,
                current_run_id: &mut self.current_run_id,
                final_text: &mut self.final_text,
                last_finish_reason: &mut self.last_finish_reason,
                total_prompt: &mut self.total_prompt,
                total_completion: &mut self.total_completion,
                total_cache_read: &mut self.total_cache_read,
                total_cache_creation: &mut self.total_cache_creation,
                total_tool_calls: &mut self.total_tool_calls,
                total_observation_tool_calls: &mut self.total_observation_tool_calls,
                step_recorder: &mut self.step_recorder,
                all_tools_used: &mut self.all_tools_used,
                has_any_usage: &mut self.has_any_usage,
                messages: &mut self.messages,
                last_measured_prompt_tokens: &mut self.last_measured_prompt_tokens,
                consecutive_context_window_errors: &mut self.consecutive_context_window_errors,
            }
        }
    }

    #[test]
    fn ingest_attaches_step_persistence_using_recorder_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let mut p = Pack::new();
        p.step_recorder =
            StepRecorder::with_deferred_persistence(TEST_USER_ID, "ephemeral", "task-1");

        let session_id = Some("authoritative-session".to_string());
        let run_id = None;
        let error_message = None;
        let tool_calls = Vec::new();
        let snap = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &session_id,
            run_id: &run_id,
            full_text: "",
            tool_calls: &tool_calls,
            prompt_tokens: 0,
            completion_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            has_usage: false,
            error_message: &error_message,
            error_kind: None,
        };

        let outcome = ingest_agentic_turn_stream(
            &snap,
            0,
            |_| String::new(),
            "continue this session",
            &[],
            true,
            p.ingest_mut(),
        );

        assert_eq!(outcome, AgenticTurnIngestOutcome::Break);
        assert_eq!(
            p.current_session_id.as_deref(),
            Some("authoritative-session")
        );
        let summary = p.step_recorder.summary();
        assert_eq!(summary.user_id, TEST_USER_ID);
        assert_eq!(summary.session_id, "authoritative-session");
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
            error_kind: Some(astra_core::ErrorKind::MissingModelSelection),
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
        assert_eq!(
            snap.error_kind,
            Some(astra_core::ErrorKind::MissingModelSelection)
        );
    }

    #[test]
    fn map_ingest_outcome_control() {
        let err = astra_core::ClassifiedError::new(astra_core::ErrorKind::Unknown, "x");
        assert_eq!(
            map_ingest_outcome_to_iteration_control(AgenticTurnIngestOutcome::Fatal(err.clone())),
            AgenticIngestIterationControl::Fatal(err)
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
            error_kind: None,
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
        assert!(matches!(out, AgenticTurnIngestOutcome::Fatal(ref e) if e.message == "boom"));
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
            error_kind: None,
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
        assert!(matches!(out, AgenticTurnIngestOutcome::Fatal(ref e)
            if e.kind == astra_core::ErrorKind::ContextWindow
            && e.message == "prompt is too long"
        ));
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
            error_kind: None,
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
        assert!(
            matches!(out, AgenticTurnIngestOutcome::Fatal(ref e) if e.message == "rate limited")
        );
        assert_eq!(pack.consecutive_context_window_errors, 0);
    }

    #[test]
    fn fatal_unclassified_error_uses_core_llm_classifier() {
        let cases = [
            (
                "Error: pool timed out while waiting for an open connection",
                astra_core::ErrorKind::ConnectionPoolExhausted,
            ),
            (
                "Error: Could not validate credentials",
                astra_core::ErrorKind::Auth,
            ),
            (
                "The security token included in the request is expired",
                astra_core::ErrorKind::Auth,
            ),
        ];

        for (message, expected_kind) in cases {
            let err = Some(message.to_string());
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
                error_kind: None,
            };
            let mut pack = Pack::new();
            pack.consecutive_context_window_errors = 3;
            let out = ingest_agentic_turn_stream(
                &snap,
                0,
                |_| String::new(),
                "hi",
                &[],
                true,
                pack.ingest_mut(),
            );

            assert!(
                matches!(out, AgenticTurnIngestOutcome::Fatal(ref e)
                    if e.kind == expected_kind && e.message == message),
                "unexpected ingest outcome for {message:?}: {out:?}"
            );
            assert_eq!(pack.consecutive_context_window_errors, 0);
        }
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
            error_kind: None,
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
        assert_eq!(pack.messages.len(), 1, "final assistant should persist");
        assert_eq!(pack.messages[0]["role"], "assistant");
        assert_eq!(pack.messages[0]["content"], "ok");
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
            error_kind: None,
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
            error_kind: None,
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
    fn has_tool_calls_canonicalizes_tool_names_before_recording() {
        let tcs = vec![
            json!({"name": " read_file ", "arguments": {}}),
            json!({"name": "read_file", "arguments": {}}),
            json!({"name": " ", "arguments": {}}),
        ];
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
            error_kind: None,
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
        assert_eq!(pack.total_tool_calls, 3);
        assert_eq!(
            pack.all_tools_used,
            HashSet::from(["read_file".to_string()])
        );
    }

    #[test]
    fn edge_tool_names_are_canonicalized_before_recording() {
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
            error_kind: None,
        };
        let mut pack = Pack::new();
        let tool_names = [" bash ", "bash", " "];
        let out = ingest_agentic_turn_stream(
            &snap,
            tool_names.len(),
            |i| tool_names[i].to_string(),
            "hi",
            &[],
            true,
            pack.ingest_mut(),
        );

        assert_eq!(out, AgenticTurnIngestOutcome::HasToolCalls);
        assert_eq!(pack.total_tool_calls, 3);
        assert_eq!(pack.all_tools_used, HashSet::from(["bash".to_string()]));
    }

    #[test]
    fn has_tool_calls_from_canonical_server_tool_calls() {
        let tcs = vec![json!({
            "id": "call_1",
            "type": "function",
            "function": {
                "name": "read_file",
                "arguments": "{\"path\":\"a.rs\"}"
            }
        })];
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
            error_kind: None,
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
    fn tool_turn_draft_text_does_not_become_final_output() {
        let tcs = vec![json!({"name": "bash", "arguments": {}})];
        let snap = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: "intermediate analysis before tool calls",
            tool_calls: &tcs,
            prompt_tokens: 1,
            completion_tokens: 2,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            has_usage: true,
            error_message: &None,
            error_kind: None,
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
        assert_eq!(
            pack.final_text, "previous stable answer",
            "tool-call preambles are intermediate and must not overwrite the user-visible final answer"
        );
    }

    #[test]
    fn no_tool_runtime_scaffolding_retry_preserves_prior_final_answer() {
        let prior_answer = "Final answer that satisfies the user's request.";
        let snap = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: "Runtime follow-up without any new tool evidence.",
            tool_calls: &[],
            prompt_tokens: 10,
            completion_tokens: 5,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            has_usage: true,
            error_message: &None,
            error_kind: None,
        };
        let mut pack = Pack::new();
        pack.final_text = prior_answer.to_string();
        pack.messages.push(json!({
            "role": "assistant",
            "content": prior_answer,
        }));
        pack.messages.push(json!({
            "role": "user",
            "content": "⚠️ VERIFICATION REQUIRED: Before you finish, run any missing checks",
        }));
        pack.messages.push(json!({
            "role": "user",
            "content": "## ⚡ Self-Status\nTurn 9/299 | Token pressure: 5% | Cache: 86%",
        }));

        let out = ingest_agentic_turn_stream(
            &snap,
            0,
            |_| String::new(),
            "give advice only",
            &[],
            true,
            pack.ingest_mut(),
        );

        assert_eq!(out, AgenticTurnIngestOutcome::Break);
        assert_eq!(pack.final_text, prior_answer);
        assert_eq!(
            pack.messages.len(),
            3,
            "no-tool runtime-scaffolding retries must not append or replace the user-visible answer"
        );
    }

    #[test]
    fn no_tool_regular_user_followup_can_replace_prior_final_answer() {
        let snap = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: "Updated answer for the user's follow-up.",
            tool_calls: &[],
            prompt_tokens: 10,
            completion_tokens: 5,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            has_usage: true,
            error_message: &None,
            error_kind: None,
        };
        let mut pack = Pack::new();
        pack.final_text = "Previous answer.".to_string();
        pack.messages.push(json!({
            "role": "assistant",
            "content": "Previous answer.",
        }));
        pack.messages.push(json!({
            "role": "user",
            "content": "Please adjust the recommendation.",
        }));

        let out = ingest_agentic_turn_stream(
            &snap,
            0,
            |_| String::new(),
            "Please adjust the recommendation.",
            &[],
            true,
            pack.ingest_mut(),
        );

        assert_eq!(out, AgenticTurnIngestOutcome::Break);
        assert_eq!(pack.final_text, "Updated answer for the user's follow-up.");
        assert_eq!(pack.messages.len(), 3);
    }

    #[test]
    fn text_only_model_output_finishes_without_synthetic_retry_history() {
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
            error_kind: None,
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
        assert_eq!(out, AgenticTurnIngestOutcome::Break);
        assert_eq!(pack.final_text, "Here are your recent PRs: ...");
        assert_eq!(pack.last_measured_prompt_tokens, Some(100));
        assert_eq!(pack.messages.len(), 1);
        assert_eq!(pack.messages[0]["role"], "assistant");
        assert_eq!(pack.messages[0]["content"], "Here are your recent PRs: ...");
    }

    #[test]
    fn ingest_replaces_internal_protocol_text_before_persisting_final() {
        let mut pack = Pack::new();
        let snapshot = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: "<ask_astra_data><query>previous task?</query></ask_astra_data>",
            tool_calls: &[],
            prompt_tokens: 10,
            completion_tokens: 20,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            has_usage: true,
            error_message: &None,
            error_kind: None,
        };

        let out = ingest_agentic_turn_stream(
            &snapshot,
            0,
            |_| String::new(),
            "hi",
            &[],
            true,
            pack.ingest_mut(),
        );

        assert_eq!(out, AgenticTurnIngestOutcome::Break);
        assert_eq!(
            pack.final_text,
            crate::response_guard::INTERNAL_PROTOCOL_FALLBACK
        );
        assert_eq!(
            pack.last_finish_reason.as_deref(),
            Some(crate::response_guard::RESPONSE_GUARD_REDACTED_FINISH_REASON)
        );
        assert_eq!(
            pack.messages.last().unwrap()["content"],
            crate::response_guard::INTERNAL_PROTOCOL_FALLBACK
        );
    }

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
            error_kind: None,
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
        assert_eq!(pack.total_cache_read, 230); // 80 + 150
        assert_eq!(pack.total_cache_creation, 50); // 20 + 30
        assert_eq!(pack.total_prompt, 300); // 100 + 200
        assert_eq!(pack.total_completion, 150); // 50 + 100
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
            error_kind: None,
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
            error_kind: None,
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
            error_kind: None,
        };
        let mut pack = Pack::new();
        ingest_agentic_turn_stream(
            &snap1,
            0,
            |_| String::new(),
            "q",
            &[],
            true,
            pack.ingest_mut(),
        );
        // Second turn: cache_creation but no cache_read
        let snap2 = AgenticTurnStreamSnapshot {
            full_text: "t2",
            cache_read_tokens: 0,
            cache_creation_tokens: 75,
            ..snap1
        };
        ingest_agentic_turn_stream(
            &snap2,
            0,
            |_| String::new(),
            "q",
            &[],
            true,
            pack.ingest_mut(),
        );
        assert_eq!(pack.total_cache_read, 90); // only from turn 1
        assert_eq!(pack.total_cache_creation, 75); // only from turn 2
    }

    /// P1-B: consecutive_context_window_errors must reset to 0 on a successful
    /// turn. Without this, a context error → success → later context error
    /// escalates compaction unnecessarily.
    #[test]
    fn consecutive_context_window_errors_resets_on_success() {
        let mut pack = Pack::new();
        let session_id = None;
        let run_id = None;

        // Turn 1: context window error → counter should be 1
        let err_msg = Some("maximum context length exceeded".to_string());
        let snap = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &session_id,
            run_id: &run_id,
            full_text: "",
            tool_calls: &[],
            prompt_tokens: 100,
            completion_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            has_usage: true,
            error_message: &err_msg,
            error_kind: None,
        };
        let outcome = ingest_agentic_turn_stream(
            &snap,
            0,
            |_| String::new(),
            "",
            &[],
            true,
            pack.ingest_mut(),
        );
        assert!(matches!(outcome, AgenticTurnIngestOutcome::Fatal(_)));
        assert_eq!(
            pack.consecutive_context_window_errors, 1,
            "counter must be 1 after context window error"
        );

        // Turn 2: successful turn (no error) → counter must reset to 0
        let no_err: Option<String> = None;
        let snap_ok = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &session_id,
            run_id: &run_id,
            full_text: "done",
            tool_calls: &[],
            prompt_tokens: 50,
            completion_tokens: 10,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            has_usage: true,
            error_message: &no_err,
            error_kind: None,
        };
        let outcome = ingest_agentic_turn_stream(
            &snap_ok,
            0,
            |_| String::new(),
            "done",
            &[],
            true,
            pack.ingest_mut(),
        );
        assert!(matches!(outcome, AgenticTurnIngestOutcome::Break));
        assert_eq!(
            pack.consecutive_context_window_errors, 0,
            "counter must reset to 0 after successful turn"
        );
    }
}
