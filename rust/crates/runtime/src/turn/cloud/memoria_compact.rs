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
//! ## On-disk session memory (Claude Code–compatible)
//!
//! When `MO_AGENT_SESSION_MEMORY_COMBINE` is set, the compactor can read
//! `CLAUDE_CONFIG_DIR/projects/<sanitized-cwd>/<session_id>/session-memory/summary.md`
//! (same layout as Claude Code) or a path from `MO_AGENT_SESSION_MEMORY_FILE`.
//! - `fallback`: use the file only if Memoria returns no memories.
//! - `merge` / `true` / `1` / `both`: keep Memoria hits and add a capped file excerpt.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::compaction::{
    CompactBoundary, CompactResult, CompactTrigger, compact_tiered_with_result,
};
use super::summary::SummaryLlmClient;
use crate::prompts::{CompactConfig, CompactionTier};

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

/// How to mix Claude Code–style on-disk `summary.md` with Memoria HTTP retrieval.
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
    /// Parse `MO_AGENT_SESSION_MEMORY_COMBINE` (`merge` | `fallback` | `true` | `1` | `both`).
    pub fn from_env() -> Self {
        let Ok(raw) = std::env::var("MO_AGENT_SESSION_MEMORY_COMBINE") else {
            return Self::None;
        };
        match raw.to_ascii_lowercase().as_str() {
            "merge" | "1" | "true" | "yes" | "both" => Self::Merge,
            "fallback" => Self::Fallback,
            _ => Self::None,
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
    /// Optional path to on-disk session memory (e.g. Claude Code `session-memory/summary.md`).
    pub session_memory_file: Option<PathBuf>,
    /// How to combine that file with Memoria retrieval.
    pub session_memory_combine: SessionMemoryFileCombine,
}

// ---------------------------------------------------------------------------
// Claude Code–compatible session memory paths
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
/// matching Claude Code’s `sanitizePath` (alphanumeric → keep, else `-`, length cap + djb2).
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

/// Resolve on-disk session memory path and combine mode from env + optional workspace cwd.
///
/// - `MO_AGENT_SESSION_MEMORY_FILE` → explicit file; if set, defaults combine mode to [`Merge`]
///   when `MO_AGENT_SESSION_MEMORY_COMBINE` is unset.
/// - Otherwise, when combine is `merge` or `fallback`, uses [`claude_code_session_memory_path`]
///   if `cwd` is present.
pub fn resolve_session_memory_file_options(
    session_id: &str,
    cwd: Option<&str>,
) -> (Option<PathBuf>, SessionMemoryFileCombine) {
    let env_combine = SessionMemoryFileCombine::from_env();

    if let Ok(p) = std::env::var("MO_AGENT_SESSION_MEMORY_FILE") {
        let path = PathBuf::from(p);
        let combine = if env_combine == SessionMemoryFileCombine::None {
            SessionMemoryFileCombine::Merge
        } else {
            env_combine
        };
        return (Some(path), combine);
    }

    if env_combine == SessionMemoryFileCombine::None {
        return (None, SessionMemoryFileCombine::None);
    }

    let Some(cwd) = cwd.filter(|s| !s.is_empty()) else {
        return (None, env_combine);
    };

    (
        Some(claude_code_session_memory_path(cwd, session_id)),
        env_combine,
    )
}

// ---------------------------------------------------------------------------
// Memoria Client Trait
// ---------------------------------------------------------------------------

/// Trait for Memoria HTTP operations (allows mocking in tests).
#[async_trait::async_trait]
pub trait MemoriaClient: Send + Sync {
    /// Retrieve memories for a query.
    async fn retrieve(
        &self,
        query: &str,
        session_id: Option<&str>,
        top_k: usize,
    ) -> Result<Vec<MemoriaMemory>, String>;

    /// Store a memory.
    async fn store(
        &self,
        content: &str,
        memory_type: &str,
        session_id: Option<&str>,
    ) -> Result<String, String>;

    /// Purge working memories for a session.
    async fn purge_working(&self, session_id: &str) -> Result<u64, String>;
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
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    /// Create from environment variables.
    pub fn from_env() -> Option<Self> {
        let base_url = std::env::var("MEMORIA_BASE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8100".to_string());
        let api_key = std::env::var("MEMORIA_API_KEY")
            .or_else(|_| std::env::var("MEMORIA_MASTER_KEY"))
            .ok()?;
        Some(Self::new(base_url, api_key))
    }
}

#[async_trait::async_trait]
impl MemoriaClient for HttpMemoriaClient {
    async fn retrieve(
        &self,
        query: &str,
        session_id: Option<&str>,
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
        if let Some(sid) = session_id {
            body["session_id"] = json!(sid);
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

        let memories = data
            .get("memories")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| serde_json::from_value(v.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();

        Ok(memories)
    }

    async fn store(
        &self,
        content: &str,
        memory_type: &str,
        session_id: Option<&str>,
    ) -> Result<String, String> {
        let url = format!("{}/v1/memories", self.base_url.trim_end_matches('/'));
        let mut body = json!({
            "content": content,
            "memory_type": memory_type,
        });
        if let Some(sid) = session_id {
            body["session_id"] = json!(sid);
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
    format!(
        "[Session memory — on-disk summary]\n{body}\n[End on-disk session memory]"
    )
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

fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    // Walk back from max_bytes to find a valid char boundary
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
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
    budget_chars.saturating_sub(
        memory_content_chars
            .saturating_add(summary_reserve_chars),
    )
}

fn truncate_summary_for_budget(summary: String, summary_token_budget: usize) -> String {
    let max_chars = summary_token_budget.saturating_mul(4).max(256);
    if summary.chars().count() <= max_chars {
        summary
    } else {
        summary
            .chars()
            .take(max_chars)
            .collect::<String>()
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

    let user_focus = window.iter().rev().find_map(message_user_text).map(|s| {
        collapse_whitespace(&truncate_chars_prefix(&s, MAX_USER_CHARS))
    });

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

    let client = client.unwrap();
    let sid = session_id.unwrap();

    // Step 1: Retrieve session context from Memoria
    let query = memoria_compact_retrieve_query(messages);
    let memories = client
        .retrieve(&query, Some(sid), config.max_memories)
        .await
        .unwrap_or_default();

    let file_text = params
        .session_memory_file
        .as_ref()
        .and_then(|p| read_session_memory_file(p));
    let had_on_disk_session_memory = file_text.is_some();

    let will_summarize = compact_config
        .zip(summary_client.as_ref())
        .is_some_and(|(cfg, _)| cfg.should_summarize(params.tier));
    let summary_token_budget = compact_config
        .map(|c| c.summary_token_budget)
        .unwrap_or(0);

    let (memory_max_tokens, summary_reserve_chars) = plan_injection_reservations(
        params.budget_chars,
        will_summarize,
        summary_token_budget,
        config.max_memory_tokens,
    );

    // Step 2: Build context summary (token cap unified with summary reservation)
    let memory_context = build_session_context_with_optional_file(
        &memories,
        file_text.as_deref(),
        params.session_memory_combine,
        memory_max_tokens,
    );
    let has_memory_context = !memory_context.is_empty();
    let memory_chars = memory_context.chars().count();

    let adjusted_budget_chars = adjusted_message_budget_chars(
        params.budget_chars,
        memory_chars,
        summary_reserve_chars,
    );

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

    // Step 5: Optionally store updated working memory
    if config.store_on_compact && has_memory_context {
        let working_content = build_working_memory_content(messages, 2000);
        if !working_content.is_empty() {
            let store_content = format!(
                "[session:{}] Recent conversation:\n{}",
                sid, working_content
            );
            if let Err(e) = client.store(&store_content, "working", Some(sid)).await {
                eprintln!("[compact] Failed to store working memory: {e}");
            }
        }
    }

    // Step 6: Optionally generate LLM summary
    if let Some(cfg) = compact_config
        && let Some(s_client) = summary_client
        && cfg.should_summarize(params.tier)
    {
        match super::summary::generate_compact_summary(messages, s_client).await {
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
    use std::sync::Mutex;

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
        async fn retrieve(
            &self,
            _query: &str,
            _session_id: Option<&str>,
            _top_k: usize,
        ) -> Result<Vec<MemoriaMemory>, String> {
            Ok(self.memories.lock().unwrap().clone())
        }

        async fn store(
            &self,
            content: &str,
            memory_type: &str,
            _session_id: Option<&str>,
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
        assert!(mem_tok > 0, "memory token cap should be positive: {mem_tok}");
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

        // Check for context injection
        let has_context = result.messages.iter().any(|m| {
            m.get("content")
                .and_then(Value::as_str)
                .map(|s| s.contains("[Session Context from Memory]"))
                .unwrap_or(false)
        });
        assert!(has_context);

        // Should have stored working memory
        let stored = mock.stored.lock().unwrap();
        assert!(!stored.is_empty());
        assert_eq!(stored[0].1, "working");
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
    impl super::super::summary::SummaryLlmClient for MockSummaryClient {
        async fn summarize(
            &self,
            _messages: &[Value],
        ) -> Result<super::super::summary::SummaryResponse, String> {
            match self.response.lock().unwrap().as_ref() {
                Some(text) => Ok(super::super::summary::SummaryResponse {
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
            Some(&summary_client as &dyn super::super::summary::SummaryLlmClient),
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
        let config = MemoriaCompactConfig::default();
        let params = MemoriaCompactParams {
            budget_chars: 10000,
            keep_chars: 2000,
            tier: CompactionTier::AggressivePrune,
            keep_recent_turns: 4,
            current_tokens: 1000,
            session_memory_file: None,
            session_memory_combine: SessionMemoryFileCombine::None,
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
            None,
            Some(&compact_config),
            Some(&summary_client as &dyn super::super::summary::SummaryLlmClient),
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
        let config = MemoriaCompactConfig::default();
        let params = MemoriaCompactParams {
            budget_chars: 10000,
            keep_chars: 2000,
            tier: CompactionTier::AggressivePrune,
            keep_recent_turns: 4,
            current_tokens: 1000,
            session_memory_file: None,
            session_memory_combine: SessionMemoryFileCombine::None,
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
            None,
            Some(&compact_config),
            Some(&summary_client as &dyn super::super::summary::SummaryLlmClient),
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
        let config = MemoriaCompactConfig::default();
        let params = MemoriaCompactParams {
            budget_chars: 10000,
            keep_chars: 2000,
            tier: CompactionTier::TrimSchemas, // Below AggressivePrune threshold
            keep_recent_turns: 4,
            current_tokens: 1000,
            session_memory_file: None,
            session_memory_combine: SessionMemoryFileCombine::None,
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
            None,
            Some(&compact_config),
            Some(&summary_client as &dyn super::super::summary::SummaryLlmClient),
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
}
