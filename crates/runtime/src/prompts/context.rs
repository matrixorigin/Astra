/// Unicode-aware token estimation for a string.
///
/// BPE tokenizers (GPT-4, Claude, etc.) encode dense non-ASCII scripts
/// (CJK, kana, Hangul, Cyrillic, Arabic, etc.) at roughly 1.5 tokens per
/// character on average. A single character often splits into 2 BPE tokens,
/// while common bigrams merge back. We use 3/2 integer arithmetic for a
/// conservative estimate without floating point. ASCII text averages ~4 bytes
/// per token.
///
/// JSON content uses ~2 bytes per token due to many single-character tokens
/// (`{`, `}`, `:`, `,`, `"`). Detected by leading `{` or `[`.
///
/// Emoji typically consume 2-3 tokens each, so they use a separate 2.5-token
/// estimate. All non-ASCII text not classified as emoji uses the dense-script
/// estimate; this avoids brittle Unicode block allowlists.
pub fn estimate_str_tokens(s: &str) -> usize {
    let mut dense_unicode_tokens: usize = 0;
    let mut emoji_tokens: usize = 0;
    let mut ascii_bytes: usize = 0;

    for ch in s.chars() {
        if ch.is_ascii() {
            ascii_bytes += ch.len_utf8();
        } else if is_emoji_like(ch) {
            emoji_tokens += 1;
        } else {
            dense_unicode_tokens += 1;
        }
    }

    // Dense Unicode scripts: ~1.5 tokens per char (3*n/2).
    let dense_unicode_total = (dense_unicode_tokens * 3).div_ceil(2);
    // Emoji: ~2.5 tokens each (5*n/2)
    let emoji_total = (emoji_tokens * 5).div_ceil(2);

    // JSON-like content: ~2 bytes/token. Regular text: ~4 bytes/token.
    // Trim leading whitespace before peeking — tool results often have
    // newlines or spaces before the opening brace.
    let first = s
        .as_bytes()
        .iter()
        .find(|b| !b.is_ascii_whitespace())
        .copied()
        .unwrap_or(0);
    let ascii_divisor = if first == b'{' || first == b'[' { 2 } else { 4 };

    dense_unicode_total + emoji_total + ascii_bytes / ascii_divisor
}

fn is_emoji_like(ch: char) -> bool {
    matches!(
        ch as u32,
        0x1F000..=0x1FAFF // pictographs, flags, symbols, supplemental emoji
            | 0x2600..=0x27BF // miscellaneous symbols and dingbats
            | 0xFE00..=0xFE0F // variation selectors used in emoji presentation
    )
}

/// Approximate token count with CJK-aware estimation.
/// Adds overhead per message for role/formatting tokens, plus system prompt
/// and tool schema overhead that the LLM API counts but we don't see in the
/// messages array.
///
/// * `schema_token_total` — sum of measured token costs for all selected tool
///   schemas (from `ToolRegistry::token_cost`). Pass 0 if unavailable.
/// * `system_prompt_tokens` — estimated tokens of the system prompt, or 0 to
///   use the default 14,000 estimate.
///
/// Calibrated default: the full system prompt (~52 KB) is approximately 14,000 tokens.
pub const DEFAULT_SYSTEM_PROMPT_TOKENS: usize = 14_000;
pub(crate) const MODEL_FRAMING_TOKENS: usize = 300;

pub fn estimate_tokens(
    messages: &[serde_json::Value],
    schema_token_total: usize,
    system_prompt_tokens: usize,
) -> usize {
    let sys_tokens = if system_prompt_tokens > 0 {
        system_prompt_tokens
    } else {
        DEFAULT_SYSTEM_PROMPT_TOKENS
    };

    let message_tokens: usize = messages
        .iter()
        .map(|m| estimate_single_message_tokens(m) + PER_MESSAGE_OVERHEAD)
        .sum();
    message_tokens + sys_tokens + schema_token_total + MODEL_FRAMING_TOKENS
}

pub(crate) const PER_MESSAGE_OVERHEAD: usize = 4;

/// Estimate an arbitrary JSON value without assuming a provider-specific
/// message shape. This covers string content, block arrays, multimodal
/// envelopes, tool calls, and future fields with one conservative rule.
///
/// This recursively walks the JSON tree with per-variant rules matched to
/// provider tokenization behavior. The pipeline estimator in
/// astra-turn-core/src/context/pipeline.rs uses a different method
/// (serde_json::to_string → char count → token estimate), which is coarser
/// but available at a lower layer without depending on this crate.
/// Both are approximations; the provider tokenizer is authoritative.
pub fn estimate_json_value_tokens(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) => 1,
        serde_json::Value::Number(number) => estimate_str_tokens(&number.to_string()).max(1),
        serde_json::Value::String(text) => estimate_str_tokens(text).saturating_add(1),
        serde_json::Value::Array(items) => items
            .iter()
            .map(estimate_json_value_tokens)
            .fold(1_usize, usize::saturating_add),
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(key, value)| {
                estimate_str_tokens(key)
                    .saturating_add(1)
                    .saturating_add(estimate_json_value_tokens(value))
            })
            .fold(1_usize, usize::saturating_add),
    }
}

/// Estimate tokens for a single provider message in any JSON wire shape.
pub(crate) fn estimate_single_message_tokens(m: &serde_json::Value) -> usize {
    estimate_json_value_tokens(m)
}

// ---------------------------------------------------------------------------
// Cache-aware token accounting
// ---------------------------------------------------------------------------

