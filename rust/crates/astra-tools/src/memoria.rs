//! Memoria (memory service) HTTP client for tool execution.
//!
//! Provides HTTP client for storing, retrieving, and managing memories
//! via the Memoria API, with circuit breaker for resilience.
//!
//! This module is shared between CLI and server — both use HTTP proxy
//! calls to the Memoria service.

use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// HTTP method for Memoria API calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Put,
    Post,
}

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

/// A single `focus` hint: a session-scoped attention boost with TTL.
///
/// Stored in-process by [`MemoriaClient`]. On each `recall` call the
/// client consults the hints whose `expires_at` is still in the future
/// and forwards them to the backend as `boost_topics` / `boost_tags`
/// hints. The hint is evicted on first access past its TTL.
#[derive(Debug, Clone)]
struct FocusHint {
    /// `"topic" | "tag" | "memory_id" | "session"` (matches v2 FocusRequest).
    focus_type: String,
    value: String,
    boost: f64,
    expires_at: Instant,
}

/// Memoria HTTP client with circuit breaker.
///
/// Used by both CLI (via ToolExecutor) and server (via ServerToolExecutor)
/// to proxy memory operations to the Memoria service.
///
/// **Cognitive verbs**: the LLM-facing surface exposes v2 cognitive verbs
/// (`remember`, `recall`, `forget`, `update`, `expand`, `focus`, `reflect`,
/// `profile`, `feedback`). Those are translated to v1 HTTP endpoints by
/// [`Self::build_direct_request`]. `focus` is handled in-process via the
/// [`FocusHint`] store; subsequent `recall`s read it and forward boost
/// hints to the backend.
pub struct MemoriaClient {
    /// Cloud API base URL for proxied calls.
    pub cloud_base: Option<String>,
    /// Auth token for cloud proxy calls.
    pub cloud_token: Option<String>,
    /// Circuit breaker: skip after consecutive failures.
    fail_count: AtomicU32,
    /// Session-scoped attention boosts. Keyed by `session_id`. Each entry
    /// is consulted on `recall` and auto-expired by `Instant`.
    focus_store: RwLock<HashMap<String, Vec<FocusHint>>>,
}

const MAX_FAILS: u32 = 2;

impl MemoriaClient {
    pub fn new(cloud_base: Option<String>, cloud_token: Option<String>) -> Self {
        Self {
            cloud_base,
            cloud_token,
            fail_count: AtomicU32::new(0),
            focus_store: RwLock::new(HashMap::new()),
        }
    }

    /// Record a `focus` hint for the given session. Returns the synthetic
    /// response the LLM sees (mirrors the v2 FocusResponse shape).
    pub fn focus_set(&self, session_id: &str, args: &Value) -> String {
        let focus_type = match args
            .get("focus_type")
            .or_else(|| args.get("type"))
            .and_then(Value::as_str)
        {
            Some(t @ ("topic" | "tag" | "memory_id" | "session")) => t.to_string(),
            _ => {
                return json!({"error":
                    "memory(action=focus) requires `focus_type` ∈ {topic,tag,memory_id,session}"})
                .to_string();
            }
        };
        let value = match args
            .get("focus_value")
            .or_else(|| args.get("value"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(v) => v.to_string(),
            None => {
                return json!({"error": "memory(action=focus) requires non-empty `focus_value`"})
                    .to_string();
            }
        };
        let boost = args.get("boost").and_then(Value::as_f64).unwrap_or(1.5);
        let ttl_secs = args
            .get("ttl_secs")
            .and_then(Value::as_i64)
            .unwrap_or(3600)
            .max(1) as u64;
        let expires_at = Instant::now() + Duration::from_secs(ttl_secs);
        let hint = FocusHint {
            focus_type: focus_type.clone(),
            value: value.clone(),
            boost,
            expires_at,
        };
        if let Ok(mut store) = self.focus_store.write() {
            let sid_key = if session_id.is_empty() {
                "_global".to_string()
            } else {
                session_id.to_string()
            };
            let bucket = store.entry(sid_key).or_default();
            // Evict any existing hint with the same (type, value) so the
            // newest boost/ttl wins.
            bucket.retain(|h| !(h.focus_type == focus_type && h.value == value));
            bucket.push(hint);
        }
        json!({
            "status": "ok",
            "focus_type": focus_type,
            "value": value,
            "boost": boost,
            "active_for_secs": ttl_secs,
        })
        .to_string()
    }

    /// Return active focus hints for a session. Expired entries are
    /// evicted as a side effect.
    fn focus_active(&self, session_id: &str) -> Vec<FocusHint> {
        let now = Instant::now();
        let sid_key = if session_id.is_empty() {
            "_global".to_string()
        } else {
            session_id.to_string()
        };
        if let Ok(mut store) = self.focus_store.write()
            && let Some(bucket) = store.get_mut(&sid_key)
        {
            bucket.retain(|h| h.expires_at > now);
            return bucket.clone();
        }
        Vec::new()
    }

    /// Inject focus hints into a `recall` payload. Called by the
    /// `call_with_timeout` path right before the HTTP send when `op ==
    /// "recall"`.
    fn apply_focus_hints(&self, session_id: &str, payload: &mut Value) {
        let hints = self.focus_active(session_id);
        if hints.is_empty() {
            return;
        }
        let Some(obj) = payload.as_object_mut() else {
            return;
        };
        let mut topics: Vec<Value> = Vec::new();
        let mut tags: Vec<Value> = Vec::new();
        let mut memory_ids: Vec<Value> = Vec::new();
        for h in hints {
            let entry = json!({ "value": h.value, "boost": h.boost });
            match h.focus_type.as_str() {
                "topic" => topics.push(entry),
                "tag" => tags.push(entry),
                "memory_id" => memory_ids.push(entry),
                _ => {}
            }
        }
        if !topics.is_empty() {
            obj.insert("boost_topics".into(), Value::Array(topics));
        }
        if !tags.is_empty() {
            obj.insert("boost_tags".into(), Value::Array(tags));
        }
        if !memory_ids.is_empty() {
            obj.insert("boost_memory_ids".into(), Value::Array(memory_ids));
        }
    }

    /// Builds a tool result that confirms purge success to the agent.
    /// Use this instead of returning the raw Memoria `{}` response.
    pub fn purge_result_to_agent_response(raw: &Value, filter: &str) -> Value {
        let deleted = raw
            .get("deleted_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        json!({
            "status": "ok",
            "deleted_count": deleted,
            "message": Self::purge_success_message(deleted, filter),
        })
    }

