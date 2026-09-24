use crate::cli::slash::slash_stats;

#[test]
fn displayed_cost_rows_keep_missing_evidence_and_overflow_unknown() {
    let pricing = astra_services::models::PricingData {
        prompt: 0.01,
        completion: 0.02,
        cache_read: None,
        cache_write: None,
    };
    let complete =
        slash_stats::scenario_cost_for_lanes([Some(10), Some(2), Some(0), Some(0)], &pricing);
    let missing =
        slash_stats::scenario_cost_for_lanes([Some(10), None, Some(0), Some(0)], &pricing);
    let unpriced =
        slash_stats::scenario_cost_for_lanes([Some(10), Some(2), Some(1), Some(0)], &pricing);
    assert!(complete.is_some());
    for unavailable in [missing, unpriced] {
        assert_eq!(unavailable, None);
        for rows in [
            [complete, unavailable, complete],
            [unavailable, complete, complete],
        ] {
            assert_eq!(
                rows.into_iter()
                    .fold(Some(0.0), slash_stats::add_scenario_cost),
                None
            );
        }
    }
    assert_eq!(
        slash_stats::add_scenario_cost(Some(f64::MAX), Some(f64::MAX)),
        None
    );
    let zero = slash_stats::scenario_cost_for_lanes([Some(0); 4], &pricing);
    assert_eq!(slash_stats::format_optional_cost(zero), "$0.0000");
}

#[test]
fn unavailable_cost_is_not_formatted_as_free() {
    for cost in [None, Some(f64::NAN), Some(f64::INFINITY), Some(-1.0)] {
        assert_eq!(slash_stats::format_optional_cost(cost), "unavailable");
    }
    assert_eq!(slash_stats::format_optional_cost(Some(0.0)), "$0.0000");
    assert_eq!(slash_stats::format_optional_cost(Some(1.5)), "$1.50");
}

#[test]
fn current_rate_scenario_preserves_unknown_prices_and_observed_counts() {
    let mut state = crate::cli::session::session_state::SessionState::default();
    state.total_prompt_tokens = 100;
    state.total_completion_tokens = 20;
    state.total_cache_read_tokens = 900;
    state.total_cache_creation_tokens = 30;
    state.turn = 2;
    state.cached_pricing = astra_services::models::PricingData {
        prompt: 0.01,
        completion: 0.02,
        cache_read: None,
        cache_write: None,
    };
    let rows = slash_stats::current_rate_cost_rows(&state);
    assert!(rows.contains(&("billing", "not a session bill".into())));
    assert!(rows.contains(&("coverage", "unknown".into())));
    assert!(rows.contains(&("attribution", "unknown".into())));
    assert!(rows.contains(&("cache read", "900 (unavailable)".into())));
    assert!(rows.contains(&("cache write", "30 (unavailable)".into())));
    assert!(rows.contains(&("scenario sum", "unavailable".into())));
    assert!(!rows.iter().any(|(label, value)| label.contains("avg")
        || value.contains("saved")
        || value.contains("%")));
    state.cached_pricing.cache_read = Some(0.001);
    state.cached_pricing.cache_write = Some(0.01);
    let rows = slash_stats::current_rate_cost_rows(&state);
    assert!(rows.contains(&("scenario sum", "$2.60".into())));
    state.cached_pricing.cache_read = Some(0.0);
    state.cached_pricing.cache_write = Some(0.0);
    let rows = slash_stats::current_rate_cost_rows(&state);
    assert!(rows.contains(&("cache read", "900 ($0.0000)".into())));
    assert!(rows.contains(&("scenario sum", "$1.40".into())));
    state.total_cache_read_tokens = 0;
    state.total_cache_creation_tokens = 0;
    state.cached_pricing.cache_read = None;
    state.cached_pricing.cache_write = None;
    let rows = slash_stats::current_rate_cost_rows(&state);
    assert!(rows.contains(&("scenario sum", "$1.40".into())));
}

// ── Explicit-rate scenarios ────────────────────────────────────────

