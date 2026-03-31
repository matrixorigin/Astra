/// CJK-aware token estimation for a string.
///
/// BPE tokenizers (GPT-4, Claude, etc.) encode CJK characters at ~1.5 tokens
/// each on average — a single character often splits into 2 BPE tokens, but
/// common bigrams merge back.  We use 3/2 integer arithmetic for accuracy
/// without floating-point.  ASCII text averages ~4 bytes per token.
pub fn estimate_str_tokens(s: &str) -> usize {
    let mut cjk_chars: usize = 0;
    let mut ascii_bytes: usize = 0;
    for ch in s.chars() {
        if ('\u{4E00}'..='\u{9FFF}').contains(&ch)
            || ('\u{3400}'..='\u{4DBF}').contains(&ch)
            || ('\u{F900}'..='\u{FAFF}').contains(&ch)
            || ('\u{3000}'..='\u{303F}').contains(&ch)
            || ('\u{FF00}'..='\u{FFEF}').contains(&ch)
        {
            cjk_chars += 1;
        } else {
            ascii_bytes += ch.len_utf8();
        }
    }
    // CJK: ~1.5 tokens per char (3*n/2). ASCII: ~4 bytes per token.
    (cjk_chars * 3).div_ceil(2) + ascii_bytes / 4
}

/// Approximate token count with CJK-aware estimation.
/// Adds overhead per message for role/formatting tokens, plus estimated
/// system prompt and tool schema overhead that the LLM API counts but
/// we don't see in the messages array.
pub fn estimate_tokens(messages: &[serde_json::Value]) -> usize {
    const PER_MESSAGE_OVERHEAD: usize = 4; // role + separators
    // System prompt (~1.2K tokens) + tool schemas (~1.5K tokens avg) + model framing
    const FIXED_OVERHEAD: usize = 3000;
    let message_tokens: usize = messages
        .iter()
        .map(|m| estimate_single_message_tokens(m) + PER_MESSAGE_OVERHEAD)
        .sum();
    message_tokens + FIXED_OVERHEAD
}

/// Precise token estimation using actual overhead measurements instead of the
/// hardcoded 3,000-token `FIXED_OVERHEAD`.
///
/// * `schema_token_total` — sum of measured token costs for all selected tool
///   schemas (from `ToolRegistry::token_cost`).
/// * `system_prompt_tokens` — estimated tokens of the system prompt, or 0 to
///   use the default 1,200 estimate.
///
/// This produces more accurate compaction-tier decisions, especially under
/// CJK-heavy conversations where the old estimate was 50% too low.
pub fn estimate_tokens_precise(
    messages: &[serde_json::Value],
    schema_token_total: usize,
    system_prompt_tokens: usize,
) -> usize {
    const PER_MESSAGE_OVERHEAD: usize = 4;
    const MODEL_FRAMING: usize = 300; // JSON wrappers, role tokens, separators
    const DEFAULT_SYSTEM_PROMPT: usize = 1200;

    let sys_tokens = if system_prompt_tokens > 0 {
        system_prompt_tokens
    } else {
        DEFAULT_SYSTEM_PROMPT
    };

    let message_tokens: usize = messages
        .iter()
        .map(|m| estimate_single_message_tokens(m) + PER_MESSAGE_OVERHEAD)
        .sum();
    message_tokens + sys_tokens + schema_token_total + MODEL_FRAMING
}

