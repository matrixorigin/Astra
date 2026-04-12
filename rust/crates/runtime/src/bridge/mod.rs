use super::*;

pub mod side_effects;

use self::side_effects::{
    build_bridge_response_guard_error_event, build_bridge_response_guard_side_effect_payloads,
    build_bridge_side_effect_payloads, dispatch_bridge_side_effect_request,
    run_bridge_hook_side_effects, sync_bridge_state_event, take_bridge_explain_event,
    take_bridge_prompt_fingerprints, take_bridge_side_effect_inputs, take_bridge_tail_update_args,
    take_bridge_warning_event,
};

pub mod circuit_breaker;
pub mod rate_limit_cooldown;
pub mod sse_events;

use self::circuit_breaker::{BridgeHealthMetrics, CircuitBreaker};
pub use self::rate_limit_cooldown::{
    CooldownReason, PerModelCooldown, RateLimitAction, RateLimitCooldown, RateLimitMetrics,
    RateLimitState,
};

/// Header allow-list predicate: only `x-mo-*` and `authorization` headers
/// are forwarded to the upstream bridge.
pub(crate) fn is_allowed_bridge_header(name: &str) -> bool {
    name.starts_with("x-mo-") || name == "authorization"
}
use self::sse_events::{
    bridge_state_tool_signatures, build_cloud_loop_progress_event_from_frame,
    build_cloud_tool_result_event_from_frame, build_error_event_from_frame,
    build_reasoning_delta_event_from_frame, build_text_delta_event_from_frame,
    build_token_usage_from_usage_event, build_tool_call_event_from_frame,
    build_tool_call_start_event_from_frame, build_tool_result_quality_event_from_frame,
    build_turn_complete_event_from_bridge_state, build_usage_event_from_frame, find_sse_frame_end,
    is_explain_frame, is_session_info_frame, is_turn_complete_frame, is_warning_frame,
    parse_bridge_state_frame, render_sse_json,
};

use crate::turn::routing::max_tool_rounds;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio_util::sync::CancellationToken;

const TOOL_RESULT_AUDIT_CHARS: usize = 4000;
/// Safety limit for SSE frame buffer. Prevents OOM if a client is slow or a
/// response is unexpectedly large. 16 MB accommodates any realistic SSE stream.
const MAX_SSE_BUFFER_BYTES: usize = 16 * 1024 * 1024;
/// Cap buffered non-SSE error bodies so upstream 5xx/JSON responses cannot
/// force unbounded memory growth while we normalize them into a local SSE error.
const MAX_BRIDGE_ERROR_BODY_BYTES: usize = 16 * 1024;

fn synthesized_session_info_event(session_id: &str, run_id: Option<&str>) -> serde_json::Value {
    let mut event = serde_json::json!({
        "type": "session_info",
        "session_id": session_id,
    });
    if let Some(run_id) = run_id
        && let Some(obj) = event.as_object_mut()
    {
        obj.insert(
            "run_id".to_string(),
            serde_json::Value::String(run_id.to_string()),
        );
    }
    event
}

struct BridgeResponseStream<S> {
    stream: Pin<Box<S>>,
    _disconnect_guard: Option<crate::turn::llm_client::CancelOnClientDisconnect>,
}

impl<S> BridgeResponseStream<S>
where
    S: futures_util::stream::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    fn new(stream: S, client_cancel: Option<Arc<CancellationToken>>) -> Self {
        Self {
            stream: Box::pin(stream),
            _disconnect_guard: client_cancel.map(crate::turn::llm_client::CancelOnClientDisconnect::new),
        }
    }
}

impl<S> futures_util::stream::Stream for BridgeResponseStream<S>
where
    S: futures_util::stream::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.stream.as_mut().poll_next(cx)
    }
}

fn is_bridge_sse_content_type(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|value| value.starts_with("text/event-stream"))
}

fn bridge_http_response_health(status: StatusCode, is_sse: bool) -> (bool, bool) {
    let is_success = status.is_success() && is_sse;
    let should_trip_breaker =
        status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS || !is_sse;
    (is_success, should_trip_breaker)
}

fn bridge_error_sse_message(status: StatusCode, body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return format!(
            "Bridge returned HTTP {} {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or("error")
        );
    }
    format!(
        "Bridge returned HTTP {} {}: {}",
        status.as_u16(),
        status.canonical_reason().unwrap_or("error"),
        trimmed
    )
}

fn bridge_status_to_sse_error_code(status: StatusCode) -> &'static str {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "AUTH_ERROR",
        StatusCode::NOT_FOUND => "NOT_FOUND",
        StatusCode::UNPROCESSABLE_ENTITY => "VALIDATION_ERROR",
        StatusCode::TOO_MANY_REQUESTS => "RATE_LIMIT",
        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT => {
            "UPSTREAM_ERROR"
        }
        _ => "INTERNAL_ERROR",
    }
}

fn bridge_error_sse_response(
    status: StatusCode,
    message: impl Into<String>,
    trusted_session_id: Option<&str>,
    trusted_run_id: Option<&str>,
) -> Response {
    let event = serde_json::json!({
        "type": "error",
        "message": message.into(),
        "code": bridge_status_to_sse_error_code(status),
        "retryable": status.is_server_error()
            || matches!(
                status,
                StatusCode::TOO_MANY_REQUESTS
                    | StatusCode::BAD_GATEWAY
                    | StatusCode::SERVICE_UNAVAILABLE
                    | StatusCode::GATEWAY_TIMEOUT
            ),
    });
    let mut body = Vec::new();
    if let Some(session_id) = trusted_session_id {
        body.extend_from_slice(&render_sse_json(synthesized_session_info_event(
            session_id,
            trusted_run_id,
        )));
    }
    body.extend_from_slice(&render_sse_json(event));
    sse_stream_response(StatusCode::OK, Body::from(body))
}

fn trusted_bridge_identity(headers: &HeaderMap) -> (Option<String>, Option<String>) {
    let trusted_session_id = headers
        .get("x-mo-session-id")
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string)
        .filter(|value| !value.is_empty());
    let trusted_turn_chain_id = headers
        .get("x-mo-turn-chain-id")
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string)
        .filter(|value| !value.is_empty());
    (trusted_session_id, trusted_turn_chain_id)
}

async fn read_bridge_error_body_excerpt<S, E>(mut stream: S, max_bytes: usize) -> String
where
    S: futures_util::Stream<Item = Result<Bytes, E>> + Unpin,
{
    use futures_util::StreamExt;

    let mut collected = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            break;
        };
        let remaining = max_bytes.saturating_sub(collected.len());
        if remaining == 0 {
            truncated = true;
            break;
        }
        if chunk.len() > remaining {
            collected.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        collected.extend_from_slice(&chunk);
    }
    let mut text = String::from_utf8_lossy(&collected).into_owned();
    if truncated {
        text.push_str("\n… [truncated]");
    }
    text
}

fn build_max_rounds_turn_complete_event(
    bridge_state: &serde_json::Map<String, serde_json::Value>,
    trusted_execution_state: Option<&serde_json::Value>,
    latest_user_message: Option<&str>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut turn_complete = build_turn_complete_event_from_bridge_state(
        bridge_state,
        trusted_execution_state,
        latest_user_message,
    );
    turn_complete.insert(
        "max_rounds_exceeded".to_string(),
        serde_json::Value::Bool(true),
    );
    turn_complete
}

#[async_trait]
pub trait ChatTurnBridge: Send + Sync {
    /// Last argument: optional cancel token — HTTP `/chat/turn` passes one so dropping the SSE body
    /// (client disconnect) stops in-process LLM streaming promptly.
    #[allow(clippy::too_many_arguments)]
    async fn forward(
        &self,
        headers: &HeaderMap,
        body: Bytes,
        turn_core_event_writer: Arc<dyn TurnCoreEventWriter>,
        turn_tool_event_writer: Arc<dyn TurnToolEventWriter>,
        turn_hook_db_writer: Arc<dyn TurnHookDbWriter>,
        turn_reflection_state_store: Arc<dyn TurnReflectionStateStore>,
        turn_reflection_lesson_writer: Arc<dyn TurnReflectionLessonWriter>,
        turn_observer_worker: Arc<dyn TurnObserverWorker>,
        turn_auxiliary_event_writer: Arc<dyn TurnAuxiliaryEventWriter>,
        turn_session_activity_writer: Arc<dyn TurnSessionActivityWriter>,
        client_cancel: Option<Arc<CancellationToken>>,
    ) -> Result<Response, (StatusCode, String)>;
}

