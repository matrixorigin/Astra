//! Apply one `/chat/turn` stream outcome to agentic loop state (usage, response guards, factual retry).
//!
//! Hosts (CLI `stream_chat_sse`, future headless clients) build [`AgenticTurnStreamSnapshot`] and pass
//! edge-tool names via `edge_round_len` + an index closure (`usize` → owned `String`) so closure
//! signatures stay lifetime-simple.

use std::collections::HashSet;

use astra_core::agent_warn;
use serde_json::Value;

use crate::chat_turn_heuristics::{
    TaskExecutionProfile, openai_factual_tool_retry_user_message, should_force_factual_tool_retry,
};
use crate::chat_turn_sse_dispatch::ChatTurnSseAccum;
use crate::interaction_types::{TurnInteractionPolicy, tool_counts_as_factual_evidence};
use crate::response_guard::apply_response_guards;
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
    pub task_profile: TaskExecutionProfile,
    pub step_persistence_enabled: bool,
    pub first_ttft_ms: &'a mut Option<u64>,
    pub current_session_id: &'a mut Option<String>,
    pub current_run_id: &'a mut Option<String>,
    pub final_text: &'a mut String,
    pub total_prompt: &'a mut u64,
    pub total_completion: &'a mut u64,
    pub total_cache_read: &'a mut u64,
    pub total_cache_creation: &'a mut u64,
    pub total_tool_calls: &'a mut u32,
    pub total_evidence_tool_calls: &'a mut u32,
    pub step_recorder: &'a mut StepRecorder,
    pub all_tools_used: &'a mut HashSet<String>,
    pub has_any_usage: &'a mut bool,
    pub forced_factual_retry: &'a mut bool,
    pub factual_retry_fallback_text: &'a mut Option<String>,
    pub factual_retry_fallback_decision: Option<FactualRetryFallbackDecision>,
    pub messages: &'a mut Vec<Value>,
    /// See [`crate::agentic_loop_host_types::AgenticLoopState::last_measured_prompt_tokens`].
    pub last_measured_prompt_tokens: &'a mut Option<u64>,
    /// See [`crate::agentic_loop_host_types::AgenticLoopState::consecutive_context_window_errors`].
    pub consecutive_context_window_errors: &'a mut u32,
    /// Actual user-interaction + visible-tool policy for the just-finished turn.
    pub turn_policy: TurnInteractionPolicy,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AgenticTurnIngestOutcome {
    Break,
    Continue,
    Fatal(astra_core::ClassifiedError),
    HasToolCalls,
}

/// Explicit response-selection decision for a factual retry with no real
/// evidence. This is intentionally an input to ingest, not computed inside it:
/// only an upstream judge/policy layer may decide that the pre-retry answer is
/// better than the retry output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactualRetryFallbackDecision {
    RestoreFallback,
    KeepRetry,
}

#[derive(Debug, Clone, Copy)]
pub struct FactualRetryFallbackJudgeInput<'a> {
    pub original_query: &'a str,
    pub fallback_text: &'a str,
    pub retry_text: &'a str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FactualRetryFallbackJudgeVerdict {
    pub decision: FactualRetryFallbackDecision,
    pub confidence: f64,
    pub reason: Option<String>,
}

impl FactualRetryFallbackJudgeVerdict {
    #[must_use]
    pub fn accepted_decision(&self) -> FactualRetryFallbackDecision {
        if self.decision == FactualRetryFallbackDecision::RestoreFallback
            && self.confidence < FACTUAL_RETRY_FALLBACK_MIN_CONFIDENCE
        {
            FactualRetryFallbackDecision::KeepRetry
        } else {
            self.decision
        }
    }
}

pub const FACTUAL_RETRY_FALLBACK_MIN_CONFIDENCE: f64 = 0.70;

