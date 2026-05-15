//! Background memory extraction agent.
//!
//! After each successful turn, analyzes the conversation and automatically
//! stores durable memories (user preferences, feedback, project context,
//! references) via Memoria. Inspired by Claude Code's `extractMemories`.
//!
//! Key invariants:
//! - **Mutual exclusion**: if the main model already invoked the `memory`
//!   tool this turn, extraction is skipped (no double-writes).
//! - **Fire-and-forget**: never blocks the next user input.
//! - **Single in-flight**: at most one extraction runs at a time.
//! - **Quality gate**: extracted content must pass `is_high_quality_lesson`.
//! - **Auditable**: every run emits a `[memory-extraction]` stderr line
//!   and a `SessionMemoryExtraction` journal event.

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

Body structure for feedback and project types (REQUIRED — without these a future reader
can't judge edge cases and the memory degrades into a slogan):
  Lead with the rule or fact, then two lines:
    **Why:** the reason the user gave — often a past incident or strong preference
    **How to apply:** when/where the guidance should kick in

  Good: \"Integration tests must hit a real database, not mocks.\\n**Why:** prior incident \
where mock/prod divergence masked a broken migration.\\n**How to apply:** when touching \
tests in services/**/tests/.\"
  Bad: \"Use real DBs in tests.\"  (missing the why; future-you can't judge exceptions.)

Do NOT extract:
- Code patterns or architecture (derivable from the codebase)
- Git history or file paths
- Ephemeral debug context or temporary state
- Things the user did NOT say (don't infer preferences from tool usage)

These exclusions apply EVEN when the user explicitly asks you to save. If the user asks
you to \"save this week's PR list\" or \"remember what I did today\", do NOT store the raw
list — store only the one surprising / non-obvious part (e.g. \"user is reviewing PRs on \
weekends this month — flag as pattern, not policy\"). Activity logs are noise.

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
            if astra_tools::memoria::contains_sensitive_memory_content(&content) {
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

fn write_decision_for_failed_conflict_probe() -> astra_tools::memoria::WriteDecision {
    astra_tools::memoria::WriteDecision::Skip {
        reason: "conflict pre-check failed; skipping extraction write to avoid duplicates".into(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExtractionWriteAccounting {
    stored_or_updated: usize,
    candidates: usize,
    writes: usize,
    updates: usize,
    skipped: usize,
    failures: usize,
}

fn extraction_write_accounting(
    candidates: usize,
    writes: usize,
    updates: usize,
    skipped: usize,
    failures: usize,
) -> ExtractionWriteAccounting {
    ExtractionWriteAccounting {
        stored_or_updated: writes + updates,
        candidates,
        writes,
        updates,
        skipped,
        failures,
    }
}

/// Check if the main model already **wrote** to memory this turn.
///
/// Write actions (`remember`, `forget`, `update`, `focus`, `reflect`,
/// `feedback`) mean the agent is actively shaping durable state — the
/// background extractor should stand down to avoid racing or
/// double-writing. Read actions (`recall`, `expand`, `profile`) do not
/// block extraction.
///
/// `actions` should list every `memory(action=…)` call made this turn,
/// populated at the call site that has access to tool-call args.
/// When the slice is empty the function treats the turn as
/// "wrote nothing" — the conservative legacy name-only skip is
/// handled by the caller via the secondary `tools_used` check
/// (see [`MemoryExtractor::maybe_extract`]).
pub fn main_model_wrote_memory_actions(actions: &[String]) -> bool {
    actions.iter().any(|a| {
        matches!(
            a.as_str(),
            "remember" | "forget" | "update" | "focus" | "reflect" | "feedback"
        )
    })
}

/// Legacy name-only check. Kept for callers that don't yet have
/// per-action visibility (e.g. restored sessions where we only
/// rehydrate tool names). Treats any `memory` call as a write —
/// conservative but correctness-safe.
pub fn main_model_wrote_memory(tools_used: &[String]) -> bool {
    tools_used.iter().any(|t| t == "memory")
}

/// Extract the `action` field from every `memory(action=…)` tool call
/// in a list of journal records. Returns actions in call order.
///
/// We scan `args_preview` (the already-truncated JSON-ish blob the
/// journal stores) with a small, forgiving pattern: the preview is
/// capped at ~80 chars but the `"action":"…"` entry sits at the front
/// of the object in all dispatch paths, so it survives truncation.
pub fn extract_memory_actions(
    records: &[astra_services::session_journal::ToolCallRecord],
) -> Vec<String> {
    records
        .iter()
        .filter(|r| r.name == "memory")
        .filter_map(|r| {
            r.args_preview
                .as_deref()
                .and_then(parse_action_from_preview)
        })
        .collect()
}

/// Best-effort extraction of the `"action"` value from a truncated
/// JSON-ish preview string. Accepts both double-quoted and single-
/// quoted variants since different dispatch paths use slightly
/// different preview formatters. Returns `None` when the field is
/// absent or malformed.
fn parse_action_from_preview(preview: &str) -> Option<String> {
    for marker in [
        "\"action\":\"",
        "\"action\": \"",
        "'action':'",
        "'action': '",
    ] {
        if let Some(idx) = preview.find(marker) {
            let rest = &preview[idx + marker.len()..];
            let end = rest.find(['"', '\''])?;
            let action = &rest[..end];
            if !action.is_empty() {
                return Some(action.to_string());
            }
        }
    }
    // Key=value fallback used by some test fixtures.
    if let Some(idx) = preview.find("action=") {
        let rest = &preview[idx + "action=".len()..];
        let action: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        if !action.is_empty() {
            return Some(action);
        }
    }
    None
}

/// Actual cache usage reported by the provider, when available.
/// `read` is how many input tokens came from cache (hit); `creation` is
/// how many were newly cached (miss or first-write). Both None means the
/// provider didn't report usage or we failed to parse it.
///
/// Marked `#[non_exhaustive]` because we may add more provider-reported
/// fields (total_tokens, completion_tokens, etc.) without a semver bump.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CacheUsage {
    pub cache_read_tokens: Option<u64>,
    pub cache_creation_tokens: Option<u64>,
    pub prompt_tokens: Option<u64>,
}

/// Cache hit ratio, typed by which formula produced it. Making the
/// formula visible at the type level prevents dashboards from silently
/// comparing apples-to-oranges when a provider stops reporting
/// `cache_creation_input_tokens` and we fall back to `read / prompt`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum CacheHitRatio {
    /// Formula: read / (read + created). "What share of cache operations
    /// hit." Most accurate when both fields are reported by the provider.
    CacheVsCreated { ratio: f32, read: u64, created: u64 },
    /// Formula: read / prompt_tokens. "What share of input tokens came
    /// from cache." Used as a fallback when `cache_creation` is missing;
    /// semantically different from CacheVsCreated — don't average them.
    CacheVsPrompt { ratio: f32, read: u64, prompt: u64 },
    /// Usage fields missing or all-zero. No meaningful ratio available.
    Unknown,
}

impl CacheHitRatio {
    /// Extract the raw f32 ratio (for tracing / metrics fields). Returns
    /// None for Unknown. Use the enum variant when you need to know
    /// WHICH formula produced the number.
    pub fn as_f32(&self) -> Option<f32> {
        match self {
            CacheHitRatio::CacheVsCreated { ratio, .. }
            | CacheHitRatio::CacheVsPrompt { ratio, .. } => Some(*ratio),
            CacheHitRatio::Unknown => None,
        }
    }
}

impl CacheUsage {
    /// Compute the cache hit ratio. The variant names tell callers (and
    /// dashboards) which formula was used — previously a silent fallback
    /// could move a metric between two semantically different scales.
    pub fn hit_ratio(&self) -> CacheHitRatio {
        match (self.cache_read_tokens, self.cache_creation_tokens) {
            (Some(read), Some(created)) if read + created > 0 => CacheHitRatio::CacheVsCreated {
                ratio: read as f32 / (read + created) as f32,
                read,
                created,
            },
            _ => match (self.cache_read_tokens, self.prompt_tokens) {
                (Some(read), Some(prompt)) if prompt > 0 => CacheHitRatio::CacheVsPrompt {
                    ratio: read as f32 / prompt as f32,
                    read,
                    prompt,
                },
                _ => CacheHitRatio::Unknown,
            },
        }
    }

    pub fn is_empty(&self) -> bool {
        self.cache_read_tokens.is_none()
            && self.cache_creation_tokens.is_none()
            && self.prompt_tokens.is_none()
    }
}

/// Outcome of an extraction attempt, for journal/audit.
///
/// New variants may be added between releases. External callers
/// pattern-matching on this enum MUST include a `_ =>` catch-all.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ExtractionOutcome {
    /// Background task fired — actual results available via `drain()`.
    Started,
    /// Background task completed with results.
    Extracted {
        count: usize,
        categories: Vec<String>,
        duration_ms: u64,
        /// Whether the extraction attempted to reuse a fork prefix (claim).
        prefix_reused: bool,
        /// Actual cache utilization from the provider (verification).
        /// If prefix_reused=true but usage.hit_ratio() is low/None, the
        /// provider did NOT honor the cache — ops should see this in
        /// journals & tracing to debug the 70-95% savings claim.
        usage: CacheUsage,
    },
    SkippedMainWrote,
    SkippedNoModel,
    SkippedDisabled,
    /// A prior turn's extraction is still in flight. The current turn's
    /// extraction was NOT run. `prior_turn` identifies what is blocking us
    /// so the journal can correlate. last_processed_turn is intentionally
    /// NOT advanced — the caller may choose to retry later.
    SkippedBusy {
        prior_turn: u32,
    },
    Error(String),
}

impl ExtractionOutcome {
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Extracted { .. } => "extracted",
            Self::SkippedMainWrote => "skipped_main_wrote",
            Self::SkippedNoModel => "skipped_no_model",
            Self::SkippedDisabled => "skipped_disabled",
            Self::SkippedBusy { .. } => "skipped_busy",
            Self::Error(_) => "error",
        }
    }
}

/// Input context for a single extraction attempt.
pub struct ExtractionContext<'a> {
    pub turn: u32,
    pub memory_model_params: Option<&'a LlmConnParams>,
    pub user_message: &'a str,
    pub assistant_response: &'a str,
    /// Tool *names* the main model invoked this turn (names only; no args).
    /// Used for the legacy name-only skip heuristic when
    /// `recent_memory_actions` is empty.
    pub tools_used: &'a [String],
    /// `memory(action=…)` action strings observed on this turn, in
    /// call order. When non-empty, the orchestrator uses these to
    /// decide skip-on-write precisely (only actual write actions
    /// block extraction). Empty means "no per-action info available";
    /// the caller falls back to the conservative name-only heuristic.
    pub recent_memory_actions: &'a [String],
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
    /// Construct an extractor that defers to the `enabled` flag passed in at
    /// each `maybe_extract` call (driven by `SessionState::auto_memory_enabled`,
    /// which is synced via `pref_keys::AUTO_MEMORY_ENABLED`).
    pub fn new() -> Self {
        Self {
            last_processed_turn: 0,
            in_flight: None,
            enabled: true,
        }
    }

    /// Update the enabled flag (used after a preference pull / `/config` change).
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
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
            let prior = self.last_processed_turn;
            tracing::warn!(
                target: "astra_cli::memory_extraction",
                current_turn = ctx.turn,
                prior_turn = prior,
                "extraction skipped: prior extraction still in flight. \
                 The single-slot queue means this turn's memory may not be captured. \
                 See plans/tool-unification follow-up for bounded queue."
            );
            return ExtractionOutcome::SkippedBusy { prior_turn: prior };
        }
        // Skip extraction when the main model already *wrote* to memory
        // this turn. Prefer the precise per-action signal when we have
        // it; fall back to the conservative name-only heuristic only
        // when actions weren't threaded through (legacy code paths).
        let skip = if !ctx.recent_memory_actions.is_empty() {
            main_model_wrote_memory_actions(ctx.recent_memory_actions)
        } else {
            main_model_wrote_memory(ctx.tools_used)
        };
        if skip {
            self.last_processed_turn = ctx.turn;
            return ExtractionOutcome::SkippedMainWrote;
        }
        let Some(params) = ctx.memory_model_params.cloned() else {
            self.last_processed_turn = ctx.turn;
            return ExtractionOutcome::SkippedNoModel;
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
    use astra_turn_core::fork_prefix::ProviderKind;

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
    let body_json: serde_json::Value =
        serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    let usage = parse_cache_usage(&body_json);
    if prefix_reused {
        let ratio = usage.hit_ratio();
        // Tag the formula in the log so dashboards don't mix denominators.
        let formula = match ratio {
            CacheHitRatio::CacheVsCreated { .. } => "read/(read+created)",
            CacheHitRatio::CacheVsPrompt { .. } => "read/prompt",
            CacheHitRatio::Unknown => "unknown",
        };
        match ratio.as_f32() {
            Some(r) if r >= 0.5 => {
                tracing::info!(
                    target: "astra_cli::memory_extraction",
                    hit_ratio = r,
                    formula,
                    cache_read = usage.cache_read_tokens,
                    cache_creation = usage.cache_creation_tokens,
                    "fork-prefix cache HIT confirmed by provider usage"
                );
            }
            Some(r) => {
                tracing::warn!(
                    target: "astra_cli::memory_extraction",
                    hit_ratio = r,
                    formula,
                    cache_read = usage.cache_read_tokens,
                    cache_creation = usage.cache_creation_tokens,
                    "prefix_reused=true but provider hit_ratio is LOW — \
                     provider may not be honoring our cached blocks"
                );
            }
            None => {
                tracing::warn!(
                    target: "astra_cli::memory_extraction",
                    "prefix_reused=true but provider did not report cache usage — \
                     cannot verify claimed savings"
                );
            }
        }
    }
    let text = body_json
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(String::from)
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
            usage: usage.clone(),
        };
    }

    let categories: Vec<String> = quality_filtered
        .iter()
        .map(|m| format!("{:?}", m.category).to_lowercase())
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
                usage: usage.clone(),
            };
        }
    };

    // Per-candidate conflict pre-check: route near-duplicates to
    // `/correct` instead of batch-inserting a new row. The prior path
    // POSTed every extracted item to `/v1/memories/batch`, creating
    // duplicates whenever a refinement of an existing memory surfaced.
    // Running the pre-checks in parallel keeps latency roughly equal
    // to a single recall RTT; each candidate then independently POSTs
    // either `/v1/memories` (new) or `/v1/memories/{id}/correct`
    // (refinement).
    let auth = format!("Bearer {key}");
    let base = mem.base_url.clone();
    let mut writes = 0usize;
    let mut updates = 0usize;
    let mut skipped = 0usize;
    let mut skip_reasons: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for m in &quality_filtered {
        let encoded = astra_prompts::memory_types::encode(m.category, &m.content);
        // Step 1: top-3 recall with a 2-second timeout (same budget as
        // the tool-side conflict pre-check).
        let probe = client
            .post(format!("{base}/v1/memories/retrieve"))
            .header("Authorization", &auth)
            .timeout(std::time::Duration::from_secs(2))
            .json(&serde_json::json!({
                "query": encoded,
                "top_k": 3,
                "session_id": session_id,
            }))
            .send()
            .await;
        let decision = match probe {
            Ok(resp) if resp.status().is_success() => match resp.text().await {
                Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                    Ok(parsed) => {
                        let arr = parsed
                            .get("memories")
                            .and_then(serde_json::Value::as_array)
                            .cloned()
                            .or_else(|| parsed.as_array().cloned())
                            .unwrap_or_default();
                        astra_tools::memoria::classify_write(&arr)
                    }
                    Err(_) => write_decision_for_failed_conflict_probe(),
                },
                Err(_) => write_decision_for_failed_conflict_probe(),
            },
            // Fail closed on probe/network failure. Writing a fresh row
            // when duplicate detection is unavailable degrades memory
            // quality; skipping one extraction is safer than storing noise.
            _ => write_decision_for_failed_conflict_probe(),
        };

        // Step 2: dispatch.
        let write_result = match &decision {
            astra_tools::memoria::WriteDecision::Update { memory_id, .. } => {
                updates += 1;
                client
                    .put(format!("{base}/v1/memories/{memory_id}/correct"))
                    .header("Authorization", &auth)
                    .json(&serde_json::json!({
                        "new_content": encoded,
                        "reason": "session-end extraction refinement",
                    }))
                    .send()
                    .await
            }
            astra_tools::memoria::WriteDecision::Store => {
                writes += 1;
                client
                    .post(format!("{base}/v1/memories"))
                    .header("Authorization", &auth)
                    .json(&serde_json::json!({
                        "content": encoded,
                        "memory_type": m.category.memoria_type(),
                        "trust_tier": m.category.trust_tier(),
                        "session_id": session_id,
                        "source": {"agent": "extraction"},
                    }))
                    .send()
                    .await
            }
            astra_tools::memoria::WriteDecision::Skip { reason } => {
                skipped += 1;
                skip_reasons.push(reason.clone());
                continue;
            }
        };
        match write_result {
            Ok(resp) if !resp.status().is_success() => {
                errors.push(format!("{:?} HTTP {}", decision, resp.status()));
            }
            Err(e) => errors.push(format!("{decision:?} network: {e}")),
            _ => {}
        }
    }
    if !skip_reasons.is_empty() {
        eprintln!(
            "  [memory-extraction] skipped candidates ({}): {}",
            skipped,
            skip_reasons.join("; ")
        );
    }
    if !errors.is_empty() {
        eprintln!(
            "  [memory-extraction] write failures ({}): {}",
            errors.len(),
            errors.join("; ")
        );
    }

    let duration_ms = start.elapsed().as_millis() as u64;
    let accounting = extraction_write_accounting(
        quality_filtered.len(),
        writes,
        updates,
        skipped,
        errors.len(),
    );
    eprintln!(
        "  [memory-extraction] turn: stored/updated {} memories from {} candidates ({} new, {} updated, {} skipped, {} failures) [{}] in {:.1}s{}",
        accounting.stored_or_updated,
        accounting.candidates,
        accounting.writes,
        accounting.updates,
        accounting.skipped,
        accounting.failures,
        categories.join(", "),
        duration_ms as f64 / 1000.0,
        if prefix_reused { " [cache-shared]" } else { "" },
    );

    ExtractionOutcome::Extracted {
        count: accounting.stored_or_updated,
        categories,
        duration_ms,
        prefix_reused,
        usage,
    }
}

