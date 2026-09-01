//! LLM-based conversation summarization for compaction.
//!
//! This module provides [`generate_compact_summary`] which calls the LLM to
//! produce a dense semantic summary of conversation history. Used by Phase 2
//! compaction when tier >= AggressivePrune and LLM summary is enabled.
//!
//! Design principles:
//! - **PTL retry**: if the summary request itself exceeds the context window,
//!   drop the oldest API rounds and retry (up to [`MAX_PTL_RETRIES`]).
//! - **Fallback**: if retries are exhausted, return `None` so callers can
//!   fall back to pure truncation.
//! - **Testable**: the LLM call is abstracted behind [`SummaryLlmClient`] so
//!   tests can inject mock responses without a real API.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{
    cloud::compact_prompt::{
        COMPACT_SYSTEM_PROMPT, build_compact_user_prompt, render_messages_for_summary,
    },
    cloud::grouping::{ApiRound, drop_oldest_rounds, flatten_rounds, group_by_api_round},
};

/// Maximum number of PTL retry attempts before giving up and returning `None`.
pub const MAX_PTL_RETRIES: usize = 3;

/// Minimum number of API rounds to keep when dropping for PTL retry.
pub const MIN_ROUNDS_TO_KEEP: usize = 1;

fn cloud_summary_serialization_dimensions(rendered: &str, source_rows: usize) -> (u64, u64) {
    (
        u64::try_from(rendered.len()).unwrap_or(u64::MAX),
        u64::try_from(source_rows).unwrap_or(u64::MAX),
    )
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

fn record_summary_rounds_clone(rounds: &[ApiRound]) {
    if !astra_core::history_work::instrumentation_enabled() {
        return;
    }
    let mut bytes = 0_u64;
    let mut rows = 0_u64;
    for round in rounds {
        for message in round
            .user_messages
            .iter()
            .chain(round.assistant_message.iter())
            .chain(round.tool_messages.iter())
        {
            match astra_core::history_work::serialized_bytes(message) {
                Ok(message_bytes) => {
                    bytes = bytes.saturating_add(message_bytes);
                    rows = rows.saturating_add(1);
                }
                Err(error) => {
                    astra_core::history_work::record_serialization_failure(
                        astra_core::history_work::HistoryWorkSite::CloudSummaryPromptClone,
                        &error,
                    );
                    return;
                }
            }
        }
    }
    astra_core::history_work::record_operation(
        astra_core::history_work::HistoryWorkSite::CloudSummaryPromptClone,
        bytes,
        rows,
        0,
    );
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
the summary refreshed a file. Omit \
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

// ---------------------------------------------------------------------------
// LLM client abstraction (for testability)
// ---------------------------------------------------------------------------

/// Result of a single LLM summary call.
#[derive(Debug, Clone)]
pub struct SummaryResponse {
    /// The generated summary text.
    pub text: String,
    /// Whether the request exceeded the context window (PTL error).
    pub is_ptl_error: bool,
}

/// Abstraction over the LLM API for summary generation.
/// Provider execution belongs to the runtime; this crate only owns summary
/// behavior and the test seam.
#[async_trait]
pub trait SummaryLlmClient: Send + Sync {
    /// Send a summary request. Returns the response or an error.
    async fn summarize(
        &self,
        purpose: astra_turn_types::InferencePurpose,
        messages: &[Value],
    ) -> Result<SummaryResponse, String>;
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
/// 3. If PTL error: drop oldest round and retry (up to `MAX_PTL_RETRIES`)
/// 4. If other error: return `None` immediately
pub async fn generate_compact_summary(
    messages: &[Value],
    client: &dyn SummaryLlmClient,
) -> Option<String> {
    let (system_msgs, mut rounds) = group_by_api_round(messages);
    let min_keep = MIN_ROUNDS_TO_KEEP;

    for attempt in 0..=MAX_PTL_RETRIES {
        let msgs_for_summary = flatten_rounds(&system_msgs, &rounds);
        record_summary_prompt_clone(&msgs_for_summary);
        let rendered = render_messages_for_summary(&msgs_for_summary);
        if astra_core::history_work::instrumentation_enabled() {
            let (bytes, rows) =
                cloud_summary_serialization_dimensions(&rendered, msgs_for_summary.len());
            astra_core::history_work::record_operation(
                astra_core::history_work::HistoryWorkSite::CloudSummarySerialization,
                bytes,
                rows,
                0,
            );
        }
        let prompt_messages = build_summary_messages(&rendered);
        record_summary_prompt_clone(&prompt_messages);

        match client
            .summarize(
                astra_turn_types::InferencePurpose::RequiredCompaction,
                &prompt_messages,
            )
            .await
        {
            Ok(resp) if !resp.is_ptl_error => {
                return Some(crate::cloud::compact_prompt::format_structured_summary(
                    &resp.text,
                ));
            }
            Ok(resp) if resp.is_ptl_error => {
                if attempt >= MAX_PTL_RETRIES {
                    eprintln!(
                        "[compact_summary] PTL retries exhausted after {} attempts, falling back to truncation",
                        attempt
                    );
                    return None;
                }
                // Drop the oldest round and retry
                let rounds_before = rounds.len();
                let new_rounds = drop_oldest_rounds(&rounds, 1, min_keep);
                if new_rounds.len() == rounds_before {
                    // Can't drop any more rounds
                    eprintln!("[compact_summary] cannot drop more rounds, giving up");
                    return None;
                }
                eprintln!(
                    "[compact_summary] PTL error, dropping oldest round (attempt {}, {} → {} rounds)",
                    attempt,
                    rounds_before,
                    new_rounds.len()
                );
                rounds = new_rounds.to_vec();
                record_summary_rounds_clone(&rounds);
            }
            Ok(_) => unreachable!(),
            Err(e) => {
                eprintln!("[compact_summary] LLM error: {e}, falling back to truncation");
                return None;
            }
        }
    }

    None
}

/// Build the messages array for the summary LLM call.
fn build_summary_messages(rendered_conversation: &str) -> Vec<Value> {
    vec![
        serde_json::json!({
            "role": "system",
            "content": COMPACT_SYSTEM_PROMPT,
        }),
        serde_json::json!({
            "role": "user",
            "content": build_compact_user_prompt(rendered_conversation),
        }),
    ]
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
/// *actual* `system_messages` and `history` used by the main turn. The wire
/// prefix therefore matches the main LLM call's prefix exactly — which is the
/// condition Anthropic / OpenAI / Bedrock prompt caching hashes on.
///
/// Arguments:
/// - `system_messages`: the system-role messages used by the main turn
///   (already includes any pipeline-injected prompt). Passed through verbatim.
/// - `history`: the conversation turns to summarize. Passed through verbatim
///   (same cache hash as the main call's tail messages, up to the boundary).
/// - `client`: LLM client used to POST the request.
///
/// Returns `Some(summary_text)` on success, or `None` if PTL retries are
/// exhausted (caller should fall back to structural compaction).
///
/// PTL retry behaviour mirrors [`generate_compact_summary`]: on a context-window
/// error we drop the oldest API round from `history` and retry up to
/// [`MAX_PTL_RETRIES`] times.
pub async fn generate_inline_summary(
    system_messages: &[Value],
    history: &[Value],
    client: &dyn SummaryLlmClient,
) -> Option<String> {
    let mut rounds = group_by_api_round(history).1;
    let min_keep = MIN_ROUNDS_TO_KEEP;

    for attempt in 0..=MAX_PTL_RETRIES {
        // Build the messages array: <system...> + <history rounds...> + trailing user instruction.
        let mut messages: Vec<Value> =
            Vec::with_capacity(system_messages.len() + history.len() + 1);
        messages.extend(system_messages.iter().cloned());
        for round in &rounds {
            for msg in round.messages() {
                messages.push(msg);
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
                return Some(crate::cloud::compact_prompt::format_structured_summary(
                    &resp.text,
                ));
            }
            Ok(resp) if resp.is_ptl_error => {
                if attempt >= MAX_PTL_RETRIES {
                    eprintln!(
                        "[inline_summary] PTL retries exhausted after {} attempts",
                        attempt
                    );
                    return None;
                }
                let rounds_before = rounds.len();
                let new_rounds = drop_oldest_rounds(&rounds, 1, min_keep);
                if new_rounds.len() == rounds_before {
                    eprintln!("[inline_summary] cannot drop more rounds, giving up");
                    return None;
                }
                eprintln!(
                    "[inline_summary] PTL error, dropping oldest round (attempt {}, {} → {} rounds)",
                    attempt,
                    rounds_before,
                    new_rounds.len()
                );
                rounds = new_rounds.to_vec();
                record_summary_rounds_clone(&rounds);
            }
            Ok(_) => unreachable!(),
            Err(e) => {
                eprintln!("[inline_summary] LLM error: {e}, giving up");
                return None;
            }
        }
    }

    None
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
        pub responses: Vec<Result<SummaryResponse, String>>,
        pub call_count: Arc<AtomicUsize>,
        purposes: Arc<Mutex<Vec<astra_turn_types::InferencePurpose>>>,
        requests: Arc<Mutex<Vec<Vec<Value>>>>,
    }

    impl MockSummaryClient {
        pub fn success(text: &str) -> Self {
            Self {
                responses: vec![Ok(SummaryResponse {
                    text: text.to_string(),
                    is_ptl_error: false,
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
                        text: String::new(),
                        is_ptl_error: true,
                    }),
                    Ok(SummaryResponse {
                        text: success_text.to_string(),
                        is_ptl_error: false,
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
                    text: String::new(),
                    is_ptl_error: true,
                })],
                call_count: Arc::new(AtomicUsize::new(0)),
                purposes: Arc::new(Mutex::new(Vec::new())),
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }

        pub fn error(msg: &str) -> Self {
            Self {
                responses: vec![Err(msg.to_string())],
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
        ) -> Result<SummaryResponse, String> {
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
    async fn ptl_retry_drops_oldest_round_and_succeeds() {
        let body = "### Primary Request\nX\n### Pending Tasks\nY\n### Current Work\nW\n### Current State\nZ";
        let client = MockSummaryClient::ptl_then_success(body);
        let msgs = make_messages(4); // 4 rounds, enough to drop one
        let result = generate_compact_summary(&msgs, &client).await;
        assert_eq!(result.as_deref(), Some(body));
        assert_eq!(client.call_count.load(Ordering::SeqCst), 2);
        assert_eq!(
            client.recorded_purposes(),
            [
                astra_turn_types::InferencePurpose::RequiredCompaction,
                astra_turn_types::InferencePurpose::RequiredCompaction,
            ]
        );
    }

    #[tokio::test]
    async fn returns_none_when_all_retries_exhausted() {
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
    fn build_summary_messages_structure() {
        let msgs = build_summary_messages("some conversation");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"].as_str().unwrap(), "system");
        assert_eq!(msgs[1]["role"].as_str().unwrap(), "user");
        assert!(
            msgs[1]["content"]
                .as_str()
                .unwrap()
                .contains("some conversation")
        );
    }

    #[test]
    fn cloud_summary_serialization_counts_exact_utf8_artifact_and_source_rows() {
        let messages = vec![
            json!({"role": "user", "content": "你好🚀"}),
            json!({"role": "assistant", "content": "résumé"}),
        ];
        let rendered = render_messages_for_summary(&messages);

        assert_eq!(
            cloud_summary_serialization_dimensions(&rendered, messages.len()),
            (rendered.len() as u64, 2)
        );
        assert!(
            rendered.len() > rendered.chars().count(),
            "measurement must count the existing UTF-8 artifact bytes"
        );
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
        let client = MockSummaryClient::success("current state");

        let summary = generate_inline_summary(&system_messages, &history, &client)
            .await
            .expect("inline summary should succeed");

        assert!(summary.contains("current state"));
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
    async fn ptl_retry_with_minimum_rounds() {
        // Exactly 2 messages (1 round) — can't drop below minimum, returns None
        let client = MockSummaryClient::always_ptl();
        let msgs = vec![
            json!({"role": "user", "content": "single question"}),
            json!({"role": "assistant", "content": "single answer"}),
        ];
        let result = generate_compact_summary(&msgs, &client).await;
        assert!(result.is_none());
        // Should give up quickly — can't drop the only round
        assert!(client.call_count.load(Ordering::SeqCst) <= 2);
    }
}
