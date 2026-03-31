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
            http: reqwest::Client::new(),
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

fn truncate_str(s: &str, max_chars: usize) -> &str {
    if s.len() <= max_chars {
        s
    } else {
        &s[..max_chars]
    }
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
    let query = "current session context working memory";
    let memories = client
        .retrieve(query, Some(sid), config.max_memories)
        .await
        .unwrap_or_default();

    // Step 2: Build context summary
    let memory_context = build_memory_context(&memories, config.max_memory_tokens);
    let has_memory_context = !memory_context.is_empty();

    // Step 3: Apply truncation
    let mut result = compact_tiered_with_result(
        messages,
        params.budget_chars,
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
        if let Some(ref mut boundary) = result.boundary {
            boundary.summary = Some(format!("Memoria: {} memories retrieved", memories.len()));
        } else {
            result.boundary = Some(
                CompactBoundary::new(CompactTrigger::Auto, params.tier)
                    .with_pre_metrics(params.current_tokens, messages.len())
                    .with_post_count(result.messages.len()),
            );
            if let Some(ref mut b) = result.boundary {
                b.summary = Some(format!("Memoria: {} memories retrieved", memories.len()));
            }
        }

        eprintln!(
            "[compact] Memoria context injected ({} memories, {} tokens)",
            memories.len(),
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

                eprintln!("[compact] LLM summary generated ({} chars)", summary.len());
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
}