/// Breakdown of token usage separating cache-eligible (stable) from volatile
/// (per-turn) tokens. Cache-eligible tokens benefit from provider-side prompt
/// caching (e.g. Anthropic cache, OpenAI automatic caching) and should be
/// considered "cheaper" in cost calculations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheAwareEstimate {
    /// Total estimated tokens across all components.
    pub total_tokens: usize,
    /// System prompt + tool schemas — stable across turns, cache-eligible.
    pub cache_eligible_tokens: usize,
    /// Conversation messages that change each turn.
    pub volatile_tokens: usize,
}

fn estimate_message_batch_tokens(messages: &[serde_json::Value]) -> usize {
    messages
        .iter()
        .map(|m| estimate_single_message_tokens(m) + PER_MESSAGE_OVERHEAD)
        .sum()
}

/// Estimate tokens with cache-awareness when the caller already has separate
/// stable-prefix and volatile-tail slices and wants to avoid cloning them into
/// one temporary vector.
pub fn estimate_tokens_cache_aware_split(
    stable_messages: &[serde_json::Value],
    volatile_messages: &[serde_json::Value],
    tool_schema_tokens: usize,
) -> CacheAwareEstimate {
    let stable_tokens = estimate_message_batch_tokens(stable_messages);
    let volatile_tokens = estimate_message_batch_tokens(volatile_messages);
    let cache_eligible = stable_tokens + tool_schema_tokens;
    let total = cache_eligible + volatile_tokens;

    CacheAwareEstimate {
        total_tokens: total,
        cache_eligible_tokens: cache_eligible,
        volatile_tokens,
    }
}

/// Estimate tokens with cache-awareness, separating stable prefix from
/// volatile conversation tokens.
///
/// * `messages` — the full messages array sent to the LLM.
/// * `tool_schema_tokens` — estimated tokens consumed by tool/function
///   schemas (varies by toolset; pass 0 if unknown).
///
/// The first message is assumed to be the system prompt (cache-eligible).
/// All subsequent messages are volatile.
pub fn estimate_tokens_cache_aware(
    messages: &[serde_json::Value],
    tool_schema_tokens: usize,
) -> CacheAwareEstimate {
    if let Some((system_message, volatile_messages)) = messages.split_first() {
        estimate_tokens_cache_aware_split(
            std::slice::from_ref(system_message),
            volatile_messages,
            tool_schema_tokens,
        )
    } else {
        estimate_tokens_cache_aware_split(&[], &[], tool_schema_tokens)
    }
}

// ---------------------------------------------------------------------------
// Tiered compaction strategy
// ---------------------------------------------------------------------------

/// Compaction urgency tiers — each tier triggers progressively more aggressive
/// context reduction strategies.
pub use astra_turn_core::compaction_types::CompactionTier;

// ---------------------------------------------------------------------------
// Context budget
// ---------------------------------------------------------------------------

/// Configuration for LLM-based compaction summary (Phase 2 feature).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompactConfig {
    /// Enable LLM-generated summary instead of pure truncation.
    /// Defaults to `true`. Set `ASTRA_COMPACT_SUMMARY_ENABLED=false` to disable.
    pub enable_summary: bool,
    /// Maximum tokens to generate for the summary.
    pub summary_token_budget: usize,
    /// Maximum PTL retry attempts before falling back to truncation.
    pub max_ptl_retries: usize,
    /// Minimum compaction tier that triggers LLM summary.
    /// Defaults to CompactHistory (75%+ context usage).
    pub summary_min_tier: CompactionTier,
}

impl Default for CompactConfig {
    fn default() -> Self {
        Self {
            enable_summary: true,
            summary_token_budget: 20_000,
            max_ptl_retries: 3,
            summary_min_tier: CompactionTier::CompactHistory,
        }
    }
}

impl CompactConfig {
    /// Build config using hardcoded defaults (no env overrides).
    pub fn from_env() -> Self {
        Self::default()
    }

    /// Returns true if LLM summary should be attempted for the given tier.
    pub fn should_summarize(&self, tier: CompactionTier) -> bool {
        if !self.enable_summary {
            return false;
        }
        let tier_level = |t: CompactionTier| match t {
            CompactionTier::Normal => 0,
            CompactionTier::TrimSchemas => 1,
            CompactionTier::CompactHistory => 2,
            CompactionTier::AggressivePrune => 3,
        };
        tier_level(tier) >= tier_level(self.summary_min_tier)
    }
}

/// Context budget configuration — model-aware limits.
#[derive(Debug, Clone)]
pub struct ContextBudget {
    /// Maximum context tokens the model supports.
    pub model_limit: usize,
    /// Auto-compact fires when estimated tokens exceed this fraction of
    /// effective_input_limit (corresponds to CompactHistory threshold).
    pub compact_threshold: f64,
    /// Number of recent turns to keep after compaction.
    pub keep_recent_turns: usize,
    /// Max chars for memory retrieval injection.
    pub memory_budget_chars: usize,
    /// Compatibility ratio for direct construction. Production constructors
    /// resolve exact token reserves in [`ContextWindowPolicy`].
    pub output_reserve_ratio: f64,
    /// LLM-based compaction summary configuration.
    pub compact_config: CompactConfig,
    /// Exact resolved policy for production paths. `None` is retained only
    /// for direct construction in compatibility tests and embedded callers.
    pub(crate) resolved_policy: Option<ContextWindowPolicy>,
}

pub const DEFAULT_CONTEXT_WINDOW_TOKENS: usize = 200_000;
pub const DEFAULT_OUTPUT_RESERVE_TOKENS: usize = 16_384;
pub const DEFAULT_PROTOCOL_RESERVE_TOKENS: usize = 300;

/// Provenance for a resolved context-window policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextWindowPolicySource {
    /// Both the raw window and completion limit came from the model catalog.
    ModelCatalog,
    /// The catalog supplied only part of the limit metadata.
    PartialModelCatalog,
    /// No catalog window was available, so the documented generic fallback
    /// was used. No model-name matching is performed.
    GenericFallback,
}

