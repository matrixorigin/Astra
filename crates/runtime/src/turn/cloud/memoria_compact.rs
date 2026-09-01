//! Memoria-based message compaction.
//!
//! Retrieves typed current-session memory for the context pipeline and may
//! persist a typed compaction episode after a successful summary.
//!
//! Architecture:
//! ```text
//! 1. Messages exceed budget threshold
//! 2. Retrieve typed current-session memory as separate runtime context
//! 3. Truncate old messages (keep recent turns)
//! 4. Optionally store a semantic compaction episode after summary succeeds
//! ```

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::compaction::CompactResult;
use super::compaction_engine::CompactionEngine;
use crate::prompts::{CompactConfig, CompactionTier};
use astra_text_utils::str_preview::truncate_str;
use astra_turn_core::cloud_summary::SummaryLlmClient;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for Memoria-based compaction.
#[derive(Debug, Clone)]
pub struct MemoriaCompactConfig {
    /// Minimum tokens before attempting Memoria retrieval.
    pub min_tokens_for_retrieval: usize,
    /// Maximum memories to retrieve for context.
    pub max_memories: usize,
    /// Maximum prompt tokens reserved for non-snapshot working memories.
    pub max_memory_tokens: usize,
}

impl Default for MemoriaCompactConfig {
    fn default() -> Self {
        Self {
            min_tokens_for_retrieval: 5_000,
            max_memories: 10,
            max_memory_tokens: 4_000,
        }
    }
}

/// Parameters for a single compaction invocation.
#[derive(Debug, Clone)]
pub struct MemoriaCompactParams {
    /// Total character budget for output messages.
    pub budget_chars: usize,
    /// Characters to keep from recent messages (high priority).
    pub keep_chars: usize,
    /// Compaction tier (affects aggressiveness).
    pub tier: CompactionTier,
    /// Number of recent turns to preserve.
    pub keep_recent_turns: usize,
    /// Current token count before compaction.
    pub current_tokens: usize,
    /// Optional session facts for facts-first compaction (L1a ground truth).
    /// When present, `build_facts_first_injection()` is used as the primary
    /// memory context, with Memoria narrative as supplement.
    pub session_facts: Option<astra_turn_types::session_facts::SessionFacts>,
}

impl Default for MemoriaCompactParams {
    fn default() -> Self {
        Self {
            budget_chars: 0,
            keep_chars: 0,
            tier: CompactionTier::Normal,
            keep_recent_turns: 0,
            current_tokens: 0,
            session_facts: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Compatible session memory paths
// ---------------------------------------------------------------------------

const CLAUDE_PROJECTS_SANITIZE_MAX_CHARS: usize = 200;

fn djb2_hash_utf16(s: &str) -> i32 {
    let mut hash: i32 = 0;
    for unit in s.encode_utf16() {
        hash = hash
            .wrapping_shl(5)
            .wrapping_sub(hash)
            .wrapping_add(i32::from(unit));
    }
    hash
}

fn abs_hash_to_string_36(h: i32) -> String {
    let mut n = h.unsigned_abs() as u64;
    if n == 0 {
        return "0".to_string();
    }
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = Vec::new();
    while n > 0 {
        buf.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap_or_default()
}

/// Sanitize a working-directory path for use under `CLAUDE_CONFIG_DIR/projects/`,
/// (alphanumeric → keep, else `-`, length cap + djb2).
pub fn sanitize_path_for_claude_projects(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if sanitized.chars().count() <= CLAUDE_PROJECTS_SANITIZE_MAX_CHARS {
        return sanitized;
    }
    let prefix: String = sanitized
        .chars()
        .take(CLAUDE_PROJECTS_SANITIZE_MAX_CHARS)
        .collect();
    let hash = abs_hash_to_string_36(djb2_hash_utf16(name));
    format!("{prefix}-{hash}")
}

fn claude_config_home_dir() -> PathBuf {
    std::env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".claude")
        })
}

/// `{CLAUDE_CONFIG_DIR}/projects/<sanitized cwd>/<session_id>/session-memory/summary.md`
///
/// Kept only for external tooling that still looks at the legacy
/// on-disk layout. The runtime itself no longer reads or writes this
/// path — session memory lives in Memoria.
pub fn claude_code_session_memory_path(cwd: &str, session_id: &str) -> PathBuf {
    claude_config_home_dir()
        .join("projects")
        .join(sanitize_path_for_claude_projects(cwd))
        .join(session_id)
        .join("session-memory")
        .join("summary.md")
}

pub use astra_memoria::{
    MemoriaMemory, MemoriaPort, MemoryScope, ReflectCandidate, ReflectSummary,
    parse_reflect_candidates, validate_strict_memories,
};

fn cross_session_abstract(prefix: &str, evidence: &str) -> String {
    let evidence = evidence
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(evidence)
        .trim()
        .trim_start_matches(['-', '*', '#', ' ']);
    let mut abstract_text = format!("{prefix}: {evidence}")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if abstract_text.chars().count() < astra_prompts::memory_proto::ABSTRACT_MIN_CHARS {
        abstract_text.push_str(" — retained cross-session evidence");
    }
    if abstract_text.chars().count() > astra_prompts::memory_proto::ABSTRACT_MAX_CHARS {
        abstract_text = abstract_text
            .chars()
            .take(astra_prompts::memory_proto::ABSTRACT_MAX_CHARS - 1)
            .collect::<String>();
        abstract_text.push('…');
    }
    abstract_text
}

fn encode_episode_memory(session_id: &str, overview: &str) -> String {
    let goal = overview
        .lines()
        .find_map(|line| line.trim().strip_prefix("Goal: "))
        .unwrap_or_else(|| overview.lines().next().unwrap_or(overview));
    let abstract_text = cross_session_abstract(&format!("Session {session_id}"), goal);
    astra_prompts::memory_proto::MemoryEntry::new_layered(
        astra_prompts::memory_proto::NS_EPISODE,
        astra_prompts::memory_proto::ST_SUMMARY,
        &abstract_text,
        Some(overview.trim()),
        None,
    )
    .encode()
}

fn encode_scene_memory(signal: &str, summary: &str) -> String {
    let prefix = if signal.trim().is_empty() {
        "Recurring cross-session pattern".to_string()
    } else {
        format!("Recurring pattern {}", signal.trim())
    };
    let abstract_text = cross_session_abstract(&prefix, summary);
    astra_prompts::memory_proto::MemoryEntry::new_layered(
        astra_prompts::memory_proto::NS_INSIGHT,
        astra_prompts::memory_proto::ST_AUTO,
        &abstract_text,
        Some(summary.trim()),
        None,
    )
    .encode()
}

// ---------------------------------------------------------------------------
// HTTP Client Implementation
// ---------------------------------------------------------------------------

/// HTTP-based Memoria client.
#[derive(Clone)]
pub struct HttpMemoriaPort {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
    owner_user_id: Option<String>,
    owner_binding_required: bool,
}

impl HttpMemoriaPort {
    pub fn new(base_url: String, api_key: String) -> Self {
        Self {
            base_url,
            api_key,
            http: astra_core::net::build_internal_http_client(
                reqwest::Client::builder()
                    .connect_timeout(std::time::Duration::from_secs(10))
                    .timeout(std::time::Duration::from_secs(60)),
                "memoria compact client",
            ),
            owner_user_id: None,
            owner_binding_required: false,
        }
    }

    fn new_master(base_url: String, master_key: String) -> Self {
        Self {
            owner_binding_required: true,
            ..Self::new(base_url, master_key)
        }
    }

    /// Bind this transport to the authenticated owner that created the
    /// runtime. Strict session operations fail closed when no owner is bound.
    pub fn with_owner_user_id(mut self, user_id: impl Into<String>) -> Self {
        self.owner_user_id = Some(user_id.into());
        self
    }

    /// Create from environment variables.
    pub fn from_env() -> Option<Self> {
        let mem = astra_core::MemoriaSettings::from_env();
        Some(Self::new_master(mem.base_url, mem.master_key?))
    }

    fn request(
        &self,
        method: reqwest::Method,
        url: &str,
        requested_owner: Option<&str>,
    ) -> Result<reqwest::RequestBuilder, String> {
        let requested_owner = requested_owner
            .filter(|owner| !owner.is_empty())
            .map(|owner| astra_memoria::MemoryScope::new(owner, "transport-owner"))
            .transpose()?
            .map(|scope| scope.user_id);
        let bound_owner = self
            .owner_user_id
            .as_deref()
            .map(|owner| astra_memoria::MemoryScope::new(owner, "transport-owner"))
            .transpose()?
            .map(|scope| scope.user_id);
        if let (Some(bound), Some(requested)) = (bound_owner.as_deref(), requested_owner.as_deref())
            && bound != requested
        {
            return Err("memory_scope_violation: requested owner differs from bound owner".into());
        }
        let owner = requested_owner.as_deref().or(bound_owner.as_deref());
        if self.owner_binding_required && owner.is_none() {
            return Err(
                "Memoria master-key data request requires an authenticated owner binding".into(),
            );
        }
        let request = self
            .http
            .request(method, url)
            .header("Authorization", format!("Bearer {}", self.api_key));
        Ok(match owner {
            Some(owner) => request.header("X-User-Id", owner),
            None => request,
        })
    }

    pub async fn health_check(&self) -> Result<(), String> {
        let url = format!("{}/v1/health/analyze", self.base_url.trim_end_matches('/'));
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.http
                .get(url)
                .header("Authorization", format!("Bearer {}", self.api_key))
                .send(),
        )
        .await
        .map_err(|_| "Memoria health check timed out after 5s".to_string())?
        .map_err(|error| format!("Memoria health check request failed: {error}"))?;

        if response.status().is_success() {
            return Ok(());
        }

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(format!(
            "Memoria health check failed: status={status}, body={body}"
        ))
    }
}

