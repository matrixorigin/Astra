use super::*;
use crate::server::run::handlers;
use astra_services::runs::SSE_HEARTBEAT_INTERVAL_SECS;

/// Stable identifiers available at a chat SSE boundary. These are support
/// diagnostics only; raw failure detail remains confined to server logs.
#[derive(Clone, Copy, Default)]
pub(super) struct SseErrorContext<'a> {
    pub request_id: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub run_id: Option<&'a str>,
}

fn validated_diagnostic_session_id(session_id: Option<&str>) -> Option<&str> {
    session_id
        .filter(|session_id| astra_services::validate_persisted_session_id(session_id).is_ok())
}

fn remove_invalid_metadata_session_id(metadata: &mut Option<serde_json::Value>) -> Option<String> {
    let metadata = metadata.as_mut()?;
    let session_id = metadata
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)?;
    if astra_services::validate_persisted_session_id(&session_id).is_ok() {
        return Some(session_id);
    }
    if let Some(object) = metadata.as_object_mut() {
        object.remove("session_id");
    }
    None
}

pub(super) fn sse_json_response(events: Vec<serde_json::Value>) -> Response {
    sse_replay_response(events, None)
}

pub(super) fn sse_replay_response(
    events: Vec<serde_json::Value>,
    through: Option<i64>,
) -> Response {
    let mut body = events.iter().map(sse_event_frame).collect::<String>();
    body.push_str(&sse_done_frame(through));

    sse_run_response_body(Body::from(body))
}

pub(super) fn sse_done_frame(through: Option<i64>) -> String {
    match through {
        Some(index) => format!("id: {}\ndata: [DONE]\n\n", index.saturating_add(1)),
        None => "data: [DONE]\n\n".to_string(),
    }
}

pub(super) fn sse_event_frame(event: &serde_json::Value) -> String {
    format!("data: {event}\n\n")
}

pub(super) fn sse_run_response_body(body: Body) -> Response {
    with_interaction_protocol(sse_stream_response(StatusCode::OK, body))
}

pub(crate) fn sse_stream_response(status: StatusCode, body: Body) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .header("x-accel-buffering", "no")
        .body(body)
        .expect("static SSE response headers are valid")
}

fn sse_error_kind(status: StatusCode, _domain_code: Option<&str>) -> astra_core::ErrorKind {
    // The HTTP status is the only stable classifier at this boundary.  Domain
    // codes are descriptive and may contain words such as "authority" or
    // "token" that do not mean authentication or rate limiting.
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => astra_core::ErrorKind::Auth,
        StatusCode::TOO_MANY_REQUESTS => astra_core::ErrorKind::RateLimit,
        status if status.is_client_error() => astra_core::ErrorKind::InvalidRequest,
        status if status.is_server_error() => astra_core::ErrorKind::ServerError,
        _ => astra_core::ErrorKind::Unknown,
    }
}

fn with_interaction_protocol(mut response: Response) -> Response {
    response.headers_mut().insert(
        axum::http::HeaderName::from_static(astra_server_types::AGENT_INTERACTION_API_MAJOR_HEADER),
        axum::http::HeaderValue::from_static(astra_server_types::AGENT_INTERACTION_API_MAJOR),
    );
    response
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
    sse_error_response_with_retryable_and_context(
        status,
        message,
        retryable,
        SseErrorContext::default(),
    )
}

pub(super) fn sse_error_response_with_retryable_and_context(
    status: StatusCode,
    message: impl Into<String>,
    retryable: bool,
    context: SseErrorContext<'_>,
) -> Response {
    let message = message.into();
    let session_id = validated_diagnostic_session_id(context.session_id);
    tracing::warn!(
        target: "astra_runtime::sse",
        http_status = status.as_u16(),
        error_code = status_to_chat_sse_error_code(status),
        retryable,
        request_id = context.request_id.unwrap_or(""),
        session_id = session_id.unwrap_or(""),
        run_id = context.run_id.unwrap_or(""),
        message = %message,
        "sse error response emitted to client",
    );
    let mut event = serde_json::json!({
        "type": "error",
        "message": message,
        "code": status_to_chat_sse_error_code(status),
        "retryable": retryable,
        "error_kind": sse_error_kind(status, None).as_str(),
    });
    if let Some(object) = event.as_object_mut() {
        for (field, value) in [
            ("request_id", context.request_id),
            ("session_id", session_id),
        ] {
            if let Some(value) = value.filter(|value| !value.is_empty()) {
                object.insert(
                    field.to_string(),
                    serde_json::Value::String(value.to_string()),
                );
            }
        }
    }
    sse_json_response(vec![event])
}

