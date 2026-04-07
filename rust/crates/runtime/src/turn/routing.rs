use serde_json::{Map, Value, json};

/// Maximum tool execution rounds per turn. Reads from `MO_MAX_TOOL_ROUNDS` env
/// var at process start, defaulting to 10.
pub fn max_tool_rounds() -> i64 {
    astra_core::RuntimeLimits::global().max_tool_rounds
}

/// Compile-time constant for tests that need `const` assertions.
/// Runtime value from `max_tool_rounds()` may differ if env var is set.
pub const MAX_TOOL_ROUNDS: i64 = 10;

pub fn detect_correction(query: &str) -> bool {
    let pattern = regex::Regex::new(
        r"不对|错了|不是这样|你搞错|不正确|wrong|incorrect|that's not|no,\s|actually,?\s",
    )
    .expect("correction regex should compile");
    pattern.is_match(query.trim())
}

pub fn build_skipped_routing_metadata(reason: &str) -> Map<String, Value> {
    Map::from_iter([
        ("skipped".to_string(), Value::Bool(true)),
        ("reason".to_string(), Value::String(reason.to_string())),
    ])
}

#[allow(clippy::too_many_arguments)]
pub fn build_routing_metadata(
    router: &str,
    intent: &str,
    confidence: f64,
    tier: i64,
    matched_by: &str,
    threshold: f64,
    latency_ms: f64,
    forced: Option<&str>,
    load_tools: bool,
    load_history: &Value,
    load_memory: &Value,
    estimated_tokens: i64,
    memory_policy: Option<Value>,
    has_tier1: bool,
    tier1_compressed: bool,
    tier1_pruned_tools: Option<Vec<String>>,
) -> Map<String, Value> {
    let mut metadata = Map::from_iter([
        ("router".to_string(), Value::String(router.to_string())),
        ("intent".to_string(), Value::String(intent.to_string())),
        ("confidence".to_string(), json!(confidence)),
        ("tier".to_string(), json!(tier)),
        (
            "matched_by".to_string(),
            Value::String(matched_by.to_string()),
        ),
        ("threshold".to_string(), json!(threshold)),
        ("latency_ms".to_string(), json!(latency_ms)),
        (
            "forced".to_string(),
            forced
                .map(|value| Value::String(value.to_string()))
                .unwrap_or(Value::Null),
        ),
        (
            "skipped_sections".to_string(),
            Value::Array(
                [
                    (!load_tools).then_some("tools"),
                    (load_history == &Value::Bool(false)).then_some("history"),
                    (load_memory == &Value::Bool(false)).then_some("memory"),
                ]
                .into_iter()
                .flatten()
                .map(|value| Value::String(value.to_string()))
                .collect(),
            ),
        ),
        ("estimated_tokens".to_string(), json!(estimated_tokens)),
        (
            "memory_policy".to_string(),
            memory_policy.unwrap_or(Value::Null),
        ),
    ]);

    if has_tier1 {
        metadata.insert(
            "tier1".to_string(),
            json!({
                "compressed": tier1_compressed,
                "pruned_tools": tier1_pruned_tools,
            }),
        );
    }

    metadata
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── detect_correction ──

    #[test]
    fn detect_correction_positive_en() {
        assert!(detect_correction("wrong, that's not what I meant"));
        assert!(detect_correction("that's not right"));
    }

    #[test]
    fn detect_correction_positive_cn() {
        assert!(detect_correction("不对，重新来"));
        assert!(detect_correction("你搞错了"));
    }

    #[test]
    fn detect_correction_negative() {
        assert!(!detect_correction("looks good, merge it"));
        assert!(!detect_correction("list the files"));
    }

    // --- edge cases ---

    #[test]
    fn detect_correction_empty_string() {
        assert!(!detect_correction(""));
    }

    #[test]
    fn detect_correction_whitespace_only() {
        assert!(!detect_correction("   \t  "));
    }

    #[test]
    fn detect_correction_case_sensitivity() {
        // The regex uses lowercase, so "Wrong" won't match (case sensitive)
        assert!(!detect_correction("Wrong answer"));
        // but "wrong" will
        assert!(detect_correction("wrong answer"));
    }

    #[test]
    fn detect_correction_actually_with_comma() {
        assert!(detect_correction("actually, I meant something else"));
        assert!(detect_correction("actually I meant something else"));
    }

    #[test]
    fn build_skipped_routing_fields() {
        let meta = build_skipped_routing_metadata("too short");
        assert_eq!(meta.get("skipped").and_then(Value::as_bool), Some(true));
        assert_eq!(
            meta.get("reason").and_then(Value::as_str),
            Some("too short")
        );
    }

    #[test]
    fn build_routing_metadata_no_tier1() {
        let meta = build_routing_metadata(
            "default",
            "code_edit",
            0.95,
            2,
            "keyword",
            0.7,
            12.5,
            None,
            true,
            &Value::Bool(true),
            &Value::Bool(true),
            5000,
            None,
            false,
            false,
            None,
        );
        assert_eq!(meta.get("router").and_then(Value::as_str), Some("default"));
        assert_eq!(
            meta.get("intent").and_then(Value::as_str),
            Some("code_edit")
        );
        assert!(meta.get("tier1").is_none()); // has_tier1 = false
        assert_eq!(meta.get("forced"), Some(&Value::Null));
    }

    #[test]
    fn build_routing_metadata_with_tier1() {
        let meta = build_routing_metadata(
            "r",
            "intent",
            0.5,
            1,
            "m",
            0.5,
            10.0,
            Some("user_override"),
            true,
            &Value::Bool(true),
            &Value::Bool(true),
            1000,
            None,
            true,
            true,
            Some(vec!["bash".into()]),
        );
        let tier1 = meta.get("tier1").and_then(Value::as_object).unwrap();
        assert_eq!(tier1.get("compressed").and_then(Value::as_bool), Some(true));
        assert_eq!(
            meta.get("forced").and_then(Value::as_str),
            Some("user_override")
        );
    }

    #[test]
    fn build_routing_metadata_skipped_sections() {
        let meta = build_routing_metadata(
            "r",
            "i",
            0.5,
            1,
            "m",
            0.5,
            0.0,
            None,
            false,
            &Value::Bool(false),
            &Value::Bool(false),
            0,
            None,
            false,
            false,
            None,
        );
        let skipped = meta
            .get("skipped_sections")
            .and_then(Value::as_array)
            .unwrap();
        // load_tools=false, history=false, memory=false → all three sections skipped
        assert_eq!(skipped.len(), 3);
    }

    #[test]
    fn build_routing_metadata_no_skipped_sections() {
        let meta = build_routing_metadata(
            "r",
            "i",
            0.5,
            1,
            "m",
            0.5,
            0.0,
            None,
            true,
            &Value::Bool(true),
            &Value::Bool(true),
            0,
            None,
            false,
            false,
            None,
        );
        let skipped = meta
            .get("skipped_sections")
            .and_then(Value::as_array)
            .unwrap();
        assert!(skipped.is_empty());
    }
}
