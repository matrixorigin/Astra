//! Memoria (memory service) integration for tool execution.
//!
//! Provides HTTP client for storing, retrieving, and managing memories
//! via the Memoria API, with circuit breaker for resilience.

use std::time::Duration;

use serde_json::{Value, json};

use super::ToolExecutor;

pub use astra_tools::memoria::{
    BoostSearchHit,
    memoria_branch_checkout,
    memoria_branch_create,
    memoria_branch_diff,
    memoria_branch_merge,
    memoria_branches_list,
    memoria_health,
    memoria_oneshot_client,
    memoria_reflect,
    // Cloud helpers — single source of truth in astra-tools
    memoria_snapshot_create,
    memoria_snapshot_diff,
    memoria_snapshot_rollback,
    memoria_snapshots_list,
    parse_memory_search_hits,
};

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
        // CLI-specific circuit breaker with user notification.
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

        // Delegate to the shared MemoriaClient (single source of truth for
        // build_direct_request, type normalization, and HTTP method routing).
        let cloud_token = self.cloud_token();
        let client = astra_tools::memoria::MemoriaClient::new(self.cloud_base.clone(), cloud_token);
        let result = client.call_with_timeout(op, args, timeout).await;

        // CLI-specific: update circuit breaker + user notification
        if result.contains("\"error\"") {
            self.memoria_fail_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        } else {
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
        }
        result
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
            for mid in memory_ids {
                let url = format!("{base}/v1/memories/{mid}/feedback");
                if let Err(e) = client
                    .post(&url)
                    .header("Authorization", format!("Bearer {key}"))
                    .json(&json!({
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

// HttpMethod + build_direct_request moved to astra_tools::memoria::MemoriaClient
// (single source of truth for CLI and server).
use astra_tools::memoria::HttpMethod;

fn build_direct_request(base: &str, op: &str, args: &Value) -> (String, Value, HttpMethod) {
    astra_tools::memoria::MemoriaClient::build_direct_request(base, op, args)
}

// Old build_direct_request body (120 lines) removed.

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
        let (endpoint, pl, _) = build_direct_request("http://mem", "retrieve", &args);
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
        let (endpoint, pl, _) = build_direct_request("http://mem", "retrieve", &args);
        assert_eq!(endpoint, "http://mem/v1/memories/retrieve");
        assert_eq!(pl["filter_session"], true);
        assert_eq!(pl["include_cross_session"], false);
    }

    #[test]
    fn retrieve_omits_filter_and_include_when_absent() {
        let args = json!({"query": "test", "top_k": 5});
        let (_, pl, _) = build_direct_request("http://mem", "retrieve", &args);
        assert!(pl.get("filter_session").is_none());
        assert!(pl.get("include_cross_session").is_none());
    }

    #[test]
    fn search_routes_to_retrieve_endpoint() {
        let args = json!({"query": "test", "top_k": 10});
        let (endpoint, _, _) = build_direct_request("http://mem", "search", &args);
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
        let (_, pl, _) = build_direct_request("http://mem", "search", &args);
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
        let (_, pl, _) = build_direct_request("http://mem", "search", &args);
        assert_eq!(pl["include_cross_session"], false);
    }

    #[test]
    fn search_omits_session_fields_when_absent() {
        let args = json!({"query": "test", "top_k": 10});
        let (_, pl, _) = build_direct_request("http://mem", "search", &args);
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
        let (endpoint, pl, _) = build_direct_request("http://mem", "store", &args);
        assert_eq!(endpoint, "http://mem/v1/memories");
        assert_eq!(pl["session_id"], "sess-42");
        assert_eq!(pl["trust_tier"], "T1");
    }

    #[test]
    fn store_maps_business_type_to_memoria_primitive() {
        let args = json!({"content": "preference", "memory_type": "feedback"});
        let (_, pl, _) = build_direct_request("http://mem", "store", &args);
        assert_eq!(
            pl["memory_type"], "semantic",
            "business type 'feedback' must map to 'semantic'"
        );
    }

    #[test]
    fn store_passes_through_valid_memoria_types() {
        let args = json!({"content": "test", "memory_type": "profile"});
        let (_, pl, _) = build_direct_request("http://mem", "store", &args);
        assert_eq!(pl["memory_type"], "profile");
    }

    // ── Cloud helpers now in astra-tools (single source of truth) ──

    #[test]
    fn cloud_helpers_are_re_exported_from_shared() {
        // These re-exports prove the shared module exposes all cloud helpers.
        // If any is removed from astra-tools, this file won't compile.
        let _ = memoria_snapshot_create;
        let _ = memoria_branch_create;
        let _ = memoria_reflect;
        let _ = memoria_health;
    }
}

/// Public accessor for session_cleanup's working-memory purge.
pub fn memoria_oneshot_client_pub(timeout_secs: u64) -> Option<(reqwest::Client, String, String)> {
    astra_tools::memoria::memoria_oneshot_client(timeout_secs)
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
    // Memoria /v1/memories/retrieve returns a direct array when explain
    // is off, or {"results": [...]} when explain is on. Handle both.
    let memories = if let Some(arr) = value.as_array() {
        arr
    } else if let Some(arr) = value.get("memories").and_then(|v| v.as_array()) {
        arr
    } else if let Some(arr) = value.get("results").and_then(|v| v.as_array()) {
        arr
    } else {
        return Vec::new();
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
            let action = astra_services::sanitize_for_prompt(content);
            let compact = if action.len() > 80 {
                action
                    .split_once(['.', '—', ';'])
                    .map(|(s, _)| s.trim().to_string())
            } else {
                None
            };
            Some(astra_runtime::self_model::LessonHint {
                kind,
                trigger_signal: "memoria".into(),
                action,
                compact,
                workload_tag: None,
            })
        })
        .collect()
}

pub use astra_tools::memoria::{
    memoria_consolidate_fire_and_forget, memoria_governance_fire_and_forget,
};

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
                "source": {"agent": "session_end"},
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

// Cloud memory helpers (snapshot, branch, reflect, health) now live in
// astra_tools::memoria — re-exported at the top of this file.
