use super::*;

pub mod side_effects;

use self::side_effects::{
    build_bridge_response_guard_error_event, build_bridge_side_effect_payloads,
    dispatch_bridge_side_effect_request, run_bridge_hook_side_effects, sync_bridge_state_event,
    take_bridge_explain_event, take_bridge_prompt_fingerprints, take_bridge_side_effect_inputs,
    take_bridge_tail_update_args, take_bridge_warning_event,
};

mod sse_events;

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

const TOOL_RESULT_AUDIT_CHARS: usize = 4000;

#[async_trait]
pub trait ChatTurnBridge: Send + Sync {
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
    ) -> Result<Response, (StatusCode, String)>;
}

#[derive(Clone, Debug)]
pub(crate) struct HttpChatTurnBridge {
    url: String,
    client: reqwest::Client,
    cache: Arc<tokio::sync::Mutex<SessionCache>>,
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
                .build()
                .expect("chat turn bridge client should build"),
            cache,
        }
    }
}

#[async_trait]
impl ChatTurnBridge for UnavailableChatTurnBridge {
    async fn forward(
        &self,
        _headers: &HeaderMap,
        _body: Bytes,
        _turn_core_event_writer: Arc<dyn TurnCoreEventWriter>,
        _turn_tool_event_writer: Arc<dyn TurnToolEventWriter>,
        _turn_hook_db_writer: Arc<dyn TurnHookDbWriter>,
        _turn_reflection_state_store: Arc<dyn TurnReflectionStateStore>,
        _turn_reflection_lesson_writer: Arc<dyn TurnReflectionLessonWriter>,
        _turn_observer_worker: Arc<dyn TurnObserverWorker>,
        _turn_auxiliary_event_writer: Arc<dyn TurnAuxiliaryEventWriter>,
        _turn_session_activity_writer: Arc<dyn TurnSessionActivityWriter>,
    ) -> Result<Response, (StatusCode, String)> {
        Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "chat turn bridge disabled. Configure CHAT_TURN_BRIDGE_URL to a reachable /internal/chat/turn endpoint (example: compatible chat-turn bridge service), then restart API."
                .to_string(),
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
    ) -> Result<Response, (StatusCode, String)> {
        let mut bridge_headers = HeaderMap::new();
        let side_effect_request_context = parse_bridge_side_effect_request_context(&body);
        let mut request = self
            .client
            .post(&self.url)
            .header("content-type", "application/json");
        for (header_name, value) in headers.iter() {
            let allowed_header = header_name.as_str().starts_with("x-mo-")
                || header_name.as_str() == "authorization";
            if allowed_header && let Ok(value_str) = value.to_str() {
                bridge_headers.insert(header_name.clone(), value.clone());
                request = request.header(header_name.as_str(), value_str);
            }
        }
        request = request.body(body.to_vec());

        let response = request
            .send()
            .await
            .map_err(|error| (StatusCode::BAD_GATEWAY, error.to_string()))?;
        let status = StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::OK);
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string);
        let trusted_session_id = headers
            .get("x-mo-session-id")
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string)
            .filter(|value| !value.is_empty());
        let filtered_stream = filter_bridge_state_events(
            response.bytes_stream(),
            self.cache.clone(),
            if status == StatusCode::OK
                && content_type
                    .as_deref()
                    .is_some_and(|value| value.starts_with("text/event-stream"))
            {
                trusted_session_id
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
                bridge_headers
                    .get("x-mo-turn-chain-id")
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
        );
        Ok(sse_stream_response(
            status,
            Body::from_stream(filtered_stream),
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
) -> impl futures_util::stream::Stream<Item = Result<Bytes, std::io::Error>>
where
    S: futures_util::stream::Stream<Item = Result<Bytes, reqwest::Error>> + Unpin + Send + 'static,
{
    stream! {
        if let Some(session_id) = trusted_session_id.as_deref() {
            yield Ok(Bytes::from(render_sse_json(serde_json::json!({
                "type": "session_info",
                "session_id": session_id,
            }))));
        }
        let mut buffer = Vec::new();
        let mut pending_bridge_state: Option<serde_json::Map<String, serde_json::Value>> = None;
        let mut pending_warning_event: Option<serde_json::Map<String, serde_json::Value>> = None;
        let mut pending_explain_event: Option<serde_json::Map<String, serde_json::Value>> = None;
        let mut latest_token_usage: Option<serde_json::Value> = None;
        let mut suppress_next_turn_complete = false;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(chunk) => {
                    buffer.extend_from_slice(&chunk);
                    while let Some(end) = find_sse_frame_end(&buffer) {
                        let frame = buffer.drain(..end + 2).collect::<Vec<_>>();
                        if is_session_info_frame(&frame) {
                            continue;
                        } else if suppress_next_turn_complete && is_turn_complete_frame(&frame) {
                            suppress_next_turn_complete = false;
                            continue;
                        } else if let Some(mut bridge_state) = parse_bridge_state_frame(&frame) {
                            let Some(trusted_session_id) = trusted_session_id.as_deref() else {
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
                                pending_warning_event = None;
                                pending_explain_event = None;
                                suppress_next_turn_complete = true;
                                yield Ok(Bytes::from(render_sse_json(
                                    serde_json::Value::Object(response_guard_error),
                                )));
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
                                    );
                                }
                            pending_bridge_state = Some(synced_bridge_state);
                            pending_warning_event = warning_event;
                            pending_explain_event = explain_event;
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
                            } else if is_warning_frame(&frame) || is_explain_frame(&frame) {
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
                                        ),
                                    ),
                                )));
                                pending_bridge_state = None;
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
                            continue;
                        } else {
                            yield Ok(Bytes::from(frame));
                        }
                    }
                }
                Err(error) => {
                    yield Err(std::io::Error::other(error));
                    return;
                }
            }
        }

        if !buffer.is_empty() {
            if let Some(mut bridge_state) = parse_bridge_state_frame(&buffer) {
                let Some(trusted_session_id) = trusted_session_id.as_deref() else {
                    return;
                };
                let prompt_fingerprints = take_bridge_prompt_fingerprints(&mut bridge_state);
                let tail_update_args = take_bridge_tail_update_args(&mut bridge_state);
                let warning_event = take_bridge_warning_event(&mut bridge_state);
                let explain_event = take_bridge_explain_event(&mut bridge_state);
                let side_effect_inputs = take_bridge_side_effect_inputs(&mut bridge_state);
                if let Some(response_guard_error) = tail_update_args
                    .as_ref()
                    .and_then(|tail_update_args| {
                        build_bridge_response_guard_error_event(
                            tail_update_args,
                            &prompt_fingerprints,
                        )
                    })
                {
                    yield Ok(Bytes::from(render_sse_json(
                        serde_json::Value::Object(response_guard_error),
                    )));
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
                            );
                        }
                    pending_bridge_state = Some(synced_bridge_state);
                    pending_warning_event = warning_event;
                    pending_explain_event = explain_event;
                }
            } else {
                yield Ok(Bytes::from(buffer));
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
                build_turn_complete_event_from_bridge_state(
                    &bridge_state,
                    trusted_execution_state.as_ref(),
                ),
            ))));
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
