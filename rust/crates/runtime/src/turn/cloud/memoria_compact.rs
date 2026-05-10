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
    /// Turn number used to tag observatory records. `0` is a valid
    /// pre-turn value — compaction can fire before turn 1 on warm
    /// sessions — so callers supply the current turn explicitly when
    /// wiring observatory; callers that don't bother may leave it 0.
    pub turn_number: u32,
    /// Optional post-hoc observer. When `Some`, one
    /// [`InjectionRecord`] per compaction lands in the ring. `None`
    /// (the default for tests and offline call sites) is a
    /// zero-overhead no-op — no clones, no mutex acquires.
    pub observatory: Option<std::sync::Arc<crate::session_memory::SessionMemoryObservatory>>,
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
            turn_number: 0,
            observatory: None,
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
        let mem = astra_core::MemoriaSettings::from_env();
        Some(Self::new(mem.base_url, mem.master_key?))
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

/// Build a working memory summary from recent messages.
fn build_working_memory_content(messages: &[Value], max_chars: usize) -> String {
    let mut parts = Vec::new();
    let mut total_chars = 0;

    // Extract key information from recent messages.
    //
    // Write-time gate: route every candidate through
    // `should_store_in_memory`. That predicate composes the
    // scaffolding-message check (runtime-injected nudges / attention
    // manifests / correction headers / tool-rollups) with the
    // ephemeral-ack length gate (rejects "继续啊", "修复", "hi",
    // "好", "ok" and similar short user inputs that carry no durable
    // signal).
    //
    // Both filters run at WRITE time so Memoria never indexes them —
    // read-time filters (is_memory_worthy / is_digest_worthy) are now
    // defense-in-depth for legacy-polluted sessions, not the primary
    // cleanup path. Single source of truth:
    // `astra_turn_types::should_store_in_memory`.
    //
    // Systematic rather than whack-a-mole: Claude Code's memdir design
    // (see docs/design/memoria-compared-to-claude-code.md) makes the
    // type-and-description frontmatter mandatory at store time; this
    // is L1 of porting that principle — reject obvious non-memories
    // before they ever reach the index. L2 (require [@ns/type] prefix
    // on stored bodies) and L3 (replace Memoria with file-based
    // memdir) are follow-ups.
    for msg in messages
        .iter()
        .rev()
        .filter(|m| astra_turn_types::should_store_in_memory(m))
        .take(10)
    {
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

/// Build the `[@episode/compaction]`-tagged memory body for storing a
/// compaction summary as semantic memory.
///
/// The LLM summary is usually multi-paragraph; we need a one-line
/// abstract (30–150 chars) for the compact view plus the full summary
/// as detail so future sessions can `memory_expand` on demand.
///
/// Strategy:
/// 1. Take the first sentence (or line). If it fits 30–150 chars after
///    collapsing whitespace, use it verbatim as the abstract.
/// 2. Otherwise synthesize a deterministic fallback:
///    `"Compaction of session <sid-prefix>: <N>-char summary"`.
///
/// Returns `None` when the summary is empty — caller skips the store.
fn build_compaction_layered_body(session_id: &str, summary: &str) -> Option<String> {
    let summary_trimmed = summary.trim();
    if summary_trimmed.is_empty() {
        return None;
    }

    let abstract_ = compaction_abstract_from_summary(session_id, summary_trimmed);
    let detail = format!("session={session_id}\n\n{summary_trimmed}");

    Some(
        astra_prompts::memory_proto::MemoryEntry::new(
            astra_prompts::memory_proto::NS_EPISODE,
            "compaction",
            &astra_prompts::memory_proto::encode_body_layers(&abstract_, None, Some(&detail)),
        )
        .encode(),
    )
}

/// Try the summary's first sentence as the abstract; fall back to a
/// deterministic count-based line if the sentence doesn't fit.
fn compaction_abstract_from_summary(session_id: &str, summary: &str) -> String {
    let min = astra_prompts::memory_proto::ABSTRACT_MIN_CHARS;
    let max = astra_prompts::memory_proto::ABSTRACT_MAX_CHARS;

    // First-sentence candidate: everything up to the first `. `, `。`,
    // `\n`, or EOF — whichever comes first. Collapsed whitespace, no
    // leading/trailing space.
    let first = first_sentence(summary);
    let first_chars = first.chars().count();
    if (min..=max).contains(&first_chars) {
        return first;
    }

    // Fallback: deterministic, bounded. `session_id` is truncated to
    // 12 chars (same convention as session_end_governance).
    let sid_short: String = session_id.chars().take(12).collect();
    let summary_chars = summary.chars().count();
    format!("Compaction of session {sid_short}: {summary_chars}-char summary")
}

fn first_sentence(s: &str) -> String {
    // Terminators: `\n`, `。`, or `.` followed by whitespace/EOF.
    // The `.` case needs lookahead — `v1.0` shouldn't split at the
    // dot, but `axum over actix.` should.
    let terminator_pos = s
        .char_indices()
        .find_map(|(i, c)| {
            let is_terminator = match c {
                '\n' | '。' => true,
                '.' => s[i + c.len_utf8()..]
                    .chars()
                    .next()
                    .is_none_or(|n| n.is_whitespace() || n == '\n'),
                _ => false,
            };
            is_terminator.then_some(i)
        })
        .unwrap_or(s.len());

    s[..terminator_pos]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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

    // Step 2: Build context summary from retrieved Memoria memories.
    // After wip-3 the runtime no longer injects a session anchor /
    // facts-first narrative — the compacted message stream speaks for
    // itself.
    let memory_context = build_memory_context(&memories, memory_max_tokens);
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
        let inj_summary = format!("Memoria: {} memories retrieved", memories.len());
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
            "[compact] Session context injected ({} memories, {} est. tokens)",
            memories.len(),
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

                // Step 6b: Store compaction summary as semantic memory
                // for cross-session retrieval. Wrapped in the
                // `[@episode/compaction]` structural envelope with a
                // layered body — abstract = first sentence (or a
                // deterministic fallback), detail = full summary. The
                // abstract is what the compact view ships to future
                // sessions, so it stays within the 150-char cap even
                // when summaries are long.
                if config.store_on_compact
                    && let Some(semantic_content) = build_compaction_layered_body(sid, &summary)
                {
                    match astra_turn_types::should_store_persistent_memory(
                        &semantic_content,
                        "semantic",
                    ) {
                        Ok(()) => {
                            if let Err(e) = client
                                .store(
                                    &semantic_content,
                                    "semantic",
                                    Some(sid),
                                    Some(astra_prompts::memory_proto::TIER_INFERRED),
                                )
                                .await
                            {
                                eprintln!(
                                    "[compact] Failed to store compaction summary as semantic: {e}"
                                );
                            }
                        }
                        Err(reason) => {
                            eprintln!("[compact] L2 rejected compaction summary write: {reason}");
                        }
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

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
            session_facts: None,
            turn_number: 0,
            observatory: None,
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
            session_facts: None,
            turn_number: 0,
            observatory: None,
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
            session_facts: None,
            turn_number: 0,
            observatory: None,
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
            session_facts: None,
            turn_number: 0,
            observatory: None,
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
            session_facts: None,
            turn_number: 0,
            observatory: None,
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
            session_facts: None,
            turn_number: 0,
            observatory: None,
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
            session_facts: None,
            turn_number: 0,
            observatory: None,
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
    // build_working_memory_content
    // ──────────────────────────────────────────────────────────

    #[test]
    fn working_memory_empty_messages() {
        assert!(build_working_memory_content(&[], 1000).is_empty());
    }

    #[test]
    fn working_memory_user_and_assistant() {
        // User messages must clear the should_store_in_memory length
        // gate (20+ unicode scalars), else they're treated as
        // ephemeral acks and skipped. Use realistic prose.
        let msgs = vec![
            user("please review the auth middleware refactor"),
            assistant("Reviewed. Found one issue with the token compare path."),
        ];
        let r = build_working_memory_content(&msgs, 10000);
        assert!(r.contains("review the auth middleware"));
        assert!(r.contains("Reviewed. Found one issue"));
    }

    #[test]
    fn working_memory_skips_tool_role() {
        let tool_msg = json!({"role": "tool", "content": "tool output", "tool_call_id": "t1"});
        // Long user msg passes the length gate so we have signal in the
        // output; assertion is on tool_msg NOT appearing.
        let msgs = vec![
            user("look up the current build state of the repo"),
            tool_msg,
            assistant("Build is green."),
        ];
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
        // User msg long enough to pass the write-time gate, so the
        // assistant tool_calls line has a preceding user line to
        // anchor the format.
        let msgs = vec![user("kick off the build and report the result"), a];
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

    // ── Scaffolding filter (closes runtime→Memoria feedback loop) ─────
    // The volatile block was being polluted by Memoria retrieving back
    // the runtime's own scaffolding messages from prior turns. Root
    // cause was `build_working_memory_content` walking messages[] and
    // emitting every user/assistant role line into the stored working-
    // memory blob. When a later turn's retrieval matched any of those
    // lines, they returned as `**Context:** …` memories. Fix: filter
    // `is_runtime_scaffolding_message` before storing. The retrieval-
    // time filter is defense-in-depth; the real cut is here, at the
    // write path.

    #[test]
    fn working_memory_skips_parallel_feedback_nudge() {
        let msgs = vec![
            user("review the latest commits"),
            assistant("✓ Previous round: 2 tools executed in parallel — excellent."),
            assistant("Here are the three commits…"),
        ];
        let r = build_working_memory_content(&msgs, 10000);
        assert!(
            !r.contains("Previous round"),
            "parallel-feedback nudge must not reach Memoria: {r}"
        );
        assert!(r.contains("review the latest commits"));
        assert!(r.contains("three commits"));
    }

    #[test]
    fn working_memory_skips_runtime_correction_headers() {
        // Long enough user msgs to pass the write-time length gate —
        // these carry real intent that's worth storing. The short
        // acks "continue" / "fix it" are now (correctly) rejected as
        // ephemeral, but the scaffolding-filter assertion below still
        // holds.
        let msgs = vec![
            user("please continue from where the last turn left off"),
            assistant("## ⤴ Execution Escalation Runtime correction: ten read-only calls"),
            assistant("## ⚠ Sequential Tool Calls Detected. Last 4 rounds each ran one tool."),
            user("fix the broken migration and verify it runs cleanly"),
        ];
        let r = build_working_memory_content(&msgs, 10000);
        assert!(!r.contains("⤴"), "correction header leaked: {r}");
        assert!(!r.contains("Sequential Tool Calls Detected"), "leaked: {r}");
        assert!(r.contains("continue from where"));
        assert!(r.contains("broken migration"));
    }

    #[test]
    fn working_memory_skips_verification_and_error_budget() {
        let msgs = vec![
            user("⚠️ VERIFICATION REQUIRED: Before you finish"),
            user("🔄 ERROR BUDGET EXHAUSTED: hit 3 errors"),
            user("skip the verification nudges and just fix the test assertion"),
        ];
        let r = build_working_memory_content(&msgs, 10000);
        assert!(!r.contains("VERIFICATION REQUIRED"), "leaked: {r}");
        assert!(!r.contains("ERROR BUDGET"), "leaked: {r}");
        assert!(r.contains("just fix the test assertion"));
    }

    #[test]
    fn working_memory_skips_tools_used_rollup() {
        let msgs = vec![
            user("explore the repo and summarize the top-level layout"),
            assistant("Tools used: bash, grep, read_file"),
            assistant("Found three relevant files."),
        ];
        let r = build_working_memory_content(&msgs, 10000);
        assert!(!r.contains("Tools used:"), "rollup leaked: {r}");
        assert!(r.contains("three relevant files"));
    }

    #[test]
    fn working_memory_skips_system_role_even_if_it_somehow_appears() {
        // Defensive: system messages should never reach compaction, but
        // if they do, they are scaffolding by definition.
        let sys = json!({"role": "system", "content": "runtime injected guidance"});
        let msgs = vec![
            sys,
            user("please walk through the pipeline compaction logic"),
        ];
        let r = build_working_memory_content(&msgs, 10000);
        assert!(!r.contains("runtime injected"), "system leaked: {r}");
        assert!(r.contains("pipeline compaction logic"));
    }

    #[test]
    fn working_memory_pure_scaffolding_produces_empty_output() {
        // When every message is scaffolding, nothing should be stored —
        // Memoria must not receive a `[session:…] Recent conversation:\n`
        // wrapper with no body, which would just be noise in the index.
        let msgs = vec![
            user("## ⤴ Runtime correction: ten calls"),
            assistant("Tools used: bash"),
            user("✓ Previous round: 2 tools in parallel"),
        ];
        let r = build_working_memory_content(&msgs, 10000);
        assert!(
            r.is_empty(),
            "pure-scaffolding input must yield empty working memory: {r:?}"
        );
    }

    // ── L1 memory-writability gate ────────────────────────────────────
    // `should_store_in_memory` rejects short user messages (below 20
    // unicode scalars) as ephemeral acks / imperatives. Regression
    // for session `c6e18730` where "继续啊", "修复啊！", "hi", "好"
    // polluted Memoria's index on every compaction write. Real
    // signal-bearing user messages still pass through — the gate is
    // length-based, not prefix-based, because the bad content had no
    // consistent prefix to filter on.

    #[test]
    fn working_memory_rejects_short_user_acks() {
        let msgs = vec![
            user("hi"),       // English single-word
            user("好"),       // CJK single char
            user("继续啊"),   // CJK 3 chars + particle
            user("修复啊！"), // CJK 3 chars + punctuation
            user("ok"),
            user("yes"),
            user("continue"),
            user("just fix it"), // 11 chars — still below threshold
        ];
        let r = build_working_memory_content(&msgs, 10000);
        assert!(
            r.is_empty(),
            "short ephemeral user acks must not reach Memoria; got: {r:?}"
        );
    }

    #[test]
    fn working_memory_keeps_substantive_user_intent() {
        // Long user messages — the kind that carry durable signal —
        // pass through unchanged. The gate is narrow by design.
        let msgs = vec![
            user(
                "Add OAuth2 support with JWT tokens and refresh-token rotation, \
                 using RS256 for signing.",
            ),
            user(
                "Focus on the auth middleware path, not the schema migration \
                 that's already in flight.",
            ),
        ];
        let r = build_working_memory_content(&msgs, 10000);
        assert!(r.contains("Add OAuth2 support"));
        assert!(r.contains("auth middleware path"));
    }

    #[test]
    fn working_memory_mixed_user_acks_and_signal() {
        // Realistic mixed sequence from session `c6e18730`: short
        // imperatives interleaved with substantive requests. Only the
        // substantive ones survive into working memory.
        let msgs = vec![
            user("continue"),                                       // reject
            user("please review the delegation fan-out code path"), // keep
            assistant("Reviewed — three potential issues."),        // keep
            user("修复啊！"),                                       // reject
            user("fix the ordering bug in the prefix-store write"), // keep
        ];
        let r = build_working_memory_content(&msgs, 10000);
        // Kept content present
        assert!(r.contains("delegation fan-out"));
        assert!(r.contains("three potential issues"));
        assert!(r.contains("ordering bug"));
        // Rejected content absent
        assert!(
            !r.contains("User: continue"),
            "short ack 'continue' leaked into output: {r}"
        );
        assert!(
            !r.contains("修复啊"),
            "short CJK imperative leaked into output: {r}"
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
            session_facts: None,
            turn_number: 0,
            observatory: None,
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
        // L2 structural envelope + layered body. The abstract is
        // drawn from the summary's first sentence (or a deterministic
        // fallback when that doesn't fit 30–150 chars). The summary
        // text always lives in detail, so we assert there — the
        // pipeline sometimes prefixes a section-hint warning which
        // would otherwise dominate the abstract.
        assert!(
            content.starts_with("[@episode/compaction]"),
            "should have L2 structural envelope, got: {}",
            &content[..50.min(content.len())]
        );
        let entry =
            astra_prompts::memory_proto::MemoryEntry::parse(content).expect("wire form must parse");
        let abs_chars = entry.abstract_layer().chars().count();
        assert!(
            (30..=150).contains(&abs_chars),
            "abstract must clear the L2 gate, got {} chars: {}",
            abs_chars,
            entry.abstract_layer(),
        );
        let detail = entry.detail_layer().expect("detail layer emitted");
        assert!(
            detail.contains("session=sess-test-42"),
            "detail should embed the session id"
        );
        assert!(detail.contains("JWT"), "detail must carry the summary text");
        // The stored content must pass the L2 gate by construction;
        // if a future refactor weakens the envelope this assertion
        // catches it immediately.
        assert!(
            astra_turn_types::should_store_persistent_memory(content, "semantic").is_ok(),
            "auto-stored compaction summary must satisfy L2 gate"
        );
    }

    // Unit-level coverage for the summary→layered-body helper. Direct
    // tests for each branch so future changes to the fallback rule
    // don't silently regress.

    #[test]
    fn compaction_body_uses_first_sentence_when_it_fits() {
        // 30–150 chars, ends with `. ` → verbatim abstract.
        let sid = "sess-xyz";
        let summary =
            "User picked axum over actix for its tower stack. Then wired sqlx for persistence.";
        let body = build_compaction_layered_body(sid, summary).unwrap();
        let entry = astra_prompts::memory_proto::MemoryEntry::parse(&body).unwrap();
        assert_eq!(
            entry.abstract_layer(),
            "User picked axum over actix for its tower stack"
        );
        assert!(entry.detail_layer().unwrap().contains(summary));
    }

    #[test]
    fn compaction_body_falls_back_when_first_sentence_too_short() {
        // First sentence is 14 chars — under the 30-char minimum.
        // Fallback abstract must take over and still pass the L2 gate.
        let sid = "sess-short";
        let summary = "OK done. Details: we refactored the auth path, added refresh rotation, and migrated the session table to MatrixOne.";
        let body = build_compaction_layered_body(sid, summary).unwrap();
        assert!(astra_turn_types::should_store_persistent_memory(&body, "semantic").is_ok());
        let entry = astra_prompts::memory_proto::MemoryEntry::parse(&body).unwrap();
        // Fallback format is stable.
        assert!(
            entry.abstract_layer().starts_with("Compaction of session"),
            "got: {}",
            entry.abstract_layer()
        );
    }

    #[test]
    fn compaction_body_falls_back_when_first_sentence_too_long() {
        // No `. ` terminator and no newlines → the whole summary is
        // the "first sentence", which is likely way over 150 chars.
        let sid = "sess-long";
        let summary = "a".repeat(500);
        let body = build_compaction_layered_body(sid, &summary).unwrap();
        assert!(astra_turn_types::should_store_persistent_memory(&body, "semantic").is_ok());
        let entry = astra_prompts::memory_proto::MemoryEntry::parse(&body).unwrap();
        assert!(entry.abstract_layer().starts_with("Compaction of session"));
        assert!(
            entry.abstract_layer().chars().count()
                <= astra_prompts::memory_proto::ABSTRACT_MAX_CHARS
        );
    }

    #[test]
    fn compaction_body_returns_none_for_empty_summary() {
        assert!(build_compaction_layered_body("sid", "").is_none());
        assert!(build_compaction_layered_body("sid", "   \n  ").is_none());
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
            session_facts: None,
            turn_number: 0,
            observatory: None,
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
