use serde_json::{Map, Value};

use crate::try_repair_tool_args;

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
    path: Option<&str>,
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
    ]);
    if let Some(p) = path.filter(|s| !s.is_empty()) {
        m.insert("path".to_string(), Value::String(p.to_string()));
    }
    m
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn approval_required_includes_optional_path() {
        let ev = build_approval_required_event("a1", "write_file", Some("p/x.rs"));
        assert_eq!(
            ev.get("type").and_then(Value::as_str),
            Some("approval_required")
        );
        assert_eq!(ev.get("request_id").and_then(Value::as_str), Some("a1"));
        assert_eq!(ev.get("tool").and_then(Value::as_str), Some("write_file"));
        assert_eq!(ev.get("path").and_then(Value::as_str), Some("p/x.rs"));
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
}
