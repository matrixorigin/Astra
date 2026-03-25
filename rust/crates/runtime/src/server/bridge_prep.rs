use super::*;

pub(super) struct PreparedChatTurnBridgeRequest {
    pub(super) body: Bytes,
    pub(super) trusted_session_id: Option<String>,
    pub(super) turn_chain_id: Option<String>,
    pub(super) user_query_event_id: Option<String>,
    pub(super) tools_changed: Option<bool>,
    pub(super) task_hint: Option<String>,
    pub(super) user_query_b64: Option<String>,
    pub(super) routing_meta_b64: Option<String>,
    pub(super) force_intent: Option<String>,
    pub(super) execution_state_b64: Option<String>,
}

pub(super) async fn prepare_chat_turn_bridge_body(
    state: &AppState,
    user: &AuthUserRecord,
    body: Bytes,
) -> Result<PreparedChatTurnBridgeRequest, (StatusCode, Json<ErrorResponse>)> {
    let Ok(mut payload) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return Ok(PreparedChatTurnBridgeRequest {
            body,
            trusted_session_id: None,
            turn_chain_id: None,
            user_query_event_id: None,
            tools_changed: None,
            task_hint: None,
            user_query_b64: None,
            routing_meta_b64: None,
            force_intent: None,
            execution_state_b64: None,
        });
    };
    let Some(object) = payload.as_object_mut() else {
        return Ok(PreparedChatTurnBridgeRequest {
            body,
            trusted_session_id: None,
            turn_chain_id: None,
            user_query_event_id: None,
            tools_changed: None,
            task_hint: None,
            user_query_b64: None,
            routing_meta_b64: None,
            force_intent: None,
            execution_state_b64: None,
        });
    };

    let (trusted_session_id, trusted_session_created_at) = if let Some(session_id) = object
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string)
    {
        let session = state
            .session_service
            .get_session(session_id.clone(), user.user_id.clone())
            .await
            .map_err(normalize_chat_turn_session_error)?;
        (
            Some(session_id),
            normalize_session_created_at_for_bridge(&session.created_at),
        )
    } else if object
        .get("session_id")
        .is_some_and(|value| !value.is_null())
    {
        (None, None)
    } else {
        let agent_id = object
            .get("agent_id")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string);
        let metadata = agent_id.as_ref().map(|agent_id| {
            serde_json::Map::from_iter([(
                "agent_id".to_string(),
                serde_json::Value::String(agent_id.clone()),
            )])
        });
        let session = state
            .session_service
            .create_session(
                user.user_id.clone(),
                SessionCreateRequestData {
                    agent_id,
                    title: None,
                    metadata,
                },
            )
            .await?;

        let created_session_id = session.session_id;
        let created_session_created_at =
            normalize_session_created_at_for_bridge(&session.created_at);
        object.insert(
            "session_id".to_string(),
            serde_json::Value::String(created_session_id.clone()),
        );
        (Some(created_session_id), created_session_created_at)
    };
    if let (Some(session_id), Some(created_at)) = (
        trusted_session_id.as_deref(),
        trusted_session_created_at.as_deref(),
    ) {
        seed_bridge_session_created_at(state, session_id, created_at).await;
    }

    let (turn_chain_id, user_query_event_id) = if let (Some(session_id), Some(messages)) = (
        trusted_session_id.as_deref(),
        object.get("messages").and_then(serde_json::Value::as_array),
    ) {
        let has_tool_results = object
            .get("tool_results")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|results| !results.is_empty());
        let (turn_chain_id, user_query_event_id) =
            prepare_chat_turn_bridge_identifiers(state, session_id, messages, has_tool_results)
                .await;
        (Some(turn_chain_id), Some(user_query_event_id))
    } else {
        (None, None)
    };

    let tools_changed = if let Some(session_id) = trusted_session_id.as_deref() {
        Some(prepare_chat_turn_bridge_cached_inputs(state, session_id, object).await)
    } else {
        None
    };
    let user_query = object
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .map(|messages| extract_latest_user_query(messages))
        .unwrap_or_default();
    // Tool selection is now handled client-side by ToolRegistry.
    // The server trusts the client's pre-selected tool set.
    trim_edge_tools_for_result_turn(object, &user_query);
    let task_hint = object
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .map(|messages| {
            let normalized = messages
                .iter()
                .filter_map(serde_json::Value::as_object)
                .cloned()
                .collect::<Vec<_>>();
            classify_task(&normalized)
        })
        .unwrap_or(None);
    let user_query_b64 = Some(URL_SAFE.encode(user_query.as_bytes()));
    let routing_meta_b64 = object
        .get("model")
        .and_then(serde_json::Value::as_str)
        .map(|_| {
            let meta = serde_json::Value::Object(build_skipped_routing_metadata("model_override"));
            URL_SAFE.encode(serde_json::to_string(&meta).unwrap().as_bytes())
        });
    let force_intent = detect_correction(&user_query).then_some("question".to_string());
    let request_execution_state = object
        .get("execution_state")
        .and_then(serde_json::Value::as_object);
    // Tool selection is client-side; server passes through.
    // Execution state only needed if client sent one.
    let execution_state_b64 = request_execution_state
        .is_some()
        .then(|| {
            request_execution_state
                .map(normalize_execution_state)
                .unwrap_or_else(|| normalize_execution_state(&serde_json::Map::new()))
        })
        .filter(|execution_state| !execution_state.is_empty())
        .map(|execution_state| URL_SAFE.encode(serde_json::to_string(&execution_state).unwrap()));

    serde_json::to_vec(&payload)
        .map(Bytes::from)
        .map(|body| PreparedChatTurnBridgeRequest {
            body,
            trusted_session_id,
            turn_chain_id,
            user_query_event_id,
            tools_changed,
            task_hint,
            user_query_b64,
            routing_meta_b64,
            force_intent,
            execution_state_b64,
        })
        .map_err(internal_error)
}

