//! Opt-in **compile-time** hooks for full-stack bridge tests without a real LLM or MatrixOne model row.
//!
//! Enable with crate feature `bridge-e2e-hooks`. At runtime, set `ASTRA_BRIDGE_TEST_SECRET` to a
//! non-empty value and send the same value as header `x-mo-bridge-test-secret` on `POST /chat/turn`
//! (forwarded to the in-process bridge). Body field `test_llm_rounds` is a JSON array of objects:
//! `{ "full_text"?, "reasoning"?, "tool_calls"?, "usage"? }` — same shape as the internal
//! `_inprocess_summary` payload. For streaming-failure E2E, body field `test_llm_stream_blocks`
//! may contain raw SSE blocks (strings) that are fed directly into the bridge's in-process stream
//! parser. **Never** enable the feature or set the env var in production.

use axum::http::HeaderMap;
use serde_json::{Map, Value};

fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

pub fn authorized(headers: &HeaderMap) -> bool {
    let Ok(expected) = std::env::var("ASTRA_BRIDGE_TEST_SECRET") else {
        return false;
    };
    if expected.is_empty() {
        return false;
    }
    header_str(headers, "x-mo-bridge-test-secret").as_deref() == Some(expected.as_str())
}

pub fn parse_llm_round(v: &Value) -> (String, String, Vec<Value>, Map<String, Value>) {
    let full_text = v
        .get("full_text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let reasoning = v
        .get("reasoning")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let tool_calls = v
        .get("tool_calls")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let usage = v
        .get("usage")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    (full_text, reasoning, tool_calls, usage)
}

pub fn parse_stream_blocks(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- parse_llm_round ---

    #[test]
    fn parse_full_round() {
        let v = json!({
            "full_text": "hello",
            "reasoning": "because",
            "tool_calls": [{"id": "1"}],
            "usage": {"prompt_tokens": 10}
        });
        let (ft, r, tc, u) = parse_llm_round(&v);
        assert_eq!(ft, "hello");
        assert_eq!(r, "because");
        assert_eq!(tc.len(), 1);
        assert_eq!(u["prompt_tokens"], 10);
    }

    #[test]
    fn parse_empty_object() {
        let (ft, r, tc, u) = parse_llm_round(&json!({}));
        assert!(ft.is_empty());
        assert!(r.is_empty());
        assert!(tc.is_empty());
        assert!(u.is_empty());
    }

    #[test]
    fn parse_wrong_types() {
        let v = json!({"full_text": 42, "tool_calls": "not_array", "usage": "not_object"});
        let (ft, _, tc, u) = parse_llm_round(&v);
        assert!(ft.is_empty());
        assert!(tc.is_empty());
        assert!(u.is_empty());
    }

    #[test]
    fn parse_stream_blocks_reads_strings_only() {
        let blocks = parse_stream_blocks(&json!([
            "data: {\"type\":\"text_delta\",\"content\":\"hi\"}\n\n",
            7,
            "data: {\"type\":\"_inprocess_summary\",\"full_text\":\"hi\"}\n\n"
        ]));
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].contains("\"text_delta\""));
        assert!(blocks[1].contains("\"_inprocess_summary\""));
    }

    // --- authorized ---
    // Note: authorized() tests omitted — env var manipulation is
    // inherently racy in parallel test harness.

    // --- header_str ---

    #[test]
    fn header_str_exists() {
        let mut h = HeaderMap::new();
        h.insert("x-test", "value".parse().unwrap());
        assert_eq!(header_str(&h, "x-test").as_deref(), Some("value"));
    }

    #[test]
    fn header_str_missing() {
        assert!(header_str(&HeaderMap::new(), "x-test").is_none());
    }

    #[test]
    fn header_str_empty_value() {
        let mut h = HeaderMap::new();
        h.insert("x-test", "".parse().unwrap());
        assert!(header_str(&h, "x-test").is_none());
    }
}
