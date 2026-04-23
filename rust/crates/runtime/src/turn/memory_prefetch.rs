//! Memory prefetch utilities for LLM prompt augmentation.
//!
//! Provides hybrid retrieval (full message + entity-keyword) from Memoria HTTP API,
//! merging and deduplicating results into a structured section for injection into
//! the system prompt.

use std::time::Instant;

/// Result of a memory prefetch operation.
#[derive(Debug, Default)]
pub struct MemoryPrefetchResult {
    pub section: Option<String>,
    pub items: usize,
    pub preview: Vec<String>,
    pub fetch_ms: i64,
}

/// Prefetch memories relevant to the user message via hybrid retrieval.
/// Sends two queries (full message + entity tokens), merges and deduplicates.
pub async fn prefetch_memories(
    mem_url: &str,
    mem_key: &str,
    user_msg: &str,
    user_id: &str,
    top_k: u32,
) -> MemoryPrefetchResult {
    if mem_key.is_empty() || user_msg.trim().is_empty() {
        return MemoryPrefetchResult::default();
    }
    let started = Instant::now();
    let entity_query = extract_entity_tokens(user_msg);
    let trimmed_msg = user_msg.trim();

    // Parallel fetch: full message retrieval + entity-keyword retrieval via tokio::join!
    let do_entity = !entity_query.is_empty() && entity_query != trimmed_msg;
    let (full_result, entity_result) = tokio::join!(
        fetch_memories(mem_url, mem_key, trimmed_msg, user_id, top_k),
        async {
            if do_entity {
                fetch_memories(mem_url, mem_key, &entity_query, user_id, top_k).await
            } else {
                String::new()
            }
        }
    );
    let merged = merge_memory_results(&[&full_result, &entity_result]);
    let fetch_ms = started.elapsed().as_millis() as i64;
    let preview = merged.iter().take(3).map(|l| l.to_string()).collect();
    let items = merged.len();
    let section = build_memory_section(&merged);
    MemoryPrefetchResult {
        section,
        items,
        preview,
        fetch_ms,
    }
}

/// Merge and deduplicate memory results from multiple retrieval queries.
pub(crate) fn merge_memory_results(results: &[&str]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut merged = Vec::new();
    for result in results {
        for line in result.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
                merged.push(trimmed.to_string());
            }
        }
    }
    merged
}

/// Build the memory section for the profile block.
/// Returns None if no memories matched.
pub(crate) fn build_memory_section(merged_lines: &[String]) -> Option<String> {
    if merged_lines.is_empty() {
        return None;
    }
    let refs: Vec<&str> = merged_lines.iter().map(|s| s.as_str()).collect();
    let formatted = crate::prompts::memory_proto::format_for_llm(&refs);
    if !formatted.is_empty() {
        Some(format!("## User Memories\n{formatted}"))
    } else {
        Some(format!("## User Memories\n{}", merged_lines.join("\n")))
    }
}

/// Extract non-CJK, non-punctuation tokens from a message for keyword-based retrieval.
pub(crate) fn extract_entity_tokens(msg: &str) -> String {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in msg.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            current.push(ch);
        } else {
            if current.len() >= 3 {
                tokens.push(current.clone());
            }
            current.clear();
        }
    }
    if current.len() >= 3 {
        tokens.push(current);
    }
    tokens.join(" ")
}