/// One exact token policy shared by assembly, wire preflight, trace, and
/// compaction-effectiveness checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContextWindowPolicy {
    pub raw_context_window_tokens: usize,
    pub usable_input_limit_tokens: usize,
    pub reserved_output_tokens: usize,
    pub reserved_summary_tokens: usize,
    pub reserved_protocol_tokens: usize,
    pub auto_compact_trigger_tokens: usize,
    pub hard_input_limit_tokens: usize,
    pub source: ContextWindowPolicySource,
}

impl ContextWindowPolicy {
    #[must_use]
    pub fn resolve(
        context_window_tokens: Option<u32>,
        max_completion_tokens: Option<u32>,
        summary_reserve_tokens: usize,
        compact_threshold: f64,
    ) -> Self {
        let catalog_context_window = context_window_tokens.filter(|tokens| *tokens > 0);
        let catalog_max_completion = max_completion_tokens.filter(|tokens| *tokens > 0);
        let raw = catalog_context_window
            .map(|tokens| tokens as usize)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS);
        // Catalog metadata is authoritative. The fallback is one fixed
        // documented reserve, clamped only to keep malformed/tiny windows
        // arithmetically valid; it never branches on provider/model text.
        let output = catalog_max_completion
            .map(|tokens| tokens as usize)
            .unwrap_or_else(|| DEFAULT_OUTPUT_RESERVE_TOKENS.min((raw / 4).max(1)))
            .min(raw.saturating_sub(1));
        let protocol = DEFAULT_PROTOCOL_RESERVE_TOKENS.min(raw.saturating_sub(output));
        let hard_input = raw.saturating_sub(output).saturating_sub(protocol);
        let summary = summary_reserve_tokens.min(hard_input / 4);
        let usable_input = hard_input.saturating_sub(summary);
        let threshold = if compact_threshold.is_finite() {
            compact_threshold.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let auto_compact_trigger = (usable_input as f64 * threshold)
            .floor()
            .min(usize::MAX as f64) as usize;
        let source = match (catalog_context_window, catalog_max_completion) {
            (Some(_), Some(_)) => ContextWindowPolicySource::ModelCatalog,
            (Some(_), None) | (None, Some(_)) => ContextWindowPolicySource::PartialModelCatalog,
            (None, None) => ContextWindowPolicySource::GenericFallback,
        };
        Self {
            raw_context_window_tokens: raw,
            usable_input_limit_tokens: usable_input,
            reserved_output_tokens: output,
            reserved_summary_tokens: summary,
            reserved_protocol_tokens: protocol,
            auto_compact_trigger_tokens: auto_compact_trigger,
            hard_input_limit_tokens: hard_input,
            source,
        }
    }

    /// Exit-gate target: a successful compaction must land at least ten
    /// percentage points below the trigger, measured against usable input.
    #[must_use]
    pub fn post_compaction_target_tokens(self) -> usize {
        self.auto_compact_trigger_tokens
            .saturating_sub(self.usable_input_limit_tokens / 10)
    }
}

impl ContextBudget {
    /// Create a ContextBudget from RuntimeConfig and explicit catalog limits.
    ///
    /// This bridges RuntimeConfig's strategy parameters with model-aware defaults.
    /// - Model limit comes from metadata or the generic fallback
    /// - compact_threshold, keep_recent_turns come from RuntimeConfig.compression
    /// - memory_budget_chars comes from RuntimeConfig.memory.max_memory_tokens
    pub fn from_runtime_config(
        config: &astra_config::runtime_config::RuntimeConfig,
        model: Option<&str>,
    ) -> Self {
        Self::from_runtime_config_with_context_window(config, model, None)
    }

    /// Create a ContextBudget from RuntimeConfig, applying an optional
    /// server-side model registry context_window before static model heuristics.
    pub fn from_runtime_config_with_context_window(
        config: &astra_config::runtime_config::RuntimeConfig,
        model: Option<&str>,
        context_window_tokens: Option<u32>,
    ) -> Self {
        // Get model-specific limits
        let base = budget_for_model_with_override(model, context_window_tokens);
        let compact_config = CompactConfig::from_env();
        let resolved_policy = ContextWindowPolicy::resolve(
            context_window_tokens,
            None,
            compact_config.summary_token_budget,
            config.compression.compression_threshold,
        );

        // Apply RuntimeConfig overrides
        Self {
            model_limit: base.model_limit,
            output_reserve_ratio: base.output_reserve_ratio,
            // Map compression_threshold (0.8 = 80% full) to compact_threshold
            compact_threshold: config.compression.compression_threshold,
            // preserve_recent_turns from RuntimeConfig
            keep_recent_turns: config.compression.preserve_recent_turns as usize,
            // Convert max_memory_tokens to chars (rough: 4 chars/token avg)
            memory_budget_chars: (config.memory.max_memory_tokens as usize) * 4,
            compact_config,
            resolved_policy: Some(resolved_policy),
        }
    }

    /// Usable input token budget after reserving headroom for output.
    pub fn effective_input_limit(&self) -> usize {
        self.resolved_policy.map_or_else(
            || (self.model_limit as f64 * (1.0 - self.output_reserve_ratio)) as usize,
            |policy| policy.usable_input_limit_tokens,
        )
    }

    /// Returns the token count at which auto-compact should trigger.
    /// Now based on `effective_input_limit` rather than raw `model_limit`.
    pub fn compact_trigger(&self) -> usize {
        self.resolved_policy.map_or_else(
            || (self.effective_input_limit() as f64 * self.compact_threshold) as usize,
            |policy| policy.auto_compact_trigger_tokens,
        )
    }