/// Evidence for this physical request only. Call exclusively before the Run
/// lifecycle has been invoked or before a new Run has been persisted.
pub(super) fn mark_attempt_rejected(error: &mut ErrorResponse) {
    let metadata = error.metadata.get_or_insert_with(|| serde_json::json!({}));
    if let Some(fields) = metadata.as_object_mut() {
        fields.insert(
            "attempt_admission_state".into(),
            serde_json::json!("rejected"),
        );
    }
}

pub(super) fn sse_error_response_from_error_with_context(
    status: StatusCode,
    error: ErrorResponse,
    context: SseErrorContext<'_>,
) -> Response {
    sse_json_response(vec![sse_error_event(status, error, context)])
}

pub(super) fn sse_error_event(
    status: StatusCode,
    mut error: ErrorResponse,
    context: SseErrorContext<'_>,
) -> serde_json::Value {
    if error.request_id.is_none() {
        error.request_id = context.request_id.map(str::to_owned);
    }
    let metadata_session_id = remove_invalid_metadata_session_id(&mut error.metadata);
    let context_session_id = validated_diagnostic_session_id(context.session_id);
    let metadata = error.metadata.as_ref();
    let session_id = metadata_session_id
        .as_deref()
        .or(context_session_id)
        .unwrap_or("");
    let agent_binding_id = metadata
        .and_then(|metadata| metadata.get("agent_binding_id"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    tracing::warn!(
        target: "astra_runtime::sse",
        http_status = status.as_u16(),
        error_code = status_to_chat_sse_error_code(status),
        retryable = status_to_sse_retryable(status),
        domain_error_code = error.error_code.as_deref().unwrap_or(""),
        request_id = error.request_id.as_deref().unwrap_or(""),
        caller_request_id = context.request_id.unwrap_or(""),
        session_id,
        run_id = context.run_id.unwrap_or(""),
        agent_binding_id = %agent_binding_id,
        message = %error.detail,
        "sse error response emitted to client",
    );
    let mut event = serde_json::json!({
        "type": "error",
        "message": error.detail,
        "code": status_to_chat_sse_error_code(status),
        "retryable": status_to_sse_retryable(status),
    });
    // Every structured HTTP failure crossing the SSE boundary carries the
    // same stable classifier as the JSON API. In particular, a 409 admission
    // rejection is a user-actionable request conflict, never an "unknown"
    // provider failure in the CLI.
    if let Some(obj) = event.as_object_mut() {
        let kind = sse_error_kind(status, error.error_code.as_deref());
        obj.insert(
            "error_kind".to_string(),
            serde_json::Value::String(kind.as_str().to_string()),
        );
    }
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
        if let Some(value) = metadata
            .get("agent_binding_id")
            .and_then(serde_json::Value::as_str)
        {
            obj.insert(
                "agent_binding_id".to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
        if let Some(value) = metadata_session_id.as_deref() {
            obj.insert(
                "session_id".to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
        obj.insert("metadata".to_string(), metadata);
    }
    if !session_id.is_empty()
        && let Some(obj) = event.as_object_mut()
    {
        obj.entry("session_id".to_string())
            .or_insert_with(|| serde_json::Value::String(session_id.to_string()));
    }
    event
}

/// Emits an SSE error tied to the request trace that admitted the chat turn.
///
/// SSE error frames use HTTP 200, so the JSON error-response middleware cannot
/// enrich them after the handler returns. Attach the request id before logging
/// and serializing the frame instead.
pub(super) fn sse_error_response_from_error_with_request_id(
    status: StatusCode,
    error: ErrorResponse,
    request_id: Option<&str>,
) -> Response {
    sse_error_response_from_error_with_context(
        status,
        error,
        SseErrorContext {
            request_id,
            ..SseErrorContext::default()
        },
    )
}

#[cfg(any(test, feature = "e2e-hooks"))]
pub(super) fn sse_streaming_response(
    session_id: String,
    run_id: String,
    request_id: Option<String>,
    event_rx: tokio::sync::mpsc::Receiver<serde_json::Value>,
) -> Response {
    sse_run_stream_response(session_id, run_id, request_id, event_rx, None)
}

pub(super) fn sse_run_stream_response(
    session_id: String,
    run_id: String,
    request_id: Option<String>,
    event_rx: tokio::sync::mpsc::Receiver<serde_json::Value>,
    attachment_completion: Option<tokio::sync::oneshot::Receiver<i64>>,
) -> Response {
    sse_projected_streaming_response(
        serde_json::json!({
            "type": "session_info",
            "session_id": session_id,
            "run_id": run_id,
        }),
        session_id,
        run_id,
        request_id,
        event_rx,
        handlers::transform_stream_run_events_for_client_with_pending,
        attachment_completion,
    )
}

pub(super) type SseStreamEventProjector =
    fn(&str, Vec<serde_json::Value>, &mut Option<String>) -> Vec<serde_json::Value>;

const SSE_DURABLE_TERMINAL_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Shared SSE transport framing. Product boundaries provide their own first
/// event and structural event projector, so opaque internal identities never
/// have to cross a public protocol merely to reuse heartbeat/backpressure.
pub(super) fn sse_projected_streaming_response(
    initial_event: serde_json::Value,
    session_id: String,
    run_id: String,
    request_id: Option<String>,
    mut event_rx: tokio::sync::mpsc::Receiver<serde_json::Value>,
    project_events: SseStreamEventProjector,
    mut attachment_completion: Option<tokio::sync::oneshot::Receiver<i64>>,
) -> Response {
    // Build an async stream that yields SSE frames from the channel.
    let stream = async_stream::stream! {
        let execution = attachment_completion.is_none();
        yield Ok::<_, std::convert::Infallible>(format!(
            "data: {}\n\n",
            serde_json::to_string(&initial_event).unwrap_or_default()
        ));

        // Stream events as they arrive from the background loop.
        let mut pending_run_error = None;
        let mut pending_terminal_deadline = None;
        let mut saw_terminal = false;
        let mut requires_authoritative_completion = false;
        let mut saw_authoritative_completion = false;
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
                _ = tokio::time::sleep_until(
                    pending_terminal_deadline.unwrap_or_else(tokio::time::Instant::now)
                ), if pending_terminal_deadline.is_some() => {
                    // Publication may be unavailable while the owner still
                    // settles execution. The common close path reports uncertainty.
                    break;
                }
            };
            let raw_event_type = event
                .get("event_type")
                .and_then(serde_json::Value::as_str);
            let client_event_type = event.get("type").and_then(serde_json::Value::as_str);
            let producer_run_id = event
                .get("run_id")
                .or_else(|| event.get("data").and_then(|data| data.get("run_id")))
                .and_then(serde_json::Value::as_str);
            let event_belongs_to_root = producer_run_id
                .is_none_or(|producer_run_id| producer_run_id == run_id);
            if (raw_event_type == Some("run_error") || client_event_type == Some("run_error"))
                && event_belongs_to_root
            {
                if execution {
                    pending_terminal_deadline = Some(
                        tokio::time::Instant::now() + SSE_DURABLE_TERMINAL_GRACE
                    );
                }
                tracing::warn!(
                    target: "astra_runtime::sse",
                    request_id = request_id.as_deref().unwrap_or(""),
                    session_id = %session_id,
                    run_id = %run_id,
                    event_type = "run_error",
                    domain_error_code = sse_stream_event_string_field(&event, "error_code").unwrap_or(""),
                    error_kind = sse_stream_event_string_field(&event, "error_kind").unwrap_or(""),
                    retryable = sse_stream_event_bool_field(&event, "retryable").unwrap_or(false),
                    error = sse_stream_event_string_field(&event, "error")
                        .or_else(|| sse_stream_event_string_field(&event, "message")).unwrap_or(""),
                    "run failed mid-stream (run_error)",
                );
            } else if client_event_type == Some("error") && event_belongs_to_root {
                // Attempt diagnostics do not impose a terminal deadline on
                // an owner that may still recover and continue the run.
                tracing::warn!(
                    target: "astra_runtime::sse",
                    request_id = request_id.as_deref().unwrap_or(""),
                    session_id = %session_id,
                    run_id = %run_id,
                    event_type = "error",
                    domain_error_code = sse_stream_event_string_field(&event, "error_code").unwrap_or(""),
                    error_kind = sse_stream_event_string_field(&event, "error_kind").unwrap_or(""),
                    retryable = sse_stream_event_bool_field(&event, "retryable").unwrap_or(false),
                    error = sse_stream_event_string_field(&event, "error")
                        .or_else(|| sse_stream_event_string_field(&event, "message")).unwrap_or(""),
                    "error event emitted mid-stream",
                );
            } else if event
                .get("event_type")
                .and_then(serde_json::Value::as_str)
                == Some("run_finished")
                && pending_run_error.is_none()
                && (sse_stream_event_string_field(&event, "status") == Some("failed")
                    || sse_stream_event_string_field(&event, "error").is_some()
                    || sse_stream_event_string_field(&event, "error_code").is_some())
            {
                tracing::warn!(
                    target: "astra_runtime::sse",
                    request_id = request_id.as_deref().unwrap_or(""),
                    session_id = %session_id,
                    run_id = %run_id,
                    event_type = "run_finished",
                    domain_error_code = sse_stream_event_string_field(&event, "error_code").unwrap_or(""),
                    error_kind = sse_stream_event_string_field(&event, "error_kind").unwrap_or(""),
                    retryable = sse_stream_event_bool_field(&event, "retryable").unwrap_or(false),
                    error = sse_stream_event_string_field(&event, "error").unwrap_or(""),
                    "run finished with failure evidence but no preceding run_error",
                );
            }

            let events = project_events(
                &run_id,
                vec![event],
                &mut pending_run_error,
            );
            let mut close_after_projection = false;
            for event in events {
                let event_type = event.get("type").and_then(serde_json::Value::as_str);
                let is_root_terminal = event_type == Some("run_finished")
                    && event.get("run_id").and_then(serde_json::Value::as_str)
                        == Some(run_id.as_str());
                saw_terminal |= is_root_terminal;
                if is_root_terminal && execution {
                    let status = event.get("status").and_then(serde_json::Value::as_str);
                    requires_authoritative_completion =
                        matches!(status, Some("completed" | "paused"));
                    if requires_authoritative_completion {
                        // A completed/paused Server-owned loop still owes the
                        // client-only authoritative `turn_complete`. Bound
                        // that settlement without confusing descendant
                        // terminals for physical stream completion.
                        pending_terminal_deadline = Some(
                            tokio::time::Instant::now() + SSE_DURABLE_TERMINAL_GRACE
                        );
                    } else {
                        pending_terminal_deadline = None;
                        close_after_projection = true;
                    }
                } else if execution
                    && event_type == Some("turn_complete")
                    && event.get("continuation_owner").and_then(serde_json::Value::as_str)
                        == Some("server")
                {
                    saw_authoritative_completion = true;
                    pending_terminal_deadline = None;
                    close_after_projection = true;
                }
                let line = match serde_json::to_string(&event) {
                    Ok(json) => format!("data: {json}\n\n"),
                    Err(error) => {
                        tracing::error!(
                            target: "astra_runtime::sse",
                            request_id = request_id.as_deref().unwrap_or(""),
                            session_id = %session_id,
                            run_id = %run_id,
                            event_type = event
                                .get("type")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or(""),
                            %error,
                            "failed to serialize chat SSE event",
                        );
                        "data: {\"type\":\"error\",\"message\":\"serialization failed\"}\n\n"
                            .to_string()
                    }
                };
                yield Ok::<_, std::convert::Infallible>(line);
            }
            if close_after_projection {
                break;
            }
            // A client-shaped error is evidence about the current attempt,
            // not authority to terminate the durable run.  The lifecycle
            // owner may still commit a more precise paused/cancelled/failed
            // terminal after settling provider and persistence state.  Keep
            // the transport open and only synthesize a fallback if the owner
            // channel actually closes without a typed run terminal.
        }
        if requires_authoritative_completion && !saw_authoritative_completion {
            let protocol_error = serde_json::json!({
                "type": "error",
                "run_id": run_id,
                "message": "durable run terminal omitted authoritative turn_complete",
                "error_kind": "contract_violation",
                "retryable": false,
            });
            yield Ok::<_, std::convert::Infallible>(format!(
                "data: {}\n\n",
                serde_json::to_string(&protocol_error).unwrap_or_default()
            ));
        }
        let through = attachment_completion.as_mut().and_then(|completion| completion.try_recv().ok());
        let observation_completed = through.is_some() || (execution && saw_terminal);
        if !observation_completed {
            let unconfirmed = serde_json::json!({
                "type": "error",
                "run_id": run_id,
                "message": "The connection ended before the run outcome was confirmed. Inspect the existing run before retrying.",
                "code": "stream_observation_interrupted",
                "error_kind": astra_core::ErrorKind::StreamTransport.as_str(),
                "retryable": false,
            });
            yield Ok::<_, std::convert::Infallible>(format!(
                "data: {}\n\n",
                serde_json::to_string(&unconfirmed).unwrap_or_default()
            ));
        }
        // `[DONE]` is the one transport-level completion marker. It follows
        // every projected or synthesized typed terminal and never replaces
        // that outcome. Owning it here gives CLI, Web, Edge, and Desktop the
        // same exact-once framing contract.
        yield Ok::<_, std::convert::Infallible>(sse_done_frame(through));
    };

    let body = Body::from_stream(stream);
    with_interaction_protocol(sse_stream_response(StatusCode::OK, body))
}

fn sse_stream_event_string_field<'a>(event: &'a serde_json::Value, field: &str) -> Option<&'a str> {
    event
        .get(field)
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            event
                .get("data")
                .and_then(serde_json::Value::as_object)
                .and_then(|data| data.get(field))
                .and_then(serde_json::Value::as_str)
        })
}