/// Fetch memories from Memoria HTTP API. Returns joined content string.
async fn fetch_memories(
    base_url: &str,
    api_key: &str,
    query: &str,
    user_id: &str,
    top_k: u32,
) -> String {
    let client = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let mut payload = serde_json::json!({"query": query, "top_k": top_k});
    if !user_id.is_empty() {
        payload["session_id"] = serde_json::Value::String(user_id.to_string());
        payload["user_id"] = serde_json::Value::String(user_id.to_string());
    }
    let resp = match client
        .post(format!("{base_url}/v1/memories/retrieve"))
        .header("Authorization", format!("Bearer {api_key}"))
        .json(&payload)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            astra_core::agent_error!("memory", "fetch error: {e:#}");
            return String::new();
        }
    };
    if !resp.status().is_success() {
        return String::new();
    }
    let arr = match resp.json::<Vec<serde_json::Value>>().await {
        Ok(a) => a,
        Err(_) => return String::new(),
    };
    arr.iter()
        .filter_map(|m| m.get("content").and_then(|c| c.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_entity_tokens_empty_string() {
        assert_eq!(extract_entity_tokens(""), "");
    }

    #[test]
    fn extract_entity_tokens_short_words_filtered() {
        assert_eq!(extract_entity_tokens("a bc"), "");
    }

    #[test]
    fn extract_entity_tokens_preserves_long_tokens() {
        assert_eq!(extract_entity_tokens("hello world"), "hello world");
    }

    #[test]
    fn extract_entity_tokens_special_chars_split() {
        assert_eq!(extract_entity_tokens("hello.world!foo"), "hello world foo");
    }

    #[test]
    fn extract_entity_tokens_hyphens_and_underscores_kept() {
        assert_eq!(extract_entity_tokens("my-var_name"), "my-var_name");
    }

    #[test]
    fn extract_entity_tokens_unicode_chars_as_delimiters() {
        assert_eq!(extract_entity_tokens("memoria 最新的ci?"), "memoria");
    }

    #[test]
    fn extract_entity_tokens_only_special_chars() {
        assert_eq!(extract_entity_tokens("!@#$%"), "");
    }

    #[test]
    fn extract_entity_tokens_from_mixed_language() {
        assert_eq!(extract_entity_tokens("memoria 最新的ci?"), "memoria");
        assert_eq!(
            extract_entity_tokens("matrixone latest pr"),
            "matrixone latest"
        );
        assert_eq!(extract_entity_tokens("你好"), "");
        assert_eq!(
            extract_entity_tokens("check astra status"),
            "check astra status"
        );
    }

    #[test]
    fn merge_deduplicates_across_queries() {
        let r1 = "[@fact/semantic] memoria is matrixorigin/memoria\nsome other fact";
        let r2 = "[@fact/semantic] memoria is matrixorigin/memoria\nnew fact";
        let merged = merge_memory_results(&[r1, r2]);
        assert_eq!(
            merged.len(),
            3,
            "duplicate should be removed, got: {merged:?}"
        );
        assert!(merged.contains(&"[@fact/semantic] memoria is matrixorigin/memoria".to_string()));
        assert!(merged.contains(&"some other fact".to_string()));
        assert!(merged.contains(&"new fact".to_string()));
    }

    #[test]
    fn merge_skips_empty_lines() {
        let r1 = "line1\n\n\nline2";
        let r2 = "";
        let merged = merge_memory_results(&[r1, r2]);
        assert_eq!(merged, vec!["line1", "line2"]);
    }

    #[test]
    fn merge_empty_inputs() {
        assert!(merge_memory_results(&["", ""]).is_empty());
        assert!(merge_memory_results(&[]).is_empty());
    }

    #[test]
    fn build_memory_section_returns_none_for_empty() {
        assert!(build_memory_section(&[]).is_none());
    }

    #[test]
    fn build_memory_section_includes_header() {
        let lines = vec!["[@pref/active] memoria = matrixorigin/Memoria".to_string()];
        let section = build_memory_section(&lines).unwrap();
        assert!(section.starts_with("## User Memories"), "got: {section}");
    }

    #[test]
    fn build_memory_section_formats_structured_entries() {
        let lines = vec!["[@pref/active] dark mode preferred".to_string()];
        let section = build_memory_section(&lines).unwrap();
        assert!(
            section.contains("Preferences"),
            "structured entries should be grouped, got: {section}"
        );
    }

    #[test]
    fn build_memory_section_handles_unstructured() {
        let lines = vec!["just a plain memory without tags".to_string()];
        let section = build_memory_section(&lines).unwrap();
        assert!(section.contains("just a plain memory"), "got: {section}");
    }

    #[test]
    fn entity_query_differs_from_mixed_language_input() {
        let msg = "memoria 最新的ci?";
        let entity = extract_entity_tokens(msg);
        assert_ne!(
            entity,
            msg.trim(),
            "entity query should differ for mixed-language"
        );
        assert_eq!(entity, "memoria");
    }

    #[test]
    fn entity_query_same_for_pure_ascii() {
        let msg = "memoria latest ci";
        let entity = extract_entity_tokens(msg);
        assert_eq!(
            entity, "memoria latest",
            "pure ASCII: entity ≈ original (minus short words)"
        );
    }

    #[tokio::test]
    async fn prefetch_memories_empty_key_returns_default() {
        let result = prefetch_memories("http://localhost", "", "query", "user1", 5).await;
        assert!(result.section.is_none());
        assert_eq!(result.items, 0);
    }

    #[tokio::test]
    async fn prefetch_memories_whitespace_message_returns_default() {
        let result = prefetch_memories("http://localhost", "key", "   ", "user1", 5).await;
        assert!(result.section.is_none());
        assert_eq!(result.items, 0);
    }

    #[test]
    fn memory_prefetch_result_default() {
        let r = MemoryPrefetchResult::default();
        assert!(r.section.is_none());
        assert_eq!(r.items, 0);
        assert!(r.preview.is_empty());
        assert_eq!(r.fetch_ms, 0);
    }

    /// audit-A2: fetch_memories must time out on an unresponsive Memoria server
    /// instead of blocking the turn pipeline indefinitely.
    #[tokio::test]
    async fn fetch_memories_times_out_on_unresponsive_server() {
        // Black-hole server: accepts connections, never responds.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move {
            loop {
                let (sock, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                    drop(sock);
                });
            }
        });

        let start = std::time::Instant::now();
        let result = fetch_memories(
            &format!("http://{addr}"),
            "test-key",
            "test query",
            "user1",
            5,
        )
        .await;
        let elapsed = start.elapsed();

        // fetch_memories returns empty string on error, not Err
        assert!(result.is_empty(), "should return empty on timeout");
        assert!(
            elapsed < std::time::Duration::from_secs(30),
            "should time out well before 30s, took {elapsed:?}"
        );
    }
}
