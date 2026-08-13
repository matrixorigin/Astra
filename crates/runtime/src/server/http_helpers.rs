use super::*;
use crate::server::run::handlers;
use astra_services::runs::SSE_HEARTBEAT_INTERVAL_SECS;

pub(super) fn sse_json_response(events: Vec<serde_json::Value>) -> Response {
    let body = events
        .into_iter()
        .map(|event| match serde_json::to_string(&event) {
            Ok(json) => format!("data: {json}\n\n"),
            Err(_) => {
                "data: {\"type\":\"error\",\"message\":\"serialization failed\"}\n\n".to_string()
            }
        })
        .collect::<String>();

    bridge::sse_stream_response(StatusCode::OK, Body::from(body))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn sse_error_response(status: StatusCode, message: impl Into<String>) -> Response {
    sse_error_response_with_retryable(status, message, status_to_sse_retryable(status))
}

pub(super) fn sse_error_response_with_retryable(
    status: StatusCode,
    message: impl Into<String>,
    retryable: bool,
) -> Response {
    let message = message.into();
    tracing::warn!(
        target: "astra_runtime::sse",
        http_status = status.as_u16(),
        error_code = status_to_sse_error_code(status),
        retryable,
        message = %message,
        "sse error response emitted to client",
    );
    sse_json_response(vec![serde_json::json!({
        "type": "error",
        "message": message,
        "code": status_to_sse_error_code(status),
        "retryable": retryable,
    })])
}

pub(super) fn sse_error_response_from_error(status: StatusCode, error: ErrorResponse) -> Response {
    tracing::warn!(
        target: "astra_runtime::sse",
        http_status = status.as_u16(),
        error_code = status_to_sse_error_code(status),
        retryable = status_to_sse_retryable(status),
        domain_error_code = error.error_code.as_deref().unwrap_or(""),
        message = %error.detail,
        "sse error response emitted to client",
    );
    let mut event = serde_json::json!({
        "type": "error",
        "message": error.detail,
        "code": status_to_sse_error_code(status),
        "retryable": status_to_sse_retryable(status),
    });
    if let Some(error_code) = error.error_code
        && let Some(obj) = event.as_object_mut()
    {
        obj.insert(
            "error_code".to_string(),
            serde_json::Value::String(error_code),
        );
    }
    if let Some(request_id) = error.request_id
        && let Some(obj) = event.as_object_mut()
    {
        obj.insert(
            "request_id".to_string(),
            serde_json::Value::String(request_id),
        );
    }
    if let Some(metadata) = error.metadata
        && let Some(obj) = event.as_object_mut()
    {
        if let Some(agent_binding_id) = metadata
            .get("agent_binding_id")
            .and_then(serde_json::Value::as_str)
        {
            obj.insert(
                "agent_binding_id".to_string(),
                serde_json::Value::String(agent_binding_id.to_string()),
            );
        }
        obj.insert("metadata".to_string(), metadata);
    }
    sse_json_response(vec![event])
}

pub(super) fn sse_streaming_response(
    session_id: String,
    run_id: String,
    mut event_rx: tokio::sync::mpsc::Receiver<serde_json::Value>,
) -> Response {
    // Build an async stream that yields SSE frames from the channel.
    let stream = async_stream::stream! {
        // First frame: session info.
        let session_info = serde_json::json!({
            "type": "session_info",
            "session_id": session_id,
            "run_id": run_id,
        });
        yield Ok::<_, std::convert::Infallible>(format!(
            "data: {}\n\n",
            serde_json::to_string(&session_info).unwrap_or_default()
        ));

        // Stream events as they arrive from the background loop.
        let mut pending_run_error = None;
        let mut pending_terminal_error = None;
        let mut saw_terminal = false;
        let heartbeat_interval = std::time::Duration::from_secs(SSE_HEARTBEAT_INTERVAL_SECS);
        let mut heartbeat = tokio::time::interval_at(
            tokio::time::Instant::now() + heartbeat_interval,
            heartbeat_interval,
        );
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let event = tokio::select! {
                maybe_event = event_rx.recv() => {
                    let Some(event) = maybe_event else {
                        break;
                    };
                    event
                }
                _ = heartbeat.tick() => {
                    let heartbeat_event = serde_json::json!({
                        "type": "ping",
                        "run_id": run_id,
                        "heartbeat_interval_ms": SSE_HEARTBEAT_INTERVAL_SECS * 1000,
                    });
                    yield Ok::<_, std::convert::Infallible>(format!(
                        "data: {}\n\n",
                        serde_json::to_string(&heartbeat_event).unwrap_or_default()
                    ));
                    continue;
                }
            };
            let incoming_client_error =
                event.get("type").and_then(serde_json::Value::as_str) == Some("error");
            if event
                .get("event_type")
                .and_then(serde_json::Value::as_str)
                == Some("run_error")
            {
                pending_terminal_error = event
                    .get("data")
                    .and_then(serde_json::Value::as_object)
                    .and_then(|data| data.get("error"))
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned);
                tracing::warn!(
                    target: "astra_runtime::sse",
                    run_id = %run_id,
                    error = pending_terminal_error.as_deref().unwrap_or(""),
                    "run failed mid-stream (run_error)",
                );
            } else if event.get("type").and_then(serde_json::Value::as_str) == Some("error") {
                pending_terminal_error = event
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned);
                tracing::warn!(
                    target: "astra_runtime::sse",
                    run_id = %run_id,
                    error = pending_terminal_error.as_deref().unwrap_or(""),
                    "error event emitted mid-stream",
                );
            }

            let events = handlers::transform_stream_run_events_for_client_with_pending(
                &run_id,
                vec![event],
                &mut pending_run_error,
            );
            for event in events {
                if event.get("type").and_then(serde_json::Value::as_str) == Some("run_finished") {
                    saw_terminal = true;
                    pending_terminal_error = None;
                }
                let line = match serde_json::to_string(&event) {
                    Ok(json) => format!("data: {json}\n\n"),
                    Err(_) => {
                        "data: {\"type\":\"error\",\"message\":\"serialization failed\"}\n\n"
                            .to_string()
                    }
                };
                yield Ok::<_, std::convert::Infallible>(line);
            }
            if incoming_client_error
                && let Some(error) = pending_terminal_error.take()
            {
                saw_terminal = true;
                let synthetic_terminal = serde_json::json!({
                    "type": "run_finished",
                    "run_id": run_id,
                    "status": "failed",
                    "error": error,
                });
                yield Ok::<_, std::convert::Infallible>(format!(
                    "data: {}\n\n",
                    serde_json::to_string(&synthetic_terminal).unwrap_or_default()
                ));
                break;
            }
        }
        if !saw_terminal
            && let Some(error) = pending_terminal_error.take()
        {
            let synthetic_terminal = serde_json::json!({
                "type": "run_finished",
                "run_id": run_id,
                "status": "failed",
                "error": error,
            });
            yield Ok::<_, std::convert::Infallible>(format!(
                "data: {}\n\n",
                serde_json::to_string(&synthetic_terminal).unwrap_or_default()
            ));
        }
    };

    let body = Body::from_stream(stream);
    bridge::sse_stream_response(StatusCode::OK, body)
}