/// Build a JournalEvent capturing the given extraction outcome, including
/// structured metadata specific to the outcome variant (e.g. `prior_turn`
/// for `SkippedBusy`). Centralizing this keeps chat_turn/session_cleanup
/// from dropping fields during manual construction.
pub fn journal_event_for_outcome(
    session_id: Option<&str>,
    turn: u32,
    outcome: &ExtractionOutcome,
) -> astra_services::session_journal::JournalEvent {
    use astra_services::session_journal::{JournalEvent, JournalEventType};
    let (memories_saved, categories, duration_ms, prefix_reused) = match outcome {
        ExtractionOutcome::Extracted {
            count,
            categories,
            duration_ms,
            prefix_reused,
            ..
        } => (*count, categories.clone(), *duration_ms, *prefix_reused),
        _ => (0usize, Vec::<String>::new(), 0u64, false),
    };
    let mut evt = JournalEvent::base_public(JournalEventType::SessionMemoryExtraction, session_id);
    evt.turn = Some(turn);
    evt.duration_ms = Some(duration_ms);
    let mut meta = serde_json::json!({
        "outcome": outcome.tag(),
        "memories_saved": memories_saved,
        "categories": categories,
        "prefix_reused": prefix_reused,
    });
    // Merge outcome-specific structured fields into the metadata JSON.
    if let Some(obj) = meta.as_object_mut() {
        match outcome {
            ExtractionOutcome::SkippedBusy { prior_turn } => {
                obj.insert("prior_turn".into(), serde_json::Value::from(*prior_turn));
            }
            ExtractionOutcome::Error(err) => {
                obj.insert("error".into(), serde_json::Value::from(err.clone()));
            }
            _ => {}
        }
    }
    evt.metadata = Some(meta);
    evt
}

