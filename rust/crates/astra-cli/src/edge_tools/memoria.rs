//! Memoria (memory service) integration for tool execution.
//!
//! Provides HTTP client for storing, retrieving, and managing memories
//! via the Memoria API, with circuit breaker for resilience.

use std::time::Duration;

use serde_json::{Value, json};

use super::ToolExecutor;

pub use astra_tools::memoria::{BoostSearchHit, parse_memory_search_hits};

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
            if !self
                .memoria_notified_down
                .swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                eprintln!(
                    "  {} Memoria memory service is unreachable — memory features \
                     disabled for this session. Check MEMORIA_BASE_URL or /info.",
                    crossterm::style::Stylize::yellow("⚠"),
                );
            }
            return json!({"error": "Memory service unavailable (circuit open)"}).to_string();
        }

        // Build endpoint and payload
        let cloud_token = self.cloud_token();
        let (endpoint, payload, auth_header) = if let (Some(cloud_base), Some(token)) =
            (&self.cloud_base, cloud_token.as_deref())
        {
            (
                format!("{cloud_base}/memory/{op}"),
                args.clone(),
                format!("Bearer {token}"),
            )
        } else {
            let base = std::env::var("MEMORIA_BASE_URL")
                .unwrap_or_else(|_| astra_core::config::DEFAULT_MEMORIA_URL.to_string());
            let key = match std::env::var("MEMORIA_MASTER_KEY").ok() {
                Some(k) => k,
                None => {
                    return json!({
                            "error": "Memory unavailable: not connected to cloud and MEMORIA_MASTER_KEY not set",
                            "hint": "Login with /login to enable cloud-backed memory with user isolation"
                        })
                        .to_string();
                }
            };

            let (ep, pl) = build_direct_request(&base, op, args);
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
                        if self
                            .memoria_notified_down
                            .swap(false, std::sync::atomic::Ordering::Relaxed)
                        {
                            eprintln!(
                                "  {} Memoria memory service reconnected.",
                                crossterm::style::Stylize::green("✓"),
                            );
                        }
                        text
                    }
                    Err(e) => {
                        self.memoria_fail_count
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        json!({"error": format!("read response: {e}")}).to_string()
                    }
                },
                Err(e) => {
                    let prev = self
                        .memoria_fail_count
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if prev + 1 >= MAX_FAILS
                        && !self
                            .memoria_notified_down
                            .swap(true, std::sync::atomic::Ordering::Relaxed)
                    {
                        eprintln!(
                            "  {} Memoria memory service is unreachable — memory features \
                             disabled for this session. Check MEMORIA_BASE_URL or /info.",
                            crossterm::style::Stylize::yellow("⚠"),
                        );
                    }
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
        let key = match std::env::var("MEMORIA_MASTER_KEY").ok() {
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
            .post(format!("{base}/v1/memories/retrieve"))
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
        let key = match std::env::var("MEMORIA_MASTER_KEY").ok() {
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
                if let Err(e) = client
                    .post(&url)
                    .header("Authorization", format!("Bearer {key}"))
                    .json(&json!({
                        "memory_id": mid,
                        "signal": "useful",
                        "context": "boost_search retrieval"
                    }))
                    .send()
                    .await
                {
                    eprintln!("[memoria] feedback for {mid} failed: {e}");
                    break; // don't spam on persistent failures
                }
            }
        });
    }
}

