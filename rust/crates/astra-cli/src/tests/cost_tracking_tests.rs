use super::*;

// ── Cost Tracking Tests ──────────────────────────────────────────────

#[test]
fn cost_for_tokens_basic() {
    let pricing = astra_services::models::PricingData {
        prompt: 0.003,     // $0.003 per 1K tokens = $3 per 1M
        completion: 0.015, // $0.015 per 1K tokens = $15 per 1M
        cache_read: None,
        cache_write: None,
    };
    let cost = slash_stats::cost_for_tokens(1000, 500, 0, 0, &pricing);
    // 1000 * 0.003/1000 + 500 * 0.015/1000 = 0.003 + 0.0075 = 0.0105
    assert!(
        (cost - 0.0105).abs() < 1e-10,
        "cost should be $0.0105, got {cost}"
    );
}

#[test]
fn cost_for_tokens_zero() {
    let pricing = astra_services::models::PricingData {
        prompt: 0.003,
        completion: 0.015,
        cache_read: None,
        cache_write: None,
    };
    assert_eq!(slash_stats::cost_for_tokens(0, 0, 0, 0, &pricing), 0.0);
}

#[test]
fn cost_for_tokens_zero_pricing() {
    let pricing = astra_services::models::PricingData::default();
    assert_eq!(
        slash_stats::cost_for_tokens(10000, 5000, 0, 0, &pricing),
        0.0
    );
}

#[test]
fn cost_for_tokens_large_values() {
    let pricing = astra_services::models::PricingData {
        prompt: 0.003,
        completion: 0.015,
        cache_read: None,
        cache_write: None,
    };
    // 1M prompt + 500K completion
    let cost = slash_stats::cost_for_tokens(1_000_000, 500_000, 0, 0, &pricing);
    // 1M * 0.003/1K + 500K * 0.015/1K = 3.0 + 7.5 = 10.5
    assert!(
        (cost - 10.5).abs() < 1e-6,
        "large token cost should be $10.50, got {cost}"
    );
}

#[test]
fn cost_for_tokens_with_cache() {
    let pricing = astra_services::models::PricingData {
        prompt: 0.003,
        completion: 0.015,
        cache_read: Some(0.0003),   // 10% of prompt
        cache_write: Some(0.00375), // 125% of prompt
    };
    // 500 prompt + 200 completion + 1000 cache_read + 100 cache_write
    let cost = slash_stats::cost_for_tokens(500, 200, 1000, 100, &pricing);
    let expected = (500.0 * 0.003 / 1000.0)
        + (200.0 * 0.015 / 1000.0)
        + (1000.0 * 0.0003 / 1000.0)
        + (100.0 * 0.00375 / 1000.0);
    assert!(
        (cost - expected).abs() < 1e-10,
        "cache cost should be {expected}, got {cost}"
    );
}

#[test]
fn cost_for_tokens_cache_fallback_rates() {
    // When cache_read/cache_write are None, uses 10%/125% of prompt rate
    let pricing = astra_services::models::PricingData {
        prompt: 0.003,
        completion: 0.015,
        cache_read: None,
        cache_write: None,
    };
    let cost = slash_stats::cost_for_tokens(0, 0, 1000, 1000, &pricing);
    let expected = (1000.0 * 0.003 * 0.1 / 1000.0) + (1000.0 * 0.003 * 1.25 / 1000.0);
    assert!(
        (cost - expected).abs() < 1e-10,
        "fallback cache cost should be {expected}, got {cost}"
    );
}

#[test]
fn format_cost_sub_cent() {
    assert_eq!(slash_stats::format_cost(0.0001), "$0.0001");
    assert_eq!(slash_stats::format_cost(0.0099), "$0.0099");
}

#[test]
fn format_cost_sub_dollar() {
    assert_eq!(slash_stats::format_cost(0.01), "$0.010");
    assert_eq!(slash_stats::format_cost(0.123), "$0.123");
    assert_eq!(slash_stats::format_cost(0.999), "$0.999");
}

