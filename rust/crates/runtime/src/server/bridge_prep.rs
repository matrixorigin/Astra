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
            URL_SAFE.encode(serde_json::to_string(&meta).unwrap_or_default().as_bytes())
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
        .map(|execution_state| {
            URL_SAFE.encode(serde_json::to_string(&execution_state).unwrap_or_default())
        });

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_value(name: &str) -> serde_json::Value {
        json!({ "function": { "name": name } })
    }

    // ── normalize_session_created_at_for_bridge ──────────────────────

    #[test]
    fn normalize_created_at_rfc3339_with_positive_offset() {
        let result = normalize_session_created_at_for_bridge("2024-01-15T10:30:00+08:00");
        assert_eq!(result, Some("2024-01-15T02:30:00Z".to_string()));
    }

    #[test]
    fn normalize_created_at_rfc3339_with_z() {
        let result = normalize_session_created_at_for_bridge("2024-01-15T10:30:00Z");
        assert_eq!(result, Some("2024-01-15T10:30:00Z".to_string()));
    }

    #[test]
    fn normalize_created_at_rfc3339_with_plus_zero_offset() {
        let result = normalize_session_created_at_for_bridge("2024-01-15T10:30:00+00:00");
        assert_eq!(result, Some("2024-01-15T10:30:00Z".to_string()));
    }

    #[test]
    fn normalize_created_at_rfc3339_with_negative_offset() {
        let result = normalize_session_created_at_for_bridge("2024-01-15T10:30:00-05:00");
        assert_eq!(result, Some("2024-01-15T15:30:00Z".to_string()));
    }

    #[test]
    fn normalize_created_at_naive_datetime() {
        let result = normalize_session_created_at_for_bridge("2024-01-15T10:30:00");
        assert_eq!(result, Some("2024-01-15T10:30:00Z".to_string()));
    }

    #[test]
    fn normalize_created_at_empty_string() {
        assert_eq!(normalize_session_created_at_for_bridge(""), None);
    }

    #[test]
    fn normalize_created_at_whitespace_only() {
        assert_eq!(normalize_session_created_at_for_bridge("  "), None);
    }

    #[test]
    fn normalize_created_at_garbage_falls_back_to_raw() {
        let result = normalize_session_created_at_for_bridge("not-a-date");
        assert_eq!(result, Some("not-a-date".to_string()));
    }

    #[test]
    fn normalize_created_at_leading_trailing_whitespace_trimmed() {
        let result = normalize_session_created_at_for_bridge("  2024-01-15T10:30:00Z  ");
        assert_eq!(result, Some("2024-01-15T10:30:00Z".to_string()));
    }

    // ── normalize_chat_turn_session_error ────────────────────────────

    #[test]
    fn session_error_not_found_is_normalized() {
        let input = error_response(StatusCode::NOT_FOUND, "some db error details");
        let (status, body) = normalize_chat_turn_session_error(input);
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body.0.detail, "Session not found");
    }

    #[test]
    fn session_error_internal_passes_through() {
        let msg = "unexpected failure";
        let input = error_response(StatusCode::INTERNAL_SERVER_ERROR, msg);
        let (status, body) = normalize_chat_turn_session_error(input);
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body.0.detail, msg);
    }

    #[test]
    fn session_error_bad_request_passes_through() {
        let msg = "invalid request body";
        let input = error_response(StatusCode::BAD_REQUEST, msg);
        let (status, body) = normalize_chat_turn_session_error(input);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body.0.detail, msg);
    }

    // ── same_tool_names ─────────────────────────────────────────────

    #[test]
    fn same_tools_same_order() {
        let left = vec![tool_value("bash")];
        let right = vec![tool_value("bash")];
        assert!(same_tool_names(&left, &right));
    }

    #[test]
    fn same_tools_different_order() {
        let left = vec![tool_value("bash"), tool_value("grep")];
        let right = vec![tool_value("grep"), tool_value("bash")];
        assert!(same_tool_names(&left, &right));
    }

    #[test]
    fn different_tools() {
        let left = vec![tool_value("bash")];
        let right = vec![tool_value("grep")];
        assert!(!same_tool_names(&left, &right));
    }

    #[test]
    fn extra_tool_means_not_same() {
        let left = vec![tool_value("bash"), tool_value("grep")];
        let right = vec![tool_value("bash")];
        assert!(!same_tool_names(&left, &right));
    }

    #[test]
    fn both_empty_arrays() {
        assert!(same_tool_names(&[], &[]));
    }

    #[test]
    fn duplicates_are_deduped() {
        let left = vec![tool_value("bash"), tool_value("bash")];
        let right = vec![tool_value("bash")];
        assert!(same_tool_names(&left, &right));
    }

    #[test]
    fn missing_function_field_both_sides() {
        let left = vec![json!({})];
        let right = vec![json!({})];
        assert!(same_tool_names(&left, &right));
    }

    #[test]
    fn only_named_tools_count() {
        let left = vec![json!({}), tool_value("bash")];
        let right = vec![tool_value("bash")];
        assert!(same_tool_names(&left, &right));
    }

    #[test]
    fn missing_name_in_function() {
        let left = vec![json!({ "function": {} })];
        let right = vec![json!({ "function": {} })];
        assert!(same_tool_names(&left, &right));
    }

    // ── sync_cached_bridge_field ────────────────────────────────────

    #[test]
    fn sync_object_has_field_entry_does_not() {
        let mut entry = serde_json::Map::new();
        let mut object = serde_json::Map::new();
        object.insert("project_rules".to_string(), json!("rule-value"));

        sync_cached_bridge_field(&mut entry, &mut object, "project_rules");

        assert_eq!(entry.get("project_rules"), Some(&json!("rule-value")));
        assert_eq!(object.get("project_rules"), Some(&json!("rule-value")));
    }

    #[test]
    fn sync_entry_has_field_object_does_not() {
        let mut entry = serde_json::Map::new();
        entry.insert("project_rules".to_string(), json!("cached-value"));
        let mut object = serde_json::Map::new();

        sync_cached_bridge_field(&mut entry, &mut object, "project_rules");

        assert_eq!(entry.get("project_rules"), Some(&json!("cached-value")));
        assert_eq!(object.get("project_rules"), Some(&json!("cached-value")));
    }

    #[test]
    fn sync_both_have_field_object_wins() {
        let mut entry = serde_json::Map::new();
        entry.insert("project_rules".to_string(), json!("old-cached"));
        let mut object = serde_json::Map::new();
        object.insert("project_rules".to_string(), json!("new-incoming"));

        sync_cached_bridge_field(&mut entry, &mut object, "project_rules");

        assert_eq!(entry.get("project_rules"), Some(&json!("new-incoming")));
        assert_eq!(object.get("project_rules"), Some(&json!("new-incoming")));
    }

    #[test]
    fn sync_object_has_null_treated_as_absent() {
        let mut entry = serde_json::Map::new();
        entry.insert("project_rules".to_string(), json!("cached-value"));
        let mut object = serde_json::Map::new();
        object.insert("project_rules".to_string(), serde_json::Value::Null);

        sync_cached_bridge_field(&mut entry, &mut object, "project_rules");

        // null is treated as absent, so entry's cached value is used
        assert_eq!(entry.get("project_rules"), Some(&json!("cached-value")));
        assert_eq!(object.get("project_rules"), Some(&json!("cached-value")));
    }

    #[test]
    fn sync_neither_has_field() {
        let mut entry = serde_json::Map::new();
        let mut object = serde_json::Map::new();

        sync_cached_bridge_field(&mut entry, &mut object, "project_rules");

        assert!(entry.get("project_rules").is_none());
        assert!(object.get("project_rules").is_none());
    }

    #[test]
    fn sync_complex_nested_value_preserved() {
        let complex = json!({
            "rules": [{"id": 1, "text": "do this"}, {"id": 2, "text": "do that"}],
            "meta": {"version": 3}
        });
        let mut entry = serde_json::Map::new();
        let mut object = serde_json::Map::new();
        object.insert("project_rules".to_string(), complex.clone());

        sync_cached_bridge_field(&mut entry, &mut object, "project_rules");

        assert_eq!(entry.get("project_rules"), Some(&complex));
    }

    // ── inject_bridge_cache_state ───────────────────────────────────

    #[test]
    fn inject_empty_entry_does_nothing() {
        let entry = serde_json::Map::new();
        let mut object = serde_json::Map::new();

        inject_bridge_cache_state(&entry, &mut object);

        assert!(object.get("bridge_cache_state").is_none());
    }

    #[test]
    fn inject_entry_with_seed_state_injects() {
        let mut entry = serde_json::Map::new();
        entry.insert("created_at".to_string(), json!("2024-01-15T10:30:00Z"));

        let mut object = serde_json::Map::new();
        inject_bridge_cache_state(&entry, &mut object);

        let state = object.get("bridge_cache_state");
        assert!(state.is_some());
        let state_obj = state.unwrap().as_object().unwrap();
        assert!(state_obj.contains_key("created_at"));
    }

    #[test]
    fn inject_overwrites_existing_bridge_cache_state() {
        let mut entry = serde_json::Map::new();
        entry.insert("created_at".to_string(), json!("2024-01-15T10:30:00Z"));

        let mut object = serde_json::Map::new();
        object.insert("bridge_cache_state".to_string(), json!("old-stuff"));

        inject_bridge_cache_state(&entry, &mut object);

        let state = object.get("bridge_cache_state").unwrap();
        assert!(state.is_object()); // replaced with the normalized object
    }

    #[test]
    fn inject_entry_with_history_seed() {
        let mut entry = serde_json::Map::new();
        entry.insert("history".to_string(), json!(["turn1", "turn2"]));

        let mut object = serde_json::Map::new();
        inject_bridge_cache_state(&entry, &mut object);

        let state = object
            .get("bridge_cache_state")
            .unwrap()
            .as_object()
            .unwrap();
        assert_eq!(state.get("history"), Some(&json!(["turn1", "turn2"])));
        assert_eq!(state.get("turn_count"), Some(&json!(0)));
    }

    // ── trim_edge_tools_for_result_turn ─────────────────────────────

    #[test]
    fn trim_no_tool_results_returns_early() {
        let mut object = serde_json::Map::new();
        object.insert("edge_tools".to_string(), json!([tool_value("bash")]));
        let original_tools = object.get("edge_tools").cloned();

        trim_edge_tools_for_result_turn(&mut object, "");

        // No tool_results, so edge_tools unchanged
        assert_eq!(object.get("edge_tools"), original_tools.as_ref());
    }

    #[test]
    fn trim_no_edge_tools_returns_early() {
        let mut object = serde_json::Map::new();
        object.insert(
            "tool_results".to_string(),
            json!([{"name": "bash", "output": "ok"}]),
        );

        trim_edge_tools_for_result_turn(&mut object, "");

        assert!(object.get("edge_tools").is_none());
    }

    #[test]
    fn trim_does_not_panic_on_empty_object() {
        let mut object = serde_json::Map::new();
        trim_edge_tools_for_result_turn(&mut object, "some query");
    }

    #[test]
    fn trim_with_user_query_skips_filtering() {
        let mut object = serde_json::Map::new();
        object.insert(
            "tool_results".to_string(),
            json!([{"name": "bash", "output": "ok"}]),
        );
        object.insert(
            "edge_tools".to_string(),
            json!([tool_value("bash"), tool_value("grep")]),
        );

        trim_edge_tools_for_result_turn(&mut object, "what happened?");

        // Non-empty user_query means plan_tool_subset_for_result_turn returns None,
        // so edge_tools stays unchanged
        let tools = object.get("edge_tools").unwrap().as_array().unwrap();
        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn trim_filters_to_used_tools() {
        let mut object = serde_json::Map::new();
        object.insert(
            "tool_results".to_string(),
            json!([{"name": "bash", "output": "ok"}]),
        );
        object.insert(
            "edge_tools".to_string(),
            json!([tool_value("bash"), tool_value("grep"), tool_value("view")]),
        );

        trim_edge_tools_for_result_turn(&mut object, "");

        let tools = object.get("edge_tools").unwrap().as_array().unwrap();
        // Only "bash" should remain since it was used in tool_results
        assert_eq!(tools.len(), 1);
        let name = tools[0]
            .get("function")
            .unwrap()
            .get("name")
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(name, "bash");
    }

    #[test]
    fn trim_empty_tool_results_array_is_no_op() {
        let mut object = serde_json::Map::new();
        object.insert("tool_results".to_string(), json!([]));
        object.insert(
            "edge_tools".to_string(),
            json!([tool_value("bash"), tool_value("grep")]),
        );
        let original = object.get("edge_tools").cloned();

        trim_edge_tools_for_result_turn(&mut object, "");

        // Empty tool_results → plan_tool_subset returns None → no filtering
        assert_eq!(object.get("edge_tools"), original.as_ref());
    }
}
