//! Memory relevance filtering via a cheap selector model.
//!
//! Filters retrieved memories/lessons to only those clearly relevant
//! to the current task, reducing prompt noise and token waste.
//! Uses the cheapest `selector`-tagged model from the registry
//! (resolved via `resolve_memory_model` from the model DB).

/// Prompt for the selector model to judge memory relevance.
pub const RELEVANCE_FILTER_PROMPT: &str = "\
You are filtering retrieved memories for relevance to a user's task.
Return ONLY a JSON array of indices for memories that are CLEARLY useful.
If unsure whether a memory is relevant, EXCLUDE it — false negatives
are better than noise. Return [] if nothing is relevant.";

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

/// Connection parameters for an OpenAI-compatible LLM endpoint.
/// Resolved from the model registry via `resolve_memory_model`.
#[derive(Debug, Clone)]
pub struct LlmConnParams {
    pub base_url: String,
    pub api_key: String,
    pub model_name: String,
    pub provider: String,
}

/// Filter a list of text items through the selector model.
/// Returns only items deemed relevant to `user_message`.
///
/// On any error (network, timeout, parse failure) returns the original
/// list unchanged — graceful degradation over silent filtering.
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
        Err(_) => return items.to_vec(),
    };

    let mut req_body = serde_json::json!({
        "model": params.model_name,
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
        Err(_) => return items.to_vec(),
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
        return items.to_vec();
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
        return items.to_vec();
    }

    filter_by_indices(items, &indices)
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
    fn parse_json_array() {
        assert_eq!(parse_relevance_response("[0, 2, 4]", 5), vec![0, 2, 4]);
    }

    #[test]
    fn parse_json_array_filters_out_of_bounds() {
        assert_eq!(parse_relevance_response("[0, 2, 10]", 3), vec![0, 2]);
    }

    #[test]
    fn parse_markdown_wrapped() {
        assert_eq!(
            parse_relevance_response("```json\n[1, 3]\n```", 5),
            vec![1, 3]
        );
    }

    #[test]
    fn parse_bare_numbers() {
        assert_eq!(parse_relevance_response("0, 2", 5), vec![0, 2]);
    }

    #[test]
    fn parse_empty_array() {
        assert!(parse_relevance_response("[]", 5).is_empty());
    }

    #[test]
    fn parse_garbage_returns_empty() {
        assert!(parse_relevance_response("no relevant memories", 5).is_empty());
    }

    #[test]
    fn filter_by_indices_preserves_order() {
        let items = vec!["a", "b", "c", "d", "e"];
        let filtered = filter_by_indices(&items, &[4, 1]);
        assert_eq!(filtered, vec!["e", "b"]);
    }

    #[test]
    fn filter_empty_indices_returns_empty() {
        let items = vec!["a", "b"];
        assert!(filter_by_indices(&items, &[]).is_empty());
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
    fn build_query_truncates_long_inputs() {
        let long_msg = "x".repeat(500);
        let query = build_relevance_query(&long_msg, &["short".into()]);
        assert!(query.len() < 500 + 200); // truncated message + memory
    }

    // ── parse_relevance_response edge cases ──

    #[test]
    fn parse_all_out_of_bounds_returns_empty() {
        assert!(parse_relevance_response("[10, 20, 30]", 5).is_empty());
    }

    #[test]
    fn parse_mixed_valid_invalid_indices() {
        assert_eq!(parse_relevance_response("[0, 99, 2, 150]", 5), vec![0, 2]);
    }

    #[test]
    fn parse_negative_in_json_falls_back_to_digit_extraction() {
        // JSON parse fails (usize can't be negative), fallback extracts digits:
        // "-1" splits on '-' → "1", so [1, 0, 2] are the extracted indices.
        assert_eq!(parse_relevance_response("[-1, 0, 2]", 5), vec![1, 0, 2]);
    }

    // ── LlmConnParams tests ──

    #[test]
    fn llm_conn_params_clone() {
        let params = LlmConnParams {
            base_url: "https://api.example.com/v1".into(),
            api_key: "sk-test".into(),
            model_name: "qwen-flash".into(),
            provider: "openai".into(),
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
            provider: "openai".into(),
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
            provider: "openai".into(),
        };
        let result = filter_memories(&params, "query", &[]).await;
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn filter_memories_unreachable_server_returns_all() {
        let params = LlmConnParams {
            base_url: "http://127.0.0.1:1".into(),
            api_key: "key".into(),
            model_name: "model".into(),
            provider: "openai".into(),
        };
        let items = vec!["a".into(), "b".into(), "c".into()];
        let result = filter_memories(&params, "query", &items).await;
        assert_eq!(result, items, "unreachable server should return all items");
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
            provider: "dashscope".into(),
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
            provider: "openai".into(),
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
            provider: "openai".into(),
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
            provider: "openai".into(),
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
            provider: "openai".into(),
        };
        let items = vec!["irrelevant".into(), "relevant".into(), "noise".into()];
        let result = filter_memories(&params, "query", &items).await;
        assert_eq!(result, vec!["relevant"]);
    }
}