/// Estimate tokens for a single message (content + tool_calls arguments).
fn estimate_single_message_tokens(m: &serde_json::Value) -> usize {
    let content_tokens = m
        .get("content")
        .and_then(|v| v.as_str())
        .map(estimate_str_tokens)
        .unwrap_or(0);
    let tool_call_tokens = m
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|tc| {
                    tc.get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(|a| a.as_str())
                        .map(estimate_str_tokens)
                        .unwrap_or(0)
                })
                .sum::<usize>()
        })
        .unwrap_or(0);
    content_tokens + tool_call_tokens
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
    const PER_MESSAGE_OVERHEAD: usize = 4;

    let (system_tokens, volatile_tokens) = if messages.is_empty() {
        (0, 0)
    } else {
        let sys = estimate_single_message_tokens(&messages[0]) + PER_MESSAGE_OVERHEAD;
        let vol: usize = messages[1..]
            .iter()
            .map(|m| estimate_single_message_tokens(m) + PER_MESSAGE_OVERHEAD)
            .sum();
        (sys, vol)
    };

    let cache_eligible = system_tokens + tool_schema_tokens;
    let total = cache_eligible + volatile_tokens;

    CacheAwareEstimate {
        total_tokens: total,
        cache_eligible_tokens: cache_eligible,
        volatile_tokens,
    }
}

// ---------------------------------------------------------------------------
// Tiered compaction strategy
// ---------------------------------------------------------------------------

/// Compaction urgency tiers — each tier triggers progressively more aggressive
/// context reduction strategies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionTier {
    /// < 60% of effective input limit — no action needed.
    Normal,
    /// 60–75% — reduce dynamic tool schemas to free headroom.
    TrimSchemas,
    /// 75–85% — compact older conversation turns, keep recent.
    CompactHistory,
    /// > 85% — aggressive pruning, summarize entire history.
    AggressivePrune,
}

impl CompactionTier {
    /// Scalar 0.0–0.9 for edge tool output scaling / selection (`chat_stream`, `ToolSelector`).
    #[must_use]
    pub fn budget_pressure(self) -> f64 {
        match self {
            Self::Normal => 0.0,
            Self::TrimSchemas => 0.3,
            Self::CompactHistory => 0.6,
            Self::AggressivePrune => 0.9,
        }
    }
}

// ---------------------------------------------------------------------------
// Context budget
// ---------------------------------------------------------------------------

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
    /// Fraction of model_limit reserved for output generation.
    /// `effective_input_limit = model_limit * (1.0 - output_reserve_ratio)`.
    pub output_reserve_ratio: f64,
}

impl ContextBudget {
    /// Usable input token budget after reserving headroom for output.
    pub fn effective_input_limit(&self) -> usize {
        (self.model_limit as f64 * (1.0 - self.output_reserve_ratio)) as usize
    }

    /// Returns the token count at which auto-compact should trigger.
    /// Now based on `effective_input_limit` rather than raw `model_limit`.
    pub fn compact_trigger(&self) -> usize {
        (self.effective_input_limit() as f64 * self.compact_threshold) as usize
    }

    /// Whether the given token count exceeds the compact trigger.
    pub fn should_compact(&self, estimated_tokens: usize) -> bool {
        estimated_tokens > self.compact_trigger()
    }

    /// Determine the compaction tier for the current token usage.
    pub fn compaction_tier(&self, estimated_tokens: usize) -> CompactionTier {
        let limit = self.effective_input_limit() as f64;
        let ratio = estimated_tokens as f64 / limit;
        if ratio > 0.85 {
            CompactionTier::AggressivePrune
        } else if ratio > 0.75 {
            CompactionTier::CompactHistory
        } else if ratio > 0.60 {
            CompactionTier::TrimSchemas
        } else {
            CompactionTier::Normal
        }
    }
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self {
            model_limit: 128_000,
            compact_threshold: 0.75,
            keep_recent_turns: 4,
            memory_budget_chars: 8_000,
            output_reserve_ratio: 0.15,
        }
    }
}

