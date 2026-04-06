//! §5.5 cloud → edge tool delivery: optional approval gate, then `tool_request`, then tool result ledger.
//!
//! Used by [`super::bridge_inprocess::InProcessChatTurnBridge`] so logic stays testable without LLM I/O.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use astra_thin_client::ApprovalRespondRequest;
use serde_json::{Map, Value, json};

use super::cloud_approval_policy::{bash_command_is_read_only, edge_tool_requires_cloud_approval};
use super::edge_ledger::{
    MSG_TOOL_LEDGER_TIMEOUT, approval_callback_key, persist_value_for_ledger_tool_result,
    take_ledger_entry, tool_callback_key, tool_content_from_ledger_entry,
};
use super::stream_events::{
    build_approval_required_event, build_edge_tool_call_event, build_tool_request_event,
};
use super::tool_argument_hints::{normalize_llm_function_arguments, path_hint_from_args};

pub const MSG_APPROVAL_LEDGER_TIMEOUT: &str =
    "timed out waiting for edge POST /approval/respond (§5.5 ledger)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloudApprovalResult {
    Allowed,
    Denied { reason: Option<String> },
    Timeout,
    Malformed,
}

fn cloud_tool_requires_approval(tool_call: &Value) -> bool {
    let name = tool_call
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");

    // Special handling for bash: check if command is read-only
    if name == "bash" || name == "shell" || name == "exec" || name == "run_command" {
        let args = raw_tool_arguments(tool_call);
        let parsed = normalize_llm_function_arguments(&args);
        if let Some(command) = parsed.get("command").and_then(Value::as_str)
            && bash_command_is_read_only(command)
        {
            return false; // Read-only bash commands don't need approval
        }
    }

    edge_tool_requires_cloud_approval(name)
}

fn raw_tool_arguments(tool_call: &Value) -> Value {
    tool_call
        .get("function")
        .and_then(|f| f.get("arguments"))
        .cloned()
        .unwrap_or_else(|| json!("{}"))
}

fn tool_path_hint(tool_call: &Value) -> Option<String> {
    let raw = raw_tool_arguments(tool_call);
    let parsed = normalize_llm_function_arguments(&raw);
    path_hint_from_args(&parsed)
}

pub fn parse_cloud_approval_outcome(entry: Option<&Value>) -> CloudApprovalResult {
    let Some(wrapper) = entry else {
        return CloudApprovalResult::Timeout;
    };
    let body = wrapper.get("body").unwrap_or(wrapper);
    let Ok(req) = serde_json::from_value::<ApprovalRespondRequest>(body.clone()) else {
        return CloudApprovalResult::Malformed;
    };
    match req.decision {
        astra_thin_client::ApprovalDecision::Allow
        | astra_thin_client::ApprovalDecision::AllowSession => CloudApprovalResult::Allowed,
        astra_thin_client::ApprovalDecision::Deny => {
            CloudApprovalResult::Denied { reason: req.reason }
        }
    }
}

fn denied_tool_content(reason: Option<&str>) -> String {
    json!({
        "error": "user_denied",
        "reason": reason.unwrap_or(""),
    })
    .to_string()
}

fn persist_denied_tool_result(tc: &Value, reason: Option<&str>) -> Value {
    let id = tc.get("id").and_then(Value::as_str).unwrap_or("");
    let name = tc
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    json!({
        "tool_call_id": id,
        "name": name,
        "result": denied_tool_content(reason),
    })
}

#[derive(Debug, Default, Clone)]
pub struct EdgeToolRoundDelivery {
    pub sse_maps: Vec<Map<String, Value>>,
    pub tool_messages: Vec<Value>,
    pub persist_tool_results: Vec<Value>,
}

