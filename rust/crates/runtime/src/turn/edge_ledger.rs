//! §5.5 edge callback ledger — single source of truth for keys and consume semantics.
//!
//! HTTP handlers insert; [`super::bridge_inprocess::InProcessChatTurnBridge`] removes entries
//! (poll + take) so each callback is delivered at most once.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use uuid::Uuid;

/// Cap in-memory map size; evicted wholesale when exceeded (handlers + design tradeoff).
pub const LEDGER_MAX_ENTRIES: usize = 4096;

pub const DEFAULT_POLL_INTERVAL_MS: u64 = 50;

pub const MSG_TOOL_LEDGER_TIMEOUT: &str =
    "timed out waiting for edge POST /tools/result (§5.5 ledger)";

#[inline]
pub fn tool_callback_key(user_id: &str, request_id: &str) -> String {
    format!("{user_id}:tool:{request_id}")
}

#[inline]
pub fn approval_callback_key(user_id: &str, request_id: &str) -> String {
    format!("{user_id}:approval:{request_id}")
}

/// Remove and return the value for `key`, waiting up to `timeout` (50ms polling).
pub async fn take_ledger_entry(
    ledger: &Arc<tokio::sync::Mutex<HashMap<String, Value>>>,
    key: &str,
    timeout: Duration,
) -> Option<Value> {
    let poll = Duration::from_millis(DEFAULT_POLL_INTERVAL_MS);
    let started = Instant::now();
    loop {
        {
            let mut g = ledger.lock().await;
            if let Some(v) = g.remove(key) {
                return Some(v);
            }
        }
        if started.elapsed() >= timeout {
            return None;
        }
        tokio::time::sleep(poll).await;
    }
}

/// Fill missing or empty `id` on each tool call so SSE + `POST /tools/result` agree.
pub fn ensure_tool_call_ids(tool_calls: &mut [Value]) {
    for tc in tool_calls.iter_mut() {
        let Some(obj) = tc.as_object_mut() else {
            continue;
        };
        let id_empty = obj
            .get("id")
            .map(|v| v.as_str().map(|s| s.is_empty()).unwrap_or(true))
            .unwrap_or(true);
        if id_empty {
            obj.insert("id".to_string(), Value::String(Uuid::now_v7().to_string()));
        }
    }
}

pub fn tool_content_from_ledger_entry(entry: &Value) -> String {
    let body = entry.get("body").unwrap_or(entry);
    let status = body
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let output = body.get("output").and_then(Value::as_str).unwrap_or("");
    if output.is_empty() {
        serde_json::to_string(&json!({"status": status})).unwrap_or_else(|_| status.to_string())
    } else if matches!(status, "ok" | "success" | "completed") {
        output.to_string()
    } else {
        format!("status={status}\n{output}")
    }
}

pub fn persist_value_for_ledger_tool_result(
    tc: &Value,
    ledger_entry: Option<&Value>,
    timed_out: bool,
) -> Value {
    let id = tc.get("id").and_then(Value::as_str).unwrap_or("");
    let name = tc
        .get("function")
        .and_then(Value::as_object)
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let result = if timed_out {
        MSG_TOOL_LEDGER_TIMEOUT.to_string()
    } else if let Some(e) = ledger_entry {
        tool_content_from_ledger_entry(e)
    } else {
        "missing tool_call id".to_string()
    };
    json!({
        "tool_call_id": id,
        "name": name,
        "result": result,
    })
}

pub fn assistant_message_with_tool_calls(tool_calls: &[Value]) -> Value {
    json!({
        "role": "assistant",
        "content": Value::Null,
        "tool_calls": tool_calls,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_keys_match_handler_convention() {
        assert_eq!(tool_callback_key("u42", "r7"), "u42:tool:r7");
        assert_eq!(approval_callback_key("u42", "a1"), "u42:approval:a1");
    }

    #[test]
    fn ensure_tool_call_ids_fills_empty_and_skips_nonempty() {
        let mut calls = vec![
            json!({"id": "", "function": {"name": "x", "arguments": "{}"}}),
            json!({"id": "keep-me", "function": {"name": "y", "arguments": "{}"}}),
        ];
        ensure_tool_call_ids(&mut calls);
        let id0 = calls[0].get("id").and_then(Value::as_str).unwrap();
        assert!(!id0.is_empty());
        assert_eq!(calls[1].get("id").and_then(Value::as_str), Some("keep-me"));
    }

    #[test]
    fn tool_content_prefers_output_on_success_status() {
        let entry = json!({
            "kind": "tool_result",
            "body": {"status": "ok", "output": "hello"}
        });
        assert_eq!(tool_content_from_ledger_entry(&entry), "hello");
    }

    #[test]
    fn tool_content_wraps_non_success_with_status() {
        let entry = json!({
            "body": {"status": "error", "output": "boom"}
        });
        assert_eq!(tool_content_from_ledger_entry(&entry), "status=error\nboom");
    }

    #[test]
    fn tool_content_reads_handler_shaped_wrapper() {
        let entry = json!({
            "kind": "tool_result",
            "user_id": "u1",
            "edge_id": "e1",
            "body": {"request_id": "c1", "status": "ok", "output": "done"}
        });
        assert_eq!(tool_content_from_ledger_entry(&entry), "done");
    }

    #[test]
    fn persist_value_matches_timeout_constant() {
        let tc = json!({"id": "i", "function": {"name": "n", "arguments": "{}"}});
        let v = persist_value_for_ledger_tool_result(&tc, None, true);
        assert_eq!(v["result"].as_str().unwrap(), MSG_TOOL_LEDGER_TIMEOUT);
    }

    #[tokio::test]
    async fn take_ledger_entry_immediate_remove() {
        let ledger = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let key = tool_callback_key("u", "1");
        ledger.lock().await.insert(key.clone(), json!({"k": 1}));
        let got = take_ledger_entry(&ledger, &key, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(got["k"], 1);
        let again = take_ledger_entry(&ledger, &key, Duration::from_millis(80)).await;
        assert!(again.is_none());
    }

    #[tokio::test]
    async fn take_ledger_entry_waits_for_late_insert() {
        let ledger = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let key = tool_callback_key("u", "late");
        let l2 = ledger.clone();
        let k2 = key.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            l2.lock().await.insert(k2, json!("ready"));
        });
        let got = take_ledger_entry(&ledger, &key, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(got, json!("ready"));
    }

    #[tokio::test]
    async fn take_ledger_entry_times_out_when_never_inserted() {
        let ledger = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let key = tool_callback_key("u", "missing");
        let started = Instant::now();
        let got = take_ledger_entry(&ledger, &key, Duration::from_millis(60)).await;
        assert!(got.is_none());
        assert!(started.elapsed() >= Duration::from_millis(50));
    }
}
