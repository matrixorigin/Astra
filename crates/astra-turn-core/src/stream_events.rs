use serde_json::{Map, Value};

use astra_services::multi_agent::EdgeDispatchIdentity;
use astra_thin_client::ApprovalKind;

pub fn build_stream_error_event(message: &str, code: &str, retryable: bool) -> Map<String, Value> {
    let mut event = Map::from_iter([
        ("type".to_string(), Value::String("error".to_string())),
        ("message".to_string(), Value::String(message.to_string())),
        ("code".to_string(), Value::String(code.to_string())),
        ("retryable".to_string(), Value::Bool(retryable)),
    ]);
    if let Some(kind) = astra_core::ErrorKind::parse_tag(code) {
        event.insert(
            "error_kind".to_string(),
            Value::String(kind.as_str().to_string()),
        );
    }
    event
}

/// Build an SSE error while preserving both the product/protocol `code` and
/// the producer's typed runtime classification.
///
/// A code such as `inference_ledger` identifies the failing boundary; it is
/// deliberately not required to duplicate [`astra_core::ErrorKind`].
pub fn build_classified_stream_error_event(
    error: &astra_core::ClassifiedError,
    code: &str,
    retryable: bool,
) -> Map<String, Value> {
    let mut event = build_stream_error_event(&error.message, code, retryable);
    event.insert(
        "error_kind".to_string(),
        Value::String(error.kind.as_str().to_string()),
    );
    event
}

/// Configuration for a single error kind: the SSE error code, whether it is retryable,
/// and the retry-after delay (if any).
struct ErrorKindConfig {
    code: &'static str,
    retryable: bool,
    retry_after_ms: Option<i64>,
    /// When set, this fixed message overrides the caller-provided message.
    fixed_message: Option<&'static str>,
}

const ERROR_KIND_TABLE: &[(&[&str], ErrorKindConfig)] = &[
    (
        &["permission"],
        ErrorKindConfig {
            code: "MODEL_NOT_AVAILABLE",
            retryable: false,
            retry_after_ms: None,
            fixed_message: None,
        },
    ),
    (
        &["budget", "budget_exhausted"],
        ErrorKindConfig {
            code: "BUDGET_EXCEEDED",
            retryable: false,
            retry_after_ms: None,
            fixed_message: None,
        },
    ),
    (
        &["auth"],
        ErrorKindConfig {
            code: "AUTH_ERROR",
            retryable: false,
            retry_after_ms: None,
            fixed_message: None,
        },
    ),
    (
        &["rate_limit"],
        ErrorKindConfig {
            code: "LLM_RATE_LIMIT",
            retryable: true,
            retry_after_ms: Some(5000),
            fixed_message: None,
        },
    ),
    (
        &["timeout", "stream_idle", "tool_timeout"],
        ErrorKindConfig {
            code: "LLM_TIMEOUT",
            retryable: true,
            retry_after_ms: Some(2000),
            fixed_message: None,
        },
    ),
    (
        &["server", "server_error"],
        ErrorKindConfig {
            code: "SERVER_ERROR",
            retryable: true,
            retry_after_ms: Some(1000),
            fixed_message: None,
        },
    ),
    (
        &["transport", "stream_transport", "network"],
        ErrorKindConfig {
            code: "LLM_TRANSPORT_ERROR",
            retryable: true,
            retry_after_ms: Some(2000),
            fixed_message: Some("LLM provider connection failed. Please retry."),
        },
    ),
    (
        &["context_window"],
        ErrorKindConfig {
            code: "CONTEXT_WINDOW_EXCEEDED",
            retryable: false,
            retry_after_ms: None,
            fixed_message: None,
        },
    ),
    (
        &["invalid_request"],
        ErrorKindConfig {
            code: "LLM_INVALID_REQUEST",
            retryable: false,
            retry_after_ms: None,
            fixed_message: None,
        },
    ),
];

fn lookup_error_kind(kind: &str) -> ErrorKindConfig {
    for (aliases, config) in ERROR_KIND_TABLE {
        if aliases.contains(&kind) {
            return ErrorKindConfig {
                fixed_message: config.fixed_message,
                ..*config
            };
        }
    }
    ErrorKindConfig {
        code: "INTERNAL_ERROR",
        retryable: false,
        retry_after_ms: None,
        fixed_message: None,
    }
}

