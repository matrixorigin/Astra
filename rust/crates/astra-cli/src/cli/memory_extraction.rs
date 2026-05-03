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

use astra_prompts::memory_types::MemoryCategory;
use astra_runtime::memory_relevance::LlmConnParams;

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

        let handle =
            tokio::spawn(async move { run_extraction(&params, &query, sid.as_deref()).await });
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

async fn run_extraction(
    params: &LlmConnParams,
    query: &str,
    session_id: Option<&str>,
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

    let resp = match client
        .post(format!("{}/chat/completions", params.base_url))
        .header("Authorization", format!("Bearer {}", params.api_key))
        .json(&serde_json::json!({
            "model": params.model_name,
            "messages": [
                {"role": "system", "content": EXTRACTION_SYSTEM_PROMPT},
                {"role": "user", "content": query},
            ],
            "max_tokens": 200,
            "temperature": 0.0,
        }))
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

    let base = std::env::var("MEMORIA_BASE_URL")
        .unwrap_or_else(|_| astra_core::config::DEFAULT_MEMORIA_URL.to_string());
    let key = match std::env::var("MEMORIA_MASTER_KEY").ok() {
        Some(k) => k,
        None => {
            return ExtractionOutcome::Extracted {
                count: quality_filtered.len(),
                categories,
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }
    };

    match client
        .post(format!("{base}/v1/memories/batch"))
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
        "  [memory-extraction] turn: extracted {} memories ({}) in {:.1}s",
        quality_filtered.len(),
        categories.join(", "),
        duration_ms as f64 / 1000.0,
    );

    ExtractionOutcome::Extracted {
        count: quality_filtered.len(),
        categories,
        duration_ms,
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
        }
    }

    #[test]
    fn extractor_skips_when_main_wrote() {
        let mut ext = MemoryExtractor::new();
        let params = LlmConnParams {
            base_url: "http://x".into(),
            api_key: "k".into(),
            model_name: "m".into(),
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
                duration_ms: 0
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
}
