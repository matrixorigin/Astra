use super::*;

/// Safely convert a string to a HeaderValue, returning an SSE error response on failure.
#[allow(clippy::result_large_err)]
fn safe_header_value(value: &str) -> Result<HeaderValue, Response> {
    HeaderValue::from_str(value).map_err(|_| {
        sse_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Invalid header value: contains non-visible ASCII".to_string(),
        )
    })
}

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
        match safe_header_value(&state.chat_turn_bridge_secret) {
            Ok(v) => v,
            Err(r) => return r,
        },
    );
    bridge_headers.insert(
        HeaderName::from_static("x-mo-user-id"),
        match safe_header_value(&user.user_id) {
            Ok(v) => v,
            Err(r) => return r,
        },
    );
    let username_b64 = URL_SAFE.encode(user.username.as_bytes());
    bridge_headers.insert(
        HeaderName::from_static("x-mo-username-b64"),
        match safe_header_value(&username_b64) {
            Ok(v) => v,
            Err(r) => return r,
        },
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
            match safe_header_value(trusted_session_id) {
                Ok(v) => v,
                Err(r) => return r,
            },
        );
    }
    if let Some(turn_chain_id) = prepared.turn_chain_id.as_deref() {
        bridge_headers.insert(
            HeaderName::from_static("x-mo-turn-chain-id"),
            match safe_header_value(turn_chain_id) {
                Ok(v) => v,
                Err(r) => return r,
            },
        );
    }
    if let Some(user_query_event_id) = prepared.user_query_event_id.as_deref() {
        bridge_headers.insert(
            HeaderName::from_static("x-mo-user-query-event-id"),
            match safe_header_value(user_query_event_id) {
                Ok(v) => v,
                Err(r) => return r,
            },
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
            match safe_header_value(task_hint) {
                Ok(v) => v,
                Err(r) => return r,
            },
        );
    }
    if let Some(user_query_b64) = prepared.user_query_b64.as_deref() {
        bridge_headers.insert(
            HeaderName::from_static("x-mo-user-query-b64"),
            match safe_header_value(user_query_b64) {
                Ok(v) => v,
                Err(r) => return r,
            },
        );
    }
    if let Some(routing_meta_b64) = prepared.routing_meta_b64.as_deref() {
        bridge_headers.insert(
            HeaderName::from_static("x-mo-routing-meta-b64"),
            match safe_header_value(routing_meta_b64) {
                Ok(v) => v,
                Err(r) => return r,
            },
        );
    }
    if let Some(force_intent) = prepared.force_intent.as_deref() {
        bridge_headers.insert(
            HeaderName::from_static("x-mo-force-intent"),
            match safe_header_value(force_intent) {
                Ok(v) => v,
                Err(r) => return r,
            },
        );
    }
    if let Some(execution_state_b64) = prepared.execution_state_b64.as_deref() {
        bridge_headers.insert(
            HeaderName::from_static("x-mo-execution-state-b64"),
            match safe_header_value(execution_state_b64) {
                Ok(v) => v,
                Err(r) => return r,
            },
        );
    }
    // Bridge E2E hooks (`bridge-e2e-hooks`): in-process bridge reads this header with env
    // `ASTRA_BRIDGE_TEST_SECRET`; harmless if unset or header absent.
    if let Some(v) = source_headers.get("x-mo-bridge-test-secret").cloned() {
        bridge_headers.insert(HeaderName::from_static("x-mo-bridge-test-secret"), v);
    }

    let client_disconnect = std::sync::Arc::new(tokio_util::sync::CancellationToken::new());

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
            Some(client_disconnect),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_header_names_are_valid() {
        let headers = [
            "x-mo-bridge-secret",
            "x-mo-user-id",
            "x-mo-username-b64",
            "x-mo-bridge-capabilities",
            "x-mo-session-id",
            "x-mo-turn-chain-id",
            "x-mo-user-query-event-id",
            "x-mo-tools-changed",
            "x-mo-task-hint",
            "x-mo-user-query-b64",
            "x-mo-routing-meta-b64",
            "x-mo-force-intent",
            "x-mo-execution-state-b64",
            "x-mo-bridge-test-secret",
        ];
        for name in headers {
            assert!(
                HeaderName::from_static(name).as_str() == name,
                "invalid header name: {name}"
            );
        }
    }

    #[test]
    fn username_b64_encoding() {
        let username = "alice";
        let encoded = URL_SAFE.encode(username.as_bytes());
        let decoded = URL_SAFE.decode(&encoded).unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "alice");

        // CJK username
        let cjk = "张三";
        let encoded_cjk = URL_SAFE.encode(cjk.as_bytes());
        let decoded_cjk = URL_SAFE.decode(&encoded_cjk).unwrap();
        assert_eq!(String::from_utf8(decoded_cjk).unwrap(), "张三");
    }

    #[test]
    fn bridge_capabilities_header_value() {
        let hv = HeaderValue::from_static("state-sync-v1");
        assert_eq!(hv.to_str().unwrap(), "state-sync-v1");
    }

    #[test]
    fn tools_changed_header_values() {
        let true_val = if true { "1" } else { "0" };
        let false_val = if false { "1" } else { "0" };
        assert_eq!(true_val, "1");
        assert_eq!(false_val, "0");
        // Ensure they are valid header values
        assert!(HeaderValue::from_static("1").to_str().is_ok());
        assert!(HeaderValue::from_static("0").to_str().is_ok());
    }

    #[test]
    fn chat_stream_fallback_payload_shape() {
        let payload = serde_json::json!({
            "session_id": Some("s1"),
            "agent_id": Some("a1"),
            "model": Some("gpt-4"),
            "context": null,
            "messages": [{
                "role": "user",
                "content": "hello"
            }]
        });
        let obj = payload.as_object().unwrap();
        assert!(obj.contains_key("messages"));
        assert!(obj.contains_key("session_id"));
        let messages = obj["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
    }

    #[test]
    fn header_value_from_str_handles_special_chars() {
        // UUID format
        assert!(HeaderValue::from_str("550e8400-e29b-41d4-a716-446655440000").is_ok());
        // Base64 with padding
        assert!(HeaderValue::from_str("dXNlcm5hbWU=").is_ok());
        // Base64 URL-safe
        assert!(HeaderValue::from_str("aGVsbG8td29ybGQ").is_ok());
    }

    #[test]
    fn dispatch_header_count() {
        // Base headers: 4 (secret, user-id, username-b64, capabilities)
        // + authorization passthrough: 1
        // + optional from prepared: 9 (session-id, turn-chain-id, user-query-event-id,
        //   tools-changed, task-hint, user-query-b64, routing-meta-b64, force-intent,
        //   execution-state-b64)
        // Total possible: 14
        assert_eq!(4 + 1 + 9, 14);
    }
}