    #[must_use]
    pub fn window_policy(&self) -> ContextWindowPolicy {
        self.resolved_policy.unwrap_or_else(|| {
            let reserved_output =
                (self.model_limit as f64 * self.output_reserve_ratio.clamp(0.0, 1.0)) as usize;
            ContextWindowPolicy::resolve(
                u32::try_from(self.model_limit).ok(),
                u32::try_from(reserved_output).ok(),
                0,
                self.compact_threshold,
            )
        })
    }

    /// Whether the given token count exceeds the compact trigger.
    pub fn should_compact(&self, estimated_tokens: usize) -> bool {
        estimated_tokens > self.compact_trigger()
    }

    /// Determine the compaction tier for the current token usage.
    ///
    /// The tier boundaries are scaled relative to `compact_threshold`:
    /// - TrimSchemas: starts at 80% of compact_threshold
    /// - CompactHistory: starts at compact_threshold
    /// - AggressivePrune: starts at 113% of compact_threshold
    ///
    /// With default compact_threshold=0.75, this gives ~60%/75%/85% boundaries.
    /// With aggressive compact_threshold=0.60, boundaries become ~48%/60%/68%.
    pub fn compaction_tier(&self, estimated_tokens: usize) -> CompactionTier {
        let limit = self.effective_input_limit() as f64;
        let ratio = estimated_tokens as f64 / limit;

        // Scale tier boundaries based on compact_threshold
        let trim_start = self.compact_threshold * 0.80; // ~60% for default 0.75
        let compact_start = self.compact_threshold; // 75% for default 0.75
        let aggressive_start = self.compact_threshold * 1.133; // ~85% for default 0.75

        if ratio > aggressive_start {
            CompactionTier::AggressivePrune
        } else if ratio > compact_start {
            CompactionTier::CompactHistory
        } else if ratio > trim_start {
            CompactionTier::TrimSchemas
        } else {
            CompactionTier::Normal
        }
    }
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self {
            model_limit: DEFAULT_CONTEXT_WINDOW_TOKENS,
            compact_threshold: 0.75,
            keep_recent_turns: 6,
            memory_budget_chars: 8_000,
            output_reserve_ratio: 0.10,
            compact_config: CompactConfig::default(),
            resolved_policy: None,
        }
    }
}

/// Return a context budget for the current model.
///
/// Model names are intentionally not used as context-window metadata. If the
/// model registry has a `context_window`, call
/// [`budget_for_model_with_override`]; otherwise this returns the generic 200K
/// default.
/// Convenience wrapper around [`budget_for_model_with_override`].
pub fn budget_for_model(model: Option<&str>) -> ContextBudget {
    budget_for_model_with_override(model, None)
}

/// Return a context budget for the current model.
///
/// When `config_context_window` is provided (from `.models.yaml` or the DB),
/// it is authoritative. When it is absent, use the generic 200K default rather
/// than inferring from the model name.
pub fn budget_for_model_with_override(
    model: Option<&str>,
    config_context_window: Option<u32>,
) -> ContextBudget {
    budget_for_model_with_metadata(model, config_context_window, None)
}

/// Resolve an exact context policy from trusted catalog metadata.
///
/// `model` is accepted for call-site compatibility and trace correlation but
/// is deliberately not interpreted. Unknown/missing metadata follows one
/// generic fallback rather than model-name or provider-name matching.
pub fn budget_for_model_with_metadata(
    _model: Option<&str>,
    config_context_window: Option<u32>,
    max_completion_tokens: Option<u32>,
) -> ContextBudget {
    let default = ContextBudget::default();
    let policy = ContextWindowPolicy::resolve(
        config_context_window,
        max_completion_tokens,
        default.compact_config.summary_token_budget,
        default.compact_threshold,
    );
    ContextBudget {
        model_limit: policy.raw_context_window_tokens,
        output_reserve_ratio: if policy.raw_context_window_tokens == 0 {
            1.0
        } else {
            (policy.raw_context_window_tokens - policy.usable_input_limit_tokens) as f64
                / policy.raw_context_window_tokens as f64
        },
        resolved_policy: Some(policy),
        ..default
    }
}

/// Compatibility cap for directly constructed legacy budgets.
const DEFAULT_OUTPUT_TOKEN_CAP: usize = 16_384;

