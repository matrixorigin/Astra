//! Memory relevance filtering via a cheap selector model.
//!
//! Filters retrieved memories/lessons to only those clearly relevant
//! to the current task, reducing prompt noise and token waste.
//! Uses the cheapest `selector`-tagged model from the registry
//! (resolved via `resolve_memory_model` from the model DB).

use std::collections::HashSet;

use astra_text_utils::text_tokenize::tokenize;

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

/// Parse the selector model's response into a list of indices.
/// Handles: `[0, 2]`, `[0,2]`, bare `0, 2`, and markdown-wrapped responses.
#[must_use]
pub fn parse_relevance_response(response: &str, memory_count: usize) -> Vec<usize> {
    let trimmed = response.trim();

    // Strip markdown code fences if present
    let clean = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|s| s.strip_suffix("```"))
        .unwrap_or(trimmed)
        .trim();

    // Try JSON array parse
    if let Ok(indices) = serde_json::from_str::<Vec<usize>>(clean) {
        return indices.into_iter().filter(|&i| i < memory_count).collect();
    }

    // Fallback: extract numbers from the string
    clean
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|s| s.parse::<usize>().ok())
        .filter(|&i| i < memory_count)
        .collect()
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
#[derive(Debug, Clone)]
pub struct LlmConnParams {
    pub base_url: String,
    pub api_key: String,
    pub model_name: String,
    pub wire_model_name: Option<String>,
    pub provider: String,
    pub request_body_overrides: Option<serde_json::Map<String, serde_json::Value>>,
    pub thinking_capability: Option<astra_services::models::ThinkingCapability>,
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

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .no_proxy()
        .build()
    {
        Ok(c) => c,
        Err(_) => return lexical_filter_memories(user_message, items),
    };

    let mut req_body = serde_json::json!({
        "model": params.wire_model_name.as_deref().unwrap_or(&params.model_name),
        "messages": [
            {"role": "system", "content": RELEVANCE_FILTER_PROMPT},
            {"role": "user", "content": query},
        ],
        "max_tokens": 50,
        "temperature": 0.0,
    });
    // Always suppress thinking for selector/memory calls — no point
    // spending tokens on reasoning for simple JSON tasks.
    astra_turn_core::thinking_config::ThinkingConfig::Off.apply_openai_suppression(
        &mut req_body,
        &params.provider,
        &params.base_url,
    );

    let resp = match client
        .post(format!("{}/chat/completions", params.base_url))
        .header("Authorization", format!("Bearer {}", params.api_key))
        .json(&req_body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return lexical_filter_memories(user_message, items),
    };

    let body = resp.text().await.unwrap_or_default();
    let text = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            v.get("choices")?
                .get(0)?
                .get("message")?
                .get("content")?
                .as_str()
                .map(String::from)
        })
        .unwrap_or_default();

    if text.is_empty() {
        return lexical_filter_memories(user_message, items);
    }

    // Safety net: strip <think> tags that native thinkers may emit despite suppression.
    // If stripping empties the text, fall back to the original.
    let stripped = astra_turn_core::thinking_config::strip_think_tags(&text);
    let text = if stripped.trim().is_empty() {
        text
    } else {
        stripped
    };

    let indices = parse_relevance_response(&text, items.len());
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
    let text = match run_selector_prompt(params, MEMORY_FEEDBACK_FILTER_PROMPT, query).await {
        Some(text) => text,
        None => return Vec::new(),
    };
    parse_relevance_response(&text, items.len())
}

async fn run_selector_prompt(
    params: &LlmConnParams,
    system_prompt: &str,
    user_content: String,
) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .no_proxy()
        .build()
        .ok()?;

    let mut req_body = serde_json::json!({
        "model": params.wire_model_name.as_deref().unwrap_or(&params.model_name),
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": user_content},
        ],
        "max_tokens": 50,
        "temperature": 0.0,
    });
    astra_turn_core::thinking_config::ThinkingConfig::Off.apply_openai_suppression(
        &mut req_body,
        &params.provider,
        &params.base_url,
    );

    let resp = client
        .post(format!("{}/chat/completions", params.base_url))
        .header("Authorization", format!("Bearer {}", params.api_key))
        .json(&req_body)
        .send()
        .await
        .ok()?;

    let body = resp.text().await.unwrap_or_default();
    let text = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            v.get("choices")?
                .get(0)?
                .get("message")?
                .get("content")?
                .as_str()
                .map(String::from)
        })?;
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
    fn test_parse_relevance_response() {
        // (input, max_items) → expected
        let cases: &[(&str, usize, &[usize])] = &[
            ("[0, 2, 4]", 5, &[0, 2, 4]),
            ("[0, 2, 10]", 3, &[0, 2]),
            ("```json\n[1, 3]\n```", 5, &[1, 3]),
            ("0, 2", 5, &[0, 2]),
            ("[]", 5, &[]),
            ("no relevant memories", 5, &[]),
            ("[10, 20, 30]", 5, &[]),
            ("[0, 99, 2, 150]", 5, &[0, 2]),
        ];
        for (input, max, expected) in cases {
            assert_eq!(
                parse_relevance_response(input, *max),
                *expected,
                "input={input:?}, max={max}"
            );
        }
        // Negative indices: JSON parse fails, fallback digit extraction yields [1,0,2]
        assert_eq!(parse_relevance_response("[-1, 0, 2]", 5), vec![1, 0, 2]);
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

    // ── parse_relevance_response edge cases (negatives) ──
    // Negative in JSON: JSON parse fails (usize can't be negative),
    // fallback digit extraction: "-1" splits on '-' → "1", so [1,0,2].
    // This case is included in test_parse_relevance_response above.

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
            thinking_capability: None,
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
            thinking_capability: None,
        };
        let debug = format!("{params:?}");
        assert!(debug.contains("LlmConnParams"));
        assert!(debug.contains("localhost"));
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
            thinking_capability: None,
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
            thinking_capability: None,
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
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
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
            thinking_capability: None,
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
            thinking_capability: None,
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
            thinking_capability: None,
        };
        let items: Vec<String> = (0..3).map(|i| format!("mem-{i}")).collect();
        let result = filter_memories(&params, "query", &items).await;
        assert_eq!(result, vec!["mem-0", "mem-2"]);
    }

    #[tokio::test]
    async fn filter_memories_think_wrapping_json_falls_back_to_original() {
        let captured = Arc::new(Mutex::new(None));
        // Model wraps JSON inside think tags — strip would empty it
        let base = spawn_mock_completions(captured.clone(), "<think>[0, 1]</think>").await;
        let params = LlmConnParams {
            base_url: base,
            api_key: "k".into(),
            model_name: "m".into(),
            wire_model_name: None,
            provider: "openai".into(),
            request_body_overrides: None,
            thinking_capability: None,
        };
        let items: Vec<String> = (0..3).map(|i| format!("mem-{i}")).collect();
        let result = filter_memories(&params, "query", &items).await;
        // Fallback to original text which contains the think-wrapped JSON
        // parse_relevance_response will extract digits from "<think>[0, 1]</think>"
        assert!(!result.is_empty(), "should fall back and parse something");
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
            thinking_capability: None,
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
            thinking_capability: None,
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
            thinking_capability: None,
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
