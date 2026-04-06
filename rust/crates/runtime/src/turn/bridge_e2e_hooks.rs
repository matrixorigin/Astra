//! Opt-in **compile-time** hooks for full-stack bridge tests without a real LLM or MatrixOne model row.
//!
//! Enable with crate feature `bridge-e2e-hooks`. At runtime, set `ASTRA_BRIDGE_TEST_SECRET` to a
//! non-empty value and send the same value as header `x-mo-bridge-test-secret` on `POST /chat/turn`
//! (forwarded to the in-process bridge). Body field `test_llm_rounds` is a JSON array of objects:
//! `{ "full_text"?, "reasoning"?, "tool_calls"?, "usage"? }` — same shape as the internal
//! `_inprocess_summary` payload. **Never** enable the feature or set the env var in production.

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
