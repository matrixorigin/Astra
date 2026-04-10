//! Memoria (memory service) integration for tool execution.
//!
//! Provides HTTP client for storing, retrieving, and managing memories
//! via the Memoria API, with circuit breaker for resilience.

use std::time::Duration;

use serde_json::{Value, json};

use super::ToolExecutor;

/// A single memory hit from boost search, carrying both ID and content.
#[derive(Debug, Clone)]
pub struct BoostSearchHit {
    pub memory_id: Option<String>,
    pub content: String,
}

/// Parse content strings from a Memoria search/retrieve response.
///
/// Handles common Memoria response shapes:
/// - `{ "memories": [ { "content": "..." }, ... ] }`
/// - `[ { "content": "..." }, ... ]`
/// - `{ "results": [ { "content": "..." }, ... ] }`
///
/// Returns empty vec on parse failure or error responses (graceful degradation).
pub fn parse_memory_search_contents(raw: &str) -> Vec<String> {
    parse_memory_search_hits(raw)
        .into_iter()
        .map(|h| h.content)
        .collect()
}

/// Parse memory hits (ID + content) from a Memoria search/retrieve response.
///
/// Extracts both `memory_id` (or `id`) and `content` from each result item.
/// Falls back gracefully: missing IDs become `None`.
pub fn parse_memory_search_hits(raw: &str) -> Vec<BoostSearchHit> {
    let Ok(val) = serde_json::from_str::<Value>(raw) else {
        return vec![];
    };
    if val.get("error").is_some() {
        return vec![];
    }
    let items = val
        .get("memories")
        .or_else(|| val.get("results"))
        .and_then(Value::as_array)
        .or_else(|| val.as_array());

    let Some(arr) = items else {
        if let Some(c) = val.get("content").and_then(Value::as_str) {
            let mid = val
                .get("memory_id")
                .or_else(|| val.get("id"))
                .and_then(Value::as_str)
                .map(String::from);
            return vec![BoostSearchHit {
                memory_id: mid,
                content: c.to_string(),
            }];
        }
        return vec![];
    };

    arr.iter()
        .filter_map(|item| {
            let content = item
                .get("content")
                .or_else(|| item.get("text"))
                .and_then(Value::as_str)?;
            if content.is_empty() {
                return None;
            }
            let memory_id = item
                .get("memory_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .map(String::from);
            Some(BoostSearchHit {
                memory_id,
                content: content.to_string(),
            })
        })
        .collect()
}

impl ToolExecutor {
    pub(super) async fn memoria_call(&self, op: &str, args: &Value) -> String {
        self.memoria_call_with_timeout(op, args, Duration::from_secs(10))
            .await
    }

    pub(super) async fn memoria_call_with_timeout(
        &self,
        op: &str,
        args: &Value,
        timeout: Duration,
    ) -> String {
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
                    let mut pl = json!({"query": query, "top_k": top_k});
                    if let Some(mc) = args.get("min_confidence").and_then(Value::as_f64) {
                        pl["min_confidence"] = json!(mc);
                    }
                    (
                        format!("{base}/v1/memories/retrieve"),
                        pl,
                    )
                }
                "store" => {
                    let content = args.get("content").and_then(Value::as_str).unwrap_or("");
                    let memory_type = args
                        .get("memory_type")
                        .and_then(Value::as_str)
                        .unwrap_or("semantic");
                    let mut payload = json!({"content": content, "memory_type": memory_type});
                    // Forward trust_tier and session_id when provided by the LLM
                    if let Some(tier) = args.get("trust_tier").and_then(Value::as_str) {
                        payload["trust_tier"] = json!(tier);
                    }
                    if let Some(sid) = args.get("session_id").and_then(Value::as_str) {
                        payload["session_id"] = json!(sid);
                    }
                    (format!("{base}/v1/memories"), payload)
                }
                "search" => {
                    let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                    let top_k = args.get("top_k").and_then(Value::as_u64).unwrap_or(10);
                    let mut pl = json!({"query": query, "top_k": top_k});
                    if let Some(mc) = args.get("min_confidence").and_then(Value::as_f64) {
                        pl["min_confidence"] = json!(mc);
                    }
                    (
                        format!("{base}/v1/memories/search"),
                        pl,
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

    pub async fn memory_boost_search(&self, query: &str, top_k: u64) -> Vec<BoostSearchHit> {
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
            .json(&json!({
                "query": query,
                "top_k": top_k,
                "min_confidence": 0.3
            }))
            .send()
            .await
        {
            Ok(resp) => {
                self.memoria_fail_count
                    .store(0, std::sync::atomic::Ordering::Relaxed);
                let text = resp.text().await.unwrap_or_default();
                parse_memory_search_hits(&text)
            }
            Err(_) => {
                self.memoria_fail_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                vec![]
            }
        }
    }

    /// Fire-and-forget: send "useful" feedback for retrieved memory IDs.
    ///
    /// Called after boost search results are injected into the prompt.
    /// Spawns a background task — does not block the caller.
    pub fn memory_feedback_useful(&self, memory_ids: Vec<String>) {
        if memory_ids.is_empty() {
            return;
        }
        if self
            .memoria_fail_count
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 2
        {
            return;
        }
        let base = std::env::var("MEMORIA_BASE_URL")
            .unwrap_or_else(|_| astra_core::config::DEFAULT_MEMORIA_URL.to_string());
        let key = match std::env::var("MEMORIA_API_KEY")
            .ok()
            .or_else(|| std::env::var("MEMORIA_MASTER_KEY").ok())
        {
            Some(k) => k,
            None => return,
        };
        tokio::spawn(async move {
            let client = match reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .no_proxy()
                .build()
            {
                Ok(c) => c,
                Err(_) => return,
            };
            let url = format!("{base}/v1/memories/feedback");
            for mid in memory_ids {
                let _ = client
                    .post(&url)
                    .header("Authorization", format!("Bearer {key}"))
                    .json(&json!({
                        "memory_id": mid,
                        "signal": "useful",
                        "context": "boost_search retrieval"
                    }))
                    .send()
                    .await;
            }
        });
    }
}

/// Build a one-shot Memoria HTTP client + auth header. Returns None if no API key.
fn memoria_oneshot_client(timeout_secs: u64) -> Option<(reqwest::Client, String, String)> {
    let base = std::env::var("MEMORIA_BASE_URL")
        .unwrap_or_else(|_| astra_core::config::DEFAULT_MEMORIA_URL.to_string());
    let key = std::env::var("MEMORIA_API_KEY")
        .ok()
        .or_else(|| std::env::var("MEMORIA_MASTER_KEY").ok())?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .no_proxy()
        .build()
        .ok()?;
    Some((client, base, key))
}

/// Fire-and-forget: trigger Memoria governance (quarantine low-confidence,
/// clean stale data). Called at session end. Server has 1-hour cooldown.
pub async fn memoria_governance_fire_and_forget() {
    let Some((client, base, key)) = memoria_oneshot_client(10) else { return };
    let _ = client
        .post(format!("{base}/v1/memories/governance"))
        .header("Authorization", format!("Bearer {key}"))
        .json(&json!({"force": false}))
        .send()
        .await;
}

/// Fire-and-forget: trigger Memoria graph consolidation (merge duplicates,
/// detect contradictions, fix orphaned nodes, promote trust tiers).
/// Called at session end. Server has 30-minute cooldown.
pub async fn memoria_consolidate_fire_and_forget() {
    let Some((client, base, key)) = memoria_oneshot_client(15) else { return };
    let _ = client
        .post(format!("{base}/v1/memories/consolidate"))
        .header("Authorization", format!("Bearer {key}"))
        .json(&json!({"force": false}))
        .send()
        .await;
}