    /// Returns a human-readable success message for a purge response.
    pub fn purge_success_message(deleted_count: u64, filter: &str) -> String {
        if deleted_count == 0 {
            format!("memory_purge: no entries matched filter [{filter}] (0 deleted)")
        } else {
            format!("memory_purge: deleted {deleted_count} entries matching [{filter}]")
        }
    }

    /// Check if the circuit breaker is open (too many consecutive failures).
    pub fn is_circuit_open(&self) -> bool {
        self.fail_count.load(Ordering::Relaxed) >= MAX_FAILS
    }

    /// Whitelist of agent_type values the client will forward to the
    /// Memoria backend. Kept in sync with the `agent_type` enum in the
    /// `memory` tool schema (astra-cli edge_tools). Unknown values are
    /// dropped client-side so that untrusted tool-call arguments from
    /// an LLM cannot smuggle arbitrary strings (`"../admin"`,
    /// `"' OR 1=1--"`, etc.) into request bodies even if the backend
    /// filter is misconfigured.
    const AGENT_TYPE_ALLOWLIST: &'static [&'static str] =
        &["explore", "code-review", "task", "general-purpose"];

    /// Pull an `agent_type` value out of tool args and apply the
    /// client-side allowlist. Returns `None` if the value is missing,
    /// empty, or not in [`AGENT_TYPE_ALLOWLIST`].
    fn sanitized_agent_type(args: &Value) -> Option<&str> {
        let v = args.get("agent_type").and_then(Value::as_str)?;
        let trimmed = v.trim();
        if trimmed.is_empty() {
            return None;
        }
        if Self::AGENT_TYPE_ALLOWLIST.contains(&trimmed) {
            Some(trimmed)
        } else {
            None
        }
    }

    /// Execute a memoria operation (store, retrieve, search, purge, correct, profile).
    pub async fn call(&self, op: &str, args: &Value) -> String {
        self.call_with_timeout(op, args, Duration::from_secs(10))
            .await
    }

    /// Execute a memoria operation with custom timeout.
    pub async fn call_with_timeout(&self, op: &str, args: &Value, timeout: Duration) -> String {
        // `focus` is handled entirely in-process; no HTTP call.
        if op == "focus" {
            let sid = args.get("session_id").and_then(Value::as_str).unwrap_or("");
            return self.focus_set(sid, args);
        }

        if self.is_circuit_open() {
            return json!({"error": "Memory service unavailable (circuit open)"}).to_string();
        }

        // The v2→v1 translation — including business-category expansion
        // into (content-prefix + trust_tier + tag) — now happens inside
        // `build_direct_request` for the `remember` branch. No
        // pre-normalization needed here.

        let (endpoint, mut payload, auth_header, method) = if let (Some(cloud_base), Some(token)) =
            (&self.cloud_base, &self.cloud_token)
        {
            // Cloud proxy: route v2 verbs via a dedicated `/memory/v2/:op`
            // namespace so the server-side executor can translate to v1
            // using its own credentials. If the cloud doesn't implement
            // v2 yet, the fall-through direct path still works via the
            // agent's own MEMORIA_MASTER_KEY.
            (
                format!("{cloud_base}/memory/v2/{op}"),
                args.clone(),
                format!("Bearer {token}"),
                HttpMethod::Post,
            )
        } else {
            let mem = astra_core::MemoriaSettings::from_env();
            let key = match mem.master_key {
                Some(k) => k,
                None => {
                    return json!({
                            "error": "Memory unavailable: not connected to cloud and MEMORIA_MASTER_KEY not set",
                            "hint": "Login with /login to enable cloud-backed memory with user isolation"
                        })
                        .to_string();
                }
            };
            let (ep, pl, m) = Self::build_direct_request(&mem.base_url, op, args);
            if ep.is_empty() {
                // Validation errors (and anything else the mapper decides
                // to short-circuit) return the payload verbatim.
                return pl.to_string();
            }
            (ep, pl, format!("Bearer {key}"), m)
        };

        // For `recall`, layer in session-scoped focus boosts. The backend
        // is free to ignore fields it doesn't understand; they become
        // active once Memoria v2 lands.
        if op == "recall" {
            let sid = args.get("session_id").and_then(Value::as_str).unwrap_or("");
            self.apply_focus_hints(sid, &mut payload);
        }

        match reqwest::Client::builder()
            .timeout(timeout)
            .no_proxy()
            .build()
        {
            Ok(client) => {
                let req = match method {
                    HttpMethod::Get => client.get(&endpoint),
                    HttpMethod::Put => client.put(&endpoint),
                    HttpMethod::Post => client.post(&endpoint),
                };
                match req
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
                }
            }
            Err(e) => json!({"error": format!("build client: {e}")}).to_string(),
        }
    }

    /// Boost search: best-effort memory lookup on the critical path.
    pub async fn boost_search(&self, query: &str, top_k: u64) -> Vec<BoostSearchHit> {
        if query.trim().is_empty() || self.is_circuit_open() {
            return vec![];
        }
        let mem = astra_core::MemoriaSettings::from_env();
        let token = match mem.bearer_token() {
            Some(t) => t,
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
            .post(format!("{}/v1/memories/retrieve", mem.base_url))
            .header("Authorization", token)
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
    /// Map an LLM-facing v2 cognitive verb (`op`) to a concrete Memoria
    /// v1 HTTP request.
    ///
    /// The LLM only ever sees v2 verbs: `remember`, `recall`, `expand`,
    /// `forget`, `update`, `focus`, `reflect`, `profile`, `feedback`.
    /// Runtime translates each to the v1 endpoint with the appropriate
    /// body shape. Some v2-only semantics (`focus`, `expand` detail
    /// levels, `reflect` candidate synthesis) are synthesized client-side
    /// on top of what v1 exposes — see per-verb comments.
    ///
    /// Returns `(endpoint, payload, method)`. An empty `endpoint` signals
    /// "client-side only, return `payload` verbatim as the tool output"
    /// (used for validation errors and `focus`/synthetic responses).
    pub fn build_direct_request(base: &str, op: &str, args: &Value) -> (String, Value, HttpMethod) {
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
            // ── remember → v1 store (`POST /v1/memories`) ────────────────
            //
            // v2 exposes an open-ended `memory_type` (any Memoria primitive
            // OR an astra business category). The mapping layer lives in
            // [`astra_prompts::memory_types`]:
            //   business `user`       → v1 `profile`     + trust_tier=T1
            //   business `feedback|project|lesson` → v1 `semantic` + T2/T3
            //   business `ref`        → v1 `procedural`  + T2
            //   business `episode`    → v1 `episodic`    + T3
            // The content is prefix-encoded (`[user] …`, `[feedback] …`)
            // so the category survives a v1 store→retrieve round-trip.
            // When v2 stabilises the prefix moves into the `tags` array
            // (`astra:user`, etc.) — the tag is *already* emitted today,
            // v1 just ignores it.
            "remember" => {
                let content = args.get("content").and_then(Value::as_str).unwrap_or("");
                if content.trim().is_empty() {
                    return (
                        String::new(),
                        json!({"error": "memory(action=remember) requires `content`"}),
                        HttpMethod::Post,
                    );
                }
                let raw_type = args
                    .get("memory_type")
                    .and_then(Value::as_str)
                    .unwrap_or("semantic");

                // Protocol translation between v2 taxonomy (business
                // categories: user/feedback/project/ref/lesson/episode) and
                // v1 primitives (semantic/profile/procedural/episodic/
                // working/tool_result).
                use astra_prompts::memory_types::{MemoryCategory, encode as encode_category};
                let (resolved_content, resolved_type, resolved_tier, category_tag) = match raw_type
                {
                    "user" => (
                        encode_category(MemoryCategory::User, content),
                        "profile",
                        Some(MemoryCategory::User.trust_tier()),
                        Some(MemoryCategory::User.v2_tag()),
                    ),
                    "feedback" => (
                        encode_category(MemoryCategory::Feedback, content),
                        "semantic",
                        Some(MemoryCategory::Feedback.trust_tier()),
                        Some(MemoryCategory::Feedback.v2_tag()),
                    ),
                    "project" => (
                        encode_category(MemoryCategory::Project, content),
                        "semantic",
                        Some(MemoryCategory::Project.trust_tier()),
                        Some(MemoryCategory::Project.v2_tag()),
                    ),
                    "ref" | "reference" => (
                        encode_category(MemoryCategory::Reference, content),
                        "procedural",
                        Some(MemoryCategory::Reference.trust_tier()),
                        Some(MemoryCategory::Reference.v2_tag()),
                    ),
                    "lesson" => (
                        encode_category(MemoryCategory::Lesson, content),
                        "semantic",
                        Some(MemoryCategory::Lesson.trust_tier()),
                        Some(MemoryCategory::Lesson.v2_tag()),
                    ),
                    "episode" => (
                        encode_category(MemoryCategory::Episode, content),
                        "episodic",
                        Some(MemoryCategory::Episode.trust_tier()),
                        Some(MemoryCategory::Episode.v2_tag()),
                    ),
                    // Already a v1 primitive — pass through with no
                    // prefix encoding and no implicit trust tier.
                    other => (
                        content.to_string(),
                        astra_prompts::memory_types::normalize_memoria_type(other),
                        None,
                        None,
                    ),
                };

                let mut payload =
                    json!({"content": resolved_content, "memory_type": resolved_type});

                // Explicit `trust_tier` from the caller wins over the
                // category default — agents occasionally need to downgrade
                // confidence (e.g. speculative project memory).
                if let Some(tier) = args.get("trust_tier").and_then(Value::as_str) {
                    payload["trust_tier"] = json!(tier);
                } else if let Some(tier) = resolved_tier {
                    payload["trust_tier"] = json!(tier);
                }

                if let Some(imp) = args.get("importance").and_then(Value::as_f64) {
                    payload["initial_confidence"] = json!(imp.clamp(0.0, 1.0));
                }

                // Tags: explicit caller tags + the astra v2 category tag
                // (so v2 migration doesn't require re-writing history).
                let mut tags: Vec<Value> = args
                    .get("tags")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if let Some(tag) = category_tag
                    && !tags.iter().any(|t| t.as_str() == Some(tag))
                {
                    tags.push(json!(tag));
                }
                if !tags.is_empty() {
                    payload["tags"] = Value::Array(tags);
                }

                if let Some(at) = Self::sanitized_agent_type(args) {
                    payload["agent_type"] = json!(at);
                }
                inject_identity(&mut payload);

                // Session-scoped memory types MUST carry a session_id so
                // Memoria's governance can archive / isolate them. Without
                // it the row becomes orphaned and never cleans up.
                let has_sid = payload
                    .get("session_id")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty());
                if matches!(resolved_type, "working" | "episodic") && !has_sid {
                    return (
                        String::new(),
                        json!({"error": format!(
                            "memory(action=remember, memory_type=\"{resolved_type}\") requires \
                             `session_id` for session-scoped isolation; the dispatcher must inject it"
                        )}),
                        HttpMethod::Post,
                    );
                }

                (format!("{base}/v1/memories"), payload, HttpMethod::Post)
            }
            // ── recall → v1 retrieve (`POST /v1/memories/retrieve`) ──────
            //
            // v2 collapses `retrieve` + `search` into a single `recall`.
            // Both v1 endpoints share the same request/response shape,
            // so we always hit `/v1/memories/retrieve` (the hybrid path).
            "recall" => {
                let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                let top_k = args.get("top_k").and_then(Value::as_u64).unwrap_or(10);
                let mut pl = json!({"query": query, "top_k": top_k});
                if let Some(mc) = args.get("min_confidence").and_then(Value::as_f64) {
                    pl["min_confidence"] = json!(mc);
                }
                inject_identity(&mut pl);
                // v2 `scope` → v1 `session_scope`. Memoria v1 understands
                // "prefer" (rank session matches higher, still surface
                // cross-session) and "only" (strict session isolation).
                // v2 only exposes a binary `scope`:
                //   - "session" → "only" (strict)
                //   - "all" (default) → no scope header (v1 default is "prefer"
                //     when session_id is present, otherwise unscoped).
                // `session_scope` + `session_id` is a hard pair in v1; we
                // forward scope only when session_id is already present.
                let has_sid = pl
                    .get("session_id")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty());
                if let Some(scope) = args.get("scope").and_then(Value::as_str) {
                    match scope {
                        "session" if has_sid => {
                            pl["session_scope"] = json!("only");
                        }
                        "session" => {
                            // Can't enforce strict session-scope without a
                            // session id; fail loud so callers catch it
                            // instead of silently downgrading to unscoped.
                            return (
                                String::new(),
                                json!({"error":
                                    "memory(action=recall, scope=\"session\") requires an active session_id"}),
                                HttpMethod::Post,
                            );
                        }
                        _ => {}
                    }
                }
                if let Some(at) = Self::sanitized_agent_type(args) {
                    pl["agent_type"] = json!(at);
                }
                // `view` is v2-only (compact/overview/full). v1 ignores it
                // silently; keeping it in the payload lets the eventual
                // v2 backend see the hint without code change.
                if let Some(view) = args.get("view").and_then(Value::as_str) {
                    pl["view"] = json!(view);
                }
                (format!("{base}/v1/memories/retrieve"), pl, HttpMethod::Post)
            }
            // ── expand → v1 GET memory by id (`GET /v1/memories/:id`) ────
            //
            // v2 has abstract / overview / detail / linked levels; v1
            // stores flat content. For now we fetch the full row; the
            // dispatcher downgrades according to `level`.
            "expand" => match args.get("memory_id").and_then(Value::as_str) {
                Some(mid) if !mid.is_empty() => (
                    format!("{base}/v1/memories/{mid}"),
                    json!({}),
                    HttpMethod::Get,
                ),
                _ => (
                    String::new(),
                    json!({"error": "memory(action=expand) requires `memory_id`"}),
                    HttpMethod::Post,
                ),
            },
            // ── forget → v1 purge (`POST /v1/memories/purge`) ────────────
            "forget" => {
                let mut pl = json!({});
                if let Some(ids) = args.get("memory_ids").or_else(|| args.get("memory_id")) {
                    pl["memory_ids"] = if ids.is_array() {
                        ids.clone()
                    } else if let Some(s) = ids.as_str() {
                        json!(s.split(',').map(str::trim).collect::<Vec<_>>())
                    } else {
                        json!([ids.to_string()])
                    };
                } else if let Some(topic) = args.get("topic").and_then(Value::as_str) {
                    pl["topic"] = json!(topic);
                }
                if let Some(reason) = args.get("reason").and_then(Value::as_str) {
                    pl["reason"] = json!(reason);
                }
                let has_filter = pl
                    .as_object()
                    .is_some_and(|m| m.contains_key("memory_ids") || m.contains_key("topic"));
                if has_filter {
                    (format!("{base}/v1/memories/purge"), pl, HttpMethod::Post)
                } else {
                    (
                        String::new(),
                        json!({"error": "memory(action=forget) requires `memory_id` or `topic`"}),
                        HttpMethod::Post,
                    )
                }
            }
            // ── update → v1 correct (`PUT /v1/memories/:id/correct`) ─────
            //
            // v2's richer update (tags_add / tags_remove / importance) is
            // flattened into v1's single `new_content` + `reason` shape;
            // tag and importance fields are dropped until v1 grows support.
            "update" => {
                let new_content = args
                    .get("content")
                    .or_else(|| args.get("new_content"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let reason = args
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("update");
                if let Some(mid) = args
                    .get("memory_id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    let mut pl = json!({"new_content": new_content, "reason": reason});
                    inject_identity(&mut pl);
                    (
                        format!("{base}/v1/memories/{mid}/correct"),
                        pl,
                        HttpMethod::Put,
                    )
                } else if let Some(query) = args
                    .get("query")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    let mut pl =
                        json!({"query": query, "new_content": new_content, "reason": reason});
                    inject_identity(&mut pl);
                    (format!("{base}/v1/memories/correct"), pl, HttpMethod::Post)
                } else {
                    (
                        String::new(),
                        json!({"error": "memory(action=update) requires `memory_id` or `query`"}),
                        HttpMethod::Post,
                    )
                }
            }
            // ── feedback → v1 feedback (`POST /v1/memories/:id/feedback`) ─
            "feedback" => {
                let mid = args.get("memory_id").and_then(Value::as_str).unwrap_or("");
                let signal = args.get("signal").and_then(Value::as_str).unwrap_or("");
                if mid.is_empty() || signal.is_empty() {
                    return (
                        String::new(),
                        json!({"error":
                            "memory(action=feedback) requires `memory_id` and `signal` (useful|irrelevant|outdated|wrong)"}),
                        HttpMethod::Post,
                    );
                }
                let mut pl = json!({"signal": signal});
                if let Some(ctx) = args.get("context").and_then(Value::as_str) {
                    pl["context"] = json!(ctx);
                }
                (
                    format!("{base}/v1/memories/{mid}/feedback"),
                    pl,
                    HttpMethod::Post,
                )
            }
            // ── reflect → v1 reflect (`POST /v1/reflect`) ────────────────
            "reflect" => {
                let mut pl = json!({});
                if let Some(force) = args.get("force").and_then(Value::as_bool) {
                    pl["force"] = json!(force);
                }
                if let Some(mode) = args.get("mode").and_then(Value::as_str) {
                    pl["mode"] = json!(mode);
                }
                if let Some(limit) = args.get("limit").and_then(Value::as_i64) {
                    pl["limit"] = json!(limit);
                }
                inject_identity(&mut pl);
                (format!("{base}/v1/reflect"), pl, HttpMethod::Post)
            }
            // ── profile → v1 profile (`GET /v1/profiles/me`) ─────────────
            "profile" => {
                let mut pl = json!({});
                inject_identity(&mut pl);
                (format!("{base}/v1/profiles/me"), pl, HttpMethod::Get)
            }
            // ── focus → client-side synthetic (no v1 endpoint) ───────────
            //
            // v1 doesn't expose an attention-boost primitive, so the
            // dispatcher handles `focus` in-process: it stores a session-
            // scoped boost hint that subsequent `recall` calls consult.
            // Returning an empty endpoint tells the caller to short-circuit
            // before the HTTP client runs.
            "focus" => (
                String::new(),
                json!({"error": "memory(action=focus) is handled in-process; see dispatcher"}),
                HttpMethod::Post,
            ),
            _ => (
                String::new(),
                json!({"error": format!("Unknown memory action: {op}")}),
                HttpMethod::Post,
            ),
        }
    }
}