async fn seed_bridge_session_created_at(state: &AppState, session_id: &str, created_at: &str) {
    if created_at.is_empty() {
        return;
    }
    let now = current_unix_seconds();
    let mut cache = state.chat_turn_bridge_cache.lock().await;
    let mut entry = cache.get(session_id, now).unwrap_or_default();
    entry
        .entry("created_at".to_string())
        .or_insert_with(|| serde_json::Value::String(created_at.to_string()));
    cache.insert(session_id.to_string(), entry, now);
}

fn normalize_session_created_at_for_bridge(created_at: &str) -> Option<String> {
    let trimmed = created_at.trim();
    if trimmed.is_empty() {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(trimmed)
        .map(|dt| dt.with_timezone(&Utc).to_rfc3339().replace("+00:00", "Z"))
        .ok()
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S")
                .ok()
                .map(|naive| {
                    chrono::DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc)
                        .to_rfc3339()
                        .replace("+00:00", "Z")
                })
        })
        .or_else(|| Some(trimmed.to_string()))
}

fn normalize_chat_turn_session_error(
    error: (StatusCode, Json<ErrorResponse>),
) -> (StatusCode, Json<ErrorResponse>) {
    let (status, detail) = error;
    if status == StatusCode::NOT_FOUND {
        error_response(StatusCode::NOT_FOUND, "Session not found")
    } else {
        (status, detail)
    }
}

async fn prepare_chat_turn_bridge_identifiers(
    state: &AppState,
    session_id: &str,
    messages: &[serde_json::Value],
    has_tool_results: bool,
) -> (String, String) {
    let now = current_unix_seconds();
    let mut cache = state.chat_turn_bridge_cache.lock().await;
    let mut prev_entry = cache.get(session_id, now);
    let new_turn_chain_id = Uuid::now_v7().to_string();
    let new_user_query_event_id = Uuid::now_v7().to_string();
    let (turn_chain_id, user_query_event_id) = resolve_turn_identifiers(
        messages,
        has_tool_results,
        prev_entry.as_mut(),
        &new_turn_chain_id,
        &new_user_query_event_id,
    );
    let mut updated_entry = prev_entry.unwrap_or_default();
    updated_entry.insert(
        "turn_chain_id".to_string(),
        serde_json::Value::String(turn_chain_id.clone()),
    );
    updated_entry.insert(
        "user_query_event_id".to_string(),
        serde_json::Value::String(user_query_event_id.clone()),
    );
    cache.insert(session_id.to_string(), updated_entry, now);
    (turn_chain_id, user_query_event_id)
}

