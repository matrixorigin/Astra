use serde_json::{Map, Value};

use astra_thin_client::ApprovalKind;

use crate::tool_args_repair::try_repair_tool_args;

pub fn build_stream_error_event(message: &str, code: &str, retryable: bool) -> Map<String, Value> {
    Map::from_iter([
        ("type".to_string(), Value::String("error".to_string())),
        ("message".to_string(), Value::String(message.to_string())),
        ("code".to_string(), Value::String(code.to_string())),
        ("retryable".to_string(), Value::Bool(retryable)),
    ])
}

pub fn build_runtime_error_event(
    message: Value,
    error_kind: Option<&str>,
    http_status_code: Option<u16>,
    http_detail: Option<Value>,
) -> Map<String, Value> {
    let mut event = if let Some(status_code) = http_status_code {
        let code = match status_code {
            401 => "AUTH_ERROR",
            403 => "AUTH_ERROR",
            404 => "NOT_FOUND",
            422 => "VALIDATION_ERROR",
            _ => "INTERNAL_ERROR",
        };
        let detail = http_detail.unwrap_or(message);
        Map::from_iter([
            ("type".to_string(), Value::String("error".to_string())),
            ("message".to_string(), detail),
            ("code".to_string(), Value::String(code.to_string())),
            ("retryable".to_string(), Value::Bool(false)),
        ])
    } else {
        match error_kind.unwrap_or("internal") {
            "permission" => build_stream_error_event(
                message.as_str().unwrap_or_default(),
                "MODEL_NOT_AVAILABLE",
                false,
            ),
            "budget" => build_stream_error_event(
                message.as_str().unwrap_or_default(),
                "BUDGET_EXCEEDED",
                false,
            ),
            "rate_limit" => build_stream_error_event(
                message.as_str().unwrap_or_default(),
                "LLM_RATE_LIMIT",
                true,
            ),
            "timeout" => {
                build_stream_error_event(message.as_str().unwrap_or_default(), "LLM_TIMEOUT", true)
            }
            "server" => {
                build_stream_error_event(message.as_str().unwrap_or_default(), "SERVER_ERROR", true)
            }
            "transport" => build_stream_error_event(
                "LLM provider connection failed. Please retry.",
                "LLM_TRANSPORT_ERROR",
                true,
            ),
            _ => build_stream_error_event(
                message.as_str().unwrap_or_default(),
                "INTERNAL_ERROR",
                false,
            ),
        }
    };

    match error_kind {
        Some("rate_limit") => {
            event.insert("retry_after_ms".to_string(), Value::from(5000));
        }
        Some("timeout") | Some("transport") => {
            event.insert("retry_after_ms".to_string(), Value::from(2000));
        }
        Some("server") => {
            event.insert("retry_after_ms".to_string(), Value::from(1000));
        }
        _ => {}
    }
    event
}

pub fn build_firewall_warning_event(claims_failed: i64) -> Map<String, Value> {
    Map::from_iter([
        ("type".to_string(), Value::String("warning".to_string())),
        (
            "message".to_string(),
            Value::String("Response may contain unverified claims".to_string()),
        ),
        ("claims_failed".to_string(), Value::from(claims_failed)),
    ])
}

