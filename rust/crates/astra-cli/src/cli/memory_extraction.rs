//! Background memory extraction agent.
//!
//! After each successful turn, analyzes the conversation and automatically
//! stores durable memories (user preferences, feedback, project context,
//! references) via Memoria. Inspired by Claude Code's `extractMemories`.
//!
//! Key invariants:
//! - **Mutual exclusion**: if the main model already called `memory_store`
//!   this turn, extraction is skipped (no double-writes).
//! - **Fire-and-forget**: never blocks the next user input.
//! - **Single in-flight**: at most one extraction runs at a time.
//! - **Quality gate**: extracted content must pass `is_high_quality_lesson`.
//! - **Auditable**: every run emits a `[memory-extraction]` stderr line
//!   and a `MemoryExtraction` journal event.

use std::sync::Arc;

use astra_prompts::memory_types::MemoryCategory;
use astra_runtime::memory_relevance::LlmConnParams;
use astra_turn_core::fork_prefix::ForkPrefix;

/// Extraction system prompt — teaches the selector model the 4 user-facing
/// memory types and what NOT to save.
pub const EXTRACTION_SYSTEM_PROMPT: &str = "\
You are analyzing a conversation turn to extract durable memories.
Return a JSON array of memories worth persisting. Each object has:
  {\"type\": \"<user|feedback|project|ref>\", \"content\": \"<concise fact>\"}

Types:
- user: role, preferences, knowledge (\"I'm a data scientist\", \"I prefer Rust\")
- feedback: corrections or confirmations (\"don't mock the DB\", \"yes, that approach works\")
- project: deadlines, decisions, incidents (\"merge freeze May 8\", \"auth rewrite for compliance\")
- ref: external system pointers (\"bugs in Linear project INGEST\", \"dashboard at grafana.internal/d/api-latency\")

Do NOT extract:
- Code patterns or architecture (derivable from the codebase)
- Git history or file paths
- Ephemeral debug context or temporary state
- Things the user did NOT say (don't infer preferences from tool usage)

Return [] if nothing is worth remembering. Be selective — false negatives are better than noise.";

/// Build the user-turn content for extraction.
pub fn build_extraction_query(
    user_message: &str,
    assistant_response: &str,
    existing_manifest: &str,
) -> String {
    let mut s = String::with_capacity(1500);
    s.push_str("Recent conversation:\n\nUser: ");
    s.push_str(&truncate(user_message, 500));
    s.push_str("\n\nAssistant: ");
    s.push_str(&truncate(assistant_response, 500));
    if !existing_manifest.is_empty() {
        s.push_str("\n\nExisting memories (avoid duplicates):\n");
        s.push_str(&truncate(existing_manifest, 500));
    }
    s.push_str("\n\nExtract memories as JSON array:");
    s
}

/// A single extracted memory ready for Memoria storage.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtractedMemory {
    pub category: MemoryCategory,
    pub content: String,
}

/// Parse the selector model's extraction response.
/// Handles JSON array, markdown-wrapped, and malformed responses.
pub fn parse_extraction_response(response: &str) -> Vec<ExtractedMemory> {
    let trimmed = response.trim();

    let clean = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|s| s.strip_suffix("```"))
        .unwrap_or(trimmed)
        .trim();

    let arr: Vec<serde_json::Value> = match serde_json::from_str(clean) {
        Ok(a) => a,
        Err(_) => return Vec::new(),
    };

    arr.into_iter()
        .filter_map(|v| {
            let type_str = v.get("type")?.as_str()?;
            let content = v.get("content")?.as_str()?.trim().to_string();
            if content.is_empty() {
                return None;
            }
            let category = match type_str {
                "user" => MemoryCategory::User,
                "feedback" => MemoryCategory::Feedback,
                "project" => MemoryCategory::Project,
                "ref" | "reference" => MemoryCategory::Reference,
                _ => return None,
            };
            Some(ExtractedMemory { category, content })
        })
        .collect()
}

/// Check if the main model already wrote to memory this turn.
pub fn main_model_wrote_memory(tools_used: &[String]) -> bool {
    tools_used.iter().any(|t| t == "memory_store")
}

/// Outcome of an extraction attempt, for journal/audit.
#[derive(Debug, Clone)]
pub enum ExtractionOutcome {
    /// Background task fired — actual results available via `drain()`.
    Started,
    /// Background task completed with results.
    Extracted {
        count: usize,
        categories: Vec<String>,
        duration_ms: u64,
        /// Whether the extraction reused a fork prefix for cache sharing.
        prefix_reused: bool,
    },
    SkippedMainWrote,
    SkippedNoSelector,
    SkippedDisabled,
    Error(String),
}

impl ExtractionOutcome {
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Extracted { .. } => "extracted",
            Self::SkippedMainWrote => "skipped_main_wrote",
            Self::SkippedNoSelector => "skipped_no_selector",
            Self::SkippedDisabled => "skipped_disabled",
            Self::Error(_) => "error",
        }
    }
}

/// Input context for a single extraction attempt.
pub struct ExtractionContext<'a> {
    pub turn: u32,
    pub selector_params: Option<&'a LlmConnParams>,
    pub user_message: &'a str,
    pub assistant_response: &'a str,
    pub tools_used: &'a [String],
    pub session_id: Option<&'a str>,
    pub existing_manifest: &'a str,
    /// Fork prefix captured from the parent turn. When available AND the
    /// selector model matches the prefix's model/provider, extraction can
    /// reuse the parent's cached prefix instead of paying full input cost.
    /// Gated by `ASTRA_FORK_INHERIT_PREFIX`.
    pub fork_prefix: Option<Arc<ForkPrefix>>,
}

/// State for the background extraction agent.
pub struct MemoryExtractor {
    last_processed_turn: u32,
    in_flight: Option<tokio::task::JoinHandle<ExtractionOutcome>>,
    enabled: bool,
}

impl Default for MemoryExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryExtractor {
    pub fn new() -> Self {
        let enabled = std::env::var("ASTRA_DISABLE_AUTO_MEMORY")
            .map(|v| v != "1" && v != "true")
            .unwrap_or(true);
        Self {
            last_processed_turn: 0,
            in_flight: None,
            enabled,
        }
    }

    /// Check if extraction is in progress.
    pub fn is_busy(&self) -> bool {
        self.in_flight.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Fire extraction for the current turn.
    /// Returns immediately — actual extraction runs in background.
    pub fn maybe_extract(&mut self, ctx: ExtractionContext<'_>) -> ExtractionOutcome {
        if !self.enabled {
            return ExtractionOutcome::SkippedDisabled;
        }
        if ctx.turn <= self.last_processed_turn {
            return ExtractionOutcome::SkippedDisabled;
        }
        if self.is_busy() {
            return ExtractionOutcome::SkippedDisabled;
        }
        if main_model_wrote_memory(ctx.tools_used) {
            self.last_processed_turn = ctx.turn;
            return ExtractionOutcome::SkippedMainWrote;
        }
        let Some(params) = ctx.selector_params.cloned() else {
            self.last_processed_turn = ctx.turn;
            return ExtractionOutcome::SkippedNoSelector;
        };

        self.last_processed_turn = ctx.turn;
        let query = build_extraction_query(
            ctx.user_message,
            ctx.assistant_response,
            ctx.existing_manifest,
        );
        let sid = ctx.session_id.map(String::from);

        let resolved_prefix = resolve_prefix_for_extraction(ctx.fork_prefix.as_ref(), &params);

        let handle = tokio::spawn(async move {
            run_extraction(&params, &query, sid.as_deref(), resolved_prefix).await
        });
        self.in_flight = Some(handle);
        ExtractionOutcome::Started
    }

    /// Wait for in-flight extraction to complete (bounded timeout).
    /// On timeout, aborts the background task to prevent orphaned work.
    pub async fn drain(&mut self, timeout: std::time::Duration) -> Option<ExtractionOutcome> {
        let handle = self.in_flight.take()?;
        let abort_handle = handle.abort_handle();
        match tokio::time::timeout(timeout, handle).await {
            Ok(Ok(outcome)) => Some(outcome),
            Ok(Err(_)) => Some(ExtractionOutcome::Error("task panicked".into())),
            Err(_) => {
                abort_handle.abort();
                Some(ExtractionOutcome::Error("drain timeout".into()))
            }
        }
    }
}

/// Extraction has a much smaller context budget than the main turn.
/// Prefixes beyond this threshold are unlikely to yield cache hits
/// (the provider may have evicted them) and risk hitting input limits.
const EXTRACTION_PREFIX_MAX_BYTES: usize = 128 * 1024;

/// Build messages array using the fork prefix's canonical bytes as the
/// leading segment, with the extraction query appended as a user message
/// suffix.
///
/// Returns `None` when:
/// - Prefix bytes exceed `EXTRACTION_PREFIX_MAX_BYTES` (too large)
/// - Prefix bytes are not valid JSON or not an array (malformed capture)
///
/// The extraction system instruction is embedded in the user message
/// (not as a separate system block) so the parent's system blocks
/// remain the cache-leading segment — maximizing cache hit probability.
fn build_prefixed_messages(prefix: &ForkPrefix, query: &str) -> Option<serde_json::Value> {
    if prefix.size_bytes() > EXTRACTION_PREFIX_MAX_BYTES {
        return None;
    }

    let canonical = prefix.canonical_prefix_bytes();
    let parsed: serde_json::Value = serde_json::from_slice(canonical).ok()?;
    let arr = parsed.as_array()?;

    let mut messages = arr.clone();
    messages.push(serde_json::json!({
        "role": "user",
        "content": format!("{EXTRACTION_SYSTEM_PROMPT}\n\n{query}")
    }));
    Some(serde_json::Value::Array(messages))
}

/// Check whether a fork prefix can be reused for extraction.
///
/// Returns `Some(prefix)` only when ALL conditions are met:
/// 1. `ASTRA_FORK_INHERIT_PREFIX` feature flag is enabled
/// 2. A prefix is available from the parent turn's capture
/// 3. The selector model's provider matches the prefix's provider
/// 4. The selector model's model_name matches the prefix's model_id
/// 5. The prefix is not oversized (checked later in `build_prefixed_messages`)
fn resolve_prefix_for_extraction(
    prefix: Option<&Arc<ForkPrefix>>,
    params: &LlmConnParams,
) -> Option<Arc<ForkPrefix>> {
    use astra_turn_core::fork_capture::is_fork_inherit_prefix_enabled;
    use astra_turn_core::fork_prefix::ProviderKind;

    if !is_fork_inherit_prefix_enabled() {
        return None;
    }
    let prefix = prefix?;

    let selector_provider = ProviderKind::from_provider_hint(&params.provider);
    if prefix.provider != selector_provider {
        return None;
    }

    if prefix.model_id != params.model_name {
        return None;
    }

    Some(Arc::clone(prefix))
}

fn standalone_messages(query: &str) -> serde_json::Value {
    serde_json::json!([
        {"role": "system", "content": EXTRACTION_SYSTEM_PROMPT},
        {"role": "user", "content": query},
    ])
}

async fn run_extraction(
    params: &LlmConnParams,
    query: &str,
    session_id: Option<&str>,
    fork_prefix: Option<Arc<ForkPrefix>>,
) -> ExtractionOutcome {
    let start = std::time::Instant::now();

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .no_proxy()
        .build()
    {
        Ok(c) => c,
        Err(e) => return ExtractionOutcome::Error(format!("client build: {e}")),
    };

    let (messages, prefix_reused) = if let Some(prefix) = fork_prefix.as_ref() {
        match build_prefixed_messages(prefix, query) {
            Some(msgs) => {
                eprintln!(
                    "  [memory-extraction] reusing fork prefix ({}B cached)",
                    prefix.size_bytes()
                );
                (msgs, true)
            }
            None => {
                eprintln!(
                    "  [memory-extraction] prefix unusable ({}B, valid={}), falling back",
                    prefix.size_bytes(),
                    prefix.size_bytes() <= EXTRACTION_PREFIX_MAX_BYTES
                );
                (standalone_messages(query), false)
            }
        }
    } else {
        (standalone_messages(query), false)
    };

    let mut req_body = serde_json::json!({
        "model": params.model_name,
        "messages": messages,
        "max_tokens": 200,
        "temperature": 0.0,
    });
    {
        astra_turn_core::thinking_config::ThinkingConfig::Off.apply_openai_suppression(
            &mut req_body,
            &params.provider,
            &params.base_url,
        );
    }

    let resp = match client
        .post(format!("{}/chat/completions", params.base_url))
        .header("Authorization", format!("Bearer {}", params.api_key))
        .json(&req_body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return ExtractionOutcome::Error(format!("request: {e}")),
    };

    let body = resp.text().await.unwrap_or_default();
    let text = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            v.get("choices")?
                .get(0)?
                .get("message")?
                .get("content")?
                .as_str()
                .map(String::from)
        })
        .unwrap_or_default();

    // Strip <think> tags that native thinkers may emit despite suppression.
    // If stripping empties the text, fall back to the original (the model may
    // have wrapped the actual JSON inside think tags).
    let stripped = astra_turn_core::thinking_config::strip_think_tags(&text);
    let text = if stripped.trim().is_empty() {
        text
    } else {
        stripped
    };

    let extracted = parse_extraction_response(&text);
    let quality_filtered: Vec<ExtractedMemory> = extracted
        .into_iter()
        .filter(|m| astra_runtime::lesson_synthesizer::is_high_quality_lesson(&m.content))
        .collect();

    if quality_filtered.is_empty() {
        return ExtractionOutcome::Extracted {
            count: 0,
            categories: vec![],
            duration_ms: start.elapsed().as_millis() as u64,
            prefix_reused,
        };
    }

    let categories: Vec<String> = quality_filtered
        .iter()
        .map(|m| format!("{:?}", m.category).to_lowercase())
        .collect();

    let memories: Vec<serde_json::Value> = quality_filtered
        .iter()
        .map(|m| {
            let encoded = astra_prompts::memory_types::encode(m.category, &m.content);
            serde_json::json!({
                "content": encoded,
                "memory_type": m.category.memoria_type(),
                "trust_tier": m.category.trust_tier(),
                "session_id": session_id,
                "source": {"agent": "extraction"},
            })
        })
        .collect();

    let mem = astra_core::MemoriaSettings::from_env();
    let key = match mem.master_key {
        Some(k) => k,
        None => {
            return ExtractionOutcome::Extracted {
                count: quality_filtered.len(),
                categories,
                duration_ms: start.elapsed().as_millis() as u64,
                prefix_reused,
            };
        }
    };

    match client
        .post(format!("{}/v1/memories/batch", mem.base_url))
        .header("Authorization", format!("Bearer {key}"))
        .json(&serde_json::json!({ "memories": memories }))
        .send()
        .await
    {
        Ok(resp) if !resp.status().is_success() => {
            eprintln!(
                "  [memory-extraction] batch write failed ({})",
                resp.status()
            );
        }
        Err(e) => {
            eprintln!("  [memory-extraction] batch write error: {e}");
        }
        _ => {}
    }

    let duration_ms = start.elapsed().as_millis() as u64;
    eprintln!(
        "  [memory-extraction] turn: extracted {} memories ({}) in {:.1}s{}",
        quality_filtered.len(),
        categories.join(", "),
        duration_ms as f64 / 1000.0,
        if prefix_reused { " [cache-shared]" } else { "" },
    );

    ExtractionOutcome::Extracted {
        count: quality_filtered.len(),
        categories,
        duration_ms,
        prefix_reused,
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        s.chars().take(max_chars).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_extraction_response ──

    #[test]
    fn parse_valid_json_array() {
        let resp = r#"[{"type":"feedback","content":"prefers compact JSON"},{"type":"user","content":"senior Rust engineer"}]"#;
        let result = parse_extraction_response(resp);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].category, MemoryCategory::Feedback);
        assert_eq!(result[0].content, "prefers compact JSON");
        assert_eq!(result[1].category, MemoryCategory::User);
    }

    #[test]
    fn parse_markdown_wrapped() {
        let resp = "```json\n[{\"type\":\"project\",\"content\":\"merge freeze May 8\"}]\n```";
        let result = parse_extraction_response(resp);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].category, MemoryCategory::Project);
    }

    #[test]
    fn parse_empty_array() {
        assert!(parse_extraction_response("[]").is_empty());
    }

    #[test]
    fn parse_garbage_returns_empty() {
        assert!(parse_extraction_response("nothing to extract").is_empty());
    }

    #[test]
    fn parse_unknown_type_skipped() {
        let resp =
            r#"[{"type":"unknown","content":"something"},{"type":"user","content":"valid"}]"#;
        let result = parse_extraction_response(resp);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].category, MemoryCategory::User);
    }

    #[test]
    fn parse_empty_content_skipped() {
        let resp = r#"[{"type":"user","content":""},{"type":"user","content":"valid"}]"#;
        let result = parse_extraction_response(resp);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn parse_reference_alias() {
        let resp = r#"[{"type":"ref","content":"Linear INGEST"},{"type":"reference","content":"Grafana board"}]"#;
        let result = parse_extraction_response(resp);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].category, MemoryCategory::Reference);
        assert_eq!(result[1].category, MemoryCategory::Reference);
    }

    // ── main_model_wrote_memory ──

    #[test]
    fn detects_memory_store_in_tools() {
        assert!(main_model_wrote_memory(&[
            "bash".into(),
            "memory_store".into()
        ]));
    }

    #[test]
    fn no_memory_store() {
        assert!(!main_model_wrote_memory(&[
            "bash".into(),
            "read_file".into()
        ]));
    }

    #[test]
    fn empty_tools() {
        assert!(!main_model_wrote_memory(&[]));
    }

    // ── build_extraction_query ──

    #[test]
    fn query_includes_both_messages() {
        let q = build_extraction_query("fix the bug", "I fixed it", "");
        assert!(q.contains("fix the bug"));
        assert!(q.contains("I fixed it"));
    }

    #[test]
    fn query_includes_manifest_when_present() {
        let q = build_extraction_query("hi", "hello", "- prefers Rust");
        assert!(q.contains("prefers Rust"));
        assert!(q.contains("Existing memories"));
    }

    #[test]
    fn query_omits_manifest_section_when_empty() {
        let q = build_extraction_query("hi", "hello", "");
        assert!(!q.contains("Existing memories"));
    }

    #[test]
    fn query_truncates_long_inputs() {
        let long = "x".repeat(2000);
        let q = build_extraction_query(&long, &long, &long);
        assert!(q.len() < 2500);
    }

    // ── MemoryExtractor ──

    #[test]
    fn extractor_starts_enabled() {
        let ext = MemoryExtractor::new();
        assert!(ext.enabled);
        assert!(!ext.is_busy());
    }

    fn ctx<'a>(
        turn: u32,
        params: Option<&'a LlmConnParams>,
        tools: &'a [String],
    ) -> ExtractionContext<'a> {
        ExtractionContext {
            turn,
            selector_params: params,
            user_message: "msg",
            assistant_response: "resp",
            tools_used: tools,
            session_id: None,
            existing_manifest: "",
            fork_prefix: None,
        }
    }

    #[test]
    fn extractor_skips_when_main_wrote() {
        let mut ext = MemoryExtractor::new();
        let params = LlmConnParams {
            base_url: "http://x".into(),
            api_key: "k".into(),
            model_name: "m".into(),
            provider: "openai".into(),
        };
        let tools = vec!["memory_store".into()];
        let outcome = ext.maybe_extract(ctx(1, Some(&params), &tools));
        assert_eq!(outcome.tag(), "skipped_main_wrote");
    }

    #[test]
    fn extractor_skips_when_no_selector() {
        let mut ext = MemoryExtractor::new();
        let outcome = ext.maybe_extract(ctx(1, None, &[]));
        assert_eq!(outcome.tag(), "skipped_no_selector");
    }

    #[test]
    fn extractor_skips_duplicate_turn() {
        let mut ext = MemoryExtractor::new();
        ext.last_processed_turn = 5;
        let params = LlmConnParams {
            base_url: "http://x".into(),
            api_key: "k".into(),
            model_name: "m".into(),
            provider: "openai".into(),
        };
        let outcome = ext.maybe_extract(ctx(5, Some(&params), &[]));
        assert_eq!(outcome.tag(), "skipped_disabled");
    }

    #[test]
    fn extractor_advances_cursor_on_skip() {
        let mut ext = MemoryExtractor::new();
        let _ = ext.maybe_extract(ctx(3, None, &[]));
        assert_eq!(ext.last_processed_turn, 3);
    }

    // ── ExtractionOutcome ──

    #[test]
    fn outcome_tags_are_correct() {
        assert_eq!(ExtractionOutcome::Started.tag(), "started");
        assert_eq!(
            ExtractionOutcome::Extracted {
                count: 1,
                categories: vec![],
                duration_ms: 0,
                prefix_reused: false,
            }
            .tag(),
            "extracted"
        );
        assert_eq!(
            ExtractionOutcome::SkippedMainWrote.tag(),
            "skipped_main_wrote"
        );
        assert_eq!(
            ExtractionOutcome::SkippedNoSelector.tag(),
            "skipped_no_selector"
        );
        assert_eq!(ExtractionOutcome::SkippedDisabled.tag(), "skipped_disabled");
        assert_eq!(ExtractionOutcome::Error("x".into()).tag(), "error");
    }

    // ── EXTRACTION_SYSTEM_PROMPT contract ──

    #[test]
    fn prompt_covers_all_user_facing_types() {
        assert!(EXTRACTION_SYSTEM_PROMPT.contains("user:"));
        assert!(EXTRACTION_SYSTEM_PROMPT.contains("feedback:"));
        assert!(EXTRACTION_SYSTEM_PROMPT.contains("project:"));
        assert!(EXTRACTION_SYSTEM_PROMPT.contains("ref:"));
    }

    #[test]
    fn prompt_has_do_not_extract_section() {
        assert!(EXTRACTION_SYSTEM_PROMPT.contains("Do NOT extract"));
    }

    #[test]
    fn prompt_encourages_selectivity() {
        assert!(EXTRACTION_SYSTEM_PROMPT.contains("false negatives are better than noise"));
    }

    // ── drain ──

    #[tokio::test]
    async fn drain_when_no_inflight_returns_none() {
        let mut ext = MemoryExtractor::new();
        let result = ext.drain(std::time::Duration::from_millis(100)).await;
        assert!(result.is_none());
    }

    // ── Unhappy paths: parse_extraction_response ──

    #[test]
    fn parse_missing_type_field_skipped() {
        let resp = r#"[{"content":"valid text"},{"type":"user","content":"kept"}]"#;
        let result = parse_extraction_response(resp);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].category, MemoryCategory::User);
    }

    #[test]
    fn parse_missing_content_field_skipped() {
        let resp = r#"[{"type":"user"},{"type":"feedback","content":"kept"}]"#;
        let result = parse_extraction_response(resp);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].category, MemoryCategory::Feedback);
    }

    #[test]
    fn parse_non_string_content_skipped() {
        let resp = r#"[{"type":"user","content":123},{"type":"user","content":"valid"}]"#;
        let result = parse_extraction_response(resp);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "valid");
    }

    #[test]
    fn parse_whitespace_content_skipped() {
        let resp = r#"[{"type":"user","content":"   "},{"type":"user","content":"valid"}]"#;
        let result = parse_extraction_response(resp);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn parse_truncated_json_returns_empty() {
        let resp = r#"[{"type":"user","content":"text"},{"type":"brok"#;
        assert!(parse_extraction_response(resp).is_empty());
    }

    // ── Source metadata ──

    #[test]
    fn batch_payload_includes_source_metadata() {
        let src = include_str!("memory_extraction.rs");
        assert!(
            src.contains(r#""source": {"agent": "extraction"}"#),
            "batch write must include extraction source metadata"
        );
    }

    // ── Journal event ──

    #[test]
    fn journal_event_type_exists() {
        let src = include_str!("../../../services/src/session_journal.rs");
        assert!(
            src.contains("MemoryExtraction"),
            "JournalEventType must include MemoryExtraction variant"
        );
    }

    #[test]
    fn journal_event_factory_exists() {
        let src = include_str!("../../../services/src/session_journal.rs");
        assert!(
            src.contains("pub fn memory_extraction("),
            "JournalEvent must have memory_extraction factory method"
        );
    }

    // ── Mock server integration tests ────────────────────────────────────

    use std::sync::{Arc, Mutex};

    async fn spawn_extraction_mock(
        captured: Arc<Mutex<Option<serde_json::Value>>>,
        response_content: &'static str,
    ) -> String {
        use axum::{Router, routing::post};

        let handler = move |axum::Json(body): axum::Json<serde_json::Value>| {
            let captured = captured.clone();
            async move {
                *captured.lock().unwrap() = Some(body);
                axum::Json(serde_json::json!({
                    "choices": [{"message": {"content": response_content}}]
                }))
            }
        };
        let app = Router::new().route("/chat/completions", post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn extraction_native_thinker_sends_suppression() {
        let captured = Arc::new(Mutex::new(None));
        let base = spawn_extraction_mock(captured.clone(), "[]").await;
        let params = LlmConnParams {
            base_url: base,
            api_key: "k".into(),
            model_name: "qwen3.5-flash".into(),
            provider: "dashscope".into(),
        };
        let mut ext = MemoryExtractor::new();
        let outcome = ext.maybe_extract(ExtractionContext {
            turn: 1,
            selector_params: Some(&params),
            user_message: "hello",
            assistant_response: "world",
            tools_used: &[],
            session_id: None,
            existing_manifest: "",
            fork_prefix: None,
        });
        assert_eq!(outcome.tag(), "started");
        let _ = ext.drain(std::time::Duration::from_secs(3)).await;

        let body = captured.lock().unwrap().take().expect("request captured");
        assert_eq!(
            body["enable_thinking"], false,
            "native thinker should send enable_thinking: false"
        );
    }

    #[tokio::test]
    async fn extraction_strips_think_tags_before_parse() {
        let captured = Arc::new(Mutex::new(None));
        let base = spawn_extraction_mock(
            captured.clone(),
            r#"<think>reasoning</think>[{"type":"user","content":"prefers Rust"}]"#,
        )
        .await;
        let params = LlmConnParams {
            base_url: base,
            api_key: "k".into(),
            model_name: "m".into(),
            provider: "openai".into(),
        };
        let mut ext = MemoryExtractor::new();
        let _ = ext.maybe_extract(ExtractionContext {
            turn: 1,
            selector_params: Some(&params),
            user_message: "I prefer Rust",
            assistant_response: "Noted",
            tools_used: &[],
            session_id: None,
            existing_manifest: "",
            fork_prefix: None,
        });
        let result = ext.drain(std::time::Duration::from_secs(3)).await;
        match result {
            Some(ExtractionOutcome::Extracted { count, .. }) => {
                assert!(count > 0, "should extract at least one memory");
            }
            other => panic!("expected Extracted, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn extraction_think_wrapping_json_degrades_gracefully() {
        let captured = Arc::new(Mutex::new(None));
        // JSON is wrapped entirely inside think tags — strip empties it,
        // fallback to original which has <think> prefix → JSON parse fails.
        // This is graceful degradation: count=0, no panic.
        let base = spawn_extraction_mock(
            captured.clone(),
            r#"<think>[{"type":"user","content":"prefers Rust"}]</think>"#,
        )
        .await;
        let params = LlmConnParams {
            base_url: base,
            api_key: "k".into(),
            model_name: "m".into(),
            provider: "openai".into(),
        };
        let mut ext = MemoryExtractor::new();
        let _ = ext.maybe_extract(ExtractionContext {
            turn: 1,
            selector_params: Some(&params),
            user_message: "I prefer Rust",
            assistant_response: "Noted",
            tools_used: &[],
            session_id: None,
            existing_manifest: "",
            fork_prefix: None,
        });
        let result = ext.drain(std::time::Duration::from_secs(3)).await;
        match result {
            Some(ExtractionOutcome::Extracted { count, .. }) => {
                assert_eq!(count, 0, "think-wrapped JSON should degrade to count=0");
            }
            other => panic!("expected Extracted(0), got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn extraction_normal_json_response() {
        let captured = Arc::new(Mutex::new(None));
        let base = spawn_extraction_mock(
            captured.clone(),
            r#"[{"type":"feedback","content":"prefers concise code"}]"#,
        )
        .await;
        let params = LlmConnParams {
            base_url: base,
            api_key: "k".into(),
            model_name: "m".into(),
            provider: "openai".into(),
        };
        let mut ext = MemoryExtractor::new();
        let _ = ext.maybe_extract(ExtractionContext {
            turn: 1,
            selector_params: Some(&params),
            user_message: "keep it concise",
            assistant_response: "ok",
            tools_used: &[],
            session_id: None,
            existing_manifest: "",
            fork_prefix: None,
        });
        let result = ext.drain(std::time::Duration::from_secs(3)).await;
        match result {
            Some(ExtractionOutcome::Extracted { count, .. }) => {
                assert_eq!(count, 1);
            }
            other => panic!("expected Extracted(1), got: {other:?}"),
        }
    }

    // ── Fork prefix wiring tests ──────────────────────────────────────

    fn make_test_prefix(model_id: &str, provider: &str) -> Arc<ForkPrefix> {
        use astra_turn_core::fork_prefix::{
            CacheMode, ProviderKind, SystemBlock, hash_tool_schema,
        };

        let schema = serde_json::json!({"function": {"name": "bash"}});
        let (bytes, hash) = hash_tool_schema(&schema);
        // Build canonical prefix bytes as a JSON messages array (the
        // format the reconstructor expects).
        let prefix_messages = serde_json::json!([
            {"role": "system", "content": "you are a helpful assistant"},
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "hi there"}
        ]);
        let canonical_bytes = serde_json::to_vec(&prefix_messages).unwrap();

        Arc::new(ForkPrefix::build(
            "pfx-test",
            "run-parent",
            1,
            1_700_000_000,
            ProviderKind::from_provider_hint(provider),
            model_id,
            None,
            vec![SystemBlock {
                bytes: b"sys".to_vec(),
                has_cache_control: true,
            }],
            vec![astra_turn_core::fork_prefix::ToolSchemaEntry {
                name: "bash".into(),
                canonical_bytes: bytes,
                hash,
            }],
            vec![],
            canonical_bytes,
            CacheMode::SkipWrite,
        ))
    }

    #[test]
    fn resolve_prefix_returns_none_when_feature_disabled() {
        let _lock = astra_turn_core::fork_capture::FORK_FLAG_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = astra_turn_core::fork_capture::set_fork_flag_for_tests(false);

        let prefix = make_test_prefix("m", "openai");
        let params = LlmConnParams {
            base_url: "http://x".into(),
            api_key: "k".into(),
            model_name: "m".into(),
            provider: "openai".into(),
        };
        let result = resolve_prefix_for_extraction(Some(&prefix), &params);
        assert!(result.is_none(), "feature disabled must return None");

        astra_turn_core::fork_capture::restore_fork_flag_raw_for_tests(prev);
    }

    #[test]
    fn resolve_prefix_returns_none_when_no_prefix() {
        let _lock = astra_turn_core::fork_capture::FORK_FLAG_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = astra_turn_core::fork_capture::set_fork_flag_for_tests(true);

        let params = LlmConnParams {
            base_url: "http://x".into(),
            api_key: "k".into(),
            model_name: "m".into(),
            provider: "openai".into(),
        };
        let result = resolve_prefix_for_extraction(None, &params);
        assert!(result.is_none(), "missing prefix must return None");

        astra_turn_core::fork_capture::restore_fork_flag_raw_for_tests(prev);
    }

    #[test]
    fn resolve_prefix_returns_none_on_provider_mismatch() {
        let _lock = astra_turn_core::fork_capture::FORK_FLAG_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = astra_turn_core::fork_capture::set_fork_flag_for_tests(true);

        // Prefix captured on anthropic, selector is openai.
        let prefix = make_test_prefix("m", "anthropic");
        let params = LlmConnParams {
            base_url: "http://x".into(),
            api_key: "k".into(),
            model_name: "m".into(),
            provider: "openai".into(),
        };
        let result = resolve_prefix_for_extraction(Some(&prefix), &params);
        assert!(result.is_none(), "provider mismatch must return None");

        astra_turn_core::fork_capture::restore_fork_flag_raw_for_tests(prev);
    }

    #[test]
    fn resolve_prefix_returns_none_on_model_mismatch() {
        let _lock = astra_turn_core::fork_capture::FORK_FLAG_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = astra_turn_core::fork_capture::set_fork_flag_for_tests(true);

        // Same provider, different model.
        let prefix = make_test_prefix("gpt-4o", "openai");
        let params = LlmConnParams {
            base_url: "http://x".into(),
            api_key: "k".into(),
            model_name: "gpt-4o-mini".into(),
            provider: "openai".into(),
        };
        let result = resolve_prefix_for_extraction(Some(&prefix), &params);
        assert!(result.is_none(), "model mismatch must return None");

        astra_turn_core::fork_capture::restore_fork_flag_raw_for_tests(prev);
    }

    #[test]
    fn resolve_prefix_returns_some_when_model_and_provider_match() {
        let _lock = astra_turn_core::fork_capture::FORK_FLAG_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = astra_turn_core::fork_capture::set_fork_flag_for_tests(true);

        let prefix = make_test_prefix("gpt-4o", "openai");
        let params = LlmConnParams {
            base_url: "http://x".into(),
            api_key: "k".into(),
            model_name: "gpt-4o".into(),
            provider: "openai".into(),
        };
        let result = resolve_prefix_for_extraction(Some(&prefix), &params);
        assert!(result.is_some(), "matching model+provider must return Some");

        astra_turn_core::fork_capture::restore_fork_flag_raw_for_tests(prev);
    }

    #[test]
    fn build_prefixed_messages_appends_extraction_query() {
        let prefix = make_test_prefix("m", "openai");
        let query = "Extract memories as JSON array:";
        let result = build_prefixed_messages(&prefix, query);
        assert!(result.is_some());
        let msgs = result.unwrap();
        let arr = msgs.as_array().unwrap();
        // Parent had 3 messages + 1 extraction suffix = 4 total.
        assert_eq!(arr.len(), 4);
        // Last message is the extraction query.
        let last = &arr[3];
        assert_eq!(last["role"], "user");
        let content = last["content"].as_str().unwrap();
        assert!(
            content.contains(EXTRACTION_SYSTEM_PROMPT),
            "must contain extraction system instruction"
        );
        assert!(content.contains(query), "must contain the extraction query");
    }

    #[test]
    fn build_prefixed_messages_returns_none_on_invalid_bytes() {
        use astra_turn_core::fork_prefix::{
            CacheMode, ProviderKind, SystemBlock, hash_tool_schema,
        };

        let schema = serde_json::json!({"function": {"name": "bash"}});
        let (bytes, hash) = hash_tool_schema(&schema);
        // Deliberately non-JSON canonical bytes.
        let prefix = Arc::new(ForkPrefix::build(
            "pfx-bad",
            "run-x",
            1,
            1_700_000_000,
            ProviderKind::OpenAi,
            "m",
            None,
            vec![SystemBlock {
                bytes: b"sys".to_vec(),
                has_cache_control: false,
            }],
            vec![astra_turn_core::fork_prefix::ToolSchemaEntry {
                name: "bash".into(),
                canonical_bytes: bytes,
                hash,
            }],
            vec![],
            b"not valid json".to_vec(),
            CacheMode::SkipWrite,
        ));
        assert!(
            build_prefixed_messages(&prefix, "q").is_none(),
            "invalid JSON bytes must return None"
        );
    }

    #[tokio::test]
    async fn extraction_uses_prefixed_messages_when_prefix_available() {
        let captured = Arc::new(Mutex::new(None));
        let base = spawn_extraction_mock(
            captured.clone(),
            r#"[{"type":"user","content":"prefers dark mode"}]"#,
        )
        .await;
        let params = LlmConnParams {
            base_url: base,
            api_key: "k".into(),
            model_name: "gpt-4o".into(),
            provider: "openai".into(),
        };
        let prefix = make_test_prefix("gpt-4o", "openai");

        // Lock scope: flag set + maybe_extract (synchronous read).
        let (prev, mut ext) = {
            let _lock = astra_turn_core::fork_capture::FORK_FLAG_TEST_MUTEX
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let prev = astra_turn_core::fork_capture::set_fork_flag_for_tests(true);
            let mut ext = MemoryExtractor::new();
            let outcome = ext.maybe_extract(ExtractionContext {
                turn: 1,
                selector_params: Some(&params),
                user_message: "set dark mode",
                assistant_response: "done",
                tools_used: &[],
                session_id: None,
                existing_manifest: "",
                fork_prefix: Some(prefix),
            });
            assert_eq!(outcome.tag(), "started");
            (prev, ext)
        };

        let _ = ext.drain(std::time::Duration::from_secs(3)).await;

        let body = captured.lock().unwrap().take().expect("request captured");
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(
            messages.len(),
            4,
            "should have 3 prefix messages + 1 extraction suffix"
        );
        let last_content = messages[3]["content"].as_str().unwrap();
        assert!(
            last_content.contains("durable memories"),
            "suffix must contain extraction instruction"
        );

        astra_turn_core::fork_capture::restore_fork_flag_raw_for_tests(prev);
    }

    #[tokio::test]
    async fn extraction_falls_back_without_prefix() {
        // When no fork_prefix is provided, extraction uses standalone
        // 2-message format (system + user).
        let captured = Arc::new(Mutex::new(None));
        let base = spawn_extraction_mock(
            captured.clone(),
            r#"[{"type":"feedback","content":"prefers verbose output"}]"#,
        )
        .await;
        let params = LlmConnParams {
            base_url: base,
            api_key: "k".into(),
            model_name: "m".into(),
            provider: "openai".into(),
        };
        let mut ext = MemoryExtractor::new();
        let _ = ext.maybe_extract(ExtractionContext {
            turn: 1,
            selector_params: Some(&params),
            user_message: "be verbose",
            assistant_response: "ok",
            tools_used: &[],
            session_id: None,
            existing_manifest: "",
            fork_prefix: None,
        });
        let _ = ext.drain(std::time::Duration::from_secs(3)).await;

        let body = captured.lock().unwrap().take().expect("request captured");
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(
            messages.len(),
            2,
            "fallback path must use 2 messages (system + user)"
        );
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
    }

    // ── Unhappy path: oversized prefix ─────────────────────────────────

    #[test]
    fn build_prefixed_messages_returns_none_on_oversized_prefix() {
        use astra_turn_core::fork_prefix::{
            CacheMode, ProviderKind, SystemBlock, hash_tool_schema,
        };

        let schema = serde_json::json!({"function": {"name": "bash"}});
        let (bytes, hash) = hash_tool_schema(&schema);
        // Create canonical bytes exceeding EXTRACTION_PREFIX_MAX_BYTES.
        let oversized = vec![b'x'; EXTRACTION_PREFIX_MAX_BYTES + 1];
        let prefix = Arc::new(ForkPrefix::build(
            "pfx-huge",
            "run-x",
            1,
            1_700_000_000,
            ProviderKind::OpenAi,
            "m",
            None,
            vec![SystemBlock {
                bytes: b"sys".to_vec(),
                has_cache_control: false,
            }],
            vec![astra_turn_core::fork_prefix::ToolSchemaEntry {
                name: "bash".into(),
                canonical_bytes: bytes,
                hash,
            }],
            vec![],
            oversized,
            CacheMode::SkipWrite,
        ));
        assert!(
            build_prefixed_messages(&prefix, "query").is_none(),
            "oversized prefix must return None"
        );
    }

    #[tokio::test]
    async fn extraction_falls_back_on_oversized_prefix() {
        use astra_turn_core::fork_prefix::{
            CacheMode, ProviderKind, SystemBlock, hash_tool_schema,
        };

        let captured = Arc::new(Mutex::new(None));
        let base = spawn_extraction_mock(
            captured.clone(),
            r#"[{"type":"user","content":"has preference"}]"#,
        )
        .await;
        let params = LlmConnParams {
            base_url: base,
            api_key: "k".into(),
            model_name: "m".into(),
            provider: "openai".into(),
        };

        let schema = serde_json::json!({"function": {"name": "bash"}});
        let (bytes, hash) = hash_tool_schema(&schema);
        let oversized = vec![b'x'; EXTRACTION_PREFIX_MAX_BYTES + 1];
        let prefix = Arc::new(ForkPrefix::build(
            "pfx-huge",
            "run-x",
            1,
            1_700_000_000,
            ProviderKind::from_provider_hint("openai"),
            "m",
            None,
            vec![SystemBlock {
                bytes: b"sys".to_vec(),
                has_cache_control: false,
            }],
            vec![astra_turn_core::fork_prefix::ToolSchemaEntry {
                name: "bash".into(),
                canonical_bytes: bytes,
                hash,
            }],
            vec![],
            oversized,
            CacheMode::SkipWrite,
        ));

        let (prev, mut ext) = {
            let _lock = astra_turn_core::fork_capture::FORK_FLAG_TEST_MUTEX
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let prev = astra_turn_core::fork_capture::set_fork_flag_for_tests(true);
            let mut ext = MemoryExtractor::new();
            let _ = ext.maybe_extract(ExtractionContext {
                turn: 1,
                selector_params: Some(&params),
                user_message: "test",
                assistant_response: "ok",
                tools_used: &[],
                session_id: None,
                existing_manifest: "",
                fork_prefix: Some(prefix),
            });
            (prev, ext)
        };

        let result = ext.drain(std::time::Duration::from_secs(3)).await;

        let body = captured.lock().unwrap().take().expect("request captured");
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(
            messages.len(),
            2,
            "oversized prefix must fall back to standalone 2-message format"
        );

        match result {
            Some(ExtractionOutcome::Extracted { prefix_reused, .. }) => {
                assert!(
                    !prefix_reused,
                    "oversized prefix must not be marked as reused"
                );
            }
            other => panic!("expected Extracted, got: {other:?}"),
        }

        astra_turn_core::fork_capture::restore_fork_flag_raw_for_tests(prev);
    }

    // ── Unhappy path: prefix with valid JSON but not an array ──────────

    #[test]
    fn build_prefixed_messages_returns_none_on_non_array_json() {
        use astra_turn_core::fork_prefix::{
            CacheMode, ProviderKind, SystemBlock, hash_tool_schema,
        };

        let schema = serde_json::json!({"function": {"name": "bash"}});
        let (bytes, hash) = hash_tool_schema(&schema);
        // Valid JSON but an object, not an array.
        let not_array = serde_json::to_vec(&serde_json::json!({"key": "value"})).unwrap();
        let prefix = Arc::new(ForkPrefix::build(
            "pfx-obj",
            "run-x",
            1,
            1_700_000_000,
            ProviderKind::OpenAi,
            "m",
            None,
            vec![SystemBlock {
                bytes: b"sys".to_vec(),
                has_cache_control: false,
            }],
            vec![astra_turn_core::fork_prefix::ToolSchemaEntry {
                name: "bash".into(),
                canonical_bytes: bytes,
                hash,
            }],
            vec![],
            not_array,
            CacheMode::SkipWrite,
        ));
        assert!(
            build_prefixed_messages(&prefix, "q").is_none(),
            "non-array JSON must return None"
        );
    }

    // ── Unhappy path: prefix with empty messages array ─────────────────

    #[test]
    fn build_prefixed_messages_works_with_empty_array() {
        use astra_turn_core::fork_prefix::{
            CacheMode, ProviderKind, SystemBlock, hash_tool_schema,
        };

        let schema = serde_json::json!({"function": {"name": "bash"}});
        let (bytes, hash) = hash_tool_schema(&schema);
        let empty_array = serde_json::to_vec(&serde_json::json!([])).unwrap();
        let prefix = Arc::new(ForkPrefix::build(
            "pfx-empty",
            "run-x",
            1,
            1_700_000_000,
            ProviderKind::OpenAi,
            "m",
            None,
            vec![SystemBlock {
                bytes: b"sys".to_vec(),
                has_cache_control: false,
            }],
            vec![astra_turn_core::fork_prefix::ToolSchemaEntry {
                name: "bash".into(),
                canonical_bytes: bytes,
                hash,
            }],
            vec![],
            empty_array,
            CacheMode::SkipWrite,
        ));
        // Empty array is technically valid — results in just the extraction suffix.
        let result = build_prefixed_messages(&prefix, "query");
        assert!(result.is_some());
        let arr = result.unwrap();
        assert_eq!(arr.as_array().unwrap().len(), 1, "empty prefix + 1 suffix");
    }

    // ── Verify prefix_reused=true is tracked ───────────────────────────

    #[tokio::test]
    async fn extraction_reports_prefix_reused_true() {
        let captured = Arc::new(Mutex::new(None));
        let base = spawn_extraction_mock(
            captured.clone(),
            r#"[{"type":"feedback","content":"likes concise answers"}]"#,
        )
        .await;
        let params = LlmConnParams {
            base_url: base,
            api_key: "k".into(),
            model_name: "gpt-4o".into(),
            provider: "openai".into(),
        };
        let prefix = make_test_prefix("gpt-4o", "openai");

        let (prev, mut ext) = {
            let _lock = astra_turn_core::fork_capture::FORK_FLAG_TEST_MUTEX
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let prev = astra_turn_core::fork_capture::set_fork_flag_for_tests(true);
            let mut ext = MemoryExtractor::new();
            let _ = ext.maybe_extract(ExtractionContext {
                turn: 1,
                selector_params: Some(&params),
                user_message: "be concise",
                assistant_response: "ok",
                tools_used: &[],
                session_id: None,
                existing_manifest: "",
                fork_prefix: Some(prefix),
            });
            (prev, ext)
        };

        let result = ext.drain(std::time::Duration::from_secs(3)).await;

        match result {
            Some(ExtractionOutcome::Extracted { prefix_reused, .. }) => {
                assert!(
                    prefix_reused,
                    "matching prefix must report prefix_reused=true"
                );
            }
            other => panic!("expected Extracted, got: {other:?}"),
        }

        astra_turn_core::fork_capture::restore_fork_flag_raw_for_tests(prev);
    }

    // ── Unhappy path: extraction HTTP error with prefix still reports correctly ──

    #[tokio::test]
    async fn extraction_http_error_with_prefix_returns_error_outcome() {
        let params = LlmConnParams {
            base_url: "http://127.0.0.1:1".into(),
            api_key: "k".into(),
            model_name: "gpt-4o".into(),
            provider: "openai".into(),
        };
        let prefix = make_test_prefix("gpt-4o", "openai");

        let (prev, mut ext) = {
            let _lock = astra_turn_core::fork_capture::FORK_FLAG_TEST_MUTEX
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let prev = astra_turn_core::fork_capture::set_fork_flag_for_tests(true);
            let mut ext = MemoryExtractor::new();
            let _ = ext.maybe_extract(ExtractionContext {
                turn: 1,
                selector_params: Some(&params),
                user_message: "test",
                assistant_response: "ok",
                tools_used: &[],
                session_id: None,
                existing_manifest: "",
                fork_prefix: Some(prefix),
            });
            (prev, ext)
        };

        let result = ext.drain(std::time::Duration::from_secs(5)).await;

        match result {
            Some(ExtractionOutcome::Error(msg)) => {
                assert!(
                    msg.contains("request:"),
                    "error must come from request phase, got: {msg}"
                );
            }
            other => panic!("expected Error, got: {other:?}"),
        }

        astra_turn_core::fork_capture::restore_fork_flag_raw_for_tests(prev);
    }

    // ── Verify prefix_reused=false in non-prefix path ──────────────────

    #[tokio::test]
    async fn extraction_reports_prefix_reused_false_without_prefix() {
        let captured = Arc::new(Mutex::new(None));
        let base = spawn_extraction_mock(
            captured.clone(),
            r#"[{"type":"project","content":"deadline friday"}]"#,
        )
        .await;
        let params = LlmConnParams {
            base_url: base,
            api_key: "k".into(),
            model_name: "m".into(),
            provider: "openai".into(),
        };
        let mut ext = MemoryExtractor::new();
        let _ = ext.maybe_extract(ExtractionContext {
            turn: 1,
            selector_params: Some(&params),
            user_message: "deadline is friday",
            assistant_response: "noted",
            tools_used: &[],
            session_id: None,
            existing_manifest: "",
            fork_prefix: None,
        });
        let result = ext.drain(std::time::Duration::from_secs(3)).await;

        match result {
            Some(ExtractionOutcome::Extracted { prefix_reused, .. }) => {
                assert!(!prefix_reused, "no prefix must report prefix_reused=false");
            }
            other => panic!("expected Extracted, got: {other:?}"),
        }
    }
}
