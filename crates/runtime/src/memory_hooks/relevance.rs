//! Memory relevance filtering via a cheap selector model.
//!
//! Filters retrieved memories/lessons to only those clearly relevant
//! to the current task, reducing prompt noise and token waste.
//! Uses the cheapest `selector`-tagged model from the registry
//! (resolved via `resolve_memory_model` from the model DB).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use astra_text_utils::text_tokenize::tokenize;
use astra_turn_core::thinking_config::ThinkingConfig;
use astra_turn_types::InferencePurpose;

use crate::turn::llm::client::{LlmCall, LlmExecutionRoute, call_llm_nonstream, global_llm_client};

/// Prompt for the selector model to judge memory relevance.
pub const RELEVANCE_FILTER_PROMPT: &str = "\
You are filtering retrieved memories for relevance to a user's task.
Return ONLY a JSON array of indices for memories that are CLEARLY useful.
If unsure whether a memory is relevant, EXCLUDE it — false negatives
are better than noise. Return [] if nothing is relevant.";

/// Prompt for judging whether the latest user message is feedback about
/// previously injected memory/lesson/rule candidates.
pub const MEMORY_FEEDBACK_FILTER_PROMPT: &str = "\
You are reviewing a user's latest message against memory candidates that were
shown to the assistant earlier. Return ONLY a JSON array of candidate indices
that the user is explicitly rejecting as irrelevant, stale, wrong, conflicting,
or no longer applicable. Do not mark a candidate just because the new task is
about something else. If uncertain, return [].";

/// Build the user-turn content for relevance filtering.
#[must_use]
pub fn build_relevance_query(user_message: &str, memories: &[String]) -> String {
    let mut prompt = format!("User task: {}\n\nMemories:\n", truncate(user_message, 200));
    for (i, m) in memories.iter().enumerate() {
        prompt.push_str(&format!("[{}] {}\n", i, truncate(m, 150)));
    }
    prompt.push_str("\nRelevant indices (JSON array):");
    prompt
}

/// Build the user-turn content for memory feedback filtering.
#[must_use]
pub fn build_memory_feedback_query(user_message: &str, memories: &[String]) -> String {
    let mut prompt = format!(
        "Latest user message: {}\n\nInjected candidates:\n",
        truncate(user_message, 300)
    );
    for (i, m) in memories.iter().enumerate() {
        prompt.push_str(&format!("[{}] {}\n", i, truncate(m, 180)));
    }
    prompt.push_str("\nRejected candidate indices (JSON array):");
    prompt
}

/// Parse the selector's strict JSON response, dropping out-of-range and
/// duplicate indices while preserving the model's order.
fn parse_relevance_response(
    response: &str,
    memory_count: usize,
) -> Result<Vec<usize>, serde_json::Error> {
    let indices = serde_json::from_str::<Vec<usize>>(response.trim())?;
    let mut seen = HashSet::new();
    Ok(indices
        .into_iter()
        .filter(|index| *index < memory_count && seen.insert(*index))
        .collect())
}

/// Filter memories by the indices returned from the selector model.
/// Returns only the memories at the given indices, preserving order.
#[must_use]
pub fn filter_by_indices<T: Clone>(items: &[T], indices: &[usize]) -> Vec<T> {
    indices
        .iter()
        .filter_map(|&i| items.get(i).cloned())
        .collect()
}

/// Local relevance gate used when the selector model is unavailable and as a
/// deterministic fallback in tests. It only keeps items that share meaningful
/// task terms with the current user message.
#[must_use]
pub fn lexical_filter_memories(user_message: &str, items: &[String]) -> Vec<String> {
    let indices = lexical_relevant_indices(user_message, items);
    filter_by_indices(items, &indices)
}

#[must_use]
pub fn lexical_relevant_indices(user_message: &str, items: &[String]) -> Vec<usize> {
    let query_terms = meaningful_terms(user_message);
    if query_terms.is_empty() {
        return Vec::new();
    }

    let mut scored = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        let item_terms = meaningful_terms(item);
        let score = overlap_score(&query_terms, &item_terms);
        if score > 0 {
            scored.push((idx, score));
        }
    }
    scored.sort_by(|(left_idx, left_score), (right_idx, right_score)| {
        right_score
            .cmp(left_score)
            .then_with(|| left_idx.cmp(right_idx))
    });
    scored.into_iter().map(|(idx, _)| idx).collect()
}

fn meaningful_terms(text: &str) -> HashSet<String> {
    tokenize(text)
        .into_iter()
        .filter(|term| is_meaningful_term(term))
        .collect()
}