fn parse_retrieved_memories(data: &Value) -> Vec<MemoriaMemory> {
    let Some(arr) = data
        .get("memories")
        .and_then(Value::as_array)
        .or_else(|| data.as_array())
    else {
        return Vec::new();
    };

    let mut memories = Vec::with_capacity(arr.len());
    let mut dropped = 0usize;
    let mut first_error = None;

    for (index, value) in arr.iter().enumerate() {
        match serde_json::from_value::<MemoriaMemory>(value.clone()) {
            Ok(memory) => memories.push(memory),
            Err(err) => {
                dropped += 1;
                if first_error.is_none() {
                    first_error = Some(format!("index {index}: {err}"));
                }
            }
        }
    }

    if dropped > 0 {
        tracing::warn!(
            target: "astra_runtime::memoria_compact",
            parsed = memories.len(),
            dropped,
            first_error = first_error.as_deref().unwrap_or("unknown"),
            "discarded malformed Memoria retrieve entries"
        );
    }

    memories
}

fn parse_strict_retrieved_memories(
    data: &Value,
    scope: &astra_memoria::MemoryScope,
) -> Result<Vec<MemoriaMemory>, String> {
    astra_memoria::validate_strict_recall_payload(data, scope)?;
    let entries = data
        .as_array()
        .or_else(|| data.get("memories").and_then(Value::as_array))
        .or_else(|| data.get("items").and_then(Value::as_array))
        .expect("strict payload validation guarantees a supported memory collection");
    let memories = entries
        .iter()
        .enumerate()
        .map(|(index, value)| {
            serde_json::from_value::<MemoriaMemory>(value.clone())
                .map_err(|error| format!("invalid strict Memoria retrieve item {index}: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    astra_memoria::validate_strict_memories(&memories, scope)?;
    Ok(memories)
}

#[async_trait::async_trait]
impl MemoriaPort for HttpMemoriaPort {
    fn bind_owner(&self, user_id: &str) -> Result<std::sync::Arc<dyn MemoriaPort>, String> {
        let scope = astra_memoria::MemoryScope::new(user_id, "owner-binding-validation")?;
        Ok(std::sync::Arc::new(
            self.clone().with_owner_user_id(scope.user_id),
        ))
    }

    async fn retrieve_for_prompt(
        &self,
        query: &str,
        user_id: &str,
        session_id: &str,
        top_k: usize,
    ) -> Result<Vec<MemoriaMemory>, String> {
        let url = format!(
            "{}/v1/memories/retrieve",
            self.base_url.trim_end_matches('/')
        );
        let mut body = json!({
            "query": query,
            "top_k": top_k,
        });
        if !session_id.trim().is_empty() {
            body["session_id"] = json!(session_id);
        }

        let resp = self
            .request(reqwest::Method::POST, &url, Some(user_id))?
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Memoria prompt retrieve failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("Memoria prompt retrieve HTTP {}", resp.status()));
        }

        let data: Value = resp
            .json()
            .await
            .map_err(|e| format!("Memoria prompt retrieve parse failed: {e}"))?;
        Ok(parse_retrieved_memories(&data))
    }

    async fn retrieve_ext(
        &self,
        query: &str,
        session_id: Option<&str>,
        top_k: usize,
        filter_session: bool,
    ) -> Result<Vec<MemoriaMemory>, String> {
        let url = format!(
            "{}/v1/memories/retrieve",
            self.base_url.trim_end_matches('/')
        );
        let mut body = json!({
            "query": query,
            "top_k": top_k,
        });
        let strict_scope = if filter_session {
            let sid = session_id.ok_or_else(|| {
                "strict Memoria retrieve requires a non-empty session_id".to_string()
            })?;
            let user_id = self.owner_user_id.as_deref().ok_or_else(|| {
                "strict Memoria retrieve requires an authenticated owner binding".to_string()
            })?;
            Some(astra_memoria::MemoryScope::new(user_id, sid)?)
        } else {
            None
        };
        if let Some(sid) = session_id {
            body["session_id"] = json!(sid);
            if filter_session {
                // Map to v1's session_scope primitive instead of the
                // legacy `filter_session` flag (Memoria never honored it).
                body["session_scope"] = json!("only");
            }
        }
        let resp = self
            .request(reqwest::Method::POST, &url, None)?
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Memoria retrieve failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("Memoria retrieve HTTP {}", resp.status()));
        }

        let data: Value = resp
            .json()
            .await
            .map_err(|e| format!("Memoria retrieve parse failed: {e}"))?;

        match strict_scope.as_ref() {
            Some(scope) => parse_strict_retrieved_memories(&data, scope),
            None => Ok(parse_retrieved_memories(&data)),
        }
    }

    async fn retrieve_scoped_typed(
        &self,
        _query: &str,
        session_id: &str,
        top_k: usize,
        memory_types: &[&str],
    ) -> Result<Vec<MemoriaMemory>, String> {
        let user_id = self.owner_user_id.as_deref().ok_or_else(|| {
            "typed Memoria retrieve requires an authenticated owner binding".to_string()
        })?;
        let scope = astra_memoria::MemoryScope::new(user_id, session_id)?;
        let url = format!("{}/v1/memories", self.base_url.trim_end_matches('/'));
        let limit = top_k.clamp(1, 500).to_string();
        let requested_types: Vec<Option<&str>> = if memory_types.is_empty() {
            vec![None]
        } else {
            memory_types.iter().copied().map(Some).collect()
        };
        let mut memories = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for memory_type in requested_types {
            let mut query = vec![
                ("session_id", scope.session_id.as_str()),
                ("limit", limit.as_str()),
            ];
            if let Some(memory_type) = memory_type {
                query.push(("memory_type", memory_type));
            }
            let resp = self
                .request(reqwest::Method::GET, &url, None)?
                .query(&query)
                .send()
                .await
                .map_err(|e| format!("Memoria typed list failed: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!("Memoria typed list HTTP {}", resp.status()));
            }
            let data: Value = resp
                .json()
                .await
                .map_err(|e| format!("Memoria typed list parse failed: {e}"))?;
            for memory in parse_strict_retrieved_memories(&data, &scope)? {
                if memory_type.is_some_and(|expected| memory.memory_type != expected) {
                    return Err(
                        "memory_scope_violation: typed list returned an invalid memory_type".into(),
                    );
                }
                if seen.insert(memory.memory_id.clone()) {
                    memories.push(memory);
                }
            }
        }
        memories.truncate(top_k);
        Ok(memories)
    }

    async fn store(
        &self,
        content: &str,
        memory_type: &str,
        session_id: Option<&str>,
        trust_tier: Option<&str>,
    ) -> Result<String, String> {
        let url = format!("{}/v1/memories", self.base_url.trim_end_matches('/'));
        let mut body = json!({
            "content": content,
            "memory_type": memory_type,
        });
        if let Some(sid) = session_id {
            let user_id = self.owner_user_id.as_deref().ok_or_else(|| {
                "session-scoped Memoria store requires an authenticated owner binding".to_string()
            })?;
            let scope = astra_memoria::MemoryScope::new(user_id, sid)?;
            body["session_id"] = json!(scope.session_id);
        }
        if let Some(tier) = trust_tier {
            body["trust_tier"] = json!(tier);
        }

        let resp = self
            .request(reqwest::Method::POST, &url, None)?
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Memoria store failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp
                .text()
                .await
                .unwrap_or_else(|_| "<body unreadable>".to_string());
            return Err(format!("Memoria store HTTP {status}: {}", body.trim()));
        }

        let data: Value = resp
            .json()
            .await
            .map_err(|e| format!("Memoria store parse failed: {e}"))?;

        data.get("memory_id")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| "Memoria store: no memory_id in response".to_string())
    }

    /// Purge working memories for a session.
    ///
    /// Uses Memoria's `session_id` + `memory_types=["working"]` selector,
    /// which is an exact filter on the session column — reliable even
    /// for UUID-style session IDs. The prior implementation used
    /// `topic="session:<uuid>"` which resolved via fulltext ngram
    /// tokenization; UUID tokens never matched, so `purge_working` was
    /// silently a no-op on every real session.
    async fn purge_working(&self, session_id: &str) -> Result<u64, String> {
        self.purge_memory_types(session_id, &["working"]).await
    }

    async fn purge_memory_types(
        &self,
        session_id: &str,
        memory_types: &[&str],
    ) -> Result<u64, String> {
        if session_id.is_empty() {
            return Err("purge_memory_types requires non-empty session_id".into());
        }
        if memory_types.is_empty() {
            return Ok(0);
        }
        let user_id = self.owner_user_id.as_deref().ok_or_else(|| {
            "session-scoped Memoria purge requires an authenticated owner binding".to_string()
        })?;
        let scope = astra_memoria::MemoryScope::new(user_id, session_id)?;
        let url = format!("{}/v1/memories/purge", self.base_url.trim_end_matches('/'));
        let body = json!({
            "session_id": scope.session_id,
            "memory_types": memory_types,
            "reason": "session compaction cleanup",
        });

        let resp = self
            .request(reqwest::Method::POST, &url, Some(&scope.user_id))?
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Memoria purge failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("Memoria purge HTTP {}", resp.status()));
        }

        let data: Value = resp
            .json()
            .await
            .map_err(|e| format!("Memoria purge parse failed: {e}"))?;

        // Memoria returns `{ "purged": N, ... }` (see `PurgeResponse`);
        // prior v1 also emitted `deleted_count` for topic-mode. Accept
        // either so this works against both shapes.
        Ok(data
            .get("purged")
            .or_else(|| data.get("deleted_count"))
            .and_then(Value::as_u64)
            .unwrap_or(0))
    }

    async fn delete(&self, memory_id: &str) -> Result<(), String> {
        if memory_id.is_empty() {
            return Err("delete requires a non-empty memory_id".into());
        }
        let url = format!("{}/v1/memories/purge", self.base_url.trim_end_matches('/'));
        let resp = self
            .request(reqwest::Method::POST, &url, None)?
            .json(&json!({
                "memory_ids": [memory_id],
                "reason": "superseded session memory",
            }))
            .send()
            .await
            .map_err(|e| format!("Memoria delete failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("Memoria delete HTTP {}", resp.status()));
        }
        Ok(())
    }

    /// Persist an episodic session summary via v1 `/v1/memories` with
    /// `memory_type=episodic`. The session_id is mandatory — episodes
    /// without a session_id would never be cleaned up.
    async fn store_episode(&self, session_id: &str, overview: &str) -> Result<String, String> {
        if session_id.is_empty() {
            return Err("store_episode requires non-empty session_id".into());
        }
        if overview.trim().is_empty() {
            return Err("store_episode: empty overview".into());
        }
        let user_id = self
            .owner_user_id
            .as_deref()
            .ok_or_else(|| "store_episode requires an authenticated owner binding".to_string())?;
        let scope = astra_memoria::MemoryScope::new(user_id, session_id)?;
        let url = format!("{}/v1/memories", self.base_url.trim_end_matches('/'));
        let content = encode_episode_memory(session_id, overview);
        let body = json!({
            "content": content,
            "memory_type": "episodic",
            "session_id": scope.session_id,
            "trust_tier": "T3",
            "source": "astra:session_end_orchestrator",
        });
        let resp = self
            .request(reqwest::Method::POST, &url, Some(&scope.user_id))?
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Memoria store_episode failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("Memoria store_episode HTTP {}", resp.status()));
        }
        let data: Value = resp
            .json()
            .await
            .map_err(|e| format!("Memoria store_episode parse failed: {e}"))?;
        Ok(data
            .get("memory_id")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_default())
    }

    /// Persist a reflect scene candidate as a semantic memory with a stable
    /// source label. Its typed content remains the recall protocol boundary.
    async fn store_scene(
        &self,
        session_id: &str,
        signal: &str,
        summary: &str,
    ) -> Result<String, String> {
        if summary.trim().is_empty() {
            return Err("store_scene: empty summary".into());
        }
        let url = format!("{}/v1/memories", self.base_url.trim_end_matches('/'));
        let content = encode_scene_memory(signal, summary);
        let mut body = json!({
            "content": content,
            "memory_type": "semantic",
            "trust_tier": "T4",
            "source": "astra:session_end_reflect",
        });
        if !session_id.is_empty() {
            let user_id = self
                .owner_user_id
                .as_deref()
                .ok_or_else(|| "store_scene requires an authenticated owner binding".to_string())?;
            let scope = astra_memoria::MemoryScope::new(user_id, session_id)?;
            body["session_id"] = json!(scope.session_id);
        }
        let resp = self
            .request(reqwest::Method::POST, &url, None)?
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Memoria store_scene failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("Memoria store_scene HTTP {}", resp.status()));
        }
        let data: Value = resp
            .json()
            .await
            .map_err(|e| format!("Memoria store_scene parse failed: {e}"))?;
        Ok(data
            .get("memory_id")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_default())
    }

    async fn reflect_session(
        &self,
        session_id: &str,
        force: bool,
    ) -> Result<ReflectSummary, String> {
        let url = format!("{}/v1/reflect", self.base_url.trim_end_matches('/'));
        // Use `candidates` mode so we always get the raw cluster list
        // back. Memoria's `internal` mode synthesizes scenes server-side
        // via its own LLM, but that requires LLM_API_KEY on the server —
        // in the common case the backend is LLM-less. `candidates` works
        // regardless and lets the client forward-feed the clusters as
        // `astra:scene`-tagged memories so next-session prewarm sees them.
        let mut body = json!({"force": force, "mode": "candidates"});
        if !session_id.is_empty() {
            let user_id = self.owner_user_id.as_deref().ok_or_else(|| {
                "reflect_session requires an authenticated owner binding".to_string()
            })?;
            let scope = astra_memoria::MemoryScope::new(user_id, session_id)?;
            body["session_id"] = json!(scope.session_id);
        }
        let resp = self
            .request(reqwest::Method::POST, &url, None)?
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Memoria reflect failed: {e}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!(
                "Memoria reflect HTTP {status}: {}",
                text.chars().take(120).collect::<String>()
            ));
        }
        let data: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let candidate_payloads = parse_reflect_candidates(&data);
        let candidate_count = candidate_payloads.len() as u64;
        Ok(ReflectSummary {
            synthesized: data
                .get("synthesized")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            candidates: candidate_count.max(
                data.get("scenes_created")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            ),
            candidate_payloads,
            diagnostics: text.chars().take(200).collect(),
        })
    }

    async fn feedback(
        &self,
        memory_id: &str,
        signal: &str,
        context: Option<&str>,
    ) -> Result<(), String> {
        if memory_id.is_empty() {
            return Err("feedback: empty memory_id".into());
        }
        if !matches!(signal, "useful" | "irrelevant" | "outdated" | "wrong") {
            return Err(format!("invalid feedback signal {signal:?}"));
        }
        use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
        let encoded = utf8_percent_encode(memory_id, NON_ALPHANUMERIC).to_string();
        let url = format!(
            "{}/v1/memories/{}/feedback",
            self.base_url.trim_end_matches('/'),
            encoded
        );
        let mut body = json!({"signal": signal});
        if let Some(ctx) = context {
            body["context"] = json!(ctx);
        }
        let resp = self
            .request(reqwest::Method::POST, &url, None)?
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Memoria feedback failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("Memoria feedback HTTP {}", resp.status()));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Compaction Logic
// ---------------------------------------------------------------------------

