use super::*;

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
    sse_json_response(vec![serde_json::json!({
        "type": "error",
        "message": message.into(),
        "code": status_to_sse_error_code(status),
        "retryable": status_to_sse_retryable(status),
    })])
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
        while let Some(event) = event_rx.recv().await {
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
            } else if event.get("type").and_then(serde_json::Value::as_str) == Some("error") {
                pending_terminal_error = event
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned);
            }

            let events = super::run_handlers::transform_stream_run_events_for_client_with_pending(
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
}