fn overlap_score(query_terms: &HashSet<String>, item_terms: &HashSet<String>) -> usize {
    query_terms
        .intersection(item_terms)
        .filter(|term| is_strong_overlap_term(term))
        .map(|term| if term.is_ascii() { 2 } else { 1 })
        .sum()
}

fn is_strong_overlap_term(term: &str) -> bool {
    if term.is_ascii() {
        return term.chars().count() >= 3;
    }
    term.chars().count() >= 2
}

fn is_meaningful_term(term: &str) -> bool {
    if term.trim().is_empty() {
        return false;
    }
    if matches!(
        term,
        "the"
            | "and"
            | "for"
            | "with"
            | "that"
            | "this"
            | "from"
            | "into"
            | "when"
            | "rule"
            | "rules"
            | "general"
            | "always"
            | "never"
            | "should"
            | "would"
            | "could"
            | "don't"
            | "dont"
            | "doesn't"
            | "doesnt"
            | "do"
            | "not"
            | "use"
            | "using"
            | "used"
            | "user"
            | "task"
            | "please"
            | "help"
            | "need"
            | "want"
            | "about"
            | "because"
            | "instead"
            | "prefer"
            | "run"
    ) {
        return false;
    }
    if matches!(
        term,
        "的" | "了"
            | "是"
            | "在"
            | "和"
            | "与"
            | "或"
            | "这"
            | "那"
            | "用"
            | "要"
            | "不"
            | "做"
            | "说"
            | "把"
            | "给"
            | "对"
            | "错"
    ) {
        return false;
    }
    true
}

/// Connection parameters for an OpenAI-compatible LLM endpoint.
/// Resolved from the model registry via `resolve_memory_model`.
#[derive(Clone)]
pub struct LlmConnParams {
    pub base_url: String,
    pub api_key: String,
    pub model_name: String,
    pub wire_model_name: Option<String>,
    pub provider: String,
    pub header_overrides: HashMap<String, String>,
    pub request_body_overrides: Option<serde_json::Map<String, serde_json::Value>>,
    pub completions_url_override: Option<String>,
    pub request_timeout: Option<Duration>,
}

impl LlmConnParams {
    pub fn from_resolved(
        model: astra_services::models::ResolvedActiveLlmModel,
    ) -> Result<Self, String> {
        let header_overrides = model.execution_header_overrides()?;
        Ok(Self {
            base_url: model.base_url,
            api_key: model.api_key,
            model_name: model.model_name,
            wire_model_name: model.wire_model_name,
            provider: model.provider,
            header_overrides,
            request_body_overrides: model.request_body_overrides,
            completions_url_override: None,
            request_timeout: None,
        })
    }

    pub(crate) fn execution_route(&self) -> LlmExecutionRoute<'_> {
        LlmExecutionRoute {
            model_name: &self.model_name,
            wire_model_name: self.wire_model_name.as_deref(),
            api_key: &self.api_key,
            base_url: &self.base_url,
            provider: &self.provider,
            header_overrides: (!self.header_overrides.is_empty()).then_some(&self.header_overrides),
            request_body_overrides: self.request_body_overrides.as_ref(),
            completions_url_override: self.completions_url_override.as_deref(),
            request_timeout: self.request_timeout,
        }
    }
}

impl std::fmt::Debug for LlmConnParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmConnParams")
            .field("model_name", &self.model_name)
            .field("wire_model_name", &self.wire_model_name)
            .field("provider", &self.provider)
            .field("credential_present", &!self.api_key.is_empty())
            .field("header_count", &self.header_overrides.len())
            .field(
                "request_body_overrides_present",
                &self.request_body_overrides.is_some(),
            )
            .field(
                "completions_url_override_present",
                &self.completions_url_override.is_some(),
            )
            .field("request_timeout", &self.request_timeout)
            .finish()
    }
}

