use super::*;

// ─── Typed request body ──────────────────────────────────────────────────────

/// Typed representation of the incoming chat turn request payload.
///
/// All fields use `Option<serde_json::Value>` for defensive deserialization —
/// if the client sends a field with an unexpected type (e.g., `"messages": 42`),
/// the whole request still parses correctly instead of failing outright.
/// Typed accessor methods provide the same safety as manual `.get()?.as_str()?`
/// chains but with named fields and compile-time discoverability.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct ChatTurnRequestBody {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_id: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    messages: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_results: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    edge_tools: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    edge_profile: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project_rules: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    execution_state: Option<serde_json::Value>,
    /// Forward-compatible: preserves unknown fields through round-trip.
    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

impl ChatTurnRequestBody {
    fn session_id_str(&self) -> Option<&str> {
        self.session_id.as_ref()?.as_str()
    }

    fn has_non_null_session_id(&self) -> bool {
        self.session_id.as_ref().is_some_and(|v| !v.is_null())
    }

    fn agent_id_str(&self) -> Option<String> {
        self.agent_id.as_ref()?.as_str().map(String::from)
    }

    fn messages_slice(&self) -> &[serde_json::Value] {
        self.messages
            .as_ref()
            .and_then(|v| v.as_array())
            .map(|a| a.as_slice())
            .unwrap_or_default()
    }

    fn has_tool_results(&self) -> bool {
        self.tool_results
            .as_ref()
            .and_then(|v| v.as_array())
            .is_some_and(|a| !a.is_empty())
    }

    fn tool_results_slice(&self) -> &[serde_json::Value] {
        self.tool_results
            .as_ref()
            .and_then(|v| v.as_array())
            .map(|a| a.as_slice())
            .unwrap_or_default()
    }

    fn edge_tools_vec(&self) -> Option<&Vec<serde_json::Value>> {
        self.edge_tools.as_ref()?.as_array()
    }

    fn set_edge_tools(&mut self, tools: Vec<serde_json::Value>) {
        self.edge_tools = Some(serde_json::Value::Array(tools));
    }

    fn set_session_id(&mut self, id: &str) {
        self.session_id = Some(serde_json::Value::String(id.to_string()));
    }

    fn model_str(&self) -> Option<&str> {
        self.model.as_ref()?.as_str()
    }

    fn execution_state_obj(&self) -> Option<&serde_json::Map<String, serde_json::Value>> {
        self.execution_state.as_ref()?.as_object()
    }

    fn user_query(&self) -> String {
        extract_latest_user_query(self.messages_slice())
    }

    fn classify_task(&self) -> Option<String> {
        let normalized: Vec<_> = self
            .messages_slice()
            .iter()
            .filter_map(serde_json::Value::as_object)
            .cloned()
            .collect();
        classify_task(&normalized)
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Extract tool name from an OpenAI-format tool definition.
///
/// Handles `{"function": {"name": "bash", ...}}` — the standard format used
/// in edge_tools arrays. Returns `None` for malformed entries.
fn extract_tool_name(tool: &serde_json::Value) -> Option<&str> {
    tool.get("function")?.get("name")?.as_str()
}

// ─── Prepared result ─────────────────────────────────────────────────────────

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

impl PreparedChatTurnBridgeRequest {
    /// Create a passthrough result for unparseable or non-object payloads.
    fn passthrough(body: Bytes) -> Self {
        Self {
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
        }
    }
}

fn validate_session_id_shape(
    request: &ChatTurnRequestBody,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if request.has_non_null_session_id() && request.session_id_str().is_none() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "session_id must be a string",
        ));
    }
    if request
        .session_id_str()
        .is_some_and(|session_id| session_id.trim().is_empty())
    {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "session_id must not be empty",
        ));
    }
    Ok(())
}

