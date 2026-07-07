use crate::cli::slash::slash_stats;

// ── cost_for_tokens ─────────────────────────────────────────────────

#[test]
fn cost_for_tokens() {
    let pricing = astra_services::models::PricingData {
        prompt: 0.003,
        completion: 0.015,
        cache_read: None,
        cache_write: None,
    };

    // basic: 1000 prompt + 500 completion → $0.0105
    let cost = slash_stats::cost_for_tokens(1000, 500, 0, 0, &pricing);
    assert!((cost - 0.0105).abs() < 1e-10);

    // zero inputs
    assert_eq!(slash_stats::cost_for_tokens(0, 0, 0, 0, &pricing), 0.0);

    // zero pricing
    assert_eq!(
        slash_stats::cost_for_tokens(
            10000,
            5000,
            0,
            0,
            &astra_services::models::PricingData::default()
        ),
        0.0
    );

    // large values: 1M prompt + 500K completion → $10.50
    let cost = slash_stats::cost_for_tokens(1_000_000, 500_000, 0, 0, &pricing);
    assert!((cost - 10.5).abs() < 1e-6);

    // with explicit cache rates
    let cache_pricing = astra_services::models::PricingData {
        prompt: 0.003,
        completion: 0.015,
        cache_read: Some(0.0003),
        cache_write: Some(0.00375),
    };
    let cost = slash_stats::cost_for_tokens(500, 200, 1000, 100, &cache_pricing);
    let expected = (500.0 * 0.003 / 1000.0)
        + (200.0 * 0.015 / 1000.0)
        + (1000.0 * 0.0003 / 1000.0)
        + (100.0 * 0.00375 / 1000.0);
    assert!((cost - expected).abs() < 1e-10);

    // cache fallback: None → 10%/125% of prompt rate
    let cost = slash_stats::cost_for_tokens(0, 0, 1000, 1000, &pricing);
    let expected = (1000.0 * 0.003 * 0.1 / 1000.0) + (1000.0 * 0.003 * 1.25 / 1000.0);
    assert!((cost - expected).abs() < 1e-10);
}

// ── format_cost ─────────────────────────────────────────────────────

#[test]
fn format_cost() {
    for (input, expected) in [
        (0.0001, "$0.0001"),
        (0.0099, "$0.0099"),
        (0.01, "$0.010"),
        (0.123, "$0.123"),
        (0.999, "$0.999"),
        (1.0, "$1.00"),
        (12.345, "$12.35"),
        (100.0, "$100.00"),
        (0.0, "$0.0000"),
    ] {
        assert_eq!(slash_stats::format_cost(input), expected);
    }
}

// ── extract_pricing_for_model ───────────────────────────────────────

#[test]
fn extract_pricing_for_model_basic_scenarios() {
    // nested object
    let models =
        vec![serde_json::json!({"name":"gpt-4","pricing":{"prompt":0.03,"completion":0.06}})];
    let p = slash_stats::extract_pricing_for_model(&models, "gpt-4").unwrap();
    assert!((p.prompt - 0.03).abs() < 1e-10);
    assert!((p.completion - 0.06).abs() < 1e-10);

    // flat fields
    let models = vec![
        serde_json::json!({"name":"claude-3","pricing_prompt":0.008,"pricing_completion":0.024}),
    ];
    let p = slash_stats::extract_pricing_for_model(&models, "claude-3").unwrap();
    assert!((p.prompt - 0.008).abs() < 1e-10);
    assert!((p.completion - 0.024).abs() < 1e-10);

    // model not found
    assert!(slash_stats::extract_pricing_for_model(&models, "nonexistent").is_none());

    // empty models
    let empty: Vec<serde_json::Value> = vec![];
    assert!(slash_stats::extract_pricing_for_model(&empty, "any").is_none());

    // zero values → None
    let models =
        vec![serde_json::json!({"name":"test","pricing_prompt":0.0,"pricing_completion":0.0})];
    assert!(slash_stats::extract_pricing_for_model(&models, "test").is_none());
}

#[test]
fn extract_pricing_inherits_cache_read_from_family_when_missing() {
    let models = vec![serde_json::json!({
        "name": "qwen-plus",
        "pricing_prompt": 0.0008,
        "pricing_completion": 0.002,
    })];
    let p = slash_stats::extract_pricing_for_model(&models, "qwen-plus").unwrap();
    assert_eq!(p.cache_write, None);
    assert!(p.cache_read.is_some());
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
    // Bedrock model with only prompt/completion — cache rates come from family fallback
    let models = vec![serde_json::json!({
        "name": "us.anthropic.claude-sonnet-4-6",
        "pricing_prompt": 0.003,
        "pricing_completion": 0.015,
    })];
    let p =
        slash_stats::extract_pricing_for_model(&models, "us.anthropic.claude-sonnet-4-6").unwrap();
    assert!((p.cache_read.unwrap() - 0.0003).abs() < 1e-8);
    assert!((p.cache_write.unwrap() - 0.00375).abs() < 1e-8);
}

// ── fallback_pricing ────────────────────────────────────────────────

#[test]
fn fallback_pricing_by_model() {
    let cases: &[(&str, f64, Option<f64>)] = &[
        ("claude-sonnet-4-20250514", 0.003, Some(0.0003)),
        ("claude-opus-4-20250514", 0.015, None),
        ("claude-opus-4.5-20250415", 0.005, None),
        ("claude-haiku-4.5-20250514", 0.001, None),
        ("gpt-4o-2024-08-06", 0.0025, None),
        ("deepseek-chat", 0.00027, None),
        ("some-unknown-model", 0.003, None), // defaults to sonnet
    ];
    for (model, expected_prompt, expected_cache_read) in cases {
        let p = slash_stats::fallback_pricing(model);
        assert!((p.prompt - expected_prompt).abs() < 1e-6, "{model}: prompt");
        if let Some(cr) = expected_cache_read {
            assert!(
                (p.cache_read.unwrap() - cr).abs() < 1e-8,
                "{model}: cache_read"
            );
        }
    }
}

#[test]
fn fallback_cost_calculation_with_cache() {
    let p = slash_stats::fallback_pricing("claude-sonnet-4-20250514");
    let cost = slash_stats::cost_for_tokens(1000, 500, 2000, 100, &p);
    let expected = 0.003 + 0.0075 + 0.0006 + 0.000375;
    assert!((cost - expected).abs() < 1e-8);
}

#[test]
fn fallback_no_cache_write_premium_for_non_anthropic() {
    for model in ["qwen-plus", "MiniMax-M2.5", "glm-5.1"] {
        let p = slash_stats::fallback_pricing(model);
        assert_eq!(
            p.cache_write, None,
            "{model}: must not inherit Anthropic cache_write"
        );
        if model == "qwen-plus" {
            assert!(p.cache_read.is_some(), "qwen should define cache_read");
        }
    }
}