pub fn build_edge_tool_call_event(tool_call: &Map<String, Value>) -> Map<String, Value> {
    let tool_name = tool_call
        .get("function")
        .and_then(Value::as_object)
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("?");

    let arguments = if tool_call
        .get("_truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        Value::Object(Map::from_iter([(
            "_parse_error".to_string(),
            Value::String(
                "Your output was truncated by max_tokens before the tool_call arguments were complete. The JSON is cut off and cannot be parsed. Please retry with a shorter approach — for example, write smaller sections of code at a time instead of the entire file at once.".to_string(),
            ),
        )]))
    } else {
        let raw_arguments = tool_call
            .get("function")
            .and_then(Value::as_object)
            .and_then(|function| function.get("arguments"))
            .cloned()
            .unwrap_or_else(|| Value::String("{}".to_string()));
        match raw_arguments {
            Value::String(text) => match serde_json::from_str::<Value>(&text) {
                Ok(value) => value,
                Err(_) => try_repair_tool_args(tool_name, &text)
                    .map(Value::Object)
                    .unwrap_or_else(|| {
                        Value::Object(Map::from_iter([(
                            "_parse_error".to_string(),
                            Value::String(format!(
                                "Malformed arguments JSON: {}",
                                text.chars().take(200).collect::<String>()
                            )),
                        )]))
                    }),
            },
            other => other,
        }
    };

    Map::from_iter([
        ("type".to_string(), Value::String("tool_call".to_string())),
        (
            "id".to_string(),
            tool_call
                .get("id")
                .cloned()
                .unwrap_or_else(|| Value::String(String::new())),
        ),
        ("name".to_string(), Value::String(tool_name.to_string())),
        ("arguments".to_string(), arguments),
    ])
}

/// §5.5 `approval_required` — `request_id` matches `POST /approval/respond` ledger keys.
pub fn build_approval_required_event(
    request_id: &str,
    tool_name: &str,
    approval_kind: ApprovalKind,
    path: Option<&str>,
    detail: Option<&str>,
) -> Map<String, Value> {
    let mut m = Map::from_iter([
        (
            "type".to_string(),
            Value::String("approval_required".to_string()),
        ),
        (
            "request_id".to_string(),
            Value::String(request_id.to_string()),
        ),
        ("tool".to_string(), Value::String(tool_name.to_string())),
        (
            "approval_kind".to_string(),
            serde_json::to_value(approval_kind).unwrap_or(Value::String("explicit".to_string())),
        ),
    ]);
    if let Some(p) = path.filter(|s| !s.is_empty()) {
        m.insert("path".to_string(), Value::String(p.to_string()));
    }
    if let Some(d) = detail.filter(|s| !s.is_empty()) {
        m.insert("detail".to_string(), Value::String(d.to_string()));
    }
    m
}

#[derive(Debug, Clone, Copy)]
pub struct ApprovalBatchRequestEvent<'a> {
    pub request_id: &'a str,
    pub tool_name: &'a str,
    pub approval_kind: ApprovalKind,
    pub path: Option<&'a str>,
    pub detail: Option<&'a str>,
}