#[derive(Clone)]
pub(crate) struct HttpChatTurnBridge {
    url: String,
    client: reqwest::Client,
    cache: Arc<tokio::sync::Mutex<SessionCache>>,
    circuit_breaker: Arc<CircuitBreaker>,
    health_metrics: Arc<BridgeHealthMetrics>,
    turn_learning_writer: Option<Arc<dyn TurnLearningWriter>>,
}

impl std::fmt::Debug for HttpChatTurnBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpChatTurnBridge")
            .field("url", &self.url)
            .field("has_learning_writer", &self.turn_learning_writer.is_some())
            .finish()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct UnavailableChatTurnBridge;

#[derive(Clone, Debug)]
struct BridgeSideEffectRequestContext {
    messages: Vec<Value>,
    tool_results: Vec<Value>,
    agent_id: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct InMemoryTurnReflectionStateStore {
    pub(crate) state: Arc<tokio::sync::Mutex<HashMap<String, TurnReflectionMark>>>,
}

#[derive(Clone, Debug)]
pub(crate) struct NoopTurnReflectionLessonWriter;

#[derive(Clone, Debug)]
pub struct DatabaseTurnReflectionLessonWriter {
    pub(crate) base_url: String,
    pub(crate) master_key: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct NoopTurnObserverWorker;

#[derive(Clone, Debug)]
pub struct DatabaseTurnObserverWorker {
    pub(crate) base_url: String,
    pub(crate) master_key: Option<String>,
}

impl HttpChatTurnBridge {
    pub(crate) fn new(url: String, cache: Arc<tokio::sync::Mutex<SessionCache>>) -> Self {
        Self {
            url,
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("chat turn bridge client should build"),
            cache,
            circuit_breaker: Arc::new(CircuitBreaker::with_defaults()),
            health_metrics: Arc::new(BridgeHealthMetrics::new()),
            turn_learning_writer: None,
        }
    }

    /// Set the pipeline learning writer for this bridge.
    pub(crate) fn with_learning_writer(mut self, writer: Arc<dyn TurnLearningWriter>) -> Self {
        self.turn_learning_writer = Some(writer);
        self
    }

    /// Get a snapshot of bridge health metrics.
    #[cfg(test)]
    pub(crate) fn health_snapshot(&self) -> circuit_breaker::BridgeHealthSnapshot {
        self.health_metrics.snapshot()
    }
}

#[async_trait]
impl ChatTurnBridge for UnavailableChatTurnBridge {
    async fn forward(
        &self,
        headers: &HeaderMap,
        _body: Bytes,
        _turn_core_event_writer: Arc<dyn TurnCoreEventWriter>,
        _turn_tool_event_writer: Arc<dyn TurnToolEventWriter>,
        _turn_hook_db_writer: Arc<dyn TurnHookDbWriter>,
        _turn_reflection_state_store: Arc<dyn TurnReflectionStateStore>,
        _turn_reflection_lesson_writer: Arc<dyn TurnReflectionLessonWriter>,
        _turn_observer_worker: Arc<dyn TurnObserverWorker>,
        _turn_auxiliary_event_writer: Arc<dyn TurnAuxiliaryEventWriter>,
        _turn_session_activity_writer: Arc<dyn TurnSessionActivityWriter>,
        _client_cancel: Option<Arc<CancellationToken>>,
    ) -> Result<Response, (StatusCode, String)> {
        let (trusted_session_id, _trusted_turn_chain_id) = trusted_bridge_identity(headers);
        Ok(bridge_error_sse_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "chat turn bridge disabled. Configure CHAT_TURN_BRIDGE_URL to a reachable /internal/chat/turn endpoint (example: compatible chat-turn bridge service), then restart API."
                .to_string(),
            trusted_session_id.as_deref(),
            None,
        ))
    }
}

#[async_trait]
impl ChatTurnBridge for HttpChatTurnBridge {
    async fn forward(
        &self,
        headers: &HeaderMap,
        body: Bytes,
        turn_core_event_writer: Arc<dyn TurnCoreEventWriter>,
        turn_tool_event_writer: Arc<dyn TurnToolEventWriter>,
        turn_hook_db_writer: Arc<dyn TurnHookDbWriter>,
        turn_reflection_state_store: Arc<dyn TurnReflectionStateStore>,
        turn_reflection_lesson_writer: Arc<dyn TurnReflectionLessonWriter>,
        turn_observer_worker: Arc<dyn TurnObserverWorker>,
        turn_auxiliary_event_writer: Arc<dyn TurnAuxiliaryEventWriter>,
        turn_session_activity_writer: Arc<dyn TurnSessionActivityWriter>,
        client_cancel: Option<Arc<CancellationToken>>,
    ) -> Result<Response, (StatusCode, String)> {
        let mut bridge_headers = HeaderMap::new();
        let side_effect_request_context = parse_bridge_side_effect_request_context(&body);
        let mut request = self
            .client
            .post(&self.url)
            .header("content-type", "application/json");
        for (header_name, value) in headers.iter() {
            if is_allowed_bridge_header(header_name.as_str())
                && let Ok(value_str) = value.to_str()
            {
                bridge_headers.insert(header_name.clone(), value.clone());
                request = request.header(header_name.as_str(), value_str);
            }
        }
        request = request.body(body.to_vec());
        let (trusted_session_id, trusted_turn_chain_id) = trusted_bridge_identity(&bridge_headers);

        // Circuit breaker: fast-reject if bridge is in open state
        if !self.circuit_breaker.allow_request() {
            let metrics = self.circuit_breaker.metrics();
            return Ok(bridge_error_sse_response(
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "Bridge circuit breaker is open (consecutive failures: {}, state: {}). Retry after recovery timeout.",
                    metrics.consecutive_failures, metrics.state
                ),
                trusted_session_id.as_deref(),
                trusted_turn_chain_id.as_deref(),
            ));
        }

        let request_start = std::time::Instant::now();
        let response = match request.send().await {
            Ok(resp) => resp,
            Err(error) => {
                let latency_ms = request_start.elapsed().as_millis() as u64;
                let is_timeout = error.is_timeout();
                self.circuit_breaker.record_failure();
                self.health_metrics
                    .record_request(latency_ms, false, is_timeout);
                return Ok(bridge_error_sse_response(
                    StatusCode::BAD_GATEWAY,
                    error.to_string(),
                    trusted_session_id.as_deref(),
                    None,
                ));
            }
        };
        let status = StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::OK);
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string);
        let is_sse = is_bridge_sse_content_type(content_type.as_deref());
        // These bridge headers were synthesized by server-side bridge prep before dispatch, so
        // they remain the authoritative session/run identity even if the upstream response is a
        // non-SSE error body with no usable metadata of its own.
        let request_latency_ms = request_start.elapsed().as_millis() as u64;
        let (is_success, should_trip_breaker) = bridge_http_response_health(status, is_sse);
        self.health_metrics
            .record_request(request_latency_ms, is_success, false);
        if is_success {
            self.circuit_breaker.record_success();
        } else if should_trip_breaker {
            self.circuit_breaker.record_failure();
        }
        if !is_sse {
            let error_body = read_bridge_error_body_excerpt(
                response.bytes_stream(),
                MAX_BRIDGE_ERROR_BODY_BYTES,
            )
            .await;
            return Ok(bridge_error_sse_response(
                status,
                bridge_error_sse_message(status, &error_body),
                trusted_session_id.as_deref(),
                None,
            ));
        }
        let filtered_stream = filter_bridge_state_events(
            response.bytes_stream(),
            self.cache.clone(),
            if status == StatusCode::OK
                && content_type
                    .as_deref()
                    .is_some_and(|value| value.starts_with("text/event-stream"))
            {
                trusted_session_id.clone()
            } else {
                None
            },
            if status == StatusCode::OK
                && content_type
                    .as_deref()
                    .is_some_and(|value| value.starts_with("text/event-stream"))
            {
                bridge_headers
                    .get("x-mo-user-id")
                    .and_then(|value| value.to_str().ok())
                    .map(ToString::to_string)
            } else {
                None
            },
            if status == StatusCode::OK
                && content_type
                    .as_deref()
                    .is_some_and(|value| value.starts_with("text/event-stream"))
            {
                bridge_headers
                    .get("x-mo-execution-state-b64")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|encoded| URL_SAFE.decode(encoded).ok())
                    .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            } else {
                None
            },
            if status == StatusCode::OK
                && content_type
                    .as_deref()
                    .is_some_and(|value| value.starts_with("text/event-stream"))
            {
                trusted_turn_chain_id.clone()
            } else {
                None
            },
            if status == StatusCode::OK
                && content_type
                    .as_deref()
                    .is_some_and(|value| value.starts_with("text/event-stream"))
            {
                bridge_headers
                    .get("x-mo-user-query-event-id")
                    .and_then(|value| value.to_str().ok())
                    .map(ToString::to_string)
            } else {
                None
            },
            if status == StatusCode::OK
                && content_type
                    .as_deref()
                    .is_some_and(|value| value.starts_with("text/event-stream"))
            {
                bridge_headers
                    .get("x-mo-routing-meta-b64")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|encoded| URL_SAFE.decode(encoded).ok())
                    .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            } else {
                None
            },
            if status == StatusCode::OK
                && content_type
                    .as_deref()
                    .is_some_and(|value| value.starts_with("text/event-stream"))
            {
                side_effect_request_context
            } else {
                None
            },
            turn_core_event_writer,
            turn_tool_event_writer,
            turn_hook_db_writer,
            turn_reflection_state_store,
            turn_reflection_lesson_writer,
            turn_observer_worker,
            turn_auxiliary_event_writer,
            turn_session_activity_writer,
            self.turn_learning_writer.clone(),
        );
        let response_stream = BridgeResponseStream::new(filtered_stream, client_cancel);
        Ok(sse_stream_response(
            StatusCode::OK,
            Body::from_stream(response_stream),
        ))
    }
}