pub(super) fn status_to_sse_error_code(status: StatusCode) -> &'static str {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "AUTH_ERROR",
        StatusCode::NOT_FOUND => "NOT_FOUND",
        StatusCode::CONFLICT => "CONFLICT",
        StatusCode::UNPROCESSABLE_ENTITY => "VALIDATION_ERROR",
        _ => "INTERNAL_ERROR",
    }
}

pub(super) fn status_to_sse_retryable(status: StatusCode) -> bool {
    status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body;

    #[test]
    fn sse_code_unauthorized() {
        assert_eq!(
            status_to_sse_error_code(StatusCode::UNAUTHORIZED),
            "AUTH_ERROR"
        );
    }

    #[test]
    fn sse_code_forbidden() {
        assert_eq!(
            status_to_sse_error_code(StatusCode::FORBIDDEN),
            "AUTH_ERROR"
        );
    }

    #[test]
    fn sse_code_not_found() {
        assert_eq!(status_to_sse_error_code(StatusCode::NOT_FOUND), "NOT_FOUND");
    }

    #[test]
    fn sse_conflict_has_a_typed_machine_code() {
        assert_eq!(status_to_sse_error_code(StatusCode::CONFLICT), "CONFLICT");
    }

    #[tokio::test]
    async fn sse_retry_override_is_scoped_to_the_call_site() {
        let response = sse_error_response_with_retryable(StatusCode::CONFLICT, "busy", true);
        let body = body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        let event: serde_json::Value = serde_json::from_str(
            text.strip_prefix("data: ")
                .and_then(|value| value.strip_suffix("\n\n"))
                .expect("single SSE data event"),
        )
        .expect("json");
        assert_eq!(event["code"], "CONFLICT");
        assert_eq!(event["retryable"], true);
        assert!(!status_to_sse_retryable(StatusCode::CONFLICT));
    }

    #[test]
    fn sse_code_unprocessable() {
        assert_eq!(
            status_to_sse_error_code(StatusCode::UNPROCESSABLE_ENTITY),
            "VALIDATION_ERROR"
        );
    }

    #[test]
    fn sse_code_internal_server_error() {
        assert_eq!(
            status_to_sse_error_code(StatusCode::INTERNAL_SERVER_ERROR),
            "INTERNAL_ERROR"
        );
    }

    #[test]
    fn sse_code_unknown_default() {
        assert_eq!(
            status_to_sse_error_code(StatusCode::IM_A_TEAPOT),
            "INTERNAL_ERROR"
        );
    }

    #[test]
    fn sse_retryable_service_unavailable() {
        assert!(status_to_sse_retryable(StatusCode::SERVICE_UNAVAILABLE));
    }

    #[test]
    fn sse_retryable_validation_error_is_false() {
        assert!(!status_to_sse_retryable(StatusCode::UNPROCESSABLE_ENTITY));
    }

    #[tokio::test]
    async fn sse_error_response_from_error_preserves_machine_fields() {
        let error = ErrorResponse::new("stale")
            .with_error_code("bridge_session_turn_stale")
            .with_request_id("req-1")
            .with_metadata(serde_json::json!({"expected_session_turn": 2}));
        let response = sse_error_response_from_error(StatusCode::CONFLICT, error);
        let body = body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        let data = text
            .strip_prefix("data: ")
            .and_then(|value| value.strip_suffix("\n\n"))
            .expect("single SSE data event");
        let event: serde_json::Value = serde_json::from_str(data).expect("json");
        assert_eq!(event["type"], "error");
        assert_eq!(event["message"], "stale");
        assert_eq!(event["error_code"], "bridge_session_turn_stale");
        assert_eq!(event["request_id"], "req-1");
        assert_eq!(event["metadata"]["expected_session_turn"], 2);
    }

    #[tokio::test]
    async fn sse_error_response_exposes_missing_agent_binding_id_at_top_level() {
        let error = ErrorResponse::new("missing binding")
            .with_error_code("agent_binding_not_found")
            .with_metadata(serde_json::json!({"agent_binding_id": "binding-extension"}));
        let response = sse_error_response_from_error(StatusCode::NOT_FOUND, error);
        let body = body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        let data = text
            .strip_prefix("data: ")
            .and_then(|value| value.strip_suffix("\n\n"))
            .expect("single SSE data event");
        let event: serde_json::Value = serde_json::from_str(data).expect("json");
        assert_eq!(event["agent_binding_id"], "binding-extension");
        assert_eq!(event["metadata"]["agent_binding_id"], "binding-extension");
    }

    #[tokio::test]
    async fn streaming_response_synthesizes_failed_terminal_after_client_error_without_run_finished()
     {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tx.send(serde_json::json!({
            "type": "error",
            "message": "stream transport failed; non-stream recovery failed",
            "code": "stream_transport",
            "retryable": true
        }))
        .await
        .expect("queue error");
        drop(tx);

        let response = sse_streaming_response("session-1".to_string(), "run-1".to_string(), rx);
        let body = body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(
            text.contains("\"type\":\"error\""),
            "original error should still stream: {text}"
        );
        assert!(
            text.contains("\"type\":\"run_finished\""),
            "live SSE adapter should synthesize a failed terminal event when the channel closes after an error: {text}"
        );
        assert!(
            text.contains("\"status\":\"failed\""),
            "synthetic terminal should be failed: {text}"
        );
        assert!(
            text.contains("\"error\":\"stream transport failed; non-stream recovery failed\""),
            "synthetic terminal should carry the error text: {text}"
        );
    }

    #[tokio::test]
    async fn streaming_response_stops_after_client_error_without_waiting_for_sender_drop() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tx.send(serde_json::json!({
            "type": "error",
            "message": "boom",
            "code": "stream_transport",
            "retryable": true
        }))
        .await
        .expect("queue error");

        let response = sse_streaming_response("session-2".to_string(), "run-2".to_string(), rx);
        let body = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            body::to_bytes(response.into_body(), 1024 * 1024),
        )
        .await
        .expect("stream should terminate without waiting for sender drop")
        .expect("body");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(
            text.contains("\"type\":\"error\""),
            "error event should stream: {text}"
        );
        assert!(
            text.contains("\"type\":\"run_finished\""),
            "client-shaped error should force an immediate failed terminal event: {text}"
        );
    }

    #[tokio::test]
    async fn streaming_response_bounds_multiple_large_tool_results() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        for index in 1..=2 {
            let structured_content = serde_json::json!({
                "pages": [{
                    "file_id": format!("file-{index}"),
                    "page_number": 200 + index,
                    "content": "x".repeat(2 * 1024 * 1024),
                }]
            });
            tx.send(serde_json::json!({
                "type": "tool_call_end",
                "call_id": format!("call-{index}"),
                "tool": "read_catalog_file_pages",
                "result": "x".repeat(100_000),
                "output": structured_content,
                "structuredContent": structured_content,
                "success": true,
            }))
            .await
            .expect("queue tool result");
        }
        tx.send(serde_json::json!({
            "event_type": "run_finished",
            "index": 3,
            "data": {}
        }))
        .await
        .expect("queue terminal event");
        drop(tx);

        let response = sse_streaming_response("session-large".into(), "run-large".into(), rx);
        let body = body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .expect("bounded SSE body");
        let text = String::from_utf8(body.to_vec()).expect("utf8 SSE body");

        assert!(
            body.len() < 64 * 1024,
            "external SSE body was {} bytes",
            body.len()
        );
        assert_eq!(text.matches("\"result_truncated\":true").count(), 2);
        assert_eq!(text.matches("\"payload_truncated\":true").count(), 2);
        assert!(!text.contains("structuredContent"));
        assert!(text.contains("\"type\":\"run_finished\""));
        assert!(text.contains("\"status\":\"completed\""));
    }
}