pub(super) async fn prepare_chat_turn_bridge_body(
    state: &AppState,
    user: &AuthUserRecord,
    body: Bytes,
) -> Result<PreparedChatTurnBridgeRequest, (StatusCode, Json<ErrorResponse>)> {
    let Ok(mut request) = serde_json::from_slice::<ChatTurnRequestBody>(&body) else {
        return Ok(PreparedChatTurnBridgeRequest::passthrough(body));
    };
    validate_session_id_shape(&request)?;

    // ── Session resolution ──────────────────────────────────────────────
    let (trusted_session_id, trusted_session_created_at) =
        if let Some(session_id) = request.session_id_str().map(String::from) {
            let session = state
                .session_service
                .get_session(session_id.clone(), user.user_id.clone())
                .await
                .map_err(normalize_chat_turn_session_error)?;
            (
                Some(session_id),
                normalize_session_created_at_for_bridge(&session.created_at),
            )
        } else {
            let agent_id = request.agent_id_str();
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
            request.set_session_id(&created_session_id);
            (Some(created_session_id), created_session_created_at)
        };

    if let (Some(session_id), Some(created_at)) = (
        trusted_session_id.as_deref(),
        trusted_session_created_at.as_deref(),
    ) {
        seed_bridge_session_created_at(state, session_id, created_at).await;
    }

    // ── Turn identifiers ────────────────────────────────────────────────
    let (turn_chain_id, user_query_event_id) =
        if let Some(session_id) = trusted_session_id.as_deref() {
            let messages = request.messages_slice();
            let has_tool_results = request.has_tool_results();
            let (chain_id, event_id) =
                prepare_chat_turn_bridge_identifiers(state, session_id, messages, has_tool_results)
                    .await;
            (Some(chain_id), Some(event_id))
        } else {
            (None, None)
        };

    // ── Cached inputs + tool trimming ───────────────────────────────────
    let tools_changed = if let Some(session_id) = trusted_session_id.as_deref() {
        Some(prepare_chat_turn_bridge_cached_inputs(state, session_id, &mut request).await)
    } else {
        None
    };

    let user_query = request.user_query();
    // Tool selection is client-side; server trusts pre-selected set.
    trim_edge_tools_for_result_turn(&mut request, &user_query);

    // ── Metadata extraction ─────────────────────────────────────────────
    let task_hint = request.classify_task();
    let user_query_b64 = Some(URL_SAFE.encode(user_query.as_bytes()));
    let routing_meta_b64 = request.model_str().map(|_| {
        let meta = serde_json::Value::Object(build_skipped_routing_metadata("model_override"));
        URL_SAFE.encode(serde_json::to_string(&meta).unwrap_or_default().as_bytes())
    });
    let force_intent = detect_correction(&user_query).then_some("question".to_string());
    let execution_state_b64 = request
        .execution_state_obj()
        .map(normalize_execution_state)
        .filter(|es| !es.is_empty())
        .map(|es| URL_SAFE.encode(serde_json::to_string(&es).unwrap_or_default()));

    // ── Serialize mutated payload ───────────────────────────────────────
    serde_json::to_vec(&request)
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
    request: &mut ChatTurnRequestBody,
) -> bool {
    let now = current_unix_seconds();
    let mut cache = state.chat_turn_bridge_cache.lock().await;
    let mut entry = cache.get(session_id, now).unwrap_or_default();
    let cached_tools = entry
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .cloned();

    let tools_changed = if let Some(edge_tools) = request.edge_tools_vec().cloned() {
        let changed = cached_tools
            .as_ref()
            .is_some_and(|cached| !same_tool_names(cached, &edge_tools));
        entry.insert("tools".to_string(), serde_json::Value::Array(edge_tools));
        changed
    } else if let Some(cached_tools) = cached_tools {
        request.set_edge_tools(cached_tools);
        false
    } else {
        false
    };

    sync_opt_field_with_cache(&mut entry, "project_rules", &mut request.project_rules);
    sync_opt_field_with_cache(&mut entry, "edge_profile", &mut request.edge_profile);
    inject_bridge_cache_state_into(&entry, request);

    cache.insert(session_id.to_string(), entry, now);
    tools_changed
}

fn trim_edge_tools_for_result_turn(request: &mut ChatTurnRequestBody, user_query: &str) {
    let tool_result_names: Vec<Option<&str>> = request
        .tool_results_slice()
        .iter()
        .map(|tr| tr.get("name").and_then(serde_json::Value::as_str))
        .collect();
    let Some(edge_tools) = request.edge_tools_vec().cloned() else {
        return;
    };
    let available_tool_names: Vec<&str> = edge_tools.iter().filter_map(extract_tool_name).collect();
    let Some(subset_names) =
        plan_tool_subset_for_result_turn(&tool_result_names, user_query, &available_tool_names)
    else {
        return;
    };
    let filtered = edge_tools
        .into_iter()
        .filter(|tool| {
            extract_tool_name(tool)
                .is_some_and(|name| subset_names.iter().any(|candidate| candidate == name))
        })
        .collect();
    request.set_edge_tools(filtered);
}