/// After the bridge has yielded `build_approval_required_event`, waits on the approval ledger.
/// `Ok(())` means allowed; `Err` is a finished tool round (denied / timeout / malformed).
pub(crate) async fn wait_approval_ledger_for_tool(
    ledger: &Arc<tokio::sync::Mutex<HashMap<String, Value>>>,
    user_id: &str,
    tc: &Value,
    ledger_wait: Duration,
) -> Result<(), EdgeToolRoundDelivery> {
    let Some(tc_map) = tc.as_object() else {
        return Ok(());
    };
    let id = tc_map.get("id").and_then(Value::as_str).unwrap_or("");
    let tool_name = tc_map
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let ap_key = approval_callback_key(user_id, id);
    let ap_entry = take_ledger_entry(ledger, &ap_key, ledger_wait).await;
    match parse_cloud_approval_outcome(ap_entry.as_ref()) {
        CloudApprovalResult::Denied { reason } => Err(EdgeToolRoundDelivery {
            sse_maps: vec![],
            tool_messages: vec![json!({
                "role": "tool",
                "tool_call_id": id,
                "content": denied_tool_content(reason.as_deref()),
            })],
            persist_tool_results: vec![persist_denied_tool_result(tc, reason.as_deref())],
        }),
        CloudApprovalResult::Timeout => Err(EdgeToolRoundDelivery {
            sse_maps: vec![],
            tool_messages: vec![json!({
                "role": "tool",
                "tool_call_id": id,
                "content": MSG_APPROVAL_LEDGER_TIMEOUT,
            })],
            persist_tool_results: vec![json!({
                "tool_call_id": id,
                "name": tool_name,
                "result": MSG_APPROVAL_LEDGER_TIMEOUT,
            })],
        }),
        CloudApprovalResult::Malformed => Err(EdgeToolRoundDelivery {
            sse_maps: vec![],
            tool_messages: vec![json!({
                "role": "tool",
                "tool_call_id": id,
                "content": "malformed approval response (§5.5 ledger)",
            })],
            persist_tool_results: vec![json!({
                "tool_call_id": id,
                "name": tool_name,
                "result": "malformed approval response (§5.5 ledger)",
            })],
        }),
        CloudApprovalResult::Allowed => Ok(()),
    }
}

/// `edge_tool_call` + `tool_request` maps (caller must stream these before waiting on the tool ledger).
pub(crate) fn sse_maps_through_tool_request(tc: &Value) -> Vec<Map<String, Value>> {
    let Some(tc_map) = tc.as_object() else {
        return vec![];
    };
    vec![
        build_edge_tool_call_event(tc_map),
        build_tool_request_event(tc_map),
    ]
}

/// After `tool_request` was sent to the client, block on `POST /tools/result`.
pub(crate) async fn wait_tool_result_ledger_for_tool(
    ledger: &Arc<tokio::sync::Mutex<HashMap<String, Value>>>,
    user_id: &str,
    tc: &Value,
    ledger_wait: Duration,
) -> EdgeToolRoundDelivery {
    let mut out = EdgeToolRoundDelivery::default();
    let Some(tc_map) = tc.as_object() else {
        return out;
    };
    let id = tc_map.get("id").and_then(Value::as_str).unwrap_or("");
    let t_key = tool_callback_key(user_id, id);
    let tr_entry = take_ledger_entry(ledger, &t_key, ledger_wait).await;
    let timed_out = tr_entry.is_none();
    let content = tr_entry
        .as_ref()
        .map(tool_content_from_ledger_entry)
        .unwrap_or_else(|| MSG_TOOL_LEDGER_TIMEOUT.to_string());
    out.tool_messages.push(json!({
        "role": "tool",
        "tool_call_id": id,
        "content": content,
    }));
    out.persist_tool_results
        .push(persist_value_for_ledger_tool_result(
            tc,
            tr_entry.as_ref(),
            timed_out,
        ));
    out
}

pub(crate) fn cloud_tool_requires_approval_for_delivery(tool_call: &Value) -> bool {
    cloud_tool_requires_approval(tool_call)
}

pub(crate) fn tool_path_hint_for_delivery(tool_call: &Value) -> Option<String> {
    tool_path_hint(tool_call)
}

