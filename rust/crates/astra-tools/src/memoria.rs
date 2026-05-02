//! Memoria (memory service) HTTP client for tool execution.
//!
//! Provides HTTP client for storing, retrieving, and managing memories
//! via the Memoria API, with circuit breaker for resilience.
//!
//! This module is shared between CLI and server — both use HTTP proxy
//! calls to the Memoria service.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

/// A single memory hit from search, carrying both ID and content.
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
pub fn parse_memory_search_contents(raw: &str) -> Vec<String> {
    parse_memory_search_hits(raw)
        .into_iter()
        .map(|h| h.content)
        .collect()
}

/// Parse memory hits (ID + content) from a Memoria search/retrieve response.
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

/// Memoria HTTP client with circuit breaker.
///
/// Used by both CLI (via ToolExecutor) and server (via ServerToolExecutor)
/// to proxy memory operations to the Memoria service.
pub struct MemoriaClient {
    /// Cloud API base URL for proxied calls.
    pub cloud_base: Option<String>,
    /// Auth token for cloud proxy calls.
    pub cloud_token: Option<String>,
    /// Circuit breaker: skip after consecutive failures.
    fail_count: AtomicU32,
}

const MAX_FAILS: u32 = 2;

impl MemoriaClient {
    pub fn new(cloud_base: Option<String>, cloud_token: Option<String>) -> Self {
        Self {
            cloud_base,
            cloud_token,
            fail_count: AtomicU32::new(0),
        }
    }

    /// Check if the circuit breaker is open (too many consecutive failures).
    pub fn is_circuit_open(&self) -> bool {
        self.fail_count.load(Ordering::Relaxed) >= MAX_FAILS
    }

    /// Execute a memoria operation (store, retrieve, search, purge, correct, profile).
    pub async fn call(&self, op: &str, args: &Value) -> String {
        self.call_with_timeout(op, args, Duration::from_secs(10))
            .await
    }

    /// Execute a memoria operation with custom timeout.
    pub async fn call_with_timeout(&self, op: &str, args: &Value, timeout: Duration) -> String {
        if self.is_circuit_open() {
            return json!({"error": "Memory service unavailable (circuit open)"}).to_string();
        }

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
            let (ep, pl) = Self::build_direct_request(&base, op, args);
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
                        self.fail_count.store(0, Ordering::Relaxed);
                        text
                    }
                    Err(e) => {
                        self.fail_count.fetch_add(1, Ordering::Relaxed);
                        json!({"error": format!("read response: {e}")}).to_string()
                    }
                },
                Err(e) => {
                    self.fail_count.fetch_add(1, Ordering::Relaxed);
                    json!({"error": format!("memoria request failed: {e}")}).to_string()
                }
            },
            Err(e) => json!({"error": format!("build client: {e}")}).to_string(),
        }
    }

    /// Boost search: best-effort memory lookup on the critical path.
    pub async fn boost_search(&self, query: &str, top_k: u64) -> Vec<BoostSearchHit> {
        if query.trim().is_empty() || self.is_circuit_open() {
            return vec![];
        }
        let base = std::env::var("MEMORIA_BASE_URL")
            .unwrap_or_else(|_| astra_core::config::DEFAULT_MEMORIA_URL.to_string());
        let key = match std::env::var("MEMORIA_MASTER_KEY").ok() {
            Some(k) => k,
            None => return vec![],
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
                self.fail_count.store(0, Ordering::Relaxed);
                let text = resp.text().await.unwrap_or_default();
                parse_memory_search_hits(&text)
            }
            Err(_) => {
                self.fail_count.fetch_add(1, Ordering::Relaxed);
                vec![]
            }
        }
    }
    fn build_direct_request(base: &str, op: &str, args: &Value) -> (String, Value) {
        // Helper: propagate session_id and user_id when present in args
        // so Memoria can scope operations to the correct user.
        let inject_identity = |pl: &mut Value| {
            if let Some(obj) = pl.as_object_mut() {
                if let Some(sid) = args.get("session_id").and_then(Value::as_str) {
                    obj.insert("session_id".to_string(), json!(sid));
                }
                if let Some(uid) = args.get("user_id").and_then(Value::as_str) {
                    obj.insert("user_id".to_string(), json!(uid));
                }
            }
        };
        match op {
            "retrieve" => {
                let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                let top_k = args.get("top_k").and_then(Value::as_u64).unwrap_or(5);
                let mut pl = json!({"query": query, "top_k": top_k});
                if let Some(mc) = args.get("min_confidence").and_then(Value::as_f64) {
                    pl["min_confidence"] = json!(mc);
                }
                inject_identity(&mut pl);
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
                let raw_type = args
                    .get("memory_type")
                    .and_then(Value::as_str)
                    .unwrap_or("semantic");
                let memory_type = map_business_type_to_memoria(raw_type);
                let mut payload = json!({"content": content, "memory_type": memory_type});
                if let Some(tier) = args.get("trust_tier").and_then(Value::as_str) {
                    payload["trust_tier"] = json!(tier);
                }
                inject_identity(&mut payload);
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
                inject_identity(&mut pl);
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
                let mut pl = json!({"topic": topic, "reason": reason});
                inject_identity(&mut pl);
                (format!("{base}/v1/memories/purge"), pl)
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
                let mut pl =
                    json!({"memory_id": memory_id, "new_content": new_content, "reason": reason});
                inject_identity(&mut pl);
                (format!("{base}/v1/memories/correct"), pl)
            }
            "profile" => {
                let mut pl = json!({});
                inject_identity(&mut pl);
                (format!("{base}/v1/memories/profile"), pl)
            }
            _ => (
                String::new(),
                json!({"error": format!("Unknown memoria op: {op}")}),
            ),
        }
    }
}

