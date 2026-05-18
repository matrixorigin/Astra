use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

pub fn detect_correction(query: &str) -> bool {
    static CORRECTION_PATTERN: OnceLock<Regex> = OnceLock::new();
    let pattern = CORRECTION_PATTERN.get_or_init(|| {
        Regex::new(
            r"不对|错了|不是这样|你搞错|不正确|wrong|incorrect|that's not|no,\s|actually,?\s",
        )
        .expect("correction regex should compile")
    });
    pattern.is_match(query.trim())
}

pub fn build_skipped_routing_metadata(reason: &str) -> Map<String, Value> {
    Map::from_iter([
        ("skipped".to_string(), Value::Bool(true)),
        ("reason".to_string(), Value::String(reason.to_string())),
    ])
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
}
