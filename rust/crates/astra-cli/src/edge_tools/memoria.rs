//! Memoria (memory service) integration for tool execution.
//!
//! Provides HTTP client for storing, retrieving, and managing memories
//! via the Memoria API, with circuit breaker for resilience.

use std::time::Duration;

use serde_json::{json, Value};

use super::ToolExecutor;

/// Parse content strings from a Memoria search/retrieve response.
///
/// Handles common Memoria response shapes:
/// - `{ "memories": [ { "content": "..." }, ... ] }`
/// - `[ { "content": "..." }, ... ]`
/// - `{ "results": [ { "content": "..." }, ... ] }`
///
/// Returns empty vec on parse failure or error responses (graceful degradation).
pub fn parse_memory_search_contents(raw: &str) -> Vec<String> {
    let Ok(val) = serde_json::from_str::<Value>(raw) else {
        return vec![];
    };
    // Error response from memoria
    if val.get("error").is_some() {
        return vec![];
    }
    // Try common response shapes
    let items = val
        .get("memories")
        .or_else(|| val.get("results"))
        .and_then(Value::as_array)
        .or_else(|| val.as_array());

    let Some(arr) = items else {
        // Single object with content?
        if let Some(c) = val.get("content").and_then(Value::as_str) {
            return vec![c.to_string()];
        }
        return vec![];
    };

    arr.iter()
        .filter_map(|item| {
            item.get("content")
                .or_else(|| item.get("text"))
                .and_then(Value::as_str)
                .map(|s| s.to_string())
        })
        .filter(|s| !s.is_empty())
        .collect()
}

impl ToolExecutor {
    pub(super) async fn memoria_call(&self, op: &str, args: &Value) -> String {
        self.memoria_call_with_timeout(op, args, Duration::from_secs(10))
            .await
    }

    pub(super) async fn memoria_call_with_timeout(&self, op: &str, args: &Value, timeout: Duration) -> String {
        // Circuit breaker: skip after 2 consecutive failures (reset on success)
        const MAX_FAILS: u32 = 2;
        if self
            .memoria_fail_count
            .load(std::sync::atomic::Ordering::Relaxed)
            >= MAX_FAILS
        {
            return json!({"error": "Memory service unavailable (circuit open)"}).to_string();
        }

        // Build endpoint and payload
        let (endpoint, payload, auth_header) = if let (Some(cloud_base), Some(token)) =
            (&self.cloud_base, &self.cloud_token)
        {
            (
                format!("{cloud_base}/memory/{op}"),
                args.clone(),
                format!("Bearer {token}"),
            )
        } else {
            let base = std::env::var("MEMORIA_BASE_URL")
                .unwrap_or_else(|_| astra_core::config::DEFAULT_MEMORIA_URL.to_string());
            let key = match std::env::var("MEMORIA_API_KEY")
                .ok()
                .or_else(|| std::env::var("MEMORIA_MASTER_KEY").ok())
            {
                Some(k) => k,
                None => {
                    return json!({
                            "error": "Memory unavailable: not connected to cloud and MEMORIA_API_KEY not set",
                            "hint": "Login with /login to enable cloud-backed memory with user isolation"
                        })
                        .to_string();
                }
            };

            let (ep, pl) = match op {
                "retrieve" => {
                    let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                    let top_k = args.get("top_k").and_then(Value::as_u64).unwrap_or(5);
                    (
                        format!("{base}/v1/memories/retrieve"),
                        json!({"query": query, "top_k": top_k}),
                    )
                }
                "store" => {
                    let content = args.get("content").and_then(Value::as_str).unwrap_or("");
                    let memory_type = args
                        .get("memory_type")
                        .and_then(Value::as_str)
                        .unwrap_or("semantic");
                    (
                        format!("{base}/v1/memories"),
                        json!({"content": content, "memory_type": memory_type}),
                    )
                }
                "search" => {
                    let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                    let top_k = args.get("top_k").and_then(Value::as_u64).unwrap_or(10);
                    (
                        format!("{base}/v1/memories/search"),
                        json!({"query": query, "top_k": top_k}),
                    )
                }
                "purge" => {
                    let topic = args.get("topic").and_then(Value::as_str).unwrap_or("");
                    let reason = args
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("user request");
                    (
                        format!("{base}/v1/memories/purge"),
                        json!({"topic": topic, "reason": reason}),
                    )
                }
                "correct" => {
                    let memory_id = args.get("memory_id").and_then(Value::as_str).unwrap_or("");
                    let new_content = args
                        .get("new_content")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let reason = args
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("correction");
                    (
                        format!("{base}/v1/memories/correct"),
                        json!({"memory_id": memory_id, "new_content": new_content, "reason": reason}),
                    )
                }
                "profile" => (format!("{base}/v1/memories/profile"), json!({})),
                _ => return format!("Unknown memoria op: {op}"),
            };
            (ep, pl, format!("Bearer {key}"))
        };

        match reqwest::Client::builder()
            .timeout(timeout)
            .no_proxy()
            .build()
        {
            Ok(client) => match client
                .post(&endpoint)
                .header("Authorization", &auth_header)
                .json(&payload)
                .send()
                .await
            {
                Ok(resp) => match resp.text().await {
                    Ok(text) => {
                        self.memoria_fail_count
                            .store(0, std::sync::atomic::Ordering::Relaxed);
                        text
                    }
                    Err(e) => {
                        self.memoria_fail_count
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        json!({"error": format!("read response: {e}")}).to_string()
                    }
                },
                Err(e) => {
                    self.memoria_fail_count
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    json!({"error": format!("memoria request failed: {e}")}).to_string()
                }
            },
            Err(e) => json!({"error": format!("build client: {e}")}).to_string(),
        }
    }

    pub async fn memory_boost_search(&self, query: &str, top_k: u64) -> Vec<String> {
        if query.trim().is_empty() {
            return vec![];
        }
        // Direct Memoria call (skip cloud proxy — server has no /memory/* route).
        // This is best-effort on the critical path; circuit breaker prevents
        // repeated timeouts if Memoria is down.
        if self
            .memoria_fail_count
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 2
        {
            return vec![];
        }
        let base = std::env::var("MEMORIA_BASE_URL")
            .unwrap_or_else(|_| astra_core::config::DEFAULT_MEMORIA_URL.to_string());
        let key = match std::env::var("MEMORIA_API_KEY")
            .ok()
            .or_else(|| std::env::var("MEMORIA_MASTER_KEY").ok())
        {
            Some(k) => k,
            None => return vec![], // No key = no Memoria
        };
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_millis(800))
            .no_proxy()
            .build()
        {
            Ok(c) => c,
            Err(_) => return vec![],
        };
        match client
            .post(format!("{base}/v1/memories/search"))
            .header("Authorization", format!("Bearer {key}"))
            .json(&json!({"query": query, "top_k": top_k}))
            .send()
            .await
        {
            Ok(resp) => {
                self.memoria_fail_count
                    .store(0, std::sync::atomic::Ordering::Relaxed);
                let text = resp.text().await.unwrap_or_default();
                parse_memory_search_contents(&text)
            }
            Err(_) => {
                self.memoria_fail_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                vec![]
            }
        }
    }
}