/// Parse cache usage fields from a provider chat-completion response body.
/// Supports both Anthropic (`cache_read_input_tokens` / `cache_creation_input_tokens`)
/// and OpenAI (`prompt_tokens_details.cached_tokens`) shapes.
pub fn parse_cache_usage(body: &serde_json::Value) -> CacheUsage {
    let Some(usage) = body.get("usage").and_then(|u| u.as_object()) else {
        return CacheUsage::default();
    };

    let cache_read_tokens = usage
        .get("cache_read_input_tokens")
        .and_then(|v| v.as_u64())
        .or_else(|| {
            usage
                .get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(|v| v.as_u64())
        });

    let cache_creation_tokens = usage
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_u64());

    let prompt_tokens = usage.get("prompt_tokens").and_then(|v| v.as_u64());

    CacheUsage {
        cache_read_tokens,
        cache_creation_tokens,
        prompt_tokens,
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
    fn parse_sensitive_content_skipped() {
        let resp = r#"[{"type":"project","content":"deploy token: ghp_1234567890abcdef"},{"type":"user","content":"prefers concise answers with examples"}]"#;
        let result = parse_extraction_response(resp);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].category, MemoryCategory::User);
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
    fn detects_memory_use_in_tools() {
        assert!(main_model_wrote_memory(&["bash".into(), "memory".into()]));
    }

    #[test]
    fn no_memory_use() {
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
            memory_model_params: params,
            user_message: "msg",
            assistant_response: "resp",
            tools_used: tools,
            recent_memory_actions: &[],
            session_id: None,
            existing_manifest: "",
            fork_prefix: None,
        }
    }

    /// Variant that also carries `recent_memory_actions` so tests can
    /// verify the action-aware skip path.
    fn ctx_with_actions<'a>(
        turn: u32,
        params: Option<&'a LlmConnParams>,
        tools: &'a [String],
        actions: &'a [String],
    ) -> ExtractionContext<'a> {
        ExtractionContext {
            turn,
            memory_model_params: params,
            user_message: "msg",
            assistant_response: "resp",
            tools_used: tools,
            recent_memory_actions: actions,
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
        let tools = vec!["memory".into()];
        let outcome = ext.maybe_extract(ctx(1, Some(&params), &tools));
        assert_eq!(outcome.tag(), "skipped_main_wrote");
    }

    #[tokio::test]
    async fn extractor_does_not_skip_when_only_memory_reads() {
        // With per-action visibility, read-only `recall` / `expand` /
        // `profile` calls no longer block extraction — only actual
        // writes do.
        let mut ext = MemoryExtractor::new();
        let params = LlmConnParams {
            base_url: "http://x".into(),
            api_key: "k".into(),
            model_name: "m".into(),
            provider: "openai".into(),
        };
        let tools = vec!["memory".into(), "memory".into()];
        let actions = vec!["recall".into(), "expand".into()];
        let outcome = ext.maybe_extract(ctx_with_actions(1, Some(&params), &tools, &actions));
        // The background worker may spawn; we just need to confirm the
        // write-gate didn't pre-empt it.
        assert_ne!(
            outcome.tag(),
            "skipped_main_wrote",
            "read-only memory actions must not block extraction"
        );
    }

    #[test]
    fn extractor_skips_when_action_is_write() {
        let mut ext = MemoryExtractor::new();
        let params = LlmConnParams {
            base_url: "http://x".into(),
            api_key: "k".into(),
            model_name: "m".into(),
            provider: "openai".into(),
        };
        let tools = vec!["memory".into()];
        // A single `remember` should trigger the skip.
        let actions = vec!["remember".into()];
        let outcome = ext.maybe_extract(ctx_with_actions(1, Some(&params), &tools, &actions));
        assert_eq!(outcome.tag(), "skipped_main_wrote");
    }

    #[test]
    fn extract_memory_actions_reads_double_quoted_action() {
        let rec = |name: &str, preview: &str| astra_services::session_journal::ToolCallRecord {
            name: name.into(),
            ok: true,
            args_preview: Some(preview.into()),
            ..Default::default()
        };
        let records = vec![
            rec("memory", r#"{"action":"recall","query":"auth"}"#),
            rec("bash", r#"{"command":"ls"}"#),
            rec("memory", r#"{"action":"remember","content":"prefer …"#),
        ];
        assert_eq!(
            extract_memory_actions(&records),
            vec!["recall".to_string(), "remember".to_string()]
        );
    }

    #[test]
    fn extract_memory_actions_survives_truncation() {
        // Even if the preview is cut mid-content, the `"action":"…"`
        // prefix sits at the front and remains recoverable.
        let rec = astra_services::session_journal::ToolCallRecord {
            name: "memory".into(),
            ok: true,
            args_preview: Some(r#"{"action":"forget","memory_id":"m1…"#.into()),
            ..Default::default()
        };
        assert_eq!(extract_memory_actions(&[rec]), vec!["forget".to_string()]);
    }

    #[test]
    fn extract_memory_actions_handles_key_value_style() {
        let rec = astra_services::session_journal::ToolCallRecord {
            name: "memory".into(),
            ok: true,
            args_preview: Some("action=recall query=auth".into()),
            ..Default::default()
        };
        assert_eq!(extract_memory_actions(&[rec]), vec!["recall".to_string()]);
    }

    #[test]
    fn extract_memory_actions_ignores_other_tools() {
        let records = vec![astra_services::session_journal::ToolCallRecord {
            name: "bash".into(),
            args_preview: Some(r#"{"action":"not_memory"}"#.into()),
            ..Default::default()
        }];
        assert!(extract_memory_actions(&records).is_empty());
    }

    #[test]
    fn extract_memory_actions_returns_empty_when_args_missing() {
        let records = vec![astra_services::session_journal::ToolCallRecord {
            name: "memory".into(),
            args_preview: None,
            ..Default::default()
        }];
        assert!(extract_memory_actions(&records).is_empty());
    }

    #[test]
    fn main_model_wrote_memory_actions_classifies_v2_verbs() {
        // Writes
        for w in [
            "remember", "forget", "update", "focus", "reflect", "feedback",
        ] {
            assert!(
                main_model_wrote_memory_actions(&[w.to_string()]),
                "{w} should count as a write"
            );
        }
        // Reads
        for r in ["recall", "expand", "profile"] {
            assert!(
                !main_model_wrote_memory_actions(&[r.to_string()]),
                "{r} should NOT count as a write"
            );
        }
        // Empty list → not a write
        assert!(!main_model_wrote_memory_actions(&[]));
    }

    #[test]
    fn extractor_skips_when_no_model() {
        let mut ext = MemoryExtractor::new();
        let outcome = ext.maybe_extract(ctx(1, None, &[]));
        assert_eq!(outcome.tag(), "skipped_no_model");
    }

    // ── P1-2: CacheUsage parsing & metrics ────────────────────────────────
    //
    // Previously we reported prefix_reused=true whenever we sent a prefixed
    // request, regardless of whether the provider actually used the cache.
    // Now we parse the usage object and surface real cache_read_tokens.

    #[test]
    fn hit_ratio_from_read_and_creation() {
        let u = CacheUsage {
            cache_read_tokens: Some(900),
            cache_creation_tokens: Some(100),
            prompt_tokens: None,
        };
        match u.hit_ratio() {
            CacheHitRatio::CacheVsCreated {
                ratio,
                read,
                created,
            } => {
                assert!((ratio - 0.9).abs() < 1e-6);
                assert_eq!(read, 900);
                assert_eq!(created, 100);
            }
            other => panic!("expected CacheVsCreated, got {other:?}"),
        }
    }

    #[test]
    fn hit_ratio_falls_back_to_prompt_when_creation_missing() {
        let u = CacheUsage {
            cache_read_tokens: Some(80),
            cache_creation_tokens: None,
            prompt_tokens: Some(100),
        };
        match u.hit_ratio() {
            CacheHitRatio::CacheVsPrompt {
                ratio,
                read,
                prompt,
            } => {
                assert!((ratio - 0.8).abs() < 1e-6);
                assert_eq!(read, 80);
                assert_eq!(prompt, 100);
            }
            other => panic!("expected CacheVsPrompt fallback when creation missing, got {other:?}"),
        }
    }

    #[test]
    fn hit_ratio_unknown_when_nothing_known() {
        assert!(matches!(
            CacheUsage::default().hit_ratio(),
            CacheHitRatio::Unknown
        ));
    }

    #[test]
    fn hit_ratio_handles_zero_denominator() {
        let u = CacheUsage {
            cache_read_tokens: Some(0),
            cache_creation_tokens: Some(0),
            prompt_tokens: Some(0),
        };
        assert!(
            matches!(u.hit_ratio(), CacheHitRatio::Unknown),
            "zero denominators must collapse to Unknown, not a bogus 0.0"
        );
    }

    #[test]
    fn hit_ratio_as_f32_convenience() {
        // Some call sites want a plain f32 for log fields. Provide a
        // convenience accessor that extracts it without forcing callers
        // to match on the enum.
        let u = CacheUsage {
            cache_read_tokens: Some(7),
            cache_creation_tokens: Some(3),
            prompt_tokens: None,
        };
        assert_eq!(u.hit_ratio().as_f32(), Some(0.7));
        assert_eq!(CacheUsage::default().hit_ratio().as_f32(), None);
    }

    #[test]
    fn parse_usage_anthropic_format() {
        // Anthropic-style: usage.cache_read_input_tokens / cache_creation_input_tokens
        let body = serde_json::json!({
            "choices": [{"message": {"content": "{}"}}],
            "usage": {
                "cache_read_input_tokens": 1500,
                "cache_creation_input_tokens": 200,
                "prompt_tokens": 1800
            }
        });
        let usage = parse_cache_usage(&body);
        assert_eq!(usage.cache_read_tokens, Some(1500));
        assert_eq!(usage.cache_creation_tokens, Some(200));
        assert_eq!(usage.prompt_tokens, Some(1800));
    }

    #[test]
    fn parse_usage_openai_format() {
        // OpenAI-style: usage.prompt_tokens_details.cached_tokens
        let body = serde_json::json!({
            "choices": [{"message": {"content": "{}"}}],
            "usage": {
                "prompt_tokens": 1800,
                "prompt_tokens_details": {"cached_tokens": 1500}
            }
        });
        let usage = parse_cache_usage(&body);
        assert_eq!(usage.cache_read_tokens, Some(1500));
        assert_eq!(usage.prompt_tokens, Some(1800));
    }

    #[test]
    fn parse_usage_missing_yields_empty() {
        let body = serde_json::json!({
            "choices": [{"message": {"content": "{}"}}]
        });
        assert!(parse_cache_usage(&body).is_empty());
    }

    #[test]
    fn parse_usage_malformed_yields_empty() {
        let body = serde_json::json!({
            "usage": "not an object"
        });
        assert!(parse_cache_usage(&body).is_empty());
    }

    // ── P0-3: busy-skip observability ─────────────────────────────────────
    //
    // Previously, `is_busy() → SkippedDisabled` — indistinguishable from
    // feature-disabled, no journal record, no metric. In practice a user
    // sending turn N+1 before turn N's extraction completed would lose
    // extraction silently. Production operators can't tell this from
    // "feature was off". Split the outcomes.

    #[tokio::test]
    async fn busy_second_call_returns_distinct_outcome() {
        let mut ext = MemoryExtractor::new();
        ext.last_processed_turn = 0;
        // Park a task so is_busy() returns true.
        let handle = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            ExtractionOutcome::Error("never".into())
        });
        ext.in_flight = Some(handle);

        let params = LlmConnParams {
            base_url: "http://x".into(),
            api_key: "k".into(),
            model_name: "m".into(),
            provider: "openai".into(),
        };
        let outcome = ext.maybe_extract(ctx(1, Some(&params), &[]));
        assert_eq!(
            outcome.tag(),
            "skipped_busy",
            "busy must be its own tag, not conflated with 'skipped_disabled'"
        );
        match outcome {
            ExtractionOutcome::SkippedBusy { prior_turn } => {
                assert_eq!(prior_turn, 0, "prior_turn field must carry context");
            }
            other => panic!("expected SkippedBusy, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn busy_skip_does_not_advance_cursor() {
        // If turn N+1 skipped due to busy, we must NOT record N+1 as
        // processed — the user may want to retry it manually. `last_
        // processed_turn` should only advance on terminal outcomes.
        let mut ext = MemoryExtractor::new();
        ext.last_processed_turn = 4;
        let handle = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            ExtractionOutcome::Error("never".into())
        });
        ext.in_flight = Some(handle);

        let params = LlmConnParams {
            base_url: "http://x".into(),
            api_key: "k".into(),
            model_name: "m".into(),
            provider: "openai".into(),
        };
        let _ = ext.maybe_extract(ctx(5, Some(&params), &[]));
        assert_eq!(
            ext.last_processed_turn, 4,
            "busy-skip must not mark turn as processed; cursor stays at prior value"
        );
    }

    // ── review-critical #2: prior_turn must land in the journal event ─────
    //
    // P0-3's commit message claimed prior_turn was "plumbed into journal
    // for ops correlation" — but the chat_turn path only used
    // the legacy Memoria extraction event, dropping the field. Ops saw
    // `tag="skipped_busy"` with no way to know WHICH turn was blocking.

    #[test]
    fn journal_event_for_skipped_busy_carries_prior_turn() {
        let outcome = ExtractionOutcome::SkippedBusy { prior_turn: 7 };
        let evt = journal_event_for_outcome(None, 8, &outcome);
        let meta = evt
            .metadata
            .as_ref()
            .expect("memory_extraction event must have metadata");
        assert_eq!(
            meta.get("outcome").and_then(|v| v.as_str()),
            Some("skipped_busy")
        );
        assert_eq!(
            meta.get("prior_turn").and_then(|v| v.as_u64()),
            Some(7),
            "SkippedBusy must propagate prior_turn into journal metadata \
             so operators can correlate skips with the blocker turn. meta={meta:?}"
        );
    }

    // R2-#1: end-to-end assertion that the drain path also plumbs
    // outcome-specific metadata. session_cleanup's journal event for
    // SkippedBusy must carry `prior_turn`; for Extracted must carry the
    // usage fields (via prefix_reused at minimum, since usage is not in
    // the journal schema yet).
    #[test]
    fn drain_path_journal_event_carries_prior_turn_for_busy() {
        // Fake the drain() return value: SkippedBusy.
        let outcome = ExtractionOutcome::SkippedBusy { prior_turn: 42 };
        let evt = journal_event_for_outcome(Some("sess-1"), 43, &outcome);
        let meta = evt.metadata.as_ref().unwrap();
        assert_eq!(
            meta.get("prior_turn").and_then(|v| v.as_u64()),
            Some(42),
            "session_cleanup must journal prior_turn on SkippedBusy drain — \
             session-end is when ops are most likely to investigate"
        );
    }

    #[test]
    fn drain_path_journal_event_error_variant_carries_error_field() {
        // R2 test cleanup: positive test for Error variant that was missing
        // from the previous negative-only coverage.
        let outcome = ExtractionOutcome::Error("provider 503".into());
        let evt = journal_event_for_outcome(None, 1, &outcome);
        let meta = evt.metadata.as_ref().unwrap();
        assert_eq!(
            meta.get("error").and_then(|v| v.as_str()),
            Some("provider 503"),
            "Error outcome must serialize the error message into journal metadata"
        );
    }

    #[test]
    fn journal_event_for_non_busy_outcomes_has_no_prior_turn() {
        // prior_turn is only meaningful for SkippedBusy. Other outcomes
        // should not have a stray prior_turn field in metadata.
        for outcome in [
            ExtractionOutcome::SkippedMainWrote,
            ExtractionOutcome::SkippedNoModel,
            ExtractionOutcome::SkippedDisabled,
            ExtractionOutcome::Error("x".into()),
        ] {
            let evt = journal_event_for_outcome(None, 3, &outcome);
            let meta = evt.metadata.as_ref().unwrap();
            assert!(
                meta.get("prior_turn").is_none(),
                "{} outcome should not carry prior_turn: {meta:?}",
                outcome.tag()
            );
        }
    }

    #[test]
    fn skipped_busy_tag_is_distinct() {
        assert_eq!(
            ExtractionOutcome::SkippedBusy { prior_turn: 3 }.tag(),
            "skipped_busy"
        );
        // And it is NOT the same as the disabled tag (no conflation).
        assert_ne!(
            ExtractionOutcome::SkippedBusy { prior_turn: 3 }.tag(),
            ExtractionOutcome::SkippedDisabled.tag()
        );
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
                usage: CacheUsage::default(),
            }
            .tag(),
            "extracted"
        );
        assert_eq!(
            ExtractionOutcome::SkippedMainWrote.tag(),
            "skipped_main_wrote"
        );
        assert_eq!(ExtractionOutcome::SkippedNoModel.tag(), "skipped_no_model");
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

    #[test]
    fn conflict_probe_failure_does_not_store_duplicate() {
        assert!(matches!(
            write_decision_for_failed_conflict_probe(),
            astra_tools::memoria::WriteDecision::Skip { .. }
        ));
    }

    #[test]
    fn skipped_extraction_candidates_do_not_count_as_stored() {
        let accounting = extraction_write_accounting(3, 1, 1, 1, 0);

        assert_eq!(accounting.candidates, 3);
        assert_eq!(accounting.stored_or_updated, 2);
        assert_eq!(accounting.skipped, 1);
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
            src.contains("SessionMemoryExtraction"),
            "JournalEventType must include SessionMemoryExtraction variant"
        );
    }

    #[test]
    fn journal_event_factory_exists() {
        let src = include_str!("../../../services/src/session_journal.rs");
        assert!(
            src.contains("pub fn session_memory_extraction("),
            "JournalEvent must have session_memory_extraction factory method"
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
            memory_model_params: Some(&params),
            user_message: "hello",
            assistant_response: "world",
            tools_used: &[],
            recent_memory_actions: &[],
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
            memory_model_params: Some(&params),
            user_message: "I prefer Rust",
            assistant_response: "Noted",
            tools_used: &[],
            recent_memory_actions: &[],
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
            memory_model_params: Some(&params),
            user_message: "I prefer Rust",
            assistant_response: "Noted",
            tools_used: &[],
            recent_memory_actions: &[],
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
            memory_model_params: Some(&params),
            user_message: "keep it concise",
            assistant_response: "ok",
            tools_used: &[],
            recent_memory_actions: &[],
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
    fn resolve_prefix_returns_none_when_no_prefix() {
        let params = LlmConnParams {
            base_url: "http://x".into(),
            api_key: "k".into(),
            model_name: "m".into(),
            provider: "openai".into(),
        };
        let result = resolve_prefix_for_extraction(None, &params);
        assert!(result.is_none(), "missing prefix must return None");
    }

    #[test]
    fn resolve_prefix_returns_none_on_provider_mismatch() {
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
    }

    #[test]
    fn resolve_prefix_returns_none_on_model_mismatch() {
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
    }

    #[test]
    fn resolve_prefix_returns_some_when_model_and_provider_match() {
        let prefix = make_test_prefix("gpt-4o", "openai");
        let params = LlmConnParams {
            base_url: "http://x".into(),
            api_key: "k".into(),
            model_name: "gpt-4o".into(),
            provider: "openai".into(),
        };
        let result = resolve_prefix_for_extraction(Some(&prefix), &params);
        assert!(result.is_some(), "matching model+provider must return Some");
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

        let mut ext = MemoryExtractor::new();
        let outcome = ext.maybe_extract(ExtractionContext {
            turn: 1,
            memory_model_params: Some(&params),
            user_message: "set dark mode",
            assistant_response: "done",
            tools_used: &[],
            recent_memory_actions: &[],
            session_id: None,
            existing_manifest: "",
            fork_prefix: Some(prefix),
        });
        assert_eq!(outcome.tag(), "started");

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
            memory_model_params: Some(&params),
            user_message: "be verbose",
            assistant_response: "ok",
            tools_used: &[],
            recent_memory_actions: &[],
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

        let mut ext = MemoryExtractor::new();
        let _ = ext.maybe_extract(ExtractionContext {
            turn: 1,
            memory_model_params: Some(&params),
            user_message: "test",
            assistant_response: "ok",
            tools_used: &[],
            recent_memory_actions: &[],
            session_id: None,
            existing_manifest: "",
            fork_prefix: Some(prefix),
        });

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

        let mut ext = MemoryExtractor::new();
        let _ = ext.maybe_extract(ExtractionContext {
            turn: 1,
            memory_model_params: Some(&params),
            user_message: "be concise",
            assistant_response: "ok",
            tools_used: &[],
            recent_memory_actions: &[],
            session_id: None,
            existing_manifest: "",
            fork_prefix: Some(prefix),
        });

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

        let mut ext = MemoryExtractor::new();
        let _ = ext.maybe_extract(ExtractionContext {
            turn: 1,
            memory_model_params: Some(&params),
            user_message: "test",
            assistant_response: "ok",
            tools_used: &[],
            recent_memory_actions: &[],
            session_id: None,
            existing_manifest: "",
            fork_prefix: Some(prefix),
        });

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
            memory_model_params: Some(&params),
            user_message: "deadline is friday",
            assistant_response: "noted",
            tools_used: &[],
            recent_memory_actions: &[],
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