fn build_session_memory_context(
    memories: &[MemoriaMemory],
    session_id: &str,
    tier: CompactionTier,
    facts_override: Option<&astra_turn_types::session_facts::SessionFacts>,
) -> Option<String> {
    let include_overview = matches!(tier, CompactionTier::AggressivePrune);
    memories.iter().find_map(|memory| {
        crate::session_memory::runner::decode_session_memory_prompt(
            &memory.content,
            session_id,
            facts_override,
            include_overview,
        )
    })
}

/// Preserve query-relevant current-session working memories that are not the
/// canonical session snapshot. The snapshot has a dedicated lane; tool-written
/// task notes, intermediate state, and session lessons still belong in the
/// shared typed Memory lane after compaction.
fn build_retrieved_working_memory_entries(
    memories: &[MemoriaMemory],
    session_id: &str,
    max_tokens: usize,
) -> Vec<astra_turn_core::context_sources::MemoryEntry> {
    if max_tokens == 0 {
        return Vec::new();
    }

    let mut entries = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();
    let mut seen_content = std::collections::HashSet::new();
    let mut remaining_tokens = max_tokens;

    for memory in memories {
        if memory.memory_type != "working" {
            continue;
        }
        if crate::session_memory::runner::decode_session_memory_entry(&memory.content, session_id)
            .is_some()
        {
            continue;
        }

        let memory_id = memory.memory_id.trim();
        let content = memory.content.trim();
        if memory_id.is_empty()
            || content.is_empty()
            || !seen_ids.insert(memory_id.to_string())
            || !seen_content.insert(content.to_string())
        {
            continue;
        }

        let freshness = memory.freshness_suffix();
        let evidence = format!(
            "Current-session working-memory evidence. It may be incomplete or stale; the latest user request and live tool results take precedence.\n{content}{freshness}"
        );
        let estimated_tokens = crate::prompts::estimate_str_tokens(&evidence).max(1);
        let bounded_content = if estimated_tokens > remaining_tokens {
            if !entries.is_empty() {
                break;
            }
            truncate_str(&evidence, remaining_tokens.saturating_mul(4).max(1))
        } else {
            evidence
        };
        let bounded_tokens = crate::prompts::estimate_str_tokens(&bounded_content).max(1);
        if bounded_tokens > remaining_tokens {
            break;
        }

        let score = memory
            .retrieval_score
            .filter(|score| score.is_finite())
            .unwrap_or(0.0);
        entries.push(
            astra_turn_core::context_sources::MemoryEntry::scored(bounded_content, score)
                .with_memory_identity(memory_id, "working")
                .with_source("memoria.compaction_working"),
        );
        remaining_tokens = remaining_tokens.saturating_sub(bounded_tokens);
        if remaining_tokens == 0 {
            break;
        }
    }

    entries
}

// ---------------------------------------------------------------------------
// Unified budget for Memoria injection + LLM summary + truncated messages
// ---------------------------------------------------------------------------

/// Assumed extra prompt / framing tokens beyond `summary_token_budget` output.
const SUMMARY_PROMPT_OVERHEAD_TOKENS: usize = 768;

/// Never reserve more than this fraction of the total char budget for the summary slot alone.
const SUMMARY_RESERVE_MAX_PCT: usize = 40;

/// Reserve room for a typed compaction summary before trimming history.
#[must_use]
fn plan_summary_reservation(
    budget_chars: usize,
    will_summarize: bool,
    summary_token_budget: usize,
) -> usize {
    if !will_summarize {
        return 0;
    }
    let raw = summary_token_budget
        .saturating_add(SUMMARY_PROMPT_OVERHEAD_TOKENS)
        .saturating_mul(4);
    let pct_cap = budget_chars.saturating_mul(SUMMARY_RESERVE_MAX_PCT) / 100;
    raw.min(pct_cap)
}

#[must_use]
fn adjusted_message_budget_chars(
    budget_chars: usize,
    memory_content_chars: usize,
    summary_reserve_chars: usize,
) -> usize {
    budget_chars.saturating_sub(memory_content_chars.saturating_add(summary_reserve_chars))
}

fn truncate_summary_for_budget(summary: String, summary_token_budget: usize) -> String {
    let max_chars = summary_token_budget.saturating_mul(4).max(256);
    if summary.chars().count() <= max_chars {
        summary
    } else {
        summary.chars().take(max_chars).collect::<String>()
            + "\n...[summary truncated for context budget]"
    }
}

/// Default retrieve query when the conversation yields no usable signals.
const MEMORIA_RETRIEVE_QUERY_FALLBACK: &str = "current session state and open work";

