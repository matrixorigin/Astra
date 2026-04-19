use super::*;

pub(super) fn require_bearer_auth(
    headers: &HeaderMap,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if bearer_token(headers).is_ok() {
        Ok(())
    } else {
        Err(error_response(
            StatusCode::UNAUTHORIZED,
            "Not authenticated",
        ))
    }
}

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
    mut event_rx: tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
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
        while let Some(event) = event_rx.recv().await {
            let line = match serde_json::to_string(&event) {
                Ok(json) => format!("data: {json}\n\n"),
                Err(_) => "data: {\"type\":\"error\",\"message\":\"serialization failed\"}\n\n".to_string(),
            };
            yield Ok::<_, std::convert::Infallible>(line);
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
}