#[test]
fn explicit_rate_scenarios() {
    let pricing = astra_services::models::PricingData {
        prompt: 0.000_003,
        completion: 0.000_015,
        cache_read: None,
        cache_write: None,
    };

    // basic: 1000 prompt + 500 completion → $0.0105
    let cost = pricing.estimated_cost_usd(1000, 500, 0, 0).unwrap();
    assert!((cost - 0.0105).abs() < 1e-10);

    // zero inputs
    assert_eq!(pricing.estimated_cost_usd(0, 0, 0, 0).unwrap(), 0.0);

    // zero pricing
    assert_eq!(
        astra_services::models::PricingData::default().estimated_cost_usd(10000, 5000, 0, 0,),
        Some(0.0)
    );

    // large values: 1M prompt + 500K completion → $10.50
    let cost = pricing
        .estimated_cost_usd(1_000_000, 500_000, 0, 0)
        .unwrap();
    assert!((cost - 10.5).abs() < 1e-6);

    // with explicit cache rates
    let cache_pricing = astra_services::models::PricingData {
        prompt: 0.000_003,
        completion: 0.000_015,
        cache_read: Some(0.000_000_3),
        cache_write: Some(0.000_003_75),
    };
    let cost = cache_pricing
        .estimated_cost_usd(500, 200, 1000, 100)
        .unwrap();
    let expected =
        (500.0 * 0.000_003) + (200.0 * 0.000_015) + (1000.0 * 0.000_000_3) + (100.0 * 0.000_003_75);
    assert!((cost - expected).abs() < 1e-10);

    // Missing cache pricing must not charge cached tokens at the prompt rate.
    assert_eq!(
        pricing.estimated_cost_usd(0, 0, 1000, 1000),
        None,
        "unknown cache rates are unpriced instead of guessed"
    );
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
    let models = vec![
        serde_json::json!({"name":"gpt-4","pricing":{"prompt":0.000_03,"completion":0.000_06}}),
    ];
    let p = slash_stats::extract_pricing_for_model(&models, "gpt-4").unwrap();
    assert!((p.prompt - 0.000_03).abs() < 1e-10);
    assert!((p.completion - 0.000_06).abs() < 1e-10);

    // flat fields
    let models = vec![
        serde_json::json!({"name":"claude-3","pricing_prompt":0.000_008,"pricing_completion":0.000_024}),
    ];
    let p = slash_stats::extract_pricing_for_model(&models, "claude-3").unwrap();
    assert!((p.prompt - 0.000_008).abs() < 1e-10);
    assert!((p.completion - 0.000_024).abs() < 1e-10);

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
fn extract_pricing_preserves_missing_cache_rates() {
    let models = vec![serde_json::json!({
        "name": "qwen-plus",
        "pricing_prompt": 0.000_000_8,
        "pricing_completion": 0.000_002,
    })];
    let p = slash_stats::extract_pricing_for_model(&models, "qwen-plus").unwrap();
    assert_eq!(p.cache_write, None);
    assert_eq!(p.cache_read, None);
}

#[test]
fn extract_pricing_preserves_explicit_cache_rates() {
    let models = vec![serde_json::json!({
        "name": "claude-sonnet",
        "pricing_prompt": 0.000_003,
        "pricing_completion": 0.000_015,
        "pricing_cache_read": 0.000_000_3,
        "pricing_cache_write": 0.000_003_75,
    })];
    let p = slash_stats::extract_pricing_for_model(&models, "claude-sonnet").unwrap();
    assert!((p.cache_read.unwrap() - 0.000_000_3).abs() < 1e-10);
    assert!((p.cache_write.unwrap() - 0.000_003_75).abs() < 1e-10);
}

#[test]
fn extract_pricing_rejects_invalid_nested_and_cache_rates() {
    let nested = vec![serde_json::json!({
        "name": "broken-nested",
        "pricing": {"prompt": -0.000_003, "completion": 0.000_015}
    })];
    assert!(
        slash_stats::extract_pricing_for_model(&nested, "broken-nested").is_none(),
        "invalid server pricing must not enter the CLI cost accumulator"
    );

    let flat = vec![serde_json::json!({
        "name": "broken-cache",
        "pricing_prompt": 0.000_003,
        "pricing_completion": 0.000_015,
        "pricing_cache_read": -0.000_001
    })];
    assert!(
        slash_stats::extract_pricing_for_model(&flat, "broken-cache").is_none(),
        "invalid cache pricing must not be converted into a zero-cost estimate"
    );
}

#[test]
fn missing_cache_rates_remain_unpriced_without_family_guesses() {
    let models = vec![serde_json::json!({
        "name": "us.anthropic.claude-sonnet-4-6",
        "pricing_prompt": 0.000_003,
        "pricing_completion": 0.000_015,
    })];
    let p =
        slash_stats::extract_pricing_for_model(&models, "us.anthropic.claude-sonnet-4-6").unwrap();
    assert_eq!(p.cache_read, None);
    assert_eq!(p.cache_write, None);
    assert_eq!(p.estimated_cost_usd(0, 0, 1000, 1000), None);
}

// ── fallback_pricing ────────────────────────────────────────────────

#[test]
fn fallback_pricing_by_model() {
    let cases: &[(&str, f64, Option<f64>)] = &[
        ("claude-sonnet-4-20250514", 0.000_003, Some(0.000_000_3)),
        ("claude-opus-4-20250514", 0.000_015, None),
        ("claude-opus-4.5-20250415", 0.000_005, None),
        ("claude-haiku-4.5-20250514", 0.000_001, None),
        ("gpt-4o-2024-08-06", 0.000_002_5, None),
        ("deepseek-chat", 0.000_000_27, None),
        ("some-unknown-model", 0.000_003, None), // defaults to sonnet
    ];
    for (model, expected_prompt, expected_cache_read) in cases {
        let p = slash_stats::fallback_pricing(model);
        assert!(
            (p.prompt - expected_prompt).abs() < 1e-12,
            "{model}: prompt"
        );
        if let Some(cr) = expected_cache_read {
            assert!(
                (p.cache_read.unwrap() - cr).abs() < 1e-12,
                "{model}: cache_read"
            );
        }
    }
}

#[test]
fn fallback_cost_calculation_with_cache() {
    let p = slash_stats::fallback_pricing("claude-sonnet-4-20250514");
    let cost = p.estimated_cost_usd(1000, 500, 2000, 100).unwrap();
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