/// Collapse whitespace for a compact retrieval query string.
fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn message_user_text(m: &Value) -> Option<String> {
    if m.get("role").and_then(Value::as_str) != Some("user") {
        return None;
    }
    let c = m.get("content")?;
    if let Some(s) = c.as_str() {
        let t = s.trim();
        return if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        };
    }
    None
}

/// Truncate by Unicode scalar count (not bytes) so we never split a codepoint.
fn truncate_chars_prefix(s: &str, max_chars: usize) -> String {
    let n = s.chars().count();
    if n <= max_chars {
        s.to_string()
    } else {
        s.chars().take(max_chars).collect()
    }
}

/// Build a Memoria semantic-retrieval query from recent user text and tool activity.
///
/// [`compact_with_memoria`] uses this so retrieval is biased toward the current task
/// instead of a fixed generic phrase. Session scoping still comes from `session_id`
/// on the HTTP request.
#[must_use]
pub fn memoria_compact_retrieve_query(messages: &[Value]) -> String {
    const MAX_USER_CHARS: usize = 400;
    const MAX_TOOL_NAMES: usize = 12;
    const LOOKBACK_MESSAGES: usize = 48;

    if messages.is_empty() {
        return MEMORIA_RETRIEVE_QUERY_FALLBACK.to_string();
    }

    let start = messages.len().saturating_sub(LOOKBACK_MESSAGES);
    let window = &messages[start..];

    let user_focus = window
        .iter()
        .rev()
        .find_map(message_user_text)
        .map(|s| collapse_whitespace(&truncate_chars_prefix(&s, MAX_USER_CHARS)));

    let mut tool_names: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in window.iter().rev() {
        if m.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(arr) = m.get("tool_calls").and_then(Value::as_array) else {
            continue;
        };
        for tc in arr {
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if name.is_empty() {
                continue;
            }
            if seen.insert(name.to_string()) {
                tool_names.push(name.to_string());
            }
            if tool_names.len() >= MAX_TOOL_NAMES {
                break;
            }
        }
        if tool_names.len() >= MAX_TOOL_NAMES {
            break;
        }
    }
    tool_names.reverse();

    let mut parts: Vec<String> = Vec::new();
    parts.push("current session memory".to_string());
    if let Some(u) = user_focus.filter(|s| !s.is_empty()) {
        parts.push(format!("current user task: {u}"));
    }
    if !tool_names.is_empty() {
        parts.push(format!("recent tools: {}", tool_names.join(", ")));
    }

    if parts.len() == 1 {
        return MEMORIA_RETRIEVE_QUERY_FALLBACK.to_string();
    }
    parts.join(". ") + "."
}