/// Build a one-shot Memoria HTTP client + auth header.
pub fn memoria_oneshot_client(timeout_secs: u64) -> Option<(reqwest::Client, String, String)> {
    let mem = astra_core::MemoriaSettings::from_env();
    let key = mem.master_key?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .no_proxy()
        .build()
        .ok()?;
    Some((client, mem.base_url, key))
}

/// Fire-and-forget: trigger Memoria governance.
pub async fn memoria_governance_fire_and_forget() {
    let Some((client, base, key)) = memoria_oneshot_client(10) else {
        return;
    };
    let _ = client
        .post(format!("{base}/v1/governance"))
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
        .post(format!("{base}/v1/consolidate"))
        .header("Authorization", format!("Bearer {key}"))
        .json(&json!({"force": false}))
        .send()
        .await;
}

// ── Cloud memory helpers (shared between CLI and server) ────────────────

/// Generic Memoria API request for cloud management operations.
pub async fn memoria_cloud_request(
    method: HttpMethod,
    path: &str,
    timeout_secs: u64,
    body: Option<serde_json::Value>,
) -> Result<String, String> {
    let (client, base, key) =
        memoria_oneshot_client(timeout_secs).ok_or("Memoria not configured")?;
    let url = format!("{base}{path}");
    let req = match method {
        HttpMethod::Get => client.get(&url),
        HttpMethod::Put => client.put(&url),
        HttpMethod::Post => client.post(&url),
    };
    let req = req.header("Authorization", format!("Bearer {key}"));
    let req = if let Some(b) = body {
        req.json(&b)
    } else {
        req
    };
    let resp = req
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    let resp_body = resp.text().await.unwrap_or_default();
    if status.is_success() {
        Ok(resp_body)
    } else {
        Err(format!("({status}) {resp_body}"))
    }
}

