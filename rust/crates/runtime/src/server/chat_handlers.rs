use super::*;

pub(super) async fn chat_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let run = state
        .run_lifecycle_service
        .create_run(user.user_id, chat_request_into_data(request))
        .await?;
    Ok(Json(ChatResponse::from(run)))
}

pub(super) async fn chat_stream_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ChatRequest>,
) -> Response {
    let user = match state.auth_service.current_user(&headers).await {
        Ok(user) => user,
        Err((status, error)) => return sse_error_response(status, error.0.detail),
    };

    let chat_data = chat_request_into_data(request);
    match state
        .run_lifecycle_service
        .stream_chat(user.user_id.clone(), chat_data.clone())
        .await
    {
        Ok(stream) => {
            let mut events = vec![serde_json::json!({
                "type": "session_info",
                "session_id": stream.session_id,
                "run_id": stream.run_id,
            })];
            events.extend(
                stream
                    .events
                    .into_iter()
                    .map(transform_run_event_for_client),
            );
            sse_json_response(events)
        }
        Err((status, error))
            if status == StatusCode::NOT_IMPLEMENTED
                && error
                    .0
                    .detail
                    .contains("Run lifecycle service not configured") =>
        {
            // Fallback path: route /chat/stream through chat-turn bridge when lifecycle
            // service isn't wired yet. This preserves CLI usability during cutover.
            let payload = serde_json::json!({
                "session_id": chat_data.session_id,
                "agent_id": chat_data.agent_id,
                "model": chat_data.model,
                "context": chat_data.context,
                "messages": [
                    {
                        "role": "user",
                        "content": chat_data.message
                    }
                ]
            });
            let body = match serde_json::to_vec(&payload).map(Bytes::from) {
                Ok(body) => body,
                Err(e) => {
                    return sse_error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
                }
            };
            dispatch_chat_turn_bridge(&state, &user, &headers, body).await
        }
        Err((status, error)) => sse_error_response(status, error.0.detail),
    }
}

pub(super) async fn chat_turn_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match state.auth_service.current_user(&headers).await {
        Ok(user) => user,
        Err((status, error)) => return sse_error_response(status, error.0.detail),
    };
    dispatch_chat_turn_bridge(&state, &user, &headers, body).await
}

pub(super) async fn dispatch_chat_turn_bridge(
    state: &AppState,
    user: &AuthUserRecord,
    source_headers: &HeaderMap,
    body: Bytes,
) -> Response {
    let mut bridge_headers = HeaderMap::new();
    bridge_headers.insert(
        HeaderName::from_static("x-mo-bridge-secret"),
        HeaderValue::from_str(&state.chat_turn_bridge_secret).unwrap(),
    );
    bridge_headers.insert(
        HeaderName::from_static("x-mo-user-id"),
        HeaderValue::from_str(&user.user_id).unwrap(),
    );
    let username_b64 = URL_SAFE.encode(user.username.as_bytes());
    bridge_headers.insert(
        HeaderName::from_static("x-mo-username-b64"),
        HeaderValue::from_str(&username_b64).unwrap(),
    );
    bridge_headers.insert(
        HeaderName::from_static("x-mo-bridge-capabilities"),
        HeaderValue::from_static("state-sync-v1"),
    );
    if let Some(auth) = source_headers.get("authorization").cloned() {
        bridge_headers.insert(HeaderName::from_static("authorization"), auth);
    }

    let prepared = match prepare_chat_turn_bridge_body(state, user, body).await {
        Ok(result) => result,
        Err((status, error)) => return sse_error_response(status, error.0.detail),
    };
    if let Some(trusted_session_id) = prepared.trusted_session_id.as_deref() {
        bridge_headers.insert(
            HeaderName::from_static("x-mo-session-id"),
            HeaderValue::from_str(trusted_session_id).unwrap(),
        );
    }
    if let Some(turn_chain_id) = prepared.turn_chain_id.as_deref() {
        bridge_headers.insert(
            HeaderName::from_static("x-mo-turn-chain-id"),
            HeaderValue::from_str(turn_chain_id).unwrap(),
        );
    }
    if let Some(user_query_event_id) = prepared.user_query_event_id.as_deref() {
        bridge_headers.insert(
            HeaderName::from_static("x-mo-user-query-event-id"),
            HeaderValue::from_str(user_query_event_id).unwrap(),
        );
    }
    if let Some(tools_changed) = prepared.tools_changed {
        bridge_headers.insert(
            HeaderName::from_static("x-mo-tools-changed"),
            HeaderValue::from_static(if tools_changed { "1" } else { "0" }),
        );
    }
    if let Some(task_hint) = prepared.task_hint.as_deref() {
        bridge_headers.insert(
            HeaderName::from_static("x-mo-task-hint"),
            HeaderValue::from_str(task_hint).unwrap(),
        );
    }
    if let Some(user_query_b64) = prepared.user_query_b64.as_deref() {
        bridge_headers.insert(
            HeaderName::from_static("x-mo-user-query-b64"),
            HeaderValue::from_str(user_query_b64).unwrap(),
        );
    }
    if let Some(routing_meta_b64) = prepared.routing_meta_b64.as_deref() {
        bridge_headers.insert(
            HeaderName::from_static("x-mo-routing-meta-b64"),
            HeaderValue::from_str(routing_meta_b64).unwrap(),
        );
    }
    if let Some(force_intent) = prepared.force_intent.as_deref() {
        bridge_headers.insert(
            HeaderName::from_static("x-mo-force-intent"),
            HeaderValue::from_str(force_intent).unwrap(),
        );
    }
    if let Some(execution_state_b64) = prepared.execution_state_b64.as_deref() {
        bridge_headers.insert(
            HeaderName::from_static("x-mo-execution-state-b64"),
            HeaderValue::from_str(execution_state_b64).unwrap(),
        );
    }

    match state
        .chat_turn_bridge
        .forward(
            &bridge_headers,
            prepared.body,
            state.turn_core_event_writer.clone(),
            state.turn_tool_event_writer.clone(),
            state.turn_hook_db_writer.clone(),
            state.turn_reflection_state_store.clone(),
            state.turn_reflection_lesson_writer.clone(),
            state.turn_observer_worker.clone(),
            state.turn_auxiliary_event_writer.clone(),
            state.turn_session_activity_writer.clone(),
        )
        .await
    {
        Ok(response) => response,
        Err((status, error)) => {
            sse_error_response(status, format!("Chat turn bridge unavailable: {error}"))
        }
    }
}

pub(super) async fn chat_route_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ChatRouteRequest>,
) -> Result<Json<ChatRouteResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _ = state.auth_service.current_user(&headers).await?;
    Ok(Json(classify_chat_route(request.query)))
}