fn parse_bridge_side_effect_request_context(
    body: &Bytes,
) -> Option<BridgeSideEffectRequestContext> {
    let payload = serde_json::from_slice::<Value>(body).ok()?;
    let object = payload.as_object()?;
    Some(BridgeSideEffectRequestContext {
        messages: object
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        tool_results: object
            .get("tool_results")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        agent_id: object
            .get("agent_id")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .filter(|value| !value.is_empty()),
    })
}

#[allow(clippy::too_many_arguments)]
fn filter_bridge_state_events<S>(
    mut stream: S,
    cache: Arc<tokio::sync::Mutex<SessionCache>>,
    trusted_session_id: Option<String>,
    side_effect_user_id: Option<String>,
    trusted_execution_state: Option<serde_json::Value>,
    trusted_turn_chain_id: Option<String>,
    trusted_user_query_event_id: Option<String>,
    trusted_routing_meta: Option<serde_json::Value>,
    side_effect_request_context: Option<BridgeSideEffectRequestContext>,
    turn_core_event_writer: Arc<dyn TurnCoreEventWriter>,
    turn_tool_event_writer: Arc<dyn TurnToolEventWriter>,
    turn_hook_db_writer: Arc<dyn TurnHookDbWriter>,
    turn_reflection_state_store: Arc<dyn TurnReflectionStateStore>,
    turn_reflection_lesson_writer: Arc<dyn TurnReflectionLessonWriter>,
    turn_observer_worker: Arc<dyn TurnObserverWorker>,
    turn_auxiliary_event_writer: Arc<dyn TurnAuxiliaryEventWriter>,
    turn_session_activity_writer: Arc<dyn TurnSessionActivityWriter>,
    turn_learning_writer: Option<Arc<dyn TurnLearningWriter>>,
) -> impl futures_util::stream::Stream<Item = Result<Bytes, std::io::Error>>
where
    S: futures_util::stream::Stream<Item = Result<Bytes, reqwest::Error>> + Unpin + Send + 'static,
{
    fn latest_user_message_from_side_effect_inputs(
        side_effect_inputs: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Option<String> {
        side_effect_inputs
            .and_then(|inputs| inputs.get("messages"))
            .and_then(serde_json::Value::as_array)
            .and_then(|messages| {
                messages.iter().rev().find_map(|message| {
                    let object = message.as_object()?;
                    if object.get("role").and_then(serde_json::Value::as_str) != Some("user") {
                        return None;
                    }
                    object
                        .get("content")
                        .and_then(serde_json::Value::as_str)
                        .map(ToString::to_string)
                })
            })
    }

    stream! {
        if let Some(session_id) = trusted_session_id.as_deref() {
            yield Ok(Bytes::from(render_sse_json(synthesized_session_info_event(
                session_id,
                trusted_turn_chain_id.as_deref(),
            ))));
        }
        let mut buffer = Vec::new();
        let mut pending_bridge_state: Option<serde_json::Map<String, serde_json::Value>> = None;
        let mut pending_followup_user_message: Option<String> = None;
        let mut pending_warning_event: Option<serde_json::Map<String, serde_json::Value>> = None;
        let mut pending_explain_event: Option<serde_json::Map<String, serde_json::Value>> = None;
        let mut latest_token_usage: Option<serde_json::Value> = None;
        let mut suppress_next_turn_complete = false;
        let mut tool_rounds: i64 = 0;
        let mut received_turn_complete = false;
        let mut force_max_rounds_completion = false;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(chunk) => {
                    buffer.extend_from_slice(&chunk);
                    if buffer.len() > MAX_SSE_BUFFER_BYTES {
                        yield Err(std::io::Error::other(format!(
                            "SSE buffer exceeded {} bytes — possible slow client or malformed stream",
                            MAX_SSE_BUFFER_BYTES
                        )));
                        return;
                    }
                    while let Some(end) = find_sse_frame_end(&buffer) {
                        let frame = buffer.drain(..end + 2).collect::<Vec<_>>();
                        if is_session_info_frame(&frame) {
                            if trusted_session_id.is_some() {
                                continue;
                            }
                            yield Ok(Bytes::from(frame));
                            continue;
                        } else if suppress_next_turn_complete && is_turn_complete_frame(&frame) {
                            suppress_next_turn_complete = false;
                            continue;
                        } else if let Some(mut bridge_state) = parse_bridge_state_frame(&frame) {
                            let Some(trusted_session_id) = trusted_session_id.as_deref() else {
                                yield Ok(Bytes::from(frame));
                                continue;
                            };
                            let prompt_fingerprints =
                                take_bridge_prompt_fingerprints(&mut bridge_state);
                            let tail_update_args =
                                take_bridge_tail_update_args(&mut bridge_state);
                            let warning_event = take_bridge_warning_event(&mut bridge_state);
                            let explain_event = take_bridge_explain_event(&mut bridge_state);
                            let side_effect_inputs =
                                take_bridge_side_effect_inputs(&mut bridge_state);
                            let followup_user_message =
                                latest_user_message_from_side_effect_inputs(side_effect_inputs.as_ref());
                            if let Some(response_guard_error) = tail_update_args
                                .as_ref()
                                .and_then(|tail_update_args| {
                                    build_bridge_response_guard_error_event(
                                        tail_update_args,
                                        &prompt_fingerprints,
                                    )
                                })
                            {
                                pending_bridge_state = None;
                                pending_followup_user_message = None;
                                pending_warning_event = None;
                                pending_explain_event = None;
                                if let Some(side_effect_inputs) = side_effect_inputs.as_ref()
                                    && let Some((persist_payload, hook_payload)) =
                                        build_bridge_response_guard_side_effect_payloads(
                                            side_effect_user_id.as_deref(),
                                            trusted_session_id,
                                            &bridge_state,
                                            side_effect_inputs,
                                            tail_update_args.as_ref(),
                                            trusted_turn_chain_id.as_deref(),
                                            trusted_user_query_event_id.as_deref(),
                                            latest_token_usage.as_ref(),
                                            trusted_routing_meta.as_ref(),
                                            side_effect_request_context.as_ref(),
                                        )
                                {
                                    dispatch_bridge_side_effect_request(
                                        Some(persist_payload),
                                        turn_core_event_writer.clone(),
                                        turn_tool_event_writer.clone(),
                                        turn_auxiliary_event_writer.clone(),
                                        turn_session_activity_writer.clone(),
                                    );
                                    run_bridge_hook_side_effects(
                                        Some(hook_payload),
                                        turn_hook_db_writer.clone(),
                                        turn_reflection_state_store.clone(),
                                        turn_reflection_lesson_writer.clone(),
                                        turn_observer_worker.clone(),
                                        turn_learning_writer.clone(),
                                    );
                                }
                                suppress_next_turn_complete = true;
                                if let Some(warning_event) = warning_event {
                                    yield Ok(Bytes::from(render_sse_json(
                                        serde_json::Value::Object(warning_event),
                                    )));
                                }
                                if let Some(explain_event) = explain_event {
                                    yield Ok(Bytes::from(render_sse_json(
                                        serde_json::Value::Object(explain_event),
                                    )));
                                }
                                yield Ok(Bytes::from(render_sse_json(
                                    serde_json::Value::Object(response_guard_error),
                                )));
                                yield Ok(Bytes::from(render_sse_json(serde_json::Value::Object(
                                    build_turn_complete_event_from_bridge_state(
                                        &bridge_state,
                                        trusted_execution_state.as_ref(),
                                        followup_user_message.as_deref(),
                                    ),
                                ))));
                                received_turn_complete = true;
                                continue;
                            }
                            let synced_bridge_state =
                                sync_bridge_state_event(
                                    cache.clone(),
                                    trusted_session_id,
                                    bridge_state,
                                    tail_update_args.as_ref(),
                                    trusted_turn_chain_id.as_deref(),
                                    trusted_user_query_event_id.as_deref(),
                                )
                                .await;
                            if let Some(side_effect_inputs) = side_effect_inputs.as_ref()
                                && let Some((persist_payload, hook_payload)) =
                                    build_bridge_side_effect_payloads(
                                        side_effect_user_id.as_deref(),
                                        trusted_session_id,
                                        &synced_bridge_state,
                                        side_effect_inputs,
                                        tail_update_args.as_ref(),
                                        latest_token_usage.as_ref(),
                                        trusted_routing_meta.as_ref(),
                                        side_effect_request_context.as_ref(),
                                    )
                                {
                                    dispatch_bridge_side_effect_request(
                                        Some(persist_payload),
                                        turn_core_event_writer.clone(),
                                        turn_tool_event_writer.clone(),
                                        turn_auxiliary_event_writer.clone(),
                                        turn_session_activity_writer.clone(),
                                    );
                                    run_bridge_hook_side_effects(
                                        Some(hook_payload),
                                        turn_hook_db_writer.clone(),
                                        turn_reflection_state_store.clone(),
                                        turn_reflection_lesson_writer.clone(),
                                        turn_observer_worker.clone(),
                                        turn_learning_writer.clone(),
                                    );
                                }
                            pending_bridge_state = Some(synced_bridge_state);
                            pending_followup_user_message = followup_user_message;
                            pending_warning_event = warning_event;
                            pending_explain_event = explain_event;

                            // Track tool rounds from bridge_state frames
                            if let Some(sigs) = pending_bridge_state.as_ref().and_then(bridge_state_tool_signatures)
                                && !sigs.is_empty() {
                                    tool_rounds += 1;
                                    if tool_rounds > max_tool_rounds() {
                                        astra_core::agent_warn!("bridge", "Turn exceeded max_tool_rounds ({}), forcing completion", max_tool_rounds());
                                        if let Some(warning_event) = pending_warning_event.take() {
                                            yield Ok(Bytes::from(render_sse_json(
                                                serde_json::Value::Object(warning_event),
                                            )));
                                        }
                                        if let Some(explain_event) = pending_explain_event.take() {
                                            yield Ok(Bytes::from(render_sse_json(
                                                serde_json::Value::Object(explain_event),
                                            )));
                                        }
                                        let bridge_state = pending_bridge_state
                                            .as_ref()
                                            .expect("pending bridge state should exist");
                                        yield Ok(Bytes::from(render_sse_json(
                                            serde_json::Value::Object(
                                                build_max_rounds_turn_complete_event(
                                                    bridge_state,
                                                    trusted_execution_state.as_ref(),
                                                    pending_followup_user_message.as_deref(),
                                                ),
                                            ),
                                        )));
                                        return;
                                    }
                                }
                        } else if pending_bridge_state.is_some() {
                            if let Some(text_delta_event) = build_text_delta_event_from_frame(&frame) {
                                yield Ok(Bytes::from(render_sse_json(
                                    serde_json::Value::Object(text_delta_event),
                                )));
                            } else if let Some(reasoning_delta_event) =
                                build_reasoning_delta_event_from_frame(&frame)
                            {
                                yield Ok(Bytes::from(render_sse_json(
                                    serde_json::Value::Object(reasoning_delta_event),
                                )));
                            } else if let Some(usage_event) = build_usage_event_from_frame(&frame) {
                                latest_token_usage = build_token_usage_from_usage_event(&usage_event);
                                yield Ok(Bytes::from(render_sse_json(
                                    serde_json::Value::Object(usage_event),
                                )));
                            } else if let Some(tool_result_quality_event) =
                                build_tool_result_quality_event_from_frame(&frame)
                            {
                                yield Ok(Bytes::from(render_sse_json(
                                    serde_json::Value::Object(tool_result_quality_event),
                                )));
                            } else if let Some(cloud_loop_progress_event) =
                                build_cloud_loop_progress_event_from_frame(&frame)
                            {
                                yield Ok(Bytes::from(render_sse_json(
                                    serde_json::Value::Object(cloud_loop_progress_event),
                                )));
                            } else if let Some(cloud_tool_result_event) =
                                build_cloud_tool_result_event_from_frame(&frame)
                            {
                                yield Ok(Bytes::from(render_sse_json(
                                    serde_json::Value::Object(cloud_tool_result_event),
                                )));
                            } else if let Some(error_event) = build_error_event_from_frame(&frame) {
                                yield Ok(Bytes::from(render_sse_json(
                                    serde_json::Value::Object(error_event),
                                )));
                            } else if let Some(tool_call_event) = build_tool_call_event_from_frame(&frame) {
                                yield Ok(Bytes::from(render_sse_json(
                                    serde_json::Value::Object(tool_call_event),
                                )));
                            } else if let Some(tool_call_start_event) =
                                build_tool_call_start_event_from_frame(&frame)
                            {
                                yield Ok(Bytes::from(render_sse_json(
                                    serde_json::Value::Object(tool_call_start_event),
                                )));
                            } else if is_warning_frame(&frame) {
                                if pending_warning_event.is_none() {
                                    yield Ok(Bytes::from(frame));
                                }
                                continue;
                            } else if is_explain_frame(&frame) {
                                if pending_explain_event.is_none() {
                                    yield Ok(Bytes::from(frame));
                                }
                                continue;
                            } else if is_turn_complete_frame(&frame) {
                                if let Some(warning_event) = pending_warning_event.take() {
                                    yield Ok(Bytes::from(render_sse_json(
                                        serde_json::Value::Object(warning_event),
                                    )));
                                }
                                if let Some(explain_event) = pending_explain_event.take() {
                                    yield Ok(Bytes::from(render_sse_json(
                                        serde_json::Value::Object(explain_event),
                                    )));
                                }
                                let bridge_state = pending_bridge_state
                                    .as_ref()
                                    .expect("pending bridge state should exist");
                                yield Ok(Bytes::from(render_sse_json(
                                    serde_json::Value::Object(
                                        build_turn_complete_event_from_bridge_state(
                                            bridge_state,
                                            trusted_execution_state.as_ref(),
                                            pending_followup_user_message.as_deref(),
                                        ),
                                    ),
                                )));
                                received_turn_complete = true;
                                pending_bridge_state = None;
                                pending_followup_user_message = None;
                                pending_warning_event = None;
                                pending_explain_event = None;
                                latest_token_usage = None;
                            } else {
                                yield Ok(Bytes::from(frame));
                            }
                        } else if let Some(text_delta_event) = build_text_delta_event_from_frame(&frame) {
                            yield Ok(Bytes::from(render_sse_json(
                                serde_json::Value::Object(text_delta_event),
                            )));
                        } else if let Some(reasoning_delta_event) =
                            build_reasoning_delta_event_from_frame(&frame)
                        {
                            yield Ok(Bytes::from(render_sse_json(
                                serde_json::Value::Object(reasoning_delta_event),
                            )));
                        } else if let Some(usage_event) = build_usage_event_from_frame(&frame) {
                            latest_token_usage = build_token_usage_from_usage_event(&usage_event);
                            yield Ok(Bytes::from(render_sse_json(
                                serde_json::Value::Object(usage_event),
                            )));
                        } else if let Some(tool_result_quality_event) =
                            build_tool_result_quality_event_from_frame(&frame)
                        {
                            yield Ok(Bytes::from(render_sse_json(
                                serde_json::Value::Object(tool_result_quality_event),
                            )));
                        } else if let Some(cloud_loop_progress_event) =
                            build_cloud_loop_progress_event_from_frame(&frame)
                        {
                            yield Ok(Bytes::from(render_sse_json(
                                serde_json::Value::Object(cloud_loop_progress_event),
                            )));
                        } else if let Some(cloud_tool_result_event) =
                            build_cloud_tool_result_event_from_frame(&frame)
                        {
                            yield Ok(Bytes::from(render_sse_json(
                                serde_json::Value::Object(cloud_tool_result_event),
                            )));
                        } else if let Some(error_event) = build_error_event_from_frame(&frame) {
                            yield Ok(Bytes::from(render_sse_json(
                                serde_json::Value::Object(error_event),
                            )));
                        } else if let Some(tool_call_event) = build_tool_call_event_from_frame(&frame) {
                            yield Ok(Bytes::from(render_sse_json(
                                serde_json::Value::Object(tool_call_event),
                            )));
                        } else if let Some(tool_call_start_event) =
                            build_tool_call_start_event_from_frame(&frame)
                        {
                            yield Ok(Bytes::from(render_sse_json(
                                serde_json::Value::Object(tool_call_start_event),
                            )));
                        } else if is_warning_frame(&frame) || is_explain_frame(&frame) {
                            yield Ok(Bytes::from(frame));
                            continue;
                        } else {
                            yield Ok(Bytes::from(frame));
                        }
                    }
                }
                Err(error) => {
                    if let Some(warning_event) = pending_warning_event.take() {
                        yield Ok(Bytes::from(render_sse_json(serde_json::Value::Object(
                            warning_event,
                        ))));
                    }
                    if let Some(explain_event) = pending_explain_event.take() {
                        yield Ok(Bytes::from(render_sse_json(serde_json::Value::Object(
                            explain_event,
                        ))));
                    }
                    yield Ok(Bytes::from(render_sse_json(serde_json::Value::Object(
                        build_stream_error_event(
                            &format!("Failed to read bridge response: {error}"),
                            "UPSTREAM_ERROR",
                            true,
                        ),
                    ))));
                    if let Some(bridge_state) = pending_bridge_state.take() {
                        yield Ok(Bytes::from(render_sse_json(serde_json::Value::Object(
                            if force_max_rounds_completion {
                                build_max_rounds_turn_complete_event(
                                    &bridge_state,
                                    trusted_execution_state.as_ref(),
                                    pending_followup_user_message.as_deref(),
                                )
                            } else {
                                build_turn_complete_event_from_bridge_state(
                                    &bridge_state,
                                    trusted_execution_state.as_ref(),
                                    pending_followup_user_message.as_deref(),
                                )
                            },
                        ))));
                    }
                    return;
                }
            }
        }

        if !buffer.is_empty() {
            if let Some(mut bridge_state) = parse_bridge_state_frame(&buffer) {
                let Some(trusted_session_id) = trusted_session_id.as_deref() else {
                    yield Ok(Bytes::from(buffer));
                    return;
                };
                let prompt_fingerprints = take_bridge_prompt_fingerprints(&mut bridge_state);
                let tail_update_args = take_bridge_tail_update_args(&mut bridge_state);
                let warning_event = take_bridge_warning_event(&mut bridge_state);
                let explain_event = take_bridge_explain_event(&mut bridge_state);
                let side_effect_inputs = take_bridge_side_effect_inputs(&mut bridge_state);
                let followup_user_message =
                    latest_user_message_from_side_effect_inputs(side_effect_inputs.as_ref());
                if let Some(response_guard_error) = tail_update_args
                    .as_ref()
                    .and_then(|tail_update_args| {
                        build_bridge_response_guard_error_event(
                            tail_update_args,
                            &prompt_fingerprints,
                        )
                    })
                {
                    if let Some(side_effect_inputs) = side_effect_inputs.as_ref()
                        && let Some((persist_payload, hook_payload)) =
                            build_bridge_response_guard_side_effect_payloads(
                                side_effect_user_id.as_deref(),
                                trusted_session_id,
                                &bridge_state,
                                side_effect_inputs,
                                tail_update_args.as_ref(),
                                trusted_turn_chain_id.as_deref(),
                                trusted_user_query_event_id.as_deref(),
                                latest_token_usage.as_ref(),
                                trusted_routing_meta.as_ref(),
                                side_effect_request_context.as_ref(),
                            )
                    {
                        dispatch_bridge_side_effect_request(
                            Some(persist_payload),
                            turn_core_event_writer.clone(),
                            turn_tool_event_writer.clone(),
                            turn_auxiliary_event_writer.clone(),
                            turn_session_activity_writer.clone(),
                        );
                        run_bridge_hook_side_effects(
                            Some(hook_payload),
                            turn_hook_db_writer.clone(),
                            turn_reflection_state_store.clone(),
                            turn_reflection_lesson_writer.clone(),
                            turn_observer_worker.clone(),
                            turn_learning_writer.clone(),
                        );
                    }
                    if let Some(warning_event) = warning_event {
                        yield Ok(Bytes::from(render_sse_json(
                            serde_json::Value::Object(warning_event),
                        )));
                    }
                    if let Some(explain_event) = explain_event {
                        yield Ok(Bytes::from(render_sse_json(
                            serde_json::Value::Object(explain_event),
                        )));
                    }
                    yield Ok(Bytes::from(render_sse_json(
                        serde_json::Value::Object(response_guard_error),
                    )));
                    yield Ok(Bytes::from(render_sse_json(serde_json::Value::Object(
                        build_turn_complete_event_from_bridge_state(
                            &bridge_state,
                            trusted_execution_state.as_ref(),
                            followup_user_message.as_deref(),
                        ),
                    ))));
                    received_turn_complete = true;
                } else {
                    let synced_bridge_state =
                        sync_bridge_state_event(
                            cache,
                            trusted_session_id,
                            bridge_state,
                            tail_update_args.as_ref(),
                            trusted_turn_chain_id.as_deref(),
                            trusted_user_query_event_id.as_deref(),
                        )
                        .await;
                    if let Some(side_effect_inputs) = side_effect_inputs.as_ref()
                        && let Some((persist_payload, hook_payload)) =
                            build_bridge_side_effect_payloads(
                                side_effect_user_id.as_deref(),
                                trusted_session_id,
                                &synced_bridge_state,
                                side_effect_inputs,
                                tail_update_args.as_ref(),
                                latest_token_usage.as_ref(),
                                trusted_routing_meta.as_ref(),
                                side_effect_request_context.as_ref(),
                            )
                        {
                            dispatch_bridge_side_effect_request(
                                Some(persist_payload),
                                turn_core_event_writer.clone(),
                                turn_tool_event_writer.clone(),
                                turn_auxiliary_event_writer.clone(),
                                turn_session_activity_writer.clone(),
                            );
                            run_bridge_hook_side_effects(
                                Some(hook_payload),
                                turn_hook_db_writer.clone(),
                                turn_reflection_state_store.clone(),
                                turn_reflection_lesson_writer.clone(),
                                turn_observer_worker.clone(),
                                turn_learning_writer.clone(),
                            );
                        }
                    pending_bridge_state = Some(synced_bridge_state);
                    pending_followup_user_message = followup_user_message;
                    pending_warning_event = warning_event;
                    pending_explain_event = explain_event;
                    if let Some(sigs) =
                        pending_bridge_state.as_ref().and_then(bridge_state_tool_signatures)
                        && !sigs.is_empty()
                    {
                        tool_rounds += 1;
                        if tool_rounds > max_tool_rounds() {
                            astra_core::agent_warn!(
                                "bridge",
                                "Turn exceeded max_tool_rounds ({}) in buffered tail frame, forcing completion",
                                max_tool_rounds()
                            );
                            force_max_rounds_completion = true;
                        }
                    }
                }
            } else {
                if is_session_info_frame(&buffer) {
                    if trusted_session_id.is_none() {
                        yield Ok(Bytes::from(buffer));
                    }
                } else if suppress_next_turn_complete && is_turn_complete_frame(&buffer) {
                } else if pending_bridge_state.is_some() {
                    if let Some(text_delta_event) = build_text_delta_event_from_frame(&buffer) {
                        yield Ok(Bytes::from(render_sse_json(
                            serde_json::Value::Object(text_delta_event),
                        )));
                    } else if let Some(reasoning_delta_event) =
                        build_reasoning_delta_event_from_frame(&buffer)
                    {
                        yield Ok(Bytes::from(render_sse_json(
                            serde_json::Value::Object(reasoning_delta_event),
                        )));
                    } else if let Some(usage_event) = build_usage_event_from_frame(&buffer) {
                        yield Ok(Bytes::from(render_sse_json(
                            serde_json::Value::Object(usage_event),
                        )));
                    } else if let Some(tool_result_quality_event) =
                        build_tool_result_quality_event_from_frame(&buffer)
                    {
                        yield Ok(Bytes::from(render_sse_json(
                            serde_json::Value::Object(tool_result_quality_event),
                        )));
                    } else if let Some(cloud_loop_progress_event) =
                        build_cloud_loop_progress_event_from_frame(&buffer)
                    {
                        yield Ok(Bytes::from(render_sse_json(
                            serde_json::Value::Object(cloud_loop_progress_event),
                        )));
                    } else if let Some(cloud_tool_result_event) =
                        build_cloud_tool_result_event_from_frame(&buffer)
                    {
                        yield Ok(Bytes::from(render_sse_json(
                            serde_json::Value::Object(cloud_tool_result_event),
                        )));
                    } else if let Some(error_event) = build_error_event_from_frame(&buffer) {
                        yield Ok(Bytes::from(render_sse_json(
                            serde_json::Value::Object(error_event),
                        )));
                    } else if let Some(tool_call_event) = build_tool_call_event_from_frame(&buffer) {
                        yield Ok(Bytes::from(render_sse_json(
                            serde_json::Value::Object(tool_call_event),
                        )));
                    } else if let Some(tool_call_start_event) =
                        build_tool_call_start_event_from_frame(&buffer)
                    {
                        yield Ok(Bytes::from(render_sse_json(
                            serde_json::Value::Object(tool_call_start_event),
                        )));
                    } else if is_warning_frame(&buffer) {
                        if pending_warning_event.is_none() {
                            yield Ok(Bytes::from(buffer));
                        }
                    } else if is_explain_frame(&buffer) {
                        if pending_explain_event.is_none() {
                            yield Ok(Bytes::from(buffer));
                        }
                    } else if is_turn_complete_frame(&buffer) {
                        if let Some(warning_event) = pending_warning_event.take() {
                            yield Ok(Bytes::from(render_sse_json(
                                serde_json::Value::Object(warning_event),
                            )));
                        }
                        if let Some(explain_event) = pending_explain_event.take() {
                            yield Ok(Bytes::from(render_sse_json(
                                serde_json::Value::Object(explain_event),
                            )));
                        }
                        let bridge_state = pending_bridge_state
                            .as_ref()
                            .expect("pending bridge state should exist");
                        yield Ok(Bytes::from(render_sse_json(
                            serde_json::Value::Object(
                                build_turn_complete_event_from_bridge_state(
                                    bridge_state,
                                    trusted_execution_state.as_ref(),
                                    pending_followup_user_message.as_deref(),
                                ),
                            ),
                        )));
                        received_turn_complete = true;
                        pending_bridge_state = None;
                        pending_followup_user_message = None;
                        pending_warning_event = None;
                        pending_explain_event = None;
                    } else {
                        yield Ok(Bytes::from(buffer));
                    }
                } else if let Some(text_delta_event) = build_text_delta_event_from_frame(&buffer) {
                    yield Ok(Bytes::from(render_sse_json(
                        serde_json::Value::Object(text_delta_event),
                    )));
                } else if let Some(reasoning_delta_event) =
                    build_reasoning_delta_event_from_frame(&buffer)
                {
                    yield Ok(Bytes::from(render_sse_json(
                        serde_json::Value::Object(reasoning_delta_event),
                    )));
                } else if let Some(usage_event) = build_usage_event_from_frame(&buffer) {
                    yield Ok(Bytes::from(render_sse_json(
                        serde_json::Value::Object(usage_event),
                    )));
                } else if let Some(tool_result_quality_event) =
                    build_tool_result_quality_event_from_frame(&buffer)
                {
                    yield Ok(Bytes::from(render_sse_json(
                        serde_json::Value::Object(tool_result_quality_event),
                    )));
                } else if let Some(cloud_loop_progress_event) =
                    build_cloud_loop_progress_event_from_frame(&buffer)
                {
                    yield Ok(Bytes::from(render_sse_json(
                        serde_json::Value::Object(cloud_loop_progress_event),
                    )));
                } else if let Some(cloud_tool_result_event) =
                    build_cloud_tool_result_event_from_frame(&buffer)
                {
                    yield Ok(Bytes::from(render_sse_json(
                        serde_json::Value::Object(cloud_tool_result_event),
                    )));
                } else if let Some(error_event) = build_error_event_from_frame(&buffer) {
                    yield Ok(Bytes::from(render_sse_json(
                        serde_json::Value::Object(error_event),
                    )));
                } else if let Some(tool_call_event) = build_tool_call_event_from_frame(&buffer) {
                    yield Ok(Bytes::from(render_sse_json(
                        serde_json::Value::Object(tool_call_event),
                    )));
                } else if let Some(tool_call_start_event) =
                    build_tool_call_start_event_from_frame(&buffer)
                {
                    yield Ok(Bytes::from(render_sse_json(
                        serde_json::Value::Object(tool_call_start_event),
                    )));
                } else if is_warning_frame(&buffer) || is_explain_frame(&buffer) {
                    yield Ok(Bytes::from(buffer));
                } else {
                    yield Ok(Bytes::from(buffer));
                }
            }
        }
        if let Some(bridge_state) = pending_bridge_state {
            if let Some(warning_event) = pending_warning_event.take() {
                yield Ok(Bytes::from(render_sse_json(serde_json::Value::Object(
                    warning_event,
                ))));
            }
            if let Some(explain_event) = pending_explain_event.take() {
                yield Ok(Bytes::from(render_sse_json(serde_json::Value::Object(
                    explain_event,
                ))));
            }
            yield Ok(Bytes::from(render_sse_json(serde_json::Value::Object(
                if force_max_rounds_completion {
                    build_max_rounds_turn_complete_event(
                        &bridge_state,
                        trusted_execution_state.as_ref(),
                        pending_followup_user_message.as_deref(),
                    )
                } else {
                    build_turn_complete_event_from_bridge_state(
                        &bridge_state,
                        trusted_execution_state.as_ref(),
                        pending_followup_user_message.as_deref(),
                    )
                },
            ))));
            received_turn_complete = true;
        }

        if !received_turn_complete {
            astra_core::agent_warn!("bridge", "SSE stream ended without turn_complete frame — possible interruption");
        }
    }
}