#[must_use]
pub fn factual_retry_fallback_judge_messages(
    input: FactualRetryFallbackJudgeInput<'_>,
) -> Vec<Value> {
    vec![
        serde_json::json!({
            "role": "system",
            "content": "You are a response-selection judge for an agentic coding assistant. \
                Choose which assistant answer should be shown to the user after a forced factual retry. \
                Output only JSON."
        }),
        serde_json::json!({
            "role": "user",
            "content": format!(
                "The assistant first answered without tools. The runtime forced a retry, but the retry gathered no real evidence tools.\n\n\
                 Original user query:\n{original_query}\n\n\
                 Candidate A: original text-only answer before retry:\n{fallback_text}\n\n\
                 Candidate B: answer produced by the retry:\n{retry_text}\n\n\
                 Decide which candidate should be shown.\n\n\
                 Rules:\n\
                 - Return restore_fallback only when Candidate A clearly answers the user's actual question and Candidate B does not.\n\
                 - Return keep_retry when Candidate B answers the question, correctly admits lack of evidence, or when Candidate A makes an unverifiable live-data claim.\n\
                 - Return keep_retry when uncertain.\n\n\
                 Output JSON exactly like:\n\
                 {{\"decision\":\"restore_fallback\"|\"keep_retry\",\"confidence\":0.0,\"reason\":\"short reason\"}}",
                original_query = input.original_query,
                fallback_text = input.fallback_text,
                retry_text = input.retry_text,
            )
        }),
    ]
}

pub fn parse_factual_retry_fallback_judge_response(
    raw: &str,
) -> Result<FactualRetryFallbackJudgeVerdict, String> {
    let json_text = extract_first_json_object(raw)
        .ok_or_else(|| format!("judge response did not contain a JSON object: {raw}"))?;
    let value: Value = serde_json::from_str(json_text)
        .map_err(|err| format!("judge response was not valid JSON: {err}: {raw}"))?;
    let decision = match value.get("decision").and_then(Value::as_str) {
        Some("restore_fallback") => FactualRetryFallbackDecision::RestoreFallback,
        Some("keep_retry") => FactualRetryFallbackDecision::KeepRetry,
        other => {
            return Err(format!(
                "judge response had invalid decision {other:?}: {raw}"
            ));
        }
    };
    let confidence = value
        .get("confidence")
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("judge response missing numeric confidence: {raw}"))?
        .clamp(0.0, 1.0);
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);

    Ok(FactualRetryFallbackJudgeVerdict {
        decision,
        confidence,
        reason,
    })
}