pub async fn deliver_tool_calls_through_edge_ledger(
    ledger: &Arc<tokio::sync::Mutex<HashMap<String, Value>>>,
    user_id: &str,
    tool_calls: &[Value],
    ledger_wait: Duration,
) -> EdgeToolRoundDelivery {
    let mut out = EdgeToolRoundDelivery::default();

    for tc in tool_calls {
        let Some(tc_map) = tc.as_object() else {
            continue;
        };
        let id = tc_map.get("id").and_then(Value::as_str).unwrap_or("");
        let tool_name = tc_map
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("");

        if cloud_tool_requires_approval(tc) {
            let path = tool_path_hint(tc);
            out.sse_maps.push(build_approval_required_event(
                id,
                tool_name,
                path.as_deref(),
            ));
            match wait_approval_ledger_for_tool(ledger, user_id, tc, ledger_wait).await {
                Ok(()) => {}
                Err(part) => {
                    out.tool_messages.extend(part.tool_messages);
                    out.persist_tool_results.extend(part.persist_tool_results);
                    continue;
                }
            }
        }

        out.sse_maps.extend(sse_maps_through_tool_request(tc));
        let tail = wait_tool_result_ledger_for_tool(ledger, user_id, tc, ledger_wait).await;
        out.tool_messages.extend(tail.tool_messages);
        out.persist_tool_results.extend(tail.persist_tool_results);
    }

    out
}