pub async fn memoria_snapshot_create(name: &str) -> Result<String, String> {
    memoria_cloud_request(
        HttpMethod::Post,
        "/v1/snapshots",
        5,
        Some(json!({"name": name})),
    )
    .await
}
pub async fn memoria_snapshot_rollback(name: &str) -> Result<String, String> {
    memoria_cloud_request(
        HttpMethod::Post,
        &format!("/v1/snapshots/{name}/rollback"),
        10,
        None,
    )
    .await
}
pub async fn memoria_snapshot_diff(name: &str) -> Result<String, String> {
    memoria_cloud_request(
        HttpMethod::Get,
        &format!("/v1/snapshots/{name}/diff"),
        5,
        None,
    )
    .await
}
pub async fn memoria_snapshots_list() -> Result<String, String> {
    memoria_cloud_request(HttpMethod::Get, "/v1/snapshots", 5, None).await
}
pub async fn memoria_branch_create(name: &str) -> Result<String, String> {
    memoria_cloud_request(
        HttpMethod::Post,
        "/v1/branches",
        5,
        Some(json!({"name": name})),
    )
    .await
}
pub async fn memoria_branch_checkout(name: &str) -> Result<String, String> {
    memoria_cloud_request(
        HttpMethod::Post,
        &format!("/v1/branches/{name}/checkout"),
        5,
        None,
    )
    .await
}
pub async fn memoria_branch_merge(name: &str) -> Result<String, String> {
    memoria_cloud_request(
        HttpMethod::Post,
        &format!("/v1/branches/{name}/merge"),
        5,
        None,
    )
    .await
}
pub async fn memoria_branch_diff(name: &str) -> Result<String, String> {
    memoria_cloud_request(
        HttpMethod::Get,
        &format!("/v1/branches/{name}/diff"),
        5,
        None,
    )
    .await
}
pub async fn memoria_branches_list() -> Result<String, String> {
    memoria_cloud_request(HttpMethod::Get, "/v1/branches", 5, None).await
}
pub async fn memoria_reflect() -> Result<String, String> {
    memoria_cloud_request(
        HttpMethod::Post,
        "/v1/reflect",
        15,
        Some(json!({"mode": "auto"})),
    )
    .await
}
pub async fn memoria_health() -> Result<String, String> {
    memoria_cloud_request(HttpMethod::Get, "/v1/health/analyze", 5, None).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn map_business_types_to_memoria_primitives() {
        use astra_prompts::memory_types::normalize_memoria_type;
        assert_eq!(normalize_memoria_type("user"), "profile");
        assert_eq!(normalize_memoria_type("feedback"), "semantic");
        assert_eq!(normalize_memoria_type("project"), "semantic");
        assert_eq!(normalize_memoria_type("lesson"), "semantic");
        assert_eq!(normalize_memoria_type("ref"), "procedural");
        assert_eq!(normalize_memoria_type("reference"), "procedural");
        assert_eq!(normalize_memoria_type("episode"), "episodic");
        // V1 primitives pass through unchanged
        assert_eq!(normalize_memoria_type("semantic"), "semantic");
        assert_eq!(normalize_memoria_type("profile"), "profile");
        assert_eq!(normalize_memoria_type("working"), "working");
    }

    #[test]
    fn store_maps_business_type_before_sending() {
        let args = json!({"content": "test", "memory_type": "feedback"});
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "remember", &args);
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
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "recall", &args);
        assert_eq!(pl["session_id"], "user-42");
        assert_eq!(pl["user_id"], "user-42");
        assert_eq!(pl["query"], "rust patterns");
        assert!(
            pl.get("min_confidence").is_none(),
            "min_confidence should only be sent when explicitly provided"
        );

        // search
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "recall", &args);
        assert_eq!(pl["session_id"], "user-42");
        assert_eq!(pl["user_id"], "user-42");

        // store
        let store_args = json!({
            "content": "hello",
            "session_id": "user-42",
            "user_id": "user-42"
        });
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "remember", &store_args);
        assert_eq!(pl["session_id"], "user-42");
        assert_eq!(pl["user_id"], "user-42");

        // purge — Memoria requires ONLY ONE of memory_ids, topic, session_id.
        // inject_identity is NOT called (would add session_id alongside topic → 422).
        let purge_args = json!({
            "topic": "old",
            "session_id": "user-42",
            "user_id": "user-42"
        });
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "forget", &purge_args);
        assert_eq!(pl["topic"], "old", "purge should use topic as the filter");
        assert!(
            pl.get("session_id").is_none() || pl.get("topic").is_some(),
            "purge must not send both topic AND session_id"
        );

        // correct
        let correct_args = json!({
            "memory_id": "m1",
            "new_content": "fixed",
            "session_id": "user-42",
            "user_id": "user-42"
        });
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "update", &correct_args);
        assert_eq!(pl["session_id"], "user-42");
        assert_eq!(pl["user_id"], "user-42");

        // profile
        let profile_args = json!({"session_id": "user-42", "user_id": "user-42"});
        let (_, pl, _) =
            MemoriaClient::build_direct_request("http://mem", "profile", &profile_args);
        assert_eq!(pl["session_id"], "user-42");
        assert_eq!(pl["user_id"], "user-42");
    }

    #[test]
    fn build_direct_request_omits_identity_when_absent() {
        let args = json!({"query": "test"});
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "recall", &args);
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
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "recall", &args);
        assert_eq!(pl["min_confidence"], json!(0.7));
    }

    // ── Session isolation via v2 `scope` → v1 `session_scope` ──

    #[test]
    fn recall_scope_session_requires_session_id() {
        let args = json!({"query": "test", "top_k": 5, "scope": "session"});
        let (endpoint, pl, _) = MemoriaClient::build_direct_request("http://mem", "recall", &args);
        assert!(
            endpoint.is_empty(),
            "scope=session without session_id must short-circuit to an error"
        );
        assert!(
            pl.get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .contains("session_id"),
            "error must mention missing session_id"
        );
    }

    #[test]
    fn recall_scope_session_sets_session_scope_only() {
        let args = json!({
            "query": "test",
            "top_k": 5,
            "session_id": "sess-abc",
            "scope": "session",
        });
        let (endpoint, pl, _) = MemoriaClient::build_direct_request("http://mem", "recall", &args);
        assert_eq!(endpoint, "http://mem/v1/memories/retrieve");
        assert_eq!(pl["session_id"], "sess-abc");
        assert_eq!(pl["session_scope"], "only");
    }

    #[test]
    fn recall_scope_all_omits_session_scope() {
        let args = json!({
            "query": "test",
            "top_k": 10,
            "session_id": "sess-abc",
            "scope": "all",
        });
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "recall", &args);
        assert!(pl.get("session_scope").is_none());
    }

    #[test]
    fn recall_omits_session_fields_when_absent() {
        let args = json!({"query": "test", "top_k": 10});
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "recall", &args);
        assert!(pl.get("session_id").is_none());
        assert!(pl.get("session_scope").is_none());
    }

    #[test]
    fn recall_routes_to_v1_retrieve_endpoint() {
        let args = json!({"query": "test", "top_k": 10});
        let (endpoint, _, _) = MemoriaClient::build_direct_request("http://mem", "recall", &args);
        assert_eq!(endpoint, "http://mem/v1/memories/retrieve");
    }

    // ── purge exclusivity (Memoria requires ONE of memory_ids/topic/session_id) ──

    #[test]
    fn purge_with_topic_does_not_include_session_id() {
        let args = json!({"topic": "NEPTUNE", "session_id": "sess-42"});
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "forget", &args);
        assert_eq!(pl["topic"], "NEPTUNE");
        assert!(
            pl.get("session_id").is_none(),
            "purge by topic must not include session_id"
        );
    }

    #[test]
    fn purge_with_memory_ids() {
        let args = json!({"memory_ids": ["id1", "id2"]});
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "forget", &args);
        assert!(pl["memory_ids"].is_array());
        assert!(pl.get("topic").is_none());
    }

    #[test]
    fn purge_with_memory_id_string_becomes_array() {
        let args = json!({"memory_id": "id1,id2"});
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "forget", &args);
        let ids = pl["memory_ids"].as_array().expect("should be array");
        assert_eq!(ids.len(), 2);
    }
}

