//! LLM-based conversation summarization for compaction.
//!
//! This module provides [`generate_compact_summary`] which calls the LLM to
//! produce a dense semantic summary of conversation history. Used by Phase 2
//! compaction when tier >= AggressivePrune and LLM summary is enabled.
//!
//! Design principles:
//! - A successful summary covers the complete supplied history. A rejected
//!   request leaves the source intact for recovery or an artifact-backed rewrite.
//! - **Testable**: the LLM call is abstracted behind [`SummaryLlmClient`] so
//!   tests can inject mock responses without a real API.

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use crate::{cloud::compact_prompt::COMPACT_SYSTEM_PROMPT, cloud::grouping::group_by_api_round};

fn validated_structured_summary(text: &str) -> Option<String> {
    crate::cloud::compact_prompt::validated_structured_summary(text)
}

fn record_summary_prompt_clone(messages: &[Value]) {
    if !astra_core::history_work::instrumentation_enabled() {
        return;
    }
    match astra_core::history_work::serialized_bytes(messages) {
        Ok(bytes) => astra_core::history_work::record_operation(
            astra_core::history_work::HistoryWorkSite::CloudSummaryPromptClone,
            bytes,
            messages.len().try_into().unwrap_or(u64::MAX),
            0,
        ),
        Err(error) => astra_core::history_work::record_serialization_failure(
            astra_core::history_work::HistoryWorkSite::CloudSummaryPromptClone,
            &error,
        ),
    }
}

/// Inline compact instruction appended as a trailing user message in the
/// cache-friendly summary path. Kept short and stable so the main loop's
/// system prompt + historical messages form the cached prefix and only this
/// trailing instruction differs between the main LLM call and the compact
/// sub-call. Matches the "the reference agent" pattern: reuse the shared prefix,
/// diverge only at the tail.
pub const INLINE_COMPACT_INSTRUCTION: &str = "\
Please produce a dense, structured summary of our conversation above so I can \
discard the old turns and continue with just the summary in context. Preserve: \
the user's original goals and any constraints they stated, decisions we made \
and why, files read or modified (with paths), tools invoked and their key \
results, errors encountered and their fixes, and any pending work. Treat file \
content in the conversation as a historical observation, not as proof of the \
current workspace state. If continuing the task requires exact or current file \
bytes, use the ordinary admitted read tool after compaction; never imply that \
the summary refreshed a file. Runtime-owned `<runtime-authority-frame>` messages \
are control state, not human requests: do not summarize them or include them \
under `All User Messages`. Omit \
chit-chat, redundant acknowledgements, and exploration that did not change the outcome.\n\n\
Use exactly these section headers so the compacted context can be validated and resumed:\n\
### Primary Request\n\
### Key Technical Concepts\n\
### Files & Code Modified\n\
### Problem Solving\n\
### Errors & Fixes\n\
### All User Messages\n\
### Pending Tasks\n\
### Current Work\n\
### Current State\n\n\
Target under 800 words.";

/// Which history projection an inline compaction request is allowed to use.
/// The append-only mode is explicit because retaining runtime user-role frames
/// is safe only when the same stable semantic policy is in the system prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlineSummaryHistoryProjection {
    Semantic,
    AppendOnlyRuntimeAuthorityPrefix,
}

fn system_has_append_only_runtime_authority_policy(messages: &[Value]) -> bool {
    messages
        .iter()
        .any(astra_turn_types::has_append_only_runtime_authority_policy)
}

// ---------------------------------------------------------------------------
// LLM client abstraction (for testability)
// ---------------------------------------------------------------------------