/// Sync a typed `Option<Value>` field with a cache entry.
///
/// If the field has a non-null value, it's written to the cache.
/// If the field is absent/null but the cache has a value, it's restored.
fn sync_opt_field_with_cache(
    cache_entry: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    field: &mut Option<serde_json::Value>,
) {
    if let Some(value) = field.as_ref().filter(|v| !v.is_null()) {
        cache_entry.insert(key.to_string(), value.clone());
    } else if let Some(cached) = cache_entry.get(key).cloned() {
        *field = Some(cached);
    }
}

/// Inject normalized bridge cache state into a typed request body.
fn inject_bridge_cache_state_into(
    entry: &serde_json::Map<String, serde_json::Value>,
    request: &mut ChatTurnRequestBody,
) {
    if let Some(bridge_cache_state) = normalize_bridge_cache_entry(entry) {
        request.extra.insert(
            "bridge_cache_state".to_string(),
            serde_json::Value::Object(bridge_cache_state),
        );
    }
}

#[cfg(test)]
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

#[cfg(test)]
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
            .filter_map(extract_tool_name)
            .map(String::from)
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

    /// Helper to build a ChatTurnRequestBody from a Map for testing.
    fn request_from_map(map: serde_json::Map<String, serde_json::Value>) -> ChatTurnRequestBody {
        serde_json::from_value(serde_json::Value::Object(map)).unwrap_or_default()
    }

    #[test]
    fn trim_no_tool_results_returns_early() {
        let mut map = serde_json::Map::new();
        map.insert("edge_tools".to_string(), json!([tool_value("bash")]));
        let mut request = request_from_map(map);
        let original_tools = request.edge_tools_vec().cloned();

        trim_edge_tools_for_result_turn(&mut request, "");

        assert_eq!(request.edge_tools_vec().cloned(), original_tools);
    }

    #[test]
    fn trim_no_edge_tools_returns_early() {
        let mut map = serde_json::Map::new();
        map.insert(
            "tool_results".to_string(),
            json!([{"name": "bash", "output": "ok"}]),
        );
        let mut request = request_from_map(map);

        trim_edge_tools_for_result_turn(&mut request, "");

        assert!(request.edge_tools_vec().is_none());
    }

    #[test]
    fn trim_does_not_panic_on_empty_request() {
        let mut request = ChatTurnRequestBody::default();
        trim_edge_tools_for_result_turn(&mut request, "some query");
    }

    #[test]
    fn trim_with_user_query_skips_filtering() {
        let mut map = serde_json::Map::new();
        map.insert(
            "tool_results".to_string(),
            json!([{"name": "bash", "output": "ok"}]),
        );
        map.insert(
            "edge_tools".to_string(),
            json!([tool_value("bash"), tool_value("grep")]),
        );
        let mut request = request_from_map(map);

        trim_edge_tools_for_result_turn(&mut request, "what happened?");

        let tools = request.edge_tools_vec().unwrap();
        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn trim_filters_to_used_tools() {
        let mut map = serde_json::Map::new();
        map.insert(
            "tool_results".to_string(),
            json!([{"name": "bash", "output": "ok"}]),
        );
        map.insert(
            "edge_tools".to_string(),
            json!([tool_value("bash"), tool_value("grep"), tool_value("view")]),
        );
        let mut request = request_from_map(map);

        trim_edge_tools_for_result_turn(&mut request, "");

        let tools = request.edge_tools_vec().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(extract_tool_name(&tools[0]), Some("bash"));
    }

    #[test]
    fn trim_empty_tool_results_array_is_no_op() {
        let mut map = serde_json::Map::new();
        map.insert("tool_results".to_string(), json!([]));
        map.insert(
            "edge_tools".to_string(),
            json!([tool_value("bash"), tool_value("grep")]),
        );
        let original_count = 2;
        let mut request = request_from_map(map);

        trim_edge_tools_for_result_turn(&mut request, "");

        assert_eq!(request.edge_tools_vec().unwrap().len(), original_count);
    }

    // ── ChatTurnRequestBody typed deserialization ────────────────────

    #[test]
    fn typed_request_round_trip_preserves_all_fields() {
        let json = json!({
            "session_id": "sess-123",
            "agent_id": "agent-1",
            "messages": [{"role": "user", "content": "hello"}],
            "tool_results": [{"name": "bash", "output": "ok"}],
            "edge_tools": [tool_value("bash"), tool_value("grep")],
            "edge_profile": {"cwd": "/tmp"},
            "project_rules": {"max_tokens": 1000},
            "model": "gpt-4",
            "execution_state": {"pending_tools": []},
            "custom_field": "preserved"
        });

        let request: ChatTurnRequestBody = serde_json::from_value(json.clone()).unwrap();
        let serialized = serde_json::to_value(&request).unwrap();

        assert_eq!(request.session_id_str(), Some("sess-123"));
        assert_eq!(request.agent_id_str(), Some("agent-1".to_string()));
        assert_eq!(request.messages_slice().len(), 1);
        assert!(request.has_tool_results());
        assert_eq!(request.edge_tools_vec().unwrap().len(), 2);
        assert_eq!(request.model_str(), Some("gpt-4"));
        assert!(request.execution_state_obj().is_some());

        // Forward-compat: unknown fields preserved
        assert_eq!(serialized.get("custom_field").unwrap(), &json!("preserved"));
    }

    #[test]
    fn typed_request_empty_json_object() {
        let request: ChatTurnRequestBody = serde_json::from_str("{}").unwrap();
        assert!(request.session_id_str().is_none());
        assert!(!request.has_non_null_session_id());
        assert!(request.messages_slice().is_empty());
        assert!(!request.has_tool_results());
        assert!(request.edge_tools_vec().is_none());
        assert!(request.model_str().is_none());
    }

    #[test]
    fn typed_request_null_session_id() {
        let request: ChatTurnRequestBody = serde_json::from_str(r#"{"session_id": null}"#).unwrap();
        assert!(request.session_id_str().is_none());
        assert!(!request.has_non_null_session_id());
    }

    #[test]
    fn typed_request_numeric_session_id_handled() {
        let request: ChatTurnRequestBody = serde_json::from_str(r#"{"session_id": 42}"#).unwrap();
        assert!(request.session_id_str().is_none());
        assert!(request.has_non_null_session_id());
    }

    #[test]
    fn validate_session_id_shape_rejects_non_string_values() {
        let request: ChatTurnRequestBody = serde_json::from_str(r#"{"session_id": 42}"#).unwrap();
        let (status, body) = validate_session_id_shape(&request).unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body.0.detail, "session_id must be a string");
    }

    #[test]
    fn validate_session_id_shape_allows_absent_or_string_values() {
        let absent: ChatTurnRequestBody = serde_json::from_str(r#"{"messages":[]}"#).unwrap();
        let stringy: ChatTurnRequestBody =
            serde_json::from_str(r#"{"session_id":"sess-1"}"#).unwrap();
        assert!(validate_session_id_shape(&absent).is_ok());
        assert!(validate_session_id_shape(&stringy).is_ok());
    }

    #[test]
    fn validate_session_id_shape_rejects_empty_strings() {
        for raw in [r#"{"session_id":""}"#, r#"{"session_id":"   "}"#] {
            let request: ChatTurnRequestBody = serde_json::from_str(raw).unwrap();
            let (status, body) = validate_session_id_shape(&request).unwrap_err();
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(body.0.detail, "session_id must not be empty");
        }
    }

    #[test]
    fn typed_request_wrong_type_for_messages() {
        let request: ChatTurnRequestBody =
            serde_json::from_str(r#"{"messages": "not-an-array"}"#).unwrap();
        assert!(request.messages_slice().is_empty());
    }

    #[test]
    fn typed_request_set_session_id() {
        let mut request = ChatTurnRequestBody::default();
        request.set_session_id("new-sess");
        assert_eq!(request.session_id_str(), Some("new-sess"));
    }

    #[test]
    fn typed_request_set_edge_tools() {
        let mut request = ChatTurnRequestBody::default();
        assert!(request.edge_tools_vec().is_none());
        request.set_edge_tools(vec![tool_value("bash")]);
        assert_eq!(request.edge_tools_vec().unwrap().len(), 1);
    }

    #[test]
    fn typed_request_non_object_json_fails() {
        assert!(serde_json::from_str::<ChatTurnRequestBody>(r#""hello""#).is_err());
        assert!(serde_json::from_str::<ChatTurnRequestBody>("42").is_err());
        assert!(serde_json::from_str::<ChatTurnRequestBody>("[]").is_err());
        assert!(serde_json::from_str::<ChatTurnRequestBody>("null").is_err());
    }

    // ── extract_tool_name ───────────────────────────────────────────

    #[test]
    fn extract_tool_name_standard_format() {
        assert_eq!(extract_tool_name(&tool_value("bash")), Some("bash"));
    }

    #[test]
    fn extract_tool_name_missing_function() {
        assert_eq!(extract_tool_name(&json!({"type": "custom"})), None);
    }

    #[test]
    fn extract_tool_name_missing_name() {
        assert_eq!(extract_tool_name(&json!({"function": {}})), None);
    }

    #[test]
    fn extract_tool_name_non_string_name() {
        assert_eq!(extract_tool_name(&json!({"function": {"name": 42}})), None);
    }

    // ── sync_opt_field_with_cache ───────────────────────────────────

    #[test]
    fn sync_opt_field_writes_to_cache() {
        let mut entry = serde_json::Map::new();
        let mut field = Some(json!("rule-value"));

        sync_opt_field_with_cache(&mut entry, "rules", &mut field);

        assert_eq!(entry.get("rules"), Some(&json!("rule-value")));
        assert_eq!(field, Some(json!("rule-value")));
    }

    #[test]
    fn sync_opt_field_restores_from_cache() {
        let mut entry = serde_json::Map::new();
        entry.insert("rules".to_string(), json!("cached-value"));
        let mut field: Option<serde_json::Value> = None;

        sync_opt_field_with_cache(&mut entry, "rules", &mut field);

        assert_eq!(field, Some(json!("cached-value")));
    }

    #[test]
    fn sync_opt_field_null_treated_as_absent() {
        let mut entry = serde_json::Map::new();
        entry.insert("rules".to_string(), json!("cached-value"));
        let mut field = Some(serde_json::Value::Null);

        sync_opt_field_with_cache(&mut entry, "rules", &mut field);

        assert_eq!(field, Some(json!("cached-value")));
    }

    #[test]
    fn sync_opt_field_neither_has_value() {
        let mut entry = serde_json::Map::new();
        let mut field: Option<serde_json::Value> = None;

        sync_opt_field_with_cache(&mut entry, "rules", &mut field);

        assert!(entry.get("rules").is_none());
        assert!(field.is_none());
    }

    // ── inject_bridge_cache_state_into ──────────────────────────────

    #[test]
    fn inject_into_request_adds_bridge_cache_state() {
        let mut entry = serde_json::Map::new();
        entry.insert("created_at".to_string(), json!("2024-01-15T10:30:00Z"));

        let mut request = ChatTurnRequestBody::default();
        inject_bridge_cache_state_into(&entry, &mut request);

        assert!(request.extra.contains_key("bridge_cache_state"));
        let state = request.extra["bridge_cache_state"].as_object().unwrap();
        assert!(state.contains_key("created_at"));
    }

    #[test]
    fn inject_into_request_empty_entry_is_noop() {
        let entry = serde_json::Map::new();
        let mut request = ChatTurnRequestBody::default();
        inject_bridge_cache_state_into(&entry, &mut request);
        assert!(!request.extra.contains_key("bridge_cache_state"));
    }

    // ── passthrough constructor ─────────────────────────────────────

    #[test]
    fn passthrough_has_all_none_metadata() {
        let body = Bytes::from_static(b"not-json");
        let result = PreparedChatTurnBridgeRequest::passthrough(body.clone());
        assert_eq!(result.body, body);
        assert!(result.trusted_session_id.is_none());
        assert!(result.turn_chain_id.is_none());
        assert!(result.user_query_event_id.is_none());
        assert!(result.tools_changed.is_none());
        assert!(result.task_hint.is_none());
        assert!(result.user_query_b64.is_none());
        assert!(result.routing_meta_b64.is_none());
        assert!(result.force_intent.is_none());
        assert!(result.execution_state_b64.is_none());
    }
}