/// Compact messages using Memoria for session context.
///
/// Flow:
/// 1. If below threshold, skip (return original)
/// 2. Retrieve the canonical working-memory snapshot for the session
/// 3. Apply tier-based truncation
/// 4. Optionally generate a typed runtime summary for this request
pub async fn compact_with_memoria(
    messages: &[Value],
    session_id: Option<&str>,
    config: &MemoriaCompactConfig,
    params: &MemoriaCompactParams,
    client: Option<&dyn MemoriaPort>,
    compact_config: Option<&CompactConfig>,
    summary_client: Option<&dyn SummaryLlmClient>,
) -> CompactResult {
    // Check if we should attempt Memoria retrieval
    let should_retrieve = params.current_tokens >= config.min_tokens_for_retrieval
        && params.tier != CompactionTier::Normal
        && client.is_some()
        && session_id.is_some();

    if !should_retrieve {
        // Fall back to pure truncation
        astra_core::history_work::record_serialized_value(
            astra_core::history_work::HistoryWorkSite::CompactionHistoryClone,
            messages,
        );
        let mut msgs = messages.to_vec();
        return CompactionEngine::compact_tiered(
            &mut msgs,
            params.budget_chars,
            params.keep_chars,
            params.tier,
            params.keep_recent_turns,
        );
    }

    let Some(client) = client else {
        astra_core::history_work::record_serialized_value(
            astra_core::history_work::HistoryWorkSite::CompactionHistoryClone,
            messages,
        );
        let mut msgs = messages.to_vec();
        return CompactionEngine::compact_tiered(
            &mut msgs,
            params.budget_chars,
            params.keep_chars,
            params.tier,
            params.keep_recent_turns,
        );
    };
    let Some(sid) = session_id else {
        astra_core::history_work::record_serialized_value(
            astra_core::history_work::HistoryWorkSite::CompactionHistoryClone,
            messages,
        );
        let mut msgs = messages.to_vec();
        return CompactionEngine::compact_tiered(
            &mut msgs,
            params.budget_chars,
            params.keep_chars,
            params.tier,
            params.keep_recent_turns,
        );
    };

    // Step 1: Retrieve session context from Memoria (strict session scope).
    let query = memoria_compact_retrieve_query(messages);
    let memories = match client
        .retrieve_scoped_typed(&query, sid, config.max_memories, &["working"])
        .await
    {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[compact] Memoria retrieve failed: {e}");
            Vec::new()
        }
    };

    let will_summarize = compact_config
        .zip(summary_client.as_ref())
        .is_some_and(|(cfg, _)| cfg.should_summarize(params.tier));
    let summary_token_budget = compact_config.map(|c| c.summary_token_budget).unwrap_or(0);

    let summary_reserve_chars =
        plan_summary_reservation(params.budget_chars, will_summarize, summary_token_budget);

    let session_memory_context =
        build_session_memory_context(&memories, sid, params.tier, params.session_facts.as_ref());
    let session_memory_chars = session_memory_context
        .as_ref()
        .map(|text| text.chars().count())
        .unwrap_or(0);
    let retrieved_memory_entries =
        build_retrieved_working_memory_entries(&memories, sid, config.max_memory_tokens);
    let retrieved_memory_chars = retrieved_memory_entries
        .iter()
        .map(|entry| {
            usize::try_from(entry.token_estimate)
                .unwrap_or(usize::MAX)
                .saturating_mul(4)
        })
        .fold(0_usize, usize::saturating_add);

    let adjusted_budget_chars = adjusted_message_budget_chars(
        params.budget_chars,
        session_memory_chars.saturating_add(retrieved_memory_chars),
        summary_reserve_chars,
    );

    // Apply truncation against a budget that leaves room for typed runtime
    // context. Retrieval results are never inserted into history messages.
    astra_core::history_work::record_serialized_value(
        astra_core::history_work::HistoryWorkSite::CompactionHistoryClone,
        messages,
    );
    let mut msgs = messages.to_vec();
    let mut result = CompactionEngine::compact_tiered(
        &mut msgs,
        adjusted_budget_chars,
        params.keep_chars,
        params.tier,
        params.keep_recent_turns,
    );

    result.session_memory_context = session_memory_context;
    result.retrieved_memory_entries = retrieved_memory_entries;

    // Step 5: Optionally generate an LLM summary. Session working memory is
    // owned by `session_memory::runner`; compaction must not create a second
    // raw-message-derived `working` format.
    if let Some(cfg) = compact_config
        && let Some(s_client) = summary_client
        && cfg.should_summarize(params.tier)
    {
        match astra_turn_core::cloud_summary::generate_compact_summary(messages, s_client).await {
            Some(summary) => {
                let summary = truncate_summary_for_budget(summary, cfg.summary_token_budget);
                result
                    .runtime_contexts
                    .push(format!("## Compacted Conversation Summary\n{summary}"));

                if let Some(ref mut boundary) = result.boundary {
                    boundary.summary = Some(format!(
                        "{}\nLLM summary generated",
                        boundary.summary.as_deref().unwrap_or("")
                    ));
                }

                eprintln!(
                    "[compact] LLM summary generated ({} chars, budget {} tok)",
                    summary.len(),
                    cfg.summary_token_budget
                );
            }
            None => {
                eprintln!("[compact] LLM summary failed, using truncation only");
            }
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn capture_one_http_request(
        status: &str,
        response_body: &'static [u8],
    ) -> (
        std::net::SocketAddr,
        Arc<Mutex<String>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let captured = Arc::new(Mutex::new(String::new()));
        let captured_for_server = Arc::clone(&captured);
        let status = status.to_string();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let read = socket.read(&mut chunk).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = std::str::from_utf8(&request[..header_end]).unwrap_or_default();
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length: ")
                            .or_else(|| line.strip_prefix("Content-Length: "))
                    })
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or_default();
                if request.len().saturating_sub(header_end + 4) >= content_length {
                    break;
                }
            }
            *captured_for_server.lock().unwrap() = String::from_utf8(request).unwrap();
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response_body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.write_all(response_body).await.unwrap();
        });
        (address, captured, server)
    }

    #[test]
    fn long_term_episode_and_scene_writers_emit_recallable_layered_protocol() {
        let episode = encode_episode_memory(
            "session-9",
            "Goal: make memory lifecycle explicit\nOutcome: typed recall is wired",
        );
        let episode = astra_prompts::memory_proto::MemoryEntry::parse(&episode)
            .expect("typed episodic entry");
        assert_eq!(episode.ns, astra_prompts::memory_proto::NS_EPISODE);
        assert_eq!(episode.status, astra_prompts::memory_proto::ST_SUMMARY);
        assert!(
            (astra_prompts::memory_proto::ABSTRACT_MIN_CHARS
                ..=astra_prompts::memory_proto::ABSTRACT_MAX_CHARS)
                .contains(&episode.abstract_layer().chars().count())
        );
        assert!(
            episode
                .overview_layer()
                .expect("overview")
                .contains("Outcome:")
        );
        assert!(astra_prompts::memory_proto::is_prompt_recallable_status(
            &episode.status
        ));

        let scene = encode_scene_memory(
            "testing",
            "- Behavior tests repeatedly caught cross-process identity loss",
        );
        let scene =
            astra_prompts::memory_proto::MemoryEntry::parse(&scene).expect("typed semantic scene");
        assert_eq!(scene.ns, astra_prompts::memory_proto::NS_INSIGHT);
        assert_eq!(scene.status, astra_prompts::memory_proto::ST_AUTO);
        assert!(
            scene
                .overview_layer()
                .expect("overview")
                .contains("identity loss")
        );
    }

    #[test]
    fn parse_retrieved_memories_skips_and_reports_malformed_entries() {
        let data = json!({
            "memories": [
                {
                    "memory_id": "m1",
                    "content": "working memory",
                    "memory_type": "working",
                    "retrieval_score": 0.9
                },
                {
                    "memory_id": "m2",
                    "content": "missing type"
                }
            ]
        });

        let memories = parse_retrieved_memories(&data);
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].memory_id, "m1");
    }

    #[test]
    fn strict_retrieved_memories_reject_every_unproven_or_malformed_item() {
        let scope = astra_memoria::MemoryScope::new("user-1", "session-1").unwrap();
        for data in [
            json!({"memories": [{
                "memory_id": "foreign",
                "content": "must not surface",
                "memory_type": "working",
                "user_id": "user-2",
                "session_id": "session-1"
            }]}),
            json!({"memories": [{
                "memory_id": "malformed",
                "content": "must not surface",
                "user_id": "user-1",
                "session_id": "session-1"
            }]}),
        ] {
            let error = parse_strict_retrieved_memories(&data, &scope)
                .expect_err("strict retrieve must fail as one atomic response");
            assert!(!error.contains("must not surface"));
        }
    }

    struct MockMemoriaPort {
        memories: Mutex<Vec<MemoriaMemory>>,
    }

    impl MockMemoriaPort {
        fn new(memories: Vec<MemoriaMemory>) -> Self {
            Self {
                memories: Mutex::new(memories),
            }
        }
    }

    #[async_trait::async_trait]
    impl MemoriaPort for MockMemoriaPort {
        async fn retrieve_ext(
            &self,
            _query: &str,
            _session_id: Option<&str>,
            _top_k: usize,
            _filter_session: bool,
        ) -> Result<Vec<MemoriaMemory>, String> {
            Ok(self.memories.lock().unwrap().clone())
        }

        async fn store(
            &self,
            _content: &str,
            _memory_type: &str,
            _session_id: Option<&str>,
            _trust_tier: Option<&str>,
        ) -> Result<String, String> {
            Ok("mem_123".to_string())
        }

        async fn purge_working(&self, _session_id: &str) -> Result<u64, String> {
            Ok(0)
        }

        async fn delete(&self, _memory_id: &str) -> Result<(), String> {
            Ok(())
        }
    }

    fn user(content: &str) -> Value {
        json!({"role": "user", "content": content})
    }

    fn assistant(content: &str) -> Value {
        json!({"role": "assistant", "content": content})
    }

    #[test]
    fn retrieve_query_empty_messages_is_fallback() {
        let q = memoria_compact_retrieve_query(&[]);
        assert_eq!(q, MEMORIA_RETRIEVE_QUERY_FALLBACK);
    }

    #[test]
    fn retrieve_query_includes_last_user_and_tools() {
        let tc = json!([{
            "id": "1",
            "type": "function",
            "function": {"name": "read_file", "arguments": "{}"}
        }]);
        let msgs = vec![
            user("older ask"),
            assistant("ok"),
            json!({"role": "assistant", "content": "", "tool_calls": tc}),
            user("  fix the OAuth bug in handler  "),
        ];
        let q = memoria_compact_retrieve_query(&msgs);
        assert!(
            q.contains("fix the OAuth bug"),
            "query should carry latest user focus: {q}"
        );
        assert!(
            q.contains("read_file"),
            "query should mention recent tools: {q}"
        );
        assert!(q.contains("current session memory"));
    }

    #[test]
    fn retrieve_query_dedupes_tool_names() {
        let tc = json!([
            {"id": "1", "type": "function", "function": {"name": "bash", "arguments": "{}"}},
            {"id": "2", "type": "function", "function": {"name": "bash", "arguments": "{}"}}
        ]);
        let msgs = vec![json!({"role": "assistant", "content": "", "tool_calls": tc})];
        let q = memoria_compact_retrieve_query(&msgs);
        assert_eq!(q.matches("bash").count(), 1, "dedupe tool names: {q}");
    }

    #[test]
    fn retrieve_query_fallback_when_no_user_and_no_tools() {
        let msgs = vec![
            json!({"role": "system", "content": "you are a bot"}),
            json!({"role": "assistant", "content": "hello"}),
        ];
        let q = memoria_compact_retrieve_query(&msgs);
        assert_eq!(q, MEMORIA_RETRIEVE_QUERY_FALLBACK);
    }

    #[test]
    fn plan_summary_reserves_space_without_a_memory_injection_budget() {
        let budget = 100_000_usize;
        let sum_res = plan_summary_reservation(budget, true, 20_000);
        assert!(sum_res > 0, "summary branch should reserve chars");
        let adj = adjusted_message_budget_chars(budget, 5_000, sum_res);
        assert!(
            adj < budget,
            "message budget should shrink after reservations: {adj} < {budget}"
        );
    }

    #[test]
    fn plan_summary_without_summary_reserves_nothing() {
        assert_eq!(plan_summary_reservation(50_000, false, 0), 0);
    }

    #[test]
    fn summary_reserve_capped_to_pct_of_total_budget() {
        let budget = 10_000_usize;
        let sum_res = plan_summary_reservation(budget, true, 500_000);
        let max_allowed = budget * 40 / 100;
        assert!(
            sum_res <= max_allowed,
            "summary reserve {sum_res} should be <= {max_allowed}"
        );
    }

    #[test]
    fn truncate_summary_for_budget_enforces_char_cap() {
        let s = "x".repeat(5000);
        let out = truncate_summary_for_budget(s, 100);
        assert!(out.contains("truncated for context budget"));
        assert!(out.chars().count() < 5000);
    }

    #[test]
    fn build_session_memory_context_decodes_typed_session_entry() {
        let memories = vec![MemoriaMemory {
            memory_id: "m1".to_string(),
            content: crate::session_memory::runner::encode_session_memory_entry(
                "sess-1",
                "## Active Goals\n- Fix memory\n",
            ),
            memory_type: "working".to_string(),
            session_id: Some("sess-1".to_string()),
            ..Default::default()
        }];
        let session_ctx =
            build_session_memory_context(&memories, "sess-1", CompactionTier::CompactHistory, None)
                .expect("session memory context");
        assert!(session_ctx.contains("## Session State"));
        assert!(session_ctx.contains("Fix memory"));
        assert!(!session_ctx.contains(crate::session_memory::runner::SESSION_MEMORY_PREFIX));
    }

    #[test]
    fn build_session_memory_context_uses_first_ranked_snapshot() {
        let memories = vec![
            MemoriaMemory {
                memory_id: "m1".to_string(),
                content: crate::session_memory::runner::encode_session_memory_entry(
                    "sess-1",
                    "## Active Goals\n- Fix memory\n",
                ),
                memory_type: "working".to_string(),
                session_id: Some("sess-1".to_string()),
                user_id: None,
                retrieval_score: Some(0.9),
                ..Default::default()
            },
            MemoriaMemory {
                memory_id: "m2".to_string(),
                content: crate::session_memory::runner::encode_session_memory_entry(
                    "sess-1",
                    "## Active Goals\n- Stale duplicate\n",
                ),
                memory_type: "working".to_string(),
                session_id: Some("sess-1".to_string()),
                retrieval_score: Some(0.8),
                ..Default::default()
            },
        ];
        let session_ctx =
            build_session_memory_context(&memories, "sess-1", CompactionTier::CompactHistory, None)
                .expect("session memory context");
        assert!(session_ctx.contains("Fix memory"));
        assert!(!session_ctx.contains("Stale duplicate"));
    }

    #[test]
    fn build_session_memory_context_uses_overview_only_for_aggressive_prune() {
        let memories = vec![MemoriaMemory {
            memory_id: "m1".to_string(),
            content: crate::session_memory::runner::encode_session_memory_entry(
                "sess-1",
                "## Current State\n- implementing pipeline source\n\n## Pending Todos\n- rerun binder\n",
            ),
            memory_type: "working".to_string(),
            session_id: Some("sess-1".to_string()),
            ..Default::default()
        }];

        let compact =
            build_session_memory_context(&memories, "sess-1", CompactionTier::CompactHistory, None)
                .expect("compact session memory");
        assert!(compact.contains("Latest state"));
        assert!(
            !compact.contains("Open loops:"),
            "compact mode should avoid overview text"
        );

        let overview = build_session_memory_context(
            &memories,
            "sess-1",
            CompactionTier::AggressivePrune,
            None,
        )
        .expect("overview session memory");
        assert!(
            overview.contains("Open loops:"),
            "aggressive prune should carry overview"
        );
    }

    #[test]
    fn retrieved_working_memory_entries_exclude_snapshot_and_preserve_identity() {
        let memories = vec![
            MemoriaMemory {
                memory_id: "snapshot-1".to_string(),
                content: crate::session_memory::runner::encode_session_memory_entry(
                    "sess-1",
                    "## Current State\n- canonical snapshot\n",
                ),
                memory_type: "working".to_string(),
                session_id: Some("sess-1".to_string()),
                retrieval_score: Some(1.0),
                ..Default::default()
            },
            MemoriaMemory {
                memory_id: "working-2".to_string(),
                content: "Tool migration reached the server bridge; CLI remains.".to_string(),
                memory_type: "working".to_string(),
                session_id: Some("sess-1".to_string()),
                retrieval_score: Some(0.82),
                ..Default::default()
            },
        ];

        let entries = build_retrieved_working_memory_entries(&memories, "sess-1", 1_000);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].memory_id.as_deref(), Some("working-2"));
        assert_eq!(entries[0].memory_type.as_deref(), Some("working"));
        assert_eq!(
            entries[0].source.as_deref(),
            Some("memoria.compaction_working")
        );
        assert!(entries[0].content.contains("server bridge"));
        assert!(!entries[0].content.contains("canonical snapshot"));
    }

    #[tokio::test]
    async fn compact_without_client_falls_back() {
        let msgs = vec![user("hello"), assistant("hi")];
        let config = MemoriaCompactConfig::default();
        let params = MemoriaCompactParams {
            budget_chars: 10000,
            keep_chars: 2000,
            tier: CompactionTier::Normal,
            keep_recent_turns: 4,
            current_tokens: 1000,
            session_facts: None,
        };
        let result = compact_with_memoria(
            &msgs,
            Some("sess1"),
            &config,
            &params,
            None, // No client
            None, // No compact config
            None, // No summary client
        )
        .await;

        assert_eq!(result.messages.len(), 2);
    }

    #[tokio::test]
    async fn compact_below_threshold_skips_retrieval() {
        let msgs = vec![user("hello"), assistant("hi")];
        let config = MemoriaCompactConfig {
            min_tokens_for_retrieval: 10_000,
            ..Default::default()
        };
        let mock = MockMemoriaPort::new(vec![]);
        let params = MemoriaCompactParams {
            budget_chars: 10000,
            keep_chars: 2000,
            tier: CompactionTier::TrimSchemas,
            keep_recent_turns: 4,
            current_tokens: 1000, // Below threshold
            session_facts: None,
        };

        let result = compact_with_memoria(
            &msgs,
            Some("sess1"),
            &config,
            &params,
            Some(&mock),
            None,
            None,
        )
        .await;

        assert_eq!(result.messages.len(), 2);
    }

    #[tokio::test]
    async fn compact_returns_session_memory_as_typed_context_without_mutating_history() {
        let msgs = vec![
            user("implement OAuth"),
            assistant("I'll help with OAuth"),
            user("use JWT"),
        ];
        let config = MemoriaCompactConfig {
            min_tokens_for_retrieval: 100,
            ..Default::default()
        };
        let mock = MockMemoriaPort::new(vec![MemoriaMemory {
            memory_id: "m1".to_string(),
            content: crate::session_memory::runner::encode_session_memory_entry(
                "sess1",
                "## Current State\n- Working on auth module\n",
            ),
            memory_type: "working".to_string(),
            session_id: Some("sess1".to_string()),
            retrieval_score: Some(0.8),
            ..Default::default()
        }]);
        let params = MemoriaCompactParams {
            budget_chars: 10000,
            keep_chars: 2000,
            tier: CompactionTier::CompactHistory,
            keep_recent_turns: 4,
            current_tokens: 6000, // Above threshold
            session_facts: None,
        };

        let result = compact_with_memoria(
            &msgs,
            Some("sess1"),
            &config,
            &params,
            Some(&mock),
            None,
            None,
        )
        .await;

        assert_eq!(
            result.messages, msgs,
            "history must remain real messages only"
        );
        let ctx_content = result
            .session_memory_context
            .as_deref()
            .expect("typed session context");
        assert!(
            ctx_content.contains("Working on auth module"),
            "typed context should carry the snapshot, got: {ctx_content}"
        );
        assert!(result.retrieved_memory_entries.is_empty());
    }

    #[tokio::test]
    async fn compact_preserves_non_snapshot_working_memory_in_typed_dynamic_lane() {
        let msgs = vec![user("finish the migration"), assistant("continuing")];
        let config = MemoriaCompactConfig {
            min_tokens_for_retrieval: 100,
            ..Default::default()
        };
        let mock = MockMemoriaPort::new(vec![MemoriaMemory {
            memory_id: "working-1".to_string(),
            content: "CLI route is complete; server route still needs verification.".to_string(),
            memory_type: "working".to_string(),
            session_id: Some("sess1".to_string()),
            retrieval_score: Some(0.9),
            ..Default::default()
        }]);
        let params = MemoriaCompactParams {
            budget_chars: 10_000,
            keep_chars: 2_000,
            tier: CompactionTier::CompactHistory,
            keep_recent_turns: 4,
            current_tokens: 6_000,
            session_facts: None,
        };

        let result = compact_with_memoria(
            &msgs,
            Some("sess1"),
            &config,
            &params,
            Some(&mock),
            None,
            None,
        )
        .await;

        assert_eq!(result.messages, msgs);
        assert!(result.session_memory_context.is_none());
        assert_eq!(result.retrieved_memory_entries.len(), 1);
        assert_eq!(
            result.retrieved_memory_entries[0].memory_id.as_deref(),
            Some("working-1")
        );
        assert!(
            result.retrieved_memory_entries[0]
                .content
                .contains("server route still needs verification")
        );
    }

    // ── Summary integration tests ────────────────────────────────────────────

    struct MockSummaryClient {
        response: Mutex<Option<String>>,
    }

    impl MockSummaryClient {
        fn success(text: &str) -> Self {
            Self {
                response: Mutex::new(Some(text.to_string())),
            }
        }

        fn failure() -> Self {
            Self {
                response: Mutex::new(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl astra_turn_core::cloud_summary::SummaryLlmClient for MockSummaryClient {
        async fn summarize(
            &self,
            _purpose: astra_turn_types::InferencePurpose,
            _messages: &[Value],
        ) -> Result<astra_turn_core::cloud_summary::SummaryResponse, String> {
            match self.response.lock().unwrap().as_ref() {
                Some(text) => Ok(astra_turn_core::cloud_summary::SummaryResponse {
                    text: text.clone(),
                    is_ptl_error: false,
                }),
                None => Err("mock failure".to_string()),
            }
        }
    }

    #[tokio::test]
    async fn compact_with_summary_when_enabled() {
        let msgs = vec![
            user("implement OAuth"),
            assistant("I'll help with OAuth. Here's a plan..."),
            user("use JWT instead"),
            assistant("Sure, switching to JWT tokens for auth."),
        ];
        let config = MemoriaCompactConfig {
            min_tokens_for_retrieval: 100,
            ..Default::default()
        };
        let mock = MockMemoriaPort::new(vec![MemoriaMemory {
            memory_id: "m1".to_string(),
            content: "Working on auth module".to_string(),
            memory_type: "working".to_string(),
            retrieval_score: Some(0.8),
            ..Default::default()
        }]);
        let params = MemoriaCompactParams {
            budget_chars: 10000,
            keep_chars: 2000,
            tier: CompactionTier::AggressivePrune, // Meets summary_min_tier
            keep_recent_turns: 4,
            current_tokens: 6000,
            session_facts: None,
        };

        let compact_config = CompactConfig {
            enable_summary: true,
            summary_min_tier: CompactionTier::AggressivePrune,
            ..Default::default()
        };
        let summary_client =
            MockSummaryClient::success("User discussed OAuth then switched to JWT auth.");

        let result = compact_with_memoria(
            &msgs,
            Some("sess1"),
            &config,
            &params,
            Some(&mock),
            Some(&compact_config),
            Some(&summary_client as &dyn astra_turn_core::cloud_summary::SummaryLlmClient),
        )
        .await;

        assert_eq!(result.messages, msgs, "summary must not become history");
        assert_eq!(result.runtime_contexts.len(), 1);
        assert!(result.runtime_contexts[0].contains("switched to JWT auth"));
        assert!(
            result.session_memory_context.is_none(),
            "raw legacy working text is not canonical session memory"
        );
    }

    #[tokio::test]
    async fn compact_summary_disabled_skips_llm() {
        let msgs = vec![user("hello"), assistant("hi")];
        let config = MemoriaCompactConfig {
            min_tokens_for_retrieval: 100,
            ..Default::default()
        };
        let mock = MockMemoriaPort::new(vec![]);
        let params = MemoriaCompactParams {
            budget_chars: 10000,
            keep_chars: 2000,
            tier: CompactionTier::AggressivePrune,
            keep_recent_turns: 4,
            current_tokens: 6000,
            session_facts: None,
        };

        let compact_config = CompactConfig {
            enable_summary: false, // Disabled
            ..Default::default()
        };
        let summary_client = MockSummaryClient::success("should not appear");

        let result = compact_with_memoria(
            &msgs,
            Some("sess1"),
            &config,
            &params,
            Some(&mock),
            Some(&compact_config),
            Some(&summary_client as &dyn astra_turn_core::cloud_summary::SummaryLlmClient),
        )
        .await;

        let has_summary = result.messages.iter().any(|m| {
            m.get("content")
                .and_then(Value::as_str)
                .map(|s| s.contains("[Conversation summary"))
                .unwrap_or(false)
        });
        assert!(
            !has_summary,
            "summary should not be generated when disabled"
        );
    }

    #[tokio::test]
    async fn compact_summary_failure_falls_back() {
        let msgs = vec![user("hello"), assistant("hi")];
        let config = MemoriaCompactConfig {
            min_tokens_for_retrieval: 100,
            ..Default::default()
        };
        let mock = MockMemoriaPort::new(vec![]);
        let params = MemoriaCompactParams {
            budget_chars: 10000,
            keep_chars: 2000,
            tier: CompactionTier::AggressivePrune,
            keep_recent_turns: 4,
            current_tokens: 6000,
            session_facts: None,
        };

        let compact_config = CompactConfig {
            enable_summary: true,
            summary_min_tier: CompactionTier::AggressivePrune,
            ..Default::default()
        };
        let summary_client = MockSummaryClient::failure();

        let result = compact_with_memoria(
            &msgs,
            Some("sess1"),
            &config,
            &params,
            Some(&mock),
            Some(&compact_config),
            Some(&summary_client as &dyn astra_turn_core::cloud_summary::SummaryLlmClient),
        )
        .await;

        // Should still have the original messages (truncated), no summary
        assert_eq!(result.messages.len(), 2);
        let has_summary = result.messages.iter().any(|m| {
            m.get("content")
                .and_then(Value::as_str)
                .map(|s| s.contains("[Conversation summary"))
                .unwrap_or(false)
        });
        assert!(!has_summary, "failed summary should not inject message");
    }

    #[tokio::test]
    async fn compact_summary_below_tier_threshold_skips() {
        let msgs = vec![user("hello"), assistant("hi")];
        let config = MemoriaCompactConfig {
            min_tokens_for_retrieval: 100,
            ..Default::default()
        };
        let mock = MockMemoriaPort::new(vec![]);
        let params = MemoriaCompactParams {
            budget_chars: 10000,
            keep_chars: 2000,
            tier: CompactionTier::TrimSchemas, // Below AggressivePrune threshold
            keep_recent_turns: 4,
            current_tokens: 6000,
            session_facts: None,
        };

        let compact_config = CompactConfig {
            enable_summary: true,
            summary_min_tier: CompactionTier::AggressivePrune, // Requires AggressivePrune
            ..Default::default()
        };
        let summary_client = MockSummaryClient::success("should not appear");

        let result = compact_with_memoria(
            &msgs,
            Some("sess1"),
            &config,
            &params,
            Some(&mock),
            Some(&compact_config),
            Some(&summary_client as &dyn astra_turn_core::cloud_summary::SummaryLlmClient),
        )
        .await;

        let has_summary = result.messages.iter().any(|m| {
            m.get("content")
                .and_then(Value::as_str)
                .map(|s| s.contains("[Conversation summary"))
                .unwrap_or(false)
        });
        assert!(!has_summary, "tier below threshold should skip summary");
    }

    #[test]
    fn sanitize_path_replaces_non_alnum_with_hyphen() {
        assert_eq!(
            sanitize_path_for_claude_projects("/home/user/proj"),
            "-home-user-proj"
        );
    }

    // ──────────────────────────────────────────────────────────
    // collapse_whitespace
    // ──────────────────────────────────────────────────────────

    #[test]
    fn collapse_whitespace_multi_spaces() {
        assert_eq!(collapse_whitespace("a  b   c"), "a b c");
    }

    #[test]
    fn collapse_whitespace_tabs_newlines() {
        assert_eq!(collapse_whitespace("a\t\nb\n\nc"), "a b c");
    }

    #[test]
    fn collapse_whitespace_empty() {
        assert_eq!(collapse_whitespace(""), "");
    }

    #[test]
    fn collapse_whitespace_only_whitespace() {
        assert_eq!(collapse_whitespace("   \t\n  "), "");
    }

    // ──────────────────────────────────────────────────────────
    // message_user_text
    // ──────────────────────────────────────────────────────────

    #[test]
    fn message_user_text_from_user() {
        let m = user("hello world");
        assert_eq!(message_user_text(&m), Some("hello world".into()));
    }

    #[test]
    fn message_user_text_from_assistant() {
        let m = assistant("response");
        assert_eq!(message_user_text(&m), None);
    }

    #[test]
    fn message_user_text_empty_content() {
        let m = json!({"role": "user", "content": ""});
        assert_eq!(message_user_text(&m), None);
    }

    #[test]
    fn message_user_text_whitespace_only() {
        let m = json!({"role": "user", "content": "   \n  "});
        assert_eq!(message_user_text(&m), None);
    }

    #[test]
    fn message_user_text_null_content() {
        let m = json!({"role": "user", "content": null});
        assert_eq!(message_user_text(&m), None);
    }

    #[test]
    fn message_user_text_missing_content() {
        let m = json!({"role": "user"});
        assert_eq!(message_user_text(&m), None);
    }

    // ──────────────────────────────────────────────────────────
    // truncate_chars_prefix
    // ──────────────────────────────────────────────────────────

    #[test]
    fn truncate_chars_prefix_within_limit() {
        assert_eq!(truncate_chars_prefix("abc", 10), "abc");
    }

    #[test]
    fn truncate_chars_prefix_exceeds() {
        assert_eq!(truncate_chars_prefix("abcdef", 3), "abc");
    }

    #[test]
    fn truncate_chars_prefix_unicode() {
        assert_eq!(truncate_chars_prefix("αβγδ", 2), "αβ");
    }

    #[test]
    fn truncate_chars_prefix_empty() {
        assert_eq!(truncate_chars_prefix("", 5), "");
    }

    // ──────────────────────────────────────────────────────────
    // adjusted_message_budget_chars
    // ──────────────────────────────────────────────────────────

    #[test]
    fn adjusted_budget_basic() {
        assert_eq!(adjusted_message_budget_chars(1000, 200, 100), 700);
    }

    #[test]
    fn adjusted_budget_underflow_saturates() {
        assert_eq!(adjusted_message_budget_chars(100, 500, 500), 0);
    }

    #[test]
    fn adjusted_budget_zero_deductions() {
        assert_eq!(adjusted_message_budget_chars(1000, 0, 0), 1000);
    }

    // ──────────────────────────────────────────────────────────
    // truncate_summary_for_budget
    // ──────────────────────────────────────────────────────────

    #[test]
    fn truncate_summary_within_budget() {
        let s = "short summary".to_string();
        assert_eq!(truncate_summary_for_budget(s.clone(), 100), s);
    }

    #[test]
    fn truncate_summary_over_budget() {
        let s = "a".repeat(2000);
        let r = truncate_summary_for_budget(s, 1); // 1 token ≈ 4 chars, min 256
        assert!(r.contains("[summary truncated"));
    }

    /// P3 regression: `purge_working` must send `session_id` +
    /// `memory_types=["working"]` to Memoria's v1 purge endpoint, NOT a
    /// topic-based fulltext query. Verified by running a one-shot TCP
    /// mock that captures the request body and echoes a response.
    #[tokio::test]
    async fn purge_working_sends_session_id_selector_not_topic() {
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = Arc::new(std::sync::Mutex::new(String::new()));
        let captured_cl = captured.clone();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            // Read until we see \r\n\r\n then the declared Content-Length.
            loop {
                let n = sock.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(idx) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&buf[..idx]).unwrap_or("");
                    let cl: usize = headers
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("content-length: ")
                                .or_else(|| l.strip_prefix("Content-Length: "))
                        })
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    let body_so_far = buf.len() - idx - 4;
                    if body_so_far >= cl {
                        break;
                    }
                }
            }
            *captured_cl.lock().unwrap() = String::from_utf8(buf).unwrap();
            let payload = b"{\"purged\": 3}";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.write_all(payload).await;
            let _ = sock.shutdown().await;
        });

        let client = HttpMemoriaPort::new(format!("http://{addr}"), "test-key".to_string())
            .with_owner_user_id("user-7");
        let purged = client
            .purge_working("8ae95566-f123-4abc-9def-0123456789ab")
            .await
            .expect("purge ok");
        assert_eq!(purged, 3, "must parse `purged` from response");
        server.await.unwrap();

        let raw = captured.lock().unwrap().clone();
        let (headers, body) = raw.split_once("\r\n\r\n").unwrap();
        assert!(headers.starts_with("POST /v1/memories/purge HTTP/1.1"));
        assert!(
            headers
                .lines()
                .any(|line| line.eq_ignore_ascii_case("x-user-id: user-7")),
            "purge must be routed to the bound owner: {headers}"
        );
        let json: serde_json::Value = serde_json::from_str(body)
            .unwrap_or_else(|e| panic!("body parse fail: {e}, body=<{body}>"));
        assert_eq!(
            json.get("session_id").and_then(Value::as_str),
            Some("8ae95566-f123-4abc-9def-0123456789ab"),
            "session_id must be forwarded exactly"
        );
        let types = json
            .get("memory_types")
            .and_then(Value::as_array)
            .expect("memory_types array");
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].as_str(), Some("working"));
        assert!(
            json.get("topic").is_none(),
            "must NOT send topic-based selector (ngram doesn't match UUIDs)"
        );
        assert!(
            json.get("user_id").is_none(),
            "transport identity must not be duplicated in the domain payload"
        );
    }

    #[tokio::test]
    async fn purge_working_rejects_empty_session_id() {
        let client = HttpMemoriaPort::new("http://127.0.0.1:1".into(), "key".into());
        let err = client.purge_working("").await.unwrap_err();
        assert!(
            err.contains("non-empty"),
            "expected validation error: {err}"
        );
    }

    #[tokio::test]
    async fn delete_uses_owner_scoped_purge_instead_of_master_delete() {
        let (address, captured, server) =
            capture_one_http_request("200 OK", br#"{"purged":1}"#).await;
        let client =
            HttpMemoriaPort::new_master(format!("http://{address}"), "master-key".to_string())
                .with_owner_user_id("owner-7");

        client.delete("memory-42").await.expect("scoped delete");
        server.await.unwrap();

        let raw = captured.lock().unwrap().clone();
        let (headers, body) = raw.split_once("\r\n\r\n").unwrap();
        assert!(headers.starts_with("POST /v1/memories/purge HTTP/1.1"));
        assert!(
            headers
                .lines()
                .any(|line| line.eq_ignore_ascii_case("x-user-id: owner-7"))
        );
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["memory_ids"], json!(["memory-42"]));
        assert!(body.get("user_id").is_none());
    }

    #[tokio::test]
    async fn episode_store_matches_memoria_schema_and_owner_routing() {
        let response = br#"{"memory_id":"episode-1"}"#;
        let (address, captured, server) = capture_one_http_request("201 Created", response).await;
        let client =
            HttpMemoriaPort::new_master(format!("http://{address}"), "master-key".to_string())
                .with_owner_user_id("owner-7");

        let id = client
            .store_episode("session-9", "completed the migration")
            .await
            .expect("episode store");
        assert_eq!(id, "episode-1");
        server.await.unwrap();

        let raw = captured.lock().unwrap().clone();
        let (headers, body) = raw.split_once("\r\n\r\n").unwrap();
        assert!(headers.starts_with("POST /v1/memories HTTP/1.1"));
        assert!(
            headers
                .lines()
                .any(|line| line.eq_ignore_ascii_case("x-user-id: owner-7"))
        );
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["memory_type"], "episodic");
        assert_eq!(body["session_id"], "session-9");
        assert_eq!(body["source"], "astra:session_end_orchestrator");
        assert!(body["source"].is_string());
        assert!(body.get("user_id").is_none());
        assert!(body.get("tags").is_none());
    }

    #[tokio::test]
    async fn default_retrieve_scoped_typed_filters_memory_types_locally() {
        let client = MockMemoriaPort::new(vec![
            MemoriaMemory {
                memory_id: "working-1".to_string(),
                content: "keep".to_string(),
                memory_type: "working".to_string(),
                session_id: Some("sess-1".to_string()),
                user_id: None,
                retrieval_score: None,
                observed_at: None,
                updated_at: None,
                trust_tier: None,
            },
            MemoriaMemory {
                memory_id: "reference-1".to_string(),
                content: "drop".to_string(),
                memory_type: "reference".to_string(),
                session_id: Some("sess-1".to_string()),
                user_id: None,
                retrieval_score: None,
                observed_at: None,
                updated_at: None,
                trust_tier: None,
            },
            MemoriaMemory {
                memory_id: "untyped-1".to_string(),
                content: "drop".to_string(),
                memory_type: String::new(),
                session_id: Some("sess-1".to_string()),
                user_id: None,
                retrieval_score: None,
                observed_at: None,
                updated_at: None,
                trust_tier: None,
            },
        ]);

        let memories = client
            .retrieve_scoped_typed("session memory", "sess-1", 10, &["working"])
            .await
            .expect("typed retrieve");

        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].memory_id, "working-1");
    }

    #[tokio::test]
    async fn prompt_retrieve_preserves_distinct_user_and_session_identity_on_wire() {
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = Arc::new(std::sync::Mutex::new(String::new()));
        let captured_for_server = Arc::clone(&captured);
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let read = socket.read(&mut chunk).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = std::str::from_utf8(&request[..header_end]).unwrap_or_default();
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length: ")
                            .or_else(|| line.strip_prefix("Content-Length: "))
                    })
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or_default();
                if request.len().saturating_sub(header_end + 4) >= content_length {
                    break;
                }
            }
            *captured_for_server.lock().unwrap() = String::from_utf8(request).unwrap();
            let payload = b"{\"memories\":[]}";
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            socket.write_all(headers.as_bytes()).await.unwrap();
            socket.write_all(payload).await.unwrap();
        });

        let client = HttpMemoriaPort::new(format!("http://{addr}"), "test-key".into());
        let memories = client
            .retrieve_for_prompt("typed recall", "user-7", "session-9", 6)
            .await
            .expect("prompt retrieve");
        assert!(memories.is_empty());
        server.await.unwrap();

        let raw = captured.lock().unwrap().clone();
        let (headers, body) = raw.split_once("\r\n\r\n").unwrap();
        assert!(headers.starts_with("POST /v1/memories/retrieve HTTP/1.1"));
        assert!(
            headers
                .lines()
                .any(|line| line.eq_ignore_ascii_case("x-user-id: user-7")),
            "authenticated owner must be projected into the routing header: {headers}"
        );
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["query"], "typed recall");
        assert_eq!(body["top_k"], 6);
        assert!(body.get("user_id").is_none());
        assert_eq!(body["session_id"], "session-9");
        assert!(body.get("session_scope").is_none());
    }

    #[tokio::test]
    async fn retrieve_scoped_typed_sends_memory_types_filter() {
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = Arc::new(std::sync::Mutex::new(String::new()));
        let captured_cl = captured.clone();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                let n = sock.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(idx) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&buf[..idx]).unwrap_or("");
                    let cl: usize = headers
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("content-length: ")
                                .or_else(|| l.strip_prefix("Content-Length: "))
                        })
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    let body_so_far = buf.len() - idx - 4;
                    if body_so_far >= cl {
                        break;
                    }
                }
            }
            *captured_cl.lock().unwrap() = String::from_utf8(buf).unwrap();
            let payload = br#"{"items":[{"memory_id":"working-1","content":"snapshot","memory_type":"working","user_id":"user-7","session_id":"8ae95566-f123-4abc-9def-0123456789ab"}],"next_cursor":null}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.write_all(payload).await;
            let _ = sock.shutdown().await;
        });

        let client = HttpMemoriaPort::new(format!("http://{addr}"), "test-key".to_string())
            .with_owner_user_id("user-7");
        let memories = client
            .retrieve_scoped_typed(
                "session memory",
                "8ae95566-f123-4abc-9def-0123456789ab",
                7,
                &["working"],
            )
            .await
            .expect("typed retrieve ok");
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].memory_id, "working-1");
        server.await.unwrap();

        let raw = captured.lock().unwrap().clone();
        let (headers, body) = raw.split_once("\r\n\r\n").unwrap();
        assert!(body.is_empty(), "typed list must be a GET without a body");
        let request_target = headers.lines().next().unwrap();
        assert!(request_target.starts_with("GET /v1/memories?"));
        assert!(request_target.contains("session_id=8ae95566-f123-4abc-9def-0123456789ab"));
        assert!(request_target.contains("memory_type=working"));
        assert!(request_target.contains("limit=7"));
        assert!(
            headers
                .lines()
                .any(|line| line.eq_ignore_ascii_case("x-user-id: user-7")),
            "typed list must carry authenticated owner routing: {headers}"
        );
    }

    #[tokio::test]
    async fn master_transport_fails_closed_without_owner_and_rejects_owner_mismatch() {
        let unbound = HttpMemoriaPort::new_master("http://127.0.0.1:1".into(), "master-key".into());
        let error = unbound
            .store("content", "working", None, None)
            .await
            .expect_err("master data request without an owner must fail before I/O");
        assert!(error.contains("requires an authenticated owner binding"));

        let invalid = unbound.clone().with_owner_user_id(" owner-a");
        let error = invalid
            .store("content", "working", None, None)
            .await
            .expect_err("invalid bound owner must fail before I/O");
        assert!(error.contains("memory scope user_id"));

        let bound = unbound.with_owner_user_id("owner-a");
        let error = bound
            .retrieve_for_prompt("query", "owner-b", "session-1", 1)
            .await
            .expect_err("call-site owner must not override a bound owner");
        assert!(error.starts_with("memory_scope_violation:"));
    }

    #[tokio::test]
    async fn retrieve_scoped_typed_rejects_foreign_backend_response_without_leaking_content() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let read = socket.read(&mut chunk).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = std::str::from_utf8(&request[..header_end]).unwrap_or_default();
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length: ")
                            .or_else(|| line.strip_prefix("Content-Length: "))
                    })
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or_default();
                if request.len().saturating_sub(header_end + 4) >= content_length {
                    break;
                }
            }
            let payload = br#"{"memories":[{"memory_id":"foreign","content":"private foreign memory","memory_type":"session_memory","user_id":"other-user","session_id":"session-9"}]}"#;
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            socket.write_all(headers.as_bytes()).await.unwrap();
            socket.write_all(payload).await.unwrap();
        });

        let client = HttpMemoriaPort::new(format!("http://{addr}"), "test-key".to_string())
            .with_owner_user_id("user-7");
        let error = client
            .retrieve_scoped_typed("session memory", "session-9", 7, &["session_memory"])
            .await
            .expect_err("foreign response must be rejected");
        server.await.unwrap();

        assert!(error.starts_with("memory_scope_violation:"), "{error}");
        assert!(!error.contains("private foreign memory"));
    }

    #[tokio::test]
    async fn health_check_accepts_success_response() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await.unwrap();
            let payload = b"{\"status\":\"ok\"}";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.write_all(payload).await;
            let _ = sock.shutdown().await;
        });

        let client = HttpMemoriaPort::new(format!("http://{addr}"), "test-key".to_string());
        client.health_check().await.expect("health should pass");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn health_check_reports_non_success_response() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await.unwrap();
            let payload = b"{\"error\":\"unhealthy\"}";
            let response = format!(
                "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.write_all(payload).await;
            let _ = sock.shutdown().await;
        });

        let client = HttpMemoriaPort::new(format!("http://{addr}"), "test-key".to_string());
        let err = client.health_check().await.unwrap_err();
        assert!(err.contains("503"), "expected status in error: {err}");
        server.await.unwrap();
    }

    // ── P7: parse_reflect_candidates ──────────────────────────────────

    #[test]
    fn parse_reflect_candidates_returns_empty_when_no_field() {
        let data = serde_json::json!({"scenes_created": 0});
        assert!(parse_reflect_candidates(&data).is_empty());
    }

    #[test]
    fn parse_reflect_candidates_flattens_memories_into_summary() {
        let data = serde_json::json!({
            "candidates": [
                {
                    "signal": "auth-redirect",
                    "importance": 0.9,
                    "memories": [
                        {"memory_id": "m1", "content": "fixed OAuth callback"},
                        {"memory_id": "m2", "content": "added state param"},
                    ]
                },
                {
                    "signal": "test-flake",
                    "importance": 0.4,
                    "memories": [
                        {"memory_id": "m3", "content": "integration suite timeout"},
                    ]
                }
            ]
        });
        let parsed = parse_reflect_candidates(&data);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].signal, "auth-redirect");
        assert!((parsed[0].importance - 0.9).abs() < 1e-6);
        assert_eq!(
            parsed[0].summary,
            "- fixed OAuth callback\n- added state param"
        );
        assert_eq!(parsed[1].signal, "test-flake");
        assert_eq!(parsed[1].summary, "- integration suite timeout");
    }

    #[test]
    fn parse_reflect_candidates_skips_empty_entries() {
        let data = serde_json::json!({
            "candidates": [
                {"signal": "", "memories": []},
                {"signal": "useful", "memories": [{"content": "x"}]},
            ]
        });
        let parsed = parse_reflect_candidates(&data);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].signal, "useful");
    }

    #[test]
    fn parse_reflect_candidates_trims_and_skips_empty_content() {
        let data = serde_json::json!({
            "candidates": [
                {
                    "signal": "x",
                    "memories": [
                        {"content": "   "},
                        {"content": "real body"},
                        {"content": ""},
                    ]
                }
            ]
        });
        let parsed = parse_reflect_candidates(&data);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].summary, "- real body");
    }
}