/// Batch approval request for multiple gated tools in the same round.
pub fn build_approval_batch_required_event(
    requests: &[ApprovalBatchRequestEvent<'_>],
) -> Map<String, Value> {
    let payload = requests
        .iter()
        .map(|request| {
            let mut item = Map::from_iter([
                (
                    "request_id".to_string(),
                    Value::String(request.request_id.to_string()),
                ),
                (
                    "tool".to_string(),
                    Value::String(request.tool_name.to_string()),
                ),
                (
                    "approval_kind".to_string(),
                    serde_json::to_value(request.approval_kind)
                        .unwrap_or(Value::String("explicit".to_string())),
                ),
            ]);
            if let Some(path) = request.path.filter(|s| !s.is_empty()) {
                item.insert("path".to_string(), Value::String(path.to_string()));
            }
            if let Some(detail) = request.detail.filter(|s| !s.is_empty()) {
                item.insert("detail".to_string(), Value::String(detail.to_string()));
            }
            Value::Object(item)
        })
        .collect::<Vec<_>>();
    Map::from_iter([
        (
            "type".to_string(),
            Value::String("approval_batch_required".to_string()),
        ),
        ("requests".to_string(), Value::Array(payload)),
    ])
}

/// §5.5 thin-client `tool_request` — `request_id` matches `POST /tools/result` and ledger keys.
pub fn build_tool_request_event(tool_call: &Map<String, Value>) -> Map<String, Value> {
    let edge = build_edge_tool_call_event(tool_call);
    let request_id = tool_call
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let tool = edge
        .get("name")
        .cloned()
        .unwrap_or_else(|| Value::String("?".to_string()));
    let args = edge.get("arguments").cloned().unwrap_or(Value::Null);
    Map::from_iter([
        (
            "type".to_string(),
            Value::String("tool_request".to_string()),
        ),
        ("request_id".to_string(), Value::String(request_id)),
        ("tool".to_string(), tool),
        ("args".to_string(), args),
    ])
}

pub fn build_tool_call_end_event(call_id: &str, result: Value) -> Map<String, Value> {
    Map::from_iter([
        (
            "type".to_string(),
            Value::String("tool_call_end".to_string()),
        ),
        ("call_id".to_string(), Value::String(call_id.to_string())),
        ("result".to_string(), result),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn approval_required_includes_optional_path() {
        let ev = build_approval_required_event(
            "a1",
            "write_file",
            ApprovalKind::Standard,
            Some("p/x.rs"),
            None,
        );
        assert_eq!(
            ev.get("type").and_then(Value::as_str),
            Some("approval_required")
        );
        assert_eq!(ev.get("request_id").and_then(Value::as_str), Some("a1"));
        assert_eq!(ev.get("tool").and_then(Value::as_str), Some("write_file"));
        assert_eq!(
            ev.get("approval_kind").and_then(Value::as_str),
            Some("standard")
        );
        assert_eq!(ev.get("path").and_then(Value::as_str), Some("p/x.rs"));
    }

    #[test]
    fn approval_required_includes_optional_detail() {
        let ev = build_approval_required_event(
            "a1",
            "bash",
            ApprovalKind::Explicit,
            None,
            Some("git status"),
        );
        assert_eq!(ev.get("detail").and_then(Value::as_str), Some("git status"));
    }

    #[test]
    fn approval_batch_required_contains_requests() {
        let ev = build_approval_batch_required_event(&[
            ApprovalBatchRequestEvent {
                request_id: "a1",
                tool_name: "write_file",
                approval_kind: ApprovalKind::Standard,
                path: Some("src/a.rs"),
                detail: Some("src/a.rs"),
            },
            ApprovalBatchRequestEvent {
                request_id: "a2",
                tool_name: "write_file",
                approval_kind: ApprovalKind::Standard,
                path: Some("src/b.rs"),
                detail: Some("src/b.rs"),
            },
        ]);
        assert_eq!(
            ev.get("type").and_then(Value::as_str),
            Some("approval_batch_required")
        );
        let requests = ev
            .get("requests")
            .and_then(Value::as_array)
            .expect("requests array");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["request_id"].as_str(), Some("a1"));
        assert_eq!(requests[1]["detail"].as_str(), Some("src/b.rs"));
    }

    #[test]
    fn tool_request_event_aligns_request_id_with_tool_call_id() {
        let tc = Map::from_iter([
            ("id".to_string(), Value::String("call_abc".to_string())),
            (
                "function".to_string(),
                json!({"name": "bash", "arguments": "{\"cmd\": \"ls\"}"}),
            ),
        ]);
        let ev = build_tool_request_event(&tc);
        assert_eq!(ev.get("type").and_then(Value::as_str), Some("tool_request"));
        assert_eq!(
            ev.get("request_id").and_then(Value::as_str),
            Some("call_abc")
        );
        assert_eq!(ev.get("tool").and_then(Value::as_str), Some("bash"));
    }

    #[test]
    fn tool_call_end_event_preserves_call_id_and_result() {
        let ev = build_tool_call_end_event("call_abc", Value::String("ok".to_string()));
        assert_eq!(
            ev.get("type").and_then(Value::as_str),
            Some("tool_call_end")
        );
        assert_eq!(ev.get("call_id").and_then(Value::as_str), Some("call_abc"));
        assert_eq!(ev.get("result").and_then(Value::as_str), Some("ok"));
    }

    // --- edge cases ---

    #[test]
    fn stream_error_event_fields() {
        let ev = build_stream_error_event("boom", "INTERNAL_ERROR", false);
        assert_eq!(ev.get("type").and_then(Value::as_str), Some("error"));
        assert_eq!(ev.get("message").and_then(Value::as_str), Some("boom"));
        assert_eq!(
            ev.get("code").and_then(Value::as_str),
            Some("INTERNAL_ERROR")
        );
        assert_eq!(ev.get("retryable").and_then(Value::as_bool), Some(false));
    }

    #[test]
    fn stream_error_retryable_true() {
        let ev = build_stream_error_event("retry me", "LLM_RATE_LIMIT", true);
        assert_eq!(ev.get("retryable").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn runtime_error_http_401_auth() {
        let ev =
            build_runtime_error_event(Value::String("unauthorized".into()), None, Some(401), None);
        assert_eq!(ev.get("code").and_then(Value::as_str), Some("AUTH_ERROR"));
    }

    #[test]
    fn runtime_error_http_403_auth() {
        let ev =
            build_runtime_error_event(Value::String("forbidden".into()), None, Some(403), None);
        assert_eq!(ev.get("code").and_then(Value::as_str), Some("AUTH_ERROR"));
    }

    #[test]
    fn runtime_error_http_404_not_found() {
        let ev =
            build_runtime_error_event(Value::String("not found".into()), None, Some(404), None);
        assert_eq!(ev.get("code").and_then(Value::as_str), Some("NOT_FOUND"));
    }

    #[test]
    fn runtime_error_http_422_validation() {
        let ev = build_runtime_error_event(Value::String("bad".into()), None, Some(422), None);
        assert_eq!(
            ev.get("code").and_then(Value::as_str),
            Some("VALIDATION_ERROR")
        );
    }

    #[test]
    fn runtime_error_http_500_internal() {
        let ev = build_runtime_error_event(Value::String("oops".into()), None, Some(500), None);
        assert_eq!(
            ev.get("code").and_then(Value::as_str),
            Some("INTERNAL_ERROR")
        );
    }

    #[test]
    fn runtime_error_http_detail_overrides_message() {
        let detail = json!({"error": "custom detail"});
        let ev = build_runtime_error_event(
            Value::String("original".into()),
            None,
            Some(401),
            Some(detail.clone()),
        );
        assert_eq!(ev.get("message"), Some(&detail));
    }

    #[test]
    fn runtime_error_kind_permission() {
        let ev = build_runtime_error_event(
            Value::String("denied".into()),
            Some("permission"),
            None,
            None,
        );
        assert_eq!(
            ev.get("code").and_then(Value::as_str),
            Some("MODEL_NOT_AVAILABLE")
        );
    }

    #[test]
    fn runtime_error_kind_budget() {
        let ev =
            build_runtime_error_event(Value::String("exceeded".into()), Some("budget"), None, None);
        assert_eq!(
            ev.get("code").and_then(Value::as_str),
            Some("BUDGET_EXCEEDED")
        );
    }

    #[test]
    fn runtime_error_kind_rate_limit_has_retry_after() {
        let ev = build_runtime_error_event(
            Value::String("slow down".into()),
            Some("rate_limit"),
            None,
            None,
        );
        assert_eq!(
            ev.get("code").and_then(Value::as_str),
            Some("LLM_RATE_LIMIT")
        );
        assert_eq!(ev.get("retryable").and_then(Value::as_bool), Some(true));
        assert_eq!(ev.get("retry_after_ms").and_then(Value::as_i64), Some(5000));
    }

    #[test]
    fn runtime_error_kind_timeout_has_retry_after() {
        let ev = build_runtime_error_event(
            Value::String("timed out".into()),
            Some("timeout"),
            None,
            None,
        );
        assert_eq!(ev.get("code").and_then(Value::as_str), Some("LLM_TIMEOUT"));
        assert_eq!(ev.get("retry_after_ms").and_then(Value::as_i64), Some(2000));
    }

    #[test]
    fn runtime_error_kind_transport_fixed_message() {
        let ev = build_runtime_error_event(
            Value::String("ignored".into()),
            Some("transport"),
            None,
            None,
        );
        assert_eq!(
            ev.get("code").and_then(Value::as_str),
            Some("LLM_TRANSPORT_ERROR")
        );
        // Transport uses a fixed message, ignoring the input
        assert!(
            ev.get("message")
                .and_then(Value::as_str)
                .unwrap()
                .contains("connection failed")
        );
        assert_eq!(ev.get("retry_after_ms").and_then(Value::as_i64), Some(2000));
    }

    #[test]
    fn runtime_error_kind_server_has_retry_after() {
        let ev = build_runtime_error_event(Value::String("500".into()), Some("server"), None, None);
        assert_eq!(ev.get("code").and_then(Value::as_str), Some("SERVER_ERROR"));
        assert_eq!(ev.get("retry_after_ms").and_then(Value::as_i64), Some(1000));
    }

    #[test]
    fn runtime_error_unknown_kind_defaults_internal() {
        let ev = build_runtime_error_event(Value::String("wat".into()), Some("banana"), None, None);
        assert_eq!(
            ev.get("code").and_then(Value::as_str),
            Some("INTERNAL_ERROR")
        );
        assert_eq!(ev.get("retryable").and_then(Value::as_bool), Some(false));
    }

    #[test]
    fn firewall_warning_event_fields() {
        let ev = build_firewall_warning_event(3);
        assert_eq!(ev.get("type").and_then(Value::as_str), Some("warning"));
        assert_eq!(ev.get("claims_failed").and_then(Value::as_i64), Some(3));
    }

    #[test]
    fn edge_tool_call_missing_function_defaults_question_mark() {
        let tc = Map::from_iter([("id".to_string(), Value::String("c1".into()))]);
        let ev = build_edge_tool_call_event(&tc);
        assert_eq!(ev.get("name").and_then(Value::as_str), Some("?"));
    }

    #[test]
    fn edge_tool_call_truncated_gives_parse_error() {
        let tc = Map::from_iter([
            ("id".to_string(), Value::String("c1".into())),
            ("_truncated".to_string(), Value::Bool(true)),
            (
                "function".to_string(),
                json!({"name": "write_file", "arguments": "{\"path\":"}),
            ),
        ]);
        let ev = build_edge_tool_call_event(&tc);
        let args = ev.get("arguments").and_then(Value::as_object).unwrap();
        assert!(args.contains_key("_parse_error"));
    }

    #[test]
    fn edge_tool_call_valid_json_string_args() {
        let tc = Map::from_iter([
            ("id".to_string(), Value::String("c1".into())),
            (
                "function".to_string(),
                json!({"name": "bash", "arguments": "{\"command\": \"ls\"}"}),
            ),
        ]);
        let ev = build_edge_tool_call_event(&tc);
        let args = ev.get("arguments").and_then(Value::as_object).unwrap();
        assert_eq!(args.get("command").and_then(Value::as_str), Some("ls"));
    }

    #[test]
    fn approval_required_omits_empty_path() {
        let ev =
            build_approval_required_event("r1", "bash", ApprovalKind::Explicit, Some(""), Some(""));
        assert!(ev.get("path").is_none());
        assert!(ev.get("detail").is_none());
    }

    #[test]
    fn approval_required_omits_none_path() {
        let ev = build_approval_required_event("r1", "bash", ApprovalKind::Explicit, None, None);
        assert!(ev.get("path").is_none());
        assert!(ev.get("detail").is_none());
    }

    #[test]
    fn tool_request_missing_id_empty_request_id() {
        let tc = Map::from_iter([(
            "function".to_string(),
            json!({"name": "read_file", "arguments": "{}"}),
        )]);
        let ev = build_tool_request_event(&tc);
        assert_eq!(ev.get("request_id").and_then(Value::as_str), Some(""));
    }
}
