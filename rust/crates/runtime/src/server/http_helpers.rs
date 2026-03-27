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
            Err(_) => "data: {\"type\":\"error\",\"message\":\"serialization failed\"}\n\n".to_string(),
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
        "retryable": false,
    })])
}

pub(super) fn status_to_sse_error_code(status: StatusCode) -> &'static str {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "AUTH_ERROR",
        StatusCode::NOT_FOUND => "NOT_FOUND",
        StatusCode::UNPROCESSABLE_ENTITY => "VALIDATION_ERROR",
        _ => "INTERNAL_ERROR",
    }
}