pub fn build_runtime_error_event(
    message: Value,
    error_kind: Option<&str>,
    http_status_code: Option<u16>,
    http_detail: Option<Value>,
) -> Map<String, Value> {
    if let Some(status_code) = http_status_code {
        let code = match status_code {
            401 => "AUTH_ERROR",
            403 => "AUTH_ERROR",
            404 => "NOT_FOUND",
            422 => "VALIDATION_ERROR",
            _ => "INTERNAL_ERROR",
        };
        if let Some(detail) = http_detail {
            // HTTP detail overrides the message with the full response body
            // so clients can surface structured error information.
            Map::from_iter([
                ("type".to_string(), Value::String("error".to_string())),
                ("message".to_string(), detail),
                ("code".to_string(), Value::String(code.to_string())),
                ("retryable".to_string(), Value::Bool(false)),
            ])
        } else {
            build_stream_error_event(message.as_str().unwrap_or_default(), code, false)
        }
    } else {
        let cfg = lookup_error_kind(error_kind.unwrap_or("internal"));
        let msg = cfg
            .fixed_message
            .unwrap_or_else(|| message.as_str().unwrap_or_default());
        let mut ev = build_stream_error_event(msg, cfg.code, cfg.retryable);
        if let Some(retry_after) = cfg.retry_after_ms {
            ev.insert("retry_after_ms".to_string(), Value::from(retry_after));
        }
        ev
    }
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
        invalid_tool_arguments("truncated")
    } else {
        let raw_arguments = tool_call
            .get("function")
            .and_then(Value::as_object)
            .and_then(|function| function.get("arguments"))
            .cloned()
            .unwrap_or_else(|| Value::String("{}".to_string()));
        match raw_arguments {
            Value::String(text) => match serde_json::from_str::<Value>(&text) {
                Ok(arguments) => arguments,
                Err(error) => invalid_json_tool_arguments(&text, &error),
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

/// A malformed tool call is data about a failed model-to-runtime boundary,
/// never a best-effort request to execute. Keep the evidence structured and
/// omit the raw arguments: they can be large, secret-bearing, or themselves
/// confusing prompt material on the next model turn.
fn invalid_tool_arguments(kind: &str) -> Value {
    Value::Object(Map::from_iter([(
        "_parse_error".to_string(),
        serde_json::json!({
            "kind": kind,
            "executed": false,
        }),
    )]))
}

fn invalid_json_tool_arguments(text: &str, error: &serde_json::Error) -> Value {
    let category = match error.classify() {
        serde_json::error::Category::Io => "io",
        serde_json::error::Category::Syntax => "syntax",
        serde_json::error::Category::Data => "data",
        serde_json::error::Category::Eof => "eof",
    };
    Value::Object(Map::from_iter([(
        "_parse_error".to_string(),
        serde_json::json!({
            "kind": "invalid_json",
            "executed": false,
            "category": category,
            "argument_bytes": text.len(),
            "line": error.line(),
            "column": error.column(),
        }),
    )]))
}

/// §5.5 `approval_required` — `request_id` matches `POST /approval/respond` ledger keys.
///
/// Two preview fields travel together:
/// - `detail`: RAW command/path string. Used by downstream classifiers
///   (`ApprovalFingerprint::shell`, `bash_command_approval_reason`) to
///   do `starts_with` matching against permission rules. Must stay
///   raw — prepending `"$ "` would silently bypass deny rules like
///   `bash(rm -rf:*)`.
/// - `display_label`: RICH preview ("$ ls -la", "Writing: foo.rs").
///   Used by the UI for the approval dialog header so cloud-gated
///   approvals read the same as local ones.
///
/// Callers that don't care about the display label can pass `None`;
/// the client will fall back to `detail` for display, matching the
/// pre-split behaviour.
pub fn build_approval_required_event(
    request_id: &str,
    tool_name: &str,
    approval_kind: ApprovalKind,
    path: Option<&str>,
    detail: Option<&str>,
    display_label: Option<&str>,
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
    if let Some(lbl) = display_label.filter(|s| !s.is_empty()) {
        m.insert("display_label".to_string(), Value::String(lbl.to_string()));
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
    /// See [`build_approval_required_event`] for the raw-vs-display split.
    pub display_label: Option<&'a str>,
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
            if let Some(lbl) = request.display_label.filter(|s| !s.is_empty()) {
                item.insert("display_label".to_string(), Value::String(lbl.to_string()));
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

/// §5.5 thin-client `tool_request` — identity matches `POST /tools/result` and ledger keys.
pub fn build_tool_request_event(
    tool_call: &Map<String, Value>,
    identity: &EdgeDispatchIdentity,
    execution_timeout_ms: u64,
    execution_deadline_unix_ms: u64,
) -> Map<String, Value> {
    let edge = build_edge_tool_call_event(tool_call);
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
        (
            "session_id".to_string(),
            Value::String(identity.session_id.clone()),
        ),
        ("run_id".to_string(), Value::String(identity.run_id.clone())),
        (
            "turn_chain_id".to_string(),
            Value::String(identity.turn_chain_id.clone()),
        ),
        (
            "request_id".to_string(),
            Value::String(identity.request_id.clone()),
        ),
        // The Server emits tool_request only after checking the exact tool
        // against the wire-visible schema for this run/turn.  Edge executors
        // consume this typed fact instead of independently reconstructing
        // deferred activation from a different local prompt surface.
        ("schema_admitted_by_server".to_string(), Value::Bool(true)),
        // This is an execution authority issued by the server after policy
        // admission. Edge must not extend it from a locally inferred default.
        (
            "execution_timeout_ms".to_string(),
            Value::from(execution_timeout_ms),
        ),
        (
            "execution_deadline_unix_ms".to_string(),
            Value::from(execution_deadline_unix_ms),
        ),
        ("tool".to_string(), tool),
        ("args".to_string(), args),
    ])
}

fn normalized_result_status(result: &Value) -> Option<String> {
    // Case 1: result is a JSON object with an explicit "status" field
    if let Some(status) = result
        .as_object()
        .and_then(|obj| obj.get("status"))
        .and_then(Value::as_str)
    {
        return Some(status.trim().to_ascii_lowercase());
    }

    // Case 2: result is a JSON string — try parsing it
    let text = result.as_str()?.trim();
    if let Ok(parsed) = serde_json::from_str::<Value>(text)
        && let Some(obj) = parsed.as_object()
    {
        if let Some(status) = obj.get("status").and_then(Value::as_str) {
            return Some(status.trim().to_ascii_lowercase());
        }
        if obj.get("error").is_some() {
            return Some("failed".to_string());
        }
    }
    None
}

fn result_status_is_success(status: &str) -> bool {
    status == "completed"
}

fn canonical_result_status(status: &str) -> String {
    match status.trim().to_ascii_lowercase().as_str() {
        "completed" => "completed".to_string(),
        "failed" => "failed".to_string(),
        "skipped" => "skipped".to_string(),
        "rejected" => "rejected".to_string(),
        _ => "failed".to_string(),
    }
}

pub fn build_tool_call_end_event(call_id: &str, result: Value) -> Map<String, Value> {
    let status = normalized_result_status(&result);
    let mut event = Map::from_iter([
        (
            "type".to_string(),
            Value::String("tool_call_end".to_string()),
        ),
        ("call_id".to_string(), Value::String(call_id.to_string())),
        ("result".to_string(), result),
    ]);
    if let Some(status) = status {
        let status = canonical_result_status(&status);
        event.insert("status".to_string(), Value::String(status.clone()));
        event.insert(
            "success".to_string(),
            Value::Bool(result_status_is_success(&status)),
        );
        if status == "skipped" {
            event.insert("skipped".to_string(), Value::Bool(true));
        }
    }
    event
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
            None,
        );
        assert_eq!(ev.get("detail").and_then(Value::as_str), Some("git status"));
    }

    #[test]
    fn approval_required_carries_display_label() {
        let ev = build_approval_required_event(
            "a1",
            "bash",
            ApprovalKind::Explicit,
            None,
            Some("ls -la"),
            Some("$ ls -la"),
        );
        assert_eq!(ev.get("detail").and_then(Value::as_str), Some("ls -la"));
        assert_eq!(
            ev.get("display_label").and_then(Value::as_str),
            Some("$ ls -la")
        );
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
                display_label: None,
            },
            ApprovalBatchRequestEvent {
                request_id: "a2",
                tool_name: "write_file",
                approval_kind: ApprovalKind::Standard,
                path: Some("src/b.rs"),
                detail: Some("src/b.rs"),
                display_label: None,
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
        let identity = EdgeDispatchIdentity::new("u1", "s1", "r1", "chain1", "call_abc");
        let ev = build_tool_request_event(&tc, &identity, 300_000, 1_700_000_300_000);
        assert_eq!(ev.get("type").and_then(Value::as_str), Some("tool_request"));
        assert_eq!(ev.get("session_id").and_then(Value::as_str), Some("s1"));
        assert_eq!(ev.get("run_id").and_then(Value::as_str), Some("r1"));
        assert_eq!(
            ev.get("turn_chain_id").and_then(Value::as_str),
            Some("chain1")
        );
        assert_eq!(
            ev.get("request_id").and_then(Value::as_str),
            Some("call_abc")
        );
        assert_eq!(
            ev.get("schema_admitted_by_server").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            ev.get("execution_timeout_ms").and_then(Value::as_u64),
            Some(300_000)
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

    #[test]
    fn tool_call_end_event_projects_status_from_result_body() {
        let skipped = build_tool_call_end_event(
            "call_skip",
            Value::String(r#"{"status":"skipped","message":"Duplicate call skipped"}"#.to_string()),
        );
        assert_eq!(
            skipped.get("status").and_then(Value::as_str),
            Some("skipped")
        );
        assert_eq!(skipped.get("success").and_then(Value::as_bool), Some(false));
        assert_eq!(skipped.get("skipped").and_then(Value::as_bool), Some(true));

        let rejected = build_tool_call_end_event(
            "call_rejected",
            Value::String(r#"{"status":"rejected","message":"not admitted"}"#.to_string()),
        );
        assert_eq!(
            rejected.get("status").and_then(Value::as_str),
            Some("rejected")
        );
        assert_eq!(
            rejected.get("success").and_then(Value::as_bool),
            Some(false)
        );

        let denied = build_tool_call_end_event(
            "call_deny",
            Value::String(r#"{"error":"user_denied","reason":"policy"}"#.to_string()),
        );
        assert_eq!(denied.get("status").and_then(Value::as_str), Some("failed"));
        assert_eq!(denied.get("success").and_then(Value::as_bool), Some(false));
    }

    #[test]
    fn tool_call_end_event_rejects_legacy_success_alias() {
        let ev = build_tool_call_end_event(
            "call_alias",
            Value::String(r#"{"status":"success","message":"old alias"}"#.to_string()),
        );
        assert_eq!(ev.get("status").and_then(Value::as_str), Some("failed"));
        assert_eq!(ev.get("success").and_then(Value::as_bool), Some(false));
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
    fn stream_error_event_includes_structured_error_kind_for_known_code() {
        let ev = build_stream_error_event("select a model", "missing_model_selection", false);

        assert_eq!(
            ev.get("error_kind").and_then(Value::as_str),
            Some("missing_model_selection")
        );
    }

    #[test]
    fn classified_stream_error_keeps_boundary_code_and_typed_kind_distinct() {
        let error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ContractViolation,
            "duplicate inference identity",
        );
        let ev = build_classified_stream_error_event(&error, "inference_ledger", false);

        assert_eq!(
            ev.get("code").and_then(Value::as_str),
            Some("inference_ledger")
        );
        assert_eq!(
            ev.get("error_kind").and_then(Value::as_str),
            Some("contract_violation")
        );
        assert_eq!(ev.get("retryable").and_then(Value::as_bool), Some(false));
    }

    #[test]
    fn classified_stream_error_survives_sse_decode_and_host_snapshot() {
        let error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ContractViolation,
            "duplicate inference identity",
        );
        let event = Value::Object(build_classified_stream_error_event(
            &error,
            "inference_ledger",
            false,
        ));
        let block = format!("data: {event}\n\n");
        let mut accum = crate::chat_turn_sse_dispatch::ChatTurnSseAccum::default();
        let mut edge_pending = Vec::new();

        crate::chat_turn_sse_dispatch::dispatch_chat_turn_sse_event_block(
            &block,
            &mut accum,
            &mut edge_pending,
        );
        let snapshot =
            crate::agentic::turn_ingest::agentic_turn_stream_snapshot_with_kind(&accum, None, None);

        assert_eq!(
            snapshot.error_kind,
            Some(astra_core::ErrorKind::ContractViolation)
        );
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

        let ev = build_runtime_error_event(
            Value::String("exceeded".into()),
            Some("budget_exhausted"),
            None,
            None,
        );
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

        let ev = build_runtime_error_event(
            Value::String("ignored".into()),
            Some("stream_transport"),
            None,
            None,
        );
        assert_eq!(
            ev.get("code").and_then(Value::as_str),
            Some("LLM_TRANSPORT_ERROR")
        );
    }

    #[test]
    fn runtime_error_kind_server_has_retry_after() {
        let ev = build_runtime_error_event(Value::String("500".into()), Some("server"), None, None);
        assert_eq!(ev.get("code").and_then(Value::as_str), Some("SERVER_ERROR"));
        assert_eq!(ev.get("retry_after_ms").and_then(Value::as_i64), Some(1000));

        let ev = build_runtime_error_event(
            Value::String("500".into()),
            Some("server_error"),
            None,
            None,
        );
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
        assert_eq!(args["_parse_error"]["kind"], "truncated");
        assert_eq!(args["_parse_error"]["executed"], false);
    }

    #[test]
    fn edge_tool_call_invalid_json_is_unexecuted_structured_evidence() {
        let raw = r#"{/"action/": /"start/", /"target_count/": 3}"#;
        let tc = Map::from_iter([
            ("id".to_string(), Value::String("c1".into())),
            (
                "function".to_string(),
                json!({"name": "agent_fanout", "arguments": raw}),
            ),
        ]);

        let ev = build_edge_tool_call_event(&tc);
        let parse_error = &ev["arguments"]["_parse_error"];
        assert_eq!(parse_error["kind"], "invalid_json");
        assert_eq!(parse_error["executed"], false);
        assert_eq!(parse_error["category"], "syntax");
        assert_eq!(parse_error["argument_bytes"], raw.len());
        assert!(parse_error["column"].as_u64().is_some());
        assert_ne!(ev["arguments"], Value::String(raw.to_string()));
    }

    #[test]
    fn edge_tool_call_incomplete_json_records_eof_without_raw_arguments() {
        let raw = r#"{"action":"start","slots":["#;
        let tc = Map::from_iter([
            ("id".to_string(), Value::String("c1".into())),
            (
                "function".to_string(),
                json!({"name": "agent_fanout", "arguments": raw}),
            ),
        ]);

        let ev = build_edge_tool_call_event(&tc);
        let parse_error = &ev["arguments"]["_parse_error"];
        assert_eq!(parse_error["category"], "eof");
        assert_eq!(parse_error["argument_bytes"], raw.len());
        assert!(!Value::Object(ev).to_string().contains(raw));
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
        let ev = build_approval_required_event(
            "r1",
            "bash",
            ApprovalKind::Explicit,
            Some(""),
            Some(""),
            Some(""),
        );
        assert!(ev.get("path").is_none());
        assert!(ev.get("detail").is_none());
        assert!(ev.get("display_label").is_none());
    }

    #[test]
    fn approval_required_omits_none_path() {
        let ev =
            build_approval_required_event("r1", "bash", ApprovalKind::Explicit, None, None, None);
        assert!(ev.get("path").is_none());
        assert!(ev.get("detail").is_none());
        assert!(ev.get("display_label").is_none());
    }

    #[test]
    fn tool_request_missing_id_empty_request_id() {
        let tc = Map::from_iter([(
            "function".to_string(),
            json!({"name": "read_file", "arguments": "{}"}),
        )]);
        let identity = EdgeDispatchIdentity::new("u1", "s1", "r1", "chain1", "call_missing");
        let ev = build_tool_request_event(&tc, &identity, 300_000, 1_700_000_300_000);
        assert_eq!(
            ev.get("request_id").and_then(Value::as_str),
            Some("call_missing")
        );
    }
}