/// Resolve the outbound cap. Production policies return the exact catalog
/// completion limit. Direct legacy construction uses one documented fallback
/// cap rather than inferring a model class from window size.
pub fn capped_output_tokens(budget: &ContextBudget) -> usize {
    if let Some(policy) = budget.resolved_policy {
        return policy.reserved_output_tokens;
    }
    let full_reserve = (budget.model_limit as f64 * budget.output_reserve_ratio) as usize;
    full_reserve.min(DEFAULT_OUTPUT_TOKEN_CAP)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- helpers --

    fn msg(content: &str) -> serde_json::Value {
        json!({ "role": "user", "content": content })
    }

    fn tool_msg(args: &str) -> serde_json::Value {
        json!({
            "role": "assistant",
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": { "name": "test", "arguments": args }
            }]
        })
    }

    // === budget_for_model ===

    #[test]
    fn test_budget_for_model() {
        for model in [
            Some("gemini-2.5-pro"),
            Some("o1"),
            Some("gpt-5"),
            Some("claude-opus-4-6-20250514"),
            Some("deepseek-v4"),
            Some("unknown-model-xyz"),
            None,
            Some(""),
        ] {
            let b = budget_for_model(model);
            assert_eq!(b.model_limit, DEFAULT_CONTEXT_WINDOW_TOKENS);
            assert_eq!(
                b.window_policy().source,
                ContextWindowPolicySource::GenericFallback
            );
            assert_eq!(
                b.window_policy().reserved_output_tokens,
                DEFAULT_OUTPUT_RESERVE_TOKENS
            );
        }
    }

    #[test]
    fn test_budget_for_model_with_override() {
        // Explicit window metadata wins; model names remain inert.
        let b = budget_for_model_with_override(Some("claude-sonnet-4-20250514"), Some(50_000));
        assert_eq!(b.model_limit, 50_000);
        assert_eq!(
            b.window_policy().source,
            ContextWindowPolicySource::PartialModelCatalog
        );

        let b = budget_for_model_with_override(Some("unknown"), Some(100_000));
        assert_eq!(b.model_limit, 100_000);

        // A tiny catalog window is clamped arithmetically, not classified by
        // provider/model family.
        let b = budget_for_model_with_override(Some("claude-sonnet-4-20250514"), Some(10_000));
        assert_eq!(b.model_limit, 10_000);
        assert_eq!(b.window_policy().reserved_output_tokens, 2_500);

        let b = budget_for_model_with_override(Some("claude-sonnet-4-20250514"), None);
        assert_eq!(b.model_limit, DEFAULT_CONTEXT_WINDOW_TOKENS);

        let b = budget_for_model_with_override(Some("unknown-model"), None);
        assert_eq!(b.model_limit, DEFAULT_CONTEXT_WINDOW_TOKENS);

        let b = budget_for_model_with_metadata(Some("unknown-model"), Some(0), Some(0));
        assert_eq!(b.model_limit, DEFAULT_CONTEXT_WINDOW_TOKENS);
        assert_eq!(
            b.window_policy().source,
            ContextWindowPolicySource::GenericFallback
        );
    }

    #[test]
    fn catalog_completion_limit_drives_exact_one_million_token_policy() {
        let budget =
            budget_for_model_with_metadata(Some("opaque-model-id"), Some(1_000_000), Some(65_536));
        let policy = budget.window_policy();

        assert_eq!(policy.source, ContextWindowPolicySource::ModelCatalog);
        assert_eq!(policy.raw_context_window_tokens, 1_000_000);
        assert_eq!(policy.reserved_output_tokens, 65_536);
        assert_eq!(policy.reserved_summary_tokens, 20_000);
        assert_eq!(policy.reserved_protocol_tokens, 300);
        assert_eq!(policy.hard_input_limit_tokens, 934_164);
        assert_eq!(policy.usable_input_limit_tokens, 914_164);
        assert_eq!(policy.auto_compact_trigger_tokens, 685_623);
        assert_eq!(policy.post_compaction_target_tokens(), 594_207);
        assert_eq!(capped_output_tokens(&budget), 65_536);
    }

    // === effective_input_limit (8→1) ===

    #[test]
    fn test_effective_input_limit() {
        // default: 200k * 0.9 = 180_000
        let b = ContextBudget::default();
        assert_eq!(b.effective_input_limit(), 180_000);

        // Resolution reserves exact output, summary, and protocol budgets.
        let b = budget_for_model(Some("claude-sonnet-4-20250514"));
        assert_eq!(b.effective_input_limit(), 163_316);

        // Model names do not change the result.
        let b = budget_for_model(Some("gpt-4o-2024-08-06"));
        assert_eq!(b.effective_input_limit(), 163_316);

        // less than model_limit
        let b = ContextBudget::default();
        assert!(b.effective_input_limit() < b.model_limit);

        // zero reserve
        let b = ContextBudget {
            output_reserve_ratio: 0.0,
            ..Default::default()
        };
        assert_eq!(b.effective_input_limit(), b.model_limit);

        // full reserve
        let b = ContextBudget {
            model_limit: 100_000,
            output_reserve_ratio: 1.0,
            ..Default::default()
        };
        assert_eq!(b.effective_input_limit(), 0);

        // reserve exceeding 1.0
        let b = ContextBudget {
            model_limit: 100_000,
            output_reserve_ratio: 1.5,
            ..Default::default()
        };
        assert_eq!(b.effective_input_limit(), 0);
    }

    // === compaction_tier (15→2) ===

    #[test]
    fn test_compaction_tier() {
        // default budget
        let b = ContextBudget::default();
        let limit = b.effective_input_limit(); // 108_800

        // normal: < 60%
        assert_eq!(b.compaction_tier(0), CompactionTier::Normal);
        assert_eq!(
            b.compaction_tier((0.50 * limit as f64) as usize),
            CompactionTier::Normal
        );

        // trim_schemas: 60%–75% (0.75*0.80=0.60)
        assert_eq!(
            b.compaction_tier((0.65 * limit as f64) as usize),
            CompactionTier::TrimSchemas
        );

        // compact_history: 75%–85% (0.75*1.133≈0.85)
        assert_eq!(
            b.compaction_tier((0.78 * limit as f64) as usize),
            CompactionTier::CompactHistory
        );

        // aggressive_prune: >= 85%
        assert_eq!(
            b.compaction_tier((0.86 * limit as f64) as usize),
            CompactionTier::AggressivePrune
        );

        // at limit itself
        assert_eq!(b.compaction_tier(limit), CompactionTier::AggressivePrune);

        // boundary at ~60% (strict >)
        let b60 = (0.60 * limit as f64) as usize;
        assert_eq!(b.compaction_tier(b60), CompactionTier::Normal);
        assert_eq!(b.compaction_tier(b60 + 1), CompactionTier::TrimSchemas);

        // boundary at ~75% (strict >)
        let b75 = (0.75 * limit as f64) as usize;
        assert_eq!(b.compaction_tier(b75), CompactionTier::TrimSchemas);
        assert_eq!(b.compaction_tier(b75 + 1), CompactionTier::CompactHistory);

        // zero effective limit
        let bz = ContextBudget {
            model_limit: 0,
            output_reserve_ratio: 0.0,
            ..Default::default()
        };
        assert_eq!(bz.compaction_tier(10), CompactionTier::AggressivePrune);
    }

    #[test]
    fn test_tier_order_and_scaling() {
        // Verify ordering: Normal < TrimSchemas < CompactHistory < AggressivePrune
        let default = ContextBudget::default();
        assert_eq!(default.compact_threshold, 0.75);

        let trim_start = default.compact_threshold * 0.80; // 0.60
        let compact_start = default.compact_threshold; // 0.75
        let aggressive_start = default.compact_threshold * 1.133; // ~0.85

        assert!(trim_start < compact_start);
        assert!(compact_start < aggressive_start);

        // Budget pressure values: all tiers present
        let budget = ContextBudget {
            model_limit: 100_000,
            output_reserve_ratio: 0.0,
            compact_threshold: 0.75,
            ..Default::default()
        };
        assert_eq!(
            budget.compaction_tier((0.50 * 100_000.0) as usize),
            CompactionTier::Normal
        );
        assert_eq!(
            budget.compaction_tier((0.65 * 100_000.0) as usize),
            CompactionTier::TrimSchemas
        );
        assert_eq!(
            budget.compaction_tier((0.80 * 100_000.0) as usize),
            CompactionTier::CompactHistory
        );
        assert_eq!(
            budget.compaction_tier((0.90 * 100_000.0) as usize),
            CompactionTier::AggressivePrune
        );

        // Scaling with threshold
        let aggressive = ContextBudget {
            model_limit: 100_000,
            output_reserve_ratio: 0.0,
            compact_threshold: 0.60,
            ..Default::default()
        };
        // With threshold=0.60: trim_start=0.48, compact_start=0.60, aggressive_start≈0.68
        assert_eq!(
            aggressive.compaction_tier((0.30 * 100_000.0) as usize),
            CompactionTier::Normal
        );
        assert_eq!(
            aggressive.compaction_tier((0.50 * 100_000.0) as usize),
            CompactionTier::TrimSchemas
        );
        assert_eq!(
            aggressive.compaction_tier((0.65 * 100_000.0) as usize),
            CompactionTier::CompactHistory
        );
        assert_eq!(
            aggressive.compaction_tier((0.70 * 100_000.0) as usize),
            CompactionTier::AggressivePrune
        );
    }

    // === cache_aware estimation (5→1) ===

    #[test]
    fn test_cache_aware_estimation() {
        // empty
        let est = estimate_tokens_cache_aware(&[], 500);
        assert_eq!(est.total_tokens, 500);
        assert_eq!(est.cache_eligible_tokens, 500);
        assert_eq!(est.volatile_tokens, 0);

        // system only
        let messages = vec![msg(&"a".repeat(80))];
        let system_tokens = estimate_single_message_tokens(&messages[0]) + PER_MESSAGE_OVERHEAD;
        let est = estimate_tokens_cache_aware(&messages, 100);
        assert_eq!(est.cache_eligible_tokens, system_tokens + 100);
        assert_eq!(est.volatile_tokens, 0);
        assert_eq!(est.total_tokens, system_tokens + 100);

        // separates system from conversation
        let messages = vec![
            msg(&"s".repeat(400)),
            msg(&"u".repeat(200)),
            msg(&"a".repeat(100)),
        ];
        let system_tokens = estimate_single_message_tokens(&messages[0]) + PER_MESSAGE_OVERHEAD;
        let volatile_tokens = messages[1..]
            .iter()
            .map(|message| estimate_single_message_tokens(message) + PER_MESSAGE_OVERHEAD)
            .sum::<usize>();
        let schema_tokens = 200;
        let est = estimate_tokens_cache_aware(&messages, schema_tokens);
        assert_eq!(est.cache_eligible_tokens, system_tokens + schema_tokens);
        assert_eq!(est.volatile_tokens, volatile_tokens);
        assert_eq!(
            est.total_tokens,
            est.cache_eligible_tokens + est.volatile_tokens
        );

        // with tool calls
        let messages = vec![msg("system prompt"), tool_msg(&"x".repeat(120))];
        let tool_tokens = estimate_single_message_tokens(&messages[1]) + PER_MESSAGE_OVERHEAD;
        let est = estimate_tokens_cache_aware(&messages, 0);
        assert_eq!(est.volatile_tokens, tool_tokens);

        // split matches joined
        let stable = vec![msg(&"s".repeat(320))];
        let volatile = vec![msg(&"u".repeat(180)), msg(&"a".repeat(96))];
        let mut joined = stable.clone();
        joined.extend(volatile.clone());
        let schema_tokens = 123;
        let split = estimate_tokens_cache_aware_split(&stable, &volatile, schema_tokens);
        let joined_est = estimate_tokens_cache_aware(&joined, schema_tokens);
        assert_eq!(split.total_tokens, joined_est.total_tokens);
        assert_eq!(
            split.cache_eligible_tokens,
            joined_est.cache_eligible_tokens
        );
        assert_eq!(split.volatile_tokens, joined_est.volatile_tokens);
    }

    // === estimate_str_tokens (18→3) ===

    #[test]
    fn test_estimate_str_tokens() {
        // pure ASCII — char/4 (integer division, floor)
        assert_eq!(estimate_str_tokens("hello world"), 2); // 11 chars / 4 = 2
        assert_eq!(
            estimate_str_tokens("This is a pure ASCII sentence for testing."),
            10
        ); // 42/4=10 (floor)

        // pure CJK — (cjkc*3).div_ceil(2)
        assert_eq!(estimate_str_tokens("你好世界"), 6); // (4*3)/2 = 6
        assert_eq!(estimate_str_tokens("你好世界测试"), 9); // (6*3)/2 = 9
        assert_eq!(estimate_str_tokens("你好世界测试纯中文"), 14); // (9*3)/2 = 14
        assert_eq!(estimate_str_tokens("かなカナ"), 6); // Japanese kana
        assert_eq!(estimate_str_tokens("한글테스트"), 8); // Hangul
        assert_eq!(estimate_str_tokens("Ελληνικά"), 12); // Greek

        // mixed EN+CN
        let mixed = estimate_str_tokens("hello 你好 world 世界");
        assert!(mixed > 0);

        // CJK punctuation
        let punct = estimate_str_tokens("你好，世界！");
        assert!(punct > 0);
        let punct2 = estimate_str_tokens("你好「世界」測試《內容》——標點");
        assert!(punct2 > 0);

        // empty
        assert_eq!(estimate_str_tokens(""), 0);

        // code with CJK
        let code = estimate_str_tokens(r#"fn main() { println!("你好"); }"#);
        assert!(code > 0);

        // JSON ↔ smaller divisor
        let json_tokens = estimate_str_tokens("{\"key\": \"value\"}");
        assert!(json_tokens > 0);

        // array-like
        let arr = estimate_str_tokens("[1, 2, 3, 4, 5]");
        assert!(arr > 0);

        // single char
        assert_eq!(estimate_str_tokens("a"), 0); // 1/4 = 0
        assert_eq!(estimate_str_tokens("你"), 2); // (1*3)/2 ceiling = 2

        // emoji
        let emoji = estimate_str_tokens("😀🎉");
        assert!(emoji > 0);

        // whitespace-prefixed JSON
        let json = estimate_str_tokens("  {\"a\": 1}");
        assert!(json > 0);

        // CJK mixed sentence
        let mixed_sentence = estimate_str_tokens(
            "这是一个包含中文和English的混合句子mixed sentence with CJK中文和英文English",
        );
        assert!(mixed_sentence > 15);
    }

    #[test]
    fn test_estimate_str_tokens_cjk_penalty() {
        // CJK gets ~2× tokens per character vs ASCII
        let ascii = estimate_str_tokens("AAAA"); // 4 ASCII chars → char/4 = 1
        let cjk = estimate_str_tokens("啊啊啊啊"); // 4 CJK → 4/2 = 2
        assert!(
            cjk > ascii,
            "CJK tokens ({}) should exceed ASCII tokens ({})",
            cjk,
            ascii
        );

        // CJK heavy content > old heuristic
        let cjk_heavy = estimate_str_tokens(
            "你好世界这是测试用例包含大量中文内容用于验证新的估算方法是否比旧的启发式更准确",
        );
        assert!(cjk_heavy > 10);
    }

    #[test]
    fn test_estimate_str_tokens_edge_cases() {
        // empty
        assert_eq!(estimate_str_tokens(""), 0);

        // pure CJK no ASCII
        let cjk = estimate_str_tokens("你好世界测试纯中文");
        assert!(cjk > 0);

        // pure ASCII
        let ascii = estimate_str_tokens("The quick brown fox jumps over the lazy dog");
        assert!(ascii > 0);
    }

    // === estimate_tokens (8→1) ===

    #[test]
    fn test_estimate_tokens() {
        // includes schema in estimate
        let tokens_a = estimate_tokens(&[msg("hello")], 100, 0);
        let tokens_b = estimate_tokens(&[msg("hello")], 1000, 0);
        assert!(tokens_b > tokens_a, "more schema => more tokens");

        // empty messages has overhead
        let tokens = estimate_tokens(&[], 0, 0);
        assert!(tokens > 0, "empty session should have base overhead");

        // CJK message
        let tokens = estimate_tokens(&[msg("你好世界测试")], 0, 0);
        assert!(tokens > 0);

        // message without content
        let tokens = estimate_tokens(&[json!({"role": "assistant"})], 0, 0);
        assert!(tokens > 0);

        // tool call tokens included
        let tokens = estimate_tokens(&[tool_msg(&"x".repeat(120))], 0, 0);
        assert!(tokens > 30);

        // Provider block arrays must count their text instead of looking like
        // empty content merely because `content` is not a string.
        let block_message = json!({
            "role": "user",
            "content": [{"type": "text", "text": "你好世界".repeat(100)}]
        });
        assert!(estimate_single_message_tokens(&block_message) > 400);

        // with schema dwarfs estimate without for CJK
        let cjk_session: Vec<_> = (0..5)
            .map(|_| msg("你好世界测试中文内容大量中文"))
            .collect();
        let without_schema = estimate_tokens(&cjk_session, 0, 0);
        let with_schema = estimate_tokens(&cjk_session, 50_000, 0);
        assert!(with_schema > without_schema * 2);

        // large CJK session produces substantial token estimate
        let _ = ContextBudget::default();
        let large_cjk: Vec<_> = (0..200)
            .map(|_| msg("你好世界测试中文内容大量中文"))
            .collect();
        let est = estimate_tokens(&large_cjk, 25_000, 0);
        assert!(est > 40_000, "est={est}");
    }

    // === capped_output_tokens (10→2) ===

    #[test]
    fn test_capped_output_tokens() {
        // default (200k model) reserves more than the hard cap.
        let b = ContextBudget::default();
        let cap = capped_output_tokens(&b);
        assert_eq!(cap, 16_384);

        // large model (>128k)
        let b = budget_for_model(Some("claude-sonnet-4-20250514"));
        assert_eq!(capped_output_tokens(&b), 16_384);

        // small model
        let b = ContextBudget {
            model_limit: 30_000,
            output_reserve_ratio: 0.15,
            ..Default::default()
        };
        // full_reserve = 4500, min(4500, 8192) = 4500
        assert_eq!(capped_output_tokens(&b), 4_500);

        // model names do not alter the configured/default budget.
        let b = budget_for_model(Some("gemini-2.5-pro"));
        assert_eq!(capped_output_tokens(&b), 16_384);
    }

    #[test]
    fn test_capped_output_tokens_edge() {
        // zero model limit
        let b = ContextBudget {
            model_limit: 0,
            output_reserve_ratio: 0.1,
            ..Default::default()
        };
        assert_eq!(capped_output_tokens(&b), 0);

        // tiny 4k model
        let b = ContextBudget {
            model_limit: 4_096,
            output_reserve_ratio: 0.10,
            ..Default::default()
        };
        assert_eq!(capped_output_tokens(&b), 409);

        // extreme reserve ratio still capped
        let b = ContextBudget {
            model_limit: 200_000,
            output_reserve_ratio: 0.5,
            ..Default::default()
        };
        // full_reserve = 100_000, min(100000, 16384) = 16384
        assert_eq!(capped_output_tokens(&b), 16_384);

        // minimum 1 token
        let b = ContextBudget {
            model_limit: 10,
            output_reserve_ratio: 0.1,
            ..Default::default()
        };
        assert_eq!(capped_output_tokens(&b), 1);
    }

    // === compact trigger & should_compact (2→1) ===

    #[test]
    fn test_compact_trigger_and_should_compact() {
        let b = ContextBudget {
            model_limit: 200_000,
            output_reserve_ratio: 0.1,
            compact_threshold: 0.75,
            ..Default::default()
        };
        let trigger = b.compact_trigger();
        assert_eq!(trigger, (b.effective_input_limit() as f64 * 0.75) as usize);

        // should_compact uses > (strict)
        assert!(!b.should_compact(trigger));
        assert!(b.should_compact(trigger + 1));
        assert!(!b.should_compact(trigger - 1));

        // zero threshold
        let b = ContextBudget {
            compact_threshold: 0.0,
            output_reserve_ratio: 0.0,
            model_limit: 100_000,
            ..Default::default()
        };
        assert_eq!(b.compact_trigger(), 0);
        assert!(b.should_compact(1)); // 1 > 0
    }

    // === should_summarize (4→1) ===

    #[test]
    fn test_should_summarize() {
        // disabled always false
        let c = CompactConfig {
            enable_summary: false,
            ..Default::default()
        };
        assert!(!c.should_summarize(CompactionTier::Normal));
        assert!(!c.should_summarize(CompactionTier::AggressivePrune));

        // respects min_tier (default: CompactHistory)
        let c = CompactConfig::default();
        assert!(!c.should_summarize(CompactionTier::Normal));
        assert!(!c.should_summarize(CompactionTier::TrimSchemas));
        assert!(c.should_summarize(CompactionTier::CompactHistory));
        assert!(c.should_summarize(CompactionTier::AggressivePrune));

        // at lowest min_tier
        let c = CompactConfig {
            enable_summary: true,
            summary_min_tier: CompactionTier::Normal,
            ..Default::default()
        };
        assert!(c.should_summarize(CompactionTier::Normal));
        assert!(c.should_summarize(CompactionTier::AggressivePrune));

        // at highest min_tier
        let c = CompactConfig {
            enable_summary: true,
            summary_min_tier: CompactionTier::AggressivePrune,
            ..Default::default()
        };
        assert!(!c.should_summarize(CompactionTier::CompactHistory));
        assert!(c.should_summarize(CompactionTier::AggressivePrune));
    }

    // === default values & from_runtime_config (4→1) ===

    #[test]
    fn test_defaults_and_from_runtime_config() {
        // default budget
        let b = ContextBudget::default();
        assert_eq!(b.model_limit, DEFAULT_CONTEXT_WINDOW_TOKENS);
        assert_eq!(b.compact_threshold, 0.75);
        assert_eq!(b.keep_recent_turns, 6);
        assert_eq!(b.memory_budget_chars, 8_000);
        assert_eq!(b.output_reserve_ratio, 0.10);

        // default compact config
        let c = CompactConfig::default();
        assert!(c.enable_summary);
        assert_eq!(c.summary_token_budget, 20_000);
        assert_eq!(c.max_ptl_retries, 3);
        assert_eq!(c.summary_min_tier, CompactionTier::CompactHistory);

        // from_runtime_config without registry metadata uses the generic default.
        let config = astra_config::runtime_config::RuntimeConfig::default();
        let budget = ContextBudget::from_runtime_config(&config, Some("claude-sonnet-4-20250514"));
        assert_eq!(budget.model_limit, DEFAULT_CONTEXT_WINDOW_TOKENS);

        let budget = ContextBudget::from_runtime_config_with_context_window(
            &config,
            Some("custom-model"),
            Some(500_000),
        );
        assert_eq!(budget.model_limit, 500_000);

        // from_runtime_config applies compression settings
        use astra_config::runtime_config::{CompressionConfig, RuntimeConfig};
        let config = RuntimeConfig {
            compression: CompressionConfig {
                compression_threshold: 0.5,
                preserve_recent_turns: 10,
                ..CompressionConfig::default()
            },
            ..RuntimeConfig::default()
        };
        let budget = ContextBudget::from_runtime_config(&config, Some("unknown-model"));
        assert_eq!(budget.compact_threshold, 0.5);
        assert_eq!(budget.keep_recent_turns, 10);
    }
}