/// Filter a list of text items through the selector model.
/// Returns only items deemed relevant to `user_message`.
///
/// On transport/model errors, falls back to the deterministic lexical gate.
/// If the selector explicitly returns no relevant indices, returns an empty
/// list. Prompt noise is more harmful than a missed memory.
pub async fn filter_memories(
    params: &LlmConnParams,
    user_message: &str,
    items: &[String],
) -> Vec<String> {
    if items.is_empty() {
        return Vec::new();
    }
    let query = build_relevance_query(user_message, items);
    let text = match run_selector_prompt(
        params,
        InferencePurpose::MemoryRetrievalRerank,
        RELEVANCE_FILTER_PROMPT,
        query,
    )
    .await
    {
        Some(text) => text,
        None => return lexical_filter_memories(user_message, items),
    };

    let indices = match parse_relevance_response(&text, items.len()) {
        Ok(indices) => indices,
        Err(error) => {
            tracing::debug!(
                target: "astra_runtime::memory_relevance",
                model_name = %params.model_name,
                purpose = InferencePurpose::MemoryRetrievalRerank.as_str(),
                %error,
                "memory selector returned an invalid response"
            );
            return lexical_filter_memories(user_message, items);
        }
    };
    if indices.is_empty() {
        return Vec::new();
    }

    filter_by_indices(items, &indices)
}

/// Use the selector model to identify which previously injected candidates the
/// user is explicitly rejecting. On any failure, returns an empty set rather
/// than guessing from surface words.
pub async fn select_dismissed_memory_indices(
    params: &LlmConnParams,
    user_message: &str,
    items: &[String],
) -> Vec<usize> {
    if items.is_empty() {
        return Vec::new();
    }
    let query = build_memory_feedback_query(user_message, items);
    let text = match run_selector_prompt(
        params,
        InferencePurpose::MemoryRetrievalRerank,
        MEMORY_FEEDBACK_FILTER_PROMPT,
        query,
    )
    .await
    {
        Some(text) => text,
        None => return Vec::new(),
    };
    match parse_relevance_response(&text, items.len()) {
        Ok(indices) => indices,
        Err(error) => {
            tracing::debug!(
                target: "astra_runtime::memory_relevance",
                model_name = %params.model_name,
                purpose = InferencePurpose::MemoryRetrievalRerank.as_str(),
                %error,
                "memory feedback selector returned an invalid response"
            );
            Vec::new()
        }
    }
}