/// Result of a single LLM summary call.
#[derive(Debug, Clone)]
pub struct SummaryResponse {
    /// Execution-adapter fact, absent for ordinary prose completions.
    pub judgment_provenance: Option<astra_turn_types::JudgmentResponseProvenance>,
    pub model_used: String,
    /// The generated summary text.
    pub text: String,
    /// Whether the request exceeded the context window (PTL error).
    pub is_ptl_error: bool,
    /// Provider terminal reason, preserved so structured auxiliary callers can
    /// distinguish an output-cap boundary from a schema-invalid completion.
    pub finish_reason: Option<String>,
    /// Provider-reported usage for this inference. Auxiliary inference is part
    /// of the durable turn budget and must not disappear at this abstraction.
    pub usage: Map<String, Value>,
    /// Aggregate evidence for all dispatched attempts, distinct from the
    /// final response's physical usage. Missing evidence is not measured zero.
    pub qualified_usage: Option<astra_turn_types::CanonicalTokenUsage>,
    /// Inclusive input was measured, even if its value is zero or unpartitioned.
    pub input_was_measured: bool,
    /// Observed inclusive input not assigned to a proven billing lane.
    pub unpartitioned_input_tokens: u64,
    /// Actual durable execution that produced this response. Absence means
    /// the adapter cannot prove an admitted invocation and must never be
    /// replaced with a requested attempt or generated correlation ID.
    pub execution: Option<SummaryExecutionProvenance>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryExecutionProvenance {
    pub invocation_id: String,
    pub model_name: String,
    pub provider: String,
}

/// Usage already observed when a summary future is dropped before returning.
#[derive(Clone, Debug)]
pub struct InterruptedSummaryUsage {
    pub usage: Map<String, Value>,
    /// At least one dispatched attempt reported a validated inclusive input, including zero.
    pub input_was_measured: bool,
    pub qualified_usage: Option<astra_turn_types::CanonicalTokenUsage>,
    pub unpartitioned_input_tokens: u64,
}

pub type InterruptedSummaryUsageSink =
    std::sync::Arc<dyn Fn(InterruptedSummaryUsage) + Send + Sync>;

/// Abstraction over the LLM API for summary generation.
/// Provider execution belongs to the runtime; this crate only owns summary
/// behavior and the test seam.
#[async_trait]
pub trait SummaryLlmClient: Send + Sync {
    /// Install the owner of usage when an in-flight call is dropped.
    fn set_interrupted_usage_sink(&mut self, _sink: InterruptedSummaryUsageSink) {}

