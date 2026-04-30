//! Memoria-based message compaction.
//!
//! Uses Memoria's `working` memory type for session-level context storage,
//! enabling cloud-side compaction without edge→cloud file sync.
//!
//! Architecture:
//! ```text
//! 1. Messages exceed budget threshold
//! 2. Retrieve working memories for session → inject as context summary
//! 3. Truncate old messages (keep recent turns)
//! 4. Optionally store new working memory with updated context
//! ```
//!
//! ## On-disk session memory
//!
//! When `ASTRA_SESSION_MEMORY_COMBINE` is set, the compactor can read
//! `CLAUDE_CONFIG_DIR/projects/<sanitized-cwd>/<session_id>/session-memory/summary.md`
//! or a path from `ASTRA_SESSION_MEMORY_FILE`.
//! - `fallback`: use the file only if Memoria returns no memories.
//! - `merge` / `true` / `1` / `both`: keep Memoria hits and add a capped file excerpt.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::compaction::{
    CompactBoundary, CompactResult, CompactTrigger, compact_tiered_with_result,
};
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
    /// Maximum tokens to include from retrieved memories.
    pub max_memory_tokens: usize,
    /// Whether to store updated working memory after compaction.
    pub store_on_compact: bool,
}

impl Default for MemoriaCompactConfig {
    fn default() -> Self {
        Self {
            min_tokens_for_retrieval: 5_000,
            max_memories: 10,
            max_memory_tokens: 4_000,
            store_on_compact: true,
        }
    }
}

/// How to mix on-disk `summary.md` with Memoria HTTP retrieval.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SessionMemoryFileCombine {
    /// Ignore on-disk session memory for compaction injection.
    #[default]
    None,
    /// Use the file only when Memoria returns no memories.
    Fallback,
    /// Include a capped file excerpt alongside Memoria under the same token budget.
    Merge,
}