#[test]
fn format_cost_dollars() {
    assert_eq!(slash_stats::format_cost(1.0), "$1.00");
    assert_eq!(slash_stats::format_cost(12.345), "$12.35"); // rounds
    assert_eq!(slash_stats::format_cost(100.0), "$100.00");
}

#[test]
fn format_cost_zero() {
    assert_eq!(slash_stats::format_cost(0.0), "$0.0000");
}

#[test]
fn extract_pricing_from_nested_object() {
    let models = vec![serde_json::json!({
        "name": "gpt-4",
        "pricing": {
            "prompt": 0.03,
            "completion": 0.06
        }
    })];
    let p = slash_stats::extract_pricing_for_model(&models, "gpt-4").unwrap();
    assert!((p.prompt - 0.03).abs() < 1e-10);
    assert!((p.completion - 0.06).abs() < 1e-10);
}

#[test]
fn extract_pricing_from_flat_fields() {
    let models = vec![serde_json::json!({
        "name": "claude-3",
        "pricing_prompt": 0.008,
        "pricing_completion": 0.024
    })];
    let p = slash_stats::extract_pricing_for_model(&models, "claude-3").unwrap();
    assert!((p.prompt - 0.008).abs() < 1e-10);
    assert!((p.completion - 0.024).abs() < 1e-10);
}

#[test]
fn extract_pricing_model_not_found() {
    let models = vec![
        serde_json::json!({"name": "gpt-4", "pricing_prompt": 0.03, "pricing_completion": 0.06}),
    ];
    assert!(slash_stats::extract_pricing_for_model(&models, "nonexistent").is_none());
}

#[test]
fn extract_pricing_empty_models() {
    let models: Vec<serde_json::Value> = vec![];
    assert!(slash_stats::extract_pricing_for_model(&models, "any").is_none());
}

#[test]
fn extract_pricing_zero_values_returns_none() {
    let models = vec![serde_json::json!({
        "name": "test",
        "pricing_prompt": 0.0,
        "pricing_completion": 0.0
    })];
    assert!(slash_stats::extract_pricing_for_model(&models, "test").is_none());
}

// ── slash_stats::fallback_pricing tests ───────────────────────────────────────────

#[test]
fn fallback_sonnet_pricing() {
    let p = slash_stats::fallback_pricing("claude-sonnet-4-20250514");
    assert!((p.prompt - 0.003).abs() < 1e-6);
    assert!((p.completion - 0.015).abs() < 1e-6);
    assert!(p.cache_read.is_some());
    assert!((p.cache_read.unwrap() - 0.0003).abs() < 1e-8);
}

#[test]
fn fallback_opus_4_pricing() {
    let p = slash_stats::fallback_pricing("claude-opus-4-20250514");
    assert!(
        (p.prompt - 0.015).abs() < 1e-6,
        "opus-4 prompt should be $15/Mtok"
    );
    assert!((p.completion - 0.075).abs() < 1e-6);
}

#[test]
fn fallback_opus_45_pricing() {
    let p = slash_stats::fallback_pricing("claude-opus-4.5-20250415");
    assert!(
        (p.prompt - 0.005).abs() < 1e-6,
        "opus 4.5 should be $5/Mtok"
    );
}

#[test]
fn fallback_haiku_pricing() {
    let p = slash_stats::fallback_pricing("claude-haiku-4.5-20250514");
    assert!(
        (p.prompt - 0.001).abs() < 1e-6,
        "haiku 4.5 should be $1/Mtok"
    );
}

#[test]
fn fallback_gpt4o_pricing() {
    let p = slash_stats::fallback_pricing("gpt-4o-2024-08-06");
    assert!((p.prompt - 0.0025).abs() < 1e-6);
}

#[test]
fn fallback_deepseek_pricing() {
    let p = slash_stats::fallback_pricing("deepseek-chat");
    assert!((p.prompt - 0.00027).abs() < 1e-8);
}

#[test]
fn fallback_unknown_uses_sonnet() {
    let p = slash_stats::fallback_pricing("some-unknown-model");
    assert!(
        (p.prompt - 0.003).abs() < 1e-6,
        "unknown model should default to sonnet pricing"
    );
}