/// Return a ContextBudget tuned for a known model name.
pub fn budget_for_model(model: Option<&str>) -> ContextBudget {
    let name = model.unwrap_or("");

    let (limit, reserve) = match name {
        // OpenAI — GPT-5 family (256K context)
        m if m.contains("gpt-5") => (256_000, 0.12),
        // OpenAI — GPT-4o / GPT-4.1 (128K, 16K output)
        m if m.contains("gpt-4o") || m.contains("gpt-4.1") => (128_000, 0.12),
        m if m.contains("gpt-4-turbo") => (128_000, 0.12),
        m if m.contains("gpt-3.5") => (16_000, 0.12),
        // OpenAI — o1/o3 reasoning models (200K context)
        m if m.contains("o1") || m.contains("o3") => (200_000, 0.15),
        // Anthropic — Claude (200K context, large output window)
        m if m.contains("claude") => (200_000, 0.20),
        // Google — Gemini (1M context)
        m if m.contains("gemini") => (1_000_000, 0.10),
        // DeepSeek (64K context)
        m if m.contains("deepseek") => (64_000, 0.15),
        // Moonshot / Kimi
        m if m.contains("kimi") || m.contains("moonshot") => (128_000, 0.15),
        // Qwen
        m if m.contains("qwen") => (128_000, 0.15),
        // Safe default
        _ => (128_000, 0.15),
    };

    ContextBudget {
        model_limit: limit,
        output_reserve_ratio: reserve,
        ..Default::default()
    }
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

    // ---------------------------------------------------------------
    // 1. Output reservation
    // ---------------------------------------------------------------

    #[test]
    fn effective_input_limit_default() {
        let b = ContextBudget::default();
        // 128_000 * (1 - 0.15) = 108_800
        assert_eq!(b.effective_input_limit(), 108_800);
    }

    #[test]
    fn effective_input_limit_claude() {
        let b = budget_for_model(Some("claude-3.5-sonnet"));
        // 200_000 * (1 - 0.20) = 160_000
        assert_eq!(b.effective_input_limit(), 160_000);
    }

    #[test]
    fn effective_input_limit_gpt4o() {
        let b = budget_for_model(Some("gpt-4o-2024-08-06"));
        // 128_000 * (1 - 0.12) = 112_640
        assert_eq!(b.effective_input_limit(), 112_640);
    }

    #[test]
    fn effective_input_limit_deepseek() {
        let b = budget_for_model(Some("deepseek-chat"));
        // 64_000 * (1 - 0.15) = 54_400
        assert_eq!(b.effective_input_limit(), 54_400);
    }

    // ---------------------------------------------------------------
    // 2. Cache-aware estimation
    // ---------------------------------------------------------------

    #[test]
    fn cache_aware_empty_messages() {
        let est = estimate_tokens_cache_aware(&[], 500);
        assert_eq!(est.total_tokens, 500);
        assert_eq!(est.cache_eligible_tokens, 500);
        assert_eq!(est.volatile_tokens, 0);
    }

    #[test]
    fn cache_aware_system_only() {
        // 80 chars => 20 tokens + 4 overhead = 24
        let messages = vec![msg(&"a".repeat(80))];
        let est = estimate_tokens_cache_aware(&messages, 100);
        assert_eq!(est.cache_eligible_tokens, 24 + 100);
        assert_eq!(est.volatile_tokens, 0);
        assert_eq!(est.total_tokens, 24 + 100);
    }

    #[test]
    fn cache_aware_separates_system_from_conversation() {
        let messages = vec![
            msg(&"s".repeat(400)), // system: 100 tok + 4 = 104
            msg(&"u".repeat(200)), // user:    50 tok + 4 =  54
            msg(&"a".repeat(100)), // asst:    25 tok + 4 =  29
        ];
        let schema_tokens = 200;
        let est = estimate_tokens_cache_aware(&messages, schema_tokens);

        assert_eq!(est.cache_eligible_tokens, 104 + 200);
        assert_eq!(est.volatile_tokens, 54 + 29);
        assert_eq!(
            est.total_tokens,
            est.cache_eligible_tokens + est.volatile_tokens
        );
    }

    #[test]
    fn cache_aware_with_tool_calls() {
        let messages = vec![
            msg("system prompt"),
            tool_msg(&"x".repeat(120)), // 30 tokens + 4 = 34
        ];
        let est = estimate_tokens_cache_aware(&messages, 0);
        assert_eq!(est.volatile_tokens, 34);
    }

    // ---------------------------------------------------------------
    // 3. Tiered compaction
    // ---------------------------------------------------------------

    #[test]
    fn compaction_tier_normal() {
        let b = ContextBudget::default();
        let limit = b.effective_input_limit(); // 108_800
        let tokens = (limit as f64 * 0.50) as usize;
        assert_eq!(b.compaction_tier(tokens), CompactionTier::Normal);
    }

    #[test]
    fn compaction_tier_trim_schemas() {
        let b = ContextBudget::default();
        let limit = b.effective_input_limit();
        let tokens = (limit as f64 * 0.65) as usize;
        assert_eq!(b.compaction_tier(tokens), CompactionTier::TrimSchemas);
    }

    #[test]
    fn compaction_tier_compact_history() {
        let b = ContextBudget::default();
        let limit = b.effective_input_limit();
        let tokens = (limit as f64 * 0.80) as usize;
        assert_eq!(b.compaction_tier(tokens), CompactionTier::CompactHistory);
    }

    #[test]
    fn compaction_tier_aggressive_prune() {
        let b = ContextBudget::default();
        let limit = b.effective_input_limit();
        let tokens = (limit as f64 * 0.90) as usize;
        assert_eq!(b.compaction_tier(tokens), CompactionTier::AggressivePrune);
    }

    #[test]
    fn compaction_tier_budget_pressure_values() {
        assert_eq!(CompactionTier::Normal.budget_pressure(), 0.0);
        assert_eq!(CompactionTier::TrimSchemas.budget_pressure(), 0.3);
        assert_eq!(CompactionTier::CompactHistory.budget_pressure(), 0.6);
        assert_eq!(CompactionTier::AggressivePrune.budget_pressure(), 0.9);
    }

    #[test]
    fn compaction_tier_boundary_60() {
        let b = ContextBudget::default();
        let limit = b.effective_input_limit();
        // Exactly at 60% boundary — should be Normal (> 0.60 triggers TrimSchemas)
        let tokens = (limit as f64 * 0.60) as usize;
        assert_eq!(b.compaction_tier(tokens), CompactionTier::Normal);
    }

    #[test]
    fn compaction_tier_boundary_75() {
        let b = ContextBudget::default();
        let limit = b.effective_input_limit();
        // Exactly at 75% — still TrimSchemas (> 0.75 triggers CompactHistory)
        let tokens = (limit as f64 * 0.75) as usize;
        assert_eq!(b.compaction_tier(tokens), CompactionTier::TrimSchemas);
    }

    // ---------------------------------------------------------------
    // 4. Model detection
    // ---------------------------------------------------------------

    #[test]
    fn model_gemini() {
        let b = budget_for_model(Some("gemini-1.5-pro"));
        assert_eq!(b.model_limit, 1_000_000);
        assert!((b.output_reserve_ratio - 0.10).abs() < f64::EPSILON);
    }

    #[test]
    fn model_o1() {
        let b = budget_for_model(Some("o1-preview"));
        assert_eq!(b.model_limit, 200_000);
    }

    #[test]
    fn model_o3() {
        let b = budget_for_model(Some("o3-mini"));
        assert_eq!(b.model_limit, 200_000);
    }

    #[test]
    fn model_gpt5() {
        let b = budget_for_model(Some("gpt-5-turbo"));
        assert_eq!(b.model_limit, 256_000);
        assert!((b.output_reserve_ratio - 0.12).abs() < f64::EPSILON);
    }

    #[test]
    fn model_claude() {
        let b = budget_for_model(Some("claude-3-opus"));
        assert_eq!(b.model_limit, 200_000);
        assert!((b.output_reserve_ratio - 0.20).abs() < f64::EPSILON);
    }

    #[test]
    fn model_unknown_defaults() {
        let b = budget_for_model(None);
        assert_eq!(b.model_limit, 128_000);
        assert!((b.output_reserve_ratio - 0.15).abs() < f64::EPSILON);
    }

    #[test]
    fn model_qwen() {
        let b = budget_for_model(Some("qwen-turbo"));
        assert_eq!(b.model_limit, 128_000);
    }

    // ---------------------------------------------------------------
    // 5. Backward compatibility
    // ---------------------------------------------------------------

    #[test]
    fn compact_trigger_uses_effective_limit() {
        let b = ContextBudget::default();
        // effective = 108_800, compact_trigger = 108_800 * 0.75 = 81_600
        assert_eq!(b.compact_trigger(), 81_600);
    }

    #[test]
    fn should_compact_backward_compat() {
        let b = ContextBudget::default();
        let trigger = b.compact_trigger();
        assert!(!b.should_compact(trigger));
        assert!(b.should_compact(trigger + 1));
    }

    #[test]
    fn default_budget_fields_unchanged() {
        let b = ContextBudget::default();
        assert_eq!(b.model_limit, 128_000);
        assert!((b.compact_threshold - 0.75).abs() < f64::EPSILON);
        assert_eq!(b.keep_recent_turns, 4);
        assert_eq!(b.memory_budget_chars, 8_000);
    }

    #[test]
    fn estimate_tokens_unchanged_behavior() {
        let messages = vec![msg(&"x".repeat(400))];
        // 400/4 = 100 + 4 overhead + 3000 fixed = 3104
        assert_eq!(estimate_tokens(&messages), 3104);
    }

    #[test]
    fn estimate_tokens_with_tool_calls_unchanged() {
        let messages = vec![tool_msg(&"y".repeat(200))];
        // 200/4 = 50 + 4 overhead + 3000 fixed = 3054
        assert_eq!(estimate_tokens(&messages), 3054);
    }

    #[test]
    fn estimate_tokens_empty() {
        assert_eq!(estimate_tokens(&[]), 3000);
    }

    // ---------------------------------------------------------------
    // 6. CJK-aware token estimation
    // ---------------------------------------------------------------

    #[test]
    fn estimate_str_tokens_pure_ascii() {
        // 20 ASCII chars = 20 bytes / 4 = 5 tokens
        assert_eq!(estimate_str_tokens("hello world testing!"), 5);
    }

    #[test]
    fn estimate_str_tokens_pure_cjk() {
        // 4 CJK chars × 1.5 = 6 tokens (BPE rate)
        assert_eq!(estimate_str_tokens("你好世界"), 6);
    }

    #[test]
    fn estimate_str_tokens_mixed_en_cn() {
        // "我关注matrixorigin" → 3 CJK × 1.5 + "matrixorigin" 12 ASCII / 4
        // = 5 + 3 = 8
        assert_eq!(estimate_str_tokens("我关注matrixorigin"), 8);
    }

    #[test]
    fn estimate_str_tokens_cjk_more_than_old_heuristic() {
        // Old: "你好世界".len() = 12 bytes / 4 = 3 tokens (WRONG)
        // New: 4 CJK chars × 1.5 = 6 tokens (BPE-accurate)
        let old_estimate = "你好世界".len() / 4;
        let new_estimate = estimate_str_tokens("你好世界");
        assert!(
            new_estimate > old_estimate,
            "CJK estimation should be higher than bytes/4"
        );
    }

    #[test]
    fn estimate_str_tokens_cjk_punctuation() {
        // CJK punctuation (U+3000-303F, FF00-FFEF) counts as CJK tokens
        // "。" is U+3002, "！" is U+FF01
        // 3 CJK chars × 1.5 = 5 tokens (rounded up)
        assert_eq!(estimate_str_tokens("你好。"), 5);
    }

    #[test]
    fn estimate_str_tokens_empty() {
        assert_eq!(estimate_str_tokens(""), 0);
    }

    #[test]
    fn estimate_tokens_cjk_message() {
        // CJK message: "分析一下这个文件" = 8 CJK chars × 1.5 = 12 tokens + 4 overhead + 3000 fixed
        let messages = vec![msg("分析一下这个文件")];
        assert_eq!(estimate_tokens(&messages), 12 + 4 + 3000);
    }

    // ── Phase 6.1: Mixed EN/CN regression tests ──

    #[test]
    fn estimate_str_tokens_mixed_sentence() {
        // "Hello 你好世界 World" — 4 CJK chars × 1.5 = 6, plus ASCII tokens
        let t = estimate_str_tokens("Hello 你好世界 World");
        assert!(
            (6..=12).contains(&t),
            "mixed EN/CN should be 6-12 tokens, got {}",
            t
        );
    }

    #[test]
    fn estimate_str_tokens_pure_ascii_regression() {
        // "The quick brown fox jumps over the lazy dog" → ~9 words → ~9/0.75 = 12
        let t = estimate_str_tokens("The quick brown fox jumps over the lazy dog");
        assert!(
            (8..=15).contains(&t),
            "ASCII sentence should be 8-15 tokens, got {}",
            t
        );
    }

    #[test]
    fn estimate_str_tokens_pure_cjk_regression() {
        // 12 CJK chars × 1.5 = 18 tokens (BPE-accurate)
        let t = estimate_str_tokens("这是一个长句子测试效果好");
        assert_eq!(t, 18, "pure CJK 12 chars × 1.5 = 18 tokens");
    }

    #[test]
    fn estimate_str_tokens_code_mixed() {
        // Code with comments in Chinese
        let t = estimate_str_tokens("fn main() { // 主函数入口 }");
        assert!(t > 0, "code + CJK comment should estimate > 0");
    }

    // ── Phase 6.2: Token budget exhaustion boundary ──

    #[test]
    fn compaction_tier_at_warning_threshold() {
        let ctx = budget_for_model(Some("gpt-4o"));
        let limit = ctx.effective_input_limit();
        // 61% → TrimSchemas tier
        let tokens_61 = (limit as f64 * 0.61) as usize;
        assert_eq!(ctx.compaction_tier(tokens_61), CompactionTier::TrimSchemas);
    }

    #[test]
    fn compaction_tier_at_critical_threshold() {
        let ctx = budget_for_model(Some("gpt-4o"));
        let limit = ctx.effective_input_limit();
        // 86% → AggressivePrune
        let tokens_86 = (limit as f64 * 0.86) as usize;
        assert_eq!(
            ctx.compaction_tier(tokens_86),
            CompactionTier::AggressivePrune
        );
    }

    #[test]
    fn compaction_tier_at_normal() {
        let ctx = budget_for_model(Some("gpt-4o"));
        let limit = ctx.effective_input_limit();
        // 50% → Normal (no compaction)
        let tokens_50 = (limit as f64 * 0.50) as usize;
        assert_eq!(ctx.compaction_tier(tokens_50), CompactionTier::Normal);
    }

    #[test]
    fn compaction_tier_at_compact_history() {
        let ctx = budget_for_model(Some("gpt-4o"));
        let limit = ctx.effective_input_limit();
        // 76% → CompactHistory
        let tokens_76 = (limit as f64 * 0.76) as usize;
        assert_eq!(
            ctx.compaction_tier(tokens_76),
            CompactionTier::CompactHistory
        );
    }

    #[test]
    fn effective_input_limit_less_than_model_limit() {
        let ctx = budget_for_model(Some("gpt-4o"));
        assert!(
            ctx.effective_input_limit() < ctx.model_limit,
            "input limit should be less than model limit due to output reserve"
        );
    }
}
