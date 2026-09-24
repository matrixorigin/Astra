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
/// * `system_prompt_tokens` — measured tokens of the assembled system lane, or
///   0 to use the conservative pre-assembly fallback.  Once a provider
///   manifest exists, callers should pass that measured value; the fallback is
///   not a claim about the ordinary resident prompt size.
///
/// This is the canonical-history estimator: callers pass conversation history
/// that does not already contain the assembled system prompt. For a final
/// provider message list, use [`estimate_wire_input_tokens`] instead so the
/// system prompt is not counted twice.
///
/// Conservative pre-assembly fallback for a full prompt that may include
/// optional skill and deferred-tool catalogs.  Ordinary requests are smaller
/// (the current resident/Work surface is measured separately), and the shared
/// manifest replaces this fallback after the first assembled provider request.
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

/// Return the measured prompt overhead that can be added to canonical
/// conversation history estimates.
///
/// The generic history estimator has to use a conservative fallback before a
/// provider request has been assembled. Once a request has crossed the shared
/// assembler, its manifest is the authoritative source for the provider
/// visible system lane. The final wire budget is preferred because it also
/// includes runtime-owned system frames added after the pipeline pass; the
/// pipeline lanes are retained as a fallback for traces captured earlier.
/// Reading these typed fields avoids carrying a second prompt-size constant
/// through the agentic loop. Missing or malformed fields remain unknown and
/// keep the estimator's conservative fallback.
pub(crate) fn measured_prompt_tokens_from_manifest(
    trace: Option<&serde_json::Value>,
) -> Option<usize> {
    let trace = trace?;
    if let Some(system) = trace
        .pointer("/wire/budget/estimated_system_tokens")
        .and_then(serde_json::Value::as_u64)
    {
        return Some(system.min(usize::MAX as u64) as usize);
    }
    let stable = trace.pointer("/system_prompt_tokens")?.as_u64()?;
    let volatile = trace
        .pointer("/volatile_preamble_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let total = stable.saturating_add(volatile);
    (total > 0).then_some(total.min(usize::MAX as u64) as usize)
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

/// Estimate the provider-visible input portion of one already assembled wire
/// request.
///
/// `messages` is the final message projection and therefore already contains
/// any system prompt. Do not add [`DEFAULT_SYSTEM_PROMPT_TOKENS`] here: doing
/// so would count a system prompt twice. The small framing allowance is kept
/// here so provider admission and final-wire diagnostics share one formula.
pub(crate) fn estimate_wire_input_tokens(
    messages: &[serde_json::Value],
    tool_schema_tokens: usize,
) -> usize {
    estimate_tokens_cache_aware_split(&[], messages, tool_schema_tokens)
        .total_tokens
        .saturating_add(MODEL_FRAMING_TOKENS)
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
pub use astra_turn_types::context_execution::{
    CompactConfig, CompactionTier, ContextBudget, ContextWindowPolicy, ContextWindowPolicySource,
    DEFAULT_CONTEXT_WINDOW_TOKENS,
};

/// Output cap from the exact resolved policy, with no second resolution.
pub fn capped_output_tokens(budget: &ContextBudget) -> usize {
    budget.capped_output_tokens()
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

    #[test]
    fn catalog_completion_limit_drives_exact_one_million_token_policy() {
        let budget = ContextBudget::resolve(
            Some(1_000_000),
            Some(65_536),
            0.75,
            6,
            8_000,
            CompactConfig::default(),
        );
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
        let _ = ContextBudget::resolve(None, None, 0.75, 6, 8_000, CompactConfig::default());
        let large_cjk: Vec<_> = (0..200)
            .map(|_| msg("你好世界测试中文内容大量中文"))
            .collect();
        let est = estimate_tokens(&large_cjk, 25_000, 0);
        assert!(est > 40_000, "est={est}");
    }

    #[test]
    fn measured_prompt_tokens_from_manifest_uses_typed_lanes() {
        let trace = json!({
            "system_prompt_tokens": 4_000,
            "volatile_preamble_tokens": 125,
            "wire": {
                "budget": {
                    "estimated_system_tokens": 3_900,
                },
            },
        });
        assert_eq!(
            measured_prompt_tokens_from_manifest(Some(&trace)),
            Some(3_900)
        );
        let pipeline_trace = json!({
            "system_prompt_tokens": 4_000,
            "volatile_preamble_tokens": 125,
        });
        assert_eq!(
            measured_prompt_tokens_from_manifest(Some(&pipeline_trace)),
            Some(4_125)
        );
        assert_eq!(measured_prompt_tokens_from_manifest(None), None);
        assert_eq!(
            measured_prompt_tokens_from_manifest(Some(&json!({
                "system_prompt_tokens": 0,
            }))),
            None
        );
    }

    #[test]
    fn wire_input_estimate_counts_system_message_once() {
        let messages = vec![
            json!({"role": "system", "content": "stable system policy"}),
            json!({"role": "user", "content": "hello"}),
        ];
        let schema_tokens = 123;
        let message_and_schema =
            estimate_tokens_cache_aware_split(&[], &messages, schema_tokens).total_tokens;

        assert_eq!(
            estimate_wire_input_tokens(&messages, schema_tokens),
            message_and_schema + MODEL_FRAMING_TOKENS,
            "an already assembled wire message list must not receive the default system estimate"
        );
        assert_ne!(
            estimate_wire_input_tokens(&messages, schema_tokens),
            estimate_tokens(&messages, schema_tokens, 0),
            "the generic history estimator intentionally has different input semantics"
        );
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
}