    /// Send a summary request. Returns the response or an error.
    async fn summarize(
        &self,
        purpose: astra_turn_types::InferencePurpose,
        messages: &[Value],
    ) -> Result<SummaryResponse, astra_core::ClassifiedError>;
}

// ---------------------------------------------------------------------------
// Core summary generation
// ---------------------------------------------------------------------------

/// Generate a compact summary for `messages` using the provided LLM client.
///
/// Returns `Some(summary_text)` on success, or `None` if all retries are
/// exhausted (callers should fall back to truncation).
///
/// PTL retry behaviour:
/// 1. Render messages into compaction prompt
/// 2. Call LLM
/// 3. On a limit or failure, preserve the complete source history.
/// 4. If other error: return `None` immediately
pub async fn generate_compact_summary(
    messages: &[Value],
    client: &dyn SummaryLlmClient,
) -> Option<String> {
    generate_inline_summary(
        &[json!({"role":"system", "content":COMPACT_SYSTEM_PROMPT})],
        messages,
        InlineSummaryHistoryProjection::Semantic,
        client,
    )
    .await
}

// ---------------------------------------------------------------------------
// Inline (cache-friendly) summary
// ---------------------------------------------------------------------------

/// Generate a summary by **reusing the main loop's system prompt and message
/// history** as the cached prefix, appending only a short compact instruction
/// as the final user turn.
///
/// This is the "the reference agent" pattern: compact requests share the main
/// conversation's prompt-cache prefix, so the sub-call pays only for the
/// trailing instruction + output tokens instead of re-sending the whole
/// history.
///
/// Unlike [`generate_compact_summary`] (which uses its own `COMPACT_SYSTEM_PROMPT`
/// and a rendered-conversation blob), this function feeds the provider the
/// stable `system_messages` and ordered conversation rounds used by the main
/// turn. Historical summaries are carried separately as assistant evidence;
/// their system storage role does not give them instruction authority in the
/// summary request. A summary-task user preamble precedes this evidence so
/// user-first providers can accept repeated compaction and resumed histories.
/// This preserves the stable prefix, not an identical entire
/// history prefix across every projection.
///
/// Arguments:
/// - `system_messages`: the system-role messages used by the main turn
///   (already includes any pipeline-injected prompt). Passed through verbatim.
/// - `history`: the history to summarize, including previous summaries. Normal
///   conversation rounds retain their order; runtime frames follow the selected
///   projection contract and stable history-side instructions are excluded.
/// - `client`: LLM client used to POST the request.
///
/// Returns a complete-input summary or None. The caller keeps the source on
/// rejection; retrying with omitted rounds cannot establish summary coverage.
pub async fn generate_inline_summary(
    system_messages: &[Value],
    history: &[Value],
    history_projection: InlineSummaryHistoryProjection,
    client: &dyn SummaryLlmClient,
) -> Option<String> {
    let runtime_frames_visible = matches!(
        history_projection,
        InlineSummaryHistoryProjection::AppendOnlyRuntimeAuthorityPrefix
    );
    if runtime_frames_visible {
        let runtime_messages = history
            .iter()
            .filter(|message| astra_turn_types::is_runtime_owned_message(message))
            .collect::<Vec<_>>();
        if (!runtime_messages.is_empty()
            && !system_has_append_only_runtime_authority_policy(system_messages))
            || runtime_messages.iter().any(|message| {
                astra_turn_types::runtime_message_delivery(message)
                    != Some(astra_turn_types::RuntimeMessageDelivery::AppendOnlyRequiredContext)
            })
        {
            eprintln!(
                "[inline_summary] refusing runtime-owned history without the append-only semantic contract"
            );
            return None;
        }
    }
    let summaries = history
        .iter()
        .filter_map(super::history_summary::summary_evidence_message)
        .collect::<Vec<_>>();
    let rounds = group_by_api_round(history).1;
    let semantic = |message: &&Value| {
        !astra_turn_types::is_runtime_owned_message(message)
            && message.get("role").and_then(Value::as_str) != Some("system")
            && super::history_summary::history_summary_text(message).is_none()
    };
    let normalized_source = history.iter().filter(semantic).map(|message| {
        if message.get("role").and_then(Value::as_str) == Some("assistant")
            && message
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
        {
            std::borrow::Cow::Owned(
                crate::chat_history_openai::sanitize_empty_assistant_tool_calls_cloned(message),
            )
        } else {
            std::borrow::Cow::Borrowed(message)
        }
    });
    if !rounds
        .iter()
        .flat_map(|round| round.summary_messages())
        .filter(semantic)
        .map(std::borrow::Cow::Borrowed)
        .eq(normalized_source)
    {
        return None;
    }

    // Keep historical evidence in assistant context while satisfying
    // providers that require the first conversation message to be user.
    // This preamble belongs only to the auxiliary request, never history.
    let mut messages: Vec<Value> = Vec::with_capacity(system_messages.len() + history.len() + 2);
    messages.extend(system_messages.iter().cloned());
    if !summaries.is_empty() {
        messages.push(json!({
            "role": "user",
            "content": super::history_summary::HISTORY_SUMMARY_EVIDENCE_PREAMBLE,
        }));
    }
    messages.extend(summaries.iter().cloned());
    for round in &rounds {
        if runtime_frames_visible {
            messages.extend(round.messages().iter().cloned());
        } else {
            messages.extend(round.summary_messages().cloned());
        }
    }
    messages.push(json!({
        "role": "user",
        "content": INLINE_COMPACT_INSTRUCTION,
    }));
    record_summary_prompt_clone(&messages);

    match client
        .summarize(
            astra_turn_types::InferencePurpose::RequiredCompaction,
            &messages,
        )
        .await
    {
        Ok(resp) if !resp.is_ptl_error => {
            if resp.finish_reason.as_deref() == Some("length") {
                return None;
            }
            let mut summary = validated_structured_summary(&resp.text)?;
            let carried: std::collections::BTreeSet<_> = history
                .iter()
                .filter_map(super::history_summary::history_summary_text)
                .flat_map(super::history_summary::context_history_artifact_handles)
                .collect();
            let present: std::collections::BTreeSet<_> =
                super::history_summary::context_history_artifact_handles(&summary)
                    .map(str::to_owned)
                    .collect();
            for handle in carried {
                if !present.contains(handle) {
                    summary.push_str("\nRecoverable history: ");
                    summary.push_str(handle);
                }
            }
            Some(summary)
        }
        Ok(_) => None,
        Err(e) => {
            eprintln!("[inline_summary] LLM error: {e}, giving up");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Test helpers exposed for cross-crate testing (e.g. runtime's compaction tests).
pub mod test_support {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// Mock LLM client for testing.
    pub struct MockSummaryClient {
        /// Responses to return in order. If fewer than calls, last is repeated.
        pub responses: Vec<Result<SummaryResponse, astra_core::ClassifiedError>>,
        pub call_count: Arc<AtomicUsize>,
        purposes: Arc<Mutex<Vec<astra_turn_types::InferencePurpose>>>,
        requests: Arc<Mutex<Vec<Vec<Value>>>>,
    }

    impl MockSummaryClient {
        pub fn success(text: &str) -> Self {
            Self {
                responses: vec![Ok(SummaryResponse {
                    input_was_measured: false,
                    unpartitioned_input_tokens: 0,
                    qualified_usage: None,
                    judgment_provenance: None,
                    model_used: "fixture-model".into(),
                    text: text.to_string(),
                    is_ptl_error: false,
                    finish_reason: Some("stop".to_string()),
                    usage: Map::new(),
                    execution: None,
                })],
                call_count: Arc::new(AtomicUsize::new(0)),
                purposes: Arc::new(Mutex::new(Vec::new())),
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }

        pub fn ptl_then_success(success_text: &str) -> Self {
            Self {
                responses: vec![
                    Ok(SummaryResponse {
                        input_was_measured: false,
                        unpartitioned_input_tokens: 0,
                        qualified_usage: None,
                        judgment_provenance: None,
                        model_used: "fixture-model".into(),
                        text: String::new(),
                        is_ptl_error: true,
                        finish_reason: None,
                        usage: Map::new(),
                        execution: None,
                    }),
                    Ok(SummaryResponse {
                        input_was_measured: false,
                        unpartitioned_input_tokens: 0,
                        qualified_usage: None,
                        judgment_provenance: None,
                        model_used: "fixture-model".into(),
                        text: success_text.to_string(),
                        is_ptl_error: false,
                        finish_reason: Some("stop".to_string()),
                        usage: Map::new(),
                        execution: None,
                    }),
                ],
                call_count: Arc::new(AtomicUsize::new(0)),
                purposes: Arc::new(Mutex::new(Vec::new())),
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }

        pub fn always_ptl() -> Self {
            Self {
                responses: vec![Ok(SummaryResponse {
                    input_was_measured: false,
                    unpartitioned_input_tokens: 0,
                    qualified_usage: None,
                    judgment_provenance: None,
                    model_used: "fixture-model".into(),
                    text: String::new(),
                    is_ptl_error: true,
                    finish_reason: None,
                    usage: Map::new(),
                    execution: None,
                })],
                call_count: Arc::new(AtomicUsize::new(0)),
                purposes: Arc::new(Mutex::new(Vec::new())),
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }

        pub fn error(msg: &str) -> Self {
            Self {
                responses: vec![Err(astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::Network,
                    msg,
                ))],
                call_count: Arc::new(AtomicUsize::new(0)),
                purposes: Arc::new(Mutex::new(Vec::new())),
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }

        pub fn recorded_purposes(&self) -> Vec<astra_turn_types::InferencePurpose> {
            self.purposes
                .lock()
                .expect("mock summary purpose lock")
                .clone()
        }

        pub fn recorded_requests(&self) -> Vec<Vec<Value>> {
            self.requests
                .lock()
                .expect("mock summary request lock")
                .clone()
        }
    }

    #[async_trait]
    impl SummaryLlmClient for MockSummaryClient {
        async fn summarize(
            &self,
            purpose: astra_turn_types::InferencePurpose,
            messages: &[Value],
        ) -> Result<SummaryResponse, astra_core::ClassifiedError> {
            self.purposes
                .lock()
                .expect("mock summary purpose lock")
                .push(purpose);
            self.requests
                .lock()
                .expect("mock summary request lock")
                .push(messages.to_vec());
            let count = self.call_count.fetch_add(1, Ordering::SeqCst);
            let idx = count.min(self.responses.len() - 1);
            self.responses[idx].clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::MockSummaryClient;
    use super::*;
    use serde_json::json;
    use std::sync::atomic::Ordering;

    fn make_messages(n: usize) -> Vec<Value> {
        (0..n)
            .flat_map(|i| {
                vec![
                    json!({"role": "user", "content": format!("question {i}")}),
                    json!({"role": "assistant", "content": format!("answer {i}")}),
                ]
            })
            .collect()
    }

    fn valid_summary() -> &'static str {
        "### Primary Request\nDo the work\n### Pending Tasks\nNone\n### Current Work\nDone\n### Current State\nVerified"
    }

    #[tokio::test]
    async fn complete_summary_input_retains_late_tool_evidence_and_structured_content() {
        let history = vec![
            json!({"role":"user","content":"Continue the long tool task"}),
            json!({"role":"assistant","content":"decision", "tool_calls":[{
                "id":"receipt","type":"function","function":{"name":"read_file","arguments":format!(r#"{{"path":"{}LATE_ARGUMENT"}}"#, "p".repeat(900))}
            }]}),
            json!({"role":"tool","tool_call_id":"receipt","content":[
                {"type":"text","text":format!("{}CONFIRMED_RESOURCE", "evidence ".repeat(1000))},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}
            ]}),
        ];
        for inline in [false, true] {
            let client = MockSummaryClient::success(valid_summary());
            let summary = if inline {
                generate_inline_summary(
                    &[],
                    &history,
                    InlineSummaryHistoryProjection::Semantic,
                    &client,
                )
                .await
            } else {
                generate_compact_summary(&history, &client).await
            };
            assert!(summary.is_some());
            let wire = serde_json::to_string(&client.recorded_requests()[0]).unwrap();
            assert!(wire.contains("LATE_ARGUMENT"));
            assert!(wire.contains("CONFIRMED_RESOURCE"));
            assert!(wire.contains("data:image/png;base64,AA=="));
        }
        let client = MockSummaryClient::success(valid_summary());
        let reference = format!(
            "{}{}",
            astra_turn_types::CONTEXT_HISTORY_ARTIFACT_URI_PREFIX,
            uuid::Uuid::nil()
        );
        let retained = vec![
            super::super::history_summary::history_summary_message(format!(
                "Earlier evidence: {reference}"
            )),
            json!({"role":"user","content":"continue"}),
            json!({"role":"assistant","content":"done","tool_calls":[]}),
        ];
        let replacement = generate_compact_summary(&retained, &client).await.unwrap();
        assert_eq!(
            super::super::history_summary::context_history_artifact_handles(&replacement)
                .collect::<Vec<_>>(),
            vec![reference.as_str()]
        );
        let client = MockSummaryClient::success(valid_summary());
        let unsupported = [json!({"role":"unknown","content":"uncovered evidence"})];
        assert!(
            generate_compact_summary(&unsupported, &client)
                .await
                .is_none()
        );
        assert_eq!(client.call_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn success_on_first_attempt() {
        let body = "### Primary Request\nDoing stuff\n### Pending Tasks\nNone\n### Current Work\nIn progress\n### Current State\nDone";
        let client = MockSummaryClient::success(body);
        let msgs = make_messages(3);
        let result = generate_compact_summary(&msgs, &client).await;
        assert_eq!(result.as_deref(), Some(body));
        assert_eq!(client.call_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            client.recorded_purposes(),
            [astra_turn_types::InferencePurpose::RequiredCompaction]
        );
    }

    #[tokio::test]
    async fn ptl_does_not_publish_a_summary_that_omits_oldest_rounds() {
        let body = "### Primary Request\nX\n### Pending Tasks\nY\n### Current Work\nW\n### Current State\nZ";
        let client = MockSummaryClient::ptl_then_success(body);
        let msgs = make_messages(4); // 4 rounds, enough to drop one
        let result = generate_compact_summary(&msgs, &client).await;
        assert!(result.is_none());
        assert_eq!(client.call_count.load(Ordering::SeqCst), 1);
        assert_eq!(client.recorded_requests().len(), 1);
    }

    #[tokio::test]
    async fn returns_none_on_context_limit() {
        let client = MockSummaryClient::always_ptl();
        // Only 1 round — can't drop any, gives up
        let msgs = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "hello"}),
        ];
        let result = generate_compact_summary(&msgs, &client).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn returns_none_on_llm_error() {
        let client = MockSummaryClient::error("connection refused");
        let msgs = make_messages(2);
        let result = generate_compact_summary(&msgs, &client).await;
        assert!(result.is_none());
    }

    #[test]
    fn inline_compact_instruction_uses_canonical_summary_schema() {
        for section in [
            "### Primary Request",
            "### Pending Tasks",
            "### Current Work",
            "### Current State",
        ] {
            assert!(
                INLINE_COMPACT_INSTRUCTION.contains(section),
                "inline compaction prompt must include required section {section}"
            );
        }
        assert!(
            !INLINE_COMPACT_INSTRUCTION.contains("**Goals**"),
            "inline compaction must not use a parallel summary schema"
        );
        assert!(INLINE_COMPACT_INSTRUCTION.contains("historical observation"));
        assert!(INLINE_COMPACT_INSTRUCTION.contains("ordinary admitted read tool"));
        assert!(INLINE_COMPACT_INSTRUCTION.contains("never imply"));
    }

    #[tokio::test]
    async fn inline_summary_preserves_system_prefix_and_history_wire_order() {
        let system_messages = vec![
            json!({"role": "system", "content": "stable prefix"}),
            json!({"role": "system", "content": "runtime contract"}),
        ];
        let history = make_messages(2);
        let client = MockSummaryClient::success(valid_summary());

        let summary = generate_inline_summary(
            &system_messages,
            &history,
            InlineSummaryHistoryProjection::Semantic,
            &client,
        )
        .await
        .expect("inline summary should succeed");

        assert!(summary.contains("### Current State"));
        let requests = client.recorded_requests();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(
            &request[..system_messages.len()],
            system_messages.as_slice()
        );
        assert_eq!(
            &request[system_messages.len()..system_messages.len() + history.len()],
            history.as_slice()
        );
        assert_eq!(
            request.last(),
            Some(&json!({
                "role": "user",
                "content": INLINE_COMPACT_INSTRUCTION,
            }))
        );
        assert_eq!(
            client.recorded_purposes(),
            [astra_turn_types::InferencePurpose::RequiredCompaction]
        );
    }

    #[tokio::test]
    async fn inline_summary_semantic_projection_excludes_runtime_user_frames() {
        let system_messages = vec![json!({"role": "system", "content": "stable"})];
        let mut authority = json!({"role": "user", "content": "runtime settlement"});
        astra_turn_types::mark_append_only_required_context(
            &mut authority,
            "final_answer_settlement",
            astra_turn_types::RuntimeAuthorityLifetime::NextAssistantDecision,
        );
        let history = vec![
            json!({"role": "user", "content": "real request"}),
            authority,
            json!({"role": "assistant", "content": "working"}),
        ];
        let client = MockSummaryClient::success(valid_summary());

        generate_inline_summary(
            &system_messages,
            &history,
            InlineSummaryHistoryProjection::Semantic,
            &client,
        )
        .await
        .expect("semantic summary");

        let request = client.recorded_requests().pop().unwrap();
        assert!(request.iter().all(|message| {
            !astra_turn_types::is_runtime_owned_message(message)
                && message.get("content").and_then(Value::as_str) != Some("runtime settlement")
        }));
    }

    #[tokio::test]
    async fn inline_summary_append_prefix_requires_policy_and_preserves_exact_history() {
        let mut authority = json!({"role": "user", "content": "runtime settlement"});
        astra_turn_types::mark_append_only_required_context(
            &mut authority,
            "final_answer_settlement",
            astra_turn_types::RuntimeAuthorityLifetime::NextAssistantDecision,
        );
        let history = vec![
            json!({"role": "user", "content": "real request"}),
            authority,
        ];
        let missing_policy_client = MockSummaryClient::success(valid_summary());
        assert!(
            generate_inline_summary(
                &[json!({"role": "system", "content": "stable"})],
                &history,
                InlineSummaryHistoryProjection::AppendOnlyRuntimeAuthorityPrefix,
                &missing_policy_client,
            )
            .await
            .is_none()
        );
        assert_eq!(missing_policy_client.call_count.load(Ordering::SeqCst), 0);

        let mut system_message = json!({
            "role": "system",
            "content": format!("stable\n{}", astra_turn_types::APPEND_ONLY_RUNTIME_AUTHORITY_POLICY),
        });
        astra_turn_types::mark_append_only_runtime_authority_policy(&mut system_message);
        let system_messages = vec![system_message];
        let client = MockSummaryClient::success(valid_summary());
        generate_inline_summary(
            &system_messages,
            &history,
            InlineSummaryHistoryProjection::AppendOnlyRuntimeAuthorityPrefix,
            &client,
        )
        .await
        .expect("policy makes exact append history safe for summary semantics");
        let request = client.recorded_requests().pop().unwrap();
        assert_eq!(
            &request[system_messages.len()..system_messages.len() + history.len()],
            history.as_slice()
        );
    }

    #[tokio::test]
    async fn ptl_preserves_the_only_human_round() {
        // Exactly 2 messages (1 round) — can't drop below minimum, returns None
        let client = MockSummaryClient::always_ptl();
        let msgs = vec![
            json!({"role": "user", "content": "single question"}),
            json!({"role": "assistant", "content": "single answer"}),
        ];
        let result = generate_compact_summary(&msgs, &client).await;
        assert!(result.is_none());
        // Should give up quickly — can't drop the only round
        assert_eq!(client.call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn history_summary_is_covered_without_partial_history_retries() {
        use super::super::history_summary::{HISTORY_SUMMARY_FIELD, history_summary_message};
        let text = format!(
            "{}\n{}\nFACT_BEYOND_ORDINARY_EXCERPT_CAP",
            valid_summary(),
            "historical detail ".repeat(200)
        );
        // Includes a leading legacy summary and a typed summary mid-history.
        let mut history = vec![
            json!({"role":"system", "content":format!("[Conversation compacted — 20 messages summarized]\n\n{text}")}),
        ];
        history.extend(make_messages(3));
        history.insert(3, history_summary_message("SECOND_PRIOR_SUMMARY".into()));
        history.insert(4, json!({"role":"system", "content":"HISTORY_SIDE_POLICY"}));
        let mut authority = json!({"role":"user", "content":"RUNTIME_CONTROL_NOT_USER_INTENT"});
        astra_turn_types::mark_append_only_required_context(
            &mut authority,
            "final_answer_settlement",
            astra_turn_types::RuntimeAuthorityLifetime::NextAssistantDecision,
        );
        history.insert(5, authority);
        for inline in [false, true] {
            let client = MockSummaryClient::ptl_then_success(valid_summary());
            let result = if inline {
                generate_inline_summary(
                    &[json!({"role":"system", "content":"STABLE_PREFIX"})],
                    &history,
                    InlineSummaryHistoryProjection::Semantic,
                    &client,
                )
                .await
            } else {
                generate_compact_summary(&history, &client).await
            };
            assert!(result.is_none());
            let requests = client.recorded_requests();
            assert_eq!(requests.len(), 1);
            for request in requests {
                let serialized = serde_json::to_string(&request).unwrap();
                assert!(serialized.contains("FACT_BEYOND_ORDINARY_EXCERPT_CAP"));
                assert!(serialized.contains("SECOND_PRIOR_SUMMARY"));
                assert!(!serialized.contains("HISTORY_SIDE_POLICY"));
                assert!(!serialized.contains("RUNTIME_CONTROL_NOT_USER_INTENT"));
                assert!(!serialized.contains(HISTORY_SUMMARY_FIELD));
                assert_eq!(
                    serialized.matches("STABLE_PREFIX").count(),
                    usize::from(inline)
                );
            }
        }
    }

    #[tokio::test]
    async fn incomplete_or_empty_summary_fails_closed() {
        let messages = make_messages(2);
        for response in ["", "plain text", "### Primary Request\nOnly one section"] {
            let client = MockSummaryClient::success(response);
            assert!(generate_compact_summary(&messages, &client).await.is_none());
        }
    }
    #[tokio::test]
    async fn structured_summary_requires_nontruncated_completion() {
        let history = make_messages(2);
        for finish in [None, Some("stop"), Some("length")] {
            for inline in [false, true] {
                let mut client = MockSummaryClient::success(valid_summary());
                client.responses[0].as_mut().unwrap().finish_reason = finish.map(str::to_owned);
                let result = if inline {
                    generate_inline_summary(
                        &[],
                        &history,
                        InlineSummaryHistoryProjection::Semantic,
                        &client,
                    )
                    .await
                } else {
                    generate_compact_summary(&history, &client).await
                };
                assert_eq!(
                    result.is_some(),
                    finish != Some("length"),
                    "inline={inline} finish={finish:?}"
                );
                assert_eq!(
                    client.call_count.load(std::sync::atomic::Ordering::SeqCst),
                    1
                );
            }
        }
    }
}