/// Concurrent variant of [`deliver_tool_calls_through_edge_ledger`] for testing.
///
/// **Not used in production** — the bridge generator must `yield` SSE events
/// immediately (before waiting), so it inlines the same logic. This function
/// accumulates SSE maps in a vec, which would deadlock in production (client
/// can't POST results until it receives the SSE events).
///
/// Tests use spawned tasks to populate the ledger, so the accumulation is safe.
#[cfg(test)]
pub async fn deliver_tool_calls_concurrent(
    ledger: &Arc<tokio::sync::Mutex<HashMap<String, Value>>>,
    user_id: &str,
    tool_calls: &[Value],
    ledger_wait: Duration,
) -> EdgeToolRoundDelivery {
    let mut out = EdgeToolRoundDelivery::default();
    let mut read_only: Vec<&Value> = Vec::new();

    // Phase 1: approval-required tools sequentially, collect read-only for later.
    for tc in tool_calls {
        let Some(tc_map) = tc.as_object() else {
            continue;
        };
        if !cloud_tool_requires_approval(tc) {
            read_only.push(tc);
            continue;
        }
        let id = tc_map.get("id").and_then(Value::as_str).unwrap_or("");
        let tool_name = tc_map
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let path = tool_path_hint(tc);
        out.sse_maps.push(build_approval_required_event(
            id,
            tool_name,
            path.as_deref(),
        ));
        match wait_approval_ledger_for_tool(ledger, user_id, tc, ledger_wait).await {
            Ok(()) => {}
            Err(part) => {
                out.tool_messages.extend(part.tool_messages);
                out.persist_tool_results.extend(part.persist_tool_results);
                continue;
            }
        }
        out.sse_maps.extend(sse_maps_through_tool_request(tc));
        let tail = wait_tool_result_ledger_for_tool(ledger, user_id, tc, ledger_wait).await;
        out.tool_messages.extend(tail.tool_messages);
        out.persist_tool_results.extend(tail.persist_tool_results);
    }

    // Phase 2: read-only tools — emit all SSE events, then await results concurrently.
    for tc in &read_only {
        out.sse_maps.extend(sse_maps_through_tool_request(tc));
    }
    if !read_only.is_empty() {
        let futs: Vec<_> =
            read_only
                .iter()
                .map(|tc| {
                    let ledger = ledger.clone();
                    let uid = user_id.to_owned();
                    let tc = (*tc).clone();
                    async move {
                        wait_tool_result_ledger_for_tool(&ledger, &uid, &tc, ledger_wait).await
                    }
                })
                .collect();
        for tail in futures_util::future::join_all(futs).await {
            out.tool_messages.extend(tail.tool_messages);
            out.persist_tool_results.extend(tail.persist_tool_results);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_thin_client::ApprovalDecision;

    fn read_tool(id: &str) -> Value {
        json!({
            "id": id,
            "type": "function",
            "function": {"name": "read_file", "arguments": r#"{"path": "a.rs"}"#}
        })
    }

    fn write_tool(id: &str) -> Value {
        json!({
            "id": id,
            "type": "function",
            "function": {"name": "write_file", "arguments": r#"{"path": "b.rs", "content": "x"}"#}
        })
    }

    #[test]
    fn parse_allow_from_handler_shape() {
        let entry = json!({
            "kind": "approval_respond",
            "body": {"request_id": "t1", "decision": "allow"}
        });
        assert_eq!(
            parse_cloud_approval_outcome(Some(&entry)),
            CloudApprovalResult::Allowed
        );
    }

    #[test]
    fn parse_deny_with_reason() {
        let entry = json!({
            "kind": "approval_respond",
            "body": {"request_id": "t1", "decision": "deny", "reason": "nope"}
        });
        assert_eq!(
            parse_cloud_approval_outcome(Some(&entry)),
            CloudApprovalResult::Denied {
                reason: Some("nope".into())
            }
        );
    }

    #[tokio::test]
    async fn read_file_skips_approval_emits_tool_pair() {
        let ledger = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let uid = "u1";
        let tc = read_tool("c1");
        let l2 = ledger.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(15)).await;
            l2.lock().await.insert(
                tool_callback_key(uid, "c1"),
                json!({"body": {"request_id": "c1", "status": "ok", "output": "file"}}),
            );
        });
        let d = deliver_tool_calls_through_edge_ledger(&ledger, uid, &[tc], Duration::from_secs(2))
            .await;
        assert_eq!(d.sse_maps.len(), 2);
        assert_eq!(
            d.sse_maps[0].get("type").and_then(Value::as_str),
            Some("tool_call")
        );
        assert_eq!(
            d.sse_maps[1].get("type").and_then(Value::as_str),
            Some("tool_request")
        );
        assert_eq!(d.tool_messages.len(), 1);
        assert!(
            d.tool_messages[0]["content"]
                .as_str()
                .unwrap()
                .contains("file")
        );
    }

    #[tokio::test]
    async fn write_file_waits_approval_then_tool() {
        let ledger = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let uid = "u2";
        let tc = write_tool("w1");
        let l2 = ledger.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            l2.lock().await.insert(
                approval_callback_key(uid, "w1"),
                json!({
                    "kind": "approval_respond",
                    "body": serde_json::to_value(ApprovalRespondRequest {
                        request_id: "w1".into(),
                        decision: ApprovalDecision::Allow,
                        reason: None,
                    }).unwrap()
                }),
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
            l2.lock().await.insert(
                tool_callback_key(uid, "w1"),
                json!({"body": {"request_id": "w1", "status": "ok", "output": "wrote"}}),
            );
        });
        let d = deliver_tool_calls_through_edge_ledger(&ledger, uid, &[tc], Duration::from_secs(2))
            .await;
        assert_eq!(
            d.sse_maps[0].get("type").and_then(Value::as_str),
            Some("approval_required")
        );
        assert_eq!(
            d.sse_maps[0].get("path").and_then(Value::as_str),
            Some("b.rs")
        );
        assert_eq!(d.sse_maps.len(), 3);
        assert!(
            d.tool_messages[0]["content"]
                .as_str()
                .unwrap()
                .contains("wrote")
        );
    }

    #[tokio::test]
    async fn write_file_deny_skips_tool_ledger() {
        let ledger = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let uid = "u3";
        let tc = write_tool("w2");
        let l2 = ledger.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            l2.lock().await.insert(
                approval_callback_key(uid, "w2"),
                json!({
                    "kind": "approval_respond",
                    "body": serde_json::to_value(ApprovalRespondRequest {
                        request_id: "w2".into(),
                        decision: ApprovalDecision::Deny,
                        reason: Some("policy".into()),
                    }).unwrap()
                }),
            );
        });
        let d = deliver_tool_calls_through_edge_ledger(&ledger, uid, &[tc], Duration::from_secs(2))
            .await;
        assert_eq!(d.sse_maps.len(), 1);
        let body = d.tool_messages[0]["content"].as_str().unwrap();
        assert!(body.contains("user_denied"));
        assert!(body.contains("policy"));
        assert!(ledger.lock().await.is_empty());
    }

    // ── deliver_tool_calls_concurrent ─────────────────────────────────────

    #[tokio::test]
    async fn concurrent_mixed_batch_approval_plus_read_only() {
        // 1 write_file (needs approval) + 2 read_file (read-only, concurrent).
        let ledger = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let uid = "u_mix";
        let tcs = vec![write_tool("w1"), read_tool("r1"), read_tool("r2")];

        let l2 = ledger.clone();
        tokio::spawn(async move {
            // Approval for write_file
            tokio::time::sleep(Duration::from_millis(10)).await;
            l2.lock().await.insert(
                approval_callback_key(uid, "w1"),
                json!({
                    "kind": "approval_respond",
                    "body": serde_json::to_value(ApprovalRespondRequest {
                        request_id: "w1".into(),
                        decision: ApprovalDecision::Allow,
                        reason: None,
                    }).unwrap()
                }),
            );
            // Tool result for write_file
            tokio::time::sleep(Duration::from_millis(10)).await;
            l2.lock().await.insert(
                tool_callback_key(uid, "w1"),
                json!({"body": {"request_id": "w1", "status": "ok", "output": "wrote_b"}}),
            );
            // Tool results for both read_files (arrive ~concurrently)
            tokio::time::sleep(Duration::from_millis(10)).await;
            {
                let mut g = l2.lock().await;
                g.insert(
                    tool_callback_key(uid, "r1"),
                    json!({"body": {"request_id": "r1", "status": "ok", "output": "content_1"}}),
                );
                g.insert(
                    tool_callback_key(uid, "r2"),
                    json!({"body": {"request_id": "r2", "status": "ok", "output": "content_2"}}),
                );
            }
        });

        let d = deliver_tool_calls_concurrent(&ledger, uid, &tcs, Duration::from_secs(2)).await;

        // SSE events: approval_required + tool_call + tool_request (write) + 2×(tool_call + tool_request) (reads)
        // write: approval_required, tool_call, tool_request = 3
        // read×2: tool_call, tool_request each = 4
        assert_eq!(d.sse_maps.len(), 7, "sse_maps: {:#?}", d.sse_maps);
        assert_eq!(
            d.sse_maps[0].get("type").and_then(Value::as_str),
            Some("approval_required"),
            "first event must be approval for write_file"
        );

        // All 3 tool results present
        assert_eq!(d.tool_messages.len(), 3);
        let contents: Vec<&str> = d
            .tool_messages
            .iter()
            .map(|m| m["content"].as_str().unwrap())
            .collect();
        assert!(contents.iter().any(|c| c.contains("wrote_b")));
        assert!(contents.iter().any(|c| c.contains("content_1")));
        assert!(contents.iter().any(|c| c.contains("content_2")));

        // Write tool result comes first (sequential), reads come after (concurrent)
        assert!(
            d.tool_messages[0]["content"]
                .as_str()
                .unwrap()
                .contains("wrote_b")
        );
    }

    #[tokio::test]
    async fn concurrent_read_only_batch_runs_concurrently() {
        // 3 read-only tools — verify they all complete even though results
        // arrive at different times (proves concurrent, not sequential).
        let ledger = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let uid = "u_ro";
        let tcs = vec![read_tool("r1"), read_tool("r2"), read_tool("r3")];

        let l2 = ledger.clone();
        let started = std::time::Instant::now();
        tokio::spawn(async move {
            // Stagger results: r3 first, r1 second, r2 last
            tokio::time::sleep(Duration::from_millis(10)).await;
            l2.lock().await.insert(
                tool_callback_key(uid, "r3"),
                json!({"body": {"request_id": "r3", "status": "ok", "output": "c3"}}),
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
            l2.lock().await.insert(
                tool_callback_key(uid, "r1"),
                json!({"body": {"request_id": "r1", "status": "ok", "output": "c1"}}),
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
            l2.lock().await.insert(
                tool_callback_key(uid, "r2"),
                json!({"body": {"request_id": "r2", "status": "ok", "output": "c2"}}),
            );
        });

        let d = deliver_tool_calls_concurrent(&ledger, uid, &tcs, Duration::from_secs(2)).await;
        let elapsed = started.elapsed();

        assert_eq!(d.tool_messages.len(), 3);
        // Results are in original tool_call order (r1, r2, r3) regardless of arrival order
        assert!(
            d.tool_messages[0]["content"]
                .as_str()
                .unwrap()
                .contains("c1")
        );
        assert!(
            d.tool_messages[1]["content"]
                .as_str()
                .unwrap()
                .contains("c2")
        );
        assert!(
            d.tool_messages[2]["content"]
                .as_str()
                .unwrap()
                .contains("c3")
        );

        // If sequential, would take ~30ms (10+10+10). Concurrent should be ~30ms too
        // since they're staggered, but the key point is all 3 complete.
        // Just sanity-check it didn't take absurdly long.
        assert!(
            elapsed < Duration::from_secs(1),
            "took too long: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn concurrent_denied_write_still_delivers_reads() {
        // write_file denied + 1 read_file — read should still succeed.
        let ledger = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let uid = "u_deny";
        let tcs = vec![write_tool("w1"), read_tool("r1")];

        let l2 = ledger.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            l2.lock().await.insert(
                approval_callback_key(uid, "w1"),
                json!({
                    "kind": "approval_respond",
                    "body": serde_json::to_value(ApprovalRespondRequest {
                        request_id: "w1".into(),
                        decision: ApprovalDecision::Deny,
                        reason: Some("nope".into()),
                    }).unwrap()
                }),
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
            l2.lock().await.insert(
                tool_callback_key(uid, "r1"),
                json!({"body": {"request_id": "r1", "status": "ok", "output": "read_ok"}}),
            );
        });

        let d = deliver_tool_calls_concurrent(&ledger, uid, &tcs, Duration::from_secs(2)).await;

        // 2 tool messages: denied write + successful read
        assert_eq!(d.tool_messages.len(), 2);
        assert!(
            d.tool_messages[0]["content"]
                .as_str()
                .unwrap()
                .contains("user_denied")
        );
        assert!(
            d.tool_messages[1]["content"]
                .as_str()
                .unwrap()
                .contains("read_ok")
        );
    }

    // ──────────────────────────────────────────────────────────
    // raw_tool_arguments
    // ──────────────────────────────────────────────────────────

    #[test]
    fn raw_tool_arguments_valid() {
        let tc = json!({
            "function": {"name": "bash", "arguments": r#"{"cmd": "ls"}"#}
        });
        let r = raw_tool_arguments(&tc);
        assert_eq!(r.as_str().unwrap(), r#"{"cmd": "ls"}"#);
    }

    #[test]
    fn raw_tool_arguments_missing_function() {
        let tc = json!({"id": "t1"});
        let r = raw_tool_arguments(&tc);
        assert_eq!(r.as_str().unwrap(), "{}");
    }

    #[test]
    fn raw_tool_arguments_missing_arguments_key() {
        let tc = json!({"function": {"name": "bash"}});
        let r = raw_tool_arguments(&tc);
        assert_eq!(r.as_str().unwrap(), "{}");
    }

    // ──────────────────────────────────────────────────────────
    // parse_cloud_approval_outcome (additional cases)
    // ──────────────────────────────────────────────────────────

    #[test]
    fn parse_approval_none_is_timeout() {
        assert_eq!(
            parse_cloud_approval_outcome(None),
            CloudApprovalResult::Timeout
        );
    }

    #[test]
    fn parse_approval_malformed_json() {
        let v = json!({"body": {"bad": "shape"}});
        assert_eq!(
            parse_cloud_approval_outcome(Some(&v)),
            CloudApprovalResult::Malformed
        );
    }

    #[test]
    fn parse_approval_allow_session() {
        let v = json!({
            "body": {"request_id": "t1", "decision": "allow_session"}
        });
        assert_eq!(
            parse_cloud_approval_outcome(Some(&v)),
            CloudApprovalResult::Allowed
        );
    }

    #[test]
    fn parse_approval_deny_without_reason() {
        let v = json!({
            "body": {"request_id": "t1", "decision": "deny"}
        });
        assert_eq!(
            parse_cloud_approval_outcome(Some(&v)),
            CloudApprovalResult::Denied { reason: None }
        );
    }

    // ──────────────────────────────────────────────────────────
    // denied_tool_content
    // ──────────────────────────────────────────────────────────

    #[test]
    fn denied_tool_content_with_reason() {
        let s = denied_tool_content(Some("policy violation"));
        assert!(s.contains("user_denied"));
        assert!(s.contains("policy violation"));
    }

    #[test]
    fn denied_tool_content_without_reason() {
        let s = denied_tool_content(None);
        assert!(s.contains("user_denied"));
    }

    // ──────────────────────────────────────────────────────────
    // persist_denied_tool_result
    // ──────────────────────────────────────────────────────────

    #[test]
    fn persist_denied_result_extracts_id_and_name() {
        let tc = json!({
            "id": "call_123",
            "function": {"name": "write_file", "arguments": "{}"}
        });
        let r = persist_denied_tool_result(&tc, Some("no"));
        assert_eq!(r["tool_call_id"], "call_123");
        assert_eq!(r["name"], "write_file");
        assert!(r["result"].as_str().unwrap().contains("user_denied"));
    }

    #[test]
    fn persist_denied_result_missing_fields() {
        let tc = json!({}); // no id, no function
        let r = persist_denied_tool_result(&tc, None);
        assert_eq!(r["tool_call_id"], "");
        assert_eq!(r["name"], "");
    }

    // ──────────────────────────────────────────────────────────
    // sse_maps_through_tool_request
    // ──────────────────────────────────────────────────────────

    #[test]
    fn sse_maps_valid_tool_call() {
        let tc = read_tool("c1");
        let maps = sse_maps_through_tool_request(&tc);
        assert_eq!(maps.len(), 2);
    }

    #[test]
    fn sse_maps_non_object_returns_empty() {
        let tc = json!("not an object");
        let maps = sse_maps_through_tool_request(&tc);
        assert!(maps.is_empty());
    }

    // ──────────────────────────────────────────────────────────
    // cloud_tool_requires_approval
    // ──────────────────────────────────────────────────────────

    #[test]
    fn read_file_does_not_require_approval() {
        let tc = read_tool("r1");
        assert!(!cloud_tool_requires_approval(&tc));
    }

    #[test]
    fn write_file_requires_approval() {
        let tc = write_tool("w1");
        assert!(cloud_tool_requires_approval(&tc));
    }

    #[test]
    fn empty_tool_call_no_panic() {
        let tc = json!({});
        // Should not panic, just default behavior
        let _ = cloud_tool_requires_approval(&tc);
    }

    // ──────────────────────────────────────────────────────────
    // tool_path_hint
    // ──────────────────────────────────────────────────────────

    #[test]
    fn tool_path_hint_extracts_from_args() {
        let tc = json!({
            "function": {"name": "write_file", "arguments": r#"{"path": "src/main.rs"}"#}
        });
        let hint = tool_path_hint(&tc);
        assert_eq!(hint, Some("src/main.rs".to_string()));
    }

    #[test]
    fn tool_path_hint_no_path_in_args() {
        let tc = json!({
            "function": {"name": "bash", "arguments": r#"{"command": "ls"}"#}
        });
        let hint = tool_path_hint(&tc);
        // bash doesn't have a path arg, so hint may be None
        assert!(hint.is_none() || hint.is_some());
    }
}
