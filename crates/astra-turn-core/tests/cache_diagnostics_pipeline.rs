//! Public cache-diagnostic receipt contracts.
//!
//! These fixtures supply opaque provider-final component identities. Actual
//! provider-wire construction and PipelineSession receipt/feedback wiring have
//! their own tests; planned prompt hashes never supply these identities.

use astra_turn_core::cache_diagnostics::{
    CacheBreakDetector, CacheBreakReason, PromptStateSnapshot, ProviderAttemptCacheIdentity,
    ProviderFinalPromptFingerprint, ProviderFinalToolFingerprint,
};
use serde_json::{Value, json};

fn attempt(request_id: impl Into<String>) -> ProviderAttemptCacheIdentity {
    ProviderAttemptCacheIdentity {
        request_id: request_id.into(),
        attempt: 0,
    }
}

fn snap(system: &str, tools: &[Value], model: &str) -> PromptStateSnapshot {
    let mut snapshot = PromptStateSnapshot::capture("planned metadata", &[], model, 1000);
    snapshot.timestamp_secs = 1000;
    snapshot.attach_provider_final_fingerprint(ProviderFinalPromptFingerprint {
        cache_key_system_sha256: system.into(),
        cache_key_tool_schema_sequence_sha256: serde_json::to_string(tools).unwrap(),
        cache_key_tool_schema_items: tools
            .iter()
            .map(|tool| ProviderFinalToolFingerprint {
                name: tool["function"]["name"].as_str().map(str::to_owned),
                sha256: tool.to_string(),
            })
            .collect(),
        ..Default::default()
    });
    snapshot
}

fn tool(name: &str, description: &str) -> Value {
    json!({"type": "function", "function": {
        "name": name, "description": description,
        "parameters": {"type": "object", "properties": {}}
    }})
}

#[test]
fn public_receipts_classify_component_changes() {
    let tools = vec![tool("bash", "A")];
    for (case, current, expected) in [
        (
            "stable-cold",
            snap("SYS", &tools, "m"),
            Some(CacheBreakReason::UnknownColdStart),
        ),
        (
            "system",
            snap("SYS v2", &tools, "m"),
            Some(CacheBreakReason::SystemPromptChanged),
        ),
        (
            "schema",
            snap("SYS", &[tool("bash", "B")], "m"),
            Some(CacheBreakReason::ToolSchemasChanged {
                added: vec![],
                removed: vec![],
                changed: vec!["bash".into()],
            }),
        ),
        (
            "addition",
            snap("SYS", &[tool("bash", "A"), tool("grep", "G")], "m"),
            Some(CacheBreakReason::ToolSchemasChanged {
                added: vec!["grep".into()],
                removed: vec![],
                changed: vec![],
            }),
        ),
        (
            "model",
            snap("SYS", &tools, "other-model"),
            Some(CacheBreakReason::ModelChanged {
                from: "m".into(),
                to: "other-model".into(),
            }),
        ),
        (
            "combined",
            snap(
                "SYS v2",
                &[tool("bash", "A"), tool("grep", "G")],
                "other-model",
            ),
            Some(CacheBreakReason::Multiple(vec![
                CacheBreakReason::ModelChanged {
                    from: "m".into(),
                    to: "other-model".into(),
                },
                CacheBreakReason::SystemPromptChanged,
                CacheBreakReason::ToolSchemasChanged {
                    added: vec!["grep".into()],
                    removed: vec![],
                    changed: vec![],
                },
            ])),
        ),
    ] {
        let mut detector = CacheBreakDetector::new();
        let (accepted, event) = detector.record_provider_attempt_for_source(
            "main",
            &attempt(format!("{case}-baseline")),
            snap("SYS", &tools, "m"),
            Some(0),
        );
        assert!(accepted);
        assert!(event.is_none());
        let (accepted, event) = detector.record_provider_attempt_for_source(
            "main",
            &attempt(format!("{case}-current")),
            current,
            Some(0),
        );
        assert!(accepted);
        if let Some(event) = &event {
            assert!(event.estimated_token_impact > 0, "{case}");
            assert!(event.suggestion.is_some(), "{case}");
        }
        // An identical measured request with zero cache reuse is an explicit
        // cold start, even though no structural component changed.
        assert_eq!(event.map(|event| event.reason), expected, "{case}");
    }
}

#[test]
fn physical_usage_controls_ttl_attribution_and_does_not_guess_when_unknown() {
    for (case, system, gap, baseline_usage, current_usage, expected) in [
        (
            "expired",
            "SYS",
            3601,
            Some(0),
            Some(0),
            Some(CacheBreakReason::TtlExpired { gap_seconds: 3601 }),
        ),
        ("healthy", "SYS", 100_000, Some(0), Some(15_000), None),
        (
            "short-cold",
            "SYS",
            240,
            Some(0),
            Some(0),
            Some(CacheBreakReason::UnknownColdStart),
        ),
        (
            "structural",
            "changed",
            10_000,
            Some(0),
            Some(0),
            Some(CacheBreakReason::SystemPromptChanged),
        ),
        ("unknown", "SYS", 10_000, None, None, None),
    ] {
        let mut detector = CacheBreakDetector::new();
        let mut baseline = snap("SYS", &[], "m");
        baseline.cache_eligible_tokens = 10_000;
        let (accepted, event) = detector.record_provider_attempt_for_source(
            "main",
            &attempt(format!("{case}-baseline")),
            baseline,
            baseline_usage,
        );
        assert!(accepted);
        assert!(event.is_none());
        let mut current = snap(system, &[], "m");
        current.cache_eligible_tokens = 10_000;
        current.timestamp_secs += gap;
        let (accepted, event) = detector.record_provider_attempt_for_source(
            "main",
            &attempt(format!("{case}-current")),
            current,
            current_usage,
        );
        assert!(accepted);
        if let Some(event) = &event {
            assert!(event.suggestion.is_some(), "{case}");
        }
        assert_eq!(event.map(|event| event.reason), expected, "{case}");
        assert_eq!(
            detector.stats.total_turns,
            if current_usage.is_some() { 2 } else { 0 }
        );
    }
}

#[test]
fn measured_small_prefix_reuse_accumulates_exact_hits_and_misses() {
    let mut detector = CacheBreakDetector::new();
    let tools = vec![tool("bash", "A")];
    for (request_id, system, cache_read) in [
        ("initial", "SYS", 0),
        ("reuse", "SYS", 900),
        ("reuse-again", "SYS", 900),
        ("changed", "SYS v2", 0),
    ] {
        let mut snapshot = snap(system, &tools, "m");
        snapshot.cache_eligible_tokens = 512;
        assert!(
            detector
                .record_provider_attempt_for_source(
                    "main",
                    &attempt(request_id),
                    snapshot,
                    Some(cache_read),
                )
                .0
        );
    }
    assert_eq!(detector.stats.total_turns, 4);
    assert_eq!(detector.stats.cache_hits, 2);
    assert_eq!(detector.stats.cache_misses, 2);
    assert_eq!(detector.stats.hit_rate_percent(), 50.0);
    assert_eq!(detector.stats.recent_breaks.len(), 1);
    assert_eq!(
        detector.stats.recent_breaks[0].reason,
        CacheBreakReason::SystemPromptChanged
    );
}