fn extract_first_json_object(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    (start <= end).then_some(&raw[start..=end])
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
        if st.step_persistence_enabled {
            st.step_recorder.attach_persistence(sid);
        }
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
    let evidence_tool_calls_this_round = if !snap.tool_calls.is_empty() {
        snap.tool_calls
            .iter()
            .filter_map(tool_call_name)
            .filter(|name| tool_counts_as_factual_evidence(name))
            .count()
    } else {
        let mut count = 0usize;
        for i in 0..edge_round_len {
            if tool_counts_as_factual_evidence(&edge_tool_name(i)) {
                count += 1;
            }
        }
        count
    };
    *st.total_evidence_tool_calls += evidence_tool_calls_this_round as u32;

    st.step_recorder
        .record_tokens(snap.prompt_tokens, snap.completion_tokens);

    for tc in snap.tool_calls {
        if let Some(name) = tool_call_name(tc) {
            st.all_tools_used.insert(name.to_string());
        }
    }
    for i in 0..edge_round_len {
        st.all_tools_used.insert(edge_tool_name(i));
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
            let lower = err.to_lowercase();
            if astra_core::is_context_window_error(&lower) {
                *st.consecutive_context_window_errors =
                    st.consecutive_context_window_errors.saturating_add(1);
                astra_core::ErrorKind::ContextWindow
            } else {
                *st.consecutive_context_window_errors = 0;
                astra_core::classify_llm_error(err)
            }
        };
        return AgenticTurnIngestOutcome::Fatal(astra_core::ClassifiedError::new(
            kind,
            err.clone(),
        ));
    }

    if !round_has_edge_work {
        if *st.forced_factual_retry
            && *st.total_evidence_tool_calls == 0
            && st
                .factual_retry_fallback_text
                .as_ref()
                .is_some_and(|text| !text.trim().is_empty())
            && matches!(
                st.factual_retry_fallback_decision,
                Some(FactualRetryFallbackDecision::RestoreFallback)
            )
        {
            let fallback_text = st
                .factual_retry_fallback_text
                .take()
                .expect("fallback presence checked before restore");
            *st.final_text = fallback_text;
            persist_final_assistant_message(st.messages, st.final_text.as_str());
            record_prompt_calibration_success(snap, &mut st);
            return AgenticTurnIngestOutcome::Break;
        }
        if should_force_factual_tool_retry(
            st.task_profile,
            message,
            recent_tools,
            *st.total_evidence_tool_calls,
            *st.forced_factual_retry,
            &st.turn_policy,
        ) {
            *st.forced_factual_retry = true;
            if !st.final_text.trim().is_empty() {
                *st.factual_retry_fallback_text = Some(st.final_text.clone());
            }
            if !quiet {
                eprintln!("  ↻ Explicit evidence retry requested; forcing one corrective retry…");
            }
            st.messages.push(openai_factual_tool_retry_user_message(
                message,
                &st.turn_policy,
            ));
            st.final_text.clear();
            record_prompt_calibration_success(snap, &mut st);
            return AgenticTurnIngestOutcome::Continue;
        }
        if !snap.full_text.is_empty() && !preserve_prior_final_after_runtime_scaffolding_retry {
            persist_final_assistant_message(st.messages, st.final_text.as_str());
        }
        st.factual_retry_fallback_text.take();
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
    use crate::interaction_types::TurnInteractionMode;
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
        total_evidence_tool_calls: u32,
        step_recorder: StepRecorder,
        all_tools_used: HashSet<String>,
        has_any_usage: bool,
        forced_factual_retry: bool,
        factual_retry_fallback_text: Option<String>,
        factual_retry_fallback_decision: Option<FactualRetryFallbackDecision>,
        messages: Vec<Value>,
        last_measured_prompt_tokens: Option<u64>,
        consecutive_context_window_errors: u32,
        turn_policy: TurnInteractionPolicy,
        task_profile: TaskExecutionProfile,
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
                total_evidence_tool_calls: 0,
                step_recorder: StepRecorder::with_persistence(TEST_USER_ID, "s", "t"),
                all_tools_used: HashSet::new(),
                has_any_usage: false,
                forced_factual_retry: false,
                factual_retry_fallback_text: None,
                factual_retry_fallback_decision: None,
                messages: Vec::new(),
                last_measured_prompt_tokens: None,
                consecutive_context_window_errors: 0,
                turn_policy: TurnInteractionPolicy::from_visible_tool_names(
                    TurnInteractionMode::Deny,
                    vec!["read_file".into()],
                ),
                task_profile: TaskExecutionProfile::default(),
            }
        }

        fn ingest_mut(&mut self) -> AgenticTurnIngestMut<'_> {
            self.ingest_mut_with_persistence(false)
        }

        fn ingest_mut_with_persistence(
            &mut self,
            step_persistence_enabled: bool,
        ) -> AgenticTurnIngestMut<'_> {
            AgenticTurnIngestMut {
                task_profile: self.task_profile,
                step_persistence_enabled,
                first_ttft_ms: &mut self.first_ttft_ms,
                current_session_id: &mut self.current_session_id,
                current_run_id: &mut self.current_run_id,
                final_text: &mut self.final_text,
                total_prompt: &mut self.total_prompt,
                total_completion: &mut self.total_completion,
                total_cache_read: &mut self.total_cache_read,
                total_cache_creation: &mut self.total_cache_creation,
                total_tool_calls: &mut self.total_tool_calls,
                total_evidence_tool_calls: &mut self.total_evidence_tool_calls,
                step_recorder: &mut self.step_recorder,
                all_tools_used: &mut self.all_tools_used,
                has_any_usage: &mut self.has_any_usage,
                forced_factual_retry: &mut self.forced_factual_retry,
                factual_retry_fallback_text: &mut self.factual_retry_fallback_text,
                factual_retry_fallback_decision: self.factual_retry_fallback_decision,
                messages: &mut self.messages,
                last_measured_prompt_tokens: &mut self.last_measured_prompt_tokens,
                consecutive_context_window_errors: &mut self.consecutive_context_window_errors,
                turn_policy: self.turn_policy.clone(),
            }
        }
    }

    #[test]
    fn ingest_attaches_step_persistence_using_recorder_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let mut p = Pack::new();
        p.step_recorder = StepRecorder::new(TEST_USER_ID, "ephemeral", "task-1");

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
            p.ingest_mut_with_persistence(true),
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
    fn factual_retry_fallback_judge_prompt_and_parser_contract() {
        let messages = factual_retry_fallback_judge_messages(FactualRetryFallbackJudgeInput {
            original_query: "what do 59% and 117k mean?",
            fallback_text: "59% is context usage; 117k is token count.",
            retry_text: "I completed the requested work.",
        });
        assert_eq!(messages.len(), 2);
        let prompt = messages[1]["content"].as_str().unwrap();
        assert!(prompt.contains("Original user query"));
        assert!(prompt.contains("Candidate A"));
        assert!(prompt.contains("Candidate B"));
        assert!(prompt.contains("\"decision\":\"restore_fallback\"|\"keep_retry\""));

        let verdict = parse_factual_retry_fallback_judge_response(
            r#"```json
            {"decision":"restore_fallback","confidence":0.91,"reason":"A answers the question"}
            ```"#,
        )
        .expect("valid judge JSON");
        assert_eq!(
            verdict.accepted_decision(),
            FactualRetryFallbackDecision::RestoreFallback
        );
        assert_eq!(verdict.reason.as_deref(), Some("A answers the question"));
    }

    #[test]
    fn factual_retry_fallback_judge_low_confidence_restore_fails_closed() {
        let verdict = parse_factual_retry_fallback_judge_response(
            r#"{"decision":"restore_fallback","confidence":0.41}"#,
        )
        .expect("valid judge JSON");
        assert_eq!(
            verdict.accepted_decision(),
            FactualRetryFallbackDecision::KeepRetry
        );
    }

    #[test]
    fn factual_retry_fallback_judge_rejects_malformed_decisions() {
        let err = parse_factual_retry_fallback_judge_response(
            r#"{"decision":"maybe","confidence":0.99}"#,
        )
        .expect_err("unknown decisions must fail closed");
        assert!(err.contains("invalid decision"), "{err}");
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

    // ─── Factual retry injection tests ──────────────────────────────────────

    #[test]
    fn factual_retry_injects_nudge_and_clears_text() {
        // When policy explicitly requires factual retry and the LLM produced no
        // evidence tool calls, ingest should inject a retry nudge and return
        // Continue.
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
        pack.task_profile.allow_factual_retry = true;
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
        assert_eq!(
            pack.factual_retry_fallback_text.as_deref(),
            Some("Here are your recent PRs: ...")
        );
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
    fn factual_retry_is_reachable_from_workspace_evidence_profile() {
        let snap = AgenticTurnStreamSnapshot {
            ttft_ms: Some(50),
            session_id: &None,
            run_id: &None,
            full_text: "The latest PR looks green.",
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
        pack.task_profile =
            crate::chat_turn_heuristics::infer_task_execution_profile("show me the latest PR");

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
        assert!(pack.forced_factual_retry);
        assert_eq!(
            pack.factual_retry_fallback_text.as_deref(),
            Some("The latest PR looks green.")
        );
    }

    #[test]
    fn factual_retry_does_not_fire_for_status_line_concept_profile() {
        let snap = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: "59% is context usage; 117k is the token count.",
            tool_calls: &[],
            prompt_tokens: 10,
            completion_tokens: 20,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            has_usage: true,
            error_message: &None,
            error_kind: None,
        };
        let query = "what do 59% and 117k mean in the status line?";
        let mut pack = Pack::new();
        pack.task_profile = crate::chat_turn_heuristics::infer_task_execution_profile(query);

        let out = ingest_agentic_turn_stream(
            &snap,
            0,
            |_| String::new(),
            query,
            &[],
            true,
            pack.ingest_mut(),
        );

        assert_eq!(out, AgenticTurnIngestOutcome::Break);
        assert!(!pack.forced_factual_retry);
        assert_eq!(
            pack.messages.last().unwrap()["content"],
            "59% is context usage; 117k is the token count."
        );
    }

    #[test]
    fn factual_retry_restores_original_when_no_evidence_retry_drops_the_question() {
        let original = "59% is context usage; 117k is the token count.";
        let query = "what do 59% and 117k mean in the status line?";
        let mut pack = Pack::new();
        pack.forced_factual_retry = true;
        pack.factual_retry_fallback_text = Some(original.to_string());
        pack.factual_retry_fallback_decision = Some(FactualRetryFallbackDecision::RestoreFallback);

        let tool_round = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: "",
            tool_calls: &[],
            prompt_tokens: 10,
            completion_tokens: 5,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            has_usage: true,
            error_message: &None,
            error_kind: None,
        };
        let out = ingest_agentic_turn_stream(
            &tool_round,
            1,
            |_| "introspect".to_string(),
            query,
            &[],
            true,
            pack.ingest_mut(),
        );
        assert_eq!(out, AgenticTurnIngestOutcome::HasToolCalls);
        assert_eq!(
            pack.total_evidence_tool_calls, 0,
            "introspect is runtime self-observation, not factual evidence"
        );

        let retry_final = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: "I completed the requested work and no further action is needed.",
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
            &retry_final,
            0,
            |_| String::new(),
            query,
            &[],
            true,
            pack.ingest_mut(),
        );
        assert_eq!(out, AgenticTurnIngestOutcome::Break);
        assert_eq!(pack.final_text, original);
        assert_eq!(pack.factual_retry_fallback_text, None);
        assert_eq!(pack.messages.last().unwrap()["role"], "assistant");
        assert_eq!(pack.messages.last().unwrap()["content"], original);
    }

    #[test]
    fn factual_retry_accepts_direct_retry_answer_after_control_plane_tools() {
        let original = "59% is context usage; 117k is the token count.";
        let query = "what do 59% and 117k mean in the status line?";
        let corrected = "59% is the context-window usage; 117k is the current token count.";
        let mut pack = Pack::new();
        pack.forced_factual_retry = true;
        pack.factual_retry_fallback_text = Some(original.to_string());

        let tool_round = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: "",
            tool_calls: &[],
            prompt_tokens: 10,
            completion_tokens: 5,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            has_usage: true,
            error_message: &None,
            error_kind: None,
        };
        let out = ingest_agentic_turn_stream(
            &tool_round,
            1,
            |_| "introspect".to_string(),
            query,
            &[],
            true,
            pack.ingest_mut(),
        );
        assert_eq!(out, AgenticTurnIngestOutcome::HasToolCalls);
        assert_eq!(pack.total_evidence_tool_calls, 0);

        let retry_final = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: corrected,
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
            &retry_final,
            0,
            |_| String::new(),
            query,
            &[],
            true,
            pack.ingest_mut(),
        );
        assert_eq!(out, AgenticTurnIngestOutcome::Break);
        assert_eq!(pack.final_text, corrected);
        assert_eq!(pack.factual_retry_fallback_text, None);
        assert_eq!(pack.messages.last().unwrap()["content"], corrected);
    }

    #[test]
    fn factual_retry_does_not_restore_unverified_live_data_answer_without_evidence() {
        let original = "The latest CI is green.";
        let query = "latest CI?";
        let retry = "I could not verify that from the available evidence.";
        let mut pack = Pack::new();
        pack.forced_factual_retry = true;
        pack.factual_retry_fallback_text = Some(original.to_string());

        let retry_final = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: retry,
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
            &retry_final,
            0,
            |_| String::new(),
            query,
            &[],
            true,
            pack.ingest_mut(),
        );
        assert_eq!(out, AgenticTurnIngestOutcome::Break);
        assert_eq!(pack.final_text, retry);
        assert_eq!(pack.factual_retry_fallback_text, None);
        assert_eq!(pack.messages.last().unwrap()["content"], retry);
    }

    #[test]
    fn factual_retry_accepts_retry_answer_after_real_evidence_tool() {
        let original = "The CI is green.";
        let query = "最新的一个ci?";
        let mut pack = Pack::new();
        pack.forced_factual_retry = true;
        pack.factual_retry_fallback_text = Some(original.to_string());

        let tool_round = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: "",
            tool_calls: &[],
            prompt_tokens: 10,
            completion_tokens: 5,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            has_usage: true,
            error_message: &None,
            error_kind: None,
        };
        let out = ingest_agentic_turn_stream(
            &tool_round,
            1,
            |_| "github".to_string(),
            query,
            &[],
            true,
            pack.ingest_mut(),
        );
        assert_eq!(out, AgenticTurnIngestOutcome::HasToolCalls);
        assert_eq!(pack.total_evidence_tool_calls, 1);

        let corrected = "The latest CI is failing.";
        let retry_final = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: corrected,
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
            &retry_final,
            0,
            |_| String::new(),
            query,
            &[],
            true,
            pack.ingest_mut(),
        );
        assert_eq!(out, AgenticTurnIngestOutcome::Break);
        assert_eq!(pack.final_text, corrected);
        assert_eq!(pack.factual_retry_fallback_text, None);
        assert_eq!(pack.messages.last().unwrap()["content"], corrected);
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
            error_kind: None,
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
        assert_eq!(pack.messages.len(), 1, "final assistant should persist");
        assert_eq!(pack.messages[0]["role"], "assistant");
        assert_eq!(pack.messages[0]["content"], "fabricated answer");
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
            error_kind: None,
        };
        let mut pack = Pack::new();
        pack.total_tool_calls = 3;
        pack.total_evidence_tool_calls = 3; // evidence tools were called in a previous round
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
        assert_eq!(pack.messages.len(), 1);
        assert_eq!(pack.messages[0]["role"], "assistant");
        assert_eq!(pack.messages[0]["content"], "Based on the PR data...");
    }

    #[test]
    fn factual_retry_still_fires_after_ask_user_only_rounds() {
        let snap = AgenticTurnStreamSnapshot {
            ttft_ms: None,
            session_id: &None,
            run_id: &None,
            full_text: "Here is the latest CI status...",
            tool_calls: &[],
            prompt_tokens: 20,
            completion_tokens: 20,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            has_usage: true,
            error_message: &None,
            error_kind: None,
        };
        let mut pack = Pack::new();
        pack.task_profile.allow_factual_retry = true;
        pack.total_tool_calls = 1; // ask_user was called previously
        let out = ingest_agentic_turn_stream(
            &snap,
            0,
            |_| String::new(),
            "latest CI?",
            &[],
            true,
            pack.ingest_mut(),
        );
        assert_eq!(out, AgenticTurnIngestOutcome::Continue);
        assert!(pack.forced_factual_retry);
        assert_eq!(pack.total_evidence_tool_calls, 0);
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
            error_kind: None,
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
        assert_eq!(pack.messages.len(), 1);
        assert_eq!(pack.messages[0]["role"], "assistant");
        assert_eq!(
            pack.messages[0]["content"],
            "Rust is a systems programming language..."
        );
    }

    #[test]
    fn factual_retry_does_not_infer_chinese_workspace_queries_from_text() {
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
            error_kind: None,
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
        assert_eq!(out, AgenticTurnIngestOutcome::Break);
        assert!(!pack.forced_factual_retry);
        assert_eq!(pack.final_text, "代码看起来很好...");
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