#[test]
fn fallback_cost_calculation_with_cache() {
    // Sonnet: 1000 prompt + 500 completion + 2000 cache_read + 100 cache_creation
    let p = slash_stats::fallback_pricing("claude-sonnet-4-20250514");
    let cost = slash_stats::cost_for_tokens(1000, 500, 2000, 100, &p);
    // $0.003/Ktok * 1 + $0.015/Ktok * 0.5 + $0.0003/Ktok * 2 + $0.00375/Ktok * 0.1
    let expected = 0.003 + 0.0075 + 0.0006 + 0.000375;
    assert!(
        (cost - expected).abs() < 1e-8,
        "cost={cost} expected={expected}"
    );
}

// ── family-specific fallback pricing (non-Anthropic) ─────────────────────────

#[test]
fn fallback_qwen_has_no_cache_write_premium() {
    let p = slash_stats::fallback_pricing("qwen-plus");
    assert!(p.cache_read.is_some(), "qwen should define cache_read");
    assert_eq!(
        p.cache_write, None,
        "qwen has no cache_write concept — must not default to Anthropic's 125% premium"
    );
}

#[test]
fn fallback_minimax_has_no_cache_write_premium() {
    let p = slash_stats::fallback_pricing("MiniMax-M2.5");
    assert_eq!(p.cache_write, None);
}

#[test]
fn fallback_glm_has_no_cache_write_premium() {
    let p = slash_stats::fallback_pricing("glm-5.1");
    assert_eq!(p.cache_write, None);
}

// ── extract_pricing_for_model inherits missing cache rates from family ───────

#[test]
fn extract_pricing_inherits_cache_read_from_family_when_missing() {
    // Qwen model with only base prompt/completion prices — the extractor
    // should inherit the qwen family's cache_read rate and leave cache_write
    // None (no write premium on DashScope).
    let models = vec![serde_json::json!({
        "name": "qwen-plus",
        "pricing_prompt": 0.0008,
        "pricing_completion": 0.002,
    })];
    let p = slash_stats::extract_pricing_for_model(&models, "qwen-plus").unwrap();
    assert_eq!(
        p.cache_write, None,
        "no Anthropic-style write premium for qwen"
    );
    assert!(
        p.cache_read.is_some(),
        "cache_read inherited from fallback_pricing"
    );
}

#[test]
fn extract_pricing_preserves_explicit_cache_rates() {
    let models = vec![serde_json::json!({
        "name": "claude-sonnet",
        "pricing_prompt": 0.003,
        "pricing_completion": 0.015,
        "pricing_cache_read": 0.0003,
        "pricing_cache_write": 0.00375,
    })];
    let p = slash_stats::extract_pricing_for_model(&models, "claude-sonnet").unwrap();
    assert!((p.cache_read.unwrap() - 0.0003).abs() < 1e-10);
    assert!((p.cache_write.unwrap() - 0.00375).abs() < 1e-10);
}

#[test]
fn bedrock_claude_sonnet_cost_uses_family_cache_rates() {
    // Regression guard: a Bedrock Claude Sonnet model whose yaml only has
    // prompt/completion (no explicit cache_read / cache_write) should STILL
    // compute cost with the 10%/125% Anthropic cache multipliers via the
    // family fallback — not the raw 10%/125% literals of an arbitrary
    // prompt value.
    let models = vec![serde_json::json!({
        "name": "us.anthropic.claude-sonnet-4-6",
        "pricing_prompt": 0.003,
        "pricing_completion": 0.015,
    })];
    let p =
        slash_stats::extract_pricing_for_model(&models, "us.anthropic.claude-sonnet-4-6").unwrap();
    // cache_read from family fallback = 0.0003 (10% of $3/Mtok)
    assert!(
        (p.cache_read.unwrap() - 0.0003).abs() < 1e-8,
        "expected sonnet cache_read = $0.0003/Ktok, got {:?}",
        p.cache_read
    );
    // cache_write from family fallback = 0.00375 (125% of $3/Mtok)
    assert!(
        (p.cache_write.unwrap() - 0.00375).abs() < 1e-8,
        "expected sonnet cache_write = $0.00375/Ktok, got {:?}",
        p.cache_write
    );
}
