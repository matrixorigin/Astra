//! Canonical token usage accounting across LLM providers.
//!
//! Every provider reports token usage differently. This module normalizes them
//! into a single [`TokenUsage`] struct whose invariants are provider-independent.
//!
//! # Semantics
//!
//! Billable input tokens for a single LLM call are partitioned into three
//! disjoint buckets:
//!
//! - `input_tokens`         — fresh input, billed at full input rate
//! - `cached_input_tokens`  — served from prompt cache, billed at a discount
//! - `cache_creation_tokens`— written to prompt cache, billed at a premium
//!
//! Plus `output_tokens`. These four numbers are disjoint and sum to `total_tokens`.
//!
//! # Per-provider quirks (all normalized here)
//!
//! - **OpenAI-compatible**: `usage.prompt_tokens` INCLUDES cached tokens; we
//!   subtract `prompt_tokens_details.cached_tokens` so `input_tokens` reflects
//!   only fresh input. Cache creation is rarely surfaced; when present as
//!   `prompt_tokens_details.cache_creation_input_tokens` we subtract too.
//! - **Bedrock Converse**: `usage.inputTokens` EXCLUDES both
//!   `cacheReadInputTokens` and `cacheWriteInputTokens`. Use values directly.
//! - **Anthropic Messages**: `usage.input_tokens` EXCLUDES both
//!   `cache_read_input_tokens` and `cache_creation_input_tokens`. Use values
//!   directly.

use serde_json::{Map, Value};

/// Normalized per-call token usage. All fields are disjoint buckets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_tokens: u64,
    pub output_tokens: u64,
}

impl TokenUsage {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cached_input_tokens)
            .saturating_add(self.cache_creation_tokens)
            .saturating_add(self.output_tokens)
    }

    pub fn is_empty(&self) -> bool {
        self.input_tokens == 0
            && self.cached_input_tokens == 0
            && self.cache_creation_tokens == 0
            && self.output_tokens == 0
    }

    /// Serialize to the canonical JSON shape used across the codebase.
    pub fn to_json_map(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("input_tokens".into(), Value::from(self.input_tokens));
        m.insert(
            "cached_input_tokens".into(),
            Value::from(self.cached_input_tokens),
        );
        m.insert(
            "cache_creation_tokens".into(),
            Value::from(self.cache_creation_tokens),
        );
        m.insert("output_tokens".into(), Value::from(self.output_tokens));
        m.insert("total_tokens".into(), Value::from(self.total_tokens()));
        m
    }

    /// Parse back from our canonical shape. Missing fields default to 0.
    pub fn from_json_map(m: &Map<String, Value>) -> Self {
        let read = |k: &str| -> u64 {
            m.get(k)
                .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|i| i.max(0) as u64)))
                .unwrap_or(0)
        };
        Self {
            input_tokens: read("input_tokens"),
            cached_input_tokens: read("cached_input_tokens"),
            cache_creation_tokens: read("cache_creation_tokens"),
            output_tokens: read("output_tokens"),
        }
    }
}

/// Protocol dialect for a provider. Determines which extractor reads usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageDialect {
    /// OpenAI-compatible: `prompt_tokens`, `completion_tokens`,
    /// `prompt_tokens_details.cached_tokens`. `prompt_tokens` INCLUDES cache.
    OpenAi,
    /// Bedrock Converse: `inputTokens`, `outputTokens`, `cacheReadInputTokens`,
    /// `cacheWriteInputTokens`. `inputTokens` EXCLUDES cache.
    BedrockConverse,
    /// Anthropic Messages: `input_tokens`, `output_tokens`,
    /// `cache_read_input_tokens`, `cache_creation_input_tokens`.
    /// `input_tokens` EXCLUDES cache read/write buckets.
    AnthropicMessages,
}

impl UsageDialect {
    pub fn for_provider(provider: &str) -> Self {
        match provider {
            "bedrock" => Self::BedrockConverse,
            "anthropic" => Self::AnthropicMessages,
            _ => Self::OpenAi,
        }
    }
}

/// Extract a [`TokenUsage`] from the raw `usage` JSON object returned by the
/// provider (either a non-streaming response or a streaming chunk).
///
/// Accepts the object directly (caller has already navigated to `v["usage"]`).
/// Returns `None` only when no recognized tokens field is present at all.
pub fn extract_usage(dialect: UsageDialect, usage_obj: &Map<String, Value>) -> Option<TokenUsage> {
    match dialect {
        UsageDialect::OpenAi => extract_openai(usage_obj),
        UsageDialect::BedrockConverse => extract_bedrock(usage_obj),
        UsageDialect::AnthropicMessages => extract_anthropic(usage_obj),
    }
}