impl SessionMemoryFileCombine {
    /// Feature was previously env-gated; always None after cleanup.
    pub fn from_env() -> Self {
        Self::None
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
    /// Optional path to on-disk session memory.
    pub session_memory_file: Option<PathBuf>,
    /// How to combine that file with Memoria retrieval.
    pub session_memory_combine: SessionMemoryFileCombine,
    /// Optional session facts for facts-first compaction (L1a ground truth).
    /// When present, `build_facts_first_injection()` is used as the primary
    /// memory context, with Memoria narrative as supplement.
    pub session_facts: Option<astra_turn_types::session_facts::SessionFacts>,
}

// ---------------------------------------------------------------------------
// Compatible session memory paths
// ---------------------------------------------------------------------------

const CLAUDE_PROJECTS_SANITIZE_MAX_CHARS: usize = 200;
const MAX_SESSION_MEMORY_FILE_BYTES: u64 = 512 * 1024;

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
pub fn claude_code_session_memory_path(cwd: &str, session_id: &str) -> PathBuf {
    claude_config_home_dir()
        .join("projects")
        .join(sanitize_path_for_claude_projects(cwd))
        .join(session_id)
        .join("session-memory")
        .join("summary.md")
}

/// Read a bounded UTF-8 session memory file (whitespace-trimmed).
pub fn read_session_memory_file(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_SESSION_MEMORY_FILE_BYTES {
        return None;
    }
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Resolve the on-disk session memory file for crash/session recovery.
///
/// Unlike [`resolve_session_memory_file_options`], this path selection is
/// Attempt to find an on-disk session memory file. Recovery path reuses existing session-memory
/// summaries derived from the cwd + session_id (no env override after cleanup).
pub fn resolve_resume_session_memory_file(session_id: &str, cwd: Option<&str>) -> Option<PathBuf> {
    let cwd = cwd.filter(|s| !s.is_empty())?;
    Some(claude_code_session_memory_path(cwd, session_id))
}

/// Resolve on-disk session memory path and combine mode. Env override removed; always None.
pub fn resolve_session_memory_file_options(
    session_id: &str,
    cwd: Option<&str>,
) -> (Option<PathBuf>, SessionMemoryFileCombine) {
    let env_combine = SessionMemoryFileCombine::from_env();

    if env_combine == SessionMemoryFileCombine::None {
        return (None, SessionMemoryFileCombine::None);
    }

    (
        resolve_resume_session_memory_file(session_id, cwd),
        env_combine,
    )
}

// ---------------------------------------------------------------------------
// Memoria Client Trait
// ---------------------------------------------------------------------------

/// Trait for Memoria HTTP operations (allows mocking in tests).
#[async_trait::async_trait]
pub trait MemoriaClient: Send + Sync {
    /// Retrieve memories (cross-session). Delegates to [`retrieve_ext`] with `filter_session=false`.
    async fn retrieve(
        &self,
        query: &str,
        session_id: Option<&str>,
        top_k: usize,
    ) -> Result<Vec<MemoriaMemory>, String> {
        self.retrieve_ext(query, session_id, top_k, false).await
    }

    /// Retrieve with explicit filter_session control.
    async fn retrieve_ext(
        &self,
        query: &str,
        session_id: Option<&str>,
        top_k: usize,
        filter_session: bool,
    ) -> Result<Vec<MemoriaMemory>, String>;

    /// Store a memory with optional trust tier for confidence decay.
    async fn store(
        &self,
        content: &str,
        memory_type: &str,
        session_id: Option<&str>,
        trust_tier: Option<&str>,
    ) -> Result<String, String>;

    /// Purge working memories for a session.
    async fn purge_working(&self, session_id: &str) -> Result<u64, String>;

    /// Delete a single memory by ID.
    /// Default: no-op. Override for clients that support deletion.
    async fn delete(&self, _memory_id: &str) -> Result<(), String> {
        Ok(())
    }
}

/// A memory record from Memoria.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoriaMemory {
    pub memory_id: String,
    pub content: String,
    pub memory_type: String,
    #[serde(default)]
    pub retrieval_score: Option<f64>,
}

// ---------------------------------------------------------------------------
// HTTP Client Implementation
// ---------------------------------------------------------------------------

/// HTTP-based Memoria client.
#[derive(Clone)]
pub struct HttpMemoriaClient {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
}

impl HttpMemoriaClient {
    pub fn new(base_url: String, api_key: String) -> Self {
        Self {
            base_url,
            api_key,
            http: reqwest::Client::builder()
                .no_proxy()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    /// Create from environment variables.
    pub fn from_env() -> Option<Self> {
        let base_url = std::env::var("MEMORIA_BASE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8100".to_string());
        let api_key = std::env::var("MEMORIA_MASTER_KEY").ok()?;
        Some(Self::new(base_url, api_key))
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

#[async_trait::async_trait]
impl MemoriaClient for HttpMemoriaClient {
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
        if let Some(sid) = session_id {
            body["session_id"] = json!(sid);
            if filter_session {
                body["filter_session"] = json!(true);
            }
        }

        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
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

        Ok(parse_retrieved_memories(&data))
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
            body["session_id"] = json!(sid);
        }
        if let Some(tier) = trust_tier {
            body["trust_tier"] = json!(tier);
        }

        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Memoria store failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("Memoria store HTTP {}", resp.status()));
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
    /// NOTE: Currently uses topic-based purge which does fulltext search on content.
    /// This does NOT reliably match UUID-style session IDs (ngram tokenizer issue).
    /// TODO: switch to session_id-based purge once Memoria supports it
    ///       (https://github.com/matrixorigin/Memoria/issues/182)
    async fn purge_working(&self, session_id: &str) -> Result<u64, String> {
        let url = format!("{}/v1/memories/purge", self.base_url.trim_end_matches('/'));
        let body = json!({
            "topic": format!("session:{}", session_id),
            "reason": "session compaction cleanup",
        });

        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
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

        Ok(data
            .get("deleted_count")
            .and_then(Value::as_u64)
            .unwrap_or(0))
    }

    async fn delete(&self, memory_id: &str) -> Result<(), String> {
        use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
        let encoded_id = utf8_percent_encode(memory_id, NON_ALPHANUMERIC).to_string();
        let url = format!(
            "{}/v1/memories/{}",
            self.base_url.trim_end_matches('/'),
            encoded_id
        );
        let resp = self
            .http
            .delete(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await
            .map_err(|e| format!("Memoria delete failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("Memoria delete HTTP {}", resp.status()));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Compaction Logic
// ---------------------------------------------------------------------------

/// Build a context summary from retrieved memories.
fn build_memory_context(memories: &[MemoriaMemory], max_tokens: usize) -> String {
    if memories.is_empty() {
        return String::new();
    }

    let mut parts = Vec::new();
    let mut total_tokens = 0;

    for mem in memories {
        let mem_tokens = crate::prompts::estimate_str_tokens(&mem.content);
        if total_tokens + mem_tokens > max_tokens {
            break;
        }
        parts.push(format!("• {}", mem.content));
        total_tokens += mem_tokens;
    }

    if parts.is_empty() {
        return String::new();
    }

    format!(
        "[Session Context from Memory]\n{}\n[End Context]",
        parts.join("\n")
    )
}

fn trim_str_to_approx_tokens(s: &str, max_tokens: usize) -> String {
    let max_chars = max_tokens.saturating_mul(4).max(256);
    let n = s.chars().count();
    if n <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

fn wrap_file_session_context(body: &str) -> String {
    format!("[Session memory — on-disk summary]\n{body}\n[End on-disk session memory]")
}

fn build_file_only_session_context(file_text: &str, max_tokens: usize) -> String {
    let t = trim_str_to_approx_tokens(file_text, max_tokens);
    if t.is_empty() {
        String::new()
    } else {
        wrap_file_session_context(&t)
    }
}

fn build_session_context_with_optional_file(
    memories: &[MemoriaMemory],
    file_text: Option<&str>,
    combine: SessionMemoryFileCombine,
    max_tokens: usize,
) -> String {
    match combine {
        SessionMemoryFileCombine::None => build_memory_context(memories, max_tokens),
        SessionMemoryFileCombine::Fallback => {
            if memories.is_empty() {
                match file_text {
                    Some(ft) if !ft.is_empty() => build_file_only_session_context(ft, max_tokens),
                    _ => String::new(),
                }
            } else {
                build_memory_context(memories, max_tokens)
            }
        }
        SessionMemoryFileCombine::Merge => match file_text {
            Some(ft) if !ft.is_empty() => {
                let file_cap = (max_tokens * 28 / 100)
                    .max(200)
                    .min(max_tokens.saturating_sub(80));
                let file_body = trim_str_to_approx_tokens(ft, file_cap);
                let file_wrapped = wrap_file_session_context(&file_body);
                let file_used = crate::prompts::estimate_str_tokens(&file_wrapped);
                let mem_budget = max_tokens.saturating_sub(file_used);
                let mem_ctx = build_memory_context(memories, mem_budget);
                match (mem_ctx.is_empty(), file_wrapped.is_empty()) {
                    (true, false) => file_wrapped,
                    (false, true) => mem_ctx,
                    (false, false) => format!("{file_wrapped}\n\n{mem_ctx}"),
                    (true, true) => String::new(),
                }
            }
            _ => build_memory_context(memories, max_tokens),
        },
    }
}

/// Build a working memory summary from recent messages.
fn build_working_memory_content(messages: &[Value], max_chars: usize) -> String {
    let mut parts = Vec::new();
    let mut total_chars = 0;

    // Extract key information from recent messages
    for msg in messages.iter().rev().take(10) {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("unknown");
        let content = msg.get("content").and_then(Value::as_str).unwrap_or("");

        let line = match role {
            "user" => format!("User: {}", truncate_str(content, 200)),
            "assistant" => {
                if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
                    let tools: Vec<&str> = tool_calls
                        .iter()
                        .filter_map(|tc| {
                            tc.get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(Value::as_str)
                        })
                        .collect();
                    format!("Assistant: [tools: {}]", tools.join(", "))
                } else {
                    format!("Assistant: {}", truncate_str(content, 200))
                }
            }
            "tool" => continue, // Skip tool results
            _ => continue,
        };

        let line_len = line.len();
        if total_chars + line_len > max_chars {
            break;
        }
        parts.push(line);
        total_chars += line_len;
    }

    parts.reverse();
    parts.join("\n")
}

// ---------------------------------------------------------------------------
// Unified budget for Memoria injection + LLM summary + truncated messages
// ---------------------------------------------------------------------------

/// Fraction of `budget_chars` used as a cap when splitting between memory text
/// and summary reservation before tightening [`compact_tiered_with_result`].
const AUX_INJECT_BUDGET_PCT: usize = 22;

/// Assumed extra prompt / framing tokens beyond `summary_token_budget` output.
const SUMMARY_PROMPT_OVERHEAD_TOKENS: usize = 768;

/// Never reserve more than this fraction of the total char budget for the summary slot alone.
const SUMMARY_RESERVE_MAX_PCT: usize = 40;

/// When summarizing, auxiliary pool (memory + summary slot) is capped at this fraction of total.
const AUX_COMBINED_MAX_PCT: usize = 55;

/// Minimum char room left for Memoria text after subtracting summary reservation.
const MEMORY_INJECT_FLOOR_CHARS: usize = 2048;

/// Plan how many tokens may go into [`build_memory_context`] and how many chars we
/// reserve for the summary message + wrapper (so truncation runs on a tighter budget).
#[must_use]
fn plan_injection_reservations(
    budget_chars: usize,
    will_summarize: bool,
    summary_token_budget: usize,
    config_max_memory_tokens: usize,
) -> (usize, usize) {
    let summary_reserve_chars = if will_summarize {
        let raw = summary_token_budget
            .saturating_add(SUMMARY_PROMPT_OVERHEAD_TOKENS)
            .saturating_mul(4);
        let pct_cap = budget_chars.saturating_mul(SUMMARY_RESERVE_MAX_PCT) / 100;
        raw.min(pct_cap)
    } else {
        0
    };

    let pct_aux = budget_chars.saturating_mul(AUX_INJECT_BUDGET_PCT) / 100;
    let aux_cap_chars = if will_summarize {
        let floor = summary_reserve_chars.saturating_add(MEMORY_INJECT_FLOOR_CHARS);
        let top = budget_chars.saturating_mul(AUX_COMBINED_MAX_PCT) / 100;
        floor.max(pct_aux).min(top)
    } else {
        pct_aux.max(2048)
    };

    let memory_room_chars = aux_cap_chars.saturating_sub(summary_reserve_chars);
    let memory_max_tokens = (memory_room_chars / 4).min(config_max_memory_tokens);

    (memory_max_tokens, summary_reserve_chars)
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
const MEMORIA_RETRIEVE_QUERY_FALLBACK: &str = "current session context working memory";

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
    parts.push("session working memory".to_string());
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
/// 2. Retrieve working memories for session → build context prefix
/// 3. Apply tier-based truncation
/// 4. Optionally store updated working memory
pub async fn compact_with_memoria(
    messages: &[Value],
    session_id: Option<&str>,
    config: &MemoriaCompactConfig,
    params: &MemoriaCompactParams,
    client: Option<&dyn MemoriaClient>,
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
        return compact_tiered_with_result(
            messages,
            params.budget_chars,
            params.keep_chars,
            params.tier,
            params.keep_recent_turns,
        );
    }

    let Some(client) = client else {
        return compact_tiered_with_result(
            messages,
            params.budget_chars,
            params.keep_chars,
            params.tier,
            params.keep_recent_turns,
        );
    };
    let Some(sid) = session_id else {
        return compact_tiered_with_result(
            messages,
            params.budget_chars,
            params.keep_chars,
            params.tier,
            params.keep_recent_turns,
        );
    };

    // Step 1: Retrieve session context from Memoria (strict session scope).
    let query = memoria_compact_retrieve_query(messages);
    let memories = match client
        .retrieve_ext(&query, Some(sid), config.max_memories, true)
        .await
    {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[compact] Memoria retrieve failed: {e}");
            Vec::new()
        }
    };

    let file_text = params
        .session_memory_file
        .as_ref()
        .and_then(|p| read_session_memory_file(p));
    let had_on_disk_session_memory = file_text.is_some();

    let will_summarize = compact_config
        .zip(summary_client.as_ref())
        .is_some_and(|(cfg, _)| cfg.should_summarize(params.tier));
    let summary_token_budget = compact_config.map(|c| c.summary_token_budget).unwrap_or(0);

    let (memory_max_tokens, summary_reserve_chars) = plan_injection_reservations(
        params.budget_chars,
        will_summarize,
        summary_token_budget,
        config.max_memory_tokens,
    );

    // Step 2: Build context summary
    // Facts-first path: when SessionFacts available, use ground truth + narrative
    // instead of raw Memoria memories. Zero LLM, always available.
    let memory_context = if let Some(facts) = &params.session_facts {
        // Try to find L1 narrative from Memoria memories (prefix match)
        let narrative = memories
            .iter()
            .find(|m| {
                m.content
                    .starts_with(super::session_memory_protocol::SESSION_MEMORY_PREFIX)
            })
            .and_then(|m| super::session_memory_protocol::SessionMemory::parse(&m.content));
        let injection =
            super::session_memory_protocol::build_facts_first_injection(facts, narrative.as_ref());
        eprintln!(
            "[compact] Facts-first injection ({} chars, narrative={})",
            injection.len(),
            narrative.is_some(),
        );
        injection
    } else {
        build_session_context_with_optional_file(
            &memories,
            file_text.as_deref(),
            params.session_memory_combine,
            memory_max_tokens,
        )
    };
    let has_memory_context = !memory_context.is_empty();
    let memory_chars = memory_context.chars().count();

    let adjusted_budget_chars =
        adjusted_message_budget_chars(params.budget_chars, memory_chars, summary_reserve_chars);

    // Step 3: Apply truncation against budget that leaves room for injections
    let mut result = compact_tiered_with_result(
        messages,
        adjusted_budget_chars,
        params.keep_chars,
        params.tier,
        params.keep_recent_turns,
    );

    // Step 4: Inject memory context if available
    if has_memory_context {
        // Insert memory context as a system message after the first message
        let context_msg = json!({
            "role": "system",
            "content": memory_context,
        });
        if !result.messages.is_empty() {
            result.messages.insert(1, context_msg);
        }

        // Update boundary to reflect memory usage
        let inj_summary = if had_on_disk_session_memory {
            format!(
                "Memoria: {} memories retrieved; on-disk session memory included",
                memories.len()
            )
        } else {
            format!("Memoria: {} memories retrieved", memories.len())
        };
        if let Some(ref mut boundary) = result.boundary {
            boundary.summary = Some(inj_summary.clone());
        } else {
            result.boundary = Some(
                CompactBoundary::new(CompactTrigger::Auto, params.tier)
                    .with_pre_metrics(params.current_tokens, messages.len())
                    .with_post_count(result.messages.len()),
            );
            if let Some(ref mut b) = result.boundary {
                b.summary = Some(inj_summary);
            }
        }

        eprintln!(
            "[compact] Session context injected ({} memories, on_disk={}, {} est. tokens)",
            memories.len(),
            had_on_disk_session_memory,
            crate::prompts::estimate_str_tokens(&memory_context)
        );
    }

    // Step 5: Optionally store updated working memory (even on cold start)
    if config.store_on_compact {
        let working_content = build_working_memory_content(messages, 2000);
        if !working_content.is_empty() {
            let store_content = format!(
                "[session:{}] Recent conversation:\n{}",
                sid, working_content
            );
            if let Err(e) = client
                .store(&store_content, "working", Some(sid), None)
                .await
            {
                eprintln!("[compact] Failed to store working memory: {e}");
            }
        }
    }

    // Step 6: Optionally generate LLM summary
    if let Some(cfg) = compact_config
        && let Some(s_client) = summary_client
        && cfg.should_summarize(params.tier)
    {
        match astra_turn_core::cloud_summary::generate_compact_summary(messages, s_client).await {
            Some(summary) => {
                let summary = truncate_summary_for_budget(summary, cfg.summary_token_budget);
                let summary_msg = json!({
                    "role": "user",
                    "content": format!("[Conversation summary — context compacted]\n\n{summary}"),
                    "attachment_metadata": { "kind": "compact_summary" }
                });
                result.messages.insert(0, summary_msg);

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

                // Step 6b: Store compaction summary as semantic memory for cross-session retrieval.
                // The LLM summary is already generated (zero additional LLM cost);
                // storing it as "semantic" makes it persist beyond session working memory cleanup.
                // Prefix includes a stable session tag so re-compaction can be identified.
                // NOTE: Memoria has no delete-by-id API, so prior compaction summaries for
                // the same session will accumulate. Dedup relies on Memoria's natural vector
                // similarity detection (high similarity scores for same-session entries).
                if config.store_on_compact {
                    let tag = format!("[compaction:{}]", sid);
                    let semantic_content = format!("{} {}", tag, summary);
                    if let Err(e) = client
                        .store(
                            &semantic_content,
                            "semantic",
                            Some(sid),
                            Some(astra_prompts::memory_proto::TIER_INFERRED),
                        )
                        .await
                    {
                        eprintln!("[compact] Failed to store compaction summary as semantic: {e}");
                    }
                }
            }
            None => {
                eprintln!("[compact] LLM summary failed, using truncation only");
            }
        }
    }

    result
}

/// Synchronous wrapper that checks for Memoria availability.
///
/// If Memoria is not configured, falls back to pure truncation.
pub fn compact_with_memoria_sync(
    messages: &[Value],
    _session_id: Option<&str>,
    _config: &MemoriaCompactConfig,
    params: &MemoriaCompactParams,
) -> CompactResult {
    // For sync contexts, we can't use Memoria (requires async HTTP).
    // Fall back to pure truncation.
    compact_tiered_with_result(
        messages,
        params.budget_chars,
        params.keep_chars,
        params.tier,
        params.keep_recent_turns,
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        ENV_LOCK.get_or_init(|| Mutex::new(()))
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

    fn with_env_paths<R>(
        changes: &[(&'static str, Option<&std::path::Path>)],
        f: impl FnOnce() -> R,
    ) -> R {
        let _lock = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        let previous: Vec<_> = changes
            .iter()
            .map(|(key, _)| (*key, std::env::var_os(key)))
            .collect();

        for (key, value) in changes {
            if let Some(path) = value {
                unsafe {
                    std::env::set_var(key, path);
                }
            } else {
                unsafe {
                    std::env::remove_var(key);
                }
            }
        }

        let result = f();

        for (key, previous) in previous {
            if let Some(previous) = previous {
                unsafe {
                    std::env::set_var(key, previous);
                }
            } else {
                unsafe {
                    std::env::remove_var(key);
                }
            }
        }

        result
    }

    struct MockMemoriaClient {
        memories: Mutex<Vec<MemoriaMemory>>,
        stored: Mutex<Vec<(String, String)>>,
    }

    impl MockMemoriaClient {
        fn new(memories: Vec<MemoriaMemory>) -> Self {
            Self {
                memories: Mutex::new(memories),
                stored: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl MemoriaClient for MockMemoriaClient {
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
            content: &str,
            memory_type: &str,
            _session_id: Option<&str>,
            _trust_tier: Option<&str>,
        ) -> Result<String, String> {
            self.stored
                .lock()
                .unwrap()
                .push((content.to_string(), memory_type.to_string()));
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

    fn make_mem(id: &str, content: &str) -> MemoriaMemory {
        MemoriaMemory {
            memory_id: id.to_string(),
            content: content.to_string(),
            memory_type: "semantic".to_string(),
            retrieval_score: Some(0.8),
        }
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
        assert!(q.contains("session working memory"));
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
    fn plan_injection_reserves_summary_and_caps_memory_tokens() {
        let budget = 100_000_usize;
        let (mem_tok, sum_res) = plan_injection_reservations(budget, true, 20_000, 4_000);
        assert!(sum_res > 0, "summary branch should reserve chars");
        assert!(mem_tok <= 4_000);
        let adj = adjusted_message_budget_chars(budget, 5_000, sum_res);
        assert!(
            adj < budget,
            "message budget should shrink after reservations: {adj} < {budget}"
        );
    }

    #[test]
    fn plan_injection_without_summary_uses_aux_for_memory_only() {
        let budget = 50_000_usize;
        let (mem_tok, sum_res) = plan_injection_reservations(budget, false, 0, 10_000);
        assert_eq!(sum_res, 0);
        assert!(
            mem_tok > 0,
            "memory token cap should be positive: {mem_tok}"
        );
    }

    #[test]
    fn summary_reserve_capped_to_pct_of_total_budget() {
        let budget = 10_000_usize;
        let (_, sum_res) = plan_injection_reservations(budget, true, 500_000, 4_000);
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
    fn build_memory_context_empty() {
        let ctx = build_memory_context(&[], 1000);
        assert!(ctx.is_empty());
    }

    #[test]
    fn build_memory_context_single() {
        let memories = vec![MemoriaMemory {
            memory_id: "m1".to_string(),
            content: "User prefers Rust".to_string(),
            memory_type: "working".to_string(),
            retrieval_score: Some(0.9),
        }];
        let ctx = build_memory_context(&memories, 1000);
        assert!(ctx.contains("User prefers Rust"));
        assert!(ctx.contains("[Session Context from Memory]"));
    }

    #[test]
    fn build_memory_context_truncates() {
        let memories = vec![
            MemoriaMemory {
                memory_id: "m1".to_string(),
                content: "A".repeat(100),
                memory_type: "working".to_string(),
                retrieval_score: None,
            },
            MemoriaMemory {
                memory_id: "m2".to_string(),
                content: "B".repeat(100),
                memory_type: "working".to_string(),
                retrieval_score: None,
            },
        ];
        // With very small token limit, should only include first
        let ctx = build_memory_context(&memories, 30);
        assert!(ctx.contains(&"A".repeat(100)));
        assert!(!ctx.contains(&"B".repeat(100)));
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
            session_memory_file: None,
            session_memory_combine: SessionMemoryFileCombine::None,
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
        let mock = MockMemoriaClient::new(vec![]);
        let params = MemoriaCompactParams {
            budget_chars: 10000,
            keep_chars: 2000,
            tier: CompactionTier::TrimSchemas,
            keep_recent_turns: 4,
            current_tokens: 1000, // Below threshold
            session_memory_file: None,
            session_memory_combine: SessionMemoryFileCombine::None,
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

        // Should not have stored anything
        assert!(mock.stored.lock().unwrap().is_empty());
        assert_eq!(result.messages.len(), 2);
    }

    #[tokio::test]
    async fn compact_injects_memoria_context() {
        let msgs = vec![
            user("implement OAuth"),
            assistant("I'll help with OAuth"),
            user("use JWT"),
        ];
        let config = MemoriaCompactConfig {
            min_tokens_for_retrieval: 100,
            store_on_compact: true,
            ..Default::default()
        };
        let mock = MockMemoriaClient::new(vec![MemoriaMemory {
            memory_id: "m1".to_string(),
            content: "Working on auth module".to_string(),
            memory_type: "working".to_string(),
            retrieval_score: Some(0.8),
        }]);
        let params = MemoriaCompactParams {
            budget_chars: 10000,
            keep_chars: 2000,
            tier: CompactionTier::CompactHistory,
            keep_recent_turns: 4,
            current_tokens: 6000, // Above threshold
            session_memory_file: None,
            session_memory_combine: SessionMemoryFileCombine::None,
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

        // Should have injected context message
        assert!(result.messages.len() >= 3);

        // Check for context injection at index 1 (after first user message)
        assert!(
            result.messages.len() >= 3,
            "Should have context msg injected"
        );
        let ctx_content = result.messages[1]
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            ctx_content.contains("[Session Context from Memory]"),
            "Context should be injected at index 1, got: {ctx_content}"
        );

        // Should have stored working memory
        let stored = mock.stored.lock().unwrap();
        assert!(!stored.is_empty());
        assert_eq!(stored[0].1, "working");
    }

    #[tokio::test]
    async fn compact_facts_first_uses_session_facts() {
        use astra_turn_types::session_facts::{ErrorFact, FileEntry, SessionFacts};
        let msgs = vec![
            user("implement OAuth"),
            assistant("I'll help with OAuth"),
            user("use JWT"),
            assistant("Switching to JWT"),
        ];
        let config = MemoriaCompactConfig {
            min_tokens_for_retrieval: 100,
            store_on_compact: false,
            ..Default::default()
        };
        // Memoria returns an L1 narrative
        let l1_content = format!(
            "{}\n# Session Title\nOAuth impl\n# Task Specification\nImplement OAuth with JWT\n# User Messages\nuse JWT",
            super::super::session_memory_protocol::SESSION_MEMORY_PREFIX
        );
        let mock = MockMemoriaClient::new(vec![MemoriaMemory {
            memory_id: "m1".to_string(),
            content: l1_content,
            memory_type: "working".to_string(),
            retrieval_score: Some(0.9),
        }]);
        let mut facts = SessionFacts::default();
        facts.turn = 4;
        facts.estimated_tokens = 20000;
        facts.active_files.push(FileEntry {
            path: "src/auth.rs".to_string(),
            last_action: "write".to_string(),
            turn: 3,
        });
        facts.error_state = ErrorFact {
            total_errors: 1,
            last_error: Some("compile error".to_string()),
            last_error_turn: Some(3),
        };
        let params = MemoriaCompactParams {
            budget_chars: 10000,
            keep_chars: 2000,
            tier: CompactionTier::CompactHistory,
            keep_recent_turns: 4,
            current_tokens: 6000,
            session_memory_file: None,
            session_memory_combine: SessionMemoryFileCombine::None,
            session_facts: Some(facts),
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

        // Should have injected facts-first context
        let ctx = result
            .messages
            .iter()
            .find(|m| {
                m.get("role").and_then(Value::as_str) == Some("system")
                    && m.get("content")
                        .and_then(Value::as_str)
                        .map(|c| c.contains("# System State"))
                        .unwrap_or(false)
            })
            .expect("Should have facts-first injection");
        let content = ctx.get("content").unwrap().as_str().unwrap();
        // Facts present
        assert!(content.contains("Turn 4"), "should have turn from facts");
        assert!(
            content.contains("src/auth.rs"),
            "should have active file from facts"
        );
        assert!(
            content.contains("compile error"),
            "should have error from facts"
        );
        // Narrative present (from Memoria L1)
        assert!(
            content.contains("# Task"),
            "should have task from narrative"
        );
        assert!(
            content.contains("OAuth"),
            "should have OAuth from narrative"
        );
    }

    #[tokio::test]
    async fn compact_facts_first_works_without_narrative() {
        use astra_turn_types::session_facts::{FileEntry, SessionFacts};
        let msgs = vec![user("hello"), assistant("hi")];
        let config = MemoriaCompactConfig {
            min_tokens_for_retrieval: 100,
            store_on_compact: false,
            ..Default::default()
        };
        // Memoria returns nothing (no narrative available)
        let mock = MockMemoriaClient::new(vec![]);
        let mut facts = SessionFacts::default();
        facts.turn = 2;
        facts.estimated_tokens = 10000;
        facts.active_files.push(FileEntry {
            path: "main.rs".to_string(),
            last_action: "read".to_string(),
            turn: 1,
        });
        let params = MemoriaCompactParams {
            budget_chars: 10000,
            keep_chars: 2000,
            tier: CompactionTier::CompactHistory,
            keep_recent_turns: 4,
            current_tokens: 6000,
            session_memory_file: None,
            session_memory_combine: SessionMemoryFileCombine::None,
            session_facts: Some(facts),
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

        // Should still inject facts (no narrative, but facts alone is sufficient)
        let has_facts = result.messages.iter().any(|m| {
            m.get("content")
                .and_then(Value::as_str)
                .map(|c| c.contains("# System State") && c.contains("main.rs"))
                .unwrap_or(false)
        });
        assert!(
            has_facts,
            "facts-only injection should work without narrative"
        );
    }

    #[test]
    fn sync_wrapper_falls_back() {
        let msgs = vec![user("hello"), assistant("hi")];
        let config = MemoriaCompactConfig::default();
        let params = MemoriaCompactParams {
            budget_chars: 10000,
            keep_chars: 2000,
            tier: CompactionTier::Normal,
            keep_recent_turns: 4,
            current_tokens: 1000,
            session_memory_file: None,
            session_memory_combine: SessionMemoryFileCombine::None,
            session_facts: None,
        };
        let result = compact_with_memoria_sync(&msgs, Some("sess1"), &config, &params);
        assert_eq!(result.messages.len(), 2);
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
            store_on_compact: false,
            ..Default::default()
        };
        let mock = MockMemoriaClient::new(vec![MemoriaMemory {
            memory_id: "m1".to_string(),
            content: "Working on auth module".to_string(),
            memory_type: "working".to_string(),
            retrieval_score: Some(0.8),
        }]);
        let params = MemoriaCompactParams {
            budget_chars: 10000,
            keep_chars: 2000,
            tier: CompactionTier::AggressivePrune, // Meets summary_min_tier
            keep_recent_turns: 4,
            current_tokens: 6000,
            session_memory_file: None,
            session_memory_combine: SessionMemoryFileCombine::None,
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

        // Should have summary message
        let has_summary = result.messages.iter().any(|m| {
            m.get("content")
                .and_then(Value::as_str)
                .map(|s| s.contains("[Conversation summary"))
                .unwrap_or(false)
        });
        assert!(has_summary, "should have summary message");

        // Should also have memoria context
        let has_context = result.messages.iter().any(|m| {
            m.get("content")
                .and_then(Value::as_str)
                .map(|s| s.contains("[Session Context from Memory]"))
                .unwrap_or(false)
        });
        assert!(has_context, "should have memoria context");
    }

    #[tokio::test]
    async fn compact_summary_disabled_skips_llm() {
        let msgs = vec![user("hello"), assistant("hi")];
        let config = MemoriaCompactConfig {
            min_tokens_for_retrieval: 100,
            ..Default::default()
        };
        let mock = MockMemoriaClient::new(vec![]);
        let params = MemoriaCompactParams {
            budget_chars: 10000,
            keep_chars: 2000,
            tier: CompactionTier::AggressivePrune,
            keep_recent_turns: 4,
            current_tokens: 6000,
            session_memory_file: None,
            session_memory_combine: SessionMemoryFileCombine::None,
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
        let mock = MockMemoriaClient::new(vec![]);
        let params = MemoriaCompactParams {
            budget_chars: 10000,
            keep_chars: 2000,
            tier: CompactionTier::AggressivePrune,
            keep_recent_turns: 4,
            current_tokens: 6000,
            session_memory_file: None,
            session_memory_combine: SessionMemoryFileCombine::None,
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
        let mock = MockMemoriaClient::new(vec![]);
        let params = MemoriaCompactParams {
            budget_chars: 10000,
            keep_chars: 2000,
            tier: CompactionTier::TrimSchemas, // Below AggressivePrune threshold
            keep_recent_turns: 4,
            current_tokens: 6000,
            session_memory_file: None,
            session_memory_combine: SessionMemoryFileCombine::None,
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

    #[test]
    fn resolve_resume_session_memory_file_uses_claude_path_without_combine_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = with_env_paths(
            &[
                ("CLAUDE_CONFIG_DIR", Some(dir.path())),
                ("ASTRA_SESSION_MEMORY_FILE", None),
            ],
            || resolve_resume_session_memory_file("sess-123", Some("/tmp/my project")).unwrap(),
        );
        assert_eq!(
            path,
            dir.path()
                .join("projects")
                .join(sanitize_path_for_claude_projects("/tmp/my project"))
                .join("sess-123")
                .join("session-memory")
                .join("summary.md")
        );
    }

    #[tokio::test]
    async fn compact_fallback_injects_file_when_memoria_empty() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("summary.md");
        std::fs::write(&f, "# Session\nDisk-only anchor text").unwrap();
        let msgs = vec![user("hi"), assistant("hello")];
        let config = MemoriaCompactConfig {
            min_tokens_for_retrieval: 100,
            store_on_compact: false,
            ..Default::default()
        };
        let mock = MockMemoriaClient::new(vec![]);
        let params = MemoriaCompactParams {
            budget_chars: 10000,
            keep_chars: 2000,
            tier: CompactionTier::CompactHistory,
            keep_recent_turns: 4,
            current_tokens: 6000,
            session_memory_file: Some(f),
            session_memory_combine: SessionMemoryFileCombine::Fallback,
            session_facts: None,
        };
        let r = compact_with_memoria(
            &msgs,
            Some("sid"),
            &config,
            &params,
            Some(&mock),
            None,
            None,
        )
        .await;
        let text = r
            .messages
            .iter()
            .filter_map(|m| m.get("content").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Disk-only anchor"));
        assert!(text.contains("[Session memory — on-disk summary]"));
    }

    #[tokio::test]
    async fn compact_fallback_skips_file_when_memoria_hits() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("summary.md");
        std::fs::write(&f, "SHOULD_NOT_APPEAR").unwrap();
        let mock = MockMemoriaClient::new(vec![MemoriaMemory {
            memory_id: "m1".to_string(),
            content: "from memoria".to_string(),
            memory_type: "working".to_string(),
            retrieval_score: None,
        }]);
        let params = MemoriaCompactParams {
            budget_chars: 10000,
            keep_chars: 2000,
            tier: CompactionTier::CompactHistory,
            keep_recent_turns: 4,
            current_tokens: 6000,
            session_memory_file: Some(f),
            session_memory_combine: SessionMemoryFileCombine::Fallback,
            session_facts: None,
        };
        let config = MemoriaCompactConfig {
            min_tokens_for_retrieval: 100,
            store_on_compact: false,
            ..Default::default()
        };
        let r = compact_with_memoria(
            &vec![user("a"), assistant("b")],
            Some("sid"),
            &config,
            &params,
            Some(&mock),
            None,
            None,
        )
        .await;
        let text = r
            .messages
            .iter()
            .filter_map(|m| m.get("content").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("from memoria"));
        assert!(!text.contains("SHOULD_NOT_APPEAR"));
    }

    #[tokio::test]
    async fn compact_merge_combines_disk_and_memoria() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("summary.md");
        std::fs::write(&f, "DISK_UNIQUE").unwrap();
        let mock = MockMemoriaClient::new(vec![MemoriaMemory {
            memory_id: "m1".to_string(),
            content: "MEM_UNIQUE".to_string(),
            memory_type: "working".to_string(),
            retrieval_score: None,
        }]);
        let params = MemoriaCompactParams {
            budget_chars: 10000,
            keep_chars: 2000,
            tier: CompactionTier::CompactHistory,
            keep_recent_turns: 4,
            current_tokens: 6000,
            session_memory_file: Some(f),
            session_memory_combine: SessionMemoryFileCombine::Merge,
            session_facts: None,
        };
        let config = MemoriaCompactConfig {
            min_tokens_for_retrieval: 100,
            store_on_compact: false,
            ..Default::default()
        };
        let r = compact_with_memoria(
            &vec![user("a"), assistant("b")],
            Some("sid"),
            &config,
            &params,
            Some(&mock),
            None,
            None,
        )
        .await;
        let text = r
            .messages
            .iter()
            .filter_map(|m| m.get("content").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("DISK_UNIQUE"));
        assert!(text.contains("MEM_UNIQUE"));
    }

    // ──────────────────────────────────────────────────────────
    // djb2_hash_utf16
    // ──────────────────────────────────────────────────────────

    #[test]
    fn djb2_hash_empty_string() {
        assert_eq!(djb2_hash_utf16(""), 0);
    }

    #[test]
    fn djb2_hash_deterministic() {
        let h1 = djb2_hash_utf16("hello");
        let h2 = djb2_hash_utf16("hello");
        assert_eq!(h1, h2);
    }

    #[test]
    fn djb2_hash_different_for_different_inputs() {
        assert_ne!(djb2_hash_utf16("abc"), djb2_hash_utf16("def"));
    }

    // ──────────────────────────────────────────────────────────
    // abs_hash_to_string_36
    // ──────────────────────────────────────────────────────────

    #[test]
    fn abs_hash_to_string_36_zero() {
        assert_eq!(abs_hash_to_string_36(0), "0");
    }

    #[test]
    fn abs_hash_to_string_36_positive() {
        let s = abs_hash_to_string_36(36);
        assert_eq!(s, "10"); // 36 in base-36 is "10"
    }

    #[test]
    fn abs_hash_to_string_36_negative() {
        // abs(-36) = 36, same as positive
        assert_eq!(abs_hash_to_string_36(-36), "10");
    }

    #[test]
    fn abs_hash_to_string_36_large() {
        let s = abs_hash_to_string_36(i32::MAX);
        assert!(!s.is_empty());
        // Only [0-9a-z] characters
        assert!(s.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    // ──────────────────────────────────────────────────────────
    // sanitize_path_for_claude_projects (extended)
    // ──────────────────────────────────────────────────────────

    #[test]
    fn sanitize_path_short_keeps_as_is() {
        assert_eq!(sanitize_path_for_claude_projects("abc123"), "abc123");
    }

    #[test]
    fn sanitize_path_long_appends_hash() {
        let long_path = "a/".repeat(200); // > 200 chars after sanitization
        let result = sanitize_path_for_claude_projects(&long_path);
        assert!(result.len() > 200); // prefix + "-" + hash
        assert!(result.contains('-'));
    }

    #[test]
    fn sanitize_path_empty() {
        assert_eq!(sanitize_path_for_claude_projects(""), "");
    }

    // ──────────────────────────────────────────────────────────
    // trim_str_to_approx_tokens
    // ──────────────────────────────────────────────────────────

    #[test]
    fn trim_str_within_limit() {
        let s = "hello world";
        assert_eq!(trim_str_to_approx_tokens(s, 100), s);
    }

    #[test]
    fn trim_str_exceeds_limit() {
        let s = "a".repeat(2000);
        let r = trim_str_to_approx_tokens(&s, 1); // 1 token ≈ 4 chars, min 256
        assert!(r.len() < s.len());
        assert!(r.ends_with('…'));
    }

    #[test]
    fn trim_str_empty() {
        assert_eq!(trim_str_to_approx_tokens("", 100), "");
    }

    // ──────────────────────────────────────────────────────────
    // wrap_file_session_context / build_file_only_session_context
    // ──────────────────────────────────────────────────────────

    #[test]
    fn wrap_file_session_context_format() {
        let r = wrap_file_session_context("my notes");
        assert!(r.starts_with("[Session memory"));
        assert!(r.contains("my notes"));
        assert!(r.ends_with("]"));
    }

    #[test]
    fn build_file_only_empty_yields_empty() {
        assert!(build_file_only_session_context("", 100).is_empty());
    }

    #[test]
    fn build_file_only_wraps_text() {
        let r = build_file_only_session_context("disk notes", 100);
        assert!(r.contains("disk notes"));
        assert!(r.contains("[Session memory"));
    }

    // ──────────────────────────────────────────────────────────
    // build_session_context_with_optional_file
    // ──────────────────────────────────────────────────────────

    #[test]
    fn session_context_none_combine_ignores_file() {
        let mems = vec![make_mem("x", "mem stuff")];
        let r = build_session_context_with_optional_file(
            &mems,
            Some("disk stuff"),
            SessionMemoryFileCombine::None,
            1000,
        );
        assert!(r.contains("mem stuff"));
        assert!(!r.contains("disk stuff"));
    }

    #[test]
    fn session_context_fallback_with_memories_ignores_file() {
        let mems = vec![make_mem("x", "mem stuff")];
        let r = build_session_context_with_optional_file(
            &mems,
            Some("disk stuff"),
            SessionMemoryFileCombine::Fallback,
            1000,
        );
        assert!(r.contains("mem stuff"));
        assert!(!r.contains("disk stuff"));
    }

    #[test]
    fn session_context_fallback_empty_memories_uses_file() {
        let r = build_session_context_with_optional_file(
            &[],
            Some("disk fallback"),
            SessionMemoryFileCombine::Fallback,
            1000,
        );
        assert!(r.contains("disk fallback"));
    }

    #[test]
    fn session_context_fallback_empty_both() {
        let r = build_session_context_with_optional_file(
            &[],
            None,
            SessionMemoryFileCombine::Fallback,
            1000,
        );
        assert!(r.is_empty());
    }

    #[test]
    fn session_context_merge_combines_both() {
        let mems = vec![make_mem("x", "mem side")];
        let r = build_session_context_with_optional_file(
            &mems,
            Some("disk side"),
            SessionMemoryFileCombine::Merge,
            1000,
        );
        assert!(r.contains("mem side"));
        assert!(r.contains("disk side"));
    }

    #[test]
    fn session_context_merge_no_file_just_memory() {
        let mems = vec![make_mem("x", "only mem")];
        let r = build_session_context_with_optional_file(
            &mems,
            None,
            SessionMemoryFileCombine::Merge,
            1000,
        );
        assert!(r.contains("only mem"));
    }

    // ──────────────────────────────────────────────────────────
    // build_working_memory_content
    // ──────────────────────────────────────────────────────────

    #[test]
    fn working_memory_empty_messages() {
        assert!(build_working_memory_content(&[], 1000).is_empty());
    }

    #[test]
    fn working_memory_user_and_assistant() {
        let msgs = vec![user("hello"), assistant("world")];
        let r = build_working_memory_content(&msgs, 10000);
        assert!(r.contains("User: hello"));
        assert!(r.contains("Assistant: world"));
    }

    #[test]
    fn working_memory_skips_tool_role() {
        let tool_msg = json!({"role": "tool", "content": "tool output", "tool_call_id": "t1"});
        let msgs = vec![user("q"), tool_msg, assistant("a")];
        let r = build_working_memory_content(&msgs, 10000);
        assert!(!r.contains("tool output"));
    }

    #[test]
    fn working_memory_assistant_with_tool_calls() {
        let a = json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{"function": {"name": "bash", "arguments": "{}"}}]
        });
        let msgs = vec![user("run it"), a];
        let r = build_working_memory_content(&msgs, 10000);
        assert!(r.contains("[tools: bash]"));
    }

    #[test]
    fn working_memory_budget_caps() {
        let msgs = vec![user(&"x".repeat(500)), assistant(&"y".repeat(500))];
        let r = build_working_memory_content(&msgs, 100);
        // Should be capped and not include all content
        assert!(r.len() <= 500); // generous but capped
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

    // ──────────────────────────────────────────────────────────
    // read_session_memory_file
    // ──────────────────────────────────────────────────────────

    #[test]
    fn read_session_memory_file_normal() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("notes.md");
        std::fs::write(&f, "  session notes  \n").unwrap();
        assert_eq!(read_session_memory_file(&f), Some("session notes".into()));
    }

    #[test]
    fn read_session_memory_file_empty() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("empty.md");
        std::fs::write(&f, "   ").unwrap();
        assert_eq!(read_session_memory_file(&f), None);
    }

    #[test]
    fn read_session_memory_file_missing() {
        let path = std::path::Path::new("/nonexistent/file.md");
        assert_eq!(read_session_memory_file(path), None);
    }

    #[test]
    fn read_session_memory_file_too_large() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("huge.md");
        // Create file > 512KB
        let data = "x".repeat(600 * 1024);
        std::fs::write(&f, &data).unwrap();
        assert_eq!(read_session_memory_file(&f), None);
    }

    #[tokio::test]
    async fn compact_store_on_compact_stores_semantic_summary() {
        // When store_on_compact=true and summary succeeds,
        // the compaction summary should be stored as semantic memory with [compaction:sid] tag.
        let msgs = vec![
            user("implement OAuth"),
            assistant("I'll help with OAuth. Here's a plan..."),
            user("use JWT instead"),
            assistant("Sure, switching to JWT tokens for auth."),
        ];
        let config = MemoriaCompactConfig {
            min_tokens_for_retrieval: 100,
            store_on_compact: true, // <-- enable semantic storage
            ..Default::default()
        };
        let mock = MockMemoriaClient::new(vec![MemoriaMemory {
            memory_id: "m1".to_string(),
            content: "Working on auth module".to_string(),
            memory_type: "working".to_string(),
            retrieval_score: Some(0.8),
        }]);
        let params = MemoriaCompactParams {
            budget_chars: 10000,
            keep_chars: 2000,
            tier: CompactionTier::AggressivePrune,
            keep_recent_turns: 4,
            current_tokens: 6000,
            session_memory_file: None,
            session_memory_combine: SessionMemoryFileCombine::None,
            session_facts: None,
        };
        let compact_config = CompactConfig {
            enable_summary: true,
            summary_min_tier: CompactionTier::AggressivePrune,
            ..Default::default()
        };
        let summary_client =
            MockSummaryClient::success("User discussed OAuth then switched to JWT auth.");

        let _result = compact_with_memoria(
            &msgs,
            Some("sess-test-42"),
            &config,
            &params,
            Some(&mock),
            Some(&compact_config),
            Some(&summary_client as &dyn astra_turn_core::cloud_summary::SummaryLlmClient),
        )
        .await;

        // Verify semantic store was called with correct tag and type
        let stored = mock.stored.lock().unwrap();
        let semantic_entries: Vec<_> = stored
            .iter()
            .filter(|(_, mem_type)| mem_type == "semantic")
            .collect();
        assert_eq!(
            semantic_entries.len(),
            1,
            "should store exactly one semantic entry, got {}",
            semantic_entries.len()
        );
        let (content, _) = &semantic_entries[0];
        assert!(
            content.starts_with("[compaction:sess-test-42]"),
            "should have session tag prefix, got: {}",
            &content[..50.min(content.len())]
        );
        assert!(content.contains("JWT"), "should contain the summary text");
    }

    #[tokio::test]
    async fn compact_store_on_compact_false_skips_semantic_store() {
        // When store_on_compact=false, no semantic memory should be stored.
        let msgs = vec![
            user("implement OAuth"),
            assistant("I'll help with OAuth. Here's a plan..."),
            user("use JWT instead"),
            assistant("Sure, switching to JWT tokens for auth."),
        ];
        let config = MemoriaCompactConfig {
            min_tokens_for_retrieval: 100,
            store_on_compact: false, // <-- disabled
            ..Default::default()
        };
        // Non-empty memories so we don't early-return before the store_on_compact check
        let mock = MockMemoriaClient::new(vec![MemoriaMemory {
            memory_id: "m1".to_string(),
            content: "Working on auth module".to_string(),
            memory_type: "working".to_string(),
            retrieval_score: Some(0.8),
        }]);
        let params = MemoriaCompactParams {
            budget_chars: 10000,
            keep_chars: 2000,
            tier: CompactionTier::AggressivePrune,
            keep_recent_turns: 4,
            current_tokens: 6000,
            session_memory_file: None,
            session_memory_combine: SessionMemoryFileCombine::None,
            session_facts: None,
        };
        let compact_config = CompactConfig {
            enable_summary: true,
            summary_min_tier: CompactionTier::AggressivePrune,
            ..Default::default()
        };
        let summary_client = MockSummaryClient::success("Some summary");

        let _result = compact_with_memoria(
            &msgs,
            Some("sess-no-store"),
            &config,
            &params,
            Some(&mock),
            Some(&compact_config),
            Some(&summary_client as &dyn astra_turn_core::cloud_summary::SummaryLlmClient),
        )
        .await;

        // Verify NO semantic store was called
        let stored = mock.stored.lock().unwrap();
        let semantic_entries: Vec<_> = stored
            .iter()
            .filter(|(_, mem_type)| mem_type == "semantic")
            .collect();
        assert!(
            semantic_entries.is_empty(),
            "should not store semantic entries when disabled, got {}",
            semantic_entries.len()
        );
    }

    /// audit-A3: HttpMemoriaClient must have connect_timeout and timeout so a
    /// hung Memoria server cannot block the compaction pipeline indefinitely.
    #[test]
    fn memoria_compact_client_has_timeout() {
        let source = include_str!("memoria_compact.rs");
        let fn_start = source
            .find("pub fn new(base_url: String, api_key: String)")
            .expect("HttpMemoriaClient::new must exist");
        let body = &source[fn_start..fn_start + 400];
        assert!(
            body.contains("connect_timeout("),
            "HttpMemoriaClient must set connect_timeout"
        );
        assert!(
            body.contains(".timeout("),
            "HttpMemoriaClient must set request timeout"
        );
    }
}
