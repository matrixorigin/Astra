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