pub(crate) fn sse_stream_response(status: StatusCode, body: Body) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .header("x-accel-buffering", "no")
        .body(body)
        .unwrap()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    // ── Phase 6.6: Header filtering security tests ──

    #[test]
    fn allows_x_mo_headers() {
        assert!(is_allowed_bridge_header("x-mo-session-id"));
        assert!(is_allowed_bridge_header("x-mo-user-id"));
        assert!(is_allowed_bridge_header("x-mo-routing-meta-b64"));
    }

    #[test]
    fn allows_authorization() {
        assert!(is_allowed_bridge_header("authorization"));
    }

    #[test]
    fn blocks_dangerous_headers() {
        assert!(!is_allowed_bridge_header("cookie"));
        assert!(!is_allowed_bridge_header("set-cookie"));
        assert!(!is_allowed_bridge_header("host"));
        assert!(!is_allowed_bridge_header("x-forwarded-for"));
        assert!(!is_allowed_bridge_header("x-real-ip"));
        assert!(!is_allowed_bridge_header("origin"));
        assert!(!is_allowed_bridge_header("referer"));
    }

    #[test]
    fn blocks_content_type_override() {
        assert!(!is_allowed_bridge_header("content-type"));
    }

    #[test]
    fn blocks_prefix_spoof() {
        // "x-mobile" starts with "x-mo" but not "x-mo-"
        assert!(!is_allowed_bridge_header("x-mobile"));
        assert!(is_allowed_bridge_header("x-mo-"));
    }

    // ── BridgeHealthMetrics wiring test ──

    #[test]
    fn http_bridge_has_health_metrics() {
        use tokio::sync::Mutex;
        let cache = Arc::new(Mutex::new(SessionCache::default()));
        let bridge = HttpChatTurnBridge::new("http://localhost:9999".to_string(), cache);
        let snap = bridge.health_snapshot();
        assert_eq!(snap.total_requests, 0);
        assert_eq!(snap.total_failures, 0);
        assert_eq!(snap.failure_rate, 0.0);
    }

    #[test]
    fn synthesized_session_info_includes_trusted_run_id() {
        let event = synthesized_session_info_event("sess-1", Some("run-1"));
        assert_eq!(event["type"], "session_info");
        assert_eq!(event["session_id"], "sess-1");
        assert_eq!(event["run_id"], "run-1");
    }

    fn trusted_identity_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-mo-session-id", HeaderValue::from_static("sess-1"));
        headers.insert("x-mo-turn-chain-id", HeaderValue::from_static("run-1"));
        headers
    }

    async fn forward_with_noop_writers<B: ChatTurnBridge + ?Sized>(
        bridge: &B,
        headers: &HeaderMap,
    ) -> Result<Response, (StatusCode, String)> {
        bridge
            .forward(
                headers,
                Bytes::from_static(b"{}"),
                Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
                Arc::new(crate::turn::services::NoopTurnToolEventWriter),
                Arc::new(crate::turn::services::NoopTurnHookDbWriter),
                Arc::new(InMemoryTurnReflectionStateStore::default()),
                Arc::new(NoopTurnReflectionLessonWriter),
                Arc::new(NoopTurnObserverWorker),
                Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
                Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
                None,
            )
            .await
    }

    async fn response_text(response: Response) -> String {
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body should read");
        String::from_utf8(body.to_vec()).expect("utf8")
    }

    #[test]
    fn bridge_http_500_counts_as_failure_and_breaker_signal() {
        let (is_success, should_trip_breaker) =
            bridge_http_response_health(StatusCode::INTERNAL_SERVER_ERROR, true);
        assert!(!is_success);
        assert!(should_trip_breaker);
    }

    #[test]
    fn bridge_non_sse_200_counts_as_failure_and_breaker_signal() {
        let (is_success, should_trip_breaker) = bridge_http_response_health(StatusCode::OK, false);
        assert!(!is_success);
        assert!(should_trip_breaker);
    }

    #[tokio::test]
    async fn circuit_breaker_fast_reject_preserves_trusted_session_info() {
        use tokio::sync::Mutex;

        let cache = Arc::new(Mutex::new(SessionCache::default()));
        let bridge = HttpChatTurnBridge::new("http://localhost:9999".to_string(), cache);
        for _ in 0..5 {
            bridge.circuit_breaker.record_failure();
        }

        let response = forward_with_noop_writers(&bridge, &trusted_identity_headers())
            .await
            .expect("fast reject should normalize to SSE");
        let text = response_text(response).await;

        assert!(text.contains("\"type\":\"session_info\""));
        assert!(text.contains("\"session_id\":\"sess-1\""));
        assert!(text.contains("\"run_id\":\"run-1\""));
        assert!(text.contains("\"type\":\"error\""));
        assert!(text.contains("\"code\":\"UPSTREAM_ERROR\""));
    }

    #[tokio::test]
    async fn request_send_failure_preserves_trusted_session_info() {
        use tokio::sync::Mutex;

        let cache = Arc::new(Mutex::new(SessionCache::default()));
        let bridge = HttpChatTurnBridge::new("http://[::1".to_string(), cache);

        let response = forward_with_noop_writers(&bridge, &trusted_identity_headers())
            .await
            .expect("send failure should normalize to SSE");
        let text = response_text(response).await;

        assert!(text.contains("\"type\":\"session_info\""));
        assert!(text.contains("\"session_id\":\"sess-1\""));
        assert!(!text.contains("\"run_id\":"));
        assert!(text.contains("\"type\":\"error\""));
        assert!(text.contains("\"code\":\"UPSTREAM_ERROR\""));
    }

    #[tokio::test]
    async fn unavailable_bridge_preserves_trusted_session_info() {
        let response =
            forward_with_noop_writers(&UnavailableChatTurnBridge, &trusted_identity_headers())
                .await
                .expect("disabled bridge should normalize to SSE");
        let text = response_text(response).await;

        assert!(text.contains("\"type\":\"session_info\""));
        assert!(text.contains("\"session_id\":\"sess-1\""));
        assert!(!text.contains("\"run_id\":"));
        assert!(text.contains("\"type\":\"error\""));
        assert!(text.contains("chat turn bridge disabled"));
    }

    #[tokio::test]
    async fn non_sse_error_body_excerpt_is_capped() {
        use futures_util::stream;

        let chunk = "x".repeat((MAX_BRIDGE_ERROR_BODY_BYTES / 2) + 10);
        let text = read_bridge_error_body_excerpt(
            stream::iter(vec![
                Ok::<Bytes, std::io::Error>(Bytes::from(chunk.clone())),
                Ok::<Bytes, std::io::Error>(Bytes::from(chunk)),
            ]),
            MAX_BRIDGE_ERROR_BODY_BYTES,
        )
        .await;

        assert!(text.len() <= MAX_BRIDGE_ERROR_BODY_BYTES + "\n… [truncated]".len());
        assert!(text.ends_with("\n… [truncated]"));
    }

    #[tokio::test]
    async fn bridge_error_sse_response_wraps_plaintext_body() {
        let response = bridge_error_sse_response(
            StatusCode::BAD_GATEWAY,
            bridge_error_sse_message(StatusCode::BAD_GATEWAY, "upstream exploded"),
            None,
            None,
        );
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(text.contains("\"type\":\"error\""));
        assert!(text.contains("\"code\":\"UPSTREAM_ERROR\""));
        assert!(text.contains("upstream exploded"));
    }

    #[tokio::test]
    async fn bridge_response_stream_cancels_token_when_body_drops() {
        let token = Arc::new(CancellationToken::new());
        let body = Body::from_stream(BridgeResponseStream::new(
            futures_util::stream::pending::<Result<Bytes, std::io::Error>>(),
            Some(token.clone()),
        ));

        assert!(!token.is_cancelled());
        drop(body);
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn bridge_error_sse_response_prepends_trusted_session_info() {
        let response = bridge_error_sse_response(
            StatusCode::BAD_REQUEST,
            "bad request",
            Some("sess-1"),
            Some("run-1"),
        );
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        let session_info_pos = text
            .find("\"type\":\"session_info\"")
            .expect("session_info event");
        let error_pos = text.find("\"type\":\"error\"").expect("error event");
        assert!(session_info_pos < error_pos);
        assert!(text.contains("\"session_id\":\"sess-1\""));
        assert!(text.contains("\"run_id\":\"run-1\""));
    }

    #[tokio::test]
    async fn passthrough_bridge_keeps_upstream_session_info_when_untrusted() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let filtered = filter_bridge_state_events(
            stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
                "data: {\"type\":\"session_info\",\"session_id\":\"upstream-s1\"}\n\n",
            ))]),
            Arc::new(Mutex::new(SessionCache::default())),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(text.contains("\"type\":\"session_info\""));
        assert!(text.contains("\"session_id\":\"upstream-s1\""));
    }

    #[tokio::test]
    async fn passthrough_bridge_keeps_upstream_bridge_state_when_untrusted() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let filtered = filter_bridge_state_events(
            stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
                "data: {\"type\":\"bridge_state\",\"tool_sigs\":[],\"tail_update_args\":{\"full_text\":\"hello\"}}\n\n",
            ))]),
            Arc::new(Mutex::new(SessionCache::default())),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(text.contains("\"type\":\"bridge_state\""));
        assert!(text.contains("\"full_text\":\"hello\""));
    }

    #[tokio::test]
    async fn passthrough_bridge_keeps_buffered_bridge_state_when_untrusted() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let filtered = filter_bridge_state_events(
            stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
                "data: {\"type\":\"bridge_state\",\"tool_sigs\":[],\"tail_update_args\":{\"full_text\":\"hello\"}}",
            ))]),
            Arc::new(Mutex::new(SessionCache::default())),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(text.contains("\"type\":\"bridge_state\""));
        assert!(text.contains("\"full_text\":\"hello\""));
    }

    #[tokio::test]
    async fn bridge_stream_read_error_is_normalized_to_sse_error() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let reqwest_error = reqwest::Client::new()
            .get("http://[::1")
            .build()
            .expect_err("invalid url should build reqwest error");

        let filtered = filter_bridge_state_events(
            stream::iter(vec![Err::<Bytes, reqwest::Error>(reqwest_error)]),
            Arc::new(Mutex::new(SessionCache::default())),
            Some("sess-1".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should stay readable");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(text.contains("\"type\":\"session_info\""));
        assert!(text.contains("\"type\":\"error\""));
        assert!(text.contains("\"code\":\"UPSTREAM_ERROR\""));
        assert!(text.contains("Failed to read bridge response"));
    }

    #[tokio::test]
    async fn bridge_stream_read_error_flushes_pending_warning_before_error() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let reqwest_error = reqwest::Client::new()
            .get("http://[::1")
            .build()
            .expect_err("invalid url should build reqwest error");

        let filtered = filter_bridge_state_events(
            stream::iter(vec![
                Ok::<Bytes, reqwest::Error>(Bytes::from(
                    "data: {\"type\":\"bridge_state\",\"tool_sigs\":[],\"firewall_warning_claims_failed\":2}\n\n",
                )),
                Err::<Bytes, reqwest::Error>(reqwest_error),
            ]),
            Arc::new(Mutex::new(SessionCache::default())),
            Some("sess-1".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should stay readable");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        let warning_pos = text.find("\"type\":\"warning\"").expect("warning event");
        let error_pos = text.find("\"type\":\"error\"").expect("error event");
        let turn_complete_pos = text
            .find("\"type\":\"turn_complete\"")
            .expect("turn_complete event");
        assert!(warning_pos < error_pos);
        assert!(error_pos < turn_complete_pos);
        assert!(text.contains("\"code\":\"UPSTREAM_ERROR\""));
    }

    #[tokio::test]
    async fn buffered_final_session_info_is_suppressed_when_trusted() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let filtered = filter_bridge_state_events(
            stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
                "data: {\"type\":\"session_info\",\"session_id\":\"upstream-s1\"}",
            ))]),
            Arc::new(Mutex::new(SessionCache::default())),
            Some("trusted-s1".to_string()),
            None,
            None,
            Some("run-1".to_string()),
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert_eq!(text.matches("\"type\":\"session_info\"").count(), 1);
        assert!(text.contains("\"session_id\":\"trusted-s1\""));
        assert!(!text.contains("\"session_id\":\"upstream-s1\""));
        assert!(text.contains("\"run_id\":\"run-1\""));
    }

    #[tokio::test]
    async fn guard_error_emits_local_turn_complete_without_upstream_terminal() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let filtered = filter_bridge_state_events(
            stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
                "data: {\"type\":\"bridge_state\",\"tool_sigs\":[],\"tail_update_args\":{\"full_text\":\"hello hello hello hello hello hello hello hello\"}}\n\n",
            ))]),
            Arc::new(Mutex::new(SessionCache::default())),
            Some("sess-1".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(text.contains("\"type\":\"error\""));
        assert!(text.contains("\"code\":\"MODEL_DEGRADED\""));
        assert_eq!(text.matches("\"type\":\"turn_complete\"").count(), 1);
    }

    #[tokio::test]
    async fn buffered_guard_error_emits_local_turn_complete_without_upstream_terminal() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let filtered = filter_bridge_state_events(
            stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
                "data: {\"type\":\"bridge_state\",\"tool_sigs\":[],\"tail_update_args\":{\"full_text\":\"hello hello hello hello hello hello hello hello\"}}",
            ))]),
            Arc::new(Mutex::new(SessionCache::default())),
            Some("sess-1".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(text.contains("\"type\":\"error\""));
        assert!(text.contains("\"code\":\"MODEL_DEGRADED\""));
        assert_eq!(text.matches("\"type\":\"turn_complete\"").count(), 1);
    }

    #[tokio::test]
    async fn buffered_guard_error_suppresses_unterminated_upstream_turn_complete() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let filtered = filter_bridge_state_events(
            stream::iter(vec![
                Ok::<Bytes, reqwest::Error>(Bytes::from(
                    "data: {\"type\":\"bridge_state\",\"tool_sigs\":[],\"tail_update_args\":{\"full_text\":\"hello hello hello hello hello hello hello hello\"}}\n\n",
                )),
                Ok::<Bytes, reqwest::Error>(Bytes::from(
                    "data: {\"type\":\"turn_complete\",\"message\":\"upstream\"}",
                )),
            ]),
            Arc::new(Mutex::new(SessionCache::default())),
            Some("sess-1".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(text.contains("\"type\":\"error\""));
        assert_eq!(text.matches("\"type\":\"turn_complete\"").count(), 1);
        assert!(!text.contains("\"message\":\"upstream\""));
    }

    #[tokio::test]
    async fn guard_error_flushes_pending_warning_before_terminal_events() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let filtered = filter_bridge_state_events(
            stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
                "data: {\"type\":\"bridge_state\",\"tool_sigs\":[],\"firewall_warning_claims_failed\":2,\"tail_update_args\":{\"full_text\":\"hello hello hello hello hello hello hello hello\"}}\n\n",
            ))]),
            Arc::new(Mutex::new(SessionCache::default())),
            Some("sess-1".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        let warning_pos = text.find("\"type\":\"warning\"").expect("warning event");
        let error_pos = text.find("\"type\":\"error\"").expect("error event");
        let turn_complete_pos = text
            .find("\"type\":\"turn_complete\"")
            .expect("turn_complete event");
        assert!(warning_pos < error_pos);
        assert!(error_pos < turn_complete_pos);
    }

    #[tokio::test]
    async fn max_tool_rounds_flushes_pending_warning_before_forced_completion() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let mut frames = Vec::new();
        for idx in 0..=max_tool_rounds() {
            let warning = if idx == max_tool_rounds() {
                ",\"firewall_warning_claims_failed\":2"
            } else {
                ""
            };
            frames.push(Ok::<Bytes, reqwest::Error>(Bytes::from(format!(
                "data: {{\"type\":\"bridge_state\",\"tool_sigs\":[[\"run_build_test:{{}}\"]]{warning}}}\n\n"
            ))));
        }

        let filtered = filter_bridge_state_events(
            stream::iter(frames),
            Arc::new(Mutex::new(SessionCache::default())),
            Some("sess-1".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        let warning_pos = text.find("\"type\":\"warning\"").expect("warning event");
        let turn_complete_pos = text
            .find("\"type\":\"turn_complete\"")
            .expect("turn_complete event");
        assert!(warning_pos < turn_complete_pos);
        assert!(text.contains("\"max_rounds_exceeded\":true"));
    }

    #[tokio::test]
    async fn raw_explain_frame_is_preserved_without_bridge_state() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let filtered = filter_bridge_state_events(
            stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
                "data: {\"type\":\"explain\",\"total_ms\":12,\"tools_selected\":1,\"tools_available\":2}\n\n",
            ))]),
            Arc::new(Mutex::new(SessionCache::default())),
            Some("sess-1".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(text.contains("\"type\":\"explain\""));
        assert!(text.contains("\"total_ms\":12"));
    }

    #[tokio::test]
    async fn raw_warning_frame_is_preserved_without_bridge_state() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let filtered = filter_bridge_state_events(
            stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
                "data: {\"type\":\"warning\",\"message\":\"approaching limit\"}\n\n",
            ))]),
            Arc::new(Mutex::new(SessionCache::default())),
            Some("sess-1".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(text.contains("\"type\":\"warning\""));
        assert!(text.contains("approaching limit"));
    }

    #[tokio::test]
    async fn buffered_final_bridge_state_still_enforces_max_tool_rounds() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let mut frames = Vec::new();
        for _ in 0..max_tool_rounds() {
            frames.push(Ok::<Bytes, reqwest::Error>(Bytes::from(
                "data: {\"type\":\"bridge_state\",\"tool_sigs\":[[\"run_build_test:{}\"]]}\n\n",
            )));
        }
        frames.push(Ok::<Bytes, reqwest::Error>(Bytes::from(
            "data: {\"type\":\"bridge_state\",\"tool_sigs\":[[\"run_build_test:{}\"]]}",
        )));

        let filtered = filter_bridge_state_events(
            stream::iter(frames),
            Arc::new(Mutex::new(SessionCache::default())),
            Some("sess-1".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(text.contains("\"type\":\"turn_complete\""));
        assert!(text.contains("\"max_rounds_exceeded\":true"));
    }
}
