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
        let cost = cost_for_tokens(1000, 500, 0, 0, &pricing);
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
        assert_eq!(cost_for_tokens(0, 0, 0, 0, &pricing), 0.0);
    }

    #[test]
    fn cost_for_tokens_zero_pricing() {
        let pricing = astra_services::models::PricingData::default();
        assert_eq!(cost_for_tokens(10000, 5000, 0, 0, &pricing), 0.0);
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
        let cost = cost_for_tokens(1_000_000, 500_000, 0, 0, &pricing);
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
        let cost = cost_for_tokens(500, 200, 1000, 100, &pricing);
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
        let cost = cost_for_tokens(0, 0, 1000, 1000, &pricing);
        let expected = (1000.0 * 0.003 * 0.1 / 1000.0) + (1000.0 * 0.003 * 1.25 / 1000.0);
        assert!(
            (cost - expected).abs() < 1e-10,
            "fallback cache cost should be {expected}, got {cost}"
        );
    }

    #[test]
    fn format_cost_sub_cent() {
        assert_eq!(format_cost(0.0001), "$0.0001");
        assert_eq!(format_cost(0.0099), "$0.0099");
    }

    #[test]
    fn format_cost_sub_dollar() {
        assert_eq!(format_cost(0.01), "$0.010");
        assert_eq!(format_cost(0.123), "$0.123");
        assert_eq!(format_cost(0.999), "$0.999");
    }

    #[test]
    fn format_cost_dollars() {
        assert_eq!(format_cost(1.0), "$1.00");
        assert_eq!(format_cost(12.345), "$12.35"); // rounds
        assert_eq!(format_cost(100.0), "$100.00");
    }

    #[test]
    fn format_cost_zero() {
        assert_eq!(format_cost(0.0), "$0.0000");
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
        let p = extract_pricing_for_model(&models, "gpt-4").unwrap();
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
        let p = extract_pricing_for_model(&models, "claude-3").unwrap();
        assert!((p.prompt - 0.008).abs() < 1e-10);
        assert!((p.completion - 0.024).abs() < 1e-10);
    }

    #[test]
    fn extract_pricing_model_not_found() {
        let models = vec![
            serde_json::json!({"name": "gpt-4", "pricing_prompt": 0.03, "pricing_completion": 0.06}),
        ];
        assert!(extract_pricing_for_model(&models, "nonexistent").is_none());
    }

    #[test]
    fn extract_pricing_empty_models() {
        let models: Vec<serde_json::Value> = vec![];
        assert!(extract_pricing_for_model(&models, "any").is_none());
    }

    #[test]
    fn extract_pricing_zero_values_returns_none() {
        let models = vec![serde_json::json!({
            "name": "test",
            "pricing_prompt": 0.0,
            "pricing_completion": 0.0
        })];
        assert!(extract_pricing_for_model(&models, "test").is_none());
    }

    // ── fallback_pricing tests ───────────────────────────────────────────

    #[test]
    fn fallback_sonnet_pricing() {
        let p = fallback_pricing("claude-sonnet-4-20250514");
        assert!((p.prompt - 0.003).abs() < 1e-6);
        assert!((p.completion - 0.015).abs() < 1e-6);
        assert!(p.cache_read.is_some());
        assert!((p.cache_read.unwrap() - 0.0003).abs() < 1e-8);
    }

    #[test]
    fn fallback_opus_4_pricing() {
        let p = fallback_pricing("claude-opus-4-20250514");
        assert!(
            (p.prompt - 0.015).abs() < 1e-6,
            "opus-4 prompt should be $15/Mtok"
        );
        assert!((p.completion - 0.075).abs() < 1e-6);
    }

    #[test]
    fn fallback_opus_45_pricing() {
        let p = fallback_pricing("claude-opus-4.5-20250415");
        assert!(
            (p.prompt - 0.005).abs() < 1e-6,
            "opus 4.5 should be $5/Mtok"
        );
    }

    #[test]
    fn fallback_haiku_pricing() {
        let p = fallback_pricing("claude-haiku-4.5-20250514");
        assert!(
            (p.prompt - 0.001).abs() < 1e-6,
            "haiku 4.5 should be $1/Mtok"
        );
    }

    #[test]
    fn fallback_gpt4o_pricing() {
        let p = fallback_pricing("gpt-4o-2024-08-06");
        assert!((p.prompt - 0.0025).abs() < 1e-6);
    }

    #[test]
    fn fallback_deepseek_pricing() {
        let p = fallback_pricing("deepseek-chat");
        assert!((p.prompt - 0.00027).abs() < 1e-8);
    }

    #[test]
    fn fallback_unknown_uses_sonnet() {
        let p = fallback_pricing("some-unknown-model");
        assert!(
            (p.prompt - 0.003).abs() < 1e-6,
            "unknown model should default to sonnet pricing"
        );
    }

    #[test]
    fn fallback_cost_calculation_with_cache() {
        // Sonnet: 1000 prompt + 500 completion + 2000 cache_read + 100 cache_creation
        let p = fallback_pricing("claude-sonnet-4-20250514");
        let cost = cost_for_tokens(1000, 500, 2000, 100, &p);
        // $0.003/Ktok * 1 + $0.015/Ktok * 0.5 + $0.0003/Ktok * 2 + $0.00375/Ktok * 0.1
        let expected = 0.003 + 0.0075 + 0.0006 + 0.000375;
        assert!(
            (cost - expected).abs() < 1e-8,
            "cost={cost} expected={expected}"
        );
    }