async fn prepare_chat_turn_bridge_cached_inputs(
    state: &AppState,
    session_id: &str,
    object: &mut serde_json::Map<String, serde_json::Value>,
) -> bool {
    let now = current_unix_seconds();
    let mut cache = state.chat_turn_bridge_cache.lock().await;
    let mut entry = cache.get(session_id, now).unwrap_or_default();
    let cached_tools = entry
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .cloned();

    let tools_changed = if let Some(edge_tools) = object
        .get("edge_tools")
        .and_then(serde_json::Value::as_array)
        .cloned()
    {
        let changed = cached_tools
            .as_ref()
            .is_some_and(|cached| !same_tool_names(cached, &edge_tools));
        entry.insert("tools".to_string(), serde_json::Value::Array(edge_tools));
        changed
    } else if let Some(cached_tools) = cached_tools {
        object.insert(
            "edge_tools".to_string(),
            serde_json::Value::Array(cached_tools.clone()),
        );
        false
    } else {
        false
    };

    sync_cached_bridge_field(&mut entry, object, "project_rules");
    sync_cached_bridge_field(&mut entry, object, "edge_profile");
    inject_bridge_cache_state(&entry, object);

    cache.insert(session_id.to_string(), entry, now);
    tools_changed
}

fn trim_edge_tools_for_result_turn(
    object: &mut serde_json::Map<String, serde_json::Value>,
    user_query: &str,
) {
    let tool_result_names = object
        .get("tool_results")
        .and_then(serde_json::Value::as_array)
        .map(|tool_results| {
            tool_results
                .iter()
                .map(|tool_result| tool_result.get("name").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let Some(edge_tools) = object
        .get("edge_tools")
        .and_then(serde_json::Value::as_array)
        .cloned()
    else {
        return;
    };
    let available_tool_names = edge_tools
        .iter()
        .filter_map(|tool| {
            tool.get("function")
                .and_then(|function| function.get("name"))
                .and_then(serde_json::Value::as_str)
        })
        .collect::<Vec<_>>();
    let Some(subset_names) =
        plan_tool_subset_for_result_turn(&tool_result_names, user_query, &available_tool_names)
    else {
        return;
    };
    let filtered_edge_tools = edge_tools
        .into_iter()
        .filter(|tool| {
            tool.get("function")
                .and_then(|function| function.get("name"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|name| subset_names.iter().any(|candidate| candidate == name))
        })
        .collect::<Vec<_>>();
    object.insert(
        "edge_tools".to_string(),
        serde_json::Value::Array(filtered_edge_tools),
    );
}

fn sync_cached_bridge_field(
    entry: &mut serde_json::Map<String, serde_json::Value>,
    object: &mut serde_json::Map<String, serde_json::Value>,
    field: &str,
) {
    if let Some(value) = object.get(field).filter(|value| !value.is_null()).cloned() {
        entry.insert(field.to_string(), value);
    } else if let Some(cached) = entry.get(field).cloned() {
        object.insert(field.to_string(), cached);
    }
}

fn inject_bridge_cache_state(
    entry: &serde_json::Map<String, serde_json::Value>,
    object: &mut serde_json::Map<String, serde_json::Value>,
) {
    let Some(bridge_cache_state) = normalize_bridge_cache_entry(entry) else {
        return;
    };
    object.insert(
        "bridge_cache_state".to_string(),
        serde_json::Value::Object(bridge_cache_state),
    );
}

fn same_tool_names(left: &[serde_json::Value], right: &[serde_json::Value]) -> bool {
    fn names(tools: &[serde_json::Value]) -> Vec<String> {
        let mut names = tools
            .iter()
            .filter_map(|tool| {
                tool.get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string)
            })
            .collect::<Vec<_>>();
        names.sort();
        names.dedup();
        names
    }

    names(left) == names(right)
}