async fn run_selector_prompt(
    params: &LlmConnParams,
    purpose: InferencePurpose,
    system_prompt: &str,
    user_content: String,
) -> Option<String> {
    let messages = [
        serde_json::json!({"role": "system", "content": system_prompt}),
        serde_json::json!({"role": "user", "content": user_content}),
    ];
    let result = call_llm_nonstream(
        global_llm_client(),
        LlmCall {
            purpose,
            messages: &messages,
            tools: &[],
            route: params.execution_route(),
            max_output_tokens: Some(50),
            temperature: Some(0.0),
            has_fallback: false,
            thinking: &ThinkingConfig::Off,
        },
        Duration::from_secs(3),
    )
    .await;
    let text = match result {
        Ok(result) if !result.full_text.trim().is_empty() => result.full_text,
        Ok(_) => return None,
        Err(error) => {
            tracing::debug!(
                target: "astra_runtime::memory_relevance",
                model_name = %params.model_name,
                purpose = purpose.as_str(),
                error_kind = %error.kind,
                "memory selector model call unavailable"
            );
            return None;
        }
    };
    if text.trim().is_empty() {
        return None;
    }

    let stripped = astra_turn_core::thinking_config::strip_think_tags(&text);
    Some(if stripped.trim().is_empty() {
        text
    } else {
        stripped
    })
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relevance_response_requires_a_json_index_array() {
        let cases: &[(&str, usize, &[usize])] = &[
            ("[0, 2, 4]", 5, &[0, 2, 4]),
            ("[0, 2, 10]", 3, &[0, 2]),
            ("[1, 1, 3]", 5, &[1, 3]),
            ("[]", 5, &[]),
            ("[10, 20, 30]", 5, &[]),
            ("[0, 99, 2, 150]", 5, &[0, 2]),
        ];
        for (input, max, expected) in cases {
            assert_eq!(
                parse_relevance_response(input, *max).unwrap(),
                *expected,
                "input={input:?}, max={max}"
            );
        }
        for invalid in ["0, 2", "```json\n[1, 3]\n```", "[-1, 0, 2]"] {
            assert!(parse_relevance_response(invalid, 5).is_err());
        }
    }

    #[test]
    fn test_filter_by_indices() {
        let items = vec!["a", "b", "c", "d", "e"];
        assert_eq!(filter_by_indices(&items, &[4, 1]), vec!["e", "b"]);
        assert!(filter_by_indices(&items, &[]).is_empty());
    }

    #[test]
    fn lexical_filter_keeps_only_evidenced_items() {
        let items = vec![
            "Do not treat curl checks as browser verification".into(),
            "Prefer cargo test for Rust executor changes".into(),
        ];
        let result = lexical_filter_memories("review Rust executor code", &items);
        assert_eq!(
            result,
            vec!["Prefer cargo test for Rust executor changes".to_string()]
        );
    }

    #[test]
    fn lexical_filter_handles_chinese_ascii_mixed_terms() {
        let items = vec!["不要用bash执行git命令".into(), "always run clippy".into()];
        let result = lexical_filter_memories("用bash运行测试", &items);
        assert_eq!(result, vec!["不要用bash执行git命令".to_string()]);
    }

    #[test]
    fn build_query_includes_all_memories() {
        let query = build_relevance_query(
            "fix auth bug",
            &["use rg not grep".into(), "RS256 for JWT".into()],
        );
        assert!(query.contains("fix auth bug"));
        assert!(query.contains("[0] use rg not grep"));
        assert!(query.contains("[1] RS256 for JWT"));
        assert!(query.contains("Relevant indices"));
    }

    #[test]
    fn build_feedback_query_includes_candidates() {
        let query = build_memory_feedback_query(
            "the first candidate should not apply here",
            &["candidate one".into(), "candidate two".into()],
        );
        assert!(query.contains("Latest user message"));
        assert!(query.contains("[0] candidate one"));
        assert!(query.contains("[1] candidate two"));
        assert!(query.contains("Rejected candidate indices"));
    }

    #[test]
    fn build_query_truncates_long_inputs() {
        let long_msg = "x".repeat(500);
        let query = build_relevance_query(&long_msg, &["short".into()]);
        assert!(query.len() < 500 + 200); // truncated message + memory
    }

    // ── LlmConnParams tests ──

    #[test]
    fn llm_conn_params_clone() {
        let params = LlmConnParams {
            base_url: "https://api.example.com/v1".into(),
            api_key: "sk-test".into(),
            model_name: "qwen-flash".into(),
            wire_model_name: None,
            provider: "openai".into(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let cloned = params.clone();
        assert_eq!(cloned.base_url, "https://api.example.com/v1");
        assert_eq!(cloned.api_key, "sk-test");
        assert_eq!(cloned.model_name, "qwen-flash");
    }

    #[test]
    fn llm_conn_params_debug_format() {
        let params = LlmConnParams {
            base_url: "http://localhost:8080".into(),
            api_key: "key".into(),
            model_name: "model".into(),
            wire_model_name: None,
            provider: "openai".into(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let debug = format!("{params:?}");
        assert!(debug.contains("LlmConnParams"));
        assert!(debug.contains("model"));
        assert!(!debug.contains("key"));
        assert!(!debug.contains("localhost"));
    }

    // ── filter_memories tests ──

    #[tokio::test]
    async fn filter_memories_empty_input_returns_empty() {
        let params = LlmConnParams {
            base_url: "http://nonexistent:9999".into(),
            api_key: "key".into(),
            model_name: "model".into(),
            wire_model_name: None,
            provider: "openai".into(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let result = filter_memories(&params, "query", &[]).await;
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn filter_memories_unreachable_server_uses_lexical_fallback() {
        let params = LlmConnParams {
            base_url: "http://127.0.0.1:1".into(),
            api_key: "key".into(),
            model_name: "model".into(),
            wire_model_name: None,
            provider: "openai".into(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let items = vec![
            "browser verification for html pages".into(),
            "cargo test for rust executor changes".into(),
        ];
        let result = filter_memories(&params, "rust executor review", &items).await;
        assert_eq!(
            result,
            vec!["cargo test for rust executor changes".to_string()],
            "unreachable server should fall back to local relevance"
        );
    }

    // ── Mock server integration tests ────────────────────────────────────

    use std::sync::{Arc, Mutex};

    async fn spawn_mock_completions(
        captured: Arc<Mutex<Option<serde_json::Value>>>,
        response_content: &'static str,
    ) -> String {
        use axum::{Router, routing::post};

        let handler = move |axum::Json(body): axum::Json<serde_json::Value>| {
            let captured = captured.clone();
            async move {
                *captured.lock().unwrap() = Some(body);
                axum::Json(serde_json::json!({
                    "choices": [{"message": {"content": response_content}}]
                }))
            }
        };
        let app = Router::new().route("/chat/completions", post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn filter_memories_native_thinker_sends_suppression() {
        let captured = Arc::new(Mutex::new(None));
        let base = spawn_mock_completions(captured.clone(), "[0]").await;
        let params = LlmConnParams {
            base_url: base,
            api_key: "k".into(),
            model_name: "qwen3.5-flash".into(),
            wire_model_name: None,
            provider: "dashscope".into(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let items = vec!["mem-a".into(), "mem-b".into()];
        let _ = filter_memories(&params, "test query", &items).await;

        let body = captured.lock().unwrap().take().expect("request captured");
        assert_eq!(
            body["enable_thinking"], false,
            "native thinker should send enable_thinking: false"
        );
    }

    #[tokio::test]
    async fn filter_memories_non_native_does_not_send_suppression() {
        let captured = Arc::new(Mutex::new(None));
        let base = spawn_mock_completions(captured.clone(), "[0]").await;
        let params = LlmConnParams {
            base_url: base,
            api_key: "k".into(),
            model_name: "gpt-4o-mini".into(),
            wire_model_name: None,
            provider: "openai".into(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let items = vec!["mem-a".into()];
        let _ = filter_memories(&params, "test", &items).await;

        let body = captured.lock().unwrap().take().expect("request captured");
        assert!(
            body.get("enable_thinking").is_none(),
            "non-native should not have enable_thinking: {body}"
        );
    }

    #[tokio::test]
    async fn filter_memories_strips_think_tags_from_response() {
        let captured = Arc::new(Mutex::new(None));
        let base = spawn_mock_completions(captured.clone(), "<think>reasoning</think>[0, 2]").await;
        let params = LlmConnParams {
            base_url: base,
            api_key: "k".into(),
            model_name: "m".into(),
            wire_model_name: None,
            provider: "openai".into(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let items: Vec<String> = (0..3).map(|i| format!("mem-{i}")).collect();
        let result = filter_memories(&params, "query", &items).await;
        assert_eq!(result, vec!["mem-0", "mem-2"]);
    }

    #[tokio::test]
    async fn filter_memories_malformed_selector_output_uses_lexical_fallback() {
        let captured = Arc::new(Mutex::new(None));
        let base = spawn_mock_completions(captured.clone(), "<think>[0, 1]</think>").await;
        let params = LlmConnParams {
            base_url: base,
            api_key: "k".into(),
            model_name: "m".into(),
            wire_model_name: None,
            provider: "openai".into(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let items = vec![
            "cargo test for rust executor changes".to_string(),
            "browser verification for html pages".to_string(),
        ];
        let result = filter_memories(&params, "rust executor review", &items).await;
        assert_eq!(
            result,
            vec!["cargo test for rust executor changes".to_string()]
        );
    }

    #[tokio::test]
    async fn filter_memories_successful_filtering() {
        let captured = Arc::new(Mutex::new(None));
        let base = spawn_mock_completions(captured.clone(), "[1]").await;
        let params = LlmConnParams {
            base_url: base,
            api_key: "k".into(),
            model_name: "m".into(),
            wire_model_name: None,
            provider: "openai".into(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let items = vec!["irrelevant".into(), "relevant".into(), "noise".into()];
        let result = filter_memories(&params, "query", &items).await;
        assert_eq!(result, vec!["relevant"]);
    }

    #[tokio::test]
    async fn filter_memories_selector_empty_means_no_injection() {
        let captured = Arc::new(Mutex::new(None));
        let base = spawn_mock_completions(captured.clone(), "[]").await;
        let params = LlmConnParams {
            base_url: base,
            api_key: "k".into(),
            model_name: "m".into(),
            wire_model_name: None,
            provider: "openai".into(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let items = vec!["cargo test for rust executor changes".into()];
        let result = filter_memories(&params, "rust executor review", &items).await;
        assert!(
            result.is_empty(),
            "selector's explicit empty relevance result should be respected"
        );
    }

    #[tokio::test]
    async fn select_dismissed_memory_indices_uses_selector_output() {
        let captured = Arc::new(Mutex::new(None));
        let base = spawn_mock_completions(captured.clone(), "[0]").await;
        let params = LlmConnParams {
            base_url: base,
            api_key: "k".into(),
            model_name: "m".into(),
            wire_model_name: None,
            provider: "openai".into(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let items = vec![
            "candidate about browser verification".into(),
            "candidate about rust tests".into(),
        ];
        let dismissed = select_dismissed_memory_indices(
            &params,
            "the first candidate should not apply",
            &items,
        )
        .await;
        assert_eq!(dismissed, vec![0]);

        let body = captured.lock().unwrap().take().expect("request captured");
        assert_eq!(
            body["messages"][0]["content"],
            MEMORY_FEEDBACK_FILTER_PROMPT
        );
    }
}