fn sse_stream_event_bool_field(event: &serde_json::Value, field: &str) -> Option<bool> {
    event
        .get(field)
        .and_then(serde_json::Value::as_bool)
        .or_else(|| {
            event
                .get("data")
                .and_then(serde_json::Value::as_object)
                .and_then(|data| data.get(field))
                .and_then(serde_json::Value::as_bool)
        })
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

/// Chat SSE has a separate public error contract from WebSocket messages.
/// Keep this mapping private so extending the chat UI's diagnostic categories
/// cannot silently change the established WebSocket protocol.
fn status_to_chat_sse_error_code(status: StatusCode) -> &'static str {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "AUTH_ERROR",
        StatusCode::NOT_FOUND => "NOT_FOUND",
        StatusCode::CONFLICT => "CONFLICT",
        StatusCode::TOO_MANY_REQUESTS => "RATE_LIMITED",
        StatusCode::BAD_REQUEST
        | StatusCode::PAYLOAD_TOO_LARGE
        | StatusCode::UNPROCESSABLE_ENTITY => "VALIDATION_ERROR",
        StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => "TIMEOUT",
        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE => "UPSTREAM_ERROR",
        _ => "INTERNAL_ERROR",
    }
}

pub(super) fn status_to_sse_retryable(status: StatusCode) -> bool {
    status.is_server_error()
        || matches!(
            status,
            StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn durable_attachment_eof_uses_reader_confirmation() {
        for completed in [false, true] {
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            let (complete, confirmation) = tokio::sync::oneshot::channel();
            let event = if completed {
                serde_json::json!({"event_type":"run_paused","data":{"run_id":"root"}})
            } else {
                serde_json::json!({"event_type":"run_finished","data":{"run_id":"root","status":"completed"}})
            };
            tx.send(event).await.unwrap();
            if completed {
                complete.send(9).unwrap();
            } else {
                drop(complete);
            }
            drop(tx);
            let response = sse_run_stream_response(
                "session".into(),
                "root".into(),
                None,
                rx,
                Some(confirmation),
            );
            let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .unwrap();
            let body = std::str::from_utf8(&body).unwrap();
            assert_eq!(
                body.contains("stream_observation_interrupted"),
                !completed,
                "{body}"
            );
            assert!(!body.contains("contract_violation"), "{body}");
            assert_eq!(body.matches("data: [DONE]").count(), 1);
            assert_eq!(body.contains("id: 10\ndata: [DONE]"), completed);
        }
    }
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

    #[test]
    fn sse_rate_limit_has_a_typed_machine_code() {
        assert_eq!(
            status_to_chat_sse_error_code(StatusCode::TOO_MANY_REQUESTS),
            "RATE_LIMITED"
        );
    }

    fn first_sse_json(text: &str) -> serde_json::Value {
        assert!(text.ends_with("data: [DONE]\n\n"));
        let data = text
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("SSE data");
        serde_json::from_str(data).expect("JSON event")
    }

    #[tokio::test]
    async fn sse_retry_override_is_scoped_to_the_call_site() {
        let response = sse_error_response_with_retryable(StatusCode::CONFLICT, "busy", true);
        assert_eq!(
            response
                .headers()
                .get(astra_server_types::AGENT_INTERACTION_API_MAJOR_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some(astra_server_types::AGENT_INTERACTION_API_MAJOR),
        );
        let body = body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        let event = first_sse_json(&text);
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
    fn sse_code_other_invalid_requests_are_validation_errors() {
        for status in [StatusCode::BAD_REQUEST, StatusCode::PAYLOAD_TOO_LARGE] {
            assert_eq!(status_to_chat_sse_error_code(status), "VALIDATION_ERROR");
        }
    }

    #[test]
    fn sse_code_timeout_and_upstream_errors_are_distinct() {
        for status in [StatusCode::REQUEST_TIMEOUT, StatusCode::GATEWAY_TIMEOUT] {
            assert_eq!(status_to_chat_sse_error_code(status), "TIMEOUT");
        }
        for status in [StatusCode::BAD_GATEWAY, StatusCode::SERVICE_UNAVAILABLE] {
            assert_eq!(status_to_chat_sse_error_code(status), "UPSTREAM_ERROR");
        }
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

    #[test]
    fn sse_error_kind_uses_http_status_not_domain_code_substrings() {
        assert_eq!(
            sse_error_kind(
                StatusCode::UNAUTHORIZED,
                Some("conversation_authority_fenced")
            ),
            astra_core::ErrorKind::Auth
        );
        assert_eq!(
            sse_error_kind(StatusCode::FORBIDDEN, Some("token_budget_exceeded")),
            astra_core::ErrorKind::Auth
        );
        assert_eq!(
            sse_error_kind(
                StatusCode::TOO_MANY_REQUESTS,
                Some("session_authority_unavailable")
            ),
            astra_core::ErrorKind::RateLimit
        );
        for code in [
            "conversation_authority_fenced",
            "conversation_authority_run_conflict",
            "session_authority_unavailable",
            "token_budget_exceeded",
        ] {
            assert_eq!(
                sse_error_kind(StatusCode::CONFLICT, Some(code)),
                astra_core::ErrorKind::InvalidRequest,
                "domain code {code} must not be mistaken for authentication"
            );
        }
    }

    #[tokio::test]
    async fn sse_error_response_from_error_preserves_machine_fields() {
        let error = ErrorResponse::new("stale")
            .with_error_code("session_turn_mismatch")
            .with_request_id("req-1")
            .with_metadata(serde_json::json!({"expected_session_turn": 2}));
        let response = sse_error_response_from_error_with_context(
            StatusCode::CONFLICT,
            error,
            SseErrorContext::default(),
        );
        let body = body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        let event = first_sse_json(&text);
        assert_eq!(event["type"], "error");
        assert_eq!(event["message"], "stale");
        assert_eq!(event["error_code"], "session_turn_mismatch");
        assert_eq!(event["error_kind"], "invalid_request");
        assert_eq!(event["request_id"], "req-1");
        assert_eq!(event["metadata"]["expected_session_turn"], 2);
    }

    #[tokio::test]
    async fn sse_error_response_attaches_request_id_before_serializing() {
        let response = sse_error_response_from_error_with_request_id(
            StatusCode::NOT_FOUND,
            ErrorResponse::new("missing session"),
            Some("trace_1"),
        );
        let body = body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        let event = first_sse_json(&text);
        assert_eq!(event["request_id"], "trace_1");
    }

    #[tokio::test]
    async fn sse_error_response_context_exposes_request_and_session_ids() {
        let response = sse_error_response_from_error_with_context(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorResponse::new("runtime unavailable"),
            SseErrorContext {
                request_id: Some("trace_2"),
                session_id: Some("session_2"),
                run_id: Some("run_2"),
            },
        );
        let body = body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        let event = first_sse_json(&text);
        assert_eq!(event["request_id"], "trace_2");
        assert_eq!(event["session_id"], "session_2");
    }

    #[tokio::test]
    async fn sse_error_response_exposes_support_ids_at_top_level() {
        let error = ErrorResponse::new("missing binding")
            .with_error_code("agent_binding_not_found")
            .with_metadata(serde_json::json!({
                "agent_binding_id": "binding-extension",
                "session_id": "session-unavailable",
            }));
        let response = sse_error_response_from_error_with_context(
            StatusCode::NOT_FOUND,
            error,
            SseErrorContext::default(),
        );
        let body = body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        let event = first_sse_json(&text);
        assert_eq!(event["agent_binding_id"], "binding-extension");
        assert_eq!(event["metadata"]["agent_binding_id"], "binding-extension");
        assert_eq!(event["session_id"], "session-unavailable");
        assert_eq!(event["metadata"]["session_id"], "session-unavailable");
    }

    #[tokio::test]
    async fn sse_error_response_does_not_reflect_invalid_session_id() {
        let invalid_session_id = "x".repeat(4 * 1024 * 1024);
        let error = ErrorResponse::new("invalid session")
            .with_metadata(serde_json::json!({ "session_id": invalid_session_id }));
        let response = sse_error_response_from_error_with_context(
            StatusCode::BAD_REQUEST,
            error,
            SseErrorContext {
                session_id: Some(&invalid_session_id),
                ..SseErrorContext::default()
            },
        );
        let body = body::to_bytes(response.into_body(), 8 * 1024 * 1024)
            .await
            .expect("body");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        let event = first_sse_json(&text);
        assert!(event.get("session_id").is_none());
        assert!(event["metadata"].get("session_id").is_none());
        assert!(!text.contains(&invalid_session_id));
    }

    #[tokio::test]
    async fn streaming_response_waits_for_durable_terminal_after_client_error() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tx.send(serde_json::json!({
            "type": "error",
            "message": "boom",
            "code": "stream_transport",
            "retryable": true
        }))
        .await
        .expect("queue error");

        tx.send(serde_json::json!({
            "event_type": "run_interrupted",
            "data": {"kind": "budget_exhausted"}
        }))
        .await
        .expect("queue interruption");
        tx.send(serde_json::json!({
            "event_type": "run_finished",
            "data": {
                "interrupted": true,
                "interruption_kind": "budget_exhausted"
            }
        }))
        .await
        .expect("queue durable terminal");
        tx.send(serde_json::json!({
            "type": "turn_complete",
            "continuation_owner": "server",
            "execution_state": {
                "status": "interrupted",
                "interruption_kind": "budget_exhausted"
            }
        }))
        .await
        .expect("queue authoritative completion");
        drop(tx);

        let response = sse_streaming_response(
            "session-2".to_string(),
            "run-2".to_string(),
            Some("trace-2".to_string()),
            rx,
        );
        let body = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            body::to_bytes(response.into_body(), 1024 * 1024),
        )
        .await
        .expect("stream should terminate after the durable owner closes")
        .expect("body");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(
            text.contains("\"type\":\"error\""),
            "error event should stream: {text}"
        );
        assert!(
            text.contains("\"type\":\"run_finished\""),
            "durable lifecycle owner should publish the terminal event: {text}"
        );
        assert!(text.contains("\"status\":\"paused\""), "{text}");
        assert!(text.contains("\"type\":\"turn_complete\""), "{text}");
        assert!(text.contains("\"continuation_owner\":\"server\""), "{text}");
        assert!(!text.contains("\"status\":\"failed\""), "{text}");
        assert_eq!(text.matches("data: [DONE]\n\n").count(), 1);
        assert!(text.ends_with("data: [DONE]\n\n"));
    }

    #[tokio::test(start_paused = true)]
    async fn recoverable_attempt_error_does_not_end_a_live_root_stream() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tx.send(serde_json::json!({
            "type":"error", "run_id":"root", "error_kind":"context_window",
            "message":"provider attempt needs context recovery"
        }))
        .await
        .unwrap();
        let response = sse_streaming_response("session".into(), "root".into(), None, rx);
        let body = tokio::spawn(body::to_bytes(response.into_body(), 1024 * 1024));
        tokio::task::yield_now().await;
        tokio::time::advance(SSE_DURABLE_TERMINAL_GRACE * 2).await;
        tokio::task::yield_now().await;
        assert!(
            !body.is_finished(),
            "an attempt diagnostic cannot expire the owner's stream"
        );
        tx.send(serde_json::json!({
            "event_type":"run_finished", "run_id":"root",
            "data":{"status":"completed", "owner_generation":4}
        }))
        .await
        .unwrap();
        tx.send(serde_json::json!({
            "type":"turn_complete", "continuation_owner":"server"
        }))
        .await
        .unwrap();
        let bytes = body.await.unwrap().unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(!text.contains("stream_observation_interrupted"), "{text}");
        assert!(text.contains("\"status\":\"completed\""), "{text}");
        assert_eq!(text.matches("data: [DONE]\n\n").count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn unconfirmed_stream_keeps_typed_cause_without_inventing_a_run_terminal() {
        for owner_channel_closes in [true, false] {
            let (tx, rx) = tokio::sync::mpsc::channel(2);
            tx.send(serde_json::json!({
                "event_type": "run_error",
                "run_id": "root",
                "data": {"error": "storage unavailable", "error_kind": "database_error"}
            }))
            .await
            .unwrap();
            let retained_sender = (!owner_channel_closes).then_some(tx.clone());
            drop(tx);
            let response = sse_streaming_response("session".into(), "root".into(), None, rx);
            let bytes = body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap();
            let text = String::from_utf8(bytes.to_vec()).unwrap();
            let mut source = futures_util::stream::iter([Ok::<_, String>(bytes.to_vec())]);
            let mut host = astra_turn_core::sse::stream_host::NoopSseStreamHost::default();
            let (result, _) = astra_turn_core::sse::stream_host::consume_sse_stream(
                &mut source,
                &mut host,
                std::time::Duration::from_secs(30),
            )
            .await;
            assert_eq!(
                result.accum.error_kind,
                Some(astra_core::ErrorKind::DatabaseError),
                "{text}"
            );
            assert!(
                result.accum.run_terminal.is_none(),
                "a lost stream is not an owner terminal: {text}"
            );
            assert!(text.contains("stream_observation_interrupted"), "{text}");
            assert_eq!(text.matches("data: [DONE]\n\n").count(), 1);
            drop(retained_sender);
        }
    }

    #[tokio::test]
    async fn streaming_response_fails_closed_when_channel_ends_without_terminal() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(tx);

        let response = sse_streaming_response("session-gap".into(), "run-gap".into(), None, rx);
        let body = body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("SSE body");
        let text = String::from_utf8(body.to_vec()).expect("utf8 SSE");

        assert!(!text.contains("\"type\":\"run_finished\""));
        assert!(text.contains("stream_observation_interrupted"));
        assert!(text.contains("\"error_kind\":\"stream_transport\""));
        assert_eq!(text.matches("data: [DONE]\n\n").count(), 1);
        assert!(text.ends_with("data: [DONE]\n\n"));
    }

    #[tokio::test]
    async fn descendant_terminal_does_not_settle_physical_root_stream() {
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        tx.send(serde_json::json!({
            "event_type": "run_finished",
            "run_id": "child-run",
            "data": {"prompt_tokens": 1, "completion_tokens": 1}
        }))
        .await
        .unwrap();
        drop(tx);

        let response = sse_streaming_response("session-root".into(), "root-run".into(), None, rx);
        let body = body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();

        assert!(text.contains("\"run_id\":\"child-run\""), "{text}");
        assert!(text.contains("\"run_id\":\"root-run\""), "{text}");
        assert_eq!(
            text.matches("\"type\":\"run_finished\"").count(),
            1,
            "{text}"
        );
        assert!(text.contains("stream_observation_interrupted"), "{text}");
        assert!(!text.contains("\"type\":\"usage\""), "{text}");
        assert!(!text.contains("\"type\":\"context_usage\""), "{text}");
        assert_eq!(text.matches("data: [DONE]\n\n").count(), 1);
    }

    #[tokio::test]
    async fn paused_root_without_authoritative_completion_fails_closed() {
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        tx.send(serde_json::json!({
            "event_type": "run_finished",
            "run_id": "root-run",
            "data": {"interrupted": true}
        }))
        .await
        .unwrap();
        drop(tx);

        let response = sse_streaming_response("session-root".into(), "root-run".into(), None, rx);
        let body = body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();

        assert!(text.contains("\"status\":\"paused\""), "{text}");
        assert!(
            text.contains("\"error_kind\":\"contract_violation\""),
            "{text}"
        );
        assert!(
            text.contains("omitted authoritative turn_complete"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn failed_root_terminal_does_not_wait_for_sender_drop() {
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        tx.send(serde_json::json!({
            "event_type": "run_error",
            "data": {"error": "failed"}
        }))
        .await
        .unwrap();
        tx.send(serde_json::json!({
            "event_type": "run_finished",
            "run_id": "root-run",
            "data": {}
        }))
        .await
        .unwrap();

        let response = sse_streaming_response("session-root".into(), "root-run".into(), None, rx);
        let body = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            body::to_bytes(response.into_body(), 1024 * 1024),
        )
        .await
        .expect("failed terminal must close the physical stream")
        .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("\"status\":\"failed\""), "{text}");
        assert!(!text.contains("contract_violation"), "{text}");
        drop(tx);
    }

    #[tokio::test(start_paused = true)]
    async fn descendant_errors_do_not_start_root_terminal_deadline() {
        for error_type in ["run_error", "error"] {
            let (tx, rx) = tokio::sync::mpsc::channel(4);
            tx.send(serde_json::json!({
                "type": error_type,
                "run_id": "child-run",
                "message": "child failed", "error_kind": "database_error"
            }))
            .await
            .unwrap();

            let response =
                sse_streaming_response("session-root".into(), "root-run".into(), None, rx);
            let body = tokio::spawn(body::to_bytes(response.into_body(), 1024 * 1024));
            tokio::task::yield_now().await;
            tokio::time::advance(SSE_DURABLE_TERMINAL_GRACE).await;
            assert!(
                !body.is_finished(),
                "a descendant error must not terminate the physical root stream"
            );

            tx.send(serde_json::json!({
                "event_type": "run_finished",
                "run_id": "root-run",
                "data": {}
            }))
            .await
            .unwrap();
            tx.send(serde_json::json!({
                "type": "turn_complete",
                "continuation_owner": "server"
            }))
            .await
            .unwrap();
            let text = String::from_utf8(body.await.unwrap().unwrap().to_vec()).unwrap();
            assert!(text.contains("\"run_id\":\"child-run\""), "{text}");
            assert!(!text.contains("\"status\":\"failed\""), "{text}");
            drop(tx);
        }
    }

    #[tokio::test]
    async fn turn_completion_without_run_receipt_keeps_outcome_unconfirmed() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send(serde_json::json!({
            "type": "turn_complete",
            "continuation_owner": "server"
        }))
        .await
        .unwrap();

        let response = sse_streaming_response("session-root".into(), "root-run".into(), None, rx);
        let body = body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();

        assert!(text.contains("\"type\":\"turn_complete\""), "{text}");
        assert!(!text.contains("\"type\":\"run_finished\""), "{text}");
        assert!(text.contains("stream_observation_interrupted"), "{text}");
        assert_eq!(text.matches("data: [DONE]\n\n").count(), 1);
        drop(tx);
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

        let response = sse_streaming_response("session-large".into(), "run-large".into(), None, rx);
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
        assert_eq!(text.matches("data: [DONE]\n\n").count(), 1);
        assert!(text.ends_with("data: [DONE]\n\n"));
    }

    #[tokio::test]
    async fn streaming_response_projects_runtime_feedback_before_exactly_one_done_marker() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let frame = serde_json::json!({
            "schema_version": 4,
            "identity": {
                "session_id": "session-feedback",
                "run_id": "run-feedback",
                "topology": "cli_server"
            }
        });
        tx.send(serde_json::json!({
            "type": "runtime_feedback",
            "runtime_feedback": frame,
            "internal_diagnostic": "must not cross the public boundary",
        }))
        .await
        .expect("queue feedback");
        tx.send(serde_json::json!({
            "event_type": "run_finished",
            "index": 2,
            "data": {"status": "completed"}
        }))
        .await
        .expect("queue terminal");
        drop(tx);

        let response =
            sse_streaming_response("session-feedback".into(), "run-feedback".into(), None, rx);
        let body = body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("SSE body");
        let text = String::from_utf8(body.to_vec()).expect("utf8 SSE");

        assert!(text.contains("\"type\":\"runtime_feedback\""));
        assert!(text.contains("\"run_id\":\"run-feedback\""));
        assert!(!text.contains("internal_diagnostic"));
        assert_eq!(text.matches("data: [DONE]\n\n").count(), 1);
        assert!(text.ends_with("data: [DONE]\n\n"));
    }
}