fn as_u64(v: Option<&Value>) -> Option<u64> {
    v.and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|i| i.max(0) as u64)))
}

fn extract_openai(u: &Map<String, Value>) -> Option<TokenUsage> {
    // Required: prompt_tokens + completion_tokens (either one is enough to
    // consider this a usage object).
    let prompt_total = as_u64(u.get("prompt_tokens"));
    let completion = as_u64(u.get("completion_tokens")).unwrap_or(0);
    if prompt_total.is_none() && completion == 0 {
        return None;
    }
    let prompt_total = prompt_total.unwrap_or(0);

    let details = u.get("prompt_tokens_details").and_then(Value::as_object);
    // Two accepted shapes, disambiguated by **field location**:
    //
    // - **Inclusive** (OpenAI native, MiniMax, DashScope): cache tokens are
    //   nested under `prompt_tokens_details`. `prompt_tokens` INCLUDES them;
    //   fresh = prompt - cached - creation.
    // - **Disjoint** (Anthropic-native proxied as OpenAI): cache tokens are
    //   at the TOP LEVEL (`cache_read_input_tokens`, `cache_creation_input_tokens`)
    //   with no nested details. `prompt_tokens` IS fresh; cache is separate.
    //
    // Field location is deterministic and survives the `cached + creation ==
    // prompt_tokens` boundary, which pure arithmetic cannot. When both
    // signals coexist (a provider that echoes aliases), prefer inclusive —
    // that matches the authoritative OpenAI shape.
    let nested_cached = details.and_then(|d| as_u64(d.get("cached_tokens")));
    let nested_creation = details.and_then(|d| as_u64(d.get("cache_creation_input_tokens")));
    let top_cached = as_u64(u.get("cache_read_input_tokens"));
    let top_creation = as_u64(u.get("cache_creation_input_tokens"));

    let is_inclusive = nested_cached.is_some() || nested_creation.is_some();
    let cached = nested_cached.or(top_cached).unwrap_or(0);
    let cache_creation = nested_creation.or(top_creation).unwrap_or(0);

    let fresh = if is_inclusive {
        // Inclusive: fresh = prompt - cached - creation, saturating for safety
        // against providers that violate the contract.
        prompt_total
            .saturating_sub(cached)
            .saturating_sub(cache_creation)
    } else {
        // Disjoint: prompt_tokens is already fresh.
        prompt_total
    };

    Some(TokenUsage {
        input_tokens: fresh,
        cached_input_tokens: cached,
        cache_creation_tokens: cache_creation,
        output_tokens: completion,
    })
}

fn extract_bedrock(u: &Map<String, Value>) -> Option<TokenUsage> {
    let input = as_u64(u.get("inputTokens"));
    let output = as_u64(u.get("outputTokens"));
    if input.is_none() && output.is_none() {
        return None;
    }
    // Bedrock Converse: inputTokens is DISJOINT from cache{Read,Write}InputTokens.
    // https://docs.aws.amazon.com/bedrock/latest/userguide/prompt-caching.html
    let cached = as_u64(u.get("cacheReadInputTokens")).unwrap_or(0);
    let cache_creation = as_u64(u.get("cacheWriteInputTokens")).unwrap_or(0);

    Some(TokenUsage {
        input_tokens: input.unwrap_or(0),
        cached_input_tokens: cached,
        cache_creation_tokens: cache_creation,
        output_tokens: output.unwrap_or(0),
    })
}