/// Map business category types to Memoria V1 primitives.
/// Delegates to the single source of truth in astra-prompts.
fn map_business_type_to_memoria(raw: &str) -> &str {
    astra_prompts::memory_types::normalize_memoria_type(raw)
}

/// Build a one-shot Memoria HTTP client + auth header.
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

/// Fire-and-forget: trigger Memoria governance.
pub async fn memoria_governance_fire_and_forget() {
    let Some((client, base, key)) = memoria_oneshot_client(10) else {
        return;
    };
    let _ = client
        .post(format!("{base}/v1/memories/governance"))
        .header("Authorization", format!("Bearer {key}"))
        .json(&json!({"force": false}))
        .send()
        .await;
}

/// Fire-and-forget: trigger Memoria graph consolidation.
pub async fn memoria_consolidate_fire_and_forget() {
    let Some((client, base, key)) = memoria_oneshot_client(15) else {
        return;
    };
    let _ = client
        .post(format!("{base}/v1/memories/consolidate"))
        .header("Authorization", format!("Bearer {key}"))
        .json(&json!({"force": false}))
        .send()
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn map_business_types_to_memoria_primitives() {
        assert_eq!(map_business_type_to_memoria("user"), "profile");
        assert_eq!(map_business_type_to_memoria("feedback"), "semantic");
        assert_eq!(map_business_type_to_memoria("project"), "semantic");
        assert_eq!(map_business_type_to_memoria("lesson"), "semantic");
        assert_eq!(map_business_type_to_memoria("ref"), "procedural");
        assert_eq!(map_business_type_to_memoria("reference"), "procedural");
        assert_eq!(map_business_type_to_memoria("episode"), "episodic");
        // V1 primitives pass through unchanged
        assert_eq!(map_business_type_to_memoria("semantic"), "semantic");
        assert_eq!(map_business_type_to_memoria("profile"), "profile");
        assert_eq!(map_business_type_to_memoria("working"), "working");
    }

    #[test]
    fn store_maps_business_type_before_sending() {
        let args = json!({"content": "test", "memory_type": "feedback"});
        let (_, pl) = MemoriaClient::build_direct_request("http://mem", "store", &args);
        assert_eq!(
            pl["memory_type"], "semantic",
            "business type 'feedback' must be mapped to 'semantic' for Memoria V1"
        );
    }

    #[test]
    fn build_direct_request_propagates_session_and_user_id() {
        let args = json!({
            "query": "rust patterns",
            "top_k": 3,
            "session_id": "user-42",
            "user_id": "user-42"
        });

        // retrieve
        let (_, pl) = MemoriaClient::build_direct_request("http://mem", "retrieve", &args);
        assert_eq!(pl["session_id"], "user-42");
        assert_eq!(pl["user_id"], "user-42");
        assert_eq!(pl["query"], "rust patterns");
        assert!(
            pl.get("min_confidence").is_none(),
            "min_confidence should only be sent when explicitly provided"
        );

        // search
        let (_, pl) = MemoriaClient::build_direct_request("http://mem", "search", &args);
        assert_eq!(pl["session_id"], "user-42");
        assert_eq!(pl["user_id"], "user-42");

        // store
        let store_args = json!({
            "content": "hello",
            "session_id": "user-42",
            "user_id": "user-42"
        });
        let (_, pl) = MemoriaClient::build_direct_request("http://mem", "store", &store_args);
        assert_eq!(pl["session_id"], "user-42");
        assert_eq!(pl["user_id"], "user-42");

        // purge
        let purge_args = json!({
            "topic": "old",
            "session_id": "user-42",
            "user_id": "user-42"
        });
        let (_, pl) = MemoriaClient::build_direct_request("http://mem", "purge", &purge_args);
        assert_eq!(pl["session_id"], "user-42");
        assert_eq!(pl["user_id"], "user-42");

        // correct
        let correct_args = json!({
            "memory_id": "m1",
            "new_content": "fixed",
            "session_id": "user-42",
            "user_id": "user-42"
        });
        let (_, pl) = MemoriaClient::build_direct_request("http://mem", "correct", &correct_args);
        assert_eq!(pl["session_id"], "user-42");
        assert_eq!(pl["user_id"], "user-42");

        // profile
        let profile_args = json!({"session_id": "user-42", "user_id": "user-42"});
        let (_, pl) = MemoriaClient::build_direct_request("http://mem", "profile", &profile_args);
        assert_eq!(pl["session_id"], "user-42");
        assert_eq!(pl["user_id"], "user-42");
    }

    #[test]
    fn build_direct_request_omits_identity_when_absent() {
        let args = json!({"query": "test"});
        let (_, pl) = MemoriaClient::build_direct_request("http://mem", "retrieve", &args);
        assert!(pl.get("session_id").is_none());
        assert!(pl.get("user_id").is_none());
        assert!(
            pl.get("min_confidence").is_none(),
            "min_confidence omitted when not provided"
        );
    }

    #[test]
    fn build_direct_request_retrieve_respects_explicit_min_confidence() {
        let args = json!({"query": "q", "min_confidence": 0.7});
        let (_, pl) = MemoriaClient::build_direct_request("http://mem", "retrieve", &args);
        assert_eq!(pl["min_confidence"], json!(0.7));
    }

    // ── Session isolation: retrieve forwards filter_session & include_cross_session ──

    #[test]
    fn retrieve_forwards_filter_session_and_include_cross_session() {
        let args = json!({
            "query": "test query",
            "top_k": 5,
            "filter_session": true,
            "include_cross_session": false,
        });
        let (endpoint, pl) = MemoriaClient::build_direct_request("http://mem", "retrieve", &args);
        assert_eq!(endpoint, "http://mem/v1/memories/retrieve");
        assert_eq!(pl["filter_session"], true);
        assert_eq!(pl["include_cross_session"], false);
    }

    #[test]
    fn retrieve_omits_filter_and_include_when_absent() {
        let args = json!({"query": "test", "top_k": 5});
        let (_, pl) = MemoriaClient::build_direct_request("http://mem", "retrieve", &args);
        assert!(pl.get("filter_session").is_none());
        assert!(pl.get("include_cross_session").is_none());
    }

    // ── Session isolation: search routes to /retrieve, forwards session fields ──

    #[test]
    fn search_routes_to_retrieve_endpoint() {
        let args = json!({"query": "test", "top_k": 10});
        let (endpoint, _) = MemoriaClient::build_direct_request("http://mem", "search", &args);
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
        let (_, pl) = MemoriaClient::build_direct_request("http://mem", "search", &args);
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
        let (_, pl) = MemoriaClient::build_direct_request("http://mem", "search", &args);
        assert_eq!(pl["include_cross_session"], false);
    }

    #[test]
    fn search_omits_session_fields_when_absent() {
        let args = json!({"query": "test", "top_k": 10});
        let (_, pl) = MemoriaClient::build_direct_request("http://mem", "search", &args);
        assert!(pl.get("session_id").is_none());
        assert!(pl.get("filter_session").is_none());
        assert!(pl.get("include_cross_session").is_none());
    }
}