/// Build endpoint URL and JSON payload for a direct Memoria API call.
///
/// Extracted from `memoria_call_with_timeout` so it can be unit-tested
/// without requiring a live Memoria server.
fn build_direct_request(base: &str, op: &str, args: &Value) -> (String, Value) {
    match op {
        "retrieve" => {
            let query = args.get("query").and_then(Value::as_str).unwrap_or("");
            let top_k = args.get("top_k").and_then(Value::as_u64).unwrap_or(5);
            let mut pl = json!({"query": query, "top_k": top_k});
            if let Some(mc) = args.get("min_confidence").and_then(Value::as_f64) {
                pl["min_confidence"] = json!(mc);
            }
            if let Some(sid) = args.get("session_id").and_then(Value::as_str) {
                pl["session_id"] = json!(sid);
            }
            // Forward filter_session and include_cross_session for session-scoped retrieval
            if let Some(fs) = args.get("filter_session").and_then(Value::as_bool) {
                pl["filter_session"] = json!(fs);
            }
            if let Some(ics) = args.get("include_cross_session").and_then(Value::as_bool) {
                pl["include_cross_session"] = json!(ics);
            }
            (format!("{base}/v1/memories/retrieve"), pl)
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
            // Route to /v1/memories/retrieve (not /search) so session_id and
            // filter_session are honoured by the Memoria API.
            let query = args.get("query").and_then(Value::as_str).unwrap_or("");
            let top_k = args.get("top_k").and_then(Value::as_u64).unwrap_or(10);
            let mut pl = json!({"query": query, "top_k": top_k});
            if let Some(mc) = args.get("min_confidence").and_then(Value::as_f64) {
                pl["min_confidence"] = json!(mc);
            }
            if let Some(sid) = args.get("session_id").and_then(Value::as_str) {
                pl["session_id"] = json!(sid);
            }
            if let Some(fs) = args.get("filter_session").and_then(Value::as_bool) {
                pl["filter_session"] = json!(fs);
            }
            if let Some(ics) = args.get("include_cross_session").and_then(Value::as_bool) {
                pl["include_cross_session"] = json!(ics);
            }
            (format!("{base}/v1/memories/retrieve"), pl)
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
        _ => (
            String::new(),
            json!({"error": format!("Unknown memoria op: {op}")}),
        ),
    }
}

#[cfg(test)]
mod build_direct_request_tests {
    use super::*;

    #[test]
    fn retrieve_forwards_session_id() {
        let args = json!({
            "query": "test query",
            "top_k": 5,
            "session_id": "sess-123",
        });
        let (endpoint, pl) = build_direct_request("http://mem", "retrieve", &args);
        assert_eq!(endpoint, "http://mem/v1/memories/retrieve");
        assert_eq!(pl["session_id"], "sess-123");
        assert_eq!(pl["query"], "test query");
    }

    #[test]
    fn retrieve_forwards_filter_session_and_include_cross_session() {
        let args = json!({
            "query": "test query",
            "top_k": 5,
            "filter_session": true,
            "include_cross_session": false,
        });
        let (endpoint, pl) = build_direct_request("http://mem", "retrieve", &args);
        assert_eq!(endpoint, "http://mem/v1/memories/retrieve");
        assert_eq!(pl["filter_session"], true);
        assert_eq!(pl["include_cross_session"], false);
    }

    #[test]
    fn retrieve_omits_filter_and_include_when_absent() {
        let args = json!({"query": "test", "top_k": 5});
        let (_, pl) = build_direct_request("http://mem", "retrieve", &args);
        assert!(pl.get("filter_session").is_none());
        assert!(pl.get("include_cross_session").is_none());
    }

    #[test]
    fn search_routes_to_retrieve_endpoint() {
        let args = json!({"query": "test", "top_k": 10});
        let (endpoint, _) = build_direct_request("http://mem", "search", &args);
        assert_eq!(
            endpoint, "http://mem/v1/memories/retrieve",
            "search must route to /retrieve (not /search) for session_id support"
        );
    }

    #[test]
    fn search_forwards_session_id_and_filter_session() {
        let args = json!({
            "query": "test",
            "top_k": 10,
            "session_id": "sess-abc",
            "filter_session": true,
        });
        let (_, pl) = build_direct_request("http://mem", "search", &args);
        assert_eq!(pl["session_id"], "sess-abc");
        assert_eq!(pl["filter_session"], true);
    }

    #[test]
    fn search_forwards_include_cross_session() {
        let args = json!({
            "query": "test",
            "top_k": 10,
            "include_cross_session": false,
        });
        let (_, pl) = build_direct_request("http://mem", "search", &args);
        assert_eq!(pl["include_cross_session"], false);
    }

    #[test]
    fn search_omits_session_fields_when_absent() {
        let args = json!({"query": "test", "top_k": 10});
        let (_, pl) = build_direct_request("http://mem", "search", &args);
        assert!(pl.get("session_id").is_none());
        assert!(pl.get("filter_session").is_none());
        assert!(pl.get("include_cross_session").is_none());
    }

    #[test]
    fn store_forwards_session_id_and_trust_tier() {
        let args = json!({
            "content": "hello",
            "session_id": "sess-42",
            "trust_tier": "T1",
        });
        let (endpoint, pl) = build_direct_request("http://mem", "store", &args);
        assert_eq!(endpoint, "http://mem/v1/memories");
        assert_eq!(pl["session_id"], "sess-42");
        assert_eq!(pl["trust_tier"], "T1");
    }
}

/// Build a one-shot Memoria HTTP client + auth header. Returns None if no API key.
fn memoria_oneshot_client(timeout_secs: u64) -> Option<(reqwest::Client, String, String)> {
    let base = std::env::var("MEMORIA_BASE_URL")
        .unwrap_or_else(|_| astra_core::config::DEFAULT_MEMORIA_URL.to_string());
    let key = std::env::var("MEMORIA_MASTER_KEY").ok()?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .no_proxy()
        .build()
        .ok()?;
    Some((client, base, key))
}

/// Retrieve procedural/semantic lessons from Memoria for session bootstrap.
/// `context_query` should be derived from the user's first message — this
/// produces much better semantic retrieval than keyword stuffing.
/// Returns LessonHint-compatible structs. Best-effort: returns empty vec
/// on any error (circuit breaker, timeout, parse failure).
pub async fn memoria_retrieve_lessons(
    top_k: u64,
    context_query: Option<&str>,
) -> Vec<astra_runtime::self_model::LessonHint> {
    let Some((client, base, key)) = memoria_oneshot_client(3) else {
        return Vec::new();
    };
    let query = context_query.unwrap_or("reusable lessons and corrections from prior sessions");
    let payload = json!({
        "query": query,
        "top_k": top_k,
        "min_confidence": 0.3,
    });
    let resp = match client
        .post(format!("{base}/v1/memories/retrieve"))
        .header("Authorization", format!("Bearer {key}"))
        .json(&payload)
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let text = match resp.text().await {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let memories = match value.get("memories").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return Vec::new(),
    };
    memories
        .iter()
        .filter_map(|m| {
            let content = m.get("content")?.as_str()?;
            let memory_type = m.get("memory_type")?.as_str()?;
            if !matches!(memory_type, "semantic" | "procedural") {
                return None;
            }
            let kind = astra_services::LessonKind::PromptShape;
            Some(astra_runtime::self_model::LessonHint {
                kind,
                trigger_signal: "memoria".into(),
                action: astra_services::sanitize_for_prompt(content),
                workload_tag: None,
            })
        })
        .collect()
}

/// Fire-and-forget: trigger Memoria governance (quarantine low-confidence,
/// clean stale data). Called at session end. Server has 1-hour cooldown.
pub async fn memoria_governance_fire_and_forget() {
    let Some((client, base, key)) = memoria_oneshot_client(10) else {
        return;
    };
    if let Err(e) = client
        .post(format!("{base}/v1/memories/governance"))
        .header("Authorization", format!("Bearer {key}"))
        .json(&json!({"force": false}))
        .send()
        .await
    {
        eprintln!("[memoria] governance trigger failed: {e}");
    }
}

/// Fire-and-forget: trigger Memoria graph consolidation (merge duplicates,
/// detect contradictions, fix orphaned nodes, promote trust tiers).
/// Called at session end. Server has 30-minute cooldown.
pub async fn memoria_consolidate_fire_and_forget() {
    let Some((client, base, key)) = memoria_oneshot_client(15) else {
        return;
    };
    if let Err(e) = client
        .post(format!("{base}/v1/memories/consolidate"))
        .header("Authorization", format!("Bearer {key}"))
        .json(&json!({"force": false}))
        .send()
        .await
    {
        eprintln!("[memoria] consolidation trigger failed: {e}");
    }
}

/// Store extracted lessons in Memoria as L3 durable memory using the
/// batch endpoint (Session Memory Protocol §6.2). Single HTTP call for
/// up to 100 lessons. Best-effort, fire-and-forget.
pub async fn memoria_store_lessons_fire_and_forget(
    lessons: Vec<astra_runtime::lesson_synthesizer::ExtractedLesson>,
    session_id: Option<String>,
) {
    if lessons.is_empty() {
        return;
    }
    let Some((client, base, key)) = memoria_oneshot_client(5) else {
        return;
    };
    let memories: Vec<serde_json::Value> = lessons
        .iter()
        .map(|l| {
            let mut m = json!({
                "content": l.content,
                "memory_type": l.memory_type,
                "trust_tier": l.trust_tier,
            });
            if let Some(ref sid) = session_id {
                m["session_id"] = json!(sid);
            }
            m
        })
        .collect();

    if let Err(e) = client
        .post(format!("{base}/v1/memories/batch"))
        .header("Authorization", format!("Bearer {key}"))
        .json(&json!({ "memories": memories }))
        .send()
        .await
    {
        tracing::debug!(
            target: "memoria",
            count = lessons.len(),
            error = %e,
            "batch lesson store failed",
        );
    }
}