fn extract_anthropic(u: &Map<String, Value>) -> Option<TokenUsage> {
    let input = as_u64(u.get("input_tokens"));
    let output = as_u64(u.get("output_tokens"));
    let cached = as_u64(u.get("cache_read_input_tokens")).unwrap_or(0);
    let cache_creation = as_u64(u.get("cache_creation_input_tokens")).unwrap_or(0);
    if input.is_none() && output.is_none() && cached == 0 && cache_creation == 0 {
        return None;
    }
    Some(TokenUsage {
        input_tokens: input.unwrap_or(0),
        cached_input_tokens: cached,
        cache_creation_tokens: cache_creation,
        output_tokens: output.unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().expect("expected object")
    }

    // ── TokenUsage invariants ──────────────────────────────────────────────

    #[test]
    fn total_tokens_sums_all_buckets() {
        let u = TokenUsage {
            input_tokens: 100,
            cached_input_tokens: 40,
            cache_creation_tokens: 10,
            output_tokens: 50,
        };
        assert_eq!(u.total_tokens(), 200);
    }

    #[test]
    fn is_empty_true_for_default() {
        assert!(TokenUsage::default().is_empty());
    }

    #[test]
    fn roundtrip_through_json_map() {
        let original = TokenUsage {
            input_tokens: 123,
            cached_input_tokens: 45,
            cache_creation_tokens: 6,
            output_tokens: 789,
        };
        let m = original.to_json_map();
        assert_eq!(m["input_tokens"], json!(123));
        assert_eq!(m["cached_input_tokens"], json!(45));
        assert_eq!(m["cache_creation_tokens"], json!(6));
        assert_eq!(m["output_tokens"], json!(789));
        assert_eq!(m["total_tokens"], json!(963));
        let back = TokenUsage::from_json_map(&m);
        assert_eq!(back, original);
    }

    // ── Dialect routing ────────────────────────────────────────────────────

    #[test]
    fn dialect_bedrock_for_bedrock_provider() {
        assert_eq!(
            UsageDialect::for_provider("bedrock"),
            UsageDialect::BedrockConverse
        );
    }

    #[test]
    fn dialect_anthropic_for_anthropic_provider() {
        assert_eq!(
            UsageDialect::for_provider("anthropic"),
            UsageDialect::AnthropicMessages
        );
    }

    #[test]
    fn dialect_openai_for_unknown_provider() {
        assert_eq!(UsageDialect::for_provider("glm"), UsageDialect::OpenAi);
        assert_eq!(UsageDialect::for_provider("openai"), UsageDialect::OpenAi);
        assert_eq!(UsageDialect::for_provider("qwen"), UsageDialect::OpenAi);
    }

    // ── OpenAI extractor ───────────────────────────────────────────────────

    #[test]
    fn openai_plain_without_cache() {
        let u = obj(json!({"prompt_tokens": 100, "completion_tokens": 50}));
        let t = extract_usage(UsageDialect::OpenAi, &u).unwrap();
        assert_eq!(t.input_tokens, 100);
        assert_eq!(t.cached_input_tokens, 0);
        assert_eq!(t.cache_creation_tokens, 0);
        assert_eq!(t.output_tokens, 50);
        assert_eq!(t.total_tokens(), 150);
    }

    #[test]
    fn openai_with_cached_tokens_deducts_from_prompt() {
        // OpenAI prompt_tokens INCLUDES cached tokens — the 900 fresh + 100
        // cached split must come out after parsing.
        let u = obj(json!({
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "prompt_tokens_details": {"cached_tokens": 100}
        }));
        let t = extract_usage(UsageDialect::OpenAi, &u).unwrap();
        assert_eq!(t.input_tokens, 900);
        assert_eq!(t.cached_input_tokens, 100);
        assert_eq!(t.cache_creation_tokens, 0);
        assert_eq!(t.output_tokens, 50);
        // Billing identity: fresh + cached + creation = original prompt_tokens
        assert_eq!(
            t.input_tokens + t.cached_input_tokens + t.cache_creation_tokens,
            1000
        );
    }

    #[test]
    fn openai_with_cache_creation_also_deducts() {
        let u = obj(json!({
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "prompt_tokens_details": {
                "cached_tokens": 100,
                "cache_creation_input_tokens": 50
            }
        }));
        let t = extract_usage(UsageDialect::OpenAi, &u).unwrap();
        assert_eq!(t.input_tokens, 850);
        assert_eq!(t.cached_input_tokens, 100);
        assert_eq!(t.cache_creation_tokens, 50);
        assert_eq!(t.total_tokens(), 1050); // input + cached + creation + output
    }

    #[test]
    fn openai_top_level_cache_creation_is_honored() {
        // Some proxies (Anthropic-on-OpenAI-compatible) surface the cache
        // creation at the top level of the usage object.
        let u = obj(json!({
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "prompt_tokens_details": {"cached_tokens": 100},
            "cache_creation_input_tokens": 50
        }));
        let t = extract_usage(UsageDialect::OpenAi, &u).unwrap();
        assert_eq!(t.input_tokens, 850);
        assert_eq!(t.cached_input_tokens, 100);
        assert_eq!(t.cache_creation_tokens, 50);
    }

    #[test]
    fn openai_empty_usage_returns_none() {
        let u = obj(json!({}));
        assert!(extract_usage(UsageDialect::OpenAi, &u).is_none());
    }

    #[test]
    fn openai_inclusive_contract_violation_saturates_instead_of_wrapping() {
        // A provider that violates the inclusive contract (nested
        // `cached_tokens` > `prompt_tokens`) is still unambiguously inclusive
        // by shape (nested details present). Saturating subtraction keeps
        // `fresh = 0` rather than wrapping to a huge u64. This is a
        // defensive guard, not a semantic pivot — the cached count is
        // preserved verbatim so cost accounting still sees the spend.
        let u = obj(json!({
            "prompt_tokens": 50,
            "completion_tokens": 10,
            "prompt_tokens_details": {"cached_tokens": 9999}
        }));
        let t = extract_usage(UsageDialect::OpenAi, &u).unwrap();
        assert_eq!(t.input_tokens, 0);
        assert_eq!(t.cached_input_tokens, 9999);
    }

    // ── Anthropic-native disjoint shape (proxied as OpenAI-compatible) ─────

    /// Some Anthropic-behind-OpenAI proxies pass the native Anthropic shape
    /// through: `prompt_tokens` is already the FRESH input (disjoint from
    /// `cache_read_input_tokens` + `cache_creation_input_tokens`). The
    /// extractor must NOT subtract cache counts in that case — doing so
    /// would under-report fresh input and violate the billing identity.
    /// Boundary case: `cached + creation == prompt_total` exactly.
    /// With field-presence disambiguation, nested `prompt_tokens_details`
    /// forces inclusive semantics even at the tie.
    #[test]
    fn openai_inclusive_at_boundary_prefers_nested_signal() {
        let u = obj(json!({
            "prompt_tokens": 100,
            "completion_tokens": 10,
            "prompt_tokens_details": {"cached_tokens": 100}
        }));
        let t = extract_usage(UsageDialect::OpenAi, &u).unwrap();
        assert_eq!(t.input_tokens, 0, "inclusive: fresh = 100 - 100 = 0");
        assert_eq!(t.cached_input_tokens, 100);
    }

    /// Boundary case: the SAME arithmetic tie but top-level
    /// `cache_read_input_tokens` (no nested details) signals disjoint.
    /// prompt_tokens is already the fresh count; cache counts are separate.
    #[test]
    fn openai_disjoint_at_boundary_prefers_top_level_signal() {
        let u = obj(json!({
            "prompt_tokens": 100,
            "completion_tokens": 10,
            "cache_read_input_tokens": 100
        }));
        let t = extract_usage(UsageDialect::OpenAi, &u).unwrap();
        assert_eq!(t.input_tokens, 100, "disjoint: prompt IS fresh");
        assert_eq!(t.cached_input_tokens, 100);
        assert_eq!(t.total_tokens(), 210);
    }

    /// When both shapes' markers coexist (cached in nested AND top-level),
    /// prefer the nested-inclusive interpretation — that's how OpenAI-native
    /// clients that also echo Anthropic-style aliases behave.
    #[test]
    fn openai_both_signals_present_prefers_inclusive() {
        let u = obj(json!({
            "prompt_tokens": 1000,
            "completion_tokens": 20,
            "prompt_tokens_details": {"cached_tokens": 300},
            "cache_read_input_tokens": 300
        }));
        let t = extract_usage(UsageDialect::OpenAi, &u).unwrap();
        assert_eq!(t.input_tokens, 700);
        assert_eq!(t.cached_input_tokens, 300);
    }

    #[test]
    fn openai_disjoint_shape_keeps_prompt_as_fresh() {
        let u = obj(json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "cache_read_input_tokens": 200,
            "cache_creation_input_tokens": 50
        }));
        let t = extract_usage(UsageDialect::OpenAi, &u).unwrap();
        // prompt_tokens(100) < cached(200)+creation(50)=250 → disjoint shape.
        // Fresh input = prompt_tokens verbatim.
        assert_eq!(t.input_tokens, 100);
        assert_eq!(t.cached_input_tokens, 200);
        assert_eq!(t.cache_creation_tokens, 50);
        assert_eq!(t.output_tokens, 20);
        assert_eq!(t.total_tokens(), 370);
    }

    #[test]
    fn openai_inclusive_shape_still_subtracts() {
        // Sanity: inclusive shape (prompt ⊇ cached + creation) still works.
        let u = obj(json!({
            "prompt_tokens": 500,
            "completion_tokens": 20,
            "prompt_tokens_details": {"cached_tokens": 200},
            "cache_creation_input_tokens": 50
        }));
        let t = extract_usage(UsageDialect::OpenAi, &u).unwrap();
        assert_eq!(t.input_tokens, 250);
        assert_eq!(t.cached_input_tokens, 200);
        assert_eq!(t.cache_creation_tokens, 50);
        assert_eq!(t.output_tokens, 20);
    }

    // ── Bedrock extractor ──────────────────────────────────────────────────

    #[test]
    fn bedrock_plain_without_cache() {
        let u = obj(json!({"inputTokens": 100, "outputTokens": 50}));
        let t = extract_usage(UsageDialect::BedrockConverse, &u).unwrap();
        assert_eq!(t.input_tokens, 100);
        assert_eq!(t.cached_input_tokens, 0);
        assert_eq!(t.cache_creation_tokens, 0);
        assert_eq!(t.output_tokens, 50);
        assert_eq!(t.total_tokens(), 150);
    }

    #[test]
    fn bedrock_input_tokens_excludes_cache_read() {
        // Bedrock contract: inputTokens is DISJOINT from cacheReadInputTokens.
        // A correct total is input + cacheRead + cacheWrite + output.
        let u = obj(json!({
            "inputTokens": 200,
            "outputTokens": 50,
            "cacheReadInputTokens": 800,
            "cacheWriteInputTokens": 100
        }));
        let t = extract_usage(UsageDialect::BedrockConverse, &u).unwrap();
        assert_eq!(t.input_tokens, 200);
        assert_eq!(t.cached_input_tokens, 800);
        assert_eq!(t.cache_creation_tokens, 100);
        assert_eq!(t.output_tokens, 50);
        assert_eq!(t.total_tokens(), 1150);
    }

    #[test]
    fn bedrock_cache_only_without_totals() {
        // Model returned cache but no totalTokens — our total must still be
        // correct from disjoint parts.
        let u = obj(json!({
            "inputTokens": 0,
            "outputTokens": 200,
            "cacheReadInputTokens": 5000
        }));
        let t = extract_usage(UsageDialect::BedrockConverse, &u).unwrap();
        assert_eq!(t.total_tokens(), 5200);
    }

    #[test]
    fn bedrock_empty_usage_returns_none() {
        let u = obj(json!({}));
        assert!(extract_usage(UsageDialect::BedrockConverse, &u).is_none());
    }

    // ── Anthropic Messages extractor ───────────────────────────────────────

    #[test]
    fn anthropic_messages_usage_is_disjoint() {
        let u = obj(json!({
            "input_tokens": 200,
            "output_tokens": 50,
            "cache_read_input_tokens": 800,
            "cache_creation_input_tokens": 100
        }));
        let t = extract_usage(UsageDialect::AnthropicMessages, &u).unwrap();
        assert_eq!(t.input_tokens, 200);
        assert_eq!(t.cached_input_tokens, 800);
        assert_eq!(t.cache_creation_tokens, 100);
        assert_eq!(t.output_tokens, 50);
        assert_eq!(t.total_tokens(), 1150);
    }

    #[test]
    fn anthropic_messages_empty_usage_returns_none() {
        let u = obj(json!({}));
        assert!(extract_usage(UsageDialect::AnthropicMessages, &u).is_none());
    }

    // ── Canonical JSON shape used in SSE events ────────────────────────────

    #[test]
    fn json_map_uses_canonical_keys_only() {
        let t = TokenUsage {
            input_tokens: 1,
            cached_input_tokens: 2,
            cache_creation_tokens: 3,
            output_tokens: 4,
        };
        let m = t.to_json_map();
        let keys: Vec<&String> = m.keys().collect();
        // Must not leak legacy names like prompt/completion/cache_read.
        assert!(keys.iter().all(|k| matches!(
            k.as_str(),
            "input_tokens"
                | "cached_input_tokens"
                | "cache_creation_tokens"
                | "output_tokens"
                | "total_tokens"
        )));
    }
}