#[cfg(test)]
mod memoria_http_client_tests {
    use super::*;

    #[test]
    fn purge_session_id_not_supported() {
        // Memoria PurgeRequest only accepts memory_ids and topic.
        // session_id is NOT a valid filter — it would cause 422.
        let args = json!({"session_id": "sess-42"});
        let (ep, _, _) = MemoriaClient::build_direct_request("http://mem", "forget", &args);
        assert!(
            ep.is_empty(),
            "purge with only session_id must fail (not supported by Memoria)"
        );
    }

    #[test]
    fn purge_empty_filter_returns_error() {
        let args = json!({});
        let (name, pl, _) = MemoriaClient::build_direct_request("http://mem", "forget", &args);
        assert_eq!(name, "");
        assert!(pl.get("error").is_some());
        assert!(
            pl["error"]
                .as_str()
                .unwrap()
                .contains("memory(action=forget)")
        );
    }

    #[test]
    fn purge_topic_returns_topic_filter() {
        let args = json!({"topic": "NEPTUNE"});
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "forget", &args);
        assert_eq!(pl["topic"], "NEPTUNE");
        assert!(pl.get("session_id").is_none());
        assert!(pl.get("memory_ids").is_none());
    }

    #[test]
    fn purge_responses_are_not_empty() {
        let args = json!({"topic": "NEPTUNE"});
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "forget", &args);
        assert!(
            pl.is_object() && !pl.as_object().unwrap().contains_key("error"),
            "purge with valid filter must produce non-error payload, got: {pl}"
        );
    }

    #[test]
    fn purge_result_to_agent_response_delivers_message() {
        use super::*;
        let raw = json!({"deleted_count": 3});
        let enriched = MemoriaClient::purge_result_to_agent_response(&raw, "topic:NEPTUNE");
        assert_eq!(enriched["status"], "ok");
        assert_eq!(enriched["deleted_count"], 3);
        assert!(enriched["message"].as_str().unwrap().contains("3"));
    }

    #[test]
    fn purge_result_to_agent_response_zero_deleted() {
        use super::*;
        let raw = json!({"deleted_count": 0});
        let enriched = MemoriaClient::purge_result_to_agent_response(&raw, "session:abc");
        assert_eq!(enriched["deleted_count"], 0);
        assert!(enriched["message"].as_str().unwrap().contains("0 deleted"));
    }
}
